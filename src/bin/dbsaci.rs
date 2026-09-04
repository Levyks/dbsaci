//! DbSaci proxy entry point.
//!
//! Every option can be set with a CLI flag or an environment variable (the flag
//! wins when both are given):
//!
//! | flag | env | default | meaning |
//! | --- | --- | --- | --- |
//! | `--listen`            | `DBSACI_LISTEN`        | `0.0.0.0:1521` | Oracle TNS listen address |
//! | `--pg-host`           | `DBSACI_PG_HOST`       | `localhost`   | PostgreSQL host |
//! | `--pg-port`           | `DBSACI_PG_PORT`       | `5432`        | PostgreSQL port |
//! | `--pg-db`             | `DBSACI_PG_DB`         | `postgres`    | PostgreSQL database |
//! | `--pg-password`       | `DBSACI_PG_PASSWORD`   | `postgres`    | fallback password for users not in the list |
//! | `--pg-user u:p`       | —                     | —             | one `user:password` pair; repeatable |
//! | `--pg-users`          | `DBSACI_PG_USERS`     | —             | comma-separated `user:password,user:password` |
//! | `--pg-users-file`     | `DBSACI_PG_USERS_FILE`| —             | file of `user:password` lines (`#` comments ok) |
//! | `--statement-timeout-ms` | `DBSACI_STATEMENT_TIMEOUT_MS` | unset | per-statement timeout; ORA-01013 on expiry |
//! | `--idle-timeout-ms`   | `DBSACI_IDLE_TIMEOUT_MS`      | `900000` | idle client reaping; `0` disables |
//! | `--health-addr`       | `DBSACI_HEALTH_ADDR`  | unset          | `host:port` for `/healthz` + `/readyz` |
//! | `--shutdown-grace-ms` | `DBSACI_SHUTDOWN_GRACE_MS`    | `30000` | drain window after SIGINT/SIGTERM |
//! | `--oracle-version`    | `DBSACI_ORACLE_VERSION`      | `19c`   | impersonated Oracle release (`11` or `19`) |
//!
//! ## Credential model
//!
//! An Oracle client authenticates with a challenge/response, so DbSaci must
//! already hold each user's PostgreSQL password. Declare them ahead of time and
//! an Oracle client then logs in with the *same* user/password it would use
//! against PostgreSQL directly. Sources are layered (later overrides earlier):
//! `--pg-users-file` / `DBSACI_PG_USERS_FILE`, then `DBSACI_PG_USERS`, then each
//! `--pg-user` flag. A user not in the list falls back to `--pg-password` if set,
//! and is rejected with ORA-01017 otherwise.

use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use dbsaci::{Config, Credentials, OracleVersion, Server};

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

    /// PostgreSQL host.
    #[arg(long, env = "DBSACI_PG_HOST")]
    pg_host: Option<String>,

    /// PostgreSQL port.
    #[arg(long, env = "DBSACI_PG_PORT")]
    pg_port: Option<u16>,

    /// PostgreSQL database.
    #[arg(long, env = "DBSACI_PG_DB")]
    pg_db: Option<String>,

    /// Fallback password for any user not named in the credential list.
    #[arg(long, env = "DBSACI_PG_PASSWORD")]
    pg_password: Option<String>,

    /// One `user:password` pair. Repeatable. Overrides the file and env list.
    #[arg(long = "pg-user", value_name = "USER:PASSWORD")]
    pg_users: Vec<String>,

    /// Comma-separated `user:password,user:password` list.
    #[arg(long = "pg-users", env = "DBSACI_PG_USERS", value_name = "LIST")]
    pg_users_list: Option<String>,

    /// File of `user:password` lines (`#` comments and blank lines ignored).
    #[arg(long, env = "DBSACI_PG_USERS_FILE", value_name = "PATH")]
    pg_users_file: Option<PathBuf>,

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
    if let Some(path) = &cli.pg_users_file {
        creds.extend_file(path)?;
    }
    if let Some(list) = &cli.pg_users_list {
        creds.extend_comma_list(list)?;
    }
    creds.extend_pairs(cli.pg_users.iter().map(String::as_str))?;
    creds.set_fallback(cli.pg_password.clone().or_else(|| Some("postgres".into())));
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
        pg_host: cli.pg_host.unwrap_or(default.pg_host),
        pg_port: cli.pg_port.unwrap_or(default.pg_port),
        pg_db: cli.pg_db.unwrap_or(default.pg_db),
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
    };

    if let Err(e) = Server::new(config).run().await {
        eprintln!("dbsaci exited with error: {e}");
        std::process::exit(1);
    }
}
