use std::{env, net::SocketAddr, path::PathBuf};

use clap::Parser;
use novadb_core::NovaDb;
use novadb_server::ServerConfig;
use novadb_wire::PgConfig;
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
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("novadb_server=info,novadb_wire=info")),
        )
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
    };
    config.validate()?;

    tracing::info!(
        listen = %config.listen_addr,
        database = %config.database_path.display(),
        data_dir = %config.data_dir.display(),
        authentication = config.bearer_token.is_some(),
        pg_listen = ?args.pg_listen,
        "starting NovaDB server"
    );

    // If PG wire protocol is enabled, start it alongside the HTTP server
    if let Some(pg_addr) = args.pg_listen {
        let pg_db_path = args.data_dir.join("__default__.novadb");
        let pg_database = NovaDb::open(&pg_db_path)?;
        tracing::info!(
            pg_listen = %pg_addr,
            pg_database = %pg_db_path.display(),
            "PostgreSQL wire protocol enabled"
        );

        let pg_config = PgConfig {
            listen_addr: pg_addr,
            username: args.pg_user,
            password: args.pg_password,
            database_path: pg_db_path.to_string_lossy().into_owned(),
        };

        // Run both servers concurrently
        tokio::select! {
            result = novadb_server::serve(config) => {
                result?;
            }
            result = novadb_wire::serve_pg(pg_database, pg_config) => {
                if let Err(e) = result {
                    tracing::error!(error = %e, "PostgreSQL wire protocol server error");
                }
            }
        }
    } else {
        novadb_server::serve(config).await?;
    }

    Ok(())
}
