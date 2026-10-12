//! Dot (`.tables`) and psql-style backslash (`\dt`) console commands.

use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use super::csv::{build_import_sql, parse_csv, quote_literal};
use super::render::{self, OutputMode};
use super::{Flow, Session, format_duration, sql};

/// SQL predicate (on `name`) that hides SQLite and NovaDB internal objects.
pub const USER_OBJECT_FILTER: &str =
    "name NOT LIKE 'sqlite\\_%' ESCAPE '\\' AND name NOT LIKE '\\_novadb\\_%' ESCAPE '\\'";

/// All command names, for completion.
pub const COMMAND_NAMES: &[&str] = &[
    ".clear", ".exit", ".export", ".help", ".import", ".indexes", ".mode", ".quit", ".read",
    ".schema", ".tables", ".timer", "\\?", "\\d", "\\di", "\\dt", "\\i", "\\q", "\\timing", "\\x",
];

/// Commands whose first argument is a file path.
pub const FILE_COMMANDS: &[&str] = &[".read", "\\i", ".import", ".export"];

/// Commands whose argument is a table name.
pub const TABLE_COMMANDS: &[&str] = &[".schema", "\\d", ".indexes", "\\di"];

const HELP: &[(&str, &str)] = &[
    (".tables, \\dt", "List tables and views"),
    (
        ".schema [TABLE]",
        "Show CREATE statements, or describe TABLE",
    ),
    (
        "\\d [TABLE]",
        "List tables, or describe TABLE (columns, keys, indexes)",
    ),
    (".indexes [TABLE], \\di", "List indexes"),
    (".mode [table|line|json|csv]", "Set or show the output mode"),
    ("\\x", "Toggle expanded (line) output"),
    (".timer on|off, \\timing", "Show query execution time"),
    (
        ".read FILE, \\i FILE",
        "Execute SQL (and commands) from FILE",
    ),
    (
        ".import FILE TABLE",
        "Import CSV/TSV into TABLE (created if missing)",
    ),
    (
        ".export FILE QUERY",
        "Write QUERY results to FILE (.csv or .json)",
    ),
    (".clear", "Clear the screen"),
    (".help, \\?", "Show this help"),
    (".quit, .exit, \\q", "Exit (or press Ctrl+D)"),
];

/// Splits off the first (optionally quoted) argument, returning it and the rest.
fn next_arg(input: &str) -> Option<(String, &str)> {
    let input = input.trim_start();
    if input.is_empty() {
        return None;
    }
    let first = input.chars().next()?;
    if first == '"' || first == '\'' {
        let end = input[1..].find(first)? + 1;
        return Some((input[1..end].to_owned(), &input[end + 1..]));
    }
    let end = input.find(char::is_whitespace).unwrap_or(input.len());
    Some((input[..end].to_owned(), &input[end..]))
}

impl Session {
    pub(crate) fn run_command(&mut self, line: &str) -> Flow {
        let line = line.trim().trim_end_matches(';').trim_end();
        let (command, rest) = match line.find(char::is_whitespace) {
            Some(i) => (&line[..i], line[i..].trim()),
            None => (line, ""),
        };
        let result = match command {
            ".quit" | ".exit" | ".q" | "\\q" => return Flow::Quit,
            ".help" | ".h" | "\\?" | "\\h" => {
                self.help();
                Ok(())
            }
            ".tables" | "\\dt" => self.list_tables(),
            ".schema" if rest.is_empty() => self.show_all_schema(),
            "\\d" if rest.is_empty() => self.list_tables(),
            ".schema" | "\\d" => next_arg(rest)
                .ok_or_else(|| anyhow!("missing table name"))
                .and_then(|(t, _)| self.describe(&t)),
            ".indexes" | ".indices" | "\\di" => self.list_indexes(next_arg(rest).map(|a| a.0)),
            ".timer" | "\\timing" => self.set_timer(rest),
            ".mode" => self.set_mode(rest),
            "\\x" => {
                self.mode = if self.mode == OutputMode::Line {
                    OutputMode::Table
                } else {
                    OutputMode::Line
                };
                println!("Output mode is {}.", self.out.bold(self.mode.name()));
                Ok(())
            }
            ".read" | "\\i" => match next_arg(rest) {
                Some((file, _)) => return self.read_file(&file),
                None => Err(anyhow!("usage: .read FILE")),
            },
            ".import" => self.import(rest),
            ".export" => self.export(rest),
            ".clear" => {
                if io::stdout().is_terminal() {
                    print!("\x1b[2J\x1b[3J\x1b[H");
                    let _ = io::stdout().flush();
                }
                Ok(())
            }
            other => Err(anyhow!(
                "unknown command `{other}`. Type .help for available commands."
            )),
        };
        if let Err(e) = result {
            self.print_error(format!("{e:#}"));
        }
        Flow::Continue
    }

    fn help(&self) {
        let s = self.out;
        println!("{}", s.header("Commands"));
        let width = HELP.iter().map(|(c, _)| c.len()).max().unwrap_or(0);
        for (cmd, desc) in HELP {
            println!("  {}  {desc}", s.bold(&format!("{cmd:<width$}")));
        }
        println!();
        println!("{}", s.header("Tips"));
        println!("  End SQL statements with ; — several statements may share a line.");
        println!(
            "  BEGIN / COMMIT / ROLLBACK run an explicit transaction (prompt shows novadb*>)."
        );
        println!("  Tab completes keywords, tables, columns and commands; Ctrl+C clears input.");
        println!("  History is saved in ~/.novadb_history. Colors honor NO_COLOR.");
    }

    fn set_timer(&mut self, arg: &str) -> Result<()> {
        self.timer = match arg.to_ascii_lowercase().as_str() {
            "" => !self.timer,
            "on" | "1" | "true" => true,
            "off" | "0" | "false" => false,
            other => bail!("expected on|off, got `{other}`"),
        };
        println!(
            "Timer is {}.",
            self.out.bold(if self.timer { "on" } else { "off" })
        );
        Ok(())
    }

    fn set_mode(&mut self, arg: &str) -> Result<()> {
        if arg.is_empty() {
            println!(
                "Output mode is {} (available: table, line, json, csv).",
                self.out.bold(self.mode.name())
            );
            return Ok(());
        }
        self.mode = OutputMode::parse(arg)
            .ok_or_else(|| anyhow!("unknown mode `{arg}`; use table, line, json or csv"))?;
        println!("Output mode is {}.", self.out.bold(self.mode.name()));
        Ok(())
    }

    fn print_table(&self, columns: &[&str], rows: &[Value]) {
        let columns: Vec<String> = columns.iter().map(|c| (*c).to_owned()).collect();
        print!("{}", render::render_table(&columns, rows, self.out));
    }

    fn list_tables(&mut self) -> Result<()> {
        let result = self.db.query(&format!(
            "SELECT name, type, (SELECT count(*) FROM pragma_table_info(m.name)) AS columns \
             FROM sqlite_master m WHERE type IN ('table','view') AND {USER_OBJECT_FILTER} ORDER BY name"
        ))?;
        if result.rows.is_empty() {
            println!("{}", self.out.dim("(no tables)"));
        } else {
            self.print_table(&["name", "type", "columns"], &result.rows);
        }
        Ok(())
    }

    fn show_all_schema(&mut self) -> Result<()> {
        let result = self.db.query(&format!(
            "SELECT sql FROM sqlite_master WHERE sql IS NOT NULL AND {USER_OBJECT_FILTER} \
             AND tbl_name NOT LIKE '\\_novadb\\_%' ESCAPE '\\' \
             ORDER BY CASE type WHEN 'table' THEN 0 WHEN 'view' THEN 1 WHEN 'index' THEN 2 ELSE 3 END, name"
        ))?;
        if result.rows.is_empty() {
            println!("{}", self.out.dim("(empty schema)"));
        }
        for row in &result.rows {
            if let Some(text) = row["sql"].as_str() {
                let shown = if self.out.color {
                    sql::highlight(text)
                } else {
                    text.to_owned()
                };
                println!("{shown};");
            }
        }
        Ok(())
    }

    fn describe(&mut self, table: &str) -> Result<()> {
        let lit = quote_literal(table);
        let object = self.db.query(&format!(
            "SELECT name, type, sql FROM sqlite_master \
             WHERE type IN ('table','view') AND name = {lit} COLLATE NOCASE"
        ))?;
        let Some(object) = object.rows.first() else {
            bail!("no such table or view: {table}");
        };
        let name = object["name"].as_str().unwrap_or(table).to_owned();
        let kind = object["type"].as_str().unwrap_or("table");
        let lit = quote_literal(&name);

        let columns = self.db.query(&format!(
            "SELECT name, type, \"notnull\" AS nn, dflt_value, pk FROM pragma_table_info({lit}) ORDER BY cid"
        ))?;
        let s = self.out;
        let title = if kind == "view" { "View" } else { "Table" };
        println!("{} {}", s.header(title), s.bold(&format!("\"{name}\"")));
        let rows: Vec<Value> = columns
            .rows
            .iter()
            .map(|c| {
                let pk = c["pk"].as_i64().unwrap_or(0);
                json!({
                    "column": c["name"],
                    "type": c["type"].as_str().filter(|t| !t.is_empty()).unwrap_or("ANY"),
                    "nullable": if c["nn"].as_i64() == Some(1) || pk > 0 && kind == "table" { "not null" } else { "" },
                    "default": c["dflt_value"],
                    "key": match pk { 0 => String::new(), 1 if columns.rows.iter().filter(|r| r["pk"].as_i64().unwrap_or(0) > 0).count() == 1 => "PK".to_owned(), n => format!("PK({n})") },
                })
            })
            .collect();
        self.print_table(&["column", "type", "nullable", "default", "key"], &rows);

        let indexes = self.db.query(&format!(
            "SELECT il.name AS name, il.\"unique\" AS uniq, il.origin AS origin, \
             (SELECT group_concat(ii.name, ', ') FROM pragma_index_info(il.name) ii) AS cols \
             FROM pragma_index_list({lit}) il ORDER BY il.name"
        ))?;
        if !indexes.rows.is_empty() {
            println!("{}", s.header("Indexes:"));
            for index in &indexes.rows {
                let mut flags = Vec::new();
                if index["uniq"].as_i64() == Some(1) {
                    flags.push("UNIQUE");
                }
                match index["origin"].as_str() {
                    Some("pk") => flags.push("PRIMARY KEY"),
                    Some("u") => flags.push("constraint"),
                    _ => {}
                }
                println!(
                    "  • {} ({}) {}",
                    s.bold(index["name"].as_str().unwrap_or("?")),
                    index["cols"].as_str().unwrap_or(""),
                    s.dim(&flags.join(", "))
                );
            }
        }

        let fks = self.db.query(&format!(
            "SELECT \"from\" AS col, \"table\" AS ref, \"to\" AS ref_col FROM pragma_foreign_key_list({lit}) ORDER BY id, seq"
        ))?;
        if !fks.rows.is_empty() {
            println!("{}", s.header("Foreign keys:"));
            for fk in &fks.rows {
                println!(
                    "  • {} → {}({})",
                    fk["col"].as_str().unwrap_or("?"),
                    fk["ref"].as_str().unwrap_or("?"),
                    render::value_text(fk.get("ref_col"))
                );
            }
        }
        Ok(())
    }

    fn list_indexes(&mut self, table: Option<String>) -> Result<()> {
        let filter = match &table {
            Some(t) => format!("AND m.name = {} COLLATE NOCASE", quote_literal(t)),
            None => String::new(),
        };
        let result = self.db.query(&format!(
            "SELECT il.name AS \"index\", m.name AS \"table\", \
             CASE WHEN il.\"unique\" THEN 'yes' ELSE '' END AS \"unique\", \
             (SELECT group_concat(ii.name, ', ') FROM pragma_index_info(il.name) ii) AS columns \
             FROM sqlite_master m JOIN pragma_index_list(m.name) il \
             WHERE m.type = 'table' AND {} {filter} ORDER BY m.name, il.name",
            USER_OBJECT_FILTER.replace("name", "m.name")
        ))?;
        if result.rows.is_empty() {
            println!("{}", self.out.dim("(no indexes)"));
        } else {
            self.print_table(&["index", "table", "unique", "columns"], &result.rows);
        }
        Ok(())
    }

    fn read_file(&mut self, file: &str) -> Flow {
        if self.read_depth >= 16 {
            self.print_error(".read nested too deeply");
            return Flow::Continue;
        }
        let content = match std::fs::read_to_string(file) {
            Ok(c) => c,
            Err(e) => {
                self.print_error(format!("cannot read '{file}': {e}"));
                return Flow::Continue;
            }
        };
        let saved = std::mem::take(&mut self.buffer);
        let errors_before = self.errors;
        let start = Instant::now();
        self.read_depth += 1;
        self.quiet += 1;
        let flow = self.feed_text(&content);
        self.quiet -= 1;
        self.read_depth -= 1;
        self.buffer = saved;
        self.schema_dirty = true;
        let failed = self.errors - errors_before;
        if self.interactive && self.quiet == 0 {
            let summary = format!("Read '{file}'");
            if failed == 0 {
                self.print_ok(&summary, Some(start.elapsed()));
            } else {
                eprintln!(
                    "{}",
                    self.err.yellow(&format!("{summary}: {failed} error(s)"))
                );
            }
        }
        flow
    }

    fn table_exists(&self, table: &str) -> Result<bool> {
        let result = self.db.query(&format!(
            "SELECT 1 AS x FROM sqlite_master WHERE type = 'table' AND name = {} COLLATE NOCASE",
            quote_literal(table)
        ))?;
        Ok(!result.rows.is_empty())
    }

    fn import(&mut self, args: &str) -> Result<()> {
        let (file, rest) = next_arg(args).ok_or_else(|| anyhow!("usage: .import FILE TABLE"))?;
        let (table, _) = next_arg(rest).ok_or_else(|| anyhow!("usage: .import FILE TABLE"))?;
        let start = Instant::now();
        let text =
            std::fs::read_to_string(&file).with_context(|| format!("cannot read '{file}'"))?;
        let delimiter = match Path::new(&file).extension().and_then(|e| e.to_str()) {
            Some(ext) if ext.eq_ignore_ascii_case("tsv") || ext.eq_ignore_ascii_case("tab") => '\t',
            _ => ',',
        };
        let records =
            parse_csv(&text, delimiter).with_context(|| format!("invalid CSV in '{file}'"))?;
        let create = !self.table_exists(&table)?;
        let import_sql = build_import_sql(&table, &records, create)?;
        self.execute(&import_sql, "INSERT")?;
        self.schema_dirty |= create;
        let rows = records.len().saturating_sub(1);
        let mut message = format!(
            "Imported {rows} row{} into {table}",
            if rows == 1 { "" } else { "s" }
        );
        if create {
            message.push_str(" (new table)");
        }
        let timing = if self.timer {
            self.out
                .dim(&format!(" ({})", format_duration(start.elapsed())))
        } else {
            String::new()
        };
        println!("{}{timing}", self.out.green(&message));
        Ok(())
    }

    fn export(&mut self, args: &str) -> Result<()> {
        let (file, query) =
            next_arg(args).ok_or_else(|| anyhow!("usage: .export FILE.(csv|json) QUERY"))?;
        let query = query.trim().trim_end_matches(';').trim();
        if query.is_empty() {
            bail!("usage: .export FILE.(csv|json) QUERY");
        }
        let result = self.db.query(query)?;
        let ext = Path::new(&file)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let content = match ext.as_str() {
            "json" => render::render_json(&result.columns, &result.rows),
            "csv" | "" => render::render_csv(&result.columns, &result.rows),
            other => bail!("unsupported export format `.{other}`; use .csv or .json"),
        };
        std::fs::write(&file, content).with_context(|| format!("cannot write '{file}'"))?;
        let n = result.rows.len();
        println!(
            "{}",
            self.out.green(&format!(
                "Exported {n} row{} to {file}",
                if n == 1 { "" } else { "s" }
            ))
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::render::Style;
    use super::*;
    use novadb_core::NovaDb;

    fn session() -> Session {
        Session::new(
            NovaDb::open_in_memory().unwrap(),
            Style { color: false },
            Style { color: false },
            false,
        )
    }

    #[test]
    fn parses_quoted_arguments() {
        assert_eq!(
            next_arg("  \"my file.csv\" tbl"),
            Some(("my file.csv".to_owned(), " tbl"))
        );
        assert_eq!(next_arg("a.csv"), Some(("a.csv".to_owned(), "")));
        assert_eq!(next_arg("   "), None);
    }

    #[test]
    fn import_export_and_describe_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let csv_path = dir.path().join("people.csv");
        std::fs::write(&csv_path, "id,name,score\n1,\"Doe, J\",9.5\n2,O'Neil,\n").unwrap();
        let mut s = session();
        s.feed_line(&format!(".import '{}' people", csv_path.display()));
        assert_eq!(s.errors, 0);
        let r = s.db.query("SELECT * FROM people ORDER BY id").unwrap();
        assert_eq!(r.rows.len(), 2);
        assert_eq!(r.rows[0]["id"], 1);
        assert_eq!(r.rows[0]["name"], "Doe, J");
        assert_eq!(r.rows[0]["score"], 9.5);
        assert!(r.rows[1]["score"].is_null());

        // Describe with a name needing quoting must not break the SQL.
        s.feed_line(".schema people");
        s.feed_line("\\d x'); DROP TABLE people; --");
        assert_eq!(s.errors, 1, "describe of unknown table is an error");
        assert!(s.table_exists("people").unwrap());

        let out = dir.path().join("out.json");
        s.feed_line(&format!(
            ".export {} SELECT id, name FROM people ORDER BY id;",
            out.display()
        ));
        let json = std::fs::read_to_string(&out).unwrap();
        assert!(json.contains("{\"id\": 1, \"name\": \"Doe, J\"}"), "{json}");

        let sql_path = dir.path().join("script.sql");
        std::fs::write(
            &sql_path,
            ".mode csv\nINSERT INTO people (id) VALUES (3);\n",
        )
        .unwrap();
        s.feed_line(&format!(".read {}", sql_path.display()));
        assert_eq!(s.mode, OutputMode::Csv);
        assert_eq!(s.db.query("SELECT * FROM people").unwrap().rows.len(), 3);
        assert_eq!(s.errors, 1);
    }

    #[test]
    fn unknown_commands_are_errors_and_quit_quits() {
        let mut s = session();
        assert_eq!(s.feed_line(".nope"), Flow::Continue);
        assert_eq!(s.errors, 1);
        assert_eq!(s.feed_line("\\q"), Flow::Quit);
    }
}
