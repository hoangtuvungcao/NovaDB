//! PostgreSQL wire protocol v3 implementation for NovaDB.
//!
//! This crate implements the PostgreSQL frontend/backend protocol, allowing
//! NovaDB to accept connections from any PostgreSQL client: `psql`, DBeaver,
//! DataGrip, pgAdmin, and all standard language drivers (node-postgres, psycopg2,
//! pgx, etc.).
//!
//! # Architecture
//!
//! ```text
//! Client (psql)                    NovaDB Wire Server
//! ────────────────                 ───────────────────
//!   StartupMessage  ───────────►   Parse params
//!                   ◄───────────   AuthenticationOk
//!                   ◄───────────   ParameterStatus (multiple)
//!                   ◄───────────   ReadyForQuery
//!
//!   Query("SELECT…") ──────────►   Execute via NovaDb
//!                   ◄───────────   RowDescription
//!                   ◄───────────   DataRow (multiple)
//!                   ◄───────────   CommandComplete
//!                   ◄───────────   ReadyForQuery
//!
//!   Terminate       ──────────►   Close connection
//! ```

mod codec;
mod messages;
mod session;
mod type_map;

pub use session::PgSession;

use std::net::SocketAddr;
use std::sync::Arc;

use novadb_core::NovaDb;
use tokio::net::TcpListener;
use tracing::{error, info};

/// Configuration for the PostgreSQL wire protocol server.
#[derive(Debug, Clone)]
pub struct PgConfig {
    /// Address to listen on (default: 0.0.0.0:5432).
    pub listen_addr: SocketAddr,
    /// Optional username for authentication.
    pub username: Option<String>,
    /// Optional password for authentication.
    pub password: Option<String>,
    /// Database catalog path.
    pub database_path: String,
}

/// Shared server state accessible by all sessions.
pub struct ServerState {
    pub database_path: String,
    pub default_database: NovaDb,
    pub username: Option<String>,
    pub password: Option<String>,
}

/// Start the PostgreSQL wire protocol server.
///
/// This listens for TCP connections and spawns a new task for each client,
/// handling the full PostgreSQL protocol lifecycle.
pub async fn serve_pg(database: NovaDb, config: PgConfig) -> Result<(), std::io::Error> {
    let listener = TcpListener::bind(config.listen_addr).await?;
    info!(listen = %config.listen_addr, "PostgreSQL wire protocol server started");

    let state = Arc::new(ServerState {
        database_path: config.database_path,
        default_database: database,
        username: config.username,
        password: config.password,
    });

    loop {
        let (stream, peer) = listener.accept().await?;
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            info!(%peer, "new PostgreSQL client connection");
            let mut session = PgSession::new(stream, state);
            if let Err(e) = session.run().await {
                error!(%peer, error = %e, "session error");
            }
            info!(%peer, "PostgreSQL client disconnected");
        });
    }
}
