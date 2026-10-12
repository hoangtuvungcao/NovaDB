//! Interactive SQL console (`novadb console <path>`).

mod commands;
pub(crate) mod csv;
mod helper;
mod render;
pub(crate) mod sql;

use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use clap::Args;
use novadb_core::{Error as CoreError, NovaDb, QueryResult};
use rustyline::Editor;
use rustyline::config::{CompletionType, Config};
use rustyline::error::ReadlineError;
use rustyline::history::DefaultHistory;

use helper::ConsoleHelper;
use render::{OutputMode, Style};

#[derive(Debug, Args)]
pub struct ConsoleArgs {
    /// Path to the local `NovaDB` database file to open.
    #[arg(value_name = "PATH")]
    pub path: PathBuf,
}

/// Whether ANSI colors should be used for a stream.
fn color_enabled(is_tty: bool) -> bool {
    is_tty
        && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
        && std::env::var("TERM").map_or(true, |t| t != "dumb")
}

fn history_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("NOVADB_HISTORY") {
        return Some(PathBuf::from(path));
    }
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|home| PathBuf::from(home).join(".novadb_history"))
}

pub(crate) fn console(args: &ConsoleArgs) -> Result<()> {
    let existed = args.path.exists();
    let db = NovaDb::open(&args.path)?;
    let interactive = io::stdin().is_terminal();
    let out = Style {
        color: color_enabled(io::stdout().is_terminal()),
    };
    let err = Style {
        color: color_enabled(io::stderr().is_terminal()),
    };
    let mut session = Session::new(db, out, err, interactive);

    if !interactive {
        let mut input = String::new();
        io::stdin().read_to_string(&mut input)?;
        session.feed_text(&input);
        if session.errors > 0 {
            bail!("{} statement(s) failed", session.errors);
        }
        return Ok(());
    }

    let names = session.schema_names();
    print_banner(&session, args, existed, names.0.len());
    run_interactive(&mut session, names)
}

fn print_banner(session: &Session, args: &ConsoleArgs, existed: bool, tables: usize) {
    let s = session.out;
    println!(
        "{} {} {}",
        s.paint("1;36", "NovaDB"),
        s.bold(&format!("v{}", env!("CARGO_PKG_VERSION"))),
        s.dim("· interactive SQL console")
    );
    let status = if existed {
        format!("{} table{}", tables, if tables == 1 { "" } else { "s" })
    } else {
        "new database created".to_owned()
    };
    println!(
        "Connected to {} {}",
        s.bold(&args.path.display().to_string()),
        s.dim(&format!("({status})"))
    );
    println!(
        "{}",
        s.dim("Type .help or \\? for help, .quit or Ctrl+D to exit. End SQL statements with ;")
    );
    println!();
}

fn run_interactive(session: &mut Session, names: (Vec<String>, Vec<String>)) -> Result<()> {
    let config = Config::builder()
        .auto_add_history(false)
        .history_ignore_dups(true)?
        .history_ignore_space(true)
        .max_history_size(5_000)?
        .completion_type(CompletionType::List)
        .build();
    let mut editor: Editor<ConsoleHelper, DefaultHistory> = Editor::with_config(config)?;
    let mut helper = ConsoleHelper::new(session.out.color);
    (helper.tables, helper.columns) = names;
    editor.set_helper(Some(helper));

    let history = history_path();
    if let Some(path) = &history {
        let _ = editor.load_history(path);
    }

    loop {
        let prompt = session.prompt();
        match editor.readline(&prompt) {
            Ok(line) => {
                if !line.trim().is_empty() {
                    let _ = editor.add_history_entry(line.as_str());
                }
                let flow = session.feed_line(&line);
                if session.schema_dirty {
                    session.schema_dirty = false;
                    let (tables, columns) = session.schema_names();
                    if let Some(helper) = editor.helper_mut() {
                        helper.tables = tables;
                        helper.columns = columns;
                    }
                }
                if flow == Flow::Quit {
                    break;
                }
            }
            Err(ReadlineError::Interrupted) => {
                if session.buffer.is_empty() {
                    println!("{}", session.out.dim("(use .quit or Ctrl+D to exit)"));
                } else {
                    session.buffer.clear();
                    println!("{}", session.out.dim("(input cleared)"));
                }
            }
            Err(ReadlineError::Eof) => break,
            Err(error) => return Err(error.into()),
        }
    }

    if let Some(path) = &history {
        let _ = editor.save_history(path);
    }
    if session.in_transaction {
        let _ = session.db.rollback_transaction();
        println!(
            "{}",
            session.out.yellow("Open transaction was rolled back.")
        );
    }
    println!("{}", session.out.dim("Bye!"));
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flow {
    Continue,
    Quit,
}

pub(crate) struct Session {
    db: NovaDb,
    out: Style,
    err: Style,
    interactive: bool,
    mode: OutputMode,
    timer: bool,
    in_transaction: bool,
    /// Incomplete SQL carried over between input lines.
    buffer: String,
    /// `true` while `buffer` holds a T-SQL procedural batch (variables,
    /// control flow) that is sent whole to `execute_batch` on `GO` or at the
    /// end of the input instead of being split on `;`.
    batch: bool,
    /// Number of failed statements/commands.
    errors: usize,
    /// Set when the schema may have changed (refresh completion names).
    schema_dirty: bool,
    /// Nesting depth of `.read`.
    read_depth: usize,
    /// When > 0, per-statement success messages are suppressed (`.read`).
    quiet: usize,
}

impl Session {
    fn new(db: NovaDb, out: Style, err: Style, interactive: bool) -> Self {
        Self {
            db,
            out,
            err,
            interactive,
            mode: OutputMode::Table,
            timer: interactive,
            in_transaction: false,
            buffer: String::new(),
            batch: false,
            errors: 0,
            schema_dirty: false,
            read_depth: 0,
            quiet: 0,
        }
    }

    fn prompt(&self) -> (String, String) {
        let raw = if self.batch {
            "   GO?> ".to_owned()
        } else if !self.buffer.is_empty() {
            "   ...> ".to_owned()
        } else if self.in_transaction {
            "novadb*> ".to_owned()
        } else {
            "novadb> ".to_owned()
        };
        if !self.out.color {
            return (raw.clone(), raw);
        }
        let styled = if self.batch || !self.buffer.is_empty() {
            format!("\x1b[90m{raw}\x1b[0m")
        } else if self.in_transaction {
            "\x1b[1;32mnovadb\x1b[1;33m*\x1b[0m> ".to_owned()
        } else {
            "\x1b[1;32mnovadb\x1b[0m> ".to_owned()
        };
        (raw, styled)
    }

    fn print_error(&mut self, message: impl std::fmt::Display) {
        self.errors += 1;
        let message = message.to_string();
        let message = message.strip_prefix("SQLite error: ").unwrap_or(&message);
        // Core errors already read well (`unsupported SQL: ...`, `T-SQL error:
        // ...`); never fall back to a Debug dump.
        eprintln!(
            "{} {}",
            self.err.red("Error:"),
            self.err.paint("31", message)
        );
    }

    fn print_ok(&self, message: &str, elapsed: Option<Duration>) {
        if self.quiet > 0 || !self.interactive {
            return;
        }
        let timing = match elapsed {
            Some(d) if self.timer => self.out.dim(&format!(" ({})", format_duration(d))),
            _ => String::new(),
        };
        println!("{}{timing}", self.out.green(message));
    }

    /// Feeds one line of input; executes dot commands and complete statements.
    ///
    /// A T-SQL procedural statement (`DECLARE`, `WHILE`, `IF`, `BEGIN TRY`,
    /// `SET @x`, `PRINT`, ...) starts a batch: everything up to a `GO` line
    /// (or the end of the input) is executed as a single unit.
    pub(crate) fn feed_line(&mut self, line: &str) -> Flow {
        let trimmed = line.trim();
        if self.batch {
            if sql::is_go_line(trimmed) {
                self.run_batch();
            } else {
                self.buffer.push_str(line);
                self.buffer.push('\n');
            }
            return Flow::Continue;
        }
        if self.buffer.is_empty() && (trimmed.starts_with('.') || trimmed.starts_with('\\')) {
            return self.run_command(trimmed);
        }
        if self.buffer.is_empty() && trimmed.is_empty() {
            return Flow::Continue;
        }
        if sql::is_go_line(trimmed) {
            // `GO` terminates whatever is pending, even without a `;`.
            let rest = std::mem::take(&mut self.buffer);
            if !sql::is_blank(&rest) {
                self.run_sql(rest.trim());
            }
            return Flow::Continue;
        }
        self.buffer.push_str(line);
        self.buffer.push('\n');
        if let Some(pos) = sql::procedural_start(&self.buffer) {
            let batch = self.buffer.split_off(pos);
            let (statements, _) = sql::split_statements(&self.buffer);
            self.buffer = batch;
            for statement in statements {
                self.run_sql(&statement);
            }
            self.batch = true;
            return Flow::Continue;
        }
        let (statements, rest) = sql::split_statements(&self.buffer);
        self.buffer = rest;
        for statement in statements {
            self.run_sql(&statement);
        }
        Flow::Continue
    }

    /// Feeds a whole script; a trailing statement without `;` is executed too.
    pub(crate) fn feed_text(&mut self, text: &str) -> Flow {
        for line in text.lines() {
            if self.feed_line(line) == Flow::Quit {
                return Flow::Quit;
            }
        }
        if self.batch {
            self.run_batch();
        } else {
            let rest = std::mem::take(&mut self.buffer);
            if !sql::is_blank(&rest) {
                self.run_sql(rest.trim());
            }
        }
        Flow::Continue
    }

    /// Executes the buffered T-SQL batch as one unit via `execute_batch`.
    fn run_batch(&mut self) {
        self.batch = false;
        let text = std::mem::take(&mut self.buffer);
        if sql::is_blank(&text) {
            return;
        }
        let start = Instant::now();
        match self.execute(&text, "BATCH") {
            Ok(()) => {
                self.schema_dirty = true;
                self.print_ok("Batch OK", Some(start.elapsed()));
                if self.quiet == 0 && sql::batch_yields_rows(&text) {
                    eprintln!(
                        "{}",
                        self.err.dim(
                            "note: a T-SQL batch runs through execute_batch, which returns no \
                             rows, so SELECT results inside it are not shown. Run the SELECT \
                             as a separate statement (variables are batch-scoped), or store \
                             results in a table and query that."
                        )
                    );
                }
            }
            Err(e) => self.print_error(e),
        }
    }

    fn run_sql(&mut self, statement: &str) {
        let words = sql::keywords(statement, 2);
        let first = words.first().map(String::as_str).unwrap_or("");
        let start = Instant::now();
        match first {
            "BEGIN" => {
                match self.db.begin_transaction() {
                    Ok(()) => {
                        self.in_transaction = true;
                        self.print_ok("BEGIN", Some(start.elapsed()));
                    }
                    Err(e) => self.print_error(e),
                }
                return;
            }
            "COMMIT" | "END" => {
                match self.db.commit_transaction() {
                    Ok(()) => {
                        self.in_transaction = false;
                        self.print_ok("COMMIT", Some(start.elapsed()));
                    }
                    Err(e) => {
                        if e.to_string().contains("no transaction is active") {
                            self.in_transaction = false;
                        }
                        self.print_error(e);
                    }
                }
                return;
            }
            "ROLLBACK" if words.get(1).map(String::as_str) != Some("TO") => {
                self.in_transaction = false;
                match self.db.rollback_transaction() {
                    Ok(()) => self.print_ok("ROLLBACK", Some(start.elapsed())),
                    Err(e) => self.print_error(e),
                }
                return;
            }
            _ => {}
        }

        match self.db.query(statement) {
            Ok(result) => {
                let elapsed = start.elapsed();
                self.print_result(&result, elapsed);
            }
            Err(
                CoreError::QueryMustBeReadOnly
                | CoreError::QueryOperationNotAllowed
                | CoreError::TransactionControlNotAllowed,
            ) => self.run_exec(statement, first, start),
            Err(CoreError::Unsupported(m)) if m.contains("execute_batch") => {
                self.run_exec(statement, first, start);
            }
            Err(e) => self.print_error(e),
        }
    }

    fn execute(&self, sql: &str, first: &str) -> novadb_core::Result<()> {
        if self.in_transaction || first == "VACUUM" {
            self.db.execute_uncommitted(sql)
        } else {
            self.db.execute_batch(sql)
        }
    }

    fn run_exec(&mut self, statement: &str, first: &str, start: Instant) {
        match self.execute(statement, first) {
            Ok(()) => {
                let elapsed = start.elapsed();
                if matches!(first, "CREATE" | "DROP" | "ALTER") {
                    self.schema_dirty = true;
                }
                if self.quiet > 0 || !self.interactive {
                    return;
                }
                let message = if matches!(first, "INSERT" | "UPDATE" | "DELETE" | "REPLACE") {
                    match self.affected_rows() {
                        Some(n) => format!(
                            "{first} OK, {n} row{} affected",
                            if n == 1 { "" } else { "s" }
                        ),
                        None => format!("{first} OK"),
                    }
                } else if first.is_empty() {
                    "OK".to_owned()
                } else {
                    format!("{first} OK")
                };
                self.print_ok(&message, Some(elapsed));
            }
            Err(e) => self.print_error(e),
        }
    }

    fn affected_rows(&self) -> Option<i64> {
        let result = self.db.query("SELECT changes() AS n").ok()?;
        result.rows.first()?.get("n")?.as_i64()
    }

    fn print_result(&self, result: &QueryResult, elapsed: Duration) {
        let rendered = render::render(self.mode, &result.columns, &result.rows, self.out);
        let mut stdout = io::stdout().lock();
        let _ = stdout.write_all(rendered.as_bytes());
        if matches!(self.mode, OutputMode::Table | OutputMode::Line) {
            let n = result.rows.len();
            let mut footer = format!("({n} row{})", if n == 1 { "" } else { "s" });
            if self.timer {
                footer.push_str(&format!(" · {}", format_duration(elapsed)));
            }
            let _ = writeln!(stdout, "{}", self.out.dim(&footer));
        } else if self.timer && self.interactive {
            eprintln!(
                "{}",
                self.err.dim(&format!("({})", format_duration(elapsed)))
            );
        }
    }

    /// User table/view names and the distinct column names across them,
    /// fetched in a single query (used for completion and the banner).
    fn schema_names(&self) -> (Vec<String>, Vec<String>) {
        let sql = format!(
            "SELECT m.name AS t, p.name AS c FROM sqlite_master m \
             LEFT JOIN pragma_table_info(m.name) p \
             WHERE m.type IN ('table','view') AND {} ORDER BY m.name",
            commands::USER_OBJECT_FILTER.replace("name", "m.name")
        );
        let Ok(result) = self.db.query(&sql) else {
            return (Vec::new(), Vec::new());
        };
        let mut tables: Vec<String> = Vec::new();
        let mut columns: Vec<String> = Vec::new();
        for row in &result.rows {
            if let Some(t) = row.get("t").and_then(|v| v.as_str()) {
                if tables.last().map(String::as_str) != Some(t) {
                    tables.push(t.to_owned());
                }
            }
            if let Some(c) = row.get("c").and_then(|v| v.as_str()) {
                columns.push(c.to_owned());
            }
        }
        columns.sort();
        columns.dedup();
        (tables, columns)
    }
}

fn format_duration(d: Duration) -> String {
    let ms = d.as_secs_f64() * 1000.0;
    if ms < 1.0 {
        format!("{:.0} µs", ms * 1000.0)
    } else if ms < 1000.0 {
        format!("{ms:.2} ms")
    } else {
        format!("{:.2} s", ms / 1000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Session {
        Session::new(
            NovaDb::open_in_memory().unwrap(),
            Style { color: false },
            Style { color: false },
            false,
        )
    }

    #[test]
    fn executes_scripts_with_mixed_statements() {
        let mut s = session();
        s.feed_text(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT); -- c;\n\
             INSERT INTO t (name) VALUES ('a;b'), ('c');\n\
             UPDATE t SET name = 'z' WHERE id = 2",
        );
        assert_eq!(s.errors, 0);
        let r = s.db.query("SELECT name FROM t ORDER BY id").unwrap();
        assert_eq!(r.rows.len(), 2);
        assert_eq!(r.rows[0]["name"], "a;b");
        assert_eq!(r.rows[1]["name"], "z");
        assert_eq!(
            s.schema_names(),
            (
                vec!["t".to_owned()],
                vec!["id".to_owned(), "name".to_owned()]
            )
        );
    }

    #[test]
    fn reports_errors_and_continues() {
        let mut s = session();
        s.feed_text("SELECT * FROM missing; CREATE TABLE ok (x);");
        assert_eq!(s.errors, 1);
        assert_eq!(s.schema_names().0, vec!["ok".to_owned()]);
    }

    #[test]
    fn affected_rows_are_counted() {
        let mut s = session();
        s.feed_text("CREATE TABLE t (x); INSERT INTO t VALUES (1), (2), (3);");
        s.db.execute_batch("DELETE FROM t WHERE x > 1").unwrap();
        assert_eq!(s.affected_rows(), Some(2));
    }

    #[test]
    fn tsql_batches_run_whole() {
        let mut s = session();
        s.feed_text(
            "CREATE TABLE t (x INT);\n\
             DECLARE @i INT = 0;\n\
             WHILE @i < 3 BEGIN\n\
               INSERT INTO t VALUES (@i);\n\
               SET @i = @i + 1;\n\
             END;\n\
             SELECT @i;\n\
             GO\n\
             INSERT INTO t VALUES (10);",
        );
        assert_eq!(s.errors, 0);
        assert!(!s.batch);
        let r =
            s.db.query("SELECT count(*) AS n, max(x) AS m FROM t")
                .unwrap();
        assert_eq!(r.rows[0]["n"], 4);
        assert_eq!(r.rows[0]["m"], 10);
    }

    #[test]
    fn tsql_batch_waits_for_go_interactively() {
        let mut s = session();
        s.feed_line("CREATE TABLE t (x INT); DECLARE @v INT = 7;");
        assert!(s.batch);
        s.feed_line("INSERT INTO t VALUES (@v);");
        assert!(s.db.query("SELECT * FROM t").unwrap().rows.is_empty());
        s.feed_line("GO");
        assert!(!s.batch);
        assert_eq!(s.errors, 0);
        assert_eq!(s.db.query("SELECT x FROM t").unwrap().rows[0]["x"], 7);
    }

    #[test]
    fn tsql_try_catch_and_errors_are_clean() {
        let mut s = session();
        s.feed_text(
            "CREATE TABLE t (x INT);\n\
             BEGIN TRY\n  INSERT INTO t VALUES (1);\n  THROW 50001, 'boom', 1;\n\
             END TRY\nBEGIN CATCH\n  INSERT INTO t VALUES (2);\nEND CATCH\nGO\n\
             DECLARE @x INT = 1;\nTHROW 50002, 'fatal', 1;\nGO",
        );
        assert_eq!(s.errors, 1);
        let r = s.db.query("SELECT x FROM t ORDER BY x").unwrap();
        assert!(!r.rows.is_empty());
    }

    #[test]
    fn explicit_transactions_work() {
        let mut s = session();
        s.feed_text("CREATE TABLE t (x); BEGIN; INSERT INTO t VALUES (1);");
        assert!(s.in_transaction);
        s.feed_text("ROLLBACK;");
        assert!(!s.in_transaction);
        assert_eq!(s.errors, 0);
        assert!(s.db.query("SELECT * FROM t").unwrap().rows.is_empty());
    }
}
