use bytes::Bytes;

use crate::buffer::{ReadBuffer, WriteBuffer};
use crate::error::{Error, Result};

pub const PROTOCOL_VERSION: u8 = 0x06;
pub const CHARSET_UTF8: u16 = 873;
pub type AuthParameters = Vec<(String, Vec<u8>)>;

/// Server compile-time capability array for the Protocol response. Strict thin
/// drivers (python-oracledb, JDBC thin) index many entries of this array
/// (`TNS_CCAP_*`) in `adjust_for_server_compile_caps`, so it must be a full
/// array, not a stub. These are the values a real 19c client negotiates;
/// index 7 (`TNS_CCAP_FIELD_VERSION`) is 12 = 12.2, which matches the row
/// metadata layout DbSaci emits (oaccolid, pre-vector/domain fields).
///
/// Index 15 bit `0x01` advertises end-of-call-status support. `write_ttc_status`
/// always emits the `[status ub4][seq ub2]` trailer, and python-oracledb thin /
/// oracle-rs read both fields unconditionally. ojdbc thin only reads the status
/// ub4 when this bit is set; without it, ojdbc consumes one byte too few and
/// desyncs on the next message.
const SERVER_COMPILE_CAPS: [u8; 53] = [
    0x06, 0x00, 0x00, 0x00, 0xea, 0x18, 0x00, 0x0c, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
    // Index 16 bit `0x10`: advertises that every end-of-call / STATUS message
    // carries a trailing end-to-end sequence-number `ub2`. python-oracledb thin,
    // ojdbc thin and oracle-rs already read that field unconditionally in
    // `write_end_of_call*` / `write_ttc_status`; setting the bit makes ODP.NET
    // managed read it too, so a single end-of-call writer serves every client.
    0x39, 0x90, 0x03, 0x07, 0x03, 0x00, 0x01, 0x00, 0xcf, 0x00, 0x00, 0x04, 0x01, 0x00, 0x00, 0x00,
    0x10, 0x00, 0x00, 0x0c, 0x20, 0x00, 0xb8, 0x00, 0x08, 0x44, 0x00, 0x05, 0x00, 0x3e, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00,
];

/// Server run-time capability array (`TNS_RCAP_*`) for the Protocol response.
const SERVER_RUNTIME_CAPS: [u8; 4] = [0x00, 0x0b, 0x02, 0x00];

/// The TTC "server port descriptor" sent in the Protocol response. This is the
/// server *platform* build string (opaque to every client), **not** the product
/// banner — real Oracle sends something like this here. It must stay short:
/// ojdbc thin reads this field into a fixed 50-byte buffer and only stops early
/// on a NUL. A longer string is truncated without
/// consuming its terminator, desyncing every field that follows (the client
/// then reads a bogus FDO length and blocks forever waiting for that many
/// bytes). python-oracledb thin reads it NUL-terminated with no cap, so the
/// mismatch is invisible there.
const SERVER_PORT_DESCRIPTOR: &str = "IBMPC/WIN_NT64-9.1.0";

/// After ACCEPT, an OCI client (unlike the thin drivers, which set
/// `TNS_NSI_DISABLE_NA`) runs the ANO / "Secure Network Services" negotiation:
/// a Data packet whose body begins `DE AD BE EF`, offering the Supervisor,
/// Authentication, Encryption and Data-Integrity services. This is the exact
/// reply a real Oracle 21c server with no wallet / no `sqlnet.ora` encryption
/// sends back — every service resolved to its null adapter — so the client
/// proceeds with plaintext transport and normal TTC authentication. Captured
/// live and byte-stable across connections.
const ANO_NEGOTIATION_RESPONSE: [u8; 117] = [
    0xde, 0xad, 0xbe, 0xef, 0x00, 0x75, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x04, 0x00,
    0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x05, 0x15, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00,
    0x06, 0x00, 0x1f, 0x00, 0x0e, 0x00, 0x01, 0xde, 0xad, 0xbe, 0xef, 0x00, 0x03, 0x00, 0x00, 0x00,
    0x02, 0x00, 0x04, 0x00, 0x01, 0x00, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00,
    0x05, 0x15, 0x00, 0x10, 0x00, 0x00, 0x02, 0x00, 0x06, 0xfb, 0xff, 0x00, 0x02, 0x00, 0x02, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x05, 0x15, 0x00, 0x10, 0x00, 0x00, 0x01, 0x00, 0x02, 0x00,
    0x00, 0x03, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x05, 0x15, 0x00, 0x10, 0x00,
    0x00, 0x01, 0x00, 0x02, 0x00,
];

/// `true` when a negotiation Data packet body is the ANO / DEADBEEF handshake.
pub fn is_ano_negotiation(payload: &[u8]) -> bool {
    payload.len() >= 6 && payload[2..6] == [0xde, 0xad, 0xbe, 0xef]
}

/// The reply to [`is_ano_negotiation`]: a Data packet carrying the null-adapter
/// ANO response.
pub fn build_ano_negotiation_response() -> Bytes {
    let mut buf = WriteBuffer::new();
    buf.write_u16_be(0); // data flags
    buf.write_bytes(&ANO_NEGOTIATION_RESPONSE);
    buf.freeze()
}

/// The server capability blob a real Oracle 21c server puts in its Protocol
/// (TTC message 1) response — the charset-negotiation array, `SERVER_COMPILE_CAPS`,
/// and the runtime caps / FDO. Captured live; the OCI client's `nau` layer
/// parses this format, which is not the one the thin drivers expect.
#[rustfmt::skip]
const OCI_PROTOCOL_CAPS: &[u8] = &[
    0x69, 0x03, 0x01, 0x0a, 0x00, 0x66, 0x03, 0x40, 0x03, 0x01, 0x40, 0x03, 0x66, 0x03, 0x01, 0x66,
    0x03, 0x48, 0x03, 0x01, 0x48, 0x03, 0x66, 0x03, 0x01, 0x66, 0x03, 0x52, 0x03, 0x01, 0x52, 0x03,
    0x66, 0x03, 0x01, 0x66, 0x03, 0x61, 0x03, 0x01, 0x61, 0x03, 0x66, 0x03, 0x01, 0x66, 0x03, 0x1f,
    0x03, 0x08, 0x1f, 0x03, 0x66, 0x03, 0x01, 0x00, 0x64, 0x00, 0x00, 0x00, 0x60, 0x01, 0x24, 0x0f,
    0x05, 0x0b, 0x0c, 0x03, 0x0c, 0x0c, 0x05, 0x04, 0x05, 0x0d, 0x06, 0x09, 0x07, 0x08, 0x05, 0x05,
    0x05, 0x05, 0x05, 0x0f, 0x05, 0x05, 0x05, 0x05, 0x05, 0x0a, 0x05, 0x05, 0x05, 0x05, 0x05, 0x04,
    0x05, 0x06, 0x07, 0x08, 0x08, 0x23, 0x47, 0x23, 0x47, 0x08, 0x11, 0x23, 0x08, 0x11, 0x41, 0xb0,
    0x47, 0x00, 0x83, 0x03, 0x69, 0x07, 0xd0, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x2d, 0x06, 0x01,
    0x01, 0x01, 0x6f, 0x01, 0x01, 0x10, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x7f, 0xff, 0x03,
    0x10, 0x03, 0x03, 0x01, 0x01, 0xff, 0x01, 0xff, 0xff, 0x01, 0x0b, 0x01, 0x01, 0xff, 0x01, 0x06,
    0x0c, 0xe6, 0x01, 0x7f, 0x05, 0x0f, 0x7f, 0x0d, 0x03, 0x00, 0x01, 0x07, 0x02, 0x01, 0x00, 0x01,
    0x18, 0x00, 0x7f,
];

/// The DataTypes (TTC message 2) response a real Oracle 21c server sends — the
/// full server type-representation list. It is fixed for a given TTC version
/// (independent of the client's request), so it is replayed verbatim for OCI;
/// the thin reflector ([`build_data_types_response`]) would truncate it.
const OCI_DATA_TYPES_RESPONSE: &[u8] = include_bytes!("oci_datatypes_response.bin");

/// DataTypes response for an OCI client.
pub fn build_data_types_response_oci() -> Bytes {
    Bytes::from_static(OCI_DATA_TYPES_RESPONSE)
}

/// Protocol (TTC message 1) response for an OCI client: `01 06 00` +
/// server-port descriptor + the real Oracle 21c capability blob. The thin
/// drivers get [`build_protocol_response`] instead.
pub fn build_protocol_response_oci() -> Bytes {
    let mut buf = WriteBuffer::new();
    buf.write_u16_be(0); // data flags
    buf.write_u8(0x01); // message type Protocol
    buf.write_u8(0x06); // accepted version
    buf.write_u8(0x00);
    // OCI's `nau` layer indexes the capability blob at a fixed offset from the
    // packet start, so the port descriptor must be exactly what a real Oracle
    // server sends (19 bytes) — one byte longer and OCI mis-reads the caps and
    // degrades the DataTypes negotiation.
    buf.write_bytes(b"x86_64/Linux 2.4.xx");
    buf.write_u8(0x00);
    buf.write_bytes(OCI_PROTOCOL_CAPS);
    buf.freeze()
}

pub fn build_protocol_response() -> Bytes {
    let mut buf = WriteBuffer::new();
    buf.write_u16_be(0); // data flags
    buf.write_u8(0x01); // message type Protocol
    buf.write_u8(PROTOCOL_VERSION);
    buf.write_u8(0); // array terminator
    buf.write_bytes(SERVER_PORT_DESCRIPTOR.as_bytes());
    buf.write_u8(0); // null terminator
    buf.write_u16_le(CHARSET_UTF8); // charset id
    buf.write_u8(0); // server flags
    buf.write_u16_le(0); // number of elements in the server charset map
    // FDO ("Function Data Object" / legacy type-conversion blob): a uint16be
    // length then raw bytes. Strict thin drivers require length >= 7 and then
    // read the national charset id at `fdo[ix+3..ix+5]` where
    // `ix = 6 + fdo[5] + fdo[6]`. With fdo[5]=fdo[6]=0, ix=6, so an 11-byte
    // blob suffices; bytes 9..11 carry ncharset id 2000 (AL16UTF16), the real
    // Oracle default for an AL32UTF8 database.
    const FDO: [u8; 11] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0x07, 0xD0];
    buf.write_u16_be(FDO.len() as u16);
    buf.write_bytes(&FDO);
    buf.write_bytes_with_length(Some(&SERVER_COMPILE_CAPS));
    buf.write_bytes_with_length(Some(&SERVER_RUNTIME_CAPS));
    buf.freeze()
}

/// Given a DATA payload whose first TTC message is a PIGGYBACK (`0x11` at
/// offset 2 — ojdbc uses it to carry a deferred CLOSE_CURSORS ahead of the real
/// call), return the slice starting at the embedded `0x03` FUNCTION message.
/// DbSaci owns its single streamed cursor, so the piggyback body itself needs no
/// action. Returns `None` if no embedded FUNCTION message is found.
pub fn strip_piggyback(payload: &[u8]) -> Option<&[u8]> {
    // Real TTC functions that can follow a piggyback. `0x11` is the piggyback
    // marker itself (several may chain); the scan skips past every one.
    const KNOWN_FUNCS: [u8; 13] = [
        0x03, 0x04, 0x05, 0x09, 0x0e, 0x0f, 0x2f, 0x3b, 0x47, 0x4e, 0x5e, 0x67, 0x69,
    ];
    // A piggyback is `[0x11][func][seq]` then function-specific args, repeated,
    // then the real `[0x03][func]` message. Walk it structurally: from just
    // after the leading `[flags:2]`, for each `0x11` skip its 3-byte header and
    // the argument run up to the next `0x11` or `0x03` boundary; stop at the
    // first `0x03 <known func>`.
    let mut i = 2;
    while i + 1 < payload.len() {
        match payload[i] {
            0x11 => {
                // header, then scan forward to the next message boundary
                i += 3;
                while i + 1 < payload.len()
                    && !(payload[i] == 0x11
                        || (payload[i] == 0x03 && KNOWN_FUNCS.contains(&payload[i + 1])))
                {
                    i += 1;
                }
            }
            0x03 if KNOWN_FUNCS.contains(&payload[i + 1]) => return Some(&payload[i..]),
            _ => i += 1,
        }
    }
    None
}

/// The client's TTC Protocol (message type 1) negotiation: the highest protocol
/// version it wants, the descending list of versions it will also accept, and
/// the driver banner. Only the two version fields are capabilities; the banner
/// is retained for logging only and MUST NOT drive wire behaviour.
#[derive(Debug, Default, Clone)]
pub struct ProtocolRequest {
    pub version: u8,
    pub accepted_versions: Vec<u8>,
    pub banner: String,
}

pub fn parse_protocol_request(payload: &[u8]) -> Result<ProtocolRequest> {
    let mut buf = ReadBuffer::from_slice(payload);
    let _flags = buf.read_u16_be()?;
    let _msg_type = buf.read_u8()?;
    let version = buf.read_u8()?;
    // A NUL-terminated list of additional protocol versions the client accepts.
    // oracle-rs / python-oracledb send just the terminator; ojdbc sends the full
    // descending list (`05 04 03 02 01 00`). The driver banner (also
    // NUL-terminated) follows the terminator.
    let remaining = buf.remaining_slice();
    let split = remaining
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(remaining.len());
    let accepted_versions = remaining[..split].to_vec();
    let after_list = remaining.get(split + 1..).unwrap_or(&[]);
    let banner = match after_list.iter().position(|&b| b == 0) {
        Some(pos) => String::from_utf8_lossy(&after_list[..pos]).to_string(),
        None => String::from_utf8_lossy(after_list).to_string(),
    };
    Ok(ProtocolRequest {
        version,
        accepted_versions,
        banner,
    })
}

/// The client's compile-time and run-time capability vectors, carried in the
/// TTC DataTypes (message type 2) request right before the type-representation
/// list. These are `TNS_CCAP_*` / `TNS_RCAP_*` — the same arrays a real Oracle
/// server inspects to decide its wire encoding. Returns `(compile, runtime)`.
pub fn parse_data_types_caps(payload: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut buf = ReadBuffer::from_slice(payload);
    buf.skip(2)?; // data flags
    if buf.read_u8()? != 0x02 {
        return Err(Error::Protocol("not a DataTypes request".into()));
    }
    buf.read_u16_le()?; // client charset
    buf.read_u16_le()?; // client ncharset
    buf.read_u8()?; // encoding flag
    let compile = buf.read_bytes_with_length()?.unwrap_or_default();
    let runtime = buf.read_bytes_with_length()?.unwrap_or_default();
    Ok((compile, runtime))
}

/// Build the TTC DataTypes negotiation response.
///
/// The client request is `[data flags:2][msg type:1=2][charset:2][charset:2]
/// [encoding flag:1][compile caps: len-prefixed][runtime caps: len-prefixed]
/// [(data_type:2, conv:2, repr:2, reserved:2)…][terminator:2=0]`.
///
/// A real Oracle server replies with the datatype triples it supports (a
/// subset), terminated by a zero type — *not* an echo of the request's
/// capability preamble. Lenient clients (`oracle-rs`) tolerate the echo; strict
/// clients (`python-oracledb` thin, JDBC thin) stall on it. DbSaci answers "I
/// support every type you offered" by reflecting just the triple list.
pub fn build_data_types_response(request_payload: &[u8]) -> Bytes {
    let mut out = WriteBuffer::new();
    out.write_u16_be(0); // data flags
    out.write_u8(0x02); // message type: DataTypes

    if let Some(triples) = data_type_triples(request_payload) {
        out.write_bytes(&triples);
    }
    out.write_u16_be(0); // terminator
    out.freeze()
}

/// Extract the raw `(data_type, conv, repr, reserved)` u16 quads from a
/// DataTypes request payload, stopping before the zero terminator.
fn data_type_triples(payload: &[u8]) -> Option<Vec<u8>> {
    let mut buf = ReadBuffer::from_slice(payload);
    buf.skip(2).ok()?; // data flags
    let msg_type = buf.read_u8().ok()?;
    if msg_type != 0x02 {
        return None;
    }
    buf.read_u16_le().ok()?; // client charset
    buf.read_u16_le().ok()?; // client ncharset
    buf.read_u8().ok()?; // encoding flag
    buf.read_bytes_with_length().ok()?; // compile capabilities
    buf.read_bytes_with_length().ok()?; // runtime capabilities

    let mut triples = Vec::new();
    loop {
        let data_type = buf.read_u16_be().ok()?;
        if data_type == 0 {
            break;
        }
        let conv = buf.read_u16_be().ok()?;
        let repr = buf.read_u16_be().ok()?;
        let reserved = buf.read_u16_be().ok()?;
        triples.extend_from_slice(&data_type.to_be_bytes());
        triples.extend_from_slice(&conv.to_be_bytes());
        triples.extend_from_slice(&repr.to_be_bytes());
        triples.extend_from_slice(&reserved.to_be_bytes());
    }
    Some(triples)
}

/// Backwards-compatible alias retained for callers/tests.
pub fn echo_data_types_response(payload: &[u8]) -> Bytes {
    build_data_types_response(payload)
}

/// DataTypes negotiation response for a client at the `na_without_version_list`
/// negotiation point (see `WireProfile`). Derived from the observable handshake
/// of ODP.NET managed, which parses the response differently from
/// python-oracledb / JDBC thin:
///
///  * Because DbSaci advertises `SERVER_RUNTIME_CAPS[1] & 1`, this client reads
///    a fixed **11-byte DB-timezone blob** first, before any type list.
///    python-oracledb / JDBC don't read this at all, so it must *only* be
///    emitted here.
///  * `SERVER_COMPILE_CAPS[37] & 2` is unset, so the optional 4-byte
///    timezone-file version that would follow the blob is skipped.
///  * Because `SERVER_COMPILE_CAPS[27]` is non-zero, the type-representation
///    list is read as raw big-endian `UB2` values, ending at the first `0` seen
///    outside a type block — so a bare `UB2(0)` terminator is a complete, valid
///    (empty) list.
///
/// The 11-byte blob is the wire form for UTC
/// (`80 00 00 00 3C 3C 3C 80 00 00 00`); DbSaci's backing PostgreSQL runs UTC.
pub fn build_data_types_response_na_no_verlist() -> Bytes {
    const DB_TZ_UTC: [u8; 11] = [0x80, 0, 0, 0, 0x3C, 0x3C, 0x3C, 0x80, 0, 0, 0];
    let mut out = WriteBuffer::new();
    out.write_u16_be(0); // data flags
    out.write_u8(0x02); // message type: DataTypes
    out.write_bytes(&DB_TZ_UTC); // DB-timezone blob
    out.write_u16_be(0); // empty type-representation list (UB2 terminator)
    out.freeze()
}

pub fn build_auth_phase_one_response_11g(auth_sesskey: &str, auth_vfr_data: &str) -> Bytes {
    let mut buf = WriteBuffer::new();
    buf.write_u16_be(0); // data flags
    // Thin drivers (python-oracledb, ojdbc) require the return-parameters
    // message type + a trailing STATUS, exactly like the 12c response — not a
    // bare Function (`0x03`) message, which they reject as
    // `DPY-5000 unknown protocol message type 3`.
    buf.write_u8(TTC_MSG_PARAMETER); // 0x08
    buf.write_ub2(4); // num params

    write_auth_kv(&mut buf, "AUTH_SESSKEY", auth_sesskey);
    // `TNS_VERIFIER_TYPE_11G_2 = 0x1b25`; any non-12c type routes python-oracledb
    // and ojdbc to the O5LOGON (SHA-1, 24-byte key) verifier path.
    write_auth_kv_vfr(&mut buf, auth_vfr_data, 0x1B25);
    write_auth_kv(&mut buf, "AUTH_VERSION_NO", "1900000000");
    write_auth_kv(&mut buf, "AUTH_GLOBALLY_UNIQUE_DBID", "0000000000000000");
    write_ttc_status(&mut buf);

    buf.freeze()
}

pub fn build_auth_phase_one_response_12c(
    auth_sesskey: &str,
    auth_vfr_data: &str,
    csk_salt: &str,
    vgen_count: u32,
    sder_count: u32,
) -> Bytes {
    let mut buf = WriteBuffer::new();
    buf.write_u16_be(0); // data flags
    buf.write_u8(TTC_MSG_PARAMETER); // return-parameters message
    buf.write_ub2(7); // num params

    write_auth_kv(&mut buf, "AUTH_SESSKEY", auth_sesskey);
    write_auth_kv_vfr(&mut buf, auth_vfr_data, 0x4815);
    write_auth_kv(&mut buf, "AUTH_PBKDF2_CSK_SALT", csk_salt);
    write_auth_kv(&mut buf, "AUTH_PBKDF2_VGEN_COUNT", &vgen_count.to_string());
    write_auth_kv(&mut buf, "AUTH_PBKDF2_SDER_COUNT", &sder_count.to_string());
    write_auth_kv(&mut buf, "AUTH_VERSION_NO", "202375000");
    write_auth_kv(&mut buf, "AUTH_GLOBALLY_UNIQUE_DBID", "0000000000000000");
    write_ttc_status(&mut buf);
    buf.freeze()
}

/// Phase-one auth response for a `na_without_version_list` client. Same `AUTH_*`
/// key/value block as [`build_auth_phase_one_response_12c`] (this negotiation
/// point implies the 12c PBKDF2 verifier), but terminated with a `0x04`
/// end-of-call instead of `0x09` STATUS. Derived from the observable handshake
/// of ODP.NET managed, whose auth loop has no STATUS case and treats one as a
/// protocol error (connection Break + Reset).
/// The `0x04` body is the shared [`write_end_of_call_jdbc`] with all-zero args.
pub fn build_auth_phase_one_response_na_no_verlist(
    auth_sesskey: &str,
    auth_vfr_data: &str,
    csk_salt: &str,
    vgen_count: u32,
    sder_count: u32,
) -> Bytes {
    let mut buf = WriteBuffer::new();
    buf.write_u16_be(0); // data flags
    buf.write_u8(TTC_MSG_PARAMETER);
    buf.write_ub2(7); // num params

    write_auth_kv(&mut buf, "AUTH_SESSKEY", auth_sesskey);
    write_auth_kv_vfr(&mut buf, auth_vfr_data, 0x4815);
    write_auth_kv(&mut buf, "AUTH_PBKDF2_CSK_SALT", csk_salt);
    write_auth_kv(&mut buf, "AUTH_PBKDF2_VGEN_COUNT", &vgen_count.to_string());
    write_auth_kv(&mut buf, "AUTH_PBKDF2_SDER_COUNT", &sder_count.to_string());
    write_auth_kv(&mut buf, "AUTH_VERSION_NO", "202375000");
    write_auth_kv(&mut buf, "AUTH_GLOBALLY_UNIQUE_DBID", "0000000000000000");
    write_end_of_call_jdbc(&mut buf, 0, None, 0, false, 0, 0, 0);
    buf.freeze()
}

/// Phase-two auth response for a `na_without_version_list` client (ODP.NET
/// managed). Same `AUTH_*` key/value block as [`build_auth_phase_two_response`],
/// terminated with a `0x04` end-of-call.
pub fn build_auth_phase_two_response_na_no_verlist(
    auth_svr_response: &str,
    session_id: u32,
    version_no: u32,
    version_string: &str,
) -> Bytes {
    let mut buf = WriteBuffer::new();
    buf.write_u16_be(0); // data flags
    buf.write_u8(TTC_MSG_PARAMETER);
    buf.write_ub2(9); // num params

    write_auth_kv(&mut buf, "AUTH_SVR_RESPONSE", auth_svr_response);
    write_auth_kv(&mut buf, "AUTH_SESSION_ID", &session_id.to_string());
    write_auth_kv(&mut buf, "AUTH_SERIAL_NUM", "1");
    write_auth_kv(&mut buf, "AUTH_VERSION_NO", &version_no.to_string());
    write_auth_kv(&mut buf, "AUTH_VERSION_STRING", version_string);
    write_auth_kv(&mut buf, "AUTH_SC_DBUNIQUE_NAME", "DBSACI");
    write_auth_kv(&mut buf, "AUTH_SC_SERVICE_NAME", "FREEPDB1");
    write_auth_kv(&mut buf, "AUTH_MAX_OPEN_CURSORS", "1000");
    write_auth_kv(&mut buf, "AUTH_MAX_IDEN_LENGTH", "128");
    write_end_of_call_jdbc(&mut buf, 0, None, 0, false, 0, 0, 0);
    buf.freeze()
}

/// TTC server-response message type for a key/value return-parameter block.
const TTC_MSG_PARAMETER: u8 = 0x08;
/// TTC status message type.
const TTC_MSG_STATUS: u8 = 0x09;
/// TTC end-of-response marker (message type 29).
const TTC_MSG_END_OF_RESPONSE: u8 = 0x1d;

/// Append a `STATUS` message (`call_status = 0`, `seq = 0`). Strict thin
/// drivers treat this as the end of the response when the server has not
/// advertised an explicit end-of-response marker.
fn write_ttc_status(buf: &mut WriteBuffer) {
    buf.write_u8(TTC_MSG_STATUS);
    buf.write_ub4(0); // call status
    buf.write_ub2(0); // end-to-end seq
}

/// Response to the OVERSION function (TTC func 59), which ojdbc thin issues
/// during `getMetaData()` to learn the server banner and packed release number.
/// `T4C7Oversion.readRPA` reads `[ub2 len][len banner bytes][ub4 release]`
/// inside an RPA (0x08) message, then expects a terminating STATUS (0x09).
pub fn build_oversion_response(banner: &str, version_no: u32) -> Bytes {
    let mut buf = WriteBuffer::new();
    buf.write_u16_be(0); // data flags
    buf.write_u8(TTC_MSG_PARAMETER); // 0x08 RPA / return parameters
    buf.write_ub2(banner.len() as u16);
    buf.write_bytes(banner.as_bytes());
    buf.write_ub4(version_no);
    write_ttc_status(&mut buf);
    buf.freeze()
}

/// OVERSION (TTC func 59) response for an OCI client. OCI marshals the banner as
/// `[u16-LE len][u8 len][bytes]` and expects a fixed 10-byte trailer, unlike the
/// compact-`ub2` form the thin drivers use.
pub fn build_oversion_response_oci(banner: &str) -> Bytes {
    let mut buf = WriteBuffer::new();
    buf.write_u16_be(0); // data flags
    buf.write_u8(TTC_MSG_PARAMETER);
    let b = banner.as_bytes();
    buf.write_u16_le(b.len() as u16);
    buf.write_u8(b.len() as u8);
    buf.write_bytes(b);
    buf.write_u8(0);
    // Fixed trailer captured from a real Oracle 21c server.
    buf.write_bytes(&[0x00, 0x03, 0x15, 0x09, 0x01, 0x00, 0x00, 0x00, 0x27, 0x00]);
    buf.freeze()
}

/// Reply to an OCI LOGOFF (`func 0x09`, usually piggybacked). A real Oracle
/// server answers with a bare TTC `STATUS` (`0x09`) message; the OCI client
/// then closes the socket itself. Anything else (or an early socket close)
/// surfaces client-side as `ORA-03113`.
pub fn build_logoff_response_oci() -> Bytes {
    Bytes::from_static(&[0x00, 0x00, 0x09, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00])
}

/// Reply to an OCI COMMIT (`func 0x0e`) / ROLLBACK (`func 0x0f`). A real Oracle
/// server answers with a bare TTC `STATUS` (`0x09`) message
/// `00 00 09 05 00 00 00 <session op counter> 00`; the generic DML ack wedges
/// the OCI client.
pub fn build_txn_response_oci() -> Bytes {
    Bytes::from_static(&[0x00, 0x00, 0x09, 0x05, 0x00, 0x00, 0x00, 0x24, 0x00])
}

pub fn build_auth_phase_two_response(
    auth_svr_response: &str,
    session_id: u32,
    version_no: u32,
    version_string: &str,
) -> Bytes {
    let mut buf = WriteBuffer::new();
    buf.write_u16_be(0); // data flags
    buf.write_u8(TTC_MSG_PARAMETER); // return-parameters message
    buf.write_ub2(9); // num params

    write_auth_kv(&mut buf, "AUTH_SVR_RESPONSE", auth_svr_response);
    // Required by strict thin drivers' post-connect step (KeyError otherwise).
    write_auth_kv(&mut buf, "AUTH_SESSION_ID", &session_id.to_string());
    write_auth_kv(&mut buf, "AUTH_SERIAL_NUM", "1");
    write_auth_kv(&mut buf, "AUTH_VERSION_NO", &version_no.to_string());
    write_auth_kv(&mut buf, "AUTH_VERSION_STRING", version_string);
    write_auth_kv(&mut buf, "AUTH_SC_DBUNIQUE_NAME", "DBSACI");
    write_auth_kv(&mut buf, "AUTH_SC_SERVICE_NAME", "FREEPDB1");
    write_auth_kv(&mut buf, "AUTH_MAX_OPEN_CURSORS", "1000");
    write_auth_kv(&mut buf, "AUTH_MAX_IDEN_LENGTH", "128");
    write_ttc_status(&mut buf);
    buf.freeze()
}

// ---------------------------------------------------------------------------
// OCI (thick client) authentication.
//
// OCI marshals the auth key/value block differently from the thin drivers: each
// pair is `[u32-LE keylen][u8 keylen][key][u32-LE vallen][u8 vallen][value]
// [u32-LE flags]`, the message header is `[u16 dataflags][u8 0x08][u16-LE
// pair-count]`, and each phase ends with a fixed OCI-shaped end-of-call rather
// than a STATUS. The crypto (12c PBKDF2) is identical; only the framing differs.
// ---------------------------------------------------------------------------

/// End-of-call tail after the OCI phase-one key/value block (captured live).
const OCI_AUTH1_TAIL: &[u8] = include_bytes!("oci_auth1_tail.bin");
/// The full OCI phase-two response body a real Oracle 21c server sends
/// (`AUTH_VERSION_*`, session id, the NLS block, `AUTH_SVR_RESPONSE`, …). Only
/// `AUTH_SVR_RESPONSE` is session-specific and is spliced in.
const OCI_AUTH2_RESPONSE: &[u8] = include_bytes!("oci_auth2_response.bin");
/// Byte offset of the `AUTH_SVR_RESPONSE` value inside [`OCI_AUTH2_RESPONSE`].
const OCI_AUTH2_SVR_RESPONSE_OFFSET: usize = 1909;
const OCI_AUTH2_SVR_RESPONSE_LEN: usize = 96;

fn oci_write_kv(buf: &mut WriteBuffer, key: &[u8], value: &[u8], flags: u32) {
    buf.write_u32_le(key.len() as u32);
    buf.write_u8(key.len() as u8);
    buf.write_bytes(key);
    buf.write_u32_le(value.len() as u32);
    if !value.is_empty() {
        buf.write_u8(value.len() as u8);
        buf.write_bytes(value);
    }
    buf.write_u32_le(flags);
}

/// Parse the OCI auth key/value block: skip the pointer preamble, then read
/// pairs until the buffer is exhausted or a non-key length is seen. Also
/// returns the bare username (the `[u8 len][bytes]` that precedes the first
/// `AUTH_*` key in a phase-one request), if present.
pub fn parse_oci_auth(payload: &[u8]) -> (Option<String>, Vec<(String, Vec<u8>)>) {
    // Locate the first key: an ASCII run beginning "AUTH" or "SESSION" whose
    // `[u32-LE len][u8 len]` prefix is consistent.
    let mut start = None;
    for p in 5..payload.len().saturating_sub(4) {
        let looks_key = payload[p..].starts_with(b"AUTH") || payload[p..].starts_with(b"SESSION");
        if !looks_key {
            continue;
        }
        let klen = u32::from_le_bytes([
            payload[p - 5],
            payload[p - 4],
            payload[p - 3],
            payload[p - 2],
        ]) as usize;
        if (4..=64).contains(&klen) && payload[p - 1] as usize == klen {
            start = Some(p - 5);
            break;
        }
    }
    let Some(kv_start) = start else {
        return (None, Vec::new());
    };

    // Username: the `[u8 n][n bytes]` that ends exactly where the first key
    // prefix begins (phase one only; absent in phase two).
    let mut username = None;
    for ulen in 1usize..=30 {
        if kv_start < 1 + ulen {
            break;
        }
        let lenpos = kv_start - 1 - ulen;
        let bytes = &payload[kv_start - ulen..kv_start];
        if payload[lenpos] as usize == ulen && bytes.iter().all(|b| b.is_ascii_graphic()) {
            username = std::str::from_utf8(bytes).ok().map(|s| s.to_string());
            break;
        }
    }

    let mut pairs = Vec::new();
    let mut i = kv_start;
    while i + 5 <= payload.len() {
        let klen = u32::from_le_bytes([payload[i], payload[i + 1], payload[i + 2], payload[i + 3]])
            as usize;
        if klen == 0 || klen > 64 || i + 5 + klen > payload.len() || payload[i + 4] as usize != klen
        {
            break;
        }
        let key = String::from_utf8_lossy(&payload[i + 5..i + 5 + klen])
            .trim_end_matches('\0')
            .to_string();
        i += 5 + klen;
        if i + 4 > payload.len() {
            break;
        }
        let vlen = u32::from_le_bytes([payload[i], payload[i + 1], payload[i + 2], payload[i + 3]])
            as usize;
        i += 4;
        let value = if vlen == 0 {
            Vec::new()
        } else {
            if i + 1 + vlen > payload.len() {
                break;
            }
            let v = payload[i + 1..i + 1 + vlen].to_vec();
            i += 1 + vlen;
            v
        };
        i += 4; // flags
        pairs.push((key, value));
    }
    (username, pairs)
}

/// OCI phase-one auth response: the 12c PBKDF2 challenge in OCI key/value form.
pub fn build_auth_phase_one_response_oci(
    auth_sesskey: &str,
    auth_vfr_data: &str,
    csk_salt: &str,
    vgen_count: u32,
    sder_count: u32,
) -> Bytes {
    let mut buf = WriteBuffer::new();
    buf.write_u16_be(0); // data flags
    buf.write_u8(TTC_MSG_PARAMETER);
    buf.write_u16_le(6); // pair count
    oci_write_kv(&mut buf, b"AUTH_SESSKEY", auth_sesskey.as_bytes(), 0);
    oci_write_kv(&mut buf, b"AUTH_VFR_DATA", auth_vfr_data.as_bytes(), 0x4815);
    oci_write_kv(&mut buf, b"AUTH_PBKDF2_CSK_SALT", csk_salt.as_bytes(), 0);
    oci_write_kv(
        &mut buf,
        b"AUTH_PBKDF2_VGEN_COUNT",
        vgen_count.to_string().as_bytes(),
        0,
    );
    oci_write_kv(
        &mut buf,
        b"AUTH_PBKDF2_SDER_COUNT",
        sder_count.to_string().as_bytes(),
        0,
    );
    oci_write_kv(
        &mut buf,
        b"AUTH_GLOBALLY_UNIQUE_DBID\0",
        b"0000000000000000",
        0,
    );
    buf.write_bytes(OCI_AUTH1_TAIL);
    buf.freeze()
}

/// OCI phase-two auth response: the captured real-server body with our computed
/// `AUTH_SVR_RESPONSE` (mutual proof) spliced in.
pub fn build_auth_phase_two_response_oci(auth_svr_response: &str) -> Bytes {
    let mut body = OCI_AUTH2_RESPONSE.to_vec();
    let proof = auth_svr_response.as_bytes();
    if proof.len() == OCI_AUTH2_SVR_RESPONSE_LEN {
        body[OCI_AUTH2_SVR_RESPONSE_OFFSET
            ..OCI_AUTH2_SVR_RESPONSE_OFFSET + OCI_AUTH2_SVR_RESPONSE_LEN]
            .copy_from_slice(proof);
    }
    Bytes::from(body)
}

fn write_auth_string(buf: &mut WriteBuffer, s: &str) {
    let b = s.as_bytes();
    if b.is_empty() {
        buf.write_u8(0);
        return;
    }
    buf.write_u8(1); // indicator present
    buf.write_u8(b.len() as u8);
    buf.write_u8(b.len() as u8);
    buf.write_bytes(b);
}

fn write_auth_kv(buf: &mut WriteBuffer, key: &str, value: &str) {
    write_auth_string(buf, key);
    write_auth_string(buf, value);
    buf.write_ub4(0);
}

fn write_auth_kv_vfr(buf: &mut WriteBuffer, value: &str, verifier_type: u32) {
    write_auth_string(buf, "AUTH_VFR_DATA");
    write_auth_string(buf, value);
    buf.write_ub4(verifier_type);
}

pub fn parse_auth_phase_one_request(payload: &[u8]) -> Result<(String, AuthParameters)> {
    parse_auth_request(payload, false)
}

/// `one_byte_chunks`: read long-form (`0xFE`) key/value CLRs with single-byte
/// chunk prefixes instead of the `ub4` big-chunk form. ODP.NET managed needs
/// this (see [`ReadBuffer::read_bytes_with_length_1b_chunks`]).
fn parse_auth_request(payload: &[u8], one_byte_chunks: bool) -> Result<(String, AuthParameters)> {
    let read_val = |buf: &mut ReadBuffer| {
        if one_byte_chunks {
            buf.read_bytes_with_length_1b_chunks()
        } else {
            buf.read_bytes_with_length()
        }
    };
    let mut buf = ReadBuffer::from_slice(payload);
    let _flags = buf.read_u16_be()?;
    let _msg_type = buf.read_u8()?;
    let _function = buf.read_u8()?;
    let _seq = buf.read_u8()?;
    // Oracle 23ai (TTC field version >= 18) adds a token UB8 after the
    // sequence number. DbSaci advertises 19c, where this field is absent.
    // The 23ai form starts with a zero-length UB8 in oracle-rs, so retain
    // support for both forms while parsing client authentication packets.
    if buf.remaining_slice().first() == Some(&0) {
        let _token = buf.read_u8()?;
    }
    let _user_ptr = buf.read_u8()?;
    let user_len = buf.read_ub4()?;
    let _auth_mode = buf.read_ub4()?;
    let _list_ptr = buf.read_u8()?;
    let pair_count = buf.read_ub4()?;
    let _out_ptr = buf.read_u8()?;
    let _out_count_ptr = buf.read_u8()?;
    // Username framing differs by client. python-oracledb thin and oracle-rs
    // prefix the username bytes with their own length (a ub1 well below 0x20);
    // ojdbc thin omits that prefix and sends the raw username bytes, relying on
    // the `user_len` field parsed above. A real username is printable ASCII, so
    // a leading byte under 0x20 can only be the redundant length prefix.
    let username = match buf.remaining_slice().first().copied() {
        Some(b) if b != 0 && (b as usize) < 0x20 => buf
            .read_bytes_with_length()?
            .map(|b| String::from_utf8_lossy(&b).to_string())
            .unwrap_or_default(),
        _ if user_len > 0 => {
            String::from_utf8_lossy(&buf.read_bytes_vec(user_len as usize)?).to_string()
        }
        _ => String::new(),
    };

    let mut pairs = Vec::new();
    for _ in 0..pair_count {
        let _key_len = buf.read_ub4()?;
        let key = buf.read_string_with_length()?.unwrap_or_default();
        let val_len = buf.read_ub4()?;
        // A zero-length value carries NO CLR bytes at all (the flags ub4 follows
        // immediately). ojdbc's 11g/O5LOGON phase two sends an empty
        // `AUTH_PASSWORD` this way; consuming a length-prefixed value here would
        // eat the flags field and desync every following pair.
        let value = if val_len > 0 {
            read_val(&mut buf)?.unwrap_or_default()
        } else {
            Vec::new()
        };
        let _flags = buf.read_ub4()?;
        pairs.push((key, value));
    }

    Ok((username, pairs))
}

pub fn parse_auth_phase_two_request(payload: &[u8]) -> Result<(String, AuthParameters)> {
    // Same structure as phase one but different function code and keys.
    parse_auth_request(payload, false)
}

/// Phase-two auth request parser for a `na_without_version_list` client, which
/// long-form-encodes `AUTH_*` values over 252 bytes (`AUTH_CONNECT_STRING`) with
/// single-byte CLR chunk prefixes rather than the `ub4` big-chunk form the other
/// thin drivers use. Reverse-engineered against ODP.NET managed.
pub fn parse_auth_phase_two_request_na_no_verlist(
    payload: &[u8],
) -> Result<(String, AuthParameters)> {
    parse_auth_request(payload, true)
}

#[derive(Debug, Clone, PartialEq)]
pub enum BindValue {
    Null,
    String(String),
    Number(String),
    Bytes(Vec<u8>),
    /// A PostgreSQL temporal literal constructed from Oracle DATE/TIMESTAMP
    /// wire bytes, never from untrusted SQL text.
    Temporal(String),
    Boolean(bool),
    BinaryDouble(f64),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExecuteRequest {
    pub sql: String,
    pub binds: Vec<BindValue>,
    /// The Oracle datatype code of each bind, in client order. Kept so a later
    /// `REEXECUTE`/`REEXECUTE_AND_FETCH` — which does not re-send the describe —
    /// can decode its value block ([`parse_reexecute_request`]).
    pub bind_types: Vec<u8>,
    /// Rows the client asked to prefetch (its array/fetch size). 0 = unset.
    pub prefetch: u32,
    /// Iteration count from the Execute header. `executemany` / JDBC batch send
    /// one SQL with `num_iters > 1` and one value row per iteration.
    pub num_iters: u32,
    /// One entry per iteration when `num_iters > 1` (array bind / batch DML),
    /// each a full row of bind values in `$n` order. Empty for a scalar Execute
    /// (the single row is in `binds`).
    pub bind_rows: Vec<Vec<BindValue>>,
}

/// PostgreSQL SQL plus the Oracle bind values in the order of its `$n`
/// parameters. Values are never interpolated into `sql`.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundSql {
    pub sql: String,
    pub binds: Vec<BindValue>,
}

/// A `0x5E` Execute frame that carries no SQL text: ojdbc thin re-executing a
/// statement it expects to be cached on a server-side cursor. Recognised by the
/// SQL pointer being null / the declared SQL length being zero or larger than
/// the whole frame (the field is being reused as a cursor reference).
pub fn execute_frame_has_no_sql(payload: &[u8]) -> bool {
    let mut buf = ReadBuffer::from_slice(payload);
    let hdr = (|| -> Result<(u8, u32)> {
        buf.read_u16_be()?; // flags
        buf.read_u8()?; // msg type
        buf.read_u8()?; // function
        buf.read_u8()?; // seq
        buf.read_ub4()?; // exec options
        buf.read_ub4()?; // cursor id
        let sql_ptr = buf.read_u8()?;
        let sql_len = buf.read_ub4()?;
        Ok((sql_ptr, sql_len))
    })();
    match hdr {
        Ok((sql_ptr, sql_len)) => sql_ptr == 0 || sql_len == 0 || sql_len as usize > payload.len(),
        Err(_) => true,
    }
}

pub fn parse_execute_request(payload: &[u8]) -> Result<ExecuteRequest> {
    // oracle-rs and python-oracledb thin write an execute preamble with a fixed
    // field layout (TTC field version 12.2 / 12.2-EXT1). ojdbc thin's layout
    // has a different number of trailing option pointers, so the strict parse
    // desyncs on it; fall back to a content scan that finds the SQL and binds
    // without counting the variant preamble fields.
    match parse_execute_strict(payload) {
        Ok(request) => Ok(request),
        Err(strict_err) => parse_execute_scan(payload).map_err(|_| strict_err),
    }
}

fn parse_execute_strict(payload: &[u8]) -> Result<ExecuteRequest> {
    let mut buf = ReadBuffer::from_slice(payload);
    let _flags = buf.read_u16_be()?;
    let _msg_type = buf.read_u8()?;
    let _function = buf.read_u8()?;
    let _seq = buf.read_u8()?;
    // DbSaci negotiates TTC field version 12 (19c), which has no token field.
    let _exec_opts = buf.read_ub4()?;
    let _cursor_id = buf.read_ub4()?;
    let sql_ptr = buf.read_u8()?;
    let sql_len = buf.read_ub4()?;

    if sql_ptr == 0 || sql_len == 0 {
        return Ok(ExecuteRequest {
            sql: String::new(),
            binds: Vec::new(),
            bind_types: Vec::new(),
            prefetch: 0,
            num_iters: 0,
            bind_rows: Vec::new(),
        });
    }

    // oracle-rs writes this fixed execute header before the length-prefixed SQL
    // bytes. Parse it rather than scanning packet contents for SQL keywords.
    buf.read_u8()?; // vector pointer
    buf.read_ub4()?; // al8i4 array length (13)
    buf.read_u8()?; // al8o4 pointer
    buf.read_u8()?; // al8o4l pointer
    buf.read_ub4()?; // prefetch buffer size
    let prefetch = buf.read_ub4()?; // prefetch row count (num_iters for queries)
    buf.read_ub4()?; // max long size
    let binds_ptr = buf.read_u8()?;
    let num_binds = buf.read_ub4()? as usize;
    for _ in 0..5 {
        buf.read_u8()?;
    }
    buf.read_u8()?;
    buf.read_ub4()?;
    buf.read_ub4()?;
    for _ in 0..3 {
        buf.read_u8()?;
    }
    buf.read_ub4()?;
    buf.read_u8()?;
    buf.read_ub4()?;
    buf.read_ub4()?;
    buf.read_u8()?;
    buf.read_ub4()?;
    buf.read_u8()?;
    for _ in 0..2 {
        buf.read_ub4()?;
        buf.read_u8()?;
    }
    buf.read_ub4()?;
    buf.read_u8()?;
    buf.read_ub4()?;

    let sql = buf
        .read_bytes_with_length()?
        .ok_or_else(|| Error::Protocol("execute request omitted SQL text".to_string()))?;
    if sql.len() != sql_len as usize {
        return Err(Error::Protocol(
            "execute SQL length does not match header".to_string(),
        ));
    }
    let sql = String::from_utf8(sql)
        .map_err(|e| Error::Protocol(format!("execute SQL is not UTF-8: {e}")))?;

    // al8i4[0..12], written immediately after SQL by oracle-rs. al8i4[1] is the
    // iteration count: `executemany` / batch DML sends it > 1 with one value row
    // per iteration.
    let mut num_iters = 1u32;
    for i in 0..13 {
        let v = buf.read_ub4()?;
        if i == 1 {
            num_iters = v;
        }
    }

    let mut bind_types = Vec::with_capacity(num_binds);
    let mut row_marker_already_read = false;
    if binds_ptr != 0 {
        loop {
            let oracle_type = match buf.read_u8() {
                Ok(t) => t,
                Err(Error::BufferUnderflow { .. }) => break,
                Err(e) => return Err(e),
            };
            // oracle-rs emits one descriptor for a duplicated SQL placeholder
            // but retains the duplicate count in Execute's header. The RowData
            // marker therefore appears where the next descriptor would be.
            if oracle_type == 0x07 {
                row_marker_already_read = true;
                break;
            }
            if bind_types.len() >= 1024 {
                return Err(Error::Protocol("too many bind descriptors".into()));
            }
            buf.read_u8()?; // flags
            buf.read_u8()?; // precision
            buf.read_u8()?; // scale
            buf.read_ub4()?; // buffer size
            buf.read_ub4()?; // array elements
            buf.read_ub8()?; // continuation flags
            let oid_len = buf.read_ub4()?;
            if oid_len > 0 {
                buf.read_bytes_with_length()?;
                buf.read_ub4()?; // object version
            } else {
                buf.read_ub2()?; // scalar version
            }
            buf.read_ub2()?; // charset id
            buf.read_u8()?; // charset form
            buf.read_ub4()?; // LOB prefetch
            buf.read_ub4()?; // oaccolid (12.2+)
            bind_types.push(oracle_type);
        }
    }

    let mut binds = Vec::with_capacity(num_binds);
    let mut bind_rows: Vec<Vec<BindValue>> = Vec::new();
    if num_binds > 0 {
        let row_marker = if row_marker_already_read {
            Some(0x07)
        } else {
            match buf.read_u8() {
                Ok(m) => Some(m),
                // A statement whose every bind is an OUT bind (`RETURNING …
                // INTO :x` with no input placeholders) sends descriptors but no
                // RowData block at all. That is not a framing error.
                Err(Error::BufferUnderflow { .. }) => None,
                Err(e) => return Err(e),
            }
        };
        if let Some(marker) = row_marker
            && marker != 0x07
        {
            return Err(Error::Protocol(format!(
                "expected bind RowData marker, got {marker}"
            )));
        }
        if row_marker.is_none() {
            return Ok(ExecuteRequest {
                sql,
                binds,
                bind_types,
                prefetch,
                num_iters,
                bind_rows,
            });
        }
        // Be tolerant of a client that frames a different number of bind values
        // than descriptors (oracle-rs does this for a positional bind reused in
        // SQL, and for RETURNING ... INTO out-binds). Stop at the first short
        // read rather than dropping the connection; `substitute_bind_values`
        // then reports any genuinely missing placeholder.
        for &oracle_type in &bind_types {
            match buf.read_bytes_with_length() {
                Ok(raw) => binds.push(decode_bind_value(oracle_type, raw)?),
                Err(Error::BufferUnderflow { .. }) => break,
                Err(e) => return Err(e),
            }
        }
        // Array bind / batch DML: iterations 1.. each carry their own `0x07`
        // RowData marker followed by a full value row. Row 0 is `binds`.
        if num_iters > 1 {
            bind_rows.push(std::mem::take(&mut binds));
            for _ in 1..num_iters {
                match buf.read_u8() {
                    Ok(0x07) => {}
                    Ok(other) => {
                        return Err(Error::Protocol(format!(
                            "expected array-bind RowData marker, got {other}"
                        )));
                    }
                    Err(Error::BufferUnderflow { .. }) => break,
                    Err(e) => return Err(e),
                }
                let mut row = Vec::with_capacity(bind_types.len());
                for &oracle_type in &bind_types {
                    match buf.read_bytes_with_length() {
                        Ok(raw) => row.push(decode_bind_value(oracle_type, raw)?),
                        Err(Error::BufferUnderflow { .. }) => break,
                        Err(e) => return Err(e),
                    }
                }
                bind_rows.push(row);
            }
            binds = bind_rows.first().cloned().unwrap_or_default();
        }
        // A client may serialize surplus input values after the declared bind
        // count. They belong to this already-complete TNS payload, so discard
        // them rather than letting them corrupt parsing of the next packet.
        buf.skip(buf.remaining())?;
    }

    Ok(ExecuteRequest {
        sql,
        binds,
        bind_types,
        prefetch,
        num_iters,
        bind_rows,
    })
}

/// ojdbc-thin fallback: skip the fixed header, then find the SQL by content and
/// read binds tolerantly. Used only when [`parse_execute_strict`] desyncs.
fn parse_execute_scan(payload: &[u8]) -> Result<ExecuteRequest> {
    let mut buf = ReadBuffer::from_slice(payload);
    buf.read_u16_be()?; // data flags
    buf.read_u8()?; // msg type
    buf.read_u8()?; // function
    buf.read_u8()?; // seq
    let _exec_opts = buf.read_ub4()?;
    let _cursor_id = buf.read_ub4()?;
    let sql_ptr = buf.read_u8()?;
    let sql_len = buf.read_ub4()? as usize;
    if sql_ptr == 0 || sql_len == 0 {
        return Ok(ExecuteRequest {
            sql: String::new(),
            binds: Vec::new(),
            bind_types: Vec::new(),
            prefetch: 0,
            num_iters: 0,
            bind_rows: Vec::new(),
        });
    }

    // These six fields lead every client's preamble identically; the prefetch /
    // iteration count is the sixth.
    buf.read_u8()?; // al8i4 vector pointer
    buf.read_ub4()?; // al8i4 array length
    buf.read_u8()?; // al8o4 pointer
    buf.read_u8()?; // al8o4l pointer
    buf.read_ub4()?; // prefetch buffer size
    let prefetch = buf.read_ub4()?; // prefetch / iteration row count

    let rel = locate_sql_body(buf.remaining_slice(), sql_len).ok_or_else(|| {
        Error::Protocol("could not locate SQL text in execute request".to_string())
    })?;
    buf.skip(rel)?;
    // ojdbc writes the SQL bytes raw (length already given as an SWORD); `rel`
    // points at the body, so read exactly `sql_len` bytes.
    let sql = String::from_utf8(buf.read_bytes_vec(sql_len)?)
        .map_err(|e| Error::Protocol(format!("execute SQL is not UTF-8: {e}")))?;

    // al8i4[0..13] follow the SQL for every client; al8i4[1] is the iteration
    // count (`executemany` / JDBC batch send it > 1).
    let mut num_iters = 1u32;
    for i in 0..13 {
        let v = buf.read_ub4()?;
        if i == 1 {
            num_iters = v;
        }
    }

    // Whatever remains is bind descriptors + values (or nothing). Parse
    // tolerantly: a real descriptor never starts with a 0 type byte.
    let mut bind_types = Vec::new();
    let mut row_marker_already_read = false;
    while buf.remaining() > 0 {
        let oracle_type = match buf.read_u8() {
            Ok(t) => t,
            Err(Error::BufferUnderflow { .. }) => break,
            Err(e) => return Err(e),
        };
        if oracle_type == 0 {
            break;
        }
        if oracle_type == 0x07 {
            row_marker_already_read = true;
            break;
        }
        if bind_types.len() >= 1024 {
            return Err(Error::Protocol("too many bind descriptors".into()));
        }
        buf.read_u8()?; // flags
        buf.read_u8()?; // precision
        buf.read_u8()?; // scale
        buf.read_ub4()?; // buffer size
        buf.read_ub4()?; // array elements
        buf.read_ub8()?; // continuation flags
        let oid_len = buf.read_ub4()?;
        if oid_len > 0 {
            buf.read_bytes_with_length()?;
            buf.read_ub4()?;
        } else {
            buf.read_ub2()?;
        }
        buf.read_ub2()?; // charset id
        buf.read_u8()?; // charset form
        buf.read_ub4()?; // LOB prefetch
        buf.read_ub4()?; // oaccolid
        bind_types.push(oracle_type);
    }

    let mut binds = Vec::with_capacity(bind_types.len());
    let mut bind_rows: Vec<Vec<BindValue>> = Vec::new();
    if !bind_types.is_empty() {
        if !row_marker_already_read {
            // A RowData marker (0x07) precedes the value block; tolerate its
            // absence rather than dropping the connection.
            let _ = buf.read_u8();
        }
        for &oracle_type in &bind_types {
            match buf.read_bytes_with_length() {
                Ok(raw) => binds.push(decode_bind_value(oracle_type, raw)?),
                Err(Error::BufferUnderflow { .. }) => break,
                Err(e) => return Err(e),
            }
        }
        if num_iters > 1 {
            bind_rows.push(std::mem::take(&mut binds));
            for _ in 1..num_iters {
                match buf.read_u8() {
                    Ok(0x07) => {}
                    Ok(_) => break,
                    Err(_) => break,
                }
                let mut row = Vec::with_capacity(bind_types.len());
                for &oracle_type in &bind_types {
                    match buf.read_bytes_with_length() {
                        Ok(raw) => row.push(decode_bind_value(oracle_type, raw)?),
                        Err(Error::BufferUnderflow { .. }) => break,
                        Err(e) => return Err(e),
                    }
                }
                bind_rows.push(row);
            }
            binds = bind_rows.first().cloned().unwrap_or_default();
        }
        buf.skip(buf.remaining())?;
    }

    Ok(ExecuteRequest {
        sql,
        binds,
        bind_types,
        prefetch,
        num_iters,
        bind_rows,
    })
}

/// Parse an OCI (thick client) Execute request. OCI's preamble is a run of
/// 8-byte `0xFFFFFFFFFFFFFFFE` "pointer present" markers and little-/big-endian
/// length words that does not match the thin layout, so the SQL is located by
/// content: the first ASCII run that starts with a statement keyword. Binds are
/// not yet decoded from the OCI value block.
pub fn parse_execute_request_oci(payload: &[u8]) -> Result<ExecuteRequest> {
    const KEYWORDS: [&[u8]; 24] = [
        b"SELECT",
        b"INSERT",
        b"UPDATE",
        b"DELETE",
        b"WITH ",
        b"MERGE ",
        b"BEGIN",
        b"DECLARE",
        b"ALTER",
        b"CREATE",
        b"DROP ",
        b"TRUNCATE",
        b"COMMENT",
        b"GRANT ",
        b"REVOKE",
        b"RENAME",
        b"CALL ",
        b"COMMIT",
        b"ROLLBACK",
        b"SAVEPOINT",
        b"SET ",
        b"LOCK ",
        b"REFRESH",
        b"ANALYZE",
    ];
    // The graphic run starting at `i`. SQL text is ASCII graphic + space + LF +
    // tab; bytes >= 0x80 are UTF-8 lead/continuation bytes of multibyte literals
    // (e.g. `'café'`) and stay in the run.
    let graphic_run = |i: usize| -> usize {
        let mut e = i;
        while e < payload.len()
            && (payload[e].is_ascii_graphic()
                || payload[e] == b' '
                || payload[e] == b'\n'
                || payload[e] == b'\t'
                || payload[e] == b'\r'
                || payload[e] >= 0x80)
        {
            e += 1;
        }
        e - i
    };
    let is_kw_at = |i: usize| -> bool {
        KEYWORDS.iter().any(|kw| {
            payload[i..].len() >= kw.len() && payload[i..i + kw.len()].eq_ignore_ascii_case(kw)
        })
    };

    // The real SQL is prefixed by its own length: a single byte for text under
    // 254 bytes, otherwise a big-endian u32 in the four bytes before it. Pick
    // the first keyword whose preceding length prefix matches the graphic run
    // that follows — this rejects the `SELECT` after `UNION ALL` (preceded by a
    // space) and any keyword sitting inside a bind value.
    let mut start = None;
    let mut end = 0usize;
    for i in 4..payload.len() {
        if !is_kw_at(i) {
            continue;
        }
        let run = graphic_run(i);
        let run_trimmed = {
            let mut r = run;
            while r > 0 && (payload[i + r - 1] == b' ' || payload[i + r - 1] == b'\n') {
                r -= 1;
            }
            r
        };
        let one = payload[i - 1] as usize;
        let four = u32::from_be_bytes([
            payload[i - 4],
            payload[i - 3],
            payload[i - 2],
            payload[i - 1],
        ]) as usize;
        if (one == run_trimmed || one == run || four == run_trimmed || four == run)
            && run_trimmed > 0
        {
            start = Some(i);
            end = i + run_trimmed;
            break;
        }
        // A parenthesised query — `(SELECT …)` / `( SELECT …)` — has its keyword
        // one or more `(`/space bytes past the real SQL start. Re-anchor on the
        // first such `(` and match the length prefix that precedes it.
        let mut p = i;
        while p > 4 && (payload[p - 1] == b'(' || payload[p - 1] == b' ') {
            p -= 1;
        }
        if p < i && payload[p] == b'(' {
            let prun = graphic_run(p);
            let prun_trimmed = {
                let mut r = prun;
                while r > 0 && (payload[p + r - 1] == b' ' || payload[p + r - 1] == b'\n') {
                    r -= 1;
                }
                r
            };
            let pone = payload[p - 1] as usize;
            let pfour = u32::from_be_bytes([
                payload[p - 4],
                payload[p - 3],
                payload[p - 2],
                payload[p - 1],
            ]) as usize;
            if (pone == prun_trimmed || pone == prun || pfour == prun_trimmed || pfour == prun)
                && prun_trimmed > 0
            {
                start = Some(p);
                end = p + prun_trimmed;
                break;
            }
        }
        if start.is_none() {
            // Fallback: first keyword. Prefer an explicit length prefix over
            // the (possibly binary-extended) graphic run.
            start = Some(i);
            end = if (1..=payload.len() - i).contains(&one) && one <= run_trimmed.max(one) {
                i + one
            } else if (1..=payload.len() - i).contains(&four) {
                i + four
            } else {
                i + run_trimmed.max(1)
            };
        }
    }
    if start.is_none() && std::env::var("DBSACI_OCI_DEBUG").is_ok() {
        eprintln!(
            "OCI-DEBUG no-sql payload ({} bytes): {}",
            payload.len(),
            payload
                .iter()
                .map(|b| format!("{:02x}", b))
                .collect::<String>()
        );
        eprintln!(
            "OCI-DEBUG ascii: {}",
            String::from_utf8_lossy(
                &payload
                    .iter()
                    .map(|&b| if (0x20..0x7f).contains(&b) { b } else { b'.' })
                    .collect::<Vec<_>>()
            )
        );
    }
    let start = start
        .ok_or_else(|| Error::Protocol("OCI execute request: no SQL text found".to_string()))?;
    let sql = String::from_utf8_lossy(&payload[start..end.min(payload.len())])
        .trim_end()
        .to_string();

    // --- bind values -----------------------------------------------------
    // OCI lays the bind section out as: an options blob, then one descriptor
    // per placeholder (`[u8 dty][u8 flag][0][0][u32-LE max len]…`), then a
    // `0x07` "bind row" marker followed by `[u8 length][bytes]` per value
    // (Oracle NUMBER / raw / date encodings, identical to the thin wire).
    let nbinds = count_named_binds(&sql);
    let (bind_types, binds) = if nbinds == 0 {
        (Vec::new(), Vec::new())
    } else {
        let region = &payload[end..];
        let (types, after_desc) = scan_oci_bind_types(region, nbinds);
        // The bind-row marker is the first 0x07 after the descriptor block
        // (a value's own bytes can also contain 0x07, so never scan from the
        // end).
        let values_at = region[after_desc..]
            .iter()
            .position(|&b| b == 0x07)
            .map(|p| after_desc + p);
        let mut binds = Vec::with_capacity(nbinds);
        if let Some(pos) = values_at {
            let mut p = pos + 1;
            for i in 0..nbinds {
                if p >= region.len() {
                    break;
                }
                let len = region[p] as usize;
                p += 1;
                let raw = if len == 0 || len == 0xff {
                    None
                } else if len == 0xfd {
                    // OCI encodes a NULL bind value as `fd <indicator-byte>`.
                    p += 1;
                    None
                } else if p + len <= region.len() {
                    let v = region[p..p + len].to_vec();
                    p += len;
                    Some(v)
                } else {
                    None
                };
                let oty = types.get(i).copied().unwrap_or(1);
                binds.push(decode_bind_value(oty, raw)?);
            }
        }
        (types, binds)
    };

    Ok(ExecuteRequest {
        sql,
        binds,
        bind_types,
        prefetch: 0,
        num_iters: 1,
        bind_rows: Vec::new(),
    })
}

/// Count the distinct named bind placeholders (`:name` / `:1`) in an Oracle SQL
/// string, ignoring occurrences inside string / quoted-identifier literals and
/// line / block comments. python-oracledb sends one value per *distinct* name.
fn count_named_binds(sql: &str) -> usize {
    let b = sql.as_bytes();
    let mut seen: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\'' | b'"' => {
                let q = b[i];
                i += 1;
                while i < b.len() && b[i] != q {
                    i += 1;
                }
                i += 1;
            }
            b'-' if i + 1 < b.len() && b[i + 1] == b'-' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            b':' => {
                let s = i + 1;
                let mut e = s;
                while e < b.len() && (b[e].is_ascii_alphanumeric() || b[e] == b'_') {
                    e += 1;
                }
                if e > s {
                    let name = &sql[s..e];
                    // `:NEW` / `:OLD` in a trigger body are PL/SQL pseudo-records,
                    // not client binds. `:NEW.col` also leaves a trailing `.col`
                    // that must not be mistaken for another name.
                    let is_trigger_pseudo = name.eq_ignore_ascii_case("NEW")
                        || name.eq_ignore_ascii_case("OLD")
                        || name.eq_ignore_ascii_case("PARENT");
                    if !is_trigger_pseudo && !seen.contains(&name) {
                        seen.push(name);
                    }
                }
                i = e;
            }
            _ => i += 1,
        }
    }
    seen.len()
}

/// Walk an OCI bind descriptor region and return the first `want` Oracle data
/// type codes plus the offset just past the last descriptor consumed. Each
/// descriptor starts with `[u8 dty][u8 flag][0x00][0x00]`.
fn scan_oci_bind_types(region: &[u8], want: usize) -> (Vec<u8>, usize) {
    const KNOWN: [u8; 15] = [
        1, 2, 8, 9, 11, 12, 23, 96, 100, 101, 180, 181, 182, 183, 231,
    ];
    let mut out = Vec::with_capacity(want);
    let mut i = 0;
    let mut last_end = 0;
    while i + 4 <= region.len() && out.len() < want {
        let dty = region[i];
        let flag = region[i + 1];
        if KNOWN.contains(&dty)
            && region[i + 2] == 0
            && region[i + 3] == 0
            && (flag == 0x01 || flag == 0x03 || flag == 0x07 || flag == 0x21 || flag == 0x23)
        {
            out.push(dty);
            i += 4;
            last_end = i;
        } else {
            i += 1;
        }
    }
    while out.len() < want {
        out.push(1); // default: VARCHAR
    }
    (out, last_end)
}

/// Reconstruct an [`ExecuteRequest`] from a `REEXECUTE` (`0x04`) /
/// `REEXECUTE_AND_FETCH` (`0x4e`) message. These re-run a statement already
/// prepared on the cursor, so they carry no SQL text and no bind descriptors —
/// just the (new) bind values. `bind_types` is the datatype list cached from the
/// original Execute; `sql` is the original Oracle SQL. Returns the fetch/iter
/// count in `prefetch`.
/// OCI `REEXECUTE` (`0x04`) / `REEXECUTE_AND_FETCH` (`0x4e`): the client re-runs
/// a statement already in its cache. No SQL, no describe — just (optional) new
/// bind values in the trailing `0x07` row. Re-uses `sql` and `bind_types` from
/// the original Execute.
/// The server cursor id an OCI `REEXECUTE` (`0x04`) / `REEXECUTE_AND_FETCH`
/// (`0x4e`) targets. Both lay it out identically as
/// `[03][fn][seq][u32-LE cursor][u32-LE iters]` — verified byte-for-byte
/// against a live Oracle capture of a re-executed `INSERT` (`0x04`) and
/// `SELECT` (`0x4e`). A zero id means "the cursor the previous call used" —
/// the caller then falls back to `last_execute`.
pub fn parse_reexecute_cursor_id_oci(payload: &[u8], func_code: u8) -> Option<u16> {
    let off = match func_code {
        0x4E => 5,
        0x04 => 5,
        _ => return None,
    };
    if payload.len() < off + 4 {
        return None;
    }
    let id = u32::from_le_bytes([
        payload[off],
        payload[off + 1],
        payload[off + 2],
        payload[off + 3],
    ]);
    if id == 0 || id > u16::MAX as u32 {
        None
    } else {
        Some(id as u16)
    }
}

/// A `0x5E` Execute that re-parses a statement already on the client's cursor
/// (bind type changed) carries no SQL text — only the cursor id at `[9..13]`
/// (zero for a first parse). Returns the id when it looks like a re-parse.
pub fn parse_reparse_cursor_id_oci(payload: &[u8]) -> Option<u16> {
    if payload.len() < 13 {
        return None;
    }
    let id = u32::from_le_bytes([payload[9], payload[10], payload[11], payload[12]]);
    if id == 0 || id > u16::MAX as u32 {
        None
    } else {
        Some(id as u16)
    }
}

pub fn parse_reexecute_request_oci(
    payload: &[u8],
    sql: &str,
    bind_types: &[u8],
) -> Result<ExecuteRequest> {
    parse_reexecute_request_oci_ex(payload, sql, bind_types, false)
}

/// `rescan_types`: a `0x5E` re-parse frame carries FRESH bind descriptors (the
/// bind type changed) — scan them rather than trusting the cached list, or a
/// NUMBER decoded against a stale VARCHAR type raises "not UTF-8". A bare
/// `0x4e` / `0x04` has no descriptors, so keep the cached types.
pub fn parse_reexecute_request_oci_ex(
    payload: &[u8],
    sql: &str,
    bind_types: &[u8],
    rescan_types: bool,
) -> Result<ExecuteRequest> {
    let nbinds = count_named_binds(sql).max(bind_types.len());
    if nbinds == 0 {
        return Ok(ExecuteRequest {
            sql: sql.to_string(),
            binds: Vec::new(),
            bind_types: Vec::new(),
            prefetch: 0,
            num_iters: 1,
            bind_rows: Vec::new(),
        });
    }

    // The value row is `[0x07]` then one length-prefixed value per bind
    // (`fd <ind>` / `00` / `ff` = NULL), consuming to within a few trailing
    // bytes of the frame. Pick the LAST `0x07` for which that holds — spurious
    // `0x07`s inside the descriptor block or the fixed header won't.
    let value_row_ok = |at: usize| -> bool {
        let mut p = at + 1;
        for _ in 0..nbinds {
            if p >= payload.len() {
                return false;
            }
            let len = payload[p] as usize;
            p += 1;
            match len {
                0 | 0xff => {}
                0xfd => p += 1,
                _ if p + len <= payload.len() => p += len,
                _ => return false,
            }
        }
        payload.len().saturating_sub(p) <= 3
    };
    let val_at = (0..payload.len())
        .rev()
        .find(|&pos| payload[pos] == 0x07 && value_row_ok(pos));

    // Fresh types: the descriptor quads `[dty][flag][00][00]` sit before the
    // value row. Scan the window between the fixed header and the value row.
    let types: Vec<u8> = if (rescan_types || bind_types.len() != nbinds)
        && let Some(vr) = val_at
    {
        let start = vr.saturating_sub(64 + 8 * nbinds);
        let (scanned, _) = scan_oci_bind_types(&payload[start..vr], nbinds);
        scanned
    } else {
        bind_types.to_vec()
    };

    let mut binds = Vec::with_capacity(types.len());
    if let Some(pos) = val_at {
        let mut p = pos + 1;
        for &oty in &types {
            if p >= payload.len() {
                break;
            }
            let len = payload[p] as usize;
            p += 1;
            let raw = if len == 0 || len == 0xff {
                None
            } else if len == 0xfd {
                // OCI NULL bind value: `fd <indicator-byte>`.
                p += 1;
                None
            } else if p + len <= payload.len() {
                let v = payload[p..p + len].to_vec();
                p += len;
                Some(v)
            } else {
                None
            };
            binds.push(decode_bind_value(oty, raw)?);
        }
    }
    Ok(ExecuteRequest {
        sql: sql.to_string(),
        binds,
        bind_types: types,
        prefetch: 0,
        num_iters: 1,
        bind_rows: Vec::new(),
    })
}

pub fn parse_reexecute_request(
    payload: &[u8],
    sql: &str,
    bind_types: &[u8],
) -> Result<ExecuteRequest> {
    let mut buf = ReadBuffer::from_slice(payload);
    buf.read_u16_be()?; // data flags
    buf.read_u8()?; // msg type
    buf.read_u8()?; // function
    buf.read_u8()?; // seq
    let _cursor_id = buf.read_ub4()?;
    let prefetch = buf.read_ub4()?; // fetch / iteration row count

    let mut binds = Vec::with_capacity(bind_types.len());
    if !bind_types.is_empty() {
        // A short run of option fields precedes the `0x07` RowData marker; scan
        // for it rather than counting the (client-version-dependent) fields.
        let mut found = false;
        for _ in 0..24 {
            match buf.read_u8() {
                Ok(0x07) => {
                    found = true;
                    break;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        if !found {
            return Err(Error::Protocol(
                "reexecute: no bind RowData marker".to_string(),
            ));
        }
        for &oracle_type in bind_types {
            match buf.read_bytes_with_length() {
                Ok(raw) => binds.push(decode_bind_value(oracle_type, raw)?),
                Err(Error::BufferUnderflow { .. }) => break,
                Err(e) => return Err(e),
            }
        }
    }

    Ok(ExecuteRequest {
        sql: sql.to_string(),
        binds,
        bind_types: bind_types.to_vec(),
        prefetch,
        num_iters: 1,
        bind_rows: Vec::new(),
    })
}

/// Find where the SQL text body begins in the tail of an Execute request, given
/// its already-known byte length. Returns the offset of the first SQL byte
/// (past any length prefix). The bytes before it are execute-option pointers
/// and counters whose count and framing vary between client libraries
/// (oracle-rs / python-oracledb length-prefix the SQL; ojdbc writes it raw), so
/// this scans for the first run of exactly `sql_len` valid-UTF-8 bytes whose
/// first non-space character begins a statement and which is preceded by a
/// plausible boundary byte (a length prefix, or a pointer/zero byte).
fn locate_sql_body(tail: &[u8], sql_len: usize) -> Option<usize> {
    if sql_len == 0 || sql_len > tail.len() {
        return None;
    }
    let want = sql_len as u8;
    for i in 0..=(tail.len() - sql_len) {
        let boundary = i == 0 || matches!(tail[i - 1], 0x00 | 0x01 | 0xFE) || tail[i - 1] == want;
        if !boundary {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&tail[i..i + sql_len]) else {
            continue;
        };
        let trimmed = text.trim_start();
        match trimmed.bytes().next() {
            // A statement starts with a keyword or an opening paren, and real
            // SQL of this length contains whitespace; the option preamble is
            // all NUL/0x01 bytes so it never matches.
            Some(c)
                if (c.is_ascii_alphabetic() || c == b'(')
                    && trimmed.bytes().all(|b| {
                        b == 9 || b == 10 || b == 13 || (0x20..0x7f).contains(&b) || b >= 0x80
                    }) =>
            {
                return Some(i);
            }
            _ => {}
        }
    }
    None
}

fn decode_bind_value(oracle_type: u8, raw: Option<Vec<u8>>) -> Result<BindValue> {
    let Some(raw) = raw else {
        return Ok(BindValue::Null);
    };
    match oracle_type {
        1 | 8 | 9 | 96 => String::from_utf8(raw)
            .map(BindValue::String)
            .map_err(|e| Error::DataConversionError(format!("bind string is not UTF-8: {e}"))),
        2 => decode_oracle_number(&raw).map(BindValue::Number),
        23 => Ok(BindValue::Bytes(raw)),
        12 | 180 | 181 | 231 => decode_oracle_temporal(&raw, oracle_type),
        101 => {
            let mut bytes: [u8; 8] = raw.try_into().map_err(|_| {
                Error::DataConversionError("BINARY_DOUBLE bind must be 8 bytes".into())
            })?;
            if bytes[0] & 0x80 != 0 {
                bytes[0] &= 0x7f;
            } else {
                for byte in &mut bytes {
                    *byte = !*byte;
                }
            }
            Ok(BindValue::BinaryDouble(f64::from_be_bytes(bytes)))
        }
        100 => {
            let mut bytes: [u8; 4] = raw.try_into().map_err(|_| {
                Error::DataConversionError("BINARY_FLOAT bind must be 4 bytes".into())
            })?;
            if bytes[0] & 0x80 != 0 {
                bytes[0] &= 0x7f;
            } else {
                for byte in &mut bytes {
                    *byte = !*byte;
                }
            }
            Ok(BindValue::BinaryDouble(f32::from_be_bytes(bytes) as f64))
        }
        252 => Ok(BindValue::Boolean(raw.last().copied().unwrap_or(0) != 0)),
        other => Err(Error::DataConversionError(format!(
            "unsupported Oracle bind type {other}"
        ))),
    }
}

fn decode_oracle_temporal(raw: &[u8], oracle_type: u8) -> Result<BindValue> {
    if raw.len() < 7 {
        return Err(Error::DataConversionError(
            "Oracle DATE/TIMESTAMP bind must contain at least 7 bytes".into(),
        ));
    }
    let year = (i32::from(raw[0]) - 100) * 100 + i32::from(raw[1]) - 100;
    let month = raw[2];
    let day = raw[3];
    let hour = raw[4]
        .checked_sub(1)
        .ok_or_else(|| Error::DataConversionError("Oracle DATE bind has invalid hour".into()))?;
    let minute = raw[5]
        .checked_sub(1)
        .ok_or_else(|| Error::DataConversionError("Oracle DATE bind has invalid minute".into()))?;
    let second = raw[6]
        .checked_sub(1)
        .ok_or_else(|| Error::DataConversionError("Oracle DATE bind has invalid second".into()))?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return Err(Error::DataConversionError(
            "Oracle DATE bind has invalid calendar fields".into(),
        ));
    }
    let micros = if raw.len() >= 11 {
        u32::from_be_bytes([raw[7], raw[8], raw[9], raw[10]]) / 1_000
    } else {
        0
    };
    let mut literal = format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}");
    if micros != 0 {
        literal.push_str(&format!(".{micros:06}"));
    }
    if matches!(oracle_type, 181 | 231) && raw.len() >= 13 {
        let tz_hour = i16::from(raw[11]) - 20;
        let tz_minute = i16::from(raw[12]) - 60;
        literal.push_str(&format!(" {tz_hour:+03}:{:02}", tz_minute.unsigned_abs()));
        return Ok(BindValue::Temporal(format!("TIMESTAMPTZ '{literal}'")));
    }
    Ok(BindValue::Temporal(format!("TIMESTAMP '{literal}'")))
}

fn decode_oracle_number(raw: &[u8]) -> Result<String> {
    if raw == [0x80] {
        return Ok("0".into());
    }
    let (&exponent, mantissa) = raw
        .split_first()
        .ok_or_else(|| Error::DataConversionError("empty Oracle NUMBER bind".into()))?;
    let positive = exponent & 0x80 != 0;
    let mut decimal_point = if positive {
        (exponent as i16 - 193) * 2 + 2
    } else {
        ((!exponent) as i16 - 193) * 2 + 2
    };
    let mantissa = if !positive && mantissa.last() == Some(&102) {
        &mantissa[..mantissa.len() - 1]
    } else {
        mantissa
    };
    let mut digits = Vec::with_capacity(mantissa.len() * 2);
    for (index, byte) in mantissa.iter().enumerate() {
        let pair = if positive {
            byte.wrapping_sub(1)
        } else {
            101u8.wrapping_sub(*byte)
        };
        if pair > 99 {
            return Err(Error::DataConversionError(
                "invalid Oracle NUMBER bind".into(),
            ));
        }
        let high = pair / 10;
        if high == 0 && digits.is_empty() {
            decimal_point -= 1;
        } else {
            digits.push(high);
        }
        let low = pair % 10;
        if low != 0 || index + 1 < mantissa.len() {
            digits.push(low);
        }
    }
    if digits.is_empty() {
        return Err(Error::DataConversionError(
            "invalid Oracle NUMBER bind".into(),
        ));
    }
    let digit_string: String = digits.iter().map(|d| char::from(b'0' + d)).collect();
    let rendered = if decimal_point <= 0 {
        format!(
            "0.{}{}",
            "0".repeat((-decimal_point) as usize),
            digit_string
        )
    } else if decimal_point as usize >= digit_string.len() {
        format!(
            "{}{}",
            digit_string,
            "0".repeat(decimal_point as usize - digit_string.len())
        )
    } else {
        format!(
            "{}.{}",
            &digit_string[..decimal_point as usize],
            &digit_string[decimal_point as usize..]
        )
    };
    Ok(if positive {
        rendered
    } else {
        format!("-{rendered}")
    })
}

pub fn substitute_bind_values(sql: &str, binds: &[BindValue]) -> Result<String> {
    use std::collections::HashMap;
    let mut named = HashMap::<String, usize>::new();
    let mut next_named = 0usize;
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len() + binds.len() * 8);
    let mut i = 0usize;
    while i < bytes.len() {
        // Copy string literals and quoted identifiers as UTF-8 slices, so
        // multibyte data inside them is preserved exactly.
        if bytes[i] == b'\'' || bytes[i] == b'"' {
            let quote = bytes[i];
            let mut end = i + 1;
            while end < bytes.len() {
                if bytes[end] == quote {
                    if bytes.get(end + 1) == Some(&quote) {
                        end += 2;
                    } else {
                        end += 1;
                        break;
                    }
                } else {
                    end += 1;
                }
            }
            out.push_str(&sql[i..end]);
            i = end;
            continue;
        }
        if bytes[i..].starts_with(b"--") {
            let end = bytes[i..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(bytes.len(), |offset| i + offset);
            out.push_str(&sql[i..end]);
            i = end;
            continue;
        }
        if bytes[i..].starts_with(b"/*") {
            let end = bytes[i + 2..]
                .windows(2)
                .position(|window| window == b"*/")
                .map_or(bytes.len(), |offset| i + 4 + offset);
            out.push_str(&sql[i..end]);
            i = end;
            continue;
        }
        if bytes[i] == b':' && bytes.get(i + 1) != Some(&b':') {
            let start = i + 1;
            let mut end = start;
            while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
                end += 1;
            }
            if end > start {
                let name = &sql[start..end];
                let index = if name.bytes().all(|b| b.is_ascii_digit()) {
                    let index = name.parse::<usize>().ok().and_then(|n| n.checked_sub(1));
                    if let Some(index) = index {
                        next_named = next_named.max(index + 1);
                    }
                    index
                } else {
                    Some(*named.entry(name.to_ascii_uppercase()).or_insert_with(|| {
                        let n = next_named;
                        next_named += 1;
                        n
                    }))
                }
                .ok_or_else(|| {
                    Error::DataConversionError(format!("invalid bind placeholder :{name}"))
                })?;
                let value = binds.get(index).ok_or_else(|| {
                    Error::DataConversionError(format!("missing value for bind :{name}"))
                })?;
                out.push_str(&bind_sql_literal(value)?);
                i = end;
                continue;
            }
        }
        let Some(character) = sql.get(i..).and_then(|s| s.chars().next()) else {
            break;
        };
        out.push(character);
        i += character.len_utf8();
    }
    Ok(out)
}

fn bind_sql_literal(value: &BindValue) -> Result<String> {
    Ok(match value {
        BindValue::Null => "NULL".into(),
        BindValue::String(value) => format!("'{}'", value.replace('\'', "''")),
        BindValue::Number(value) => value.clone(),
        BindValue::Bytes(value) => format!("decode('{}', 'hex')", hex::encode(value)),
        BindValue::Temporal(value) => value.clone(),
        BindValue::Boolean(value) => value.to_string().to_ascii_uppercase(),
        BindValue::BinaryDouble(value) if value.is_finite() => value.to_string(),
        BindValue::BinaryDouble(_) => {
            return Err(Error::DataConversionError(
                "non-finite floating bind is unsupported".into(),
            ));
        }
    })
}

/// A DML statement's `RETURNING <exprs> INTO <bind list>` split into the parts
/// DbSaci needs: the SQL with the `INTO` clause removed (so PostgreSQL sees a
/// plain `RETURNING`), the number of OUT bind placeholders, and the number of
/// returned expressions.
#[derive(Debug, Clone, PartialEq)]
pub struct ReturningInto {
    pub sql_without_into: String,
    pub out_bind_count: usize,
    pub returning_expr_count: usize,
}

/// Detect and split a trailing `RETURNING … INTO :a, :b` clause. Case-insensitive
/// on the keywords; only fires when `INTO` is followed by one or more `:`
/// placeholders (so `INSERT … SELECT … RETURNING` without `INTO` is untouched,
/// and so is `MERGE … INTO`). The scan ignores quoted text.
pub fn split_returning_into(sql: &str) -> Option<ReturningInto> {
    let bytes = sql.as_bytes();
    let ident_byte = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    // Find the last top-level `RETURNING` keyword outside quotes. Byte-only
    // scanning — the SQL may contain multi-byte UTF-8 in string literals.
    let mut i = 0usize;
    let mut returning_at = None;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' => {
                let q = bytes[i];
                i += 1;
                while i < bytes.len() && bytes[i] != q {
                    i += 1;
                }
                i += 1;
            }
            _ => {
                if i + 9 <= bytes.len()
                    && bytes[i..i + 9].eq_ignore_ascii_case(b"RETURNING")
                    && (i == 0 || !ident_byte(bytes[i - 1]))
                    && bytes.get(i + 9).is_none_or(|b| !ident_byte(*b))
                {
                    returning_at = Some(i);
                }
                i += 1;
            }
        }
    }
    let r_at = returning_at?;
    let after_returning = &sql[r_at + 9..];
    let ab = after_returning.as_bytes();
    // Locate ` INTO ` after RETURNING, outside quotes.
    let mut j = 0usize;
    let mut into_rel = None;
    while j + 4 <= ab.len() {
        match ab[j] {
            b'\'' | b'"' => {
                let q = ab[j];
                j += 1;
                while j < ab.len() && ab[j] != q {
                    j += 1;
                }
                j += 1;
            }
            _ => {
                if ab[j..j + 4].eq_ignore_ascii_case(b"INTO")
                    && (j == 0 || ab[j - 1].is_ascii_whitespace())
                    && ab.get(j + 4).is_some_and(|b| b.is_ascii_whitespace())
                {
                    into_rel = Some(j);
                    break;
                }
                j += 1;
            }
        }
    }
    let into_rel = into_rel?;
    let exprs = after_returning[..into_rel].trim();
    let bind_list = &after_returning[into_rel + 4..];
    let out_bind_count = bind_list.matches(':').count();
    if out_bind_count == 0 {
        return None;
    }
    let returning_expr_count = split_top_level_commas(exprs).len().max(1);
    let sql_without_into = format!("{}RETURNING {}", &sql[..r_at], exprs);
    Some(ReturningInto {
        sql_without_into,
        out_bind_count,
        returning_expr_count,
    })
}

/// Split on top-level commas (ignoring those inside parentheses / quotes).
fn split_top_level_commas(s: &str) -> Vec<&str> {
    let b = s.as_bytes();
    let mut depth = 0i32;
    let mut start = 0usize;
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'\'' | b'"' => {
                let q = b[i];
                i += 1;
                while i < b.len() && b[i] != q {
                    i += 1;
                }
            }
            b'(' => depth += 1,
            b')' => depth -= 1,
            b',' if depth == 0 => {
                out.push(s[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    let last = s[start..].trim();
    if !last.is_empty() {
        out.push(last);
    }
    out
}

/// Convert Oracle `:name` / `:1` placeholders into PostgreSQL parameters.
/// Repeated placeholders map to one PostgreSQL parameter, and supplied values
/// that no placeholder references are not sent to PostgreSQL.
pub fn bind_postgres_parameters(sql: &str, binds: &[BindValue]) -> Result<BoundSql> {
    use std::collections::HashMap;

    // PL/SQL definitions and blocks carry `:NEW` / `:OLD` trigger correlations
    // and `:x` locals that are not client bind placeholders. Clients do not
    // bind values into a `CREATE ... AS <plsql>` or an anonymous block, so pass
    // the text through untouched and let the translator handle it.
    let head = sql.trim_start();
    let upper_words: Vec<String> = head
        .split_whitespace()
        .take(5)
        .map(|w| w.to_ascii_uppercase())
        .collect();
    let kind = |w: &String| matches!(w.as_str(), "TRIGGER" | "FUNCTION" | "PROCEDURE" | "PACKAGE");
    let is_plsql_defn = match upper_words.first().map(String::as_str) {
        Some("CREATE") => upper_words
            .iter()
            .skip(1)
            .take_while(|w| {
                matches!(
                    w.as_str(),
                    "OR" | "REPLACE" | "EDITIONABLE" | "NONEDITIONABLE"
                )
            })
            .count()
            .checked_add(1)
            .and_then(|i| upper_words.get(i))
            .is_some_and(kind),
        Some("BEGIN") | Some("DECLARE") => true,
        _ => false,
    };
    if is_plsql_defn {
        return Ok(BoundSql {
            sql: sql.to_string(),
            binds: Vec::new(),
        });
    }

    let mut oracle_names = HashMap::<String, usize>::new();
    let mut parameter_for_bind = HashMap::<usize, usize>::new();
    let mut next_named = 0usize;
    let mut ordered = Vec::new();
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len() + binds.len() * 8);
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] == b'\'' || bytes[i] == b'"' {
            let quote = bytes[i];
            let mut end = i + 1;
            while end < bytes.len() {
                if bytes[end] == quote {
                    if bytes.get(end + 1) == Some(&quote) {
                        end += 2;
                    } else {
                        end += 1;
                        break;
                    }
                } else {
                    end += 1;
                }
            }
            out.push_str(&sql[i..end]);
            i = end;
            continue;
        }
        if bytes[i..].starts_with(b"--") {
            let end = bytes[i..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(bytes.len(), |offset| i + offset);
            out.push_str(&sql[i..end]);
            i = end;
            continue;
        }
        if bytes[i..].starts_with(b"/*") {
            let end = bytes[i + 2..]
                .windows(2)
                .position(|window| window == b"*/")
                .map_or(bytes.len(), |offset| i + 4 + offset);
            out.push_str(&sql[i..end]);
            i = end;
            continue;
        }
        if bytes[i] == b':' && bytes.get(i + 1) != Some(&b':') {
            let start = i + 1;
            let mut end = start;
            while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
                end += 1;
            }
            if end > start {
                let name = &sql[start..end];
                let bind_index = if name.bytes().all(|b| b.is_ascii_digit()) {
                    let index = name.parse::<usize>().ok().and_then(|n| n.checked_sub(1));
                    if let Some(index) = index {
                        next_named = next_named.max(index + 1);
                    }
                    index
                } else {
                    Some(
                        *oracle_names
                            .entry(name.to_ascii_uppercase())
                            .or_insert_with(|| {
                                let index = next_named;
                                next_named += 1;
                                index
                            }),
                    )
                }
                .ok_or_else(|| {
                    Error::DataConversionError(format!("invalid bind placeholder :{name}"))
                })?;
                let value = binds.get(bind_index).ok_or_else(|| {
                    Error::DataConversionError(format!("missing value for bind :{name}"))
                })?;
                let parameter = match parameter_for_bind.get(&bind_index) {
                    Some(parameter) => *parameter,
                    None => {
                        let parameter = ordered.len() + 1;
                        parameter_for_bind.insert(bind_index, parameter);
                        ordered.push(value.clone());
                        parameter
                    }
                };
                out.push_str(&postgres_parameter(parameter, value));
                i = end;
                continue;
            }
        }
        let Some(character) = sql.get(i..).and_then(|s| s.chars().next()) else {
            break;
        };
        out.push(character);
        i += character.len_utf8();
    }

    Ok(BoundSql {
        sql: out,
        binds: ordered,
    })
}

/// Text values are cast on the server side. This preserves typed parameter
/// semantics without interpolating Oracle NUMBER's arbitrary precision text.
fn postgres_parameter(parameter: usize, value: &BindValue) -> String {
    let param = format!("${parameter}");
    match value {
        BindValue::Null | BindValue::String(_) => format!("{param}::text"),
        BindValue::Number(_) => format!("{param}::text::numeric"),
        BindValue::Bytes(_) => format!("{param}::bytea"),
        BindValue::Temporal(value) if value.starts_with("TIMESTAMPTZ '") => {
            format!("{param}::text::timestamptz")
        }
        BindValue::Temporal(_) => format!("{param}::text::timestamp"),
        BindValue::Boolean(_) => format!("{param}::boolean"),
        BindValue::BinaryDouble(_) => format!("{param}::text::double precision"),
    }
}

/// Return the generated timestamp text from a decoded Oracle temporal bind.
pub fn temporal_bind_text(value: &str) -> Result<&str> {
    value
        .split_once('\'')
        .and_then(|(_, rest)| rest.strip_suffix('\''))
        .ok_or_else(|| Error::DataConversionError("invalid decoded temporal bind".into()))
}

#[allow(clippy::too_many_arguments)]
pub fn build_query_response(
    columns: &[ColumnMeta],
    rows: &[Vec<Option<Vec<u8>>>],
    cursor_id: u16,
    has_more: bool,
    response_completion: bool,
    newer_describe_framing: bool,
    req_seq: u8,
) -> Bytes {
    build_query_response_inner(
        columns,
        rows,
        cursor_id,
        has_more,
        response_completion,
        newer_describe_framing,
        false,
        req_seq,
    )
}

/// Query (execute) response for an OCI thick client (python-oracledb thick,
/// SQL*Plus, …). OCI negotiates the *non-compact* TTC wire: every `ub2`/`ub4`/
/// `ub8` is a fixed little-endian integer, and `bytes-with-length` is a
/// `[u32-LE outer length][u8 inner length][bytes]` pair. The describe body,
/// `DESCRIBE_INFO` trailer, `ROW_HEADER` (`0x06`), `ROW_DATA` (`0x07`), the
/// post-row `0x08` block, the literal ROWID (`0x0d`) and the `0x04` end-of-call
/// were all reverse-engineered byte-for-byte from a live Oracle XE 21c capture
/// (see `dbsaci-known-gaps.md`, "OCI thick client").
///
/// The per-value encoding inside `ROW_DATA` is identical to the thin wire
/// (`[u8 length][bytes]`), so callers pass the same `rows` they build for
/// [`build_query_response`].
pub fn build_query_response_oci(
    columns: &[ColumnMeta],
    rows: &[Vec<Option<Vec<u8>>>],
    cursor_id: u16,
    has_more: bool,
    _req_seq: u8,
) -> Bytes {
    build_query_response_oci_ex(columns, rows, cursor_id, has_more, true)
}

/// [`build_query_response_oci`] with control over the leading `DESCRIBE_INFO`.
/// A `REEXECUTE_AND_FETCH` (`0x4e`) response must omit it — the client already
/// has the metadata and rejects (breaks on) a second describe.
pub fn build_query_response_oci_ex(
    columns: &[ColumnMeta],
    rows: &[Vec<Option<Vec<u8>>>],
    cursor_id: u16,
    has_more: bool,
    with_describe: bool,
) -> Bytes {
    let mut buf = WriteBuffer::new();
    let complete = !has_more;

    // ---- header + DESCRIBE_INFO ----------------------------------------
    buf.write_u16_be(0); // data flags
    // A live Oracle server prefixes the `0x10` DESCRIBE_INFO to *both* the
    // `0x5e` Execute reply and the `0x4e` REEXECUTE_AND_FETCH reply. Omitting
    // it on the `0x4e` reply works only while the client still holds a cached
    // describe for the cursor; once that is invalidated (a `conn.rollback()`
    // between corpus cases) it re-drives the fetch and then stalls on the
    // describe-less reply.
    buf.write_u8(0x10); // TTC_MSG_DESCRIBE_INFO
    write_oci_describe_body(&mut buf, columns);

    // ---- ROW_HEADER (0x06) --------------------------------------------------
    buf.write_u8(0x06);
    buf.write_u8(0x22);
    buf.write_u32_le(columns.len() as u32);
    buf.write_u16_le(0);
    buf.write_u32_le(2); // constant (Oracle's prefetch size)
    buf.write_zeros(10);

    // ---- ROW_DATA (0x07) per prefetched row -------------------------------
    for row in rows {
        buf.write_u8(0x07);
        for i in 0..columns.len() {
            let value = row.get(i).cloned().flatten();
            match value {
                Some(bytes) => {
                    buf.write_u8(bytes.len() as u8);
                    buf.write_bytes(&bytes);
                }
                None => buf.write_u8(0x00), // NULL = zero length
            }
        }
    }

    // ---- post-row 0x08 block + literal ROWID (0x5e Execute only) ---------
    if with_describe {
        buf.write_u8(0x08);
        buf.write_u8(0x06);
        buf.write_bytes(&[0x00, 0xab, 0x6d, 0x2d]);
        buf.write_zeros(5);
        buf.write_u32_le(cursor_id as u32);
        buf.write_zeros(20);

        buf.write_u8(0x0d);
        let rowid = oci_random_rowid();
        buf.write_u32_be(rowid.len() as u32);
        buf.write_bytes(&rowid);
    }

    // ---- 0x04 end-of-call -------------------------------------------------
    let rows_this = rows.len().min(255) as u8;
    if complete && with_describe {
        // 0x5e Execute, all rows fit — verified byte-for-byte against a live
        // Oracle single-row capture.
        let ctr = cursor_id.wrapping_add(6) as u8;
        buf.write_u8(0x04);
        buf.write_u32_le(1);
        buf.write_u8(ctr);
        buf.write_bytes(&[0x00, rows_this, 0x00, 0x00, 0x00]);
        buf.write_u16_le(1403);
        buf.write_zeros(4);
        buf.write_u16_le(cursor_id);
        buf.write_u16_le((columns.len() as u16).saturating_mul(16).wrapping_sub(2));
        buf.write_u16_le(3);
        buf.write_zeros(22);
        buf.write_bytes(&[ctr.wrapping_sub(2), 0x00, 0x00, 0x01]);
        buf.write_zeros(17);
        buf.write_u16_le(1403);
        buf.write_u16_le(0);
        buf.write_bytes(&[0x01, 0x01]);
        let msg = b"ORA-01403: no data found\n";
        buf.write_u8(msg.len() as u8);
        buf.write_bytes(msg);
    } else if complete {
        // 0x4e REEXECUTE_AND_FETCH, all rows fit — from a live single-row
        // `0x4e` capture (`SELECT '…' FROM DUAL` re-run). Distinct middle
        // section from the `0x5e` form: two constant `u32-LE`s (2, 3), a `07`
        // marker, then the trailing `ORA-01403`.
        let ctr = cursor_id.wrapping_add(0xd8) as u8;
        buf.write_u8(0x04);
        buf.write_u32_le(1);
        buf.write_u8(ctr);
        buf.write_bytes(&[0x00, rows_this, 0x00, 0x00, 0x00]);
        buf.write_u16_le(1403);
        buf.write_zeros(4);
        buf.write_u32_le(2);
        buf.write_u32_le(3);
        buf.write_zeros(20);
        buf.write_u8(0x07);
        buf.write_zeros(2);
        buf.write_u8(0x01);
        buf.write_zeros(17);
        buf.write_u16_le(1403);
        buf.write_u16_le(0);
        buf.write_bytes(&[0x01, 0x01]);
        let msg = b"ORA-01403: no data found\n";
        buf.write_u8(msg.len() as u8);
        buf.write_bytes(msg);
    } else if with_describe {
        // 0x5e Execute, "more rows to fetch" — from the live `SELECT 'p' UNION
        // 'q' UNION 'r'` capture.
        let ctr = cursor_id.wrapping_mul(2).wrapping_add(21) as u8;
        let mystery = cursor_id.wrapping_mul(10).wrapping_add(1) as u8;
        buf.write_u8(0x04);
        buf.write_u32_le(1);
        buf.write_u8(ctr);
        buf.write_bytes(&[0x00, rows_this]);
        buf.write_zeros(8);
        buf.write_bytes(&[0x00, cursor_id as u8]);
        buf.write_bytes(&[0x00, mystery]);
        buf.write_bytes(&[0x00, 0x03]);
        buf.write_zeros(23);
        buf.write_u8(ctr.wrapping_sub(0x13));
        buf.write_zeros(2);
        buf.write_u8(0x01);
        buf.write_zeros(21);
        buf.write_bytes(&[0x01, 0x02]);
    } else {
        // 0x4e REEXECUTE_AND_FETCH, "more rows" — from the live 0x4e capture.
        let ctr = cursor_id.wrapping_add(0x86) as u8;
        buf.write_u8(0x04);
        buf.write_u32_le(1);
        buf.write_u8(ctr);
        buf.write_bytes(&[0x00, rows_this]);
        buf.write_zeros(4);
        buf.write_u8(0x00);
        buf.write_zeros(4);
        buf.write_u32_le(cursor_id as u32);
        buf.write_u32_le(3);
        buf.write_zeros(20);
        buf.write_u8(ctr.wrapping_sub(0x81));
        buf.write_bytes(&[0x00, 0x00, 0x01]);
        buf.write_zeros(21);
        buf.write_bytes(&[0x01, 0x02]);
    }

    buf.freeze()
}

/// The `DESCRIBE_INFO` body (everything after the `0x10` message byte) for OCI:
/// `skip_bytes` blob, 3 straddled date bytes, `[u32-LE max row size]`,
/// `[u32-LE column count]`, a `0x5c` marker, one metadata block per column, then
/// the trailer (current date, four `u32-LE`, an empty `dcbqcky`).
fn write_oci_describe_body(buf: &mut WriteBuffer, columns: &[ColumnMeta]) {
    // `buf.skip_bytes()` blob: a real server sends a 23-byte describe id/hash;
    // OCI only skips it, so the content is irrelevant.
    buf.write_u8(23);
    buf.write_zeros(23);
    buf.write_bytes(&[0x03, 0x05, 0x2c]); // 3 leftover date bytes

    let max_row_size: u32 = columns
        .iter()
        .map(|c| c.buffer_size.max(c.max_size).max(1))
        .sum();
    buf.write_u32_le(max_row_size);
    buf.write_u32_le(columns.len() as u32);
    buf.write_u8(0x5c);

    for (i, col) in columns.iter().enumerate() {
        buf.write_u8(col.oracle_type);
        buf.write_u8(col.flags);
        buf.write_u8(col.precision as u8);
        buf.write_u8(col.scale as u8);
        buf.write_u32_le(col.buffer_size);
        buf.write_zeros(11); // max-arr + cont-flags + oid (always zero)
        buf.write_u16_le(col.charset_id);
        buf.write_u8(col.charset_form);
        buf.write_u16_le(col.max_size as u16);
        buf.write_u16_le(0);
        buf.write_u16_le(if col.charset_id != 0 { 0x3ffe } else { 0 });
        buf.write_u16_le(0);
        // name section: 0x01, len, len, [u32-BE len], bytes
        let name = col.name.as_bytes();
        let nlen = name.len().min(255) as u8;
        buf.write_bytes(&[0x01, nlen, nlen]);
        buf.write_u32_be(nlen as u32);
        buf.write_bytes(&name[..nlen as usize]);
        buf.write_u32_le(0); // schema
        buf.write_u32_le(0); // type name
        buf.write_u16_le(i as u16); // column position (0-based)
        buf.write_u32_le(0); // uds flags
    }

    // trailer: current date as bytes-with-length, then four u32-LE, then dcbqcky
    buf.write_u32_le(7);
    buf.write_u8(7);
    buf.write_bytes(&[0x78, 0x7e, 0x08, 0x1e, 0x03, 0x05, 0x33]);
    buf.write_u32_le(0);
    buf.write_u32_le(0x1fe8);
    buf.write_u32_le(2);
    buf.write_u32_le(2);
    buf.write_u32_le(0); // dcbqcky (empty)
}

/// A 13-character alphanumeric literal ROWID token, as Oracle emits before the
/// `0x04` end-of-call on a query.
fn oci_random_rowid() -> [u8; 13] {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut out = [0u8; 13];
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E3779B97F4A7C15);
    for slot in &mut out {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *slot = ALPHABET[(seed >> 33) as usize % ALPHABET.len()];
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn build_query_response_inner(
    columns: &[ColumnMeta],
    rows: &[Vec<Option<Vec<u8>>>],
    cursor_id: u16,
    has_more: bool,
    response_completion: bool,
    newer_describe_framing: bool,
    oci_dialect: bool,
    req_seq: u8,
) -> Bytes {
    let mut buf = WriteBuffer::new();

    // python-oracledb thin keys "response complete" off the
    // `TNS_DATA_FLAGS_END_OF_RESPONSE` (0x2000) data-flags bit on the final
    // packet; without it it blocks waiting for another packet. ojdbc thin does
    // not use that bit (it terminates its receive loop on a STATUS message).
    buf.write_u16_be(
        if response_completion && !newer_describe_framing && !oci_dialect && !has_more {
            0x2000
        } else {
            0
        },
    );
    buf.write_u8(0x10); // DescribeInfo
    if newer_describe_framing {
        write_describe_jdbc(&mut buf, columns);
    } else {
        // Both oracle-rs and python-oracledb consume a leading chunked-bytes
        // field before the describe body (`skip_raw_bytes_chunked` /
        // `buf.skip_bytes()` in the DESCRIBE_INFO dispatch arm). Keep empty.
        // NOTE: ojdbc thin's describe parser uses a different layout
        // (numuds first, no leading chunk / max-row-size); making it fully
        // reuse a `Statement` still needs `write_describe_jdbc` wired here.
        if oci_dialect {
            // OCI's describe parser wants a non-empty `skip_bytes` block here
            // (a real Oracle server puts a 23-byte describe id / hash).
            buf.write_u8(23);
            buf.write_u32_be(0x17);
            buf.write_bytes(&[0u8; 19]);
        } else {
            buf.write_u8(0);
        }
        buf.write_ub4(0); // max row size
        buf.write_ub4(columns.len() as u32); // number of columns
        buf.write_u8(0); // skip
        for col in columns {
            buf.write_u8(col.oracle_type);
            buf.write_u8(col.flags);
            buf.write_u8(col.precision as u8);
            buf.write_u8(col.scale as u8);
            buf.write_ub4(col.buffer_size);
            buf.write_ub4(0); // max num array elements
            buf.write_ub8(0); // cont flags
            // OID: an empty `bytes-with-length` (0x00). Thin drivers read this
            // field's length with `read_ub4`, which rejects the 0xFF NULL
            // sentinel as a negative integer (DPY-5003).
            buf.write_bytes_with_length(Some(&[]));
            buf.write_ub2(1); // version
            buf.write_ub2(col.charset_id);
            buf.write_u8(col.charset_form);
            buf.write_ub4(col.max_size);
            buf.write_ub4(0); // oaccolid
            buf.write_u8(if col.nullable { 1 } else { 0 });
            buf.write_u8(0); // v7 name length
            write_string_with_ub4_length(&mut buf, Some(&col.name));
            write_string_with_ub4_length(&mut buf, col.schema.as_deref());
            write_string_with_ub4_length(&mut buf, col.type_name.as_deref());
            buf.write_ub2(col.position);
            buf.write_ub4(0); // uds flags
        }
        // current_date indicator etc
        buf.write_ub4(0);
        buf.write_ub4(0);
        buf.write_ub4(0);
        buf.write_ub4(0);
        buf.write_ub4(0);
        buf.write_ub4(0);
    }

    // ojdbc's execute+fetch response frames rows as a RowHeader (`0x06`)
    // then one `0x07` RowData each.
    if newer_describe_framing && !rows.is_empty() {
        let n = rows.len() as u16;
        buf.write_u8(0x06); // RowHeader
        buf.write_u8(0); // rxhflg (no bits 8/16)
        buf.write_ub2(n); // numRqsts
        buf.write_ub2(0); // iterNum
        buf.write_ub2(n); // numItersThisTime
        buf.write_ub2(0); // uacBufLength (must be 0)
        write_dalc(&mut buf, &[]); // bit-vector
        write_dalc(&mut buf, &[]); // trailing DALC
    }

    // RowData
    for row in rows {
        buf.write_u8(0x07); // RowData
        for (i, _col) in columns.iter().enumerate() {
            let value = row.get(i).cloned().flatten();
            buf.write_bytes_with_length(value.as_deref());
        }
    }

    if response_completion {
        // `ORA-01403 no data found` when the result is complete: strict thin
        // drivers keep `_more_rows_to_fetch` set (and the fetch loop alive)
        // until they see it. ojdbc's `isORA1403Ignored()` also treats 1403 on a
        // SELECT as clean end-of-fetch.
        let (code, msg): (u32, Option<&str>) = if has_more {
            (0, None)
        } else {
            (1403, Some("ORA-01403: no data found"))
        };
        if newer_describe_framing {
            // ojdbc thin's receive loop breaks out on the `0x04` end-of-call
            // itself — it never reads a trailing STATUS, so a `0x09` here would
            // be left in the buffer and desync the next call. ODP.NET managed
            // behaves the same.
            write_end_of_call_jdbc(
                &mut buf,
                code,
                msg,
                row_count_field(has_more),
                has_more,
                req_seq,
                cursor_id,
                0,
            );
        } else {
            write_end_of_call_ext(
                &mut buf,
                code,
                msg,
                row_count_field(has_more),
                has_more,
                true,
                0,
            );
            if !oci_dialect {
                buf.write_u8(TTC_MSG_END_OF_RESPONSE);
            }
        }
    } else {
        write_end_of_call(&mut buf, 0, None, row_count_field(has_more), has_more, 0);
    }

    buf.freeze()
}

/// A fetch (continuation) response: RowHeader + RowData for each row, then the
/// end-of-call marker. No DescribeInfo — the client already has the metadata.
///
/// For `response_completion` (python-oracledb thin) the tail must mirror
/// [`build_query_response`]'s strict branch: when the cursor is drained it MUST
/// carry `ORA-01403` and the `TNS_DATA_FLAGS_END_OF_RESPONSE` (`0x2000`) bit,
/// or the driver keeps `_more_rows_to_fetch` set — the next `execute()` on that
/// cursor is then sent as a bare Fetch against the (now closed) server cursor
/// and returns nothing. oracle-rs uses the lenient
/// `parse_error_message_info` path (10-byte literal rowid, no 20c fields, no
/// end-of-response marker).
/// A Fetch continuation response for an OCI thick client: a `ROW_HEADER`
/// (`0x06 0x02` subtype, carrying the echoed request count and this batch's row
/// count), one `0x07` `ROW_DATA` per row, then the LE `0x04` end-of-call. No
/// `DESCRIBE_INFO`. Reverse-engineered from a live Oracle XE 21c capture.
pub fn build_fetch_response_oci(
    columns: &[ColumnMeta],
    rows: &[Vec<Option<Vec<u8>>>],
    req_count: u32,
    cursor_id: u16,
    has_more: bool,
    rows_total: u64,
) -> Bytes {
    let mut buf = WriteBuffer::new();
    let ncol = columns.len() as u32;
    let complete = !has_more;
    let ctr = cursor_id.wrapping_add(0x86) as u8;
    let total = (rows_total & 0xff) as u8;
    const MSG: &[u8] = b"ORA-01403: no data found\n";

    buf.write_u16_be(0); // data flags

    if !rows.is_empty() {
        // ROW_HEADER (fetch subtype 0x02). Layout verified from a live
        // REEXECUTE/FETCH capture: no inter-row separators, two trailing zero
        // u32s.
        buf.write_u8(0x06);
        buf.write_u8(0x02);
        buf.write_u32_le(ncol);
        buf.write_u16_le(0);
        buf.write_u32_le(req_count);
        buf.write_u16_le(0);
        buf.write_u32_le(0);
        buf.write_u32_le(0);
        for row in rows {
            buf.write_u8(0x07);
            for i in 0..columns.len() {
                match row.get(i).cloned().flatten() {
                    Some(v) => {
                        buf.write_u8(v.len() as u8);
                        buf.write_bytes(&v);
                    }
                    None => buf.write_u8(0x00),
                }
            }
        }
    }

    // end-of-call
    buf.write_u8(0x04);
    buf.write_u32_le(1);
    buf.write_u8(ctr);
    buf.write_bytes(&[0x00, total]);
    buf.write_zeros(3);
    buf.write_u16_le(if complete { 1403 } else { 0 });
    buf.write_zeros(4);
    if rows.is_empty() {
        buf.write_bytes(&[0x01, 0x00, 0x00, 0x00, 0x03]);
        buf.write_zeros(23);
    } else {
        buf.write_u32_le(cursor_id as u32);
        buf.write_u32_le(3);
        buf.write_zeros(20);
    }
    buf.write_u8(ctr.wrapping_sub(0x81));
    buf.write_zeros(20);
    buf.write_u16_le(if complete { 1403 } else { 0 });
    buf.write_u16_le(0);
    if complete {
        buf.write_bytes(&[0x01, if rows.is_empty() { 0x02 } else { total }]);
        buf.write_u8(MSG.len() as u8);
        buf.write_bytes(MSG);
    } else {
        buf.write_bytes(&[0x01, 0x02]);
    }

    buf.freeze()
}

pub fn build_fetch_response(
    rows: &[Vec<Option<Vec<u8>>>],
    _cursor_id: u16,
    has_more: bool,
    response_completion: bool,
) -> Bytes {
    let mut buf = WriteBuffer::new();
    buf.write_u16_be(if response_completion && !has_more {
        0x2000
    } else {
        0
    });
    for row in rows {
        write_row_header(&mut buf);
        buf.write_u8(0x07); // RowData
        for field in row {
            buf.write_bytes_with_length(field.as_deref());
        }
    }
    let (code, msg): (u32, Option<&str>) = if has_more {
        (0, None)
    } else {
        (1403, Some("ORA-01403: no data found"))
    };
    if response_completion {
        write_end_of_call_ext(
            &mut buf,
            code,
            msg,
            row_count_field(has_more),
            has_more,
            true,
            0,
        );
        buf.write_u8(TTC_MSG_END_OF_RESPONSE);
    } else {
        write_fetch_end_of_call(&mut buf, code, msg, row_count_field(has_more), has_more);
    }
    buf.freeze()
}

/// End-of-call layout for Fetch continuations to oracle-rs (`parse_error_message_info`:
/// 10-byte literal rowid, stops after `error_num` + `row_count`).
fn write_fetch_end_of_call(
    buf: &mut WriteBuffer,
    error_code: u32,
    message: Option<&str>,
    row_count: u64,
    has_more: bool,
) {
    buf.write_u8(0x04);
    buf.write_ub4(0); // call status
    buf.write_ub2(0); // end-to-end seq
    buf.write_ub4(0); // current row number
    buf.write_ub2(0); // error number short
    buf.write_ub2(0); // array elem error
    buf.write_ub2(0); // array elem error
    buf.write_ub2(if has_more { 1 } else { 0 }); // cursor ID
    buf.write_sb2(0); // error position
    buf.write_zeros(5); // SQL type, fatal, flags, cursor options, UPI
    buf.write_u8(if has_more { 0x20 } else { 0 }); // flags: 0x20 = more rows
    buf.write_zeros(10); // rowid: 10 literal bytes
    buf.write_ub4(0); // OS error
    buf.write_u8(0); // statement number
    buf.write_u8(0); // call number
    buf.write_ub2(0); // padding
    buf.write_ub4(0); // success iterations
    buf.write_ub4(0); // oerrdd num_bytes
    buf.write_ub2(0); // batch error codes count
    buf.write_ub4(0); // batch error offsets count
    buf.write_ub2(0); // batch error messages count
    buf.write_ub4(error_code);
    buf.write_ub8(row_count);
    if error_code != 0 {
        buf.write_string_with_length(message);
    }
}

/// Fetch (continuation) response for the jdbc-style clients (ojdbc thin, ODP.NET
/// managed): the Oracle-standard `0x06` RowHeader, one `0x07` RowData per row,
/// then the shared `0x04` end-of-call (`1403` = end of fetch on the last
/// batch), no STATUS.
pub fn build_fetch_response_jdbc(
    rows: &[Vec<Option<Vec<u8>>>],
    cursor_id: u16,
    has_more: bool,
    req_seq: u8,
) -> Bytes {
    let mut buf = WriteBuffer::new();
    buf.write_u16_be(0); // data flags
    if !rows.is_empty() {
        let n = rows.len() as u16;
        buf.write_u8(0x06); // RowHeader
        buf.write_u8(0); // rxhflg
        buf.write_ub2(n); // numRqsts
        buf.write_ub2(0); // iterNum
        buf.write_ub2(n); // numItersThisTime
        buf.write_ub2(0); // uacBufLength
        write_dalc(&mut buf, &[]); // bit-vector
        write_dalc(&mut buf, &[]); // trailing DALC
    }
    for row in rows {
        buf.write_u8(0x07); // RowData
        for field in row {
            buf.write_bytes_with_length(field.as_deref());
        }
    }
    let (code, msg): (u32, Option<&str>) = if has_more {
        (0, None)
    } else {
        (1403, Some("ORA-01403: no data found"))
    };
    write_end_of_call_jdbc(
        &mut buf,
        code,
        msg,
        row_count_field(has_more),
        has_more,
        req_seq,
        cursor_id,
        0,
    );
    buf.freeze()
}

/// oracle-rs derives "more rows" from `row_count > 0` on the end-of-call
/// message; keep it non-zero only while the cursor is still open.
fn row_count_field(has_more: bool) -> u64 {
    if has_more { 1 } else { 0 }
}

/// Parse a TTC Fetch request: `(cursor_id, requested row count)`.
pub fn parse_fetch_request(payload: &[u8]) -> Result<(u16, u32)> {
    let mut buf = ReadBuffer::from_slice(payload);
    buf.read_u16_be()?; // data flags
    buf.read_u8()?; // message type
    buf.read_u8()?; // function code
    buf.read_u8()?; // sequence number
    let cursor_id = buf.read_ub4()? as u16;
    let num_rows = buf.read_ub4()?;
    Ok((cursor_id, num_rows))
}

/// Parse an OCI Fetch request: `[u16 flags][u8 0x03][u8 0x05][u8 seq]
/// [u32-LE cursor id][u32-LE row count]`.
pub fn parse_fetch_request_oci(payload: &[u8]) -> Result<(u16, u32)> {
    if payload.len() < 13 {
        return Err(Error::Protocol("OCI fetch request too short".into()));
    }
    let cursor_id = u32::from_le_bytes([payload[5], payload[6], payload[7], payload[8]]) as u16;
    let num_rows = u32::from_le_bytes([payload[9], payload[10], payload[11], payload[12]]);
    Ok((cursor_id, num_rows))
}

/// Build the no-row response expected after a successful DML or DDL call.
pub fn build_dml_response(rows_affected: u64) -> Bytes {
    let mut buf = WriteBuffer::new();
    buf.write_u16_be(0); // data flags
    write_end_of_call(&mut buf, 0, None, rows_affected, false, 0);
    buf.freeze()
}

/// Successful-DML acknowledgement for an OCI thick client. Structure replicated
/// from a live `INSERT` capture (SCN fields zeroed); carries the affected-row
/// count and a literal ROWID.
pub fn build_dml_response_oci(
    rows_affected: u64,
    cursor_id: u16,
    req_seq: u8,
    stmt_kind: DmlKind,
) -> Bytes {
    // Layout replicated byte-for-byte from live `INSERT` / `UPDATE` / `DELETE`
    // captures (SCN fields zeroed). Three fields must carry live values or the
    // OCI client caches the statement under the wrong cursor id and a later
    // `0x04` REEXECUTE resolves to a different statement:
    //   [13..17] server cursor id (u32 LE)  — the id the client will name
    //   [17..21] affected-row count (u32 LE)
    //   [60]     call number  = req_seq + 0x8b
    //   [72..74] server cursor id again (u16 LE)
    //   [100]    req_seq
    // `stmt_kind` selects the op codes Oracle writes at [74]/[76].
    let mut buf = WriteBuffer::new();
    let rows = (rows_affected & 0xff) as u8;
    let (k74, k76) = match stmt_kind {
        DmlKind::Insert => (0x0cu8, 0x02u8),
        DmlKind::Update => (0x07, 0x06),
        DmlKind::Delete => (0x0c, 0x07),
        DmlKind::Merge => (0x0b, 0xbd),
        DmlKind::Other => (0x0c, 0x02),
    };
    buf.write_u16_be(0); // data flags
    buf.write_u8(0x08);
    buf.write_u8(0x06);
    buf.write_u32_le(0); // SCN
    buf.write_zeros(5); // [8..13]
    buf.write_u32_le(cursor_id as u32); // [13..17] server cursor id
    buf.write_u32_le(rows_affected as u32); // [17..21] affected rows
    buf.write_zeros(16); // [21..37]
    buf.write_u8(0x0d);
    let rowid = oci_random_rowid();
    buf.write_u32_be(rowid.len() as u32);
    buf.write_bytes(&rowid);
    // real end-of-call
    buf.write_u8(0x04); // [55]
    buf.write_u32_le(2); // [56..60]
    buf.write_u8(req_seq.wrapping_add(0x8b)); // [60] call number
    buf.write_zeros(11); // [61..72]
    buf.write_u16_le(cursor_id); // [72..74] server cursor id
    buf.write_u8(k74); // [74]
    buf.write_u8(0x00); // [75]
    buf.write_u8(k76); // [76]
    buf.write_zeros(4); // [77..81]
    buf.write_zeros(10); // [81..91] SCN
    buf.write_zeros(2); // [91..93]
    buf.write_bytes(&[0x02, 0x00, 0x00, 0x00]); // [93..97]
    buf.write_zeros(3); // [97..100]
    buf.write_u8(req_seq); // [100]
    buf.write_zeros(2); // [101..103]
    buf.write_u8(0x01); // [103]
    buf.write_zeros(3); // [104..107]
    buf.write_u8(0x0d); // [107]
    buf.write_u8(0x00); // [108]
    buf.write_u8(0x0d); // [109]
    buf.write_bytes(&[0x01, 0x00, 0x01, 0x28]); // [110..114]
    buf.write_zeros(7); // [114..121] SCN2
    buf.write_zeros(18); // [121..139]
    buf.write_bytes(&[0x01, rows]); // [139..141]
    buf.freeze()
}

/// Successful-DDL acknowledgement for an OCI thick client (`CREATE`/`ALTER`/
/// `DROP` and other statements that do not report an affected-row count).
/// Layout replicated byte-for-byte from a live `CREATE TABLE` capture (SCN
/// fields zeroed). Distinct from [`build_dml_response_oci`]: a single
/// end-of-call block, no row count, and a trailing `09` STATUS marker.
pub fn build_ddl_response_oci(cursor_id: u16, req_seq: u8) -> Bytes {
    // Byte-for-byte from a live `CREATE TABLE` capture. As with
    // [`build_dml_response_oci`], the server cursor id ([13..17] and [72..74])
    // and the call number / seq ([60] = req_seq + 0x8b, [100] = req_seq) must
    // be live so a later `0x04` REEXECUTE of a cached `CREATE ... IF NOT
    // EXISTS` resolves back to the same statement.
    let mut buf = WriteBuffer::new();
    buf.write_u16_be(0); // data flags
    buf.write_u8(0x08);
    buf.write_u8(0x06);
    buf.write_u32_le(0); // [4..8] SCN
    buf.write_zeros(5); // [8..13]
    buf.write_u32_le(cursor_id as u32); // [13..17] server cursor id
    buf.write_zeros(20); // [17..37]
    buf.write_u8(0x0d); // [37]
    let rowid = oci_random_rowid();
    buf.write_u32_be(rowid.len() as u32); // [38..42]
    buf.write_bytes(&rowid); // [42..55]
    buf.write_u8(0x04); // [55] end-of-call
    buf.write_u32_le(1); // [56..60]
    buf.write_u8(req_seq.wrapping_add(0x8b)); // [60] call number
    buf.write_zeros(11); // [61..72]
    buf.write_u16_le(cursor_id); // [72..74] server cursor id
    buf.write_zeros(2); // [74..76]
    buf.write_u8(0x01); // [76]
    buf.write_zeros(23); // [77..100]
    buf.write_u8(req_seq); // [100]
    buf.write_bytes(&[0x00, 0x00, 0x01]); // [101..104]
    buf.write_zeros(22); // [104..126]
    buf.freeze()
}

/// Acknowledgement for an OCI `REEXECUTE` (`0x04`) of a non-query statement
/// (the client re-runs a statement already prepared on the cursor, with no
/// fresh parse/describe). Structure replicated from a live back-to-back
/// `INSER​T` reexecute capture: a single `04` end-of-call block, no `08 06`
/// row header and no literal ROWID, ending in `01 <rowcount>`.
pub fn build_reexecute_response_oci(rows_affected: u64, req_seq: u8) -> Bytes {
    let mut buf = WriteBuffer::new();
    let rows = (rows_affected & 0xff) as u8;
    let ctr = req_seq.wrapping_add(0x58);
    buf.write_u16_be(0); // data flags
    buf.write_u8(0x04); // [0] end-of-call
    buf.write_u32_le(2); // [1..5]
    buf.write_u8(ctr); // [5] call counter
    buf.write_u8(0x00); // [6]
    buf.write_u8(rows); // [7] affected-row count
    buf.write_zeros(9); // [8..17]
    buf.write_bytes(&[0x02, 0x00, 0x00, 0x00]); // [17..21]
    buf.write_bytes(&[0x02, 0x00, 0x00, 0x00]); // [21..25]
    buf.write_zeros(2); // [25..27]
    buf.write_bytes(&[0xc2, 0x28, 0x01, 0x00, 0x01, 0x00, 0x00]); // [27..34]
    buf.write_zeros(4); // [34..38]  (SCN-ish, zeroed)
    buf.write_u8(0x01); // [38] reexecute count (approx)
    buf.write_zeros(6); // [39..45]
    buf.write_u8(req_seq); // [45] request seq
    buf.write_zeros(2); // [46..48]
    buf.write_bytes(&[0x01, 0x00, 0x00, 0x00]); // [48..52]
    buf.write_bytes(&[0x0d, 0x00, 0x0d, 0x01, 0x00, 0x01, 0x28]); // [52..59]
    buf.write_u8(0xc2); // [59]
    buf.write_bytes(&[0x00, 0x01, 0x00, 0x00]); // [60..64]
    buf.write_zeros(2); // [64..66]  (SCN-ish, zeroed)
    buf.write_u8(0x00); // [66]
    buf.write_u8(0x01); // [67] reexecute count (approx)
    buf.write_zeros(16); // [68..84]
    buf.write_bytes(&[0x01, rows]); // [84..86]
    buf.freeze()
}

/// Which DML verb produced a row-affected response (OCI writes a per-verb op
/// code into its end-of-call).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmlKind {
    Insert,
    Update,
    Delete,
    Merge,
    Other,
}

impl DmlKind {
    /// Classify from the leading keyword of a SQL string.
    pub fn of(sql: &str) -> Self {
        match sql
            .trim_start()
            .split_ascii_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_uppercase()
            .as_str()
        {
            "INSERT" => DmlKind::Insert,
            "UPDATE" => DmlKind::Update,
            "DELETE" => DmlKind::Delete,
            "MERGE" => DmlKind::Merge,
            _ => DmlKind::Other,
        }
    }
}

/// Response to an Execute of a `RETURNING … INTO` DML statement, for
/// python-oracledb thin (`response_completion`). Sequence the driver's
/// `MessageWithData._process_message` consumes: `TNS_MSG_TYPE_IO_VECTOR` (bind
/// directions), `TNS_MSG_TYPE_ROW_DATA` (per OUT bind, `ub4` row count then the
/// returned column values), `TNS_MSG_TYPE_PARAMETER` (empty return-params), then
/// the end-of-call + end-of-response terminator.
///
/// `out_values[k]` is the list of wire-encoded values for the k-th OUT bind
/// (one per row the DML touched). `input_bind_count` OUT binds follow the input
/// ones in `_bind_info_list`.
pub fn build_returning_response(
    input_bind_count: usize,
    out_values: &[Vec<Option<Vec<u8>>>],
    rows_affected: u64,
) -> Bytes {
    const TNS_MSG_TYPE_IO_VECTOR: u8 = 11;
    const TNS_MSG_TYPE_ROW_DATA: u8 = 7;
    const TNS_BIND_DIR_INPUT: u8 = 32;
    const TNS_BIND_DIR_OUTPUT: u8 = 16;
    let num_binds = input_bind_count + out_values.len();

    let mut buf = WriteBuffer::new();
    buf.write_u16_be(0x2000); // data flags: END_OF_RESPONSE on this (only) packet

    // --- IO vector ---
    buf.write_u8(TNS_MSG_TYPE_IO_VECTOR);
    buf.write_u8(0); // flag
    buf.write_ub2(num_binds as u16); // num requests
    buf.write_ub4(0); // num iters  (num_binds = iters*256 + requests)
    buf.write_ub4(0); // num iters this time
    buf.write_ub2(0); // uac buffer length
    buf.write_ub2(0); // fast-fetch bit vector bytes
    buf.write_ub2(0); // rowid bytes
    for k in 0..num_binds {
        buf.write_u8(if k < input_bind_count {
            TNS_BIND_DIR_INPUT
        } else {
            TNS_BIND_DIR_OUTPUT
        });
    }

    // --- Row data: the returned values for each OUT bind ---
    buf.write_u8(TNS_MSG_TYPE_ROW_DATA);
    for values in out_values {
        buf.write_ub4(values.len() as u32);
        for value in values {
            buf.write_bytes_with_length(value.as_deref());
            // `_process_column_data` reads a trailing `sb4` "actual length" after
            // every OUT-bind value when not fetching (0 = not truncated).
            buf.write_ub4(0);
        }
    }

    // --- Return parameters (empty) ---
    buf.write_u8(TTC_MSG_PARAMETER);
    buf.write_ub2(0); // al8o4l
    buf.write_ub2(0); // al8txl bytes
    buf.write_ub2(0); // key/value pair count
    buf.write_ub2(0); // registration info bytes

    // --- End of call + end of response ---
    write_end_of_call_ext(&mut buf, 0, None, rows_affected, false, true, 0);
    buf.write_u8(TTC_MSG_END_OF_RESPONSE);
    buf.freeze()
}

/// DML/DDL acknowledgement for ojdbc thin: the `0x04` end-of-call carries the
/// affected-row count and needs no trailing STATUS.
pub fn build_dml_response_jdbc(rows_affected: u64, req_seq: u8) -> Bytes {
    let mut buf = WriteBuffer::new();
    buf.write_u16_be(0); // data flags
    write_end_of_call_jdbc(&mut buf, 0, None, rows_affected, false, req_seq, 0, 0);
    buf.freeze()
}

/// Return a TTC Error message instead of disguising a PostgreSQL failure as success.
/// A standalone error reply. The end-of-call layout and terminator must match
/// what the client expects on a *successful* Execute of the same shape, or the
/// driver mis-reads the error number / message: strict thin drivers
/// (python-oracledb) key the 20c fields off the negotiated TTC version and want
/// the `0x1d` end-of-response + `0x2000` data flag; ojdbc / ODP.NET stop on the
/// `0x04` itself; oracle-rs uses the lenient `parse_error_info` path.
pub fn build_error_response(
    response_completion: bool,
    newer_describe_framing: bool,
    oci: bool,
    error_code: u32,
    message: &str,
) -> Bytes {
    build_error_response_at(
        response_completion,
        newer_describe_framing,
        oci,
        error_code,
        message,
        0,
    )
}

/// A standalone TTC error reply for an OCI thick client: the LE `0x04`
/// end-of-call carrying the short + extended Oracle error number and the
/// message text. Reverse-engineered byte-for-byte from a live `ORA-00942`
/// capture.
pub fn build_error_response_oci(
    error_code: u32,
    message: &str,
    error_pos: u16,
    req_seq: u8,
) -> Bytes {
    let mut buf = WriteBuffer::new();
    // OCI expects the fully-formed `ORA-nnnnn: <text>` string (unlike the thin
    // drivers, which prepend the code themselves).
    let full = if message.starts_with("ORA-") {
        message.to_string()
    } else {
        format!("ORA-{error_code:05}: {message}\n")
    };
    let msg = full.as_bytes();
    let mlen = msg.len().min(255) as u8;
    let code16 = (error_code & 0xffff) as u16;
    let pos = if error_pos == 0 { 14 } else { error_pos };
    // Layout replicated byte-for-byte from a live `ORA-00942` capture (SCN
    // fields zeroed — the OCI client only stores them). This frame is delivered
    // AFTER a BREAK + RESET marker exchange (see `write_error_response`);
    // sending it inline leaves the OCI call state un-reset.
    buf.write_u16_be(0); // data flags
    buf.write_u8(0x04); // [0] end-of-call
    buf.write_u32_le(1); // [1..5]
    buf.write_u8(req_seq.wrapping_add(0x9d)); // [5] call number
    buf.write_zeros(5); // [6..11]
    buf.write_u16_le(code16); // [11..13] short error number
    buf.write_zeros(4); // [13..17]
    buf.write_u8(0x03); // [17]
    buf.write_u8(0x00); // [18]
    buf.write_u16_le(pos); // [19..21] error position
    buf.write_u8(0x03); // [21]
    buf.write_zeros(23); // [22..45] SCN (zeroed)
    buf.write_u8(0x07); // [45]
    buf.write_zeros(20); // [46..66]
    buf.write_u16_le(code16); // [66..68] error number (repeat)
    buf.write_zeros(3); // [68..71]
    buf.write_u8(mlen); // [71] message length
    buf.write_bytes(&msg[..mlen as usize]);
    buf.freeze()
}

/// A standalone TTC error reply for an OCI thick client whose in-flight call
/// was aborted server-side (PostgreSQL `statement_timeout` -> ORA-01013).
///
/// Reverse-engineered byte-for-byte from a live Oracle XE capture of
/// `ALTER SYSTEM CANCEL SQL` interrupting a slow query: after the single RESET
/// marker exchange (see `write_error_response`) the server sends this frame.
/// It differs from the generic [`build_error_response_oci`] in three fixed
/// bytes -- most importantly `0x01` at head offset 50, a "call was reset"
/// flag. Without it python-oracledb thick treats the error as a stale reply
/// and re-drives the same Execute until its own `call_timeout` fires.
pub fn build_timeout_error_response_oci(req_seq: u8) -> Bytes {
    // Head = everything up to and including the 1-byte message length. Two
    // fields track the aborted call's sequence number and are validated by
    // python-oracledb thick -- a stale value makes it decide the error belongs
    // to an earlier call and re-drive the current Execute:
    //   offset  8: constant 0x01     (live captures — was 0x00 here)
    //   offset 19: `call_seq - 4`     (exact, across several live captures)
    //   offset 21: constant 0x00      (live captures — was 0x28 here)
    //   offset  7: a session-cumulative call counter the client checks; without
    //              a close value it decides the error is for an earlier call
    //              and re-drives the Execute. Approximated from `req_seq`.
    const HEAD: &[u8] = &[
        0x00, 0x00, 0x04, 0x01, 0x00, 0x00, 0x00, 0x21, 0x01, 0x00, 0x00, 0x00, 0x00, 0xf5, 0x03,
        0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x08, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xf5, 0x03, 0x00, 0x00, 0x00,
    ];
    const MSG: &[u8] = b"ORA-01013: user requested cancel of current operation\n";
    let mut head = HEAD.to_vec();
    head[19] = req_seq.wrapping_sub(4); // call seq - 4  (exact)
    head[47] = req_seq; // call seq     (exact)
    // Live `ALTER SYSTEM CANCEL SQL` samples: offset 7 grows ~9 per preceding
    // statement off a base near a fresh connection's first slow query
    // (seq 8 -> 0xb7). Approximate; the client tolerates a small window.
    head[7] = (0xb7u8).wrapping_add(req_seq.wrapping_sub(8).wrapping_mul(9));
    let mut buf = WriteBuffer::new();
    buf.write_bytes(&head);
    buf.write_u8(MSG.len() as u8);
    buf.write_bytes(MSG);
    buf.freeze()
}

/// [`build_error_response`] carrying the PostgreSQL statement character position
/// in the Oracle `error_pos` field (0 = none).
pub fn build_error_response_at(
    response_completion: bool,
    newer_describe_framing: bool,
    oci: bool,
    error_code: u32,
    message: &str,
    error_pos: u16,
) -> Bytes {
    if oci {
        // `req_seq` is only cosmetic in the error frame (an echoed call
        // counter); callers that have it use `write_error_response` directly.
        return build_error_response_oci(error_code, message, error_pos, 0);
    }
    let mut buf = WriteBuffer::new();
    if newer_describe_framing {
        buf.write_u16_be(0);
        write_end_of_call_jdbc(
            &mut buf,
            error_code,
            Some(message),
            0,
            false,
            0,
            0,
            error_pos,
        );
    } else if response_completion {
        buf.write_u16_be(0x2000);
        write_end_of_call_ext(
            &mut buf,
            error_code,
            Some(message),
            0,
            false,
            true,
            error_pos,
        );
        buf.write_u8(TTC_MSG_END_OF_RESPONSE);
    } else {
        buf.write_u16_be(0);
        write_end_of_call(&mut buf, error_code, Some(message), 0, false, error_pos);
    }
    buf.freeze()
}

fn write_end_of_call(
    buf: &mut WriteBuffer,
    error_code: u32,
    message: Option<&str>,
    row_count: u64,
    has_more: bool,
    error_pos: u16,
) {
    // Error / End-of-call status. Field order follows oracle-rs's query-path
    // TTC decoder (`parse_error_info_with_rowcount`).
    buf.write_u8(0x04);
    buf.write_ub4(0); // call status
    buf.write_ub2(0); // end-to-end seq
    buf.write_ub4(0); // current row number
    buf.write_ub2(0); // error number short
    buf.write_ub2(0); // array elem error
    buf.write_ub2(0); // array elem error
    // Only advertise an open server-side cursor while rows remain. A zero here
    // tells the client the result is complete and no Fetch should follow.
    buf.write_ub2(if has_more { 1 } else { 0 }); // cursor ID
    buf.write_sb2(error_pos as i16); // error position
    buf.write_zeros(5); // SQL type, fatal, flags, cursor options, UPI
    buf.write_u8(if has_more { 0x20 } else { 0 }); // flags: 0x20 = more rows
    // UB values use their compact TTC representation.  The rowid and OS error
    // section has six zero-valued fields, not seventeen literal wire bytes.
    buf.write_zeros(6); // rowid (five fields) + OS error
    buf.write_u8(0); // statement number
    buf.write_u8(0); // call number
    buf.write_ub2(0); // padding
    buf.write_ub4(0); // success iterations
    buf.write_ub4(0); // logical rowid length
    buf.write_ub2(0); // batch error count
    buf.write_ub4(0); // batch error offset count
    buf.write_ub2(0); // batch error message count
    buf.write_ub4(error_code);
    buf.write_ub8(row_count);
    buf.write_ub4(0); // sql type (20c+)
    buf.write_ub4(0); // server checksum (20c+)
    if error_code != 0 {
        buf.write_string_with_length(message);
    }
}

/// `write_end_of_call` with the 20c+ trailing fields omitted when `strict`.
/// Strict thin drivers key the 20c fields off the negotiated TTC field
/// version (12.2 here), so writing them shifts the driver's read of the error
/// number / message.
fn write_end_of_call_ext(
    buf: &mut WriteBuffer,
    error_code: u32,
    message: Option<&str>,
    row_count: u64,
    has_more: bool,
    strict: bool,
    error_pos: u16,
) {
    if !strict {
        write_end_of_call(buf, error_code, message, row_count, has_more, error_pos);
        return;
    }
    buf.write_u8(0x04);
    buf.write_ub4(0); // call status
    buf.write_ub2(0); // end-to-end seq
    buf.write_ub4(0); // current row number
    buf.write_ub2(0); // error number short
    buf.write_ub2(0); // array elem error
    buf.write_ub2(0); // array elem error
    buf.write_ub2(if has_more { 1 } else { 0 }); // cursor ID
    buf.write_sb2(error_pos as i16); // error position
    buf.write_zeros(5); // sql type, fatal, flags, cursor options, UPI
    buf.write_u8(if has_more { 0x20 } else { 0 }); // flags
    // read_rowid: ub4 rba, ub2 partition, ub1 skip, ub4 block, ub2 slot = 5B zero
    buf.write_zeros(5);
    buf.write_ub4(0); // OS error
    buf.write_u8(0); // statement number
    buf.write_u8(0); // call number
    buf.write_ub2(0); // padding
    buf.write_ub4(0); // success iterations
    buf.write_u8(0); // oerrdd (bytes-with-length, empty)
    buf.write_ub2(0); // batch error codes count
    buf.write_ub4(0); // batch error offsets count
    buf.write_ub2(0); // batch error messages count
    buf.write_ub4(error_code); // extended error number
    buf.write_ub8(row_count); // extended row number
    if error_code != 0 {
        buf.write_string_with_length(message);
    }
}

/// A DALC field (`[ub4 length][CLR bytes]`) as ojdbc thin reads it. An empty
/// value is a bare `ub4(0)`.
fn write_dalc(buf: &mut WriteBuffer, bytes: &[u8]) {
    buf.write_ub4(bytes.len() as u32);
    if !bytes.is_empty() {
        // CLR short form: a length byte then the bytes (values below 64).
        debug_assert!(bytes.len() < 0x40);
        buf.write_u8(bytes.len() as u8);
        buf.write_bytes(bytes);
    }
}

/// DescribeInfo body laid out for ojdbc thin at negotiated TTC version 12 (so
/// the >=17/20/24 blocks are absent). Completely different framing from the
/// python-oracledb / oracle-rs describe: no leading chunk, no max-row-size, and
/// the per-column descriptor fields come in a different order. Verified
/// byte-for-byte against a live Oracle XE 21c wire capture.
fn write_describe_jdbc(buf: &mut WriteBuffer, columns: &[ColumnMeta]) {
    // prologue: `[ub1 n][n ignore bytes]` then an ignored ub4.
    buf.write_u8(0); // n = 0 ignore bytes
    buf.write_ub4(0); // ignored count
    // column count, then a skip byte when there is at least one column.
    buf.write_ub4(columns.len() as u32); // column count
    if !columns.is_empty() {
        buf.write_u8(0); // skip byte
    }
    for col in columns {
        // per-column type descriptor
        buf.write_u8(col.oracle_type); // datatype
        buf.write_u8(col.flags); // flags
        buf.write_u8(col.precision as u8); // precision
        buf.write_u8(col.scale as u8); // scale (sb1)
        buf.write_ub4(col.max_size.max(col.buffer_size)); // max length (sb4)
        buf.write_ub4(0); // max array length (sb4)
        buf.write_ub8(0); // extra flags (sb8)
        write_dalc(buf, &[]); // type OID
        buf.write_ub2(1); // descriptor version
        buf.write_ub2(col.charset_id); // charset id
        buf.write_u8(col.charset_form); // charset form
        buf.write_ub4(col.max_size); // max length in chars
        buf.write_ub4(0); // collation id
        // per-column name/schema descriptor
        buf.write_u8(if col.nullable { 1 } else { 0 }); // nullable
        buf.write_u8(col.name.len().min(255) as u8); // column-name length
        write_dalc(buf, col.name.as_bytes()); // column name
        write_dalc(buf, col.schema.as_deref().unwrap_or("").as_bytes()); // schema name
        write_dalc(buf, col.type_name.as_deref().unwrap_or("").as_bytes()); // type name
        buf.write_ub2(col.position); // key position
        buf.write_ub4(0); // flags
    }
    // tail (not from a ref cursor), TTC version 12
    write_dalc(buf, &[]); // trailing DALC
    buf.write_ub4(0); // (TTC>=3)
    buf.write_ub4(0);
    buf.write_ub4(0); // (TTC>=4)
    buf.write_ub4(0);
    write_dalc(buf, &[]); // (TTC>=5) query compile key
}

/// The `0x04` end-of-call for the jdbc-style clients (ojdbc thin, ODP.NET
/// managed): an end-of-call-status ub4 (advertised via
/// `SERVER_COMPILE_CAPS[15] & 1`), the end-to-end sequence-number ub2
/// (advertised via `SERVER_COMPILE_CAPS[16] & 0x10` — ojdbc reads it
/// unconditionally, ODP.NET gates on the bit), then the fixed error-attribute
/// block and the negotiated-TTC-version-12 trailer pair. `0x09` STATUS must NOT
/// follow: both clients' receive loops exit on the `0x04` itself. Verified
/// byte-for-byte against a live Oracle XE wire capture.
#[allow(clippy::too_many_arguments)]
fn write_end_of_call_jdbc(
    buf: &mut WriteBuffer,
    error_code: u32,
    message: Option<&str>,
    row_count: u64,
    has_more: bool,
    req_seq: u8,
    cursor_id: u16,
    error_pos: u16,
) {
    buf.write_u8(0x04); // TTC message type (consumed by the receive switch)
    buf.write_ub4(0); // end-of-call status
    // error-attribute block
    buf.write_ub2(0); // end-to-end sequence number
    // the current-row-number field doubles as the DML affected-row count for
    // ojdbc thin (it reads `rowsProcessed` from here).
    buf.write_ub4(row_count as u32); // current row number
    buf.write_ub2(error_code as u16); // return code (short error number)
    buf.write_ub2(0); // array element with error
    buf.write_ub2(0); // array element errno
    // Report the cursor id while rows remain; report 0 once the result is
    // exhausted so ojdbc treats the server cursor as closed.
    buf.write_ub2(if has_more { cursor_id.max(1) } else { 0 }); // current cursor id
    buf.write_sb2(error_pos as i16); // error position
    buf.write_u8(0); // sql type (ub1 — raw)
    buf.write_u8(0); // fatal flag (sb1 compact; 0 == raw 0x00)
    // flags: this is a *compact* sb1 read, so a bare 0x20 byte would be parsed
    // as "read 32 bytes" and overrun. Encode it compactly.
    buf.write_ub2(if has_more { 0x20 } else { 0 }); // flags (sb1, compact)
    buf.write_u8(0); // user cursor option (sb1 compact; 0 == raw 0x00)
    buf.write_u8(0); // upi param (ub1 — raw)
    buf.write_u8(0); // warning flag (ub1)
    buf.write_ub4(0); // rba
    buf.write_ub2(0); // partition id
    buf.write_u8(0); // table id (ub1)
    buf.write_ub4(0); // block number
    buf.write_ub2(0); // slot number
    buf.write_ub4(0); // os error (SWORD == ub4)
    buf.write_u8(0); // statement number (ub1)
    buf.write_u8(req_seq); // call number (ub1) — must echo the request's TTC seq
    buf.write_ub2(0); // pad1
    buf.write_ub4(0); // success iters
    buf.write_ub4(0); // DALC: error id (len 0)
    buf.write_ub4(0); // DALC: batch error codes (len 0)
    buf.write_ub4(0); // DALC: batch error offsets (len 0)
    buf.write_ub4(0); // batch error message count (0 -> no key/value block)
    // negotiated-TTC-version-12 trailer
    buf.write_ub4(error_code); // secondary error code
    buf.write_ub8(row_count); // secondary row count (SB8)
    // message present only when the secondary error code != 0, read as a CLR.
    if error_code != 0 {
        buf.write_string_with_length(message);
    }
}

/// Minimal RowHeader preceding each RowData in a fetch continuation. Matches
/// the fields skipped by oracle-rs's `parse_row_header` / `parse_fetch_response`.
fn write_row_header(buf: &mut WriteBuffer) {
    buf.write_u8(0x06); // RowHeader
    buf.write_ub1(0); // flags
    buf.write_ub2(0); // num requests
    buf.write_ub4(0); // iteration number
    buf.write_ub4(0); // num iters
    buf.write_ub2(0); // buffer length
    buf.write_ub4(0); // bit vector length
    buf.write_ub4(0); // rxhrid length
}

/// Describe metadata strings carry an outer UB4 presence/length indicator,
/// followed by the normal TTC length-prefixed bytes.
fn write_string_with_ub4_length(buf: &mut WriteBuffer, value: Option<&str>) {
    match value {
        Some(value) => {
            buf.write_ub4(value.len() as u32);
            buf.write_string_with_length(Some(value));
        }
        None => buf.write_ub4(0),
    }
}

#[derive(Debug, Clone)]
pub struct ColumnMeta {
    pub name: String,
    pub oracle_type: u8,
    pub flags: u8,
    pub precision: i8,
    pub scale: i8,
    pub buffer_size: u32,
    pub max_size: u32,
    pub charset_id: u16,
    pub charset_form: u8,
    pub nullable: bool,
    pub schema: Option<String>,
    pub type_name: Option<String>,
    pub position: u16,
}

impl ColumnMeta {
    pub fn number(name: impl Into<String>, precision: i8, scale: i8) -> Self {
        Self {
            name: name.into(),
            oracle_type: 2,
            flags: 0,
            precision,
            scale,
            buffer_size: 22,
            max_size: 22,
            charset_id: 0,
            charset_form: 0,
            nullable: true,
            schema: None,
            type_name: None,
            position: 1,
        }
    }

    /// Oracle `RAW` (type 23) — binary payload the client renders as `0x…`.
    pub fn raw(name: impl Into<String>, size: u32) -> Self {
        Self {
            name: name.into(),
            oracle_type: 23,
            flags: 0,
            precision: 0,
            scale: 0,
            buffer_size: size,
            max_size: size,
            charset_id: 0,
            charset_form: 0,
            nullable: true,
            schema: None,
            type_name: None,
            position: 1,
        }
    }

    pub fn varchar(name: impl Into<String>, size: u32) -> Self {
        Self {
            name: name.into(),
            oracle_type: 1,
            flags: 0,
            precision: 0,
            scale: 0,
            buffer_size: size,
            max_size: size,
            charset_id: CHARSET_UTF8,
            charset_form: 1,
            nullable: true,
            schema: None,
            type_name: None,
            position: 1,
        }
    }

    /// Oracle DATE (7-byte date/time form, with second precision).
    pub fn date(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            oracle_type: 12,
            flags: 0,
            precision: 0,
            scale: 0,
            buffer_size: 7,
            max_size: 7,
            charset_id: 0,
            charset_form: 0,
            nullable: true,
            schema: None,
            type_name: None,
            position: 1,
        }
    }

    /// Oracle TIMESTAMP (internal type 180) — the 7-byte DATE form plus a
    /// 4-byte big-endian fractional-second nanoseconds field. `scale` carries
    /// the fractional-second precision (some client value decoders desync when
    /// it is left 0).
    pub fn timestamp(name: impl Into<String>, scale: i8) -> Self {
        Self {
            name: name.into(),
            oracle_type: 180,
            flags: 0,
            precision: 0,
            scale,
            buffer_size: 11,
            max_size: 11,
            charset_id: 0,
            charset_form: 0,
            nullable: true,
            schema: None,
            type_name: None,
            position: 1,
        }
    }
}

pub fn encode_oracle_number_i64(value: i64) -> Vec<u8> {
    if value == 0 {
        return vec![0x80];
    }

    let mut decimal = value.unsigned_abs().to_string();
    if !decimal.len().is_multiple_of(2) {
        decimal.insert(0, '0');
    }
    let digits: Vec<u8> = decimal
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|[hi, lo]| (hi - b'0') * 10 + (lo - b'0'))
        .collect();

    if value > 0 {
        let mut encoded = Vec::with_capacity(digits.len() + 1);
        encoded.push(0xC0 + digits.len() as u8);
        encoded.extend(digits.into_iter().map(|digit| digit + 1));
        encoded
    } else {
        let mut encoded = Vec::with_capacity(digits.len() + 2);
        encoded.push(0x3F - digits.len() as u8);
        encoded.extend(digits.into_iter().map(|digit| 101 - digit));
        encoded.push(102);
        encoded
    }
}

/// Encode a finite decimal representation into Oracle's base-100 NUMBER form.
pub fn encode_oracle_number_decimal(value: &str) -> Result<Vec<u8>> {
    let value = value.trim();
    if value.is_empty() {
        return Err(Error::DataConversionError("empty NUMBER value".into()));
    }
    let bytes = value.as_bytes();
    let mut pos = 0usize;
    let negative = bytes.first() == Some(&b'-');
    if negative || bytes.first() == Some(&b'+') {
        pos += 1;
    }
    let mut digits = Vec::new();
    while pos < bytes.len() && bytes[pos].is_ascii_digit() {
        let digit = bytes[pos] - b'0';
        if digit != 0 || !digits.is_empty() {
            digits.push(digit);
        }
        pos += 1;
    }
    let mut decimal_point = digits.len() as i32;
    if bytes.get(pos) == Some(&b'.') {
        pos += 1;
        while pos < bytes.len() && bytes[pos].is_ascii_digit() {
            let digit = bytes[pos] - b'0';
            if digit == 0 && digits.is_empty() {
                decimal_point -= 1;
            } else {
                digits.push(digit);
            }
            pos += 1;
        }
    }
    if matches!(bytes.get(pos), Some(b'e' | b'E')) {
        pos += 1;
        let exponent_negative = bytes.get(pos) == Some(&b'-');
        if exponent_negative || bytes.get(pos) == Some(&b'+') {
            pos += 1;
        }
        let exponent_start = pos;
        while pos < bytes.len() && bytes[pos].is_ascii_digit() {
            pos += 1;
        }
        let exponent = value[exponent_start..pos]
            .parse::<i32>()
            .map_err(|_| Error::DataConversionError("invalid NUMBER exponent".into()))?;
        decimal_point += if exponent_negative {
            -exponent
        } else {
            exponent
        };
    }
    if pos != bytes.len() {
        return Err(Error::DataConversionError("invalid NUMBER value".into()));
    }
    while digits.last() == Some(&0) {
        digits.pop();
    }
    if digits.is_empty() {
        return Ok(vec![0x80]);
    }
    if digits.len() > 40 || !(-129..=126).contains(&decimal_point) {
        return Err(Error::DataConversionError(
            "NUMBER out of Oracle range".into(),
        ));
    }
    let prepend_zero = decimal_point % 2 != 0;
    if prepend_zero {
        digits.push(0);
        decimal_point += 1;
    }
    if digits.len() % 2 != 0 {
        digits.push(0);
    }
    let exponent = ((decimal_point / 2) + 192) as i8;
    let mut encoded = Vec::with_capacity(digits.len() / 2 + 2);
    encoded.push(if negative {
        !exponent as u8
    } else {
        exponent as u8
    });
    let mut position = 0usize;
    for pair_index in 0..digits.len() / 2 {
        let pair = if pair_index == 0 && prepend_zero {
            let value = digits[position];
            position += 1;
            value
        } else {
            let value = digits[position] * 10 + digits[position + 1];
            position += 2;
            value
        };
        encoded.push(if negative { 101 - pair } else { pair + 1 });
    }
    if negative && encoded.len() < 21 {
        encoded.push(102);
    }
    Ok(encoded)
}

#[cfg(test)]
mod tests {
    use super::{
        BindValue, bind_postgres_parameters, decode_bind_value, decode_oracle_number,
        encode_oracle_number_decimal, encode_oracle_number_i64, parse_auth_phase_one_request,
        parse_execute_request, parse_reexecute_request, substitute_bind_values,
    };

    /// Malformed / truncated / adversarial packet bodies must come back as an
    /// `Err`, never a panic, an unbounded allocation, or a hang. Exercises the
    /// request parsers with a spread of hostile shapes.
    #[test]
    fn request_parsers_reject_malformed_input_without_panicking() {
        let mut seeds: Vec<Vec<u8>> = vec![
            vec![],
            vec![0x00],
            vec![0xff; 3],
            vec![0x00, 0x00, 0x03, 0x5e],
            // Execute preamble that then claims a huge SQL / bind length.
            vec![
                0x00, 0x00, 0x03, 0x5e, 0x01, 0x04, 0x7f, 0xff, 0xff, 0xff, 0x01, 0x04, 0xff, 0xff,
                0xff, 0xff,
            ],
            // Auth phase-one header followed by a bogus key/value count.
            vec![
                0x00, 0x00, 0x03, 0x76, 0x01, 0x04, 0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00,
            ],
            // LONG_INDICATOR (0xfe) bytes-with-length that never terminates.
            vec![
                0x00, 0x00, 0x03, 0x5e, 0x01, 0x01, 0x01, 0x01, 0xfe, 0x04, 0xff, 0xff, 0xff, 0xff,
            ],
        ];
        // A handful of longer pseudo-random bodies (deterministic LCG).
        let mut state: u32 = 0x1234_5678;
        for _ in 0..64 {
            let len = {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state as usize % 512) + 4
            };
            let body: Vec<u8> = (0..len)
                .map(|_| {
                    state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (state >> 24) as u8
                })
                .collect();
            seeds.push(body);
        }

        for body in &seeds {
            let mut framed = body.clone();
            if framed.len() >= 4 {
                framed[2] = 0x03; // msg type = FUNCTION
                framed[3] = 0x5e; // Execute
            }
            let _ = parse_execute_request(&framed);
            let _ = parse_reexecute_request(&framed, "SELECT 1", &[2, 2, 2]);
            let _ = parse_auth_phase_one_request(body);
        }
    }

    #[test]
    fn encodes_oracle_number_integers() {
        assert_eq!(encode_oracle_number_i64(0), [0x80]);
        assert_eq!(encode_oracle_number_i64(1), [0xC1, 0x02]);
        assert_eq!(encode_oracle_number_i64(123), [0xC2, 0x02, 0x18]);
        assert_eq!(encode_oracle_number_i64(-1), [0x3E, 100, 102]);
    }

    #[test]
    fn decodes_oracle_number_values() {
        assert_eq!(decode_oracle_number(&[0x80]).unwrap(), "0");
        assert_eq!(decode_oracle_number(&[0xC1, 0x02]).unwrap(), "1");
        assert_eq!(decode_oracle_number(&[0xC2, 0x02, 0x18]).unwrap(), "123");
        assert_eq!(decode_oracle_number(&[0x3E, 100, 102]).unwrap(), "-1");
    }

    #[test]
    fn round_trips_decimal_oracle_numbers() {
        for expected in ["1.25", "0.5", "-123.75", "1e3", "-0.01"] {
            let encoded = encode_oracle_number_decimal(expected).unwrap();
            let actual = decode_oracle_number(&encoded).unwrap();
            let normalized = match expected {
                "1e3" => "1000",
                other => other,
            };
            assert_eq!(actual, normalized, "{expected}");
        }
    }

    #[test]
    fn substitutes_positional_and_named_binds_safely() {
        let sql = substitute_bind_values(
            "SELECT :1, :name, :name, ':1' FROM dual",
            &[
                BindValue::Number("42".into()),
                BindValue::String("O'Reilly".into()),
            ],
        )
        .unwrap();
        assert_eq!(sql, "SELECT 42, 'O''Reilly', 'O''Reilly', ':1' FROM dual");
        assert!(substitute_bind_values("SELECT :2", &[BindValue::Null]).is_err());
    }

    #[test]
    fn converts_binds_to_postgres_parameters_without_interpolation() {
        let bound = bind_postgres_parameters(
            "SELECT :p, :p, ':p', :2 -- :3",
            &[
                BindValue::String("O'Reilly".into()),
                BindValue::Number("42".into()),
            ],
        )
        .unwrap();
        assert_eq!(
            bound.sql,
            "SELECT $1::text, $1::text, ':p', $2::text::numeric -- :3"
        );
        assert_eq!(bound.binds.len(), 2);
        assert_eq!(bound.binds[0], BindValue::String("O'Reilly".into()));
        assert_eq!(bound.binds[1], BindValue::Number("42".into()));
    }

    #[test]
    fn does_not_substitute_binds_in_quoted_or_commented_sql() {
        assert_eq!(
            substitute_bind_values(
                "SELECT ':1', \"quoted:1\", :1 /* :1 */ -- :1\n",
                &[BindValue::String("café".into())],
            )
            .unwrap(),
            "SELECT ':1', \"quoted:1\", 'café' /* :1 */ -- :1\n"
        );
    }

    #[test]
    fn decodes_oracle_date_and_timestamp_binds() {
        assert_eq!(
            decode_bind_value(12, Some(vec![120, 124, 2, 29, 14, 15, 16])).unwrap(),
            BindValue::Temporal("TIMESTAMP '2024-02-29 13:14:15'".into())
        );
        assert_eq!(
            decode_bind_value(180, Some(vec![120, 124, 2, 29, 14, 15, 16, 7, 91, 202, 0]),)
                .unwrap(),
            BindValue::Temporal("TIMESTAMP '2024-02-29 13:14:15.123456'".into())
        );
    }

    #[test]
    fn oci_execute_parses_raw_bind_select() {
        let payload = hex_to_vec(
            "0000035e066980000000000000feffffffffffffff13000000feffffffffffffff0d000000fefffffffffffffffeffffffffffffff000000000200000000000000feffffffffffffff010000000000000000000000feffffffffffffff0000000000000000fefffffffffffffffefffffffffffffffeffffffffffffff0000000000000000fefffffffffffffffeffffffffffffff000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000feffffffffffffff0d000000feffffffffffffff0000000000000000000000001353454c454354203a312046524f4d204455414c01000000000000000000000000000000000000000000000000000000010000000000000000800000000000000000000000000000170700000400000000000000011000000000000000000000000000000000000704000102ff",
        );
        let req = super::parse_execute_request_oci(&payload).expect("parse");
        assert_eq!(req.sql, "SELECT :1 FROM DUAL");
        assert_eq!(req.binds.len(), 1);
    }

    fn hex_to_vec(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// The OCI (thick) execute response must be byte-for-byte what a live
    /// Oracle XE 21c sends, modulo the two non-deterministic regions: the
    /// 23-byte `skip_bytes` describe id/hash and the 13-byte literal ROWID.
    #[test]
    fn oci_query_response_matches_live_oracle_capture() {
        use super::{ColumnMeta, build_query_response_oci};

        fn mask(mut b: Vec<u8>) -> Vec<u8> {
            // skip_bytes blob: bytes [4, 27)
            for x in &mut b[4..27] {
                *x = 0;
            }
            // literal ROWID: 13 bytes after the `0d 00 00 00 0d` marker
            if let Some(j) = b.windows(5).position(|w| w == [0x0d, 0, 0, 0, 0x0d]) {
                for x in &mut b[j + 5..j + 18] {
                    *x = 0;
                }
            }
            b
        }

        // SELECT 7 AS a, 'xy' AS b FROM DUAL  -> NUMBER(0,-127) + CHAR(2)
        let cols = vec![
            ColumnMeta {
                name: "A".into(),
                oracle_type: 2,
                flags: 0,
                precision: 0,
                scale: -127,
                buffer_size: 2,
                max_size: 0,
                charset_id: 0,
                charset_form: 0,
                nullable: true,
                schema: None,
                type_name: None,
                position: 0,
            },
            ColumnMeta {
                name: "B".into(),
                oracle_type: 96,
                flags: 0x80,
                precision: 0,
                scale: 0,
                buffer_size: 2,
                max_size: 2,
                charset_id: 873,
                charset_form: 1,
                nullable: true,
                schema: None,
                type_name: None,
                position: 1,
            },
        ];
        let rows = vec![vec![Some(vec![0xc1, 0x08]), Some(b"xy".to_vec())]];
        let got = build_query_response_oci(&cols, &rows, 3, false, 0);

        let want = hex_bytes(
            "000010170000007119d09879b50310fa1cf85ef3f2f86b787e081e03052c0400000002\
             0000005c0200008102000000000000000000000000000000000000000000000000000101\
             01000000014100000000000000000000000000006080000002000000000000000000000000\
             000069030102000000fe3f0000010101000000014200000000000000000100000000000700\
             000007787e081e03053300000000e81f00000200000002000000000000000622020000000000\
             02000000000000000000000000000702c108027879080600ab6d2d0000000000030000000000\
             0000000000000000000000000000000000000d0000000d35787930777a39707a6a77726d0401\
             0000000900010000007b050000000003001e0003000000000000000000000000000000000000\
             00000000000700000100000000000000000000000000000000007b0500000101194f52412d30\
             313430333a206e6f206461746120666f756e640a",
        );
        assert_eq!(mask(got.to_vec()), mask(want), "SELECT 7 describe mismatch");
    }

    fn hex_bytes(s: &str) -> Vec<u8> {
        let clean: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        (0..clean.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&clean[i..i + 2], 16).unwrap())
            .collect()
    }
}
