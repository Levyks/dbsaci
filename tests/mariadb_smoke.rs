//! End-to-end MariaDB Oracle-mode smoke coverage.

use std::time::Duration;

use dbsaci::{BackendKind, Config, Credentials, Server};
use mysql_async::{Opts, Pool, prelude::Queryable};
use oracle_rs::{Config as OracleConfig, Connection as OracleConnection};
use testcontainers::core::{ContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt};

#[tokio::test]
async fn mariadb_oracle_mode_executes_basic_queries_and_binds() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();
    let container = GenericImage::new("mariadb", "11.4")
        // The official image writes the readiness line through a stream that
        // differs between Docker Desktop and Linux, so use the driver's retry
        // below as the authoritative readiness check.
        .with_wait_for(WaitFor::seconds(8))
        .with_exposed_port(ContainerPort::Tcp(3306))
        .with_env_var("MARIADB_ALLOW_EMPTY_ROOT_PASSWORD", "yes")
        .with_env_var("MARIADB_DATABASE", "postgres")
        .start()
        .await
        .expect("start MariaDB container");
    let host = container
        .get_host()
        .await
        .expect("MariaDB host")
        .to_string();
    let host = if host == "localhost" {
        "127.0.0.1".to_string()
    } else {
        host
    };
    let port = container
        .get_host_port_ipv4(3306)
        .await
        .expect("MariaDB port");

    let admin = Pool::new(
        Opts::from_url(&format!("mysql://root@{host}:{port}/postgres")).expect("MariaDB URL"),
    );
    let mut conn = None;
    for _ in 0..40 {
        match admin.get_conn().await {
            Ok(ready) => {
                conn = Some(ready);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(250)).await,
        }
    }
    let mut conn = conn.expect("MariaDB ready");
    conn.query_drop("CREATE USER IF NOT EXISTS 'corpus'@'%' IDENTIFIED BY 'corpus'")
        .await
        .expect("create corpus user");
    conn.query_drop("GRANT ALL PRIVILEGES ON postgres.* TO 'corpus'@'%'")
        .await
        .expect("grant corpus user");
    drop(conn);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let proxy_port = listener.local_addr().expect("proxy address").port();
    let server = tokio::spawn(
        Server::new(Config {
            backend: BackendKind::MariaDb,
            listen_addr: format!("127.0.0.1:{proxy_port}"),
            pg_host: host,
            pg_port: port,
            pg_db: "postgres".into(),
            credentials: Credentials::with_fallback("corpus"),
            statement_timeout: Some(Duration::from_secs(5)),
            idle_timeout: Some(Duration::from_secs(30)),
            ..Default::default()
        })
        .run_with_listener(listener),
    );

    let oracle = OracleConnection::connect_with_config(OracleConfig::new(
        "127.0.0.1",
        proxy_port,
        "FREEPDB1",
        "corpus",
        "corpus",
    ))
    .await
    .expect("Oracle client connects through MariaDB backend");
    let result = oracle
        .query("SELECT 1 FROM DUAL", &[])
        .await
        .expect("DUAL query");
    assert_eq!(result.rows.len(), 1);

    let result = oracle
        .query("SELECT :1 FROM DUAL", &[oracle_rs::Value::Integer(7)])
        .await
        .expect("bound query");
    assert_eq!(result.rows.len(), 1);

    let result = oracle
        .query("SELECT CAST(1 AS NUMBER) FROM DUAL", &[])
        .await
        .expect("MariaDB NUMBER cast rewrite");
    assert_eq!(result.rows.len(), 1);

    for (sql, label) in [
        ("SELECT NVL(NULL, 7) FROM DUAL", "NVL"),
        ("SELECT DECODE(1, 1, 'yes', 'no') FROM DUAL", "DECODE"),
        ("SELECT COUNT(*) FROM DUAL", "aggregate"),
    ] {
        oracle
            .query(sql, &[])
            .await
            .unwrap_or_else(|e| panic!("{label} query: {e}"));
    }

    oracle
        .execute(
            "CREATE TABLE mariadb_smoke_items (id INT PRIMARY KEY, label VARCHAR(32))",
            &[],
        )
        .await
        .expect("create table");
    oracle
        .execute(
            "INSERT INTO mariadb_smoke_items (id, label) VALUES (:1, :2)",
            &[
                oracle_rs::Value::Integer(9),
                oracle_rs::Value::String("kept".into()),
            ],
        )
        .await
        .expect("insert row");
    let result = oracle
        .query(
            "SELECT label FROM mariadb_smoke_items WHERE id = :1",
            &[oracle_rs::Value::Integer(9)],
        )
        .await
        .expect("query inserted row");
    assert_eq!(result.rows.len(), 1);
    oracle.execute("ROLLBACK", &[]).await.expect("rollback");

    let result = oracle
        .query("SELECT COUNT(*) FROM mariadb_smoke_items", &[])
        .await
        .expect("rolled-back table query");
    assert_eq!(result.rows.len(), 1);

    drop(oracle);
    server.abort();
    let _ = server.await;
    admin.disconnect().await.expect("disconnect MariaDB");
}
