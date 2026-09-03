use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use socket2::SockRef;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::AbortHandle;
use tracing::{Instrument, debug, info, info_span, warn};

use crate::auth::{AuthState, hex_upper};
use crate::backend::{BackendKind, OracleBackend, OracleCursor, PostgresBackend};
use crate::credentials::Credentials;
use crate::error::{Error, Result};
use crate::mariadb::MariaDbBackend;
use crate::tns::{PacketType, SduMode, TnsStream, build_accept_response};
use crate::wire::{
    self, build_dml_response, build_error_response, build_error_response_at, build_query_response,
};

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);
/// Process-wide counters exposed on `/metrics` (Prometheus text format).
static ACTIVE_SESSIONS: AtomicU64 = AtomicU64::new(0);
static SESSIONS_TOTAL: AtomicU64 = AtomicU64::new(0);
static STATEMENTS_TOTAL: AtomicU64 = AtomicU64::new(0);
static BACKEND_ERRORS_TOTAL: AtomicU64 = AtomicU64::new(0);

fn render_metrics() -> String {
    let g = |c: &AtomicU64| c.load(Ordering::Relaxed);
    format!(
        "# HELP pgsaci_sessions_active Currently connected Oracle sessions.\n\
         # TYPE pgsaci_sessions_active gauge\n\
         pgsaci_sessions_active {}\n\
         # HELP pgsaci_sessions_total Sessions accepted since start.\n\
         # TYPE pgsaci_sessions_total counter\n\
         pgsaci_sessions_total {}\n\
         # HELP pgsaci_statements_total Execute/Fetch/DML calls served.\n\
         # TYPE pgsaci_statements_total counter\n\
         pgsaci_statements_total {}\n\
         # HELP pgsaci_backend_errors_total PostgreSQL errors mapped and returned to clients.\n\
         # TYPE pgsaci_backend_errors_total counter\n\
         pgsaci_backend_errors_total {}\n",
        g(&ACTIVE_SESSIONS),
        g(&SESSIONS_TOTAL),
        g(&STATEMENTS_TOTAL),
        g(&BACKEND_ERRORS_TOTAL),
    )
}

struct SessionGuard;
impl SessionGuard {
    fn enter() -> Self {
        ACTIVE_SESSIONS.fetch_add(1, Ordering::Relaxed);
        SESSIONS_TOTAL.fetch_add(1, Ordering::Relaxed);
        Self
    }
}
impl Drop for SessionGuard {
    fn drop(&mut self) {
        ACTIVE_SESSIONS.fetch_sub(1, Ordering::Relaxed);
    }
}

// --- session registry: backs `GET /sessions` and `DELETE /sessions/{id}` ------

struct SessionMeta {
    addr: String,
    user: Mutex<Option<String>>,
    connected: Instant,
    abort: AbortHandle,
}

static SESSIONS: LazyLock<Mutex<BTreeMap<u64, Arc<SessionMeta>>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

fn sessions_register(id: u64, addr: String, abort: AbortHandle) {
    SESSIONS.lock().unwrap().insert(
        id,
        Arc::new(SessionMeta {
            addr,
            user: Mutex::new(None),
            connected: Instant::now(),
            abort,
        }),
    );
}

fn sessions_set_user(id: u64, user: &str) {
    if let Some(meta) = SESSIONS.lock().unwrap().get(&id) {
        *meta.user.lock().unwrap() = Some(user.to_string());
    }
}

/// Removes on normal task exit; a `Drop` impl so it fires on panic/abort too.
struct SessionRegistration(u64);
impl Drop for SessionRegistration {
    fn drop(&mut self) {
        SESSIONS.lock().unwrap().remove(&self.0);
    }
}

/// Aborts the session's task (which drops its TNS stream and backend PostgreSQL
/// connection, releasing any locks it held). Returns false if no such session.
fn sessions_kill(id: u64) -> bool {
    match SESSIONS.lock().unwrap().get(&id) {
        Some(meta) => {
            meta.abort.abort();
            true
        }
        None => false,
    }
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn render_sessions_json() -> String {
    let map = SESSIONS.lock().unwrap();
    let mut out = String::from("[");
    for (i, (id, meta)) in map.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let user = meta.user.lock().unwrap().clone().unwrap_or_default();
        out.push_str(&format!(
            "{{\"id\":{id},\"addr\":\"{}\",\"user\":\"{}\",\"age_seconds\":{}}}",
            json_escape(&meta.addr),
            json_escape(&user),
            meta.connected.elapsed().as_secs(),
        ));
    }
    out.push(']');
    out
}

#[derive(Clone)]
pub struct Config {
    /// Database engine behind the Oracle wire protocol.
    pub backend: BackendKind,
    pub listen_addr: String,
    pub pg_host: String,
    pub pg_port: u16,
    pub pg_db: String,
    /// Per-user PostgreSQL passwords. The Oracle login challenge is run with the
    /// password matched here (or the fallback), so an Oracle client authenticates
    /// with the same credentials it would use against PostgreSQL directly.
    pub credentials: Credentials,
    /// Per-statement PostgreSQL limit. `None` preserves Oracle's unlimited
    /// default; when set, timeout cancellation is returned as ORA-01013.
    pub statement_timeout: Option<Duration>,
    /// Maximum time a connected client may remain silent while PgSaci is reading
    /// a TNS frame. `None` disables application-level idle reaping.
    pub idle_timeout: Option<Duration>,
    /// `host:port` for the plain-HTTP `/healthz` + `/readyz` probes. `None`
    /// disables the endpoint.
    pub health_addr: Option<String>,
    /// How long `run_with_listener` waits for in-flight sessions to finish
    /// after a shutdown signal (SIGINT / Ctrl-C) before returning anyway.
    pub shutdown_grace: Duration,
    /// Which Oracle release PgSaci presents itself as (banner, `v$version`,
    /// `AUTH_VERSION_*`, and the auth verifier family).
    pub oracle_version: OracleVersion,
}

/// The Oracle release PgSaci impersonates on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OracleVersion {
    /// 11.2.0.4 — 11g O5LOGON verifier. For older JDBC / OCI clients.
    V11g,
    /// 19.0.0.0 — 12c PBKDF2 verifier. The default; required by
    /// python-oracledb thin and modern JDBC thin.
    #[default]
    V19c,
}

impl OracleVersion {
    /// The `Oracle Database …` banner string.
    pub fn banner(self) -> &'static str {
        match self {
            OracleVersion::V11g => {
                "Oracle Database 11g Enterprise Edition Release 11.2.0.4.0 - 64bit Production"
            }
            OracleVersion::V19c => {
                "Oracle Database 19c Enterprise Edition Release 19.0.0.0.0 - Production"
            }
        }
    }

    /// `x.y.z.p.b` release string used for `AUTH_VERSION_STRING` and `v$version`.
    pub fn release(self) -> &'static str {
        match self {
            OracleVersion::V11g => "11.2.0.4.0",
            OracleVersion::V19c => "19.0.0.0.0",
        }
    }

    /// Packed `AUTH_VERSION_NO`. python-oracledb thin decodes it as
    /// `major=(v>>24)&0xff, minor=(v>>16)&0xff, rel=(v>>12)&0xf,
    /// update=(v>>4)&0xff, port=v&0xf` regardless of release, so both entries
    /// use that layout (verified: `conn.version` == "11.2.0.4.0" / "19.0.0.0.0").
    pub fn version_no(self) -> u32 {
        match self {
            OracleVersion::V11g => ((11 << 24) | (2 << 16)) | (4 << 4),
            OracleVersion::V19c => 19 << 24,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            backend: BackendKind::Postgres,
            listen_addr: "0.0.0.0:1521".to_string(),
            pg_host: "localhost".to_string(),
            pg_port: 5432,
            pg_db: "postgres".to_string(),
            credentials: Credentials::with_fallback("postgres"),
            statement_timeout: None,
            idle_timeout: Some(Duration::from_secs(15 * 60)),
            health_addr: None,
            shutdown_grace: Duration::from_secs(30),
            oracle_version: OracleVersion::default(),
        }
    }
}

pub struct Server {
    config: Config,
}

impl Server {
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    pub async fn run(self) -> Result<()> {
        let listener = TcpListener::bind(&self.config.listen_addr).await?;
        self.run_with_listener(listener).await
    }

    pub async fn run_with_listener(self, listener: TcpListener) -> Result<()> {
        info!("PgSaci listening on {}", listener.local_addr()?);
        match self.config.idle_timeout {
            Some(d) => info!("idle client reaping after {}s", d.as_secs()),
            None => info!("idle client reaping disabled"),
        }

        if let Some(addr) = self.config.health_addr.clone() {
            let probe = HealthProbe {
                pg_host: self.config.pg_host.clone(),
                pg_port: self.config.pg_port,
                pg_db: self.config.pg_db.clone(),
            };
            match TcpListener::bind(&addr).await {
                Ok(l) => {
                    info!(
                        "health endpoint on {} (/healthz, /readyz, /metrics, /sessions)",
                        addr
                    );
                    tokio::spawn(serve_health(l, probe));
                }
                Err(e) => warn!(%addr, %e, "could not bind health endpoint"),
            }
        }

        let grace = self.config.shutdown_grace;
        loop {
            let accepted = tokio::select! {
                a = listener.accept() => a,
                _ = shutdown_signal() => {
                    info!("shutdown signal received; no longer accepting connections");
                    break;
                }
            };
            let (stream, addr) = accepted?;
            let session_id = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
            if let Err(error) = SockRef::from(&stream).set_keepalive(true) {
                warn!(session_id, %addr, %error, "could not enable TCP keepalive");
            }
            info!(session_id, %addr, "new connection");
            let config = self.config.clone();
            let handle = tokio::spawn(
                async move {
                    let _guard = SessionGuard::enter();
                    let _reg = SessionRegistration(session_id);
                    if let Err(e) = handle_connection(stream, session_id, config).await {
                        warn!("connection handler error: {}", e);
                    }
                }
                .instrument(info_span!("oracle_session", session_id, %addr)),
            );
            sessions_register(session_id, addr.to_string(), handle.abort_handle());
        }

        // Graceful drain: give live sessions a bounded window to finish.
        let deadline = tokio::time::Instant::now() + grace;
        while ACTIVE_SESSIONS.load(Ordering::Relaxed) > 0 && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let left = ACTIVE_SESSIONS.load(Ordering::Relaxed);
        if left > 0 {
            warn!("shutdown grace elapsed with {left} session(s) still active");
        } else {
            info!("all sessions drained; shutting down cleanly");
        }
        Ok(())
    }
}

/// Resolves when the process receives SIGINT (Ctrl-C) or, on Unix, SIGTERM.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

struct HealthProbe {
    pg_host: String,
    pg_port: u16,
    pg_db: String,
}

/// Minimal HTTP/1.1 health server. `/healthz` = process/listener up;
/// `/readyz` = a fresh TCP connection to the configured PostgreSQL port
/// succeeds; `/metrics` = Prometheus text; `GET /sessions` = JSON list of live
/// Oracle sessions (id, addr, user, age); `DELETE /sessions/<id>` = abort one
/// (drops its backend PostgreSQL connection, releasing any locks it held). No
/// external HTTP dependency: one request line, one response.
async fn serve_health(listener: TcpListener, probe: HealthProbe) {
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            continue;
        };
        let probe_host = probe.pg_host.clone();
        let probe_port = probe.pg_port;
        let _db = probe.pg_db.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0u8; 1024];
            let n = stream.read(&mut buf).await.unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..n]);
            let request_line = head.lines().next().unwrap_or("");
            let mut parts = request_line.split_whitespace();
            let method = parts.next().unwrap_or("GET");
            let path = parts.next().unwrap_or("/");
            let (status, body): (&str, String) = if path == "/sessions" || path == "/sessions/" {
                ("200 OK", render_sessions_json())
            } else if let Some(rest) = path.strip_prefix("/sessions/") {
                let id_str = rest.split(['/', '?']).next().unwrap_or("");
                match id_str.parse::<u64>() {
                    Ok(id) if method == "DELETE" || method == "POST" => {
                        if sessions_kill(id) {
                            ("200 OK", format!("killed session {id}"))
                        } else {
                            ("404 Not Found", format!("no session {id}"))
                        }
                    }
                    Ok(_) => (
                        "405 Method Not Allowed",
                        "use DELETE /sessions/<id> to kill a session".to_string(),
                    ),
                    Err(_) => ("400 Bad Request", "session id must be a number".to_string()),
                }
            } else if path.starts_with("/readyz") {
                let ok = tokio::time::timeout(
                    Duration::from_secs(3),
                    tokio::net::TcpStream::connect((probe_host.as_str(), probe_port)),
                )
                .await
                .map(|r| r.is_ok())
                .unwrap_or(false);
                if ok {
                    ("200 OK", "ready".to_string())
                } else {
                    ("503 Service Unavailable", "backend unreachable".to_string())
                }
            } else if path.starts_with("/healthz") {
                ("200 OK", "ok".to_string())
            } else if path.starts_with("/metrics") {
                ("200 OK", render_metrics())
            } else {
                ("404 Not Found", "not found".to_string())
            };
            let ctype = if body.starts_with('[') || body.starts_with('{') {
                "application/json"
            } else {
                "text/plain"
            };
            let resp = format!(
                "HTTP/1.1 {status}\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes()).await;
        });
    }
}

async fn handle_connection(stream: TcpStream, session_id: u64, config: Config) -> Result<()> {
    let mut tns = TnsStream::new(stream);
    tns.set_idle_timeout(config.idle_timeout);

    // 1. Read CONNECT. A real Oracle listener answers the first CONNECT with a
    // bare RESEND (packet type 11, header only) and reads the CONNECT again
    // before it ACCEPTs — every supported client re-sends on RESEND. Matching
    // this puts python-oracledb thick's NS layer into the state where it will
    // later honour a server-initiated attention/RESET (statement_timeout ->
    // ORA-01013) instead of re-driving the Execute.
    let mut connect = tns.read_packet().await?;
    if connect.packet_type != PacketType::Connect {
        return Err(Error::Protocol("expected CONNECT packet".to_string()));
    }
    // Distinguish the OCI thick client from the thin drivers by CONNECT
    // service options: thick sends `0x0c41`, ODP.NET managed `0x0c01`,
    // python-oracledb thin / ojdbc thin lower still — bit `0x0040` is set only
    // by thick. For a thick client only, answer the first CONNECT with a bare
    // RESEND and read the CONNECT again, as a real listener does; that primes
    // its NS layer to honour a later server-initiated attention
    // (statement_timeout -> ORA-01013) rather than re-driving the Execute.
    // ODP.NET aborts the connection on an unexpected RESEND, so it must not
    // fire for anything but thick.
    let svc_opts = connect
        .payload
        .get(4..6)
        .map(|b| u16::from_be_bytes([b[0], b[1]]))
        .unwrap_or(0);
    if svc_opts & 0x0040 != 0 {
        tns.write_packet(PacketType::Resend, &[]).await?;
        connect = tns.read_packet().await?;
        if connect.packet_type != PacketType::Connect {
            return Err(Error::Protocol(
                "expected CONNECT packet after RESEND".to_string(),
            ));
        }
    }
    debug!(payload_len = connect.payload.len(), "received CONNECT");
    let (desired, minimum, descriptor) = crate::tns::parse_connect_payload(&connect.payload)?;
    debug!(
        "connect desired={} minimum={} descriptor={}",
        desired, minimum, descriptor
    );

    // 2. Send ACCEPT. The version must not exceed what the client offered — OCI
    // (`nsaccept`) rejects `server_version > client_version` with ORA-12592,
    // where the thin drivers tolerate it. The ACCEPT packet itself uses the
    // small-SDU header (2-byte length); the client switches to large SDU after
    // parsing it.
    let version = desired.clamp(300, 319);
    let accept_bytes = build_accept_response(version);
    debug!(payload_len = accept_bytes.len(), "sending ACCEPT");
    tns.write_packet(PacketType::Accept, &accept_bytes).await?;
    tns.set_mode(SduMode::Large);

    // 3. OOB check (protocol >= 318): oracle-rs only sends this when the ACCEPT
    // service options indicate CAN_RECV_ATTENTION (0x0400). We set service options
    // to 0x0001, so no OOB check is performed by the client.

    // 4-6. Pre-auth negotiation, driven by the TTC message type rather than a
    // fixed packet order. `oracle-rs` inlines the connect descriptor in the
    // CONNECT packet and goes straight to Protocol; thin drivers
    // (python-oracledb, JDBC) send a short CONNECT then the `(DESCRIPTION=…)`
    // descriptor as its own Data packet after ACCEPT. Loop until the auth
    // phase-one Function message arrives.
    const TTC_MSG_PROTOCOL: u8 = 0x01;
    const TTC_MSG_DATA_TYPES: u8 = 0x02;
    const TTC_MSG_FUNCTION: u8 = 0x03;
    // Wire behaviour is chosen from the capabilities the client negotiates in
    // the TTC handshake — the `TNS_CCAP_*` / `TNS_RCAP_*` vectors in the
    // DataTypes request and the ANO ("Secure Network Services") exchange —
    // exactly as a real Oracle server chooses it. No driver name is consulted.
    use crate::profile::WireProfile;
    let mut did_na_negotiation = false;
    let mut proto_req = wire::ProtocolRequest::default();
    let mut compile_caps: Vec<u8> = Vec::new();
    let mut runtime_caps: Vec<u8> = Vec::new();
    // Set once the OCI thick client is identified (from its caps, or — before
    // those arrive — from the Protocol-phase heuristic). Selects the OCI TTC
    // dialect for the rest of the session.
    let mut oci_dialect = false;
    let auth1 = loop {
        let pkt = tns.read_packet().await?;
        if pkt.packet_type != PacketType::Data || pkt.payload.len() < 3 {
            return Err(Error::Protocol(
                "expected a DATA packet during negotiation".to_string(),
            ));
        }
        // The ANO / NA ("Secure Network Services", `DE AD BE EF` …) exchange —
        // run by OCI, ojdbc *and* ODP.NET (python-oracledb thin and oracle-rs
        // set `TNS_NSI_DISABLE_NA`). Reply with the null-adapter response a
        // wallet-less server sends. This alone does NOT identify OCI.
        if wire::is_ano_negotiation(&pkt.payload) {
            debug!("NA negotiation packet; replying with null adapters");
            did_na_negotiation = true;
            tns.write_packet(PacketType::Data, &wire::build_ano_negotiation_response())
                .await?;
            continue;
        }
        match pkt.payload[2] {
            b'(' => {
                debug!(
                    len = pkt.payload.len(),
                    "received connect descriptor packet"
                );
            }
            TTC_MSG_PROTOCOL => {
                proto_req = wire::parse_protocol_request(&pkt.payload)?;
                // Only signal available this early: NA + the accepted-versions
                // list. OCI offers `[5]`; ojdbc `[5,4,3,2,1]`; ODP.NET `[]`.
                let probe =
                    WireProfile::new(did_na_negotiation, &proto_req, Vec::new(), Vec::new());
                oci_dialect = probe.probably_oci_at_protocol();
                tns.set_oci_client(oci_dialect);
                debug!(
                    banner = %proto_req.banner,
                    version = proto_req.version,
                    accepted = ?proto_req.accepted_versions,
                    did_na = did_na_negotiation,
                    oci_dialect,
                    "protocol negotiation"
                );
                let resp = if oci_dialect {
                    wire::build_protocol_response_oci()
                } else {
                    wire::build_protocol_response()
                };
                tns.write_packet(PacketType::Data, &resp).await?;
            }
            TTC_MSG_DATA_TYPES => {
                if let Ok((cc, rc)) = wire::parse_data_types_caps(&pkt.payload) {
                    compile_caps = cc;
                    runtime_caps = rc;
                }
                let probe = WireProfile::new(
                    did_na_negotiation,
                    &proto_req,
                    compile_caps.clone(),
                    runtime_caps.clone(),
                );
                // Now authoritative — from `TNS_CCAP_OCI1`.
                if probe.oci_dialect() != oci_dialect {
                    warn!(
                        protocol_guess = oci_dialect,
                        caps_says = probe.oci_dialect(),
                        "OCI-dialect guess disagreed with negotiated caps"
                    );
                }
                oci_dialect = probe.oci_dialect();
                tns.set_oci_client(oci_dialect);
                debug!(
                    field_version = probe.field_version(),
                    oci_dialect,
                    newer_describe_framing = probe.newer_describe_framing(),
                    response_completion = probe.wants_response_completion(),
                    na_without_version_list = probe.na_without_version_list(),
                    "datatypes negotiation"
                );
                let resp = if oci_dialect {
                    wire::build_data_types_response_oci()
                } else if probe.na_without_version_list() {
                    wire::build_data_types_response_na_no_verlist()
                } else {
                    wire::build_data_types_response(&pkt.payload)
                };
                tns.write_packet(PacketType::Data, &resp).await?;
            }
            TTC_MSG_FUNCTION => break pkt,
            other => {
                warn!(
                    msg_type = other,
                    "unexpected TTC message during negotiation"
                );
                return Err(Error::Protocol(format!(
                    "unexpected TTC message 0x{other:02x} before authentication"
                )));
            }
        }
    };
    let profile = WireProfile::new(did_na_negotiation, &proto_req, compile_caps, runtime_caps);
    // Wire-behaviour predicates for the rest of the session, each read from the
    // capabilities the client negotiated (via `WireProfile`) and named for the
    // protocol feature it gates — never for a driver:
    //   `oci_dialect`             — the OCI thick-client TTC dialect (`TNS_CCAP_OCI1`)
    //   `newer_describe_framing`   — the newer row/describe/end-of-call wire shape
    //                               (every `FIELD_VERSION >= 20.1` client, and OCI)
    //   `response_completion`     — client wants explicit end-of-response signals
    //                               (every client except oracle-rs 0.1.7)
    //   `na_without_version_list` — ran NA negotiation but sent an empty
    //                               protocol-version list; needs the shorter
    //                               datatypes response + long-form phase-two auth
    // They still pass as `bool` into the wire/describe builders; threading
    // `&WireProfile` there instead is the remaining cleanup.
    let oci_dialect = profile.oci_dialect();
    let na_without_version_list = profile.na_without_version_list();
    let newer_describe_framing = profile.newer_describe_framing() || oci_dialect;
    let response_completion = profile.wants_response_completion();
    let username = if oci_dialect {
        let (user, pairs) = wire::parse_oci_auth(&auth1.payload);
        debug!(
            "OCI auth phase one user={user:?} pairs={:?}",
            pairs.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>()
        );
        user.ok_or_else(|| Error::Protocol("OCI auth phase one has no username".to_string()))?
    } else {
        let (username, pairs) = wire::parse_auth_phase_one_request(&auth1.payload)?;
        debug!(
            "auth phase one user={} pairs={:?}",
            username,
            pairs.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>()
        );
        username
    };

    sessions_set_user(session_id, &username);

    // Resolve the Oracle username to a pre-declared PostgreSQL password. The
    // login is a challenge/response, so PgSaci must already hold the password to
    // both verify the client's proof and open the backend connection with it.
    // An unknown user with no fallback configured is rejected up front.
    let pg_password = match config.credentials.password_for(&username) {
        Some(p) => p.to_string(),
        None => {
            debug!("no PostgreSQL credential for user {username:?}");
            tns.write_packet(
                PacketType::Data,
                &build_error_response(
                    response_completion,
                    newer_describe_framing,
                    oci_dialect,
                    1017,
                    "invalid username/password",
                ),
            )
            .await?;
            return Ok(());
        }
    };

    // Auth verifier family follows the impersonated release. The 12c PBKDF2
    // verifier is required by python-oracledb thin and modern JDBC thin; the
    // 11g O5LOGON verifier is for older clients.
    let password = pg_password.clone();
    // The 11g O5LOGON (MD5) verifier is what python-oracledb thin negotiates for
    // an 11g server. ojdbc's O5Logon helper is hardwired to O7L multi-round
    // (PBKDF2) and can't do the MD5 path, so serve it the 12c PBKDF2 verifier
    // even under `PGSACI_ORACLE_VERSION=11` — only the banner / release number
    // stay 11g.
    // Both ojdbc thin and ODP.NET managed hardwire O7L multi-round (PBKDF2) and
    // cannot do the 11g O5LOGON (MD5) path, so serve them the 12c verifier even
    // under `PGSACI_ORACLE_VERSION=11` (only the banner / release stay 11g).
    let use_11g =
        config.oracle_version == OracleVersion::V11g && !newer_describe_framing && !oci_dialect;
    let mut auth_state = if use_11g {
        AuthState::new_11g(password)
    } else {
        AuthState::new_12c(password)
    };

    let sesskey = hex_upper(&auth_state.phase_one_sesskey());
    let vfr = hex_upper(&auth_state.vfr_data);
    let response = if use_11g {
        wire::build_auth_phase_one_response_11g(&sesskey, &vfr)
    } else {
        let csk_salt = hex_upper(&auth_state.csk_salt.expect("12c state has CSK salt"));
        if oci_dialect {
            wire::build_auth_phase_one_response_oci(
                &sesskey,
                &vfr,
                &csk_salt,
                auth_state.vgen_count,
                auth_state.sder_count,
            )
        } else if na_without_version_list {
            wire::build_auth_phase_one_response_na_no_verlist(
                &sesskey,
                &vfr,
                &csk_salt,
                auth_state.vgen_count,
                auth_state.sder_count,
            )
        } else {
            wire::build_auth_phase_one_response_12c(
                &sesskey,
                &vfr,
                &csk_salt,
                auth_state.vgen_count,
                auth_state.sder_count,
            )
        }
    };
    tns.write_packet(PacketType::Data, &response).await?;

    // 7. Authentication phase two
    let auth2 = tns.read_packet().await?;
    if auth2.packet_type != PacketType::Data {
        return Err(Error::Protocol(
            "expected auth phase two DATA packet".to_string(),
        ));
    }
    let pairs2 = if oci_dialect {
        wire::parse_oci_auth(&auth2.payload).1
    } else if na_without_version_list {
        wire::parse_auth_phase_two_request_na_no_verlist(&auth2.payload)?.1
    } else {
        wire::parse_auth_phase_two_request(&auth2.payload)?.1
    };

    let mut client_sesskey = None;
    let mut auth_password = None;
    let mut speedy_key = None;
    for (k, v) in pairs2 {
        let s = String::from_utf8_lossy(&v).to_string();
        match k.as_str() {
            "AUTH_SESSKEY" => client_sesskey = Some(s),
            "AUTH_PASSWORD" => auth_password = Some(s),
            "AUTH_PBKDF2_SPEEDY_KEY" => speedy_key = Some(s),
            _ => {}
        }
    }

    let client_sesskey = client_sesskey
        .ok_or_else(|| Error::AuthenticationFailed("missing AUTH_SESSKEY".to_string()))?;
    let auth_password = auth_password
        .ok_or_else(|| Error::AuthenticationFailed("missing AUTH_PASSWORD".to_string()))?;

    if let Err(error) = auth_state.set_client_sesskey(&client_sesskey) {
        tns.write_packet(
            PacketType::Data,
            &build_error_response(
                response_completion,
                newer_describe_framing,
                oci_dialect,
                1017,
                "invalid username/password",
            ),
        )
        .await?;
        return Err(Error::AuthenticationFailed(error));
    }
    if let Err(error) = auth_state.verify_password(&auth_password) {
        tns.write_packet(
            PacketType::Data,
            &build_error_response(
                response_completion,
                newer_describe_framing,
                oci_dialect,
                1017,
                "invalid username/password",
            ),
        )
        .await?;
        return Err(Error::AuthenticationFailed(error));
    }
    if let Some(sk) = speedy_key
        && let Err(error) = auth_state.verify_speedy_key(&sk)
    {
        tns.write_packet(
            PacketType::Data,
            &build_error_response(
                response_completion,
                newer_describe_framing,
                oci_dialect,
                1017,
                "invalid username/password",
            ),
        )
        .await?;
        return Err(Error::AuthenticationFailed(error));
    }

    let svr_response = hex_upper(&auth_state.svr_response());
    let sid = (NEXT_SESSION_ID.load(Ordering::Relaxed) as u32).max(1);
    let auth2_response = if oci_dialect {
        wire::build_auth_phase_two_response_oci(&svr_response)
    } else if na_without_version_list {
        wire::build_auth_phase_two_response_na_no_verlist(
            &svr_response,
            sid,
            config.oracle_version.version_no(),
            config.oracle_version.release(),
        )
    } else {
        wire::build_auth_phase_two_response(
            &svr_response,
            sid,
            config.oracle_version.version_no(),
            config.oracle_version.release(),
        )
    };
    tns.write_packet(PacketType::Data, &auth2_response).await?;

    // 8. Connect to the selected backend using Oracle credentials.
    let backend: Arc<dyn OracleBackend> = match config.backend {
        BackendKind::Postgres => match PostgresBackend::connect(
            &config.pg_host,
            config.pg_port,
            &username,
            &pg_password,
            &config.pg_db,
            config.statement_timeout,
        )
        .await
        {
            Ok(backend) => Arc::new(Arc::new(backend)),
            Err(e) => {
                return backend_connect_error(
                    &mut tns,
                    &e,
                    response_completion,
                    newer_describe_framing,
                    oci_dialect,
                )
                .await;
            }
        },
        BackendKind::MariaDb => match MariaDbBackend::connect(
            &config.pg_host,
            config.pg_port,
            &username,
            &pg_password,
            &config.pg_db,
        )
        .await
        {
            Ok(backend) => Arc::new(Arc::new(backend)),
            Err(e) => {
                return backend_connect_error(
                    &mut tns,
                    &e,
                    response_completion,
                    newer_describe_framing,
                    oci_dialect,
                )
                .await;
            }
        },
    };
    // 9. Main command loop. At most one streamed cursor is active at a time,
    // matching the way OCI/thin clients drive a single statement through
    // Execute + repeated Fetch.
    let mut cursor: Option<Box<dyn OracleCursor>> = None;
    // The Oracle SQL text + bind datatype list of the statement last prepared on
    // this session's cursor. `REEXECUTE` / `REEXECUTE_AND_FETCH` re-run it
    // without re-sending either, so PgSaci has to remember them.
    let mut last_execute: Option<(String, Vec<u8>)> = None;
    // OCI keys its client-side statement cache off the cursor id the server
    // reports; reusing one id for two different statements corrupts that cache
    // (segfault in the OCI library). Hand out a fresh id per Execute, starting
    // at 2 the way a real Oracle session does. Thin / jdbc clients ignore it.
    let mut cursor_id: u16 = if oci_dialect { 2 } else { 1 };
    // OCI caches prepared statements by SQL text and expects the server to
    // report the SAME cursor id for a repeated statement; a fresh id on a
    // re-execute mismatches its cache and it breaks the call. Map SQL -> id.
    let mut oci_cursor_ids: std::collections::HashMap<String, u16> =
        std::collections::HashMap::new();
    let mut oci_next_cursor_id: u16 = 3;
    // OCI `REEXECUTE` (0x04) / `REEXECUTE_AND_FETCH` (0x4e) carry no SQL: the
    // client re-runs a statement it prepared earlier, identified by the request
    // sequence byte it also used on that statement's original `0x5E` Execute.
    // Map that seq -> (SQL, bind datatypes) so a re-execute resolves to the
    // right statement even when another statement ran in between.
    let mut oci_seq_to_sql: std::collections::HashMap<u8, (String, Vec<u8>)> =
        std::collections::HashMap::new();
    // Reverse of `oci_cursor_ids`: the SQL + bind datatypes for a server cursor
    // id, so a `0x04` / `0x4e` that names its cursor resolves exactly (the seq
    // byte is unreliable — a re-execute after other calls uses a fresh seq).
    let mut oci_id_to_sql: std::collections::HashMap<u16, (String, Vec<u8>)> =
        std::collections::HashMap::new();
    // Running total of rows handed to an OCI client for the current cursor
    // (Execute reply + every Fetch since); its end-of-call echoes this.
    let mut oci_rows_sent: u64 = 0;
    // Sequence byte of the most recent TTC FUNCTION message. A client BREAK
    // arrives as a marker packet with no seq of its own; the OCI error frame
    // returned during break recovery still needs one.
    let mut last_req_seq: u8 = 0;
    loop {
        let req = tns.read_packet().await?;
        match req.packet_type {
            PacketType::Data => {
                if req.payload.len() < 4 {
                    continue;
                }
                // ojdbc prefixes deferred operations (mainly CLOSE_CURSORS) as a
                // PIGGYBACK message (type 0x11) in the same DATA packet as the
                // real FUNCTION message. PgSaci manages its single streamed
                // cursor itself, so skip the piggyback and act on the embedded
                // `0x03` message.
                let payload: std::borrow::Cow<[u8]> = if req.payload.get(2) == Some(&0x11) {
                    match wire::strip_piggyback(&req.payload) {
                        Some(inner) => {
                            let mut framed = Vec::with_capacity(inner.len() + 2);
                            framed.extend_from_slice(&[0, 0]);
                            framed.extend_from_slice(inner);
                            std::borrow::Cow::Owned(framed)
                        }
                        None => std::borrow::Cow::Borrowed(&req.payload[..]),
                    }
                } else {
                    std::borrow::Cow::Borrowed(&req.payload[..])
                };
                if payload.len() < 4 {
                    continue;
                }
                let msg_type = payload[2];
                let func_code = payload[3];
                // TTC sequence byte; ojdbc thin rejects an end-of-call whose
                // call-number field does not echo it.
                let req_seq = payload.get(4).copied().unwrap_or(0);
                if msg_type == 0x03 {
                    last_req_seq = req_seq;
                }
                // 0x5E Execute, 0x4E REEXECUTE_AND_FETCH, 0x04 REEXECUTE.
                if msg_type == 0x03 && matches!(func_code, 0x5E | 0x4E | 0x04 | 0x05) {
                    STATEMENTS_TOTAL.fetch_add(1, Ordering::Relaxed);
                }
                if msg_type == 0x03 && matches!(func_code, 0x5E | 0x4E | 0x04) {
                    // Execute, or re-execute of the statement already prepared on
                    // the cursor (`REEXECUTE` / `REEXECUTE_AND_FETCH` carry new
                    // bind values but no SQL and no describe).
                    let parsed = if func_code == 0x5E && oci_dialect {
                        wire::parse_execute_request_oci(&payload).or_else(|e| {
                            // A `0x5E` that re-parses a cached statement (a
                            // bind's type changed) omits the SQL — resolve it by
                            // the cursor id the frame names, then decode the
                            // (fresh) bind descriptors it still carries.
                            let hint = wire::parse_reparse_cursor_id_oci(&payload)
                                .and_then(|id| oci_id_to_sql.get(&id))
                                .or(last_execute.as_ref());
                            match hint {
                                Some((sql, types)) => {
                                    wire::parse_reexecute_request_oci_ex(&payload, sql, types, true)
                                }
                                None => Err(e),
                            }
                        })
                    } else if func_code == 0x5E {
                        wire::parse_execute_request(&payload)
                    } else {
                        // Resolve the statement a bare REEXECUTE re-runs.
                        //
                        // OCI: the cursor id the frame names is authoritative —
                        // `oci_id_to_sql` is keyed with the exact id PgSaci
                        // assigned and echoed in that statement's original `0x5E`
                        // response, so the client names it verbatim. Fall back to
                        // the seq byte, then the most-recent statement, only when
                        // the frame carries no id (id 0 = "current cursor").
                        //
                        // Thin: `oci_id_to_sql` is never populated and
                        // `parse_reexecute_cursor_id_oci` decodes an OCI
                        // fixed-width id, so `by_id` is always None. The seq-byte
                        // map must NOT be consulted here: `req_seq` is a wrapping
                        // u8, and after a large multi-batch fetch a later
                        // REEXECUTE's seq can alias a slot last written by an
                        // unrelated bind-carrying statement — PgSaci would then
                        // load that statement's datatype list and demand a bind
                        // RowData marker the query re-execute never sends
                        // (ORA-01008, seen by bench `big_fetch_25k_rows`). Thin
                        // drivers re-parse (a full `0x5E` with SQL) on every
                        // statement change and PgSaci streams a single cursor, so
                        // `last_execute` alone is the correct resolution.
                        let named_id = wire::parse_reexecute_cursor_id_oci(&payload, func_code);
                        let resolved = if oci_dialect {
                            named_id
                                .and_then(|id| oci_id_to_sql.get(&id))
                                .or_else(|| oci_seq_to_sql.get(&req_seq))
                                .or(last_execute.as_ref())
                        } else {
                            last_execute.as_ref()
                        };
                        if std::env::var("PGSACI_OCI_DEBUG").is_ok() {
                            eprintln!(
                                "OCI-DEBUG reexec func=0x{:02x} seq=0x{:02x} named_id={:?} \
                                 id_map={:?} resolved={:?}",
                                func_code,
                                req_seq,
                                named_id,
                                oci_id_to_sql
                                    .iter()
                                    .map(|(k, v)| (*k, v.0.chars().take(30).collect::<String>()))
                                    .collect::<Vec<_>>(),
                                resolved.map(|(s, _)| s.chars().take(40).collect::<String>()),
                            );
                        }
                        match resolved {
                            Some((sql, types)) if oci_dialect => {
                                wire::parse_reexecute_request_oci(&payload, sql, types)
                            }
                            Some((sql, types)) => {
                                wire::parse_reexecute_request(&payload, sql, types)
                            }
                            None => Err(Error::Protocol(
                                "reexecute with no prepared statement".to_string(),
                            )),
                        }
                    };
                    let execute = match parsed {
                        Ok(execute) => execute,
                        Err(e) => {
                            // A `0x5E` that carries no SQL text is ojdbc thin
                            // re-executing a statement it expects PgSaci to have
                            // cached on a server-side cursor (its implicit
                            // statement cache). PgSaci does not keep that cache
                            // on the thin/jdbc path, so answer the Oracle-correct
                            // "cursor has no statement" — ojdbc then re-parses
                            // and re-sends the SQL, which succeeds. This is
                            // expected traffic, not a fault, so it stays at
                            // debug level. Any other parse failure is a real
                            // malformed frame and keeps the louder `warn!`.
                            let stmt_cache_reexec =
                                func_code == 0x5E && wire::execute_frame_has_no_sql(&payload);
                            if stmt_cache_reexec {
                                debug!(
                                    "0x5E statement-cache re-execute with no cached \
                                        statement; asking client to re-parse (ORA-01003)"
                                );
                            } else {
                                warn!("could not parse execute request: {}", e);
                            }
                            if std::env::var_os("PGSACI_LOG_SQL").is_some() {
                                let n = payload.len().min(400);
                                eprintln!(
                                    "PGSACI_SQL  BAD-EXEC func=0x{:02x} len={} payload[..{}]={:02x?}",
                                    func_code,
                                    payload.len(),
                                    n,
                                    &payload[..n]
                                );
                            }
                            let (code, msg): (u32, &str) = if stmt_cache_reexec {
                                (1003, "ORA-01003: no statement parsed")
                            } else {
                                (1008, "")
                            };
                            let text = if msg.is_empty() {
                                e.to_string()
                            } else {
                                msg.to_string()
                            };
                            write_error_response(
                                &mut tns,
                                oci_dialect,
                                response_completion,
                                newer_describe_framing,
                                code,
                                &text,
                                0,
                                req_seq,
                            )
                            .await?;
                            continue;
                        }
                    };
                    if oci_dialect && func_code != 0x5E {
                        // Re-execute: reuse the cursor id assigned to this SQL.
                        if let Some(id) = oci_cursor_ids.get(&execute.sql) {
                            cursor_id = *id;
                        }
                    }
                    if func_code == 0x5E && !execute.sql.is_empty() {
                        last_execute = Some((execute.sql.clone(), execute.bind_types.clone()));
                        oci_seq_to_sql
                            .insert(req_seq, (execute.sql.clone(), execute.bind_types.clone()));
                        if oci_dialect {
                            cursor_id =
                                *oci_cursor_ids
                                    .entry(execute.sql.clone())
                                    .or_insert_with(|| {
                                        let id = oci_next_cursor_id;
                                        oci_next_cursor_id =
                                            oci_next_cursor_id.wrapping_add(1).max(3);
                                        id
                                    });
                            oci_id_to_sql.insert(
                                cursor_id,
                                (execute.sql.clone(), execute.bind_types.clone()),
                            );
                        }
                    }

                    // `RETURNING … INTO :out` DML: strip the `INTO` clause so
                    // PostgreSQL sees a plain `RETURNING`, run it, and (for
                    // python-oracledb thin) hand the returned values back as OUT
                    // bind data. Other drivers frame OUT binds differently, so
                    // they get an "unimplemented" error rather than a silent drop.
                    if let Some(ri) = wire::split_returning_into(&execute.sql) {
                        if !(response_completion && !newer_describe_framing) {
                            write_error_response(
                                &mut tns,
                                oci_dialect,
                                response_completion,
                                newer_describe_framing,
                                3001,
                                "RETURNING ... INTO bind is not supported for this driver",
                                0,
                                req_seq,
                            )
                            .await?;
                            continue;
                        }
                        let bound = match wire::bind_postgres_parameters(
                            &ri.sql_without_into,
                            &execute.binds,
                        ) {
                            Ok(b) => b,
                            Err(e) => {
                                write_error_response(
                                    &mut tns,
                                    oci_dialect,
                                    response_completion,
                                    newer_describe_framing,
                                    1008,
                                    &e.to_string(),
                                    0,
                                    req_seq,
                                )
                                .await?;
                                continue;
                            }
                        };
                        let pg_sql = match crate::translate::oracle_to_postgres(&bound.sql) {
                            Ok(s) => s,
                            Err(e) => {
                                write_error_response(
                                    &mut tns,
                                    oci_dialect,
                                    response_completion,
                                    newer_describe_framing,
                                    900,
                                    &e.to_string(),
                                    0,
                                    req_seq,
                                )
                                .await?;
                                continue;
                            }
                        };
                        log_sql_translation(&bound.sql, &pg_sql);
                        let input_bind_count = execute
                            .bind_types
                            .len()
                            .checked_sub(ri.out_bind_count)
                            .unwrap_or(bound.binds.len());
                        match race_break(
                            &mut tns,
                            backend.as_ref(),
                            backend.execute_returning(&pg_sql, &bound.binds),
                        )
                        .await?
                        {
                            Ok((rows, per_col)) => {
                                let resp = wire::build_returning_response(
                                    input_bind_count,
                                    &per_col,
                                    rows,
                                );
                                tns.write_packet(PacketType::Data, &resp).await?;
                            }
                            Err(e) => {
                                warn!("postgres RETURNING error: {}", e);
                                let (code, message, error_pos) = oracle_error_for_pos(&e);
                                write_error_response(
                                    &mut tns,
                                    oci_dialect,
                                    response_completion,
                                    newer_describe_framing,
                                    code,
                                    &message,
                                    error_pos,
                                    req_seq,
                                )
                                .await?;
                            }
                        }
                        continue;
                    }

                    let bound = match wire::bind_postgres_parameters(&execute.sql, &execute.binds) {
                        Ok(bound) => bound,
                        Err(e) => {
                            write_error_response(
                                &mut tns,
                                oci_dialect,
                                response_completion,
                                newer_describe_framing,
                                1008,
                                &e.to_string(),
                                0,
                                req_seq,
                            )
                            .await?;
                            continue;
                        }
                    };
                    debug!(bind_count = execute.binds.len(), "executing statement");

                    // Translate Oracle-specific structural syntax before executing it.
                    let pg_sql = match crate::translate::oracle_to_postgres(&bound.sql) {
                        Ok(sql) => sql,
                        Err(e) => {
                            write_error_response(
                                &mut tns,
                                oci_dialect,
                                response_completion,
                                newer_describe_framing,
                                900,
                                &e.to_string(),
                                0,
                                req_seq,
                            )
                            .await?;
                            continue;
                        }
                    };
                    log_sql_translation(&bound.sql, &pg_sql);
                    if std::env::var("PGSACI_OCI_DEBUG").is_ok() {
                        eprintln!(
                            "OCI-DEBUG exec func=0x{:02x} seq=0x{:02x} is_query={} is_ddl={} sql={:?} raw[0..24]={:02x?}",
                            func_code,
                            req_seq,
                            is_query_statement(&pg_sql),
                            is_ddl_statement(&pg_sql),
                            pg_sql.chars().take(50).collect::<String>(),
                            &payload[..payload.len().min(48)],
                        );
                    }
                    // Array bind / batch DML (`executemany`, JDBC batch): one SQL,
                    // many value rows. Run each row against the same statement and
                    // report the summed row count, matching Oracle's batch execute.
                    if execute.num_iters > 1
                        && execute.bind_rows.len() > 1
                        && !is_query_statement(&pg_sql)
                    {
                        let is_ddl = is_ddl_statement(&pg_sql);
                        let mut total: u64 = 0;
                        let mut failed: Option<Error> = None;
                        for row in &execute.bind_rows {
                            let row_bound = match wire::bind_postgres_parameters(&execute.sql, row)
                            {
                                Ok(b) => b,
                                Err(e) => {
                                    failed = Some(e);
                                    break;
                                }
                            };
                            let res = if is_ddl {
                                backend.execute_ddl(&pg_sql, &row_bound.binds).await
                            } else {
                                race_break(
                                    &mut tns,
                                    backend.as_ref(),
                                    backend.execute_simple(&pg_sql, &row_bound.binds),
                                )
                                .await?
                            };
                            match res {
                                Ok(n) => total += n,
                                Err(e) => {
                                    failed = Some(e);
                                    break;
                                }
                            }
                        }
                        match failed {
                            None => {
                                let resp = if oci_dialect && is_ddl {
                                    wire::build_ddl_response_oci(cursor_id, req_seq)
                                } else if oci_dialect {
                                    wire::build_dml_response_oci(
                                        total,
                                        cursor_id,
                                        req_seq,
                                        wire::DmlKind::of(&pg_sql),
                                    )
                                } else if newer_describe_framing {
                                    wire::build_dml_response_jdbc(total, req_seq)
                                } else {
                                    build_dml_response(total)
                                };
                                tns.write_packet(PacketType::Data, &resp).await?;
                            }
                            Some(e) => {
                                warn!("postgres batch DML error: {}", e);
                                let (code, message, error_pos) = oracle_error_for_pos(&e);
                                write_error_response(
                                    &mut tns,
                                    oci_dialect,
                                    response_completion,
                                    newer_describe_framing,
                                    code,
                                    &message,
                                    error_pos,
                                    req_seq,
                                )
                                .await?;
                            }
                        }
                        continue;
                    }
                    if is_query_statement(&pg_sql) {
                        // Abandon any half-consumed prior cursor.
                        if let Some(mut old) = cursor.take() {
                            old.finish().await;
                        }
                        // Rows are pulled from PostgreSQL incrementally and
                        // delivered via Execute + client-driven Fetch. The first
                        // batch honors the client's prefetch/array size.
                        let batch = if oci_dialect {
                            // OCI's row buffer is sized to its prefetch count (2
                            // by default); it reads exactly that many from the
                            // Execute reply and pulls the rest with Fetch.
                            2
                        } else if execute.prefetch == 0 {
                            100
                        } else {
                            (execute.prefetch as usize).min(50_000)
                        };
                        let opened = race_break(
                            &mut tns,
                            backend.as_ref(),
                            backend.open_cursor(
                                &pg_sql,
                                &bound.binds,
                                crate::backend::DescribeCaps::for_client(
                                    response_completion,
                                    newer_describe_framing,
                                    oci_dialect,
                                    profile.newer_describe_framing(),
                                ),
                            ),
                        )
                        .await?;
                        match opened {
                            Ok(mut cur) => {
                                match race_break(&mut tns, backend.as_ref(), cur.next_batch(batch))
                                    .await?
                                {
                                    Ok(rows) => {
                                        let more = !cur.is_exhausted();
                                        oci_rows_sent = rows.len() as u64;
                                        let response = if oci_dialect {
                                            // 0x4e REEXECUTE_AND_FETCH omits the
                                            // leading DESCRIBE_INFO.
                                            wire::build_query_response_oci_ex(
                                                cur.columns(),
                                                &rows,
                                                cursor_id,
                                                more,
                                                func_code == 0x5E,
                                            )
                                        } else {
                                            build_query_response(
                                                cur.columns(),
                                                &rows,
                                                cursor_id,
                                                more,
                                                response_completion,
                                                newer_describe_framing,
                                                req_seq,
                                            )
                                        };
                                        tns.write_packet(PacketType::Data, &response).await?;
                                        if more {
                                            cursor = Some(cur);
                                        } else {
                                            cur.finish().await;
                                        }
                                    }
                                    Err(e) => {
                                        warn!("postgres query error: {}", e);
                                        let (code, message, error_pos) = oracle_error_for_pos(&e);
                                        write_error_response(
                                            &mut tns,
                                            oci_dialect,
                                            response_completion,
                                            newer_describe_framing,
                                            code,
                                            &message,
                                            error_pos,
                                            req_seq,
                                        )
                                        .await?;
                                    }
                                }
                            }
                            Err(e) => {
                                warn!("postgres query error: {}", e);
                                let (code, message, error_pos) = oracle_error_for_pos(&e);
                                write_error_response(
                                    &mut tns,
                                    oci_dialect,
                                    response_completion,
                                    newer_describe_framing,
                                    code,
                                    &message,
                                    error_pos,
                                    req_seq,
                                )
                                .await?;
                            }
                        }
                    } else {
                        let result = if is_ddl_statement(&pg_sql) {
                            backend.execute_ddl(&pg_sql, &bound.binds).await
                        } else {
                            race_break(
                                &mut tns,
                                backend.as_ref(),
                                backend.execute_simple(&pg_sql, &bound.binds),
                            )
                            .await?
                        };
                        match result {
                            Ok(rows_affected) => {
                                let resp = if oci_dialect && func_code == 0x04 {
                                    // REEXECUTE of a cached statement: the client
                                    // expects the compact end-of-call ack, not a
                                    // fresh row-header / describe frame.
                                    wire::build_reexecute_response_oci(rows_affected, req_seq)
                                } else if oci_dialect && is_ddl_statement(&pg_sql) {
                                    wire::build_ddl_response_oci(cursor_id, req_seq)
                                } else if oci_dialect {
                                    wire::build_dml_response_oci(
                                        rows_affected,
                                        cursor_id,
                                        req_seq,
                                        wire::DmlKind::of(&pg_sql),
                                    )
                                } else if newer_describe_framing {
                                    wire::build_dml_response_jdbc(rows_affected, req_seq)
                                } else {
                                    build_dml_response(rows_affected)
                                };
                                tns.write_packet(PacketType::Data, &resp).await?;
                            }
                            Err(e) => {
                                warn!("postgres DML error: {}", e);
                                let (code, message, error_pos) = oracle_error_for_pos(&e);
                                write_error_response(
                                    &mut tns,
                                    oci_dialect,
                                    response_completion,
                                    newer_describe_framing,
                                    code,
                                    &message,
                                    error_pos,
                                    req_seq,
                                )
                                .await?;
                            }
                        }
                    }
                } else if msg_type == 0x03 && func_code == 0x05 {
                    // Fetch: next batch from the open cursor.
                    let (_cid, req_rows) = if oci_dialect {
                        wire::parse_fetch_request_oci(&payload).unwrap_or((cursor_id, 100))
                    } else {
                        wire::parse_fetch_request(&payload).unwrap_or((cursor_id, 100))
                    };
                    let batch = if oci_dialect {
                        (req_rows.clamp(1, 500)) as usize
                    } else {
                        (req_rows.clamp(1, 50_000)) as usize
                    };
                    match cursor.as_mut() {
                        Some(cur) => {
                            match race_break(&mut tns, backend.as_ref(), cur.next_batch(batch))
                                .await?
                            {
                                Ok(rows) => {
                                    let more = !cur.is_exhausted();
                                    oci_rows_sent = oci_rows_sent.saturating_add(rows.len() as u64);
                                    let response = if oci_dialect {
                                        wire::build_fetch_response_oci(
                                            cur.columns(),
                                            &rows,
                                            req_rows,
                                            cursor_id,
                                            more,
                                            oci_rows_sent,
                                        )
                                    } else if newer_describe_framing {
                                        wire::build_fetch_response_jdbc(
                                            &rows, cursor_id, more, req_seq,
                                        )
                                    } else {
                                        wire::build_fetch_response(
                                            &rows,
                                            cursor_id,
                                            more,
                                            response_completion,
                                        )
                                    };
                                    tns.write_packet(PacketType::Data, &response).await?;
                                    if !more && let Some(mut done) = cursor.take() {
                                        done.finish().await;
                                    }
                                }
                                Err(e) => {
                                    if let Some(mut done) = cursor.take() {
                                        done.finish().await;
                                    }
                                    let (code, message, error_pos) = oracle_error_for_pos(&e);
                                    write_error_response(
                                        &mut tns,
                                        oci_dialect,
                                        response_completion,
                                        newer_describe_framing,
                                        code,
                                        &message,
                                        error_pos,
                                        req_seq,
                                    )
                                    .await?;
                                }
                            }
                        }
                        None => {
                            // A fetch after the cursor is exhausted/closed. OCI
                            // clients will stall on `call_timeout` if handed the
                            // lenient frame, so give them the OCI empty-complete
                            // fetch shape.
                            let response = if oci_dialect {
                                wire::build_fetch_response_oci(
                                    &[],
                                    &[],
                                    req_rows,
                                    cursor_id,
                                    false,
                                    oci_rows_sent,
                                )
                            } else {
                                wire::build_fetch_response(&[], 0, false, response_completion)
                            };
                            tns.write_packet(PacketType::Data, &response).await?;
                        }
                    }
                } else if msg_type == 0x03 && func_code == 0x09 {
                    // Logoff. ojdbc thin does a synchronous LOGOFF RPC and waits
                    // for the end-of-call reply before closing the socket;
                    // dropping it straight away surfaces as ORA-03113 client-side.
                    if let Some(mut cur) = cursor.take() {
                        cur.finish().await;
                    }
                    if oci_dialect {
                        let _ = tns
                            .write_packet(PacketType::Data, &wire::build_logoff_response_oci())
                            .await;
                    } else if newer_describe_framing {
                        let _ = tns
                            .write_packet(
                                PacketType::Data,
                                &wire::build_dml_response_jdbc(0, req_seq),
                            )
                            .await;
                    }
                    break;
                } else if msg_type == 0x03 && matches!(func_code, 0x0e | 0x0f) {
                    // COMMIT (14) / ROLLBACK (15): thin drivers issue these as
                    // their own TTC function, not as an Execute.
                    let verb = if func_code == 0x0e {
                        "COMMIT"
                    } else {
                        "ROLLBACK"
                    };
                    if let Some(mut cur) = cursor.take() {
                        cur.finish().await;
                    }
                    match backend.execute_simple(verb, &[]).await {
                        Ok(_) => {
                            let resp = if oci_dialect {
                                wire::build_txn_response_oci()
                            } else if newer_describe_framing {
                                wire::build_dml_response_jdbc(0, req_seq)
                            } else {
                                wire::build_dml_response(0)
                            };
                            tns.write_packet(PacketType::Data, &resp).await?;
                        }
                        Err(e) => {
                            let (code, message, error_pos) = oracle_error_for_pos(&e);
                            write_error_response(
                                &mut tns,
                                oci_dialect,
                                response_completion,
                                newer_describe_framing,
                                code,
                                &message,
                                error_pos,
                                req_seq,
                            )
                            .await?;
                        }
                    }
                } else if msg_type == 0x03 && func_code == 0x3b {
                    // OVERSION (func 59): ojdbc thin asks for the server banner
                    // and packed release number during getMetaData(); OCI issues
                    // it right after auth.
                    let resp = if oci_dialect {
                        wire::build_oversion_response_oci(config.oracle_version.banner())
                    } else {
                        wire::build_oversion_response(
                            config.oracle_version.banner(),
                            config.oracle_version.version_no(),
                        )
                    };
                    tns.write_packet(PacketType::Data, &resp).await?;
                } else {
                    // Any other TTC function (PING 147, CLOSE_CURSORS 105,
                    // SET_SCHEMA 152, SET_END_TO_END 135, …): acknowledge with a
                    // bare, error-free end-of-call rather than a query shape.
                    let resp = if newer_describe_framing {
                        wire::build_dml_response_jdbc(0, req_seq)
                    } else {
                        wire::build_dml_response(0)
                    };
                    tns.write_packet(PacketType::Data, &resp).await?;
                }
            }
            PacketType::Marker => {
                // marker payload byte 3: 1 = BREAK, 2 = RESET.
                let marker_type = req.payload.get(2).copied().unwrap_or(0);
                tns.write_packet(PacketType::Marker, &[0x02, 0x00, 0x02])
                    .await?;
                if marker_type == 0x02 {
                    // After the client's RESET, a driver's break-recovery blocks
                    // waiting for an error Data packet that explains why the
                    // call was interrupted. Without it the OCI client re-sends
                    // RESET markers forever (and thin `_reset()` just hangs).
                    // Answer with ORA-01013.
                    if oci_dialect {
                        tns.write_packet(
                            PacketType::Data,
                            &wire::build_timeout_error_response_oci(last_req_seq),
                        )
                        .await?;
                    } else {
                        tns.write_packet(
                            PacketType::Data,
                            &build_error_response(
                                response_completion,
                                newer_describe_framing,
                                oci_dialect,
                                1013,
                                "user requested cancel of current operation",
                            ),
                        )
                        .await?;
                    }
                }
            }
            PacketType::Abort | PacketType::Null => {
                break;
            }
            _ => {
                warn!("unexpected packet type {:?}", req.packet_type);
            }
        }
    }

    Ok(())
}

/// When `PGSACI_LOG_SQL` is set, print every Oracle→Postgres translation to
/// stderr. Diagnostic only — the app integration harness turns this on to see
/// exactly what SQL each failing query became.
fn log_sql_translation(oracle_sql: &str, pg_sql: &str) {
    if std::env::var_os("PGSACI_LOG_SQL").is_some() {
        let o = oracle_sql.split_whitespace().collect::<Vec<_>>().join(" ");
        let p = pg_sql.split_whitespace().collect::<Vec<_>>().join(" ");
        eprintln!("PGSACI_SQL  IN : {o}");
        eprintln!("PGSACI_SQL  OUT: {p}");
    }
}

fn is_query_statement(sql: &str) -> bool {
    // A parenthesised leading subquery — `(SELECT ...) UNION (SELECT ...)`,
    // `(SELECT ...)` — is still a query. Strip leading `(` and whitespace before
    // looking at the first keyword.
    let keyword = sql
        .trim_start()
        .trim_start_matches(|c: char| c == '(' || c.is_whitespace())
        .split_ascii_whitespace()
        .next()
        .unwrap_or_default();
    if keyword.eq_ignore_ascii_case("select")
        || keyword.eq_ignore_ascii_case("show")
        || keyword.eq_ignore_ascii_case("explain")
        || keyword.eq_ignore_ascii_case("values")
    {
        return true;
    }
    // A CTE is a read unless it embeds DML (a data-modifying CTE), in which case
    // the client issued it via the DML path and expects a DML-shaped response.
    if keyword.eq_ignore_ascii_case("with") {
        let upper = sql.to_ascii_uppercase();
        return !(upper.contains("INSERT ")
            || upper.contains("UPDATE ")
            || upper.contains("DELETE ")
            || upper.contains("MERGE "));
    }
    false
}

async fn backend_connect_error(
    tns: &mut TnsStream,
    error: &Error,
    response_completion: bool,
    newer_describe_framing: bool,
    oci_dialect: bool,
) -> Result<()> {
    let (code, message, error_pos) = oracle_error_for_pos(error);
    tns.write_packet(
        PacketType::Data,
        &build_error_response_at(
            response_completion,
            newer_describe_framing,
            oci_dialect,
            code,
            &message,
            error_pos,
        ),
    )
    .await?;
    Err(Error::Postgres(error.to_string()))
}

/// Await `fut`, but if the client sends a TNS Marker (OCIBreak / Ctrl-C) while
/// it is still running, cancel the in-flight backend statement and keep waiting
/// for `fut` to unwind (it will surface SQLSTATE `57014` → ORA-01013). The
/// marker is acknowledged per protocol. An `Err` return is a client I/O error
/// and should tear the session down.
async fn race_break<T>(
    tns: &mut TnsStream,
    backend: &dyn OracleBackend,
    fut: impl std::future::Future<Output = T>,
) -> Result<T> {
    tokio::pin!(fut);
    loop {
        tokio::select! {
            biased;
            out = &mut fut => return Ok(out),
            pkt = tns.read_packet() => match pkt {
                Ok(p) if p.packet_type == PacketType::Marker => {
                    warn!("client break received; cancelling backend statement");
                    backend.cancel().await;
                    tns.write_packet(PacketType::Marker, &[0x02, 0x00, 0x02]).await?;
                }
                Ok(p) => warn!(
                    "ignoring unexpected {:?} packet received mid-statement",
                    p.packet_type
                ),
                Err(e) => return Err(e),
            }
        }
    }
}

/// Send a TTC error to the client, choosing the wire form for the driver.
///
/// An OCI thick client raises an ordinary error (parse failure, constraint
/// violation, trigger RAISE) straight from an inline `0x04` end-of-call DATA
/// frame — no marker exchange. An earlier revision ran a BREAK/RESET marker
/// dance here and python-oracledb thick re-drove the same Execute, eventually
/// wedging the session, so those are delivered inline.
///
/// ORA-01013 from a server-side PG `statement_timeout` is the hard case: the
/// thick client special-cases a *received* ORA-01013 as a stale cancel and
/// re-drives the Execute regardless. `ALTER SYSTEM CANCEL SQL` on a live
/// server delivers it via a TCP urgent byte + a single in-band RESET marker +
/// the error frame; PgSaci reproduces that shape, but the thick client still
/// re-drives (its thin path and oracle-rs accept it fine). Left as a known
/// gap — see `build_timeout_error_response_oci`.
#[allow(clippy::too_many_arguments)]
async fn write_error_response(
    tns: &mut TnsStream,
    oci_dialect: bool,
    response_completion: bool,
    newer_describe_framing: bool,
    code: u32,
    message: &str,
    error_pos: u16,
    req_seq: u8,
) -> Result<()> {
    if oci_dialect {
        if code == 1013 {
            // Best-effort attention handshake for a server-initiated cancel,
            // matching a live `ALTER SYSTEM CANCEL SQL` capture: TCP urgent
            // byte, one in-band RESET marker, wait for the client's echo, then
            // the byte-exact ORA-01013 end-of-call frame. python-oracledb thick
            // still re-drives; the thin path accepts it.
            tns.send_urgent_byte(0x21).await;
            tns.write_oci_marker(0x02).await?; // RESET (flag 0x20)
            tns.drain_markers(1).await?; // client's RESET echo
            tns.write_packet(
                PacketType::Data,
                &wire::build_timeout_error_response_oci(req_seq),
            )
            .await
        } else {
            tns.write_packet(
                PacketType::Data,
                &wire::build_error_response_oci(code, message, error_pos, req_seq),
            )
            .await
        }
    } else {
        tns.write_packet(
            PacketType::Data,
            &build_error_response_at(
                response_completion,
                newer_describe_framing,
                false,
                code,
                message,
                error_pos,
            ),
        )
        .await
    }
}

fn is_ddl_statement(sql: &str) -> bool {
    matches!(
        sql.split_whitespace()
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase()
            .as_str(),
        "CREATE" | "ALTER" | "DROP" | "TRUNCATE" | "RENAME" | "COMMENT" | "REFRESH"
    )
}

#[cfg(test)]
fn oracle_error_for(error: &Error) -> (u32, String) {
    let (code, message, _pos) = oracle_error_for_pos(error);
    (code, message)
}

/// Map a backend [`Error`] to an Oracle `(error_code, message, error_pos)`.
/// `error_pos` is the 1-based statement character position the PostgreSQL
/// server reported (0 = none), for the Oracle `error_pos` field.
fn oracle_error_for_pos(error: &Error) -> (u32, String, u16) {
    BACKEND_ERRORS_TOTAL.fetch_add(1, Ordering::Relaxed);
    // Non-backend errors keep their own text and a generic code.
    let (detail, position) = match error {
        Error::Postgres(detail) => (detail.clone(), 0u16),
        Error::PgStatement { detail, position } => (
            detail.clone(),
            position.unwrap_or(0).min(u16::MAX as u32) as u16,
        ),
        other => return (900, other.to_string(), 0),
    };
    let detail = &detail;

    // `pg_error_detail` formats backend errors as "<sqlstate>: <message>".
    let (sqlstate, message) = detail
        .split_once(": ")
        .map(|(s, m)| (s.trim(), m.to_string()))
        .unwrap_or(("", detail.clone()));

    let lower = message.to_ascii_lowercase();
    let code = match sqlstate {
        "42P01" | "42704" => 942,            // table/view/object does not exist
        "42703" | "42883" => 904,            // invalid identifier / no such function
        "42P07" | "42710" => 955,            // name is already used by an existing object
        "42P06" | "3F000" => 1918,           // schema does not exist  (approx: user/schema)
        "42702" => 918,                      // column ambiguously defined
        "42803" => 979,                      // not a GROUP BY expression
        "42P18" | "42809" => 902,            // invalid datatype / wrong object kind
        "0A000" => 3001,                     // unimplemented feature
        "2201B" => 12726,                    // invalid regular expression
        "23505" => 1,                        // unique constraint violated
        "23503" => 2291,                     // parent key not found
        "23502" => 1400,                     // cannot insert NULL
        "23514" => 2290,                     // check constraint violated
        "22001" => 12899,                    // value too large for column
        "22003" => 1438,                     // value larger than specified precision
        "22012" => 1476,                     // divisor is equal to zero
        "22P02" => 1722,                     // invalid number
        "22007" | "22008" | "22P03" => 1858, // not a valid <datetime> / conversion
        "21000" => 1427,                     // single-row subquery returns more than one row
        "40001" => 8177,                     // can't serialize access for this transaction
        "40P01" => 60,                       // deadlock detected
        "55P03" => 54,                       // resource busy (NOWAIT)
        "57014" => 1013,                     // user requested cancel of current operation
        "53300" | "53400" | "08004" | "08001" => 18, // maximum number of sessions
        "57P01" | "57P02" | "57P03" => 3113, // backend terminated/restarting
        "08006" => 3135,                     // connection failure during operation
        "55000" => 8002,                     // CURRVAL before NEXTVAL
        "42601" if lower.contains("target columns") => 947, // not enough values
        "42601" => 900,                      // generic syntax error
        _ if lower.contains("currval of sequence") => 8002,
        _ if lower.contains("connection closed")
            || lower.contains("broken pipe")
            || lower.contains("connection reset") =>
        {
            3113
        }
        _ => 900,
    };
    // Client renders `ORA-nnnnn: <message>`; don't repeat the SQLSTATE in it.
    (code, message, position)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;

    #[test]
    fn maps_backend_disconnects_to_oracle_session_errors() {
        assert_eq!(
            oracle_error_for(&Error::Postgres("57P01: terminating connection".into())).0,
            3113
        );
        assert_eq!(
            oracle_error_for(&Error::Postgres("connection closed".into())).0,
            3113
        );
        assert_eq!(
            oracle_error_for(&Error::Postgres("08006: connection failure".into())).0,
            3135
        );
    }

    #[test]
    fn maps_common_query_faults() {
        for (state, ora) in [
            ("42P01: no table", 942),
            ("42702: ambiguous", 918),
            ("42803: bad group by", 979),
            ("40P01: deadlock", 60),
            ("57014: canceled", 1013),
            ("53300: too many", 18),
            ("2201B: bad regex", 12726),
        ] {
            assert_eq!(
                oracle_error_for(&Error::Postgres(state.into())).0,
                ora,
                "sqlstate {state}"
            );
        }
    }

    #[tokio::test]
    async fn health_endpoint_answers_healthz_and_readyz() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // A throwaway TCP listener stands in for PostgreSQL so /readyz can connect.
        let fake_pg = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let pg_port = fake_pg.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                if fake_pg.accept().await.is_err() {
                    break;
                }
            }
        });

        let health = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let health_port = health.local_addr().unwrap().port();
        tokio::spawn(serve_health(
            health,
            HealthProbe {
                pg_host: "127.0.0.1".into(),
                pg_port,
                pg_db: "postgres".into(),
            },
        ));

        async fn get(port: u16, path: &str) -> String {
            let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            s.write_all(format!("GET {path} HTTP/1.1\r\nhost: x\r\n\r\n").as_bytes())
                .await
                .unwrap();
            let mut buf = Vec::new();
            s.read_to_end(&mut buf).await.unwrap();
            String::from_utf8_lossy(&buf).into_owned()
        }

        assert!(get(health_port, "/healthz").await.contains("200 OK"));
        assert!(get(health_port, "/readyz").await.contains("200 OK"));
        assert!(get(health_port, "/nope").await.contains("404"));

        let metrics = get(health_port, "/metrics").await;
        assert!(metrics.contains("200 OK"));
        assert!(metrics.contains("pgsaci_sessions_active"));
        assert!(metrics.contains("pgsaci_statements_total"));
        assert!(metrics.contains("# TYPE pgsaci_backend_errors_total counter"));

        let sessions = get(health_port, "/sessions").await;
        assert!(sessions.contains("200 OK"));
        assert!(sessions.contains("application/json"));
        assert!(sessions.trim_end().ends_with("[]"));
        // GET on a session path is method-not-allowed (kill is DELETE/POST).
        assert!(
            get(health_port, "/sessions/999999")
                .await
                .contains("405 Method Not Allowed")
        );
        assert!(
            get(health_port, "/sessions/notanumber")
                .await
                .contains("400 Bad Request")
        );
    }

    #[test]
    fn sessions_json_escapes_and_shapes() {
        assert_eq!(render_sessions_json(), "[]");
        assert_eq!(json_escape(r#"a"b\c"#), r#"a\"b\\c"#);
    }
}
