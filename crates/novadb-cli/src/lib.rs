use std::ffi::OsStr;
use std::fs;
use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use novadb_core::protocol::{ApplyReport, PullResponse, PushRequest, PushResponse};
use novadb_core::{
    IntegrityReport, Migration, MigrationReport, NovaDb, QueryResult, WalCheckpointReport,
};
use reqwest::{Client, RequestBuilder, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const DEFAULT_PULL_LIMIT: usize = 1_000;
const MAX_DATABASE_NAME_LENGTH: usize = 128;
const MAX_PUSH_REQUEST_BYTES: usize = 4 * 1_024 * 1_024;

/// Embedded, local-first SQL database tools.
#[derive(Debug, Parser)]
#[command(name = "novadb", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a `NovaDB` database file.
    Init(InitArgs),
    /// Execute one or more SQL statements.
    Exec(SqlArgs),
    /// Run a read query and print its rows as JSON.
    Query(SqlArgs),
    /// Enable the durable change log used for replication.
    SyncEnable(SyncEnableArgs),
    /// Print locally recorded changes as JSON.
    Changes(ChangesArgs),
    /// Push local changes to a relay server.
    Push(TransferArgs),
    /// Pull and apply changes from a relay server.
    Pull(TransferArgs),
    /// Push local changes, then pull and apply remote changes.
    Sync(SyncArgs),
    /// Create a consistent online backup.
    Backup(BackupArgs),
    /// Run a full database integrity check.
    Integrity(DatabasePathArgs),
    /// Checkpoint and truncate the write-ahead log.
    Checkpoint(DatabasePathArgs),
    /// Apply a versioned SQL migration directory.
    Migrate(MigrateArgs),
    /// Administer databases exposed by a `NovaDB` server.
    Remote(RemoteCommandArgs),
    /// Start the NovaDB HTTP relay and PostgreSQL wire protocol server.
    Serve(ServeArgs),
    /// Open an interactive SQL shell (REPL console) for a database.
    Console(ConsoleArgs),
    /// Import CSV data into a database table.
    Import(ImportArgs),
    /// Export query results to a CSV or JSON file.
    Export(ExportArgs),
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Path to the database file to create.
    #[arg(value_name = "PATH")]
    pub path: PathBuf,
}

#[derive(Debug, Args)]
pub struct SqlArgs {
    /// Path to the local `NovaDB` database.
    #[arg(value_name = "PATH")]
    pub path: PathBuf,

    /// SQL text. When omitted, SQL is read from standard input.
    #[arg(value_name = "SQL", conflicts_with = "file")]
    pub sql: Option<String>,

    /// Read SQL from a UTF-8 file.
    #[arg(short, long, value_name = "FILE", conflicts_with = "sql")]
    pub file: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct SyncEnableArgs {
    /// Path to the local `NovaDB` database.
    #[arg(value_name = "PATH")]
    pub path: PathBuf,

    /// Table whose row mutations should be added to the sync log.
    #[arg(value_name = "TABLE")]
    pub table: String,

    /// Primary-key column used as the stable row identifier.
    #[arg(long, default_value = "id", value_name = "COLUMN")]
    pub primary_key: String,
}

#[derive(Debug, Args)]
pub struct DatabasePathArgs {
    /// Path to the local `NovaDB` database.
    #[arg(value_name = "PATH")]
    pub path: PathBuf,
}

#[derive(Debug, Args)]
pub struct BackupArgs {
    /// Path to the source `NovaDB` database.
    #[arg(value_name = "PATH")]
    pub path: PathBuf,

    /// New file to receive the consistent backup.
    #[arg(value_name = "DEST")]
    pub destination: PathBuf,
}

#[derive(Debug, Args)]
pub struct MigrateArgs {
    /// Path to the local `NovaDB` database.
    #[arg(value_name = "PATH")]
    pub path: PathBuf,

    /// Directory containing `<version>_<name>.sql` files.
    #[arg(value_name = "MIGRATIONS_DIR")]
    pub migrations_dir: PathBuf,
}

#[derive(Debug, Args)]
pub struct ChangesArgs {
    /// Path to the local `NovaDB` database.
    #[arg(value_name = "PATH")]
    pub path: PathBuf,

    /// Return local changes with a sequence greater than this value.
    #[arg(long, default_value_t = 0, allow_hyphen_values = true)]
    pub after: i64,

    /// Maximum number of changes to return.
    #[arg(long, default_value_t = DEFAULT_PULL_LIMIT)]
    pub limit: usize,
}

#[derive(Debug, Args)]
pub struct TransferArgs {
    /// Path to the local `NovaDB` database.
    #[arg(value_name = "PATH")]
    pub path: PathBuf,

    #[command(flatten)]
    pub remote: RemoteArgs,

    /// Cursor to continue from: local sequence for push, remote cursor for pull.
    #[arg(long, default_value_t = 0, allow_hyphen_values = true)]
    pub after: i64,

    /// Maximum changes transferred per request.
    #[arg(long, default_value_t = DEFAULT_PULL_LIMIT)]
    pub limit: usize,
}

#[derive(Debug, Args)]
pub struct SyncArgs {
    /// Path to the local `NovaDB` database.
    #[arg(value_name = "PATH")]
    pub path: PathBuf,

    #[command(flatten)]
    pub remote: RemoteArgs,

    /// Local sequence already pushed to the relay.
    #[arg(long, default_value_t = 0, allow_hyphen_values = true)]
    pub local_after: i64,

    /// Remote cursor already pulled into this replica.
    #[arg(long, default_value_t = 0, allow_hyphen_values = true)]
    pub remote_after: i64,

    /// Maximum changes requested per pull page.
    #[arg(long, default_value_t = DEFAULT_PULL_LIMIT)]
    pub limit: usize,
}

#[derive(Debug, Args)]
pub struct RemoteArgs {
    /// Base URL of the `NovaDB` relay server.
    #[arg(long, env = "NOVADB_REMOTE", value_name = "URL")]
    pub remote: String,

    /// Database name on the relay. Defaults to the local file name.
    #[arg(long, value_name = "NAME")]
    pub database: Option<String>,

    /// Relay bearer token. Can also be supplied through `NOVADB_TOKEN`.
    #[arg(
        long,
        env = "NOVADB_TOKEN",
        hide_env_values = true,
        value_name = "TOKEN"
    )]
    pub token: Option<String>,
}

#[derive(Debug, Args)]
pub struct RemoteCommandArgs {
    #[command(subcommand)]
    pub command: RemoteCommand,
}

#[derive(Debug, Subcommand)]
pub enum RemoteCommand {
    /// List databases managed by the server.
    List(RemoteListArgs),
    /// Create or open a managed database.
    Create(RemoteDatabaseArgs),
    /// Run a read-only SQL statement on a managed database.
    Query(RemoteSqlArgs),
    /// Execute SQL on a managed database.
    Exec(RemoteSqlArgs),
    /// Inspect user-visible tables, indexes, and triggers.
    Schema(RemoteDatabaseArgs),
    /// Run a full integrity check on a managed database.
    Integrity(RemoteDatabaseArgs),
    /// Checkpoint and truncate a managed database's write-ahead log.
    Checkpoint(RemoteDatabaseArgs),
    /// Create a server-side online backup.
    Backup(RemoteDatabaseArgs),
    /// Apply a versioned SQL migration directory on the server.
    Migrate(RemoteMigrateArgs),
}

#[derive(Debug, Args)]
pub struct RemoteConnectionArgs {
    /// Base URL of the `NovaDB` server.
    #[arg(long, env = "NOVADB_REMOTE", value_name = "URL")]
    pub remote: String,

    /// Server bearer token. Can also be supplied through `NOVADB_TOKEN`.
    #[arg(
        long,
        env = "NOVADB_TOKEN",
        hide_env_values = true,
        value_name = "TOKEN"
    )]
    pub token: Option<String>,
}

#[derive(Debug, Args)]
pub struct RemoteListArgs {
    #[command(flatten)]
    pub connection: RemoteConnectionArgs,
}

#[derive(Debug, Args)]
pub struct RemoteDatabaseArgs {
    /// Managed database ID.
    #[arg(value_name = "DATABASE")]
    pub database: String,

    #[command(flatten)]
    pub connection: RemoteConnectionArgs,
}

#[derive(Debug, Args)]
pub struct RemoteSqlArgs {
    /// Managed database ID.
    #[arg(value_name = "DATABASE")]
    pub database: String,

    /// SQL text. When omitted, SQL is read from standard input.
    #[arg(value_name = "SQL", conflicts_with = "file")]
    pub sql: Option<String>,

    /// Read SQL from a UTF-8 file.
    #[arg(short, long, value_name = "FILE", conflicts_with = "sql")]
    pub file: Option<PathBuf>,

    #[command(flatten)]
    pub connection: RemoteConnectionArgs,
}

#[derive(Debug, Args)]
pub struct RemoteMigrateArgs {
    /// Managed database ID.
    #[arg(value_name = "DATABASE")]
    pub database: String,

    /// Directory containing `<version>_<name>.sql` files.
    #[arg(value_name = "MIGRATIONS_DIR")]
    pub migrations_dir: PathBuf,

    #[command(flatten)]
    pub connection: RemoteConnectionArgs,
}

#[derive(Debug, Serialize)]
struct RemoteSqlRequest<'a> {
    sql: &'a str,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct RemoteDatabaseMetadata {
    id: String,
    size_bytes: u64,
    modified_at_ms: Option<u64>,
    open: bool,
    device_id: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct RemoteDatabaseListResponse {
    databases: Vec<RemoteDatabaseMetadata>,
}

#[derive(Debug, Deserialize, Serialize)]
struct RemoteExecuteResponse {
    ok: bool,
}

#[derive(Debug, Deserialize, Serialize)]
struct RemoteSchemaObject {
    name: String,
    table: String,
    sql: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct RemoteSchemaResponse {
    tables: Vec<RemoteSchemaObject>,
    indexes: Vec<RemoteSchemaObject>,
    triggers: Vec<RemoteSchemaObject>,
}

#[derive(Debug, Deserialize, Serialize)]
struct RemoteBackupResponse {
    backup_id: String,
    size_bytes: u64,
}

#[derive(Debug, Serialize)]
struct RemoteMigrationRequest<'a> {
    migrations: Vec<RemoteMigration<'a>>,
}

#[derive(Debug, Serialize)]
struct RemoteMigration<'a> {
    version: i64,
    name: &'a str,
    sql: &'a str,
}

#[derive(Debug)]
struct LoadedMigration {
    version: i64,
    name: String,
    sql: String,
}

pub async fn run() -> Result<()> {
    run_with(Cli::parse()).await
}

pub async fn run_with(cli: Cli) -> Result<()> {
    // `reqwest` is built without its heavyweight default crypto provider. Install
    // the explicitly selected ring provider before any HTTP client is created.
    // Installation is process-global and idempotent; another library may have
    // installed the same (or a caller-selected) provider first.
    let _ = rustls::crypto::ring::default_provider().install_default();

    match cli.command {
        Command::Init(args) => init(&args),
        Command::Exec(args) => execute(&args),
        Command::Query(args) => query(&args),
        Command::SyncEnable(args) => sync_enable(&args),
        Command::Changes(args) => changes(&args),
        Command::Push(args) => push(&args).await,
        Command::Pull(args) => pull(&args).await,
        Command::Sync(args) => sync(&args).await,
        Command::Backup(args) => backup(&args),
        Command::Integrity(args) => integrity(&args),
        Command::Checkpoint(args) => checkpoint(&args),
        Command::Migrate(args) => migrate(&args),
        Command::Remote(args) => remote_command(&args).await,
        Command::Serve(args) => serve(&args).await,
        Command::Console(args) => console(&args),
        Command::Import(args) => import(&args),
        Command::Export(args) => export(&args),
    }
}

#[derive(Debug, Args)]
pub struct ImportArgs {
    /// Path to the local `NovaDB` database file.
    #[arg(value_name = "DATABASE_PATH")]
    pub path: PathBuf,

    /// Path to the CSV file to import.
    #[arg(value_name = "CSV_FILE")]
    pub file: PathBuf,

    /// Target table name to insert data into.
    #[arg(value_name = "TABLE_NAME")]
    pub table: String,

    /// Delimiter character (default: comma ',').
    #[arg(long, default_value = ",")]
    pub delimiter: char,
}

fn import(args: &ImportArgs) -> Result<()> {
    let db = NovaDb::open(&args.path)?;
    let content = std::fs::read_to_string(&args.file)
        .map_err(|e| anyhow::anyhow!("Failed to read file '{}': {e}", args.file.display()))?;

    let ext = args.file.extension().and_then(|e| e.to_str()).unwrap_or("");
    if ext.eq_ignore_ascii_case("sql") {
        db.execute_batch(&content)?;
        println!(
            "[SUCCESS] Executed SQL script '{}' in database '{}'",
            args.file.display(),
            args.path.display()
        );
        return Ok(());
    }

    let mut lines = content.lines().filter(|l| !l.trim().is_empty());
    let header_line = lines
        .next()
        .ok_or_else(|| anyhow::anyhow!("CSV file is empty"))?;

    let headers: Vec<&str> = header_line
        .split(args.delimiter)
        .map(|s| s.trim())
        .collect();
    if headers.is_empty() {
        bail!("No headers found in CSV file");
    }

    let escaped_cols = headers
        .iter()
        .map(|h| format!("\"{}\"", h.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(", ");

    let mut count = 0usize;
    let mut batch_sql = String::new();

    for line in lines {
        let values: Vec<&str> = line.split(args.delimiter).collect();
        if values.len() != headers.len() {
            continue;
        }

        let val_literals: Vec<String> = values
            .iter()
            .map(|v| {
                let trimmed = v.trim().trim_matches('"');
                if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("null") {
                    "NULL".to_string()
                } else if trimmed.parse::<f64>().is_ok() && !trimmed.starts_with('0')
                    || trimmed == "0"
                {
                    trimmed.to_string()
                } else {
                    format!("'{}'", trimmed.replace('\'', "''"))
                }
            })
            .collect();

        batch_sql.push_str(&format!(
            "INSERT INTO \"{}\" ({}) VALUES ({});\n",
            args.table.replace('"', "\"\""),
            escaped_cols,
            val_literals.join(", ")
        ));
        count += 1;
    }

    if !batch_sql.is_empty() {
        db.execute_batch(&batch_sql)?;
    }

    println!(
        "[SUCCESS] Imported {} record(s) into table '{}' from '{}'",
        count,
        args.table,
        args.file.display()
    );
    Ok(())
}

#[derive(Debug, Args)]
pub struct ExportArgs {
    /// Path to the local `NovaDB` database file.
    #[arg(value_name = "DATABASE_PATH")]
    pub path: PathBuf,

    /// SQL query to execute.
    #[arg(value_name = "QUERY")]
    pub query: String,

    /// Target destination file (e.g. results.csv, results.json, or dump.sql).
    #[arg(value_name = "OUTPUT_FILE")]
    pub output: PathBuf,
}

fn export(args: &ExportArgs) -> Result<()> {
    let db = NovaDb::open(&args.path)?;
    let result = db.query(&args.query)?;

    let ext = args
        .output
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");

    if ext.eq_ignore_ascii_case("json") {
        let json_str = serde_json::to_string_pretty(&result.rows)?;
        std::fs::write(&args.output, json_str)?;
    } else if ext.eq_ignore_ascii_case("sql") {
        let mut sql = String::new();
        sql.push_str("-- NovaDB SQL Export Dump\n");
        for row in &result.rows {
            let cols = result
                .columns
                .iter()
                .map(|c| format!("\"{}\"", c.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(", ");
            let vals = result
                .columns
                .iter()
                .map(|c| match row.get(c) {
                    Some(serde_json::Value::Null) | None => "NULL".to_string(),
                    Some(serde_json::Value::Number(n)) => n.to_string(),
                    Some(serde_json::Value::Bool(b)) => {
                        if *b {
                            "1".to_string()
                        } else {
                            "0".to_string()
                        }
                    }
                    Some(serde_json::Value::String(s)) => format!("'{}'", s.replace('\'', "''")),
                    Some(v) => format!("'{}'", v.to_string().replace('\'', "''")),
                })
                .collect::<Vec<_>>()
                .join(", ");
            sql.push_str(&format!(
                "INSERT INTO exported_data ({cols}) VALUES ({vals});\n"
            ));
        }
        std::fs::write(&args.output, sql)?;
    } else {
        // Default to CSV format
        let mut csv = String::new();
        // Header
        csv.push_str(
            &result
                .columns
                .iter()
                .map(|c| format!("\"{}\"", c.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(","),
        );
        csv.push('\n');

        // Rows
        for row in &result.rows {
            let mut line_vals = Vec::new();
            for col in &result.columns {
                let val_str = match row.get(col) {
                    Some(serde_json::Value::Null) | None => "".to_string(),
                    Some(serde_json::Value::String(s)) => format!("\"{}\"", s.replace('"', "\"\"")),
                    Some(v) => v.to_string(),
                };
                line_vals.push(val_str);
            }
            csv.push_str(&line_vals.join(","));
            csv.push('\n');
        }
        std::fs::write(&args.output, csv)?;
    }

    println!(
        "[SUCCESS] Exported {} row(s) to '{}'",
        result.rows.len(),
        args.output.display()
    );
    Ok(())
}

#[derive(Debug, Args)]
pub struct ConsoleArgs {
    /// Path to the local `NovaDB` database file to open.
    #[arg(value_name = "PATH")]
    pub path: PathBuf,
}

fn console(args: &ConsoleArgs) -> Result<()> {
    use std::io::Write;

    let db = if args.path.exists() {
        NovaDb::open(&args.path)?
    } else {
        println!(
            "[INFO] Database file '{}' does not exist. Creating new database...",
            args.path.display()
        );
        NovaDb::open(&args.path)?
    };

    println!("NovaDB Interactive SQL Console");
    println!("Connected to: {}", args.path.display());
    println!("Type .help for instructions, .quit or Ctrl+D to exit.");
    println!();

    let mut timer_enabled = true;
    let mut current_statement = String::new();
    let stdin = std::io::stdin();

    loop {
        if current_statement.is_empty() {
            print!("novadb> ");
        } else {
            print!("   ...> ");
        }
        std::io::stdout().flush()?;

        let mut line = String::new();
        if stdin.read_line(&mut line)? == 0 {
            // EOF
            println!("\nGoodbye.");
            break;
        }

        let trimmed = line.trim();

        // Check for dot commands
        if current_statement.is_empty() && trimmed.starts_with('.') {
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            match parts[0] {
                ".quit" | ".exit" | ".q" => {
                    println!("Goodbye.");
                    break;
                }
                ".help" | ".h" => {
                    println!("Available Dot Commands:");
                    println!("  .tables                List all tables in database");
                    println!("  .schema [table]        Show CREATE statements");
                    println!("  .timer [on|off]        Toggle query execution timer");
                    println!("  .help                  Show this help");
                    println!("  .quit                  Exit the console");
                    println!();
                    println!("SQL Tips:");
                    println!("  - End statements with a semicolon (;) to execute");
                    println!("  - Supports standard SQL, CTEs, Window functions, JSON, UUIDs");
                }
                ".tables" => {
                    match db.query("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' AND name NOT LIKE '_novadb_%' ORDER BY name") {
                        Ok(res) => {
                            if res.rows.is_empty() {
                                println!("(No user tables found)");
                            } else {
                                for r in res.rows {
                                    if let Some(name) = r.get("name").and_then(|v| v.as_str()) {
                                        print!("{:<20} ", name);
                                    }
                                }
                                println!();
                            }
                        }
                        Err(e) => eprintln!("Error: {e}"),
                    }
                }
                ".schema" => {
                    let sql = if parts.len() > 1 {
                        format!("SELECT sql FROM sqlite_master WHERE type IN ('table','view','index') AND name='{}'", parts[1])
                    } else {
                        "SELECT sql FROM sqlite_master WHERE type IN ('table','view','index') AND name NOT LIKE 'sqlite_%' AND sql IS NOT NULL ORDER BY type, name".to_string()
                    };
                    match db.query(&sql) {
                        Ok(res) => {
                            for r in res.rows {
                                if let Some(s) = r.get("sql").and_then(|v| v.as_str()) {
                                    println!("{};\n", s);
                                }
                            }
                        }
                        Err(e) => eprintln!("Error: {e}"),
                    }
                }
                ".timer" => {
                    if parts.len() > 1 {
                        timer_enabled = parts[1].eq_ignore_ascii_case("on") || parts[1] == "1";
                    } else {
                        timer_enabled = !timer_enabled;
                    }
                    println!("Timer is now {}", if timer_enabled { "ON" } else { "OFF" });
                }
                cmd => {
                    eprintln!("Unknown command: {cmd}. Type .help for available commands.");
                }
            }
            continue;
        }

        if trimmed.is_empty() && current_statement.is_empty() {
            continue;
        }

        current_statement.push_str(&line);

        if current_statement.trim_end().ends_with(';') {
            let sql_to_run = current_statement.trim().to_string();
            current_statement.clear();

            let is_query = sql_to_run
                .trim_start()
                .to_ascii_uppercase()
                .starts_with("SELECT")
                || sql_to_run
                    .trim_start()
                    .to_ascii_uppercase()
                    .starts_with("WITH")
                || sql_to_run
                    .trim_start()
                    .to_ascii_uppercase()
                    .starts_with("EXPLAIN")
                || sql_to_run
                    .trim_start()
                    .to_ascii_uppercase()
                    .starts_with("PRAGMA");

            let start = std::time::Instant::now();

            if is_query {
                match db.query(&sql_to_run) {
                    Ok(res) => {
                        let elapsed = start.elapsed();
                        print_ascii_table(&res.columns, &res.rows);
                        if timer_enabled {
                            println!("({} row(s), took {:.2?})", res.rows.len(), elapsed);
                        }
                    }
                    Err(e) => eprintln!("Query Error: {e}"),
                }
            } else {
                match db.execute_batch(&sql_to_run) {
                    Ok(()) => {
                        let elapsed = start.elapsed();
                        if timer_enabled {
                            println!("Statement executed successfully ({:.2?})", elapsed);
                        } else {
                            println!("Statement executed successfully.");
                        }
                    }
                    Err(e) => eprintln!("Execution Error: {e}"),
                }
            }
        }
    }

    Ok(())
}

fn print_ascii_table(columns: &[String], rows: &[serde_json::Value]) {
    if columns.is_empty() {
        println!("(No columns)");
        return;
    }

    // Calculate column widths
    let mut widths: Vec<usize> = columns.iter().map(|c| c.len()).collect();

    for row in rows {
        for (i, col) in columns.iter().enumerate() {
            let val_str = match row.get(col) {
                Some(serde_json::Value::Null) | None => "NULL".to_string(),
                Some(serde_json::Value::String(s)) => s.clone(),
                Some(v) => v.to_string(),
            };
            widths[i] = widths[i].max(val_str.len()).min(50);
        }
    }

    // Print Header
    let mut separator = String::from("+");
    for w in &widths {
        separator.push_str(&format!("{}+", "-".repeat(w + 2)));
    }
    println!("{}", separator);

    let mut header = String::from("|");
    for (i, col) in columns.iter().enumerate() {
        header.push_str(&format!(" {:<width$} |", col, width = widths[i]));
    }
    println!("{}", header);
    println!("{}", separator);

    // Print Rows
    for row in rows {
        let mut row_line = String::from("|");
        for (i, col) in columns.iter().enumerate() {
            let val_str = match row.get(col) {
                Some(serde_json::Value::Null) | None => "NULL".to_string(),
                Some(serde_json::Value::String(s)) => s.clone(),
                Some(v) => v.to_string(),
            };
            let truncated = if val_str.len() > widths[i] {
                format!("{}...", &val_str[..widths[i].saturating_sub(3)])
            } else {
                val_str
            };
            row_line.push_str(&format!(" {:<width$} |", truncated, width = widths[i]));
        }
        println!("{}", row_line);
    }
    println!("{}", separator);
}

#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Address on which the HTTP server listens.
    #[arg(long, default_value = "127.0.0.1:8787")]
    pub listen: std::net::SocketAddr,

    /// Address for PostgreSQL wire protocol (e.g., 127.0.0.1:5432).
    #[arg(long)]
    pub pg_listen: Option<std::net::SocketAddr>,

    /// `SQLite` file used for the durable relay log.
    #[arg(long, default_value = "novadb-relay.sqlite3")]
    pub database_path: PathBuf,

    /// Directory containing managed `<database-id>.novadb` database files.
    #[arg(long, default_value = "novadb-data")]
    pub data_dir: PathBuf,

    /// Bearer token required by all `/v1` routes.
    #[arg(long, visible_alias = "token", env = "NOVADB_BEARER_TOKEN")]
    pub bearer_token: Option<String>,

    /// Username for PostgreSQL wire protocol authentication.
    #[arg(long, env = "NOVADB_PG_USER")]
    pub pg_user: Option<String>,

    /// Password for PostgreSQL wire protocol authentication.
    #[arg(long, env = "NOVADB_PG_PASSWORD")]
    pub pg_password: Option<String>,
}

async fn serve(args: &ServeArgs) -> Result<()> {
    if !args.data_dir.exists() {
        std::fs::create_dir_all(&args.data_dir)?;
    }
    if let Some(parent) = args.database_path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            let _ = std::fs::create_dir_all(parent);
        }
    }

    let config = novadb_server::ServerConfig {
        listen_addr: args.listen,
        database_path: args.database_path.clone(),
        data_dir: args.data_dir.clone(),
        bearer_token: args.bearer_token.clone(),
        max_push_batch_size: novadb_server::DEFAULT_MAX_PUSH_BATCH_SIZE,
        default_pull_limit: novadb_server::DEFAULT_PULL_LIMIT,
        max_pull_limit: novadb_server::DEFAULT_MAX_PULL_LIMIT,
    };
    config
        .validate()
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;

    if let Some(pg_addr) = args.pg_listen {
        let pg_db_path = args.data_dir.join("__default__.novadb");
        let pg_database = NovaDb::open(&pg_db_path)?;
        let pg_config = novadb_wire::PgConfig {
            listen_addr: pg_addr,
            username: args.pg_user.clone(),
            password: args.pg_password.clone(),
            database_path: pg_db_path.to_string_lossy().into_owned(),
        };
        tokio::select! {
            result = novadb_server::serve(config) => {
                result.map_err(|e| anyhow::anyhow!(e.to_string()))?;
            }
            result = novadb_wire::serve_pg(pg_database, pg_config) => {
                result.map_err(|e| anyhow::anyhow!(e.to_string()))?;
            }
        }
    } else {
        novadb_server::serve(config)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    }
    Ok(())
}

fn init(args: &InitArgs) -> Result<()> {
    if args.path.exists() {
        bail!("database already exists: {}", args.path.display());
    }
    if let Some(parent) = args
        .path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }

    let database = open_database(&args.path)?;

    print_json(&json!({
        "created": true,
        "path": args.path,
        "device_id": database.device_id(),
    }))
}

fn execute(args: &SqlArgs) -> Result<()> {
    let sql = read_sql(args)?;
    let database = open_database(&args.path)?;
    database
        .execute_batch(&sql)
        .context("failed to execute SQL")?;
    print_json(&json!({ "ok": true }))
}

fn query(args: &SqlArgs) -> Result<()> {
    let sql = read_sql(args)?;
    let database = open_database(&args.path)?;
    let result = database.query(&sql).context("query failed")?;
    print_json(&result)
}

fn sync_enable(args: &SyncEnableArgs) -> Result<()> {
    let database = open_database(&args.path)?;
    database
        .enable_sync(&args.table, &args.primary_key)
        .context("failed to enable sync")?;
    print_json(&json!({
        "sync_enabled": true,
        "table": args.table,
        "primary_key": args.primary_key,
        "device_id": database.device_id(),
    }))
}

fn changes(args: &ChangesArgs) -> Result<()> {
    validate_cursor(args.after, "after")?;
    validate_limit(args.limit)?;
    let database = open_database(&args.path)?;
    let changes = database
        .changes_after(args.after, args.limit)
        .context("failed to read local changes")?;
    let cursor = latest_local_cursor(args.after, &changes);
    print_json(&json!({
        "changes": changes,
        "cursor": cursor,
    }))
}

fn backup(args: &BackupArgs) -> Result<()> {
    let database = open_existing_database(&args.path)?;
    database
        .backup_to(&args.destination)
        .with_context(|| format!("failed to back up to {}", args.destination.display()))?;
    print_json(&json!({
        "backed_up": true,
        "source": args.path,
        "destination": args.destination,
    }))
}

fn integrity(args: &DatabasePathArgs) -> Result<()> {
    let database = open_existing_database(&args.path)?;
    let report = database
        .integrity_check()
        .context("database integrity check failed")?;
    print_json(&report)
}

fn checkpoint(args: &DatabasePathArgs) -> Result<()> {
    let database = open_existing_database(&args.path)?;
    let report = database.wal_checkpoint().context("WAL checkpoint failed")?;
    print_json(&report)
}

fn migrate(args: &MigrateArgs) -> Result<()> {
    let loaded = load_migration_directory(&args.migrations_dir)?;
    let migrations = loaded
        .iter()
        .map(|migration| Migration::new(migration.version, &migration.name, &migration.sql))
        .collect::<Vec<_>>();
    let database = open_database(&args.path)?;
    let report = database
        .run_migrations(&migrations)
        .context("failed to run migrations")?;
    print_json(&report)
}

async fn push(args: &TransferArgs) -> Result<()> {
    validate_cursor(args.after, "after")?;
    let database = open_database(&args.path)?;
    let database_name = remote_database_name(&args.path, args.remote.database.as_deref())?;
    validate_limit(args.limit)?;
    let result = push_changes(
        &database,
        &args.remote,
        &database_name,
        args.after,
        args.limit,
    )
    .await?;
    print_json(&result)
}

async fn pull(args: &TransferArgs) -> Result<()> {
    validate_cursor(args.after, "after")?;
    validate_limit(args.limit)?;
    let database = open_database(&args.path)?;
    let database_name = remote_database_name(&args.path, args.remote.database.as_deref())?;
    let result = pull_changes(
        &database,
        &args.remote,
        &database_name,
        args.after,
        args.limit,
    )
    .await?;
    print_json(&result)
}

async fn sync(args: &SyncArgs) -> Result<()> {
    validate_cursor(args.local_after, "local-after")?;
    validate_cursor(args.remote_after, "remote-after")?;
    validate_limit(args.limit)?;
    let database = open_database(&args.path)?;
    let database_name = remote_database_name(&args.path, args.remote.database.as_deref())?;

    let pushed = push_changes(
        &database,
        &args.remote,
        &database_name,
        args.local_after,
        args.limit,
    )
    .await?;
    let pulled = pull_changes(
        &database,
        &args.remote,
        &database_name,
        args.remote_after,
        args.limit,
    )
    .await?;

    print_json(&json!({ "pushed": pushed, "pulled": pulled }))
}

async fn remote_command(args: &RemoteCommandArgs) -> Result<()> {
    match &args.command {
        RemoteCommand::List(args) => remote_list(args).await,
        RemoteCommand::Create(args) => remote_create(args).await,
        RemoteCommand::Query(args) => remote_sql(args, "query").await,
        RemoteCommand::Exec(args) => remote_sql(args, "execute").await,
        RemoteCommand::Schema(args) => remote_schema(args).await,
        RemoteCommand::Integrity(args) => {
            remote_maintenance::<IntegrityReport>(args, "integrity").await
        }
        RemoteCommand::Checkpoint(args) => {
            remote_maintenance::<WalCheckpointReport>(args, "checkpoint").await
        }
        RemoteCommand::Backup(args) => {
            remote_maintenance::<RemoteBackupResponse>(args, "backup").await
        }
        RemoteCommand::Migrate(args) => remote_migrate(args).await,
    }
}

async fn remote_list(args: &RemoteListArgs) -> Result<()> {
    let endpoint = server_endpoint(&args.connection.remote, "/v1/admin/databases")?;
    let response: RemoteDatabaseListResponse = send_json(
        authorized(
            Client::new().get(&endpoint),
            args.connection.token.as_deref(),
        ),
        &endpoint,
    )
    .await?;
    print_json(&response)
}

async fn remote_create(args: &RemoteDatabaseArgs) -> Result<()> {
    validate_database_name(&args.database)?;
    let endpoint = server_endpoint(
        &args.connection.remote,
        &format!("/v1/admin/databases/{}", args.database),
    )?;
    let response: RemoteDatabaseMetadata = send_json(
        authorized(
            Client::new().post(&endpoint),
            args.connection.token.as_deref(),
        ),
        &endpoint,
    )
    .await?;
    print_json(&response)
}

async fn remote_sql(args: &RemoteSqlArgs, operation: &str) -> Result<()> {
    validate_database_name(&args.database)?;
    let sql = read_sql_source(args.sql.as_deref(), args.file.as_deref())?;
    let endpoint = server_endpoint(
        &args.connection.remote,
        &format!("/v1/databases/{}/sql/{operation}", args.database),
    )?;
    let request = RemoteSqlRequest { sql: &sql };
    let http_request = authorized(
        Client::new().post(&endpoint).json(&request),
        args.connection.token.as_deref(),
    );

    if operation == "query" {
        let response: QueryResult = send_json(http_request, &endpoint).await?;
        print_json(&response)
    } else {
        let response: RemoteExecuteResponse = send_json(http_request, &endpoint).await?;
        print_json(&response)
    }
}

async fn remote_schema(args: &RemoteDatabaseArgs) -> Result<()> {
    validate_database_name(&args.database)?;
    let endpoint = server_endpoint(
        &args.connection.remote,
        &format!("/v1/databases/{}/schema", args.database),
    )?;
    let response: RemoteSchemaResponse = send_json(
        authorized(
            Client::new().get(&endpoint),
            args.connection.token.as_deref(),
        ),
        &endpoint,
    )
    .await?;
    print_json(&response)
}

async fn remote_maintenance<T>(args: &RemoteDatabaseArgs, operation: &str) -> Result<()>
where
    T: serde::de::DeserializeOwned + Serialize,
{
    validate_database_name(&args.database)?;
    let endpoint = server_endpoint(
        &args.connection.remote,
        &format!("/v1/databases/{}/maintenance/{operation}", args.database),
    )?;
    let response: T = send_json(
        authorized(
            Client::new().post(&endpoint),
            args.connection.token.as_deref(),
        ),
        &endpoint,
    )
    .await?;
    print_json(&response)
}

async fn remote_migrate(args: &RemoteMigrateArgs) -> Result<()> {
    validate_database_name(&args.database)?;
    let loaded = load_migration_directory(&args.migrations_dir)?;
    let request = RemoteMigrationRequest {
        migrations: loaded
            .iter()
            .map(|migration| RemoteMigration {
                version: migration.version,
                name: &migration.name,
                sql: &migration.sql,
            })
            .collect(),
    };
    let endpoint = server_endpoint(
        &args.connection.remote,
        &format!("/v1/databases/{}/migrations", args.database),
    )?;
    let response: MigrationReport = send_json(
        authorized(
            Client::new().post(&endpoint).json(&request),
            args.connection.token.as_deref(),
        ),
        &endpoint,
    )
    .await?;
    print_json(&response)
}

fn open_database(path: &Path) -> Result<NovaDb> {
    NovaDb::open(path).with_context(|| format!("failed to open database {}", path.display()))
}

fn open_existing_database(path: &Path) -> Result<NovaDb> {
    if !path.is_file() {
        bail!("database file does not exist: {}", path.display());
    }
    open_database(path)
}

fn read_sql(args: &SqlArgs) -> Result<String> {
    read_sql_source(args.sql.as_deref(), args.file.as_deref())
}

fn read_sql_source(sql: Option<&str>, file: Option<&Path>) -> Result<String> {
    let sql = if let Some(file) = file {
        fs::read_to_string(file)
            .with_context(|| format!("failed to read SQL file {}", file.display()))?
    } else if let Some(sql) = sql {
        sql.to_owned()
    } else {
        if io::stdin().is_terminal() {
            bail!("SQL is required as an argument, through --file, or on standard input");
        }
        let mut sql = String::new();
        io::stdin()
            .read_to_string(&mut sql)
            .context("failed to read SQL from standard input")?;
        sql
    };

    if sql.trim().is_empty() {
        bail!("SQL input is empty");
    }
    Ok(sql)
}

fn load_migration_directory(directory: &Path) -> Result<Vec<LoadedMigration>> {
    let entries = fs::read_dir(directory)
        .with_context(|| format!("failed to read migration directory {}", directory.display()))?;
    let mut migrations = Vec::new();

    for entry in entries {
        let entry = entry.with_context(|| {
            format!(
                "failed to read an entry in migration directory {}",
                directory.display()
            )
        })?;
        let path = entry.path();
        let file_name = entry.file_name().into_string().map_err(|_| {
            anyhow::anyhow!("migration filename is not valid UTF-8: {}", path.display())
        })?;
        if Path::new(&file_name).extension() != Some(OsStr::new("sql")) {
            continue;
        }
        if !entry
            .file_type()
            .with_context(|| format!("failed to inspect migration {}", path.display()))?
            .is_file()
        {
            bail!("migration is not a regular file: {}", path.display());
        }

        let stem = file_name
            .strip_suffix(".sql")
            .expect("the .sql suffix was checked");
        let (version_text, raw_name) = stem.split_once('_').with_context(|| {
            format!("migration `{file_name}` must match <positive-version>_<name>.sql")
        })?;
        if version_text.is_empty() || !version_text.bytes().all(|byte| byte.is_ascii_digit()) {
            bail!("migration `{file_name}` has an invalid positive version");
        }
        let version = version_text
            .parse::<i64>()
            .with_context(|| format!("migration `{file_name}` has an invalid positive version"))?;
        if version <= 0 {
            bail!("migration `{file_name}` has an invalid positive version");
        }
        let name = readable_migration_name(raw_name);
        if name.is_empty() {
            bail!("migration `{file_name}` has an empty name");
        }
        let bytes = fs::read(&path)
            .with_context(|| format!("failed to read migration {}", path.display()))?;
        let sql = String::from_utf8(bytes)
            .with_context(|| format!("migration {} is not valid UTF-8", path.display()))?;

        migrations.push(LoadedMigration { version, name, sql });
    }

    migrations.sort_unstable_by_key(|migration| migration.version);
    for pair in migrations.windows(2) {
        if pair[0].version == pair[1].version {
            bail!("duplicate migration version {}", pair[0].version);
        }
    }

    Ok(migrations)
}

fn readable_migration_name(raw_name: &str) -> String {
    raw_name
        .split(['_', '-'])
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn validate_cursor(cursor: i64, option: &str) -> Result<()> {
    if cursor < 0 {
        bail!("--{option} cannot be negative");
    }
    Ok(())
}

fn validate_limit(limit: usize) -> Result<()> {
    if limit == 0 {
        bail!("--limit must be greater than zero");
    }
    Ok(())
}

fn remote_database_name(path: &Path, explicit: Option<&str>) -> Result<String> {
    let name = match explicit {
        Some(name) => name.to_owned(),
        None => path
            .file_stem()
            .and_then(OsStr::to_str)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .with_context(|| {
                format!(
                    "cannot derive relay database name from {}; pass --database",
                    path.display()
                )
            })?,
    };
    validate_database_name(&name)?;
    Ok(name)
}

fn validate_database_name(name: &str) -> Result<()> {
    let valid_length = !name.is_empty() && name.len() <= MAX_DATABASE_NAME_LENGTH;
    let mut bytes = name.bytes();
    let valid_first = bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit());
    let valid_rest = bytes.all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
    });
    if valid_length && valid_first && valid_rest {
        Ok(())
    } else {
        bail!(
            "relay database name must match [a-z0-9][a-z0-9_-]{{0,{}}}",
            MAX_DATABASE_NAME_LENGTH - 1
        )
    }
}

fn latest_local_cursor(after: i64, changes: &[novadb_core::protocol::Change]) -> i64 {
    changes
        .iter()
        .map(|change| change.seq)
        .max()
        .unwrap_or(after)
}

async fn push_changes(
    database: &NovaDb,
    remote: &RemoteArgs,
    database_name: &str,
    after: i64,
    limit: usize,
) -> Result<Value> {
    let client = Client::new();
    let endpoint = endpoint(&remote.remote, database_name, "push")?;
    let mut local_cursor = after;
    let mut remote_cursor = None;
    let mut sent = 0_usize;
    let mut accepted = 0_usize;
    let mut duplicates = 0_usize;

    loop {
        let page_after = local_cursor;
        let changes = database
            .changes_after(local_cursor, limit)
            .context("failed to read local changes")?;
        let count = changes.len();
        if changes.is_empty() {
            break;
        }
        let ranges = push_request_ranges(&changes)?;
        for range in ranges {
            let request = PushRequest {
                changes: changes[range].to_vec(),
            };
            let batch_count = request.changes.len();
            let next_cursor = latest_local_cursor(local_cursor, &request.changes);
            let encoded = serde_json::to_vec(&request).context("failed to encode push request")?;
            if encoded.len() > MAX_PUSH_REQUEST_BYTES {
                bail!(
                    "push request is {} bytes, exceeding the {}-byte server limit",
                    encoded.len(),
                    MAX_PUSH_REQUEST_BYTES
                );
            }
            let response: PushResponse = send_json(
                authorized(
                    client
                        .post(&endpoint)
                        .header(reqwest::header::CONTENT_TYPE, "application/json")
                        .body(encoded),
                    remote.token.as_deref(),
                ),
                &endpoint,
            )
            .await?;

            let accounted_for = response
                .accepted
                .checked_add(response.duplicates)
                .context("relay response counters overflowed")?;
            if accounted_for != batch_count {
                bail!(
                    "relay accounted for {accounted_for} of {batch_count} pushed changes; local cursor was not advanced"
                );
            }

            sent += batch_count;
            accepted += response.accepted;
            duplicates += response.duplicates;
            remote_cursor = Some(response.cursor);
            local_cursor = next_cursor;
        }

        if count < limit {
            break;
        }
        if local_cursor <= page_after {
            bail!("local change log did not advance while more changes were expected");
        }
    }

    Ok(json!({
        "sent": sent,
        "accepted": accepted,
        "duplicates": duplicates,
        "local_cursor": local_cursor,
        "remote_cursor": remote_cursor,
    }))
}

fn push_request_ranges(
    changes: &[novadb_core::protocol::Change],
) -> Result<Vec<std::ops::Range<usize>>> {
    const EMPTY_REQUEST_BYTES: usize = b"{\"changes\":[]}".len();
    let mut ranges = Vec::new();
    let mut start = 0;

    while start < changes.len() {
        let mut end = start;
        let mut request_bytes = EMPTY_REQUEST_BYTES;
        while end < changes.len() {
            let change_bytes = serde_json::to_vec(&changes[end])
                .context("failed to encode a local change")?
                .len();
            let separator_bytes = usize::from(end > start);
            let Some(candidate_size) = request_bytes
                .checked_add(change_bytes)
                .and_then(|size| size.checked_add(separator_bytes))
            else {
                bail!("push request size overflowed");
            };
            if candidate_size > MAX_PUSH_REQUEST_BYTES {
                break;
            }
            request_bytes = candidate_size;
            end += 1;
        }
        if end == start {
            bail!(
                "local change {} at sequence {} cannot fit in the {}-byte server request limit",
                changes[start].change_id,
                changes[start].seq,
                MAX_PUSH_REQUEST_BYTES
            );
        }
        ranges.push(start..end);
        start = end;
    }
    Ok(ranges)
}

async fn pull_changes(
    database: &NovaDb,
    remote: &RemoteArgs,
    database_name: &str,
    after: i64,
    limit: usize,
) -> Result<Value> {
    let client = Client::new();
    let endpoint = endpoint(&remote.remote, database_name, "pull")?;
    let mut cursor = after;
    let mut report = ApplyReport::default();
    let mut received = 0_usize;

    loop {
        let request = client
            .get(&endpoint)
            .query(&[("after", cursor.to_string()), ("limit", limit.to_string())]);
        let page: PullResponse =
            send_json(authorized(request, remote.token.as_deref()), &endpoint).await?;

        if page.cursor < cursor {
            bail!(
                "relay returned cursor {} behind requested cursor {cursor}",
                page.cursor
            );
        }
        validate_pull_page(cursor, limit, &page)?;
        let page_count = page.changes.len();
        let changes = page
            .changes
            .into_iter()
            .map(|relay| relay.change)
            .collect::<Vec<_>>();
        let applied = database
            .apply_changes(&changes)
            .context("failed to apply pulled changes")?;
        merge_apply_report(&mut report, &applied);
        received += page_count;
        let previous_cursor = cursor;
        cursor = page.cursor;

        if !page.has_more {
            break;
        }
        if cursor <= previous_cursor {
            bail!("relay reported more changes without advancing its cursor");
        }
    }

    Ok(json!({
        "received": received,
        "applied": report.applied,
        "ignored": report.ignored,
        "duplicates": report.duplicates,
        "remote_cursor": cursor,
    }))
}

fn validate_pull_page(after: i64, limit: usize, page: &PullResponse) -> Result<()> {
    if page.changes.len() > limit {
        bail!(
            "relay returned {} changes, exceeding the requested limit of {limit}",
            page.changes.len()
        );
    }

    let mut previous = after;
    for relay in &page.changes {
        if relay.cursor <= previous {
            bail!("relay returned non-increasing change cursors");
        }
        previous = relay.cursor;
    }

    if previous != page.cursor {
        bail!(
            "relay page cursor {} does not match its last change cursor {previous}",
            page.cursor
        );
    }
    if page.has_more && page.changes.is_empty() {
        bail!("relay reported more changes in an empty page");
    }
    Ok(())
}

fn merge_apply_report(total: &mut ApplyReport, page: &ApplyReport) {
    total.applied += page.applied;
    total.ignored += page.ignored;
    total.duplicates += page.duplicates;
}

fn endpoint(remote: &str, database_name: &str, operation: &str) -> Result<String> {
    validate_database_name(database_name)?;
    server_endpoint(
        remote,
        &format!("/v1/databases/{database_name}/{operation}"),
    )
}

fn server_endpoint(remote: &str, path: &str) -> Result<String> {
    let remote = remote.trim_end_matches('/');
    if remote.is_empty() {
        bail!("--remote cannot be empty");
    }
    if !path.starts_with('/') {
        bail!("internal server path must start with '/'");
    }
    Ok(format!("{remote}{path}"))
}

fn authorized(request: RequestBuilder, token: Option<&str>) -> RequestBuilder {
    match token {
        Some(token) => request.bearer_auth(token),
        None => request,
    }
}

async fn send_json<T>(request: RequestBuilder, endpoint: &str) -> Result<T>
where
    T: serde::de::DeserializeOwned,
{
    let response = request
        .send()
        .await
        .with_context(|| format!("request to {endpoint} failed"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(http_error(response, status, endpoint).await);
    }
    response
        .json::<T>()
        .await
        .with_context(|| format!("server returned invalid JSON from {endpoint}"))
}

async fn http_error(
    response: reqwest::Response,
    status: StatusCode,
    endpoint: &str,
) -> anyhow::Error {
    let body = response.text().await.unwrap_or_default();
    let detail = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| {
                    error
                        .as_str()
                        .or_else(|| error.get("message").and_then(Value::as_str))
                })
                .map(str::to_owned)
        })
        .unwrap_or_else(|| body.trim().to_owned());

    if detail.is_empty() {
        anyhow::anyhow!("server request to {endpoint} failed with HTTP {status}")
    } else {
        anyhow::anyhow!("server request to {endpoint} failed with HTTP {status}: {detail}")
    }
}

fn print_json(value: &impl Serialize) -> Result<()> {
    let output = serde_json::to_string_pretty(value).context("failed to serialize output")?;
    println!("{output}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_init() {
        let cli = Cli::try_parse_from(["novadb", "init", "notes.db"]).unwrap();
        let Command::Init(args) = cli.command else {
            panic!("expected init command");
        };
        assert_eq!(args.path, PathBuf::from("notes.db"));
    }

    #[test]
    fn parses_sync_enable_table_and_primary_key() {
        let cli = Cli::try_parse_from([
            "novadb",
            "sync-enable",
            "notes.db",
            "notes",
            "--primary-key",
            "note_id",
        ])
        .unwrap();
        let Command::SyncEnable(args) = cli.command else {
            panic!("expected sync-enable command");
        };
        assert_eq!(args.table, "notes");
        assert_eq!(args.primary_key, "note_id");
    }

    #[test]
    fn sync_enable_defaults_to_id_primary_key() {
        let cli = Cli::try_parse_from(["novadb", "sync-enable", "notes.db", "notes"]).unwrap();
        let Command::SyncEnable(args) = cli.command else {
            panic!("expected sync-enable command");
        };
        assert_eq!(args.primary_key, "id");
    }

    #[test]
    fn parses_query_sql_argument() {
        let cli =
            Cli::try_parse_from(["novadb", "query", "notes.db", "select * from notes"]).unwrap();
        let Command::Query(args) = cli.command else {
            panic!("expected query command");
        };
        assert_eq!(args.sql.as_deref(), Some("select * from notes"));
        assert!(args.file.is_none());
    }

    #[test]
    fn parses_local_maintenance_commands() {
        let backup = Cli::try_parse_from(["novadb", "backup", "app.db", "backup.db"]).unwrap();
        let Command::Backup(args) = backup.command else {
            panic!("expected backup command");
        };
        assert_eq!(args.path, PathBuf::from("app.db"));
        assert_eq!(args.destination, PathBuf::from("backup.db"));

        let integrity = Cli::try_parse_from(["novadb", "integrity", "app.db"]).unwrap();
        assert!(matches!(integrity.command, Command::Integrity(_)));

        let checkpoint = Cli::try_parse_from(["novadb", "checkpoint", "app.db"]).unwrap();
        assert!(matches!(checkpoint.command, Command::Checkpoint(_)));

        let migrate = Cli::try_parse_from(["novadb", "migrate", "app.db", "migrations"]).unwrap();
        let Command::Migrate(args) = migrate.command else {
            panic!("expected migrate command");
        };
        assert_eq!(args.migrations_dir, PathBuf::from("migrations"));
    }

    #[test]
    fn sql_argument_conflicts_with_file() {
        let error = Cli::try_parse_from([
            "novadb",
            "exec",
            "notes.db",
            "select 1",
            "--file",
            "query.sql",
        ])
        .unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn parses_push_remote_options() {
        let cli = Cli::try_parse_from([
            "novadb",
            "push",
            "notes.db",
            "--remote",
            "http://127.0.0.1:3000",
            "--database",
            "team-notes",
            "--after",
            "42",
            "--token",
            "secret",
        ])
        .unwrap();
        let Command::Push(args) = cli.command else {
            panic!("expected push command");
        };
        assert_eq!(args.after, 42);
        assert_eq!(args.remote.database.as_deref(), Some("team-notes"));
        assert_eq!(args.remote.token.as_deref(), Some("secret"));
    }

    #[test]
    fn parses_sync_cursors() {
        let cli = Cli::try_parse_from([
            "novadb",
            "sync",
            "notes.db",
            "--remote",
            "https://relay.example",
            "--local-after",
            "8",
            "--remote-after",
            "13",
            "--limit",
            "50",
        ])
        .unwrap();
        let Command::Sync(args) = cli.command else {
            panic!("expected sync command");
        };
        assert_eq!(args.local_after, 8);
        assert_eq!(args.remote_after, 13);
        assert_eq!(args.limit, 50);
    }

    #[test]
    fn parses_nested_remote_commands() {
        let list =
            Cli::try_parse_from(["novadb", "remote", "list", "--remote", "https://db.example"])
                .unwrap();
        let Command::Remote(args) = list.command else {
            panic!("expected remote command");
        };
        assert!(matches!(args.command, RemoteCommand::List(_)));

        let query = Cli::try_parse_from([
            "novadb",
            "remote",
            "query",
            "appdb",
            "SELECT 1",
            "--remote",
            "https://db.example",
            "--token",
            "secret",
        ])
        .unwrap();
        let Command::Remote(args) = query.command else {
            panic!("expected remote command");
        };
        let RemoteCommand::Query(args) = args.command else {
            panic!("expected remote query command");
        };
        assert_eq!(args.database, "appdb");
        assert_eq!(args.sql.as_deref(), Some("SELECT 1"));
        assert_eq!(args.connection.token.as_deref(), Some("secret"));

        for subcommand in ["create", "schema", "integrity", "checkpoint", "backup"] {
            let cli = Cli::try_parse_from([
                "novadb",
                "remote",
                subcommand,
                "appdb",
                "--remote",
                "https://db.example",
            ])
            .unwrap();
            assert!(matches!(cli.command, Command::Remote(_)));
        }

        let execute = Cli::try_parse_from([
            "novadb",
            "remote",
            "exec",
            "appdb",
            "CREATE TABLE notes(id INTEGER PRIMARY KEY)",
            "--remote",
            "https://db.example",
        ])
        .unwrap();
        let Command::Remote(args) = execute.command else {
            panic!("expected remote command");
        };
        assert!(matches!(args.command, RemoteCommand::Exec(_)));

        let migrate = Cli::try_parse_from([
            "novadb",
            "remote",
            "migrate",
            "appdb",
            "migrations",
            "--remote",
            "https://db.example",
        ])
        .unwrap();
        let Command::Remote(args) = migrate.command else {
            panic!("expected remote command");
        };
        let RemoteCommand::Migrate(args) = args.command else {
            panic!("expected remote migrate command");
        };
        assert_eq!(args.database, "appdb");
        assert_eq!(args.migrations_dir, PathBuf::from("migrations"));
    }

    #[test]
    fn migration_loader_sorts_versions_and_makes_names_readable() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("20_add-user_email.sql"), "SELECT 2;").unwrap();
        fs::write(directory.path().join("10_create_users.sql"), "SELECT 1;").unwrap();
        fs::write(directory.path().join("README.md"), "ignored").unwrap();

        let migrations = load_migration_directory(directory.path()).unwrap();
        assert_eq!(migrations.len(), 2);
        assert_eq!(migrations[0].version, 10);
        assert_eq!(migrations[0].name, "create users");
        assert_eq!(migrations[1].version, 20);
        assert_eq!(migrations[1].name, "add user email");
    }

    #[test]
    fn migration_loader_rejects_duplicate_versions() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("1_first.sql"), "SELECT 1;").unwrap();
        fs::write(directory.path().join("01_second.sql"), "SELECT 2;").unwrap();

        let error = load_migration_directory(directory.path()).unwrap_err();
        assert!(error.to_string().contains("duplicate migration version 1"));
    }

    #[test]
    fn migration_loader_rejects_non_positive_version() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("0_invalid.sql"), "SELECT 1;").unwrap();

        let error = load_migration_directory(directory.path()).unwrap_err();
        assert!(error.to_string().contains("invalid positive version"));
    }

    #[test]
    fn migration_loader_rejects_invalid_utf8_sql() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("1_invalid.sql"), [0xff, 0xfe]).unwrap();

        let error = load_migration_directory(directory.path()).unwrap_err();
        assert!(error.to_string().contains("not valid UTF-8"));
    }

    #[cfg(unix)]
    #[test]
    fn migration_loader_rejects_invalid_utf8_filename() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let directory = tempfile::tempdir().unwrap();
        let name = OsString::from_vec(vec![b'1', b'_', 0xff, b'.', b's', b'q', b'l']);
        fs::write(directory.path().join(name), "SELECT 1;").unwrap();

        let error = load_migration_directory(directory.path()).unwrap_err();
        assert!(error.to_string().contains("filename is not valid UTF-8"));
    }

    #[test]
    fn backup_command_creates_an_openable_copy() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.db");
        let destination = directory.path().join("backup.db");
        let database = NovaDb::open(&source).unwrap();
        database
            .execute_batch(
                "CREATE TABLE notes(id INTEGER PRIMARY KEY, title TEXT NOT NULL); \
                 INSERT INTO notes(title) VALUES ('kept');",
            )
            .unwrap();
        drop(database);

        backup(&BackupArgs {
            path: source,
            destination: destination.clone(),
        })
        .unwrap();

        let copy = NovaDb::open(destination).unwrap();
        let result = copy.query("SELECT title FROM notes").unwrap();
        assert_eq!(result.rows, vec![json!({ "title": "kept" })]);
    }

    #[test]
    fn migrate_command_applies_gapped_manifest_idempotently() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("app.db");
        let migrations_dir = directory.path().join("migrations");
        fs::create_dir(&migrations_dir).unwrap();
        fs::write(
            migrations_dir.join("10_create-users.sql"),
            "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT NOT NULL);",
        )
        .unwrap();
        fs::write(
            migrations_dir.join("20_seed_admin.sql"),
            "INSERT INTO users(name) VALUES ('admin');",
        )
        .unwrap();
        let args = MigrateArgs {
            path: database_path.clone(),
            migrations_dir,
        };

        migrate(&args).unwrap();
        migrate(&args).unwrap();

        let database = NovaDb::open(database_path).unwrap();
        let rows = database
            .query("SELECT id, name FROM users ORDER BY id")
            .unwrap();
        assert_eq!(rows.rows, vec![json!({ "id": 1, "name": "admin" })]);
        let ledger = database
            .query("SELECT version, name FROM _novadb_migrations ORDER BY version")
            .unwrap();
        assert_eq!(
            ledger.rows,
            vec![
                json!({ "version": 10, "name": "create users" }),
                json!({ "version": 20, "name": "seed admin" }),
            ]
        );
    }

    #[test]
    fn remote_database_defaults_to_local_file_stem() {
        assert_eq!(
            remote_database_name(Path::new("data/team.db"), None).unwrap(),
            "team"
        );
    }

    #[test]
    fn remote_database_rejects_invalid_server_id() {
        let error = remote_database_name(Path::new("notes.db"), Some("team.notes")).unwrap_err();
        assert!(error.to_string().contains("must match"));
    }

    #[test]
    fn endpoint_ignores_one_trailing_slash() {
        assert_eq!(
            endpoint("http://localhost:3000/", "notes", "pull").unwrap(),
            "http://localhost:3000/v1/databases/notes/pull"
        );
    }

    #[test]
    fn validate_database_name_accepts_valid_ids() {
        for valid in ["a", "notes", "my-db", "db_1", "a123456"] {
            validate_database_name(valid).unwrap_or_else(|_| panic!("should accept: {valid}"));
        }
    }

    #[test]
    fn validate_database_name_rejects_invalid_ids() {
        for invalid in [
            "",                                        // empty
            ".hidden",                                 // starts with dot
            "-starts-dash",                            // starts with dash
            "_starts_underscore",                      // starts with underscore
            "has space",                               // contains space
            "has.dot",                                 // contains dot
            "has/slash",                               // contains slash
            "UPPER",                                   // uppercase
            &"a".repeat(MAX_DATABASE_NAME_LENGTH + 1), // too long
        ] {
            assert!(
                validate_database_name(invalid).is_err(),
                "should reject: {invalid:?}"
            );
        }
    }

    #[test]
    fn init_creates_a_database_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("new.db");
        init(&InitArgs { path: path.clone() }).unwrap();
        assert!(path.exists());
        // Can be opened as a valid NovaDB database
        let db = NovaDb::open(&path).unwrap();
        assert!(!db.device_id().is_empty());
    }

    #[test]
    fn sql_from_file_reads_correctly() {
        let directory = tempfile::tempdir().unwrap();
        let sql_file = directory.path().join("schema.sql");
        fs::write(&sql_file, "CREATE TABLE test(id INTEGER PRIMARY KEY);").unwrap();

        let sql = read_sql_source(None, Some(&sql_file)).unwrap();
        assert_eq!(sql, "CREATE TABLE test(id INTEGER PRIMARY KEY);");
    }

    #[test]
    fn validate_cursor_accepts_non_negative() {
        validate_cursor(0, "--after").unwrap();
        validate_cursor(100, "--after").unwrap();
    }

    #[test]
    fn validate_cursor_rejects_negative() {
        assert!(validate_cursor(-1, "--after").is_err());
    }

    #[test]
    fn validate_limit_accepts_positive() {
        validate_limit(1).unwrap();
        validate_limit(1000).unwrap();
    }

    #[test]
    fn validate_limit_rejects_zero() {
        assert!(validate_limit(0).is_err());
    }

    #[test]
    fn latest_local_cursor_returns_max_seq() {
        use novadb_core::protocol::{Change, ChangeOperation};
        let changes = vec![
            Change {
                seq: 5,
                change_id: "a".into(),
                table: "t".into(),
                row_id: "t:1".into(),
                operation: ChangeOperation::Upsert,
                payload: None,
                hlc: "0000000000000001-00000000".into(),
                device_id: "d".into(),
                created_at_ms: 1,
            },
            Change {
                seq: 10,
                change_id: "b".into(),
                table: "t".into(),
                row_id: "t:2".into(),
                operation: ChangeOperation::Upsert,
                payload: None,
                hlc: "0000000000000002-00000000".into(),
                device_id: "d".into(),
                created_at_ms: 2,
            },
        ];
        assert_eq!(latest_local_cursor(0, &changes), 10);
    }

    #[test]
    fn latest_local_cursor_returns_after_when_empty() {
        assert_eq!(latest_local_cursor(42, &[]), 42);
    }
}
