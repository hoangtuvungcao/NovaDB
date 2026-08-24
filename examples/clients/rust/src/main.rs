//! NovaDB Rust Client Example
//! Connects to NovaDB using standard tokio-postgres driver over TCP.

use tokio_postgres::{NoTls, Error};

#[tokio::main]
async fn main() -> Result<(), Error> {
    let host = std::env::var("NOVADB_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let port = std::env::var("NOVADB_PORT").unwrap_or_else(|_| "5432".into());
    let user = std::env::var("NOVADB_PG_USER").unwrap_or_else(|_| "admin".into());
    let password = std::env::var("NOVADB_PG_PASSWORD").unwrap_or_else(|_| "secret".into());
    let dbname = std::env::var("NOVADB_DB").unwrap_or_else(|_| "default".into());

    let conn_str = format!("host={host} port={port} user={user} password={password} dbname={dbname}");
    println!("Connecting to NovaDB at {host}:{port}...");

    let (client, connection) = tokio_postgres::connect(&conn_str, NoTls).await?;

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("Connection error: {e}");
        }
    });

    println!("Connected successfully to NovaDB!");

    // 1. Create table
    client
        .batch_execute(
            "CREATE TABLE IF NOT EXISTS audit_events (
                id TEXT PRIMARY KEY,
                event_type TEXT NOT NULL,
                actor TEXT NOT NULL,
                created_at TEXT NOT NULL
            );",
        )
        .await?;
    println!("Table `audit_events` ready.");

    // 2. Insert event with uuid_v7() and now_iso()
    client
        .batch_execute(
            "INSERT INTO audit_events (id, event_type, actor, created_at)
             VALUES (uuid_v7(), 'USER_LOGIN', 'system_admin', now_iso());",
        )
        .await?;
    println!("Inserted audit event with UUID v7.");

    // 3. Query events
    let rows = client
        .query(
            "SELECT id, event_type, actor, created_at FROM audit_events ORDER BY created_at DESC LIMIT 5;",
            &[],
        )
        .await?;

    println!("Query returned {} row(s):", rows.len());
    for row in rows {
        let id: String = row.get(0);
        let event_type: String = row.get(1);
        let actor: String = row.get(2);
        let created_at: String = row.get(3);
        println!("  [{created_at}] ID={id} Event={event_type} Actor={actor}");
    }

    Ok(())
}
