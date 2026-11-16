//! DbSaci entry point.
//!
//! ```text
//! dbsaci [serve]   run the proxy (default)
//! dbsaci check     verify a login can use the backend through DbSaci (MariaDB)
//! dbsaci install   create / upgrade the data dictionary in `dbsaci_sys` (MariaDB)
//! dbsaci migrate   translate (and optionally apply) an Oracle SQL script (MariaDB)
//! ```
//!
//! Every option can be set with a CLI flag or an environment variable (the flag
//! wins when both are given):
//!
//! | flag | env | default | meaning |
//! | --- | --- | --- | --- |
//! | `--backend`           | `DBSACI_BACKEND`       | `postgres`    | `postgres` or `mariadb` |
//! | `--listen`            | `DBSACI_LISTEN`        | `0.0.0.0:1521` | Oracle TNS listen address |
//! | `--db-host`           | `DBSACI_DB_HOST`       | `localhost`   | backend host |
//! | `--db-port`           | `DBSACI_DB_PORT`       | `5432` / `3306` | backend port (per backend) |
//! | `--db-name`           | `DBSACI_DB_NAME`       | `postgres` / none | PostgreSQL database; MariaDB fallback schema for logins without their own |
//! | `--db-password`       | `DBSACI_DB_PASSWORD`   | unset         | fallback password for users not in the list (no built-in default) |
//! | `--db-user u:p`       | —                      | —             | one `user:password` pair; repeatable |
//! | `--db-users`          | `DBSACI_DB_USERS`      | —             | comma-separated `user:password,user:password` |
//! | `--db-users-file`     | `DBSACI_DB_USERS_FILE` | —             | file of `user:password` lines (`#` comments ok) |
//! | `--identifier-case`   | `DBSACI_IDENTIFIER_CASE` | `upper`     | MariaDB: physical case of Oracle names (`upper` = as in Oracle) |
//! | `--statement-timeout-ms` | `DBSACI_STATEMENT_TIMEOUT_MS` | unset | per-call timeout; ORA-01013 on expiry |
//! | `--idle-timeout-ms`   | `DBSACI_IDLE_TIMEOUT_MS` | `900000`    | idle client reaping; `0` disables |
//! | `--max-sessions`      | `DBSACI_MAX_SESSIONS`  | unset         | refuse clients beyond this many concurrent sessions |
//! | `--health-addr`       | `DBSACI_HEALTH_ADDR`   | unset         | `host:port` for `/healthz` + `/readyz` + `/metrics` |
//! | `--health-token`      | `DBSACI_HEALTH_TOKEN`  | unset         | required for `/sessions` when health bind is not loopback |
//! | `--shutdown-grace-ms` | `DBSACI_SHUTDOWN_GRACE_MS` | `30000`   | drain window after SIGINT/SIGTERM |
//! | `--oracle-version`    | `DBSACI_ORACLE_VERSION` | `19c`        | impersonated Oracle release (`11` or `19`) |
//! | `--tls-cert` / `--tls-key` | `DBSACI_TLS_CERT` / `DBSACI_TLS_KEY` | unset | TCPS on the TNS listener |
//! | `--db-ssl`            | `DBSACI_DB_SSL`        | false         | require TLS to the backend |
//!
//! ## Credential model
//!
//! An Oracle client authenticates with a challenge/response, so DbSaci must
//! already hold each user's backend password. Declare them ahead of time and
//! an Oracle client then logs in with the *same* user/password it would use
//! against the backend directly. Sources are layered (later overrides earlier):
//! `--db-users-file` / `DBSACI_DB_USERS_FILE`, then `DBSACI_DB_USERS`, then each
//! `--db-user` flag. A user not in the list falls back to `--db-password` if set,
//! and is rejected with ORA-01017 otherwise.

use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};
use dbsaci::backend::OracleBackend;
use dbsaci::{BackendKind, Config, Credentials, IdentifierCase, OracleVersion, Server};

/// Oracle TNS/TTC proxy in front of PostgreSQL + orafce or MariaDB.
#[derive(Parser, Debug)]
#[command(name = "dbsaci", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    common: Common,
    #[command(flatten)]
    serve: ServeArgs,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the proxy (the default).
    Serve,
    /// Check that a login can use the backend through DbSaci (MariaDB):
    /// server version, case sensitivity, the login's schema, the dictionary.
    Check {
        /// Oracle user to check (its password comes from the credential list).
        #[arg(long)]
        user: String,
    },
    /// Create or upgrade the DbSaci data dictionary (`dbsaci_sys`) on MariaDB
    /// and grant it to every account. Needs an administrative account.
    Install {
        #[arg(long, env = "DBSACI_ADMIN_USER", default_value = "root")]
        admin_user: String,
        #[arg(long, env = "DBSACI_ADMIN_PASSWORD")]
        admin_password: String,
        /// Also drop the per-schema dictionary views and functions that
        /// DbSaci 0.2.x created in every user schema.
        #[arg(long)]
        drop_legacy: bool,
    },
    /// Translate an Oracle SQL script (SQL*Plus style) for MariaDB. Prints
    /// the translation and a report; with `--apply`, runs it as `--user`.
    Migrate {
        /// The Oracle script.
        file: PathBuf,
        /// Execute the statements (otherwise only print the translation).
        #[arg(long)]
        apply: bool,
        /// Oracle user to run as with `--apply`.
        #[arg(long)]
        user: Option<String>,
        /// Stop at the first failing statement (otherwise report and go on).
        #[arg(long)]
        stop_on_error: bool,
    },
}

#[derive(Args, Debug)]
struct Common {
    /// Database engine behind the Oracle wire protocol.
    #[arg(
        long,
        env = "DBSACI_BACKEND",
        value_enum,
        default_value = "postgres",
        global = true
    )]
    backend: CliBackend,

    /// Backend host.
    #[arg(long, env = "DBSACI_DB_HOST", global = true)]
    db_host: Option<String>,

    /// Backend port (default 5432 for PostgreSQL, 3306 for MariaDB).
    #[arg(long, env = "DBSACI_DB_PORT", global = true)]
    db_port: Option<u16>,

    /// PostgreSQL database; for MariaDB, the schema used by logins that have
    /// no schema of their own.
    #[arg(long, env = "DBSACI_DB_NAME", global = true)]
    db_name: Option<String>,

    /// Fallback password for any user not named in the credential list.
    #[arg(long, env = "DBSACI_DB_PASSWORD", global = true)]
    db_password: Option<String>,

    /// One `user:password` pair. Repeatable. Overrides the file and env list.
    #[arg(long = "db-user", value_name = "USER:PASSWORD", global = true)]
    db_users: Vec<String>,

    /// Comma-separated `user:password,user:password` list.
    #[arg(
        long = "db-users",
        env = "DBSACI_DB_USERS",
        value_name = "LIST",
        global = true
    )]
    db_users_list: Option<String>,

    /// File of `user:password` lines (`#` comments and blank lines ignored).
    #[arg(long, env = "DBSACI_DB_USERS_FILE", value_name = "PATH", global = true)]
    db_users_file: Option<PathBuf>,

    /// MariaDB: how Oracle names are stored. `upper` keeps Oracle's own
    /// spelling (EMP, "MixedCase"); `lower` stores Oracle-normal names in
    /// lower case for schemas authored in the MariaDB convention. Only matters
    /// when the server's `lower_case_table_names` is 0.
    #[arg(long, env = "DBSACI_IDENTIFIER_CASE", value_enum, global = true)]
    identifier_case: Option<CliIdentifierCase>,

    /// Require TLS when connecting to the backend database.
    #[arg(long, env = "DBSACI_DB_SSL", default_value_t = false, global = true)]
    db_ssl: bool,

    /// MariaDB: PEM CA bundle that signed the server certificate (implies
    /// `--db-ssl`).
    #[arg(long, env = "DBSACI_DB_SSL_CA", value_name = "PATH", global = true)]
    db_ssl_ca: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct ServeArgs {
    /// Oracle TNS listen address.
    #[arg(long, env = "DBSACI_LISTEN")]
    listen: Option<String>,

    /// Per-call timeout in milliseconds; ORA-01013 on expiry.
    #[arg(long, env = "DBSACI_STATEMENT_TIMEOUT_MS")]
    statement_timeout_ms: Option<u64>,

    /// Idle client reaping in milliseconds; `0` disables.
    #[arg(long, env = "DBSACI_IDLE_TIMEOUT_MS")]
    idle_timeout_ms: Option<u64>,

    /// Refuse clients beyond this many concurrent sessions.
    #[arg(long, env = "DBSACI_MAX_SESSIONS")]
    max_sessions: Option<usize>,

    /// `host:port` for the dependency-free `/healthz` + `/readyz` endpoints.
    #[arg(long, env = "DBSACI_HEALTH_ADDR")]
    health_addr: Option<String>,

    /// Drain window in milliseconds after SIGINT/SIGTERM.
    #[arg(long, env = "DBSACI_SHUTDOWN_GRACE_MS")]
    shutdown_grace_ms: Option<u64>,

    /// Impersonated Oracle release: `11`/`11g`/`11.2` or `19`/`19c`.
    #[arg(long, env = "DBSACI_ORACLE_VERSION")]
    oracle_version: Option<String>,

    /// Shared token for GET/DELETE `/sessions`. Required when `--health-addr`
    /// is not loopback.
    #[arg(long, env = "DBSACI_HEALTH_TOKEN")]
    health_token: Option<String>,

    /// PEM certificate for TCPS on the TNS listener.
    #[arg(long, env = "DBSACI_TLS_CERT")]
    tls_cert: Option<PathBuf>,

    /// PEM private key for TCPS on the TNS listener.
    #[arg(long, env = "DBSACI_TLS_KEY")]
    tls_key: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CliIdentifierCase {
    Upper,
    Lower,
}

impl From<CliIdentifierCase> for IdentifierCase {
    fn from(c: CliIdentifierCase) -> Self {
        match c {
            CliIdentifierCase::Upper => IdentifierCase::Upper,
            CliIdentifierCase::Lower => IdentifierCase::Lower,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum, PartialEq)]
enum CliBackend {
    Postgres,
    Mariadb,
}

fn parse_oracle_version(raw: Option<&str>) -> OracleVersion {
    match raw {
        Some("11" | "11g" | "11.2") => OracleVersion::V11g,
        Some("19" | "19c" | "19.0") | None => OracleVersion::V19c,
        Some(other) => {
            eprintln!("unknown oracle version {other:?}; using 19c");
            OracleVersion::V19c
        }
    }
}

fn build_credentials(c: &Common) -> Result<Credentials, String> {
    let mut creds = Credentials::default();
    if let Some(path) = &c.db_users_file {
        creds.extend_file(path)?;
    }
    if let Some(list) = &c.db_users_list {
        creds.extend_comma_list(list)?;
    }
    creds.extend_pairs(c.db_users.iter().map(String::as_str))?;
    // No built-in shared password. Unknown users are ORA-01017 unless
    // `--db-password` / `DBSACI_DB_PASSWORD` is set explicitly.
    creds.set_fallback(c.db_password.clone());
    Ok(creds)
}

fn backend_kind(c: &Common) -> BackendKind {
    match c.backend {
        CliBackend::Postgres => BackendKind::Postgres,
        CliBackend::Mariadb => BackendKind::MariaDb,
    }
}

fn mariadb_config(c: &Common) -> dbsaci::mariadb::MariaDbConfig {
    dbsaci::mariadb::MariaDbConfig {
        host: c.db_host.clone().unwrap_or_else(|| "localhost".into()),
        port: c.db_port.unwrap_or(3306),
        default_db: c.db_name.clone().unwrap_or_default(),
        statement_timeout: None,
        ssl: c.db_ssl || c.db_ssl_ca.is_some(),
        ssl_ca: c.db_ssl_ca.clone(),
        case: c
            .identifier_case
            .map(IdentifierCase::from)
            .unwrap_or_default(),
    }
}

fn require_mariadb(c: &Common, what: &str) {
    if c.backend != CliBackend::Mariadb {
        eprintln!("`dbsaci {what}` is for the MariaDB backend (--backend mariadb)");
        std::process::exit(2);
    }
}

fn password_for(creds: &Credentials, user: &str) -> String {
    match creds.password_for(user) {
        Some(p) => p.to_string(),
        None => {
            eprintln!("no password for {user}: add --db-user {user}:<password> or --db-password");
            std::process::exit(2);
        }
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "dbsaci=info".into()),
        )
        .init();

    let cli = Cli::parse();
    let credentials = match build_credentials(&cli.common) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("invalid credential configuration: {e}");
            std::process::exit(2);
        }
    };

    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => serve(cli.common, cli.serve, credentials).await,
        Command::Check { user } => {
            require_mariadb(&cli.common, "check");
            let password = password_for(&credentials, &user);
            let lines =
                dbsaci::mariadb::check(&mariadb_config(&cli.common), &user, &password).await;
            let mut ok = true;
            for l in &lines {
                println!("{} {}", if l.ok { "ok  " } else { "FAIL" }, l.what);
                ok &= l.ok;
            }
            std::process::exit(if ok { 0 } else { 1 });
        }
        Command::Install {
            admin_user,
            admin_password,
            drop_legacy,
        } => {
            require_mariadb(&cli.common, "install");
            let cfg = mariadb_config(&cli.common);
            match dbsaci::mariadb::install_dictionary(
                &cfg,
                &admin_user,
                &admin_password,
                drop_legacy,
            )
            .await
            {
                Ok(report) => {
                    for line in report {
                        println!("{line}");
                    }
                }
                Err(e) => {
                    eprintln!("install failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        Command::Migrate {
            file,
            apply,
            user,
            stop_on_error,
        } => {
            require_mariadb(&cli.common, "migrate");
            migrate(&cli.common, &credentials, &file, apply, user, stop_on_error).await;
        }
    }
}

async fn serve(common: Common, args: ServeArgs, credentials: Credentials) {
    let default = Config::default();
    let backend = backend_kind(&common);
    let config = Config {
        backend,
        listen_addr: args.listen.unwrap_or(default.listen_addr),
        db_host: common.db_host.clone().unwrap_or(default.db_host),
        db_port: common.db_port.unwrap_or(match backend {
            BackendKind::Postgres => 5432,
            BackendKind::MariaDb => 3306,
        }),
        db_name: common.db_name.clone().unwrap_or_else(|| match backend {
            BackendKind::Postgres => "postgres".into(),
            BackendKind::MariaDb => String::new(),
        }),
        identifier_case: common
            .identifier_case
            .map(IdentifierCase::from)
            .unwrap_or(default.identifier_case),
        credentials: credentials.clone(),
        statement_timeout: args.statement_timeout_ms.map(Duration::from_millis),
        idle_timeout: match args.idle_timeout_ms {
            Some(0) => None,
            Some(v) => Some(Duration::from_millis(v)),
            None => default.idle_timeout,
        },
        health_addr: args.health_addr,
        shutdown_grace: args
            .shutdown_grace_ms
            .map(Duration::from_millis)
            .unwrap_or(default.shutdown_grace),
        oracle_version: parse_oracle_version(args.oracle_version.as_deref()),
        health_token: args.health_token,
        tls_cert: args.tls_cert,
        tls_key: args.tls_key,
        db_ssl: common.db_ssl || common.db_ssl_ca.is_some(),
        db_ssl_ca: common.db_ssl_ca.clone(),
        max_sessions: args.max_sessions,
    };

    // MariaDB: report the backend's readiness at startup (warn-only), with
    // every configured login, so problems show before the first client does.
    if backend == BackendKind::MariaDb {
        let mut cfg = mariadb_config(&common);
        cfg.port = config.db_port;
        let users = credentials.users();
        if users.is_empty() {
            tracing::info!("no named logins configured; skipping the startup check");
        }
        for user in users {
            let Some(pw) = credentials.password_for(&user) else {
                continue;
            };
            for l in dbsaci::mariadb::check(&cfg, &user, pw).await {
                if l.ok {
                    tracing::info!("startup check ({user}): {}", l.what);
                } else {
                    tracing::warn!("startup check ({user}): {}", l.what);
                }
            }
        }
    }

    if let Err(e) = Server::new(config).run().await {
        eprintln!("dbsaci exited with error: {e}");
        std::process::exit(1);
    }
}

async fn migrate(
    common: &Common,
    credentials: &Credentials,
    file: &PathBuf,
    apply: bool,
    user: Option<String>,
    stop_on_error: bool,
) {
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("cannot read {}: {e}", file.display());
            std::process::exit(2);
        }
    };
    let statements = dbsaci::script::split_oracle_script(&text);
    let cfg = mariadb_config(common);
    let backend = if apply {
        let Some(user) = user else {
            eprintln!("--apply needs --user");
            std::process::exit(2);
        };
        let password = password_for(credentials, &user);
        match dbsaci::mariadb::MariaDbBackend::connect(&cfg, &user, &password).await {
            Ok(b) => Some(std::sync::Arc::new(b)),
            Err(e) => {
                eprintln!("cannot log in as {user}: {e}");
                std::process::exit(1);
            }
        }
    } else {
        None
    };
    let (mut ok, mut failed) = (0usize, 0usize);
    for (i, stmt) in statements.iter().enumerate() {
        let n = i + 1;
        let translated = match &backend {
            Some(b) => b.translate(stmt, &[]).await,
            None => dbsaci::translate::oracle_to_mariadb_ctx(
                stmt,
                &dbsaci::translate::MariaDbContext {
                    case: cfg.case,
                    route_dictionary: true,
                    ..Default::default()
                },
            ),
        };
        let translated = match translated {
            Ok(t) => t,
            Err(e) => {
                failed += 1;
                println!(
                    "-- [{n}] NOT TRANSLATED: {e}\n-- {}\n",
                    stmt.replace('\n', "\n-- ")
                );
                if stop_on_error {
                    break;
                }
                continue;
            }
        };
        if let Some(b) = &backend {
            let result = match b.session_statement(stmt).await {
                Some(r) => r,
                None => b.execute_simple(&translated, &[]).await,
            };
            match result {
                Ok(_) => {
                    ok += 1;
                    println!("-- [{n}] ok");
                }
                Err(e) => {
                    failed += 1;
                    println!("-- [{n}] FAILED: {e}\n-- {}", stmt.replace('\n', "\n-- "));
                    if stop_on_error {
                        break;
                    }
                }
            }
        } else {
            ok += 1;
            println!("-- [{n}]\n{translated};\n");
        }
    }
    if let Some(b) = &backend
        && let Err(e) = b.commit().await
    {
        eprintln!("commit failed: {e}");
        failed += 1;
    }
    eprintln!(
        "{} statement(s): {ok} ok, {failed} {}",
        statements.len(),
        if apply { "failed" } else { "not translated" }
    );
    std::process::exit(if failed == 0 { 0 } else { 1 });
}
