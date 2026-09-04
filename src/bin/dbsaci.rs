//! DbSaci proxy entry point.
//!
//! Every option can be set with a CLI flag or an environment variable (the flag
//! wins when both are given):
//!
//! | flag | env | default | meaning |
//! | --- | --- | --- | --- |
//! | `--listen`            | `DBSACI_LISTEN`        | `0.0.0.0:1521` | Oracle TNS listen address |
//! | `--db-host`           | `DBSACI_DB_HOST`       | `localhost`   | backend host (PostgreSQL or MariaDB) |
//! | `--db-port`           | `DBSACI_DB_PORT`       | `5432`/`3306` | backend port |
//! | `--db-name`             | `DBSACI_DB_NAME`         | `postgres`    | backend database/schema |
//! | `--db-password`       | `DBSACI_DB_PASSWORD`   | unset         | fallback password for users not in the list (no built-in default) |
//! | `--health-token`      | `DBSACI_HEALTH_TOKEN`  | unset         | required for `/sessions` when health bind is not loopback |
//! | `--tls-cert`          | `DBSACI_TLS_CERT`      | unset         | PEM cert for TCPS on the TNS listener |
//! | `--tls-key`           | `DBSACI_TLS_KEY`       | unset         | PEM key for TCPS |
//! | `--db-ssl`            | `DBSACI_DB_SSL`        | false         | require TLS to the backend |
//! | `--db-user u:p`       | —                     | —             | one `user:password` pair; repeatable |
//! | `--db-users`          | `DBSACI_DB_USERS`     | —             | comma-separated `user:password,user:password` |
//! | `--db-users-file`     | `DBSACI_DB_USERS_FILE`| —             | file of `user:password` lines (`#` comments ok) |
//! | `--statement-timeout-ms` | `DBSACI_STATEMENT_TIMEOUT_MS` | unset | per-statement timeout; ORA-01013 on expiry |
//! | `--idle-timeout-ms`   | `DBSACI_IDLE_TIMEOUT_MS`      | `900000` | idle client reaping; `0` disables |
//! | `--health-addr`       | `DBSACI_HEALTH_ADDR`  | unset          | `host:port` for `/healthz` + `/readyz` |
//! | `--shutdown-grace-ms` | `DBSACI_SHUTDOWN_GRACE_MS`    | `30000` | drain window after SIGINT/SIGTERM |
//! | `--oracle-version`    | `DBSACI_ORACLE_VERSION`      | `19c`   | impersonated Oracle release (`11` or `19`) |
//! | `--identifier-case`   | `DBSACI_IDENTIFIER_CASE`     | `upper` | MariaDB table-identifier folding (`upper`/`lower`); no effect on PostgreSQL |
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

use clap::{Parser, ValueEnum};
use dbsaci::{Config, Credentials, IdentifierCase, OracleVersion, Server};

/// Oracle TNS/TTC proxy in front of PostgreSQL + orafce or MariaDB.
#[derive(Parser, Debug)]
#[command(name = "dbsaci", version, about)]
struct Cli {
    /// Database engine behind the Oracle wire protocol.
    #[arg(long, env = "DBSACI_BACKEND", value_enum, default_value = "postgres")]
    backend: CliBackend,
    /// Oracle TNS listen address.
    #[arg(long, env = "DBSACI_LISTEN")]
    listen: Option<String>,

    /// Backend host (PostgreSQL or MariaDB).
    #[arg(long, env = "DBSACI_DB_HOST")]
    db_host: Option<String>,

    /// Backend port.
    #[arg(long, env = "DBSACI_DB_PORT")]
    db_port: Option<u16>,

    /// Backend database (PostgreSQL) or default schema (MariaDB).
    #[arg(long, env = "DBSACI_DB_NAME")]
    db_name: Option<String>,

    /// Fallback password for any user not named in the credential list.
    #[arg(long, env = "DBSACI_DB_PASSWORD")]
    db_password: Option<String>,

    /// One `user:password` pair. Repeatable. Overrides the file and env list.
    #[arg(long = "db-user", value_name = "USER:PASSWORD")]
    db_users: Vec<String>,

    /// Comma-separated `user:password,user:password` list.
    #[arg(long = "db-users", env = "DBSACI_DB_USERS", value_name = "LIST")]
    db_users_list: Option<String>,

    /// File of `user:password` lines (`#` comments and blank lines ignored).
    #[arg(long, env = "DBSACI_DB_USERS_FILE", value_name = "PATH")]
    db_users_file: Option<PathBuf>,

    /// Per-statement timeout in milliseconds; ORA-01013 on expiry.
    #[arg(long, env = "DBSACI_STATEMENT_TIMEOUT_MS")]
    statement_timeout_ms: Option<u64>,

    /// Idle client reaping in milliseconds; `0` disables.
    #[arg(long, env = "DBSACI_IDLE_TIMEOUT_MS")]
    idle_timeout_ms: Option<u64>,

    /// `host:port` for the dependency-free `/healthz` + `/readyz` endpoints.
    #[arg(long, env = "DBSACI_HEALTH_ADDR")]
    health_addr: Option<String>,

    /// Drain window in milliseconds after SIGINT/SIGTERM.
    #[arg(long, env = "DBSACI_SHUTDOWN_GRACE_MS")]
    shutdown_grace_ms: Option<u64>,

    /// Impersonated Oracle release: `11`/`11g`/`11.2` or `19`/`19c`.
    #[arg(long, env = "DBSACI_ORACLE_VERSION")]
    oracle_version: Option<String>,

    /// How MariaDB table identifiers are folded — `upper` (Oracle's own
    /// unquoted-identifier behaviour; matches a vendored `data.sql`-style
    /// schema) or `lower` (PostgreSQL/MariaDB convention). No effect on the
    /// PostgreSQL backend, which folds unquoted identifiers itself.
    #[arg(long, env = "DBSACI_IDENTIFIER_CASE", value_enum)]
    identifier_case: Option<CliIdentifierCase>,

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

    /// Require TLS when connecting to the backend database.
    #[arg(long, env = "DBSACI_DB_SSL", default_value_t = false)]
    db_ssl: bool,
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

#[derive(Clone, Copy, Debug, ValueEnum)]
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

fn build_credentials(cli: &Cli) -> Result<Credentials, String> {
    let mut creds = Credentials::default();
    if let Some(path) = &cli.db_users_file {
        creds.extend_file(path)?;
    }
    if let Some(list) = &cli.db_users_list {
        creds.extend_comma_list(list)?;
    }
    creds.extend_pairs(cli.db_users.iter().map(String::as_str))?;
    // No built-in shared password. Unknown users are ORA-01017 unless
    // `--db-password` / `DBSACI_DB_PASSWORD` is set explicitly.
    creds.set_fallback(cli.db_password.clone());
    Ok(creds)
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
    let default = Config::default();

    let credentials = match build_credentials(&cli) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("invalid credential configuration: {e}");
            std::process::exit(2);
        }
    };

    let config = Config {
        backend: match cli.backend {
            CliBackend::Postgres => dbsaci::BackendKind::Postgres,
            CliBackend::Mariadb => dbsaci::BackendKind::MariaDb,
        },
        listen_addr: cli.listen.unwrap_or(default.listen_addr),
        db_host: cli.db_host.unwrap_or(default.db_host),
        db_port: cli.db_port.unwrap_or(default.db_port),
        db_name: cli.db_name.unwrap_or(default.db_name),
        identifier_case: cli
            .identifier_case
            .map(IdentifierCase::from)
            .unwrap_or(default.identifier_case),
        credentials,
        statement_timeout: cli.statement_timeout_ms.map(Duration::from_millis),
        idle_timeout: match cli.idle_timeout_ms {
            Some(0) => None,
            Some(v) => Some(Duration::from_millis(v)),
            None => default.idle_timeout,
        },
        health_addr: cli.health_addr,
        shutdown_grace: cli
            .shutdown_grace_ms
            .map(Duration::from_millis)
            .unwrap_or(default.shutdown_grace),
        oracle_version: parse_oracle_version(cli.oracle_version.as_deref()),
        health_token: cli.health_token,
        tls_cert: cli.tls_cert,
        tls_key: cli.tls_key,
        db_ssl: cli.db_ssl,
    };

    if let Err(e) = Server::new(config).run().await {
        eprintln!("dbsaci exited with error: {e}");
        std::process::exit(1);
    }
}
