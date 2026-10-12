use std::{env, net::SocketAddr, path::PathBuf, time::Duration};

use clap::Parser;
use novadb_mysql::MySqlConfig;
use novadb_server::ServerConfig;
use novadb_wire::{PgAuthMethod, PgConfig};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "novadbd",
    version,
    about = "NovaDB database server and synchronization relay"
)]
struct Args {
    /// Address on which the HTTP server listens.
    #[arg(long, default_value = "127.0.0.1:8787")]
    listen: SocketAddr,

    /// Address for PostgreSQL wire protocol (enables `psql` and standard drivers).
    #[arg(long)]
    pg_listen: Option<SocketAddr>,

    /// Address for the MySQL wire protocol (enables `mysql` and MySQL drivers), e.g. 127.0.0.1:3306.
    #[arg(long)]
    mysql_listen: Option<SocketAddr>,

    /// `SQLite` file used for the durable relay log.
    #[arg(long, default_value = "novadb-relay.sqlite3")]
    database_path: PathBuf,

    /// Directory containing managed `<database-id>.novadb` database files.
    #[arg(long, default_value = "novadb-data")]
    data_dir: PathBuf,

    /// Bearer token required by all `/v1` routes. Prefer `NOVADB_BEARER_TOKEN`.
    #[arg(long)]
    bearer_token: Option<String>,

    /// Username for PostgreSQL wire protocol authentication.
    #[arg(long, env = "NOVADB_PG_USER")]
    pg_user: Option<String>,

    /// Password for PostgreSQL wire protocol authentication.
    #[arg(long, env = "NOVADB_PG_PASSWORD")]
    pg_password: Option<String>,

    /// PostgreSQL password authentication method: scram-sha-256, md5, or password.
    #[arg(long, env = "NOVADB_PG_AUTH_METHOD", default_value = "scram-sha-256")]
    pg_auth_method: PgAuthMethod,

    /// Maximum number of concurrent PostgreSQL client connections.
    #[arg(long, env = "NOVADB_PG_MAX_CONNECTIONS", default_value_t = novadb_wire::DEFAULT_MAX_CONNECTIONS)]
    pg_max_connections: usize,

    /// Username for MySQL wire protocol authentication.
    #[arg(long, env = "NOVADB_MYSQL_USER")]
    mysql_user: Option<String>,

    /// Password for MySQL wire protocol authentication (`mysql_native_password`).
    #[arg(long, env = "NOVADB_MYSQL_PASSWORD")]
    mysql_password: Option<String>,

    /// Maximum number of concurrent MySQL client connections.
    #[arg(long, env = "NOVADB_MYSQL_MAX_CONNECTIONS", default_value_t = novadb_mysql::DEFAULT_MAX_CONNECTIONS)]
    mysql_max_connections: usize,

    /// Browser origin allowed to call the HTTP API via CORS (repeatable; `*` allows any).
    #[arg(
        long = "cors-origin",
        env = "NOVADB_CORS_ORIGINS",
        value_delimiter = ','
    )]
    cors_origins: Vec<String>,

    /// Seconds an HTTP request may take before the server replies 408.
    #[arg(long, env = "NOVADB_REQUEST_TIMEOUT_SECS", default_value_t = novadb_server::DEFAULT_REQUEST_TIMEOUT_SECS)]
    request_timeout_secs: u64,

    /// Allow listening on non-loopback addresses without a bearer token / PG / MySQL password.
    #[arg(long, env = "NOVADB_ALLOW_INSECURE")]
    allow_insecure: bool,

    #[arg(long, default_value_t = novadb_server::DEFAULT_MAX_PUSH_BATCH_SIZE)]
    max_push_batch_size: usize,

    #[arg(long, default_value_t = novadb_server::DEFAULT_PULL_LIMIT)]
    default_pull_limit: usize,

    #[arg(long, default_value_t = novadb_server::DEFAULT_MAX_PULL_LIMIT)]
    max_pull_limit: usize,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            EnvFilter::new(
                "novadbd=info,novadb_server=info,novadb_wire=info,novadb_mysql=info,tower_http=info",
            )
        }))
        .try_init()
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;

    let args = Args::parse();
    let bearer_token = args
        .bearer_token
        .or_else(|| env::var("NOVADB_BEARER_TOKEN").ok());
    let config = ServerConfig {
        listen_addr: args.listen,
        database_path: args.database_path,
        data_dir: args.data_dir.clone(),
        bearer_token,
        max_push_batch_size: args.max_push_batch_size,
        default_pull_limit: args.default_pull_limit,
        max_pull_limit: args.max_pull_limit,
        cors_origins: args.cors_origins,
        request_timeout: Duration::from_secs(args.request_timeout_secs),
        allow_insecure: args.allow_insecure,
    };
    config.validate()?;

    let pg_config = args.pg_listen.map(|pg_addr| {
        let pg_db_path = args.data_dir.join("__default__.novadb");
        let mut pg_config = PgConfig::new(pg_addr, pg_db_path.to_string_lossy());
        pg_config.username = args.pg_user;
        pg_config.password = args.pg_password;
        pg_config.data_dir = Some(args.data_dir.clone());
        pg_config.auth_method = args.pg_auth_method;
        pg_config.max_connections = args.pg_max_connections;
        pg_config.allow_insecure = args.allow_insecure;
        pg_config
    });
    if let Some(pg_config) = &pg_config {
        pg_config.validate()?;
    }
    let mysql_config = args.mysql_listen.map(|mysql_addr| {
        let mut mysql_config = MySqlConfig::new(mysql_addr, args.data_dir.clone());
        mysql_config.username = args.mysql_user;
        mysql_config.password = args.mysql_password;
        mysql_config.max_connections = args.mysql_max_connections;
        mysql_config.allow_insecure = args.allow_insecure;
        mysql_config
    });
    if let Some(mysql_config) = &mysql_config {
        mysql_config.validate()?;
    }
    if config.allow_insecure {
        tracing::warn!(
            "--allow-insecure is set: unauthenticated non-loopback listeners are permitted"
        );
    }

    tracing::info!(
        listen = %config.listen_addr,
        database = %config.database_path.display(),
        data_dir = %config.data_dir.display(),
        authentication = config.bearer_token.is_some(),
        cors_origins = ?config.cors_origins,
        request_timeout_secs = config.request_timeout.as_secs(),
        pg_listen = ?args.pg_listen,
        "starting NovaDB server"
    );
    if let Some(pg_config) = &pg_config {
        tracing::info!(
            pg_listen = %pg_config.listen_addr,
            pg_database = %pg_config.database_path,
            pg_auth = %pg_config.auth_method,
            pg_authentication = pg_config.password.is_some(),
            pg_max_connections = pg_config.max_connections,
            "PostgreSQL wire protocol enabled"
        );
    }

    if let Some(mysql_config) = &mysql_config {
        tracing::info!(
            mysql_listen = %mysql_config.listen_addr,
            mysql_authentication = mysql_config.password.is_some(),
            mysql_max_connections = mysql_config.max_connections,
            "MySQL wire protocol enabled"
        );
    }

    novadb_server::serve_with_gateways(config, pg_config, mysql_config).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardening_flags_parse() {
        let args = Args::try_parse_from([
            "novadbd",
            "--cors-origin",
            "https://a.example",
            "--cors-origin",
            "https://b.example,https://c.example",
            "--pg-auth-method",
            "md5",
            "--pg-max-connections",
            "7",
            "--mysql-listen",
            "127.0.0.1:3307",
            "--mysql-user",
            "app",
            "--mysql-max-connections",
            "9",
            "--request-timeout-secs",
            "5",
            "--allow-insecure",
        ])
        .unwrap();
        assert_eq!(
            args.cors_origins,
            [
                "https://a.example",
                "https://b.example",
                "https://c.example"
            ]
        );
        assert_eq!(args.pg_auth_method, PgAuthMethod::Md5);
        assert_eq!(args.pg_max_connections, 7);
        assert_eq!(args.mysql_listen, Some("127.0.0.1:3307".parse().unwrap()));
        assert_eq!(args.mysql_user.as_deref(), Some("app"));
        assert_eq!(args.mysql_max_connections, 9);
        assert_eq!(args.request_timeout_secs, 5);
        assert!(args.allow_insecure);
    }

    #[test]
    fn hardening_flag_defaults() {
        let args = Args::try_parse_from(["novadbd"]).unwrap();
        assert!(args.cors_origins.is_empty() || env::var_os("NOVADB_CORS_ORIGINS").is_some());
        assert_eq!(
            args.pg_max_connections,
            novadb_wire::DEFAULT_MAX_CONNECTIONS
        );
        assert_eq!(
            args.request_timeout_secs,
            novadb_server::DEFAULT_REQUEST_TIMEOUT_SECS
        );
        assert!(Args::try_parse_from(["novadbd", "--pg-auth-method", "trust"]).is_err());
    }
}
