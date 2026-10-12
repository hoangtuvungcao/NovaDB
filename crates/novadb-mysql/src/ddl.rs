//! Table DDL helpers: a small scanner for stored SQLite `CREATE TABLE` text,
//! the `SHOW CREATE TABLE` renderer (MySQL format) and the table-rebuild
//! planner used for `ALTER TABLE` forms SQLite cannot perform in place.
//!
//! The scanner works on raw text rather than the token stream because the
//! stored DDL is SQLite SQL (`"quoted"` identifiers) and must be edited and
//! written back byte-for-byte.

use std::fmt::Write as _;

use crate::lexer::{quote_ident, quote_str};

// ---------------------------------------------------------------------------
// Raw DDL scanner
// ---------------------------------------------------------------------------

/// A `CREATE TABLE` statement split into head, items and tail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateParts {
    /// Text before the opening parenthesis (`CREATE TABLE "t" `).
    pub head: String,
    /// Column definitions and table constraints, trimmed.
    pub items: Vec<String>,
    /// Text after the closing parenthesis (`WITHOUT ROWID`, ...).
    pub tail: String,
}

impl CreateParts {
    /// Re-assembles the statement under `table`.
    pub fn render(&self, table: &str) -> String {
        format!(
            "CREATE TABLE {} (\n  {}\n){}",
            quote_ident(table),
            self.items.join(",\n  "),
            self.tail
        )
    }
}

/// Index just past the quoted run starting at `start`, or `None` when
/// `bytes[start]` does not open a quote/comment.
fn skip_quoted(bytes: &[u8], start: usize) -> Option<usize> {
    let quote = match bytes[start] {
        b'\'' | b'"' | b'`' => bytes[start],
        b'[' => b']',
        b'-' if bytes.get(start + 1) == Some(&b'-') => {
            let end = bytes[start..].iter().position(|&b| b == b'\n');
            return Some(end.map_or(bytes.len(), |e| start + e + 1));
        }
        b'/' if bytes.get(start + 1) == Some(&b'*') => {
            let mut i = start + 2;
            while i + 1 < bytes.len() {
                if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    return Some(i + 2);
                }
                i += 1;
            }
            return Some(bytes.len());
        }
        _ => return None,
    };
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == quote {
            if quote != b']' && bytes.get(i + 1) == Some(&quote) {
                i += 2;
                continue;
            }
            return Some(i + 1);
        }
        i += 1;
    }
    Some(bytes.len())
}

const ANNOTATION_OPEN: &str = "/*my:";

/// Comment recording a column's declared MySQL type inside the stored DDL.
///
/// Only written for rowid-alias columns (`INTEGER PRIMARY KEY AUTOINCREMENT`,
/// whose SQLite type must be exactly `INTEGER`). Tables created by older
/// gateway versions carry it on every column; [`annotated_type`] still reads
/// both.
pub fn annotation(declared_type: &str) -> String {
    let safe = declared_type.replace("*/", "");
    format!("{ANNOTATION_OPEN}{}*/", safe.trim())
}

/// Declared MySQL type recorded by [`annotation`] in a column item.
pub fn annotated_type(item: &str) -> Option<String> {
    let start = item.find(ANNOTATION_OPEN)? + ANNOTATION_OPEN.len();
    let end = item[start..].find("*/")? + start;
    Some(item[start..end].to_owned())
}

/// Declared MySQL types of the columns in a stored `CREATE TABLE`
/// (lowercase column name -> type); columns without annotation are omitted.
pub fn annotated_column_types(create_sql: &str) -> Vec<(String, String)> {
    let Some(parts) = split_create(create_sql) else {
        return Vec::new();
    };
    parts
        .items
        .iter()
        .filter_map(|item| match classify(item)?.kind {
            ItemKind::Column(name) => Some((name.to_lowercase(), annotated_type(item)?)),
            _ => None,
        })
        .collect()
}

/// Type words following a column name in a stored column definition.
fn sqlite_declared_type(item: &str) -> Option<String> {
    const STOP: &[&str] = &[
        "PRIMARY",
        "NOT",
        "NULL",
        "DEFAULT",
        "UNIQUE",
        "CHECK",
        "REFERENCES",
        "COLLATE",
        "GENERATED",
        "AS",
        "CONSTRAINT",
        "AUTOINCREMENT",
    ];
    let (_, _, rest) = read_ident(item)?;
    let mut words: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut depth = 0;
    let rest = rest.trim_start();
    let end = rest.find("/*").unwrap_or(rest.len());
    for c in rest[..end].chars() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        if c.is_whitespace() && depth == 0 {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
        } else {
            current.push(c);
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    let kept: Vec<String> = words
        .into_iter()
        .take_while(|w| !STOP.contains(&w.to_ascii_uppercase().as_str()))
        .collect();
    (!kept.is_empty()).then(|| kept.join(" "))
}

/// Declared type of every column of a stored `CREATE TABLE`: the annotated
/// MySQL type, else the type written in the SQLite DDL.
pub fn declared_column_types(create_sql: &str) -> Vec<(String, String)> {
    let Some(parts) = split_create(create_sql) else {
        return Vec::new();
    };
    parts
        .items
        .iter()
        .filter_map(|item| match classify(item)?.kind {
            ItemKind::Column(name) => Some((
                name.to_lowercase(),
                annotated_type(item).or_else(|| sqlite_declared_type(item))?,
            )),
            _ => None,
        })
        .collect()
}

/// Splits `sql` at top-level commas inside the first parenthesis pair.
pub fn split_create(sql: &str) -> Option<CreateParts> {
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if let Some(end) = skip_quoted(bytes, i) {
            i = end;
        } else if bytes[i] == b'(' {
            break;
        } else {
            i += 1;
        }
    }
    let open = i;
    if open >= bytes.len() {
        return None;
    }
    let mut depth = 0_i32;
    let mut start = open + 1;
    let mut items = Vec::new();
    i = open;
    while i < bytes.len() {
        if let Some(end) = skip_quoted(bytes, i) {
            i = end;
            continue;
        }
        match bytes[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    items.push(sql[start..i].trim().to_owned());
                    return Some(CreateParts {
                        head: sql[..open].to_owned(),
                        items: items.into_iter().filter(|item| !item.is_empty()).collect(),
                        tail: sql[i + 1..].to_owned(),
                    });
                }
            }
            b',' if depth == 1 => {
                items.push(sql[start..i].trim().to_owned());
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Reads an identifier at the start of `text` (after whitespace). Returns
/// the name, whether it was quoted, and the remaining text.
pub fn read_ident(text: &str) -> Option<(String, bool, &str)> {
    let text = text.trim_start();
    let first = text.chars().next()?;
    let (close, quoted) = match first {
        '"' => ('"', true),
        '`' => ('`', true),
        '[' => (']', true),
        _ => ('\0', false),
    };
    if quoted {
        let body = &text[1..];
        let mut name = String::new();
        let mut chars = body.char_indices().peekable();
        while let Some((index, c)) = chars.next() {
            if c == close {
                if close != ']' && chars.peek().is_some_and(|(_, n)| *n == close) {
                    name.push(c);
                    chars.next();
                    continue;
                }
                return Some((name, true, &body[index + 1..]));
            }
            name.push(c);
        }
        return None;
    }
    let end = text
        .char_indices()
        .find(|(_, c)| !(c.is_alphanumeric() || *c == '_' || *c == '$'))
        .map_or(text.len(), |(index, _)| index);
    (end > 0).then(|| (text[..end].to_owned(), false, &text[end..]))
}

/// Names inside the first parenthesised list of `text`.
fn paren_names(text: &str) -> Vec<String> {
    let Some(open) = text.find('(') else {
        return Vec::new();
    };
    let bytes = text.as_bytes();
    let mut depth = 0;
    let mut i = open;
    let mut close = text.len();
    while i < bytes.len() {
        if let Some(end) = skip_quoted(bytes, i) {
            i = end;
            continue;
        }
        match bytes[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    close = i;
                    break;
                }
            }
            _ => {}
        }
        i += 1;
    }
    let inner = &text[open + 1..close];
    let mut names = Vec::new();
    let inner_parts = split_create(&format!("({inner})")).map_or_else(Vec::new, |p| p.items);
    for part in inner_parts {
        if let Some((name, _, _)) = read_ident(&part) {
            names.push(name);
        }
    }
    names
}

/// What a `CREATE TABLE` item is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemKind {
    Column(String),
    PrimaryKey(Vec<String>),
    Unique(Vec<String>),
    Check,
    ForeignKey { columns: Vec<String> },
}

/// A classified item with its optional `CONSTRAINT name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub name: Option<String>,
    pub kind: ItemKind,
}

fn keyword(text: &str) -> (String, &str) {
    match read_ident(text) {
        Some((word, false, rest)) => (word.to_ascii_uppercase(), rest),
        _ => (String::new(), text),
    }
}

/// Classifies one item of a `CREATE TABLE` body.
pub fn classify(item: &str) -> Option<Item> {
    let (word, rest) = keyword(item);
    let (name, word, rest) = if word == "CONSTRAINT" {
        let (name, _, after) = read_ident(rest)?;
        let (word, rest) = keyword(after);
        (Some(name), word, rest)
    } else {
        (None, word, rest)
    };
    let kind = match word.as_str() {
        "PRIMARY" => ItemKind::PrimaryKey(paren_names(rest)),
        "UNIQUE" => ItemKind::Unique(paren_names(rest)),
        "CHECK" => ItemKind::Check,
        "FOREIGN" => ItemKind::ForeignKey {
            columns: paren_names(rest),
        },
        _ if name.is_some() => return None,
        _ => {
            let (column, _, _) = read_ident(item)?;
            ItemKind::Column(column)
        }
    };
    Some(Item { name, kind })
}

/// MySQL-style names of the table-level foreign keys of a stored `CREATE
/// TABLE`, in declaration order: the declared `CONSTRAINT name`, or
/// `<table>_ibfk_<n>` for unnamed ones. Returns `(name, columns)` pairs.
pub fn foreign_key_names(table: &str, parts: &CreateParts) -> Vec<(String, Vec<String>)> {
    let mut result = Vec::new();
    for item in &parts.items {
        if let Some(Item {
            name,
            kind: ItemKind::ForeignKey { columns },
        }) = classify(item)
        {
            let name = name.unwrap_or_else(|| format!("{table}_ibfk_{}", result.len() + 1));
            result.push((name, columns));
        }
    }
    result
}

/// Names of the foreign keys of `table` as `(SQLite foreign key id, name)`.
///
/// `foreign_keys` lists each key's id and local columns in id order (from
/// `pragma_foreign_key_list`). A key whose columns match a declared
/// `CONSTRAINT name FOREIGN KEY (..)` takes that name, the others are numbered
/// `<table>_ibfk_<n>` after the declared ones, exactly as `SHOW CREATE TABLE`
/// reports them.
pub fn resolve_foreign_key_names(
    table: &str,
    create_sql: &str,
    foreign_keys: &[(i64, Vec<String>)],
) -> Vec<(i64, String)> {
    let declared = split_create(create_sql)
        .map(|parts| foreign_key_names(table, &parts))
        .unwrap_or_default();
    let mut next = declared.len();
    foreign_keys
        .iter()
        .map(|(id, columns)| {
            let lower: Vec<String> = columns.iter().map(|c| c.to_lowercase()).collect();
            let matched = declared.iter().find(|(_, declared_columns)| {
                declared_columns
                    .iter()
                    .map(|c| c.to_lowercase())
                    .collect::<Vec<_>>()
                    == lower
            });
            let name = if let Some((name, _)) = matched {
                name.clone()
            } else {
                next += 1;
                format!("{table}_ibfk_{next}")
            };
            (*id, name)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// SHOW CREATE TABLE
// ---------------------------------------------------------------------------

/// One column as reported by `pragma_table_info`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnMeta {
    pub name: String,
    /// Declared type to display (the annotated MySQL type when known).
    pub declared_type: String,
    /// Type as stored by SQLite (`INTEGER` marks a rowid alias).
    pub sqlite_type: String,
    pub not_null: bool,
    pub default: Option<String>,
    /// 1-based position in the primary key, 0 when not part of it.
    pub pk: i64,
}

/// An index: columns in order. Expression columns are omitted by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexMeta {
    pub name: String,
    pub unique: bool,
    pub columns: Vec<String>,
}

/// A foreign key constraint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKeyMeta {
    pub name: String,
    pub columns: Vec<String>,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
    pub on_update: String,
    pub on_delete: String,
}

fn backtick(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

fn backtick_list(names: &[String]) -> String {
    names
        .iter()
        .map(|n| backtick(n))
        .collect::<Vec<_>>()
        .join(",")
}

/// Maps a declared SQLite column type to the MySQL type shown in DDL.
pub fn mysql_type(declared: &str) -> String {
    let declared = declared.trim();
    let upper = declared.to_ascii_uppercase();
    let (base, args) = match upper.find('(') {
        Some(open) => (
            upper[..open].trim().to_owned(),
            upper[open..].trim().to_owned(),
        ),
        None => (upper.clone(), String::new()),
    };
    let base = base.as_str();
    let args = args.as_str();
    let with = |default: &str| {
        if args.is_empty() {
            default.to_owned()
        } else {
            args.replace(' ', "")
        }
    };
    match base {
        "" => "text".into(),
        "BOOLEAN" | "BOOL" => "tinyint(1)".into(),
        "TINYINT" => {
            if args == "(1)" {
                "tinyint(1)".into()
            } else {
                "tinyint".into()
            }
        }
        "SMALLINT" | "INT2" => "smallint".into(),
        "MEDIUMINT" => "mediumint".into(),
        "BIGINT" | "INT8" | "UNSIGNED BIG INT" => "bigint".into(),
        "INT" | "INTEGER" | "INT4" => "int".into(),
        "VARCHAR" | "NVARCHAR" | "CHARACTER VARYING" | "VARYING CHARACTER" | "NATIVE CHARACTER" => {
            format!("varchar{}", with("(255)"))
        }
        "CHAR" | "NCHAR" | "CHARACTER" => format!("char{}", with("(1)")),
        "TINYTEXT" => "tinytext".into(),
        "MEDIUMTEXT" => "mediumtext".into(),
        "LONGTEXT" | "CLOB" => "longtext".into(),
        "TEXT" | "STRING" => "text".into(),
        "JSON" => "json".into(),
        "DATETIME" => "datetime".into(),
        "TIMESTAMP" => "timestamp".into(),
        "DATE" => "date".into(),
        "TIME" => "time".into(),
        "YEAR" => "year".into(),
        "DECIMAL" | "NUMERIC" | "DEC" | "FIXED" => format!("decimal{}", with("(10,0)")),
        "FLOAT" => "float".into(),
        "DOUBLE" | "DOUBLE PRECISION" | "REAL" => "double".into(),
        "TINYBLOB" => "tinyblob".into(),
        "MEDIUMBLOB" => "mediumblob".into(),
        "LONGBLOB" => "longblob".into(),
        "BLOB" => "blob".into(),
        "BINARY" => format!("binary{}", with("(1)")),
        "VARBINARY" => format!("varbinary{}", with("(255)")),
        other if other.contains("INT") => "bigint".into(),
        other if other.contains("CHAR") || other.contains("CLOB") || other.contains("TEXT") => {
            "text".into()
        }
        other if other.contains("BLOB") => "blob".into(),
        other if other.contains("REAL") || other.contains("FLOA") || other.contains("DOUB") => {
            "double".into()
        }
        _ => "decimal(10,0)".into(),
    }
}

/// Renders a SQLite default expression the way MySQL prints it.
fn mysql_default(expression: &str) -> String {
    let trimmed = expression.trim();
    let upper = trimmed.to_ascii_uppercase();
    let is_number = trimmed.parse::<f64>().is_ok();
    if trimmed.starts_with('\'') || is_number || upper == "NULL" {
        trimmed.to_owned()
    } else if upper == "TRUE" {
        "'1'".to_owned()
    } else if upper == "FALSE" {
        "'0'".to_owned()
    } else if matches!(
        upper.as_str(),
        "CURRENT_TIMESTAMP" | "CURRENT_DATE" | "CURRENT_TIME"
    ) {
        if upper == "CURRENT_TIMESTAMP" {
            "CURRENT_TIMESTAMP".to_owned()
        } else {
            format!("({})", upper.to_ascii_lowercase())
        }
    } else if trimmed.starts_with('(') {
        trimmed.to_owned()
    } else {
        format!("({trimmed})")
    }
}

/// Reconstructs MySQL-format `SHOW CREATE TABLE` output.
pub fn render_show_create(
    table: &str,
    columns: &[ColumnMeta],
    indexes: &[IndexMeta],
    foreign_keys: &[ForeignKeyMeta],
) -> String {
    let mut pk: Vec<&ColumnMeta> = columns.iter().filter(|c| c.pk > 0).collect();
    pk.sort_by_key(|c| c.pk);
    let rowid_alias = pk.len() == 1 && pk[0].sqlite_type.trim().eq_ignore_ascii_case("INTEGER");
    let mut lines: Vec<String> = Vec::new();
    for column in columns {
        let mut line = format!(
            "{} {}",
            backtick(&column.name),
            mysql_type(&column.declared_type)
        );
        let is_alias = rowid_alias && column.pk > 0;
        if column.not_null || column.pk > 0 {
            line.push_str(" NOT NULL");
        }
        if is_alias {
            line.push_str(" AUTO_INCREMENT");
        } else {
            match &column.default {
                Some(default) => {
                    let _ = write!(line, " DEFAULT {}", mysql_default(default));
                }
                None if !column.not_null && column.pk == 0 => line.push_str(" DEFAULT NULL"),
                None => {}
            }
        }
        lines.push(line);
    }
    if !pk.is_empty() {
        let names: Vec<String> = pk.iter().map(|c| c.name.clone()).collect();
        lines.push(format!("PRIMARY KEY ({})", backtick_list(&names)));
    }
    let mut sorted: Vec<&IndexMeta> = indexes.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    for index in sorted {
        lines.push(format!(
            "{}KEY {} ({})",
            if index.unique { "UNIQUE " } else { "" },
            backtick(&index.name),
            backtick_list(&index.columns)
        ));
    }
    for fk in foreign_keys {
        let mut line = format!(
            "CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({})",
            backtick(&fk.name),
            backtick_list(&fk.columns),
            backtick(&fk.ref_table),
            backtick_list(&fk.ref_columns)
        );
        for (clause, action) in [("DELETE", &fk.on_delete), ("UPDATE", &fk.on_update)] {
            let action = action.trim().to_ascii_uppercase();
            if !action.is_empty() && action != "NO ACTION" {
                let _ = write!(line, " ON {clause} {action}");
            }
        }
        lines.push(line);
    }
    format!(
        "CREATE TABLE {} (\n  {}\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4",
        backtick(table),
        lines.join(",\n  ")
    )
}

// ---------------------------------------------------------------------------
// Table rebuild
// ---------------------------------------------------------------------------

/// A schema change that needs the SQLite table-rebuild procedure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebuildEdit {
    /// `ADD [CONSTRAINT name] FOREIGN KEY ...` / `ADD CHECK`. `sql` is the
    /// SQLite constraint text without the `CONSTRAINT name` prefix.
    AddConstraint {
        name: Option<String>,
        sql: String,
        foreign_key: bool,
    },
    /// `DROP FOREIGN KEY name` / `DROP CONSTRAINT name` / `DROP CHECK name`.
    DropConstraint {
        name: String,
        foreign_key_only: bool,
    },
    /// `MODIFY` / `CHANGE COLUMN`; `definition` is the new SQLite column
    /// definition (including the new name).
    Column {
        column: String,
        definition: String,
        auto_increment: bool,
    },
}

/// Facts about the table being rebuilt, loaded by the session.
#[derive(Debug, Clone)]
pub struct TableState {
    pub name: String,
    /// Stored `CREATE TABLE` text.
    pub create_sql: String,
    /// Non-generated column names, in order.
    pub columns: Vec<String>,
    /// `CREATE INDEX` / `CREATE TRIGGER` text bound to the table.
    pub own_objects: Vec<String>,
    /// `(name, kind, sql)` of views and other-table triggers that must be
    /// dropped and recreated around the rename, in creation order.
    pub dependents: Vec<(String, String, String)>,
}

/// Failure while planning a rebuild: `(mysql code, sqlstate, message)`.
pub type RebuildError = (u16, &'static str, String);

fn has_word(sql: &str, word: &str) -> bool {
    sql.to_ascii_uppercase().contains(word)
}

/// Rewrites the column item of a rebuilt table.
fn modified_column(old: &str, definition: &str, auto_increment: bool) -> String {
    let upper_old = old.to_ascii_uppercase();
    let old_pk = upper_old.contains("PRIMARY KEY");
    let mut new = definition.trim().to_owned();
    let upper_new = new.to_ascii_uppercase();
    if old_pk && !upper_new.contains("PRIMARY KEY") {
        new.push_str(" PRIMARY KEY");
        if auto_increment || upper_old.contains("AUTOINCREMENT") {
            new.push_str(" AUTOINCREMENT");
        }
    }
    if upper_old.contains("UNIQUE") && !upper_new.contains("UNIQUE") && !old_pk {
        new.push_str(" UNIQUE");
    }
    if !upper_new.contains("REFERENCES") {
        if let Some(position) = upper_old.find("REFERENCES") {
            new.push(' ');
            new.push_str(old[position..].trim());
        }
    }
    new
}

/// Plans the statements that apply `edit` to `state` atomically (they are
/// executed as a single batch). Returns the statements.
pub fn plan_rebuild(state: &TableState, edit: &RebuildEdit) -> Result<Vec<String>, RebuildError> {
    let table = &state.name;
    let mut parts = split_create(&state.create_sql).ok_or((
        1105,
        "HY000",
        format!("cannot parse the stored definition of `{table}`"),
    ))?;
    let fk_names = foreign_key_names(table, &parts);
    match edit {
        RebuildEdit::AddConstraint {
            name,
            sql,
            foreign_key,
        } => {
            let name = name.clone().unwrap_or_else(|| {
                let mut n = fk_names.len() + 1;
                loop {
                    let candidate = if *foreign_key {
                        format!("{table}_ibfk_{n}")
                    } else {
                        format!("{table}_chk_{n}")
                    };
                    let taken = parts
                        .items
                        .iter()
                        .any(|i| classify(i).and_then(|i| i.name).as_deref() == Some(&candidate));
                    if !taken {
                        break candidate;
                    }
                    n += 1;
                }
            });
            let exists = parts.items.iter().any(|item| {
                classify(item)
                    .and_then(|i| i.name)
                    .is_some_and(|n| n.eq_ignore_ascii_case(&name))
            }) || (*foreign_key
                && fk_names.iter().any(|(n, _)| n.eq_ignore_ascii_case(&name)));
            if exists {
                return Err((
                    1826,
                    "HY000",
                    format!("Duplicate foreign key constraint name '{name}'"),
                ));
            }
            parts
                .items
                .push(format!("CONSTRAINT {} {}", quote_ident(&name), sql.trim()));
        }
        RebuildEdit::DropConstraint {
            name,
            foreign_key_only,
        } => {
            let mut found = None;
            let mut fk_position = 0;
            for (index, item) in parts.items.iter().enumerate() {
                let Some(classified) = classify(item) else {
                    continue;
                };
                let is_fk = matches!(classified.kind, ItemKind::ForeignKey { .. });
                let item_name = if is_fk {
                    fk_position += 1;
                    classified
                        .name
                        .clone()
                        .unwrap_or_else(|| format!("{table}_ibfk_{fk_position}"))
                } else {
                    classified.name.clone().unwrap_or_default()
                };
                let eligible = if *foreign_key_only {
                    is_fk
                } else {
                    classified.name.is_some() || is_fk
                };
                if eligible && item_name.eq_ignore_ascii_case(name) {
                    found = Some(index);
                    break;
                }
            }
            let Some(index) = found else {
                return Err((
                    1091,
                    "42000",
                    format!("Can't DROP '{name}'; check that column/key exists"),
                ));
            };
            parts.items.remove(index);
        }
        RebuildEdit::Column {
            column,
            definition,
            auto_increment,
        } => {
            let position = parts
                .items
                .iter()
                .position(|item| {
                    matches!(
                        classify(item),
                        Some(Item { kind: ItemKind::Column(c), .. }) if c.eq_ignore_ascii_case(column)
                    )
                })
                .ok_or((
                    1054,
                    "42S22",
                    format!("Unknown column '{column}' in '{table}'"),
                ))?;
            let replaced = modified_column(&parts.items[position], definition, *auto_increment);
            parts.items[position] = replaced;
        }
    }

    let temporary = format!("__novadb_rebuild_{table}");
    let column_list = state
        .columns
        .iter()
        .map(|c| quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ");
    let autoincrement = has_word(&state.create_sql, "AUTOINCREMENT")
        && has_word(&parts.items.join(","), "AUTOINCREMENT");
    let mut statements = Vec::new();
    for (name, kind, _) in state.dependents.iter().rev() {
        statements.push(format!(
            "DROP {} IF EXISTS {}",
            kind.to_ascii_uppercase(),
            quote_ident(name)
        ));
    }
    statements.push(parts.render(&temporary));
    statements.push(format!(
        "INSERT INTO {} ({column_list}) SELECT {column_list} FROM {}",
        quote_ident(&temporary),
        quote_ident(table)
    ));
    if autoincrement {
        // Carry the AUTOINCREMENT high-water mark over to the new table.
        let (old, new) = (quote_str(table), quote_str(&temporary));
        statements.push(format!(
            "UPDATE sqlite_sequence SET seq = max(seq, coalesce((SELECT max(seq) FROM sqlite_sequence WHERE name = {old}), 0)) WHERE name = {new}"
        ));
        statements.push(format!(
            "INSERT INTO sqlite_sequence (name, seq) SELECT {new}, seq FROM sqlite_sequence WHERE name = {old} \
             AND NOT EXISTS (SELECT 1 FROM sqlite_sequence WHERE name = {new})"
        ));
    }
    statements.push(format!("DROP TABLE {}", quote_ident(table)));
    statements.push(format!(
        "ALTER TABLE {} RENAME TO {}",
        quote_ident(&temporary),
        quote_ident(table)
    ));
    statements.extend(state.own_objects.iter().cloned());
    statements.extend(state.dependents.iter().map(|(_, _, sql)| sql.clone()));
    Ok(statements)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_create_table_items() {
        let parts = split_create(
            "CREATE TABLE \"t\" ( \"id\" integer PRIMARY KEY autoincrement, \"a,b\" varchar(10) DEFAULT 'x,(y',\n\
             CONSTRAINT \"fk1\" FOREIGN KEY (\"p\", q) REFERENCES \"u\" (\"id\"), CHECK (a IN (1,2)) ) WITHOUT ROWID",
        )
        .unwrap();
        assert_eq!(parts.items.len(), 4);
        assert_eq!(parts.items[1], "\"a,b\" varchar(10) DEFAULT 'x,(y'");
        assert_eq!(parts.tail.trim(), "WITHOUT ROWID");
        assert_eq!(
            classify(&parts.items[2]),
            Some(Item {
                name: Some("fk1".into()),
                kind: ItemKind::ForeignKey {
                    columns: vec!["p".into(), "q".into()]
                }
            })
        );
        assert_eq!(classify(&parts.items[3]).unwrap().kind, ItemKind::Check);
        assert_eq!(
            classify(&parts.items[0]).unwrap().kind,
            ItemKind::Column("id".into())
        );
    }

    #[test]
    fn type_mapping() {
        for (declared, expected) in [
            ("INTEGER", "int"),
            ("bigint", "bigint"),
            ("TINYINT(1)", "tinyint(1)"),
            ("varchar(80)", "varchar(80)"),
            ("VARCHAR", "varchar(255)"),
            ("text", "text"),
            ("DOUBLE", "double"),
            ("DECIMAL(10,2)", "decimal(10,2)"),
            ("DATETIME", "datetime"),
            ("blob", "blob"),
            ("", "text"),
            ("JSON", "json"),
        ] {
            assert_eq!(mysql_type(declared), expected, "{declared}");
        }
    }

    #[test]
    fn show_create_format() {
        let columns = vec![
            ColumnMeta {
                name: "id".into(),
                declared_type: "INTEGER".into(),
                sqlite_type: "INTEGER".into(),
                not_null: false,
                default: None,
                pk: 1,
            },
            ColumnMeta {
                name: "name".into(),
                declared_type: "varchar(40)".into(),
                sqlite_type: "TEXT".into(),
                not_null: true,
                default: Some("'x'".into()),
                pk: 0,
            },
            ColumnMeta {
                name: "n".into(),
                declared_type: "int".into(),
                sqlite_type: "int".into(),
                not_null: false,
                default: None,
                pk: 0,
            },
        ];
        let ddl = render_show_create(
            "t",
            &columns,
            &[IndexMeta {
                name: "t_n".into(),
                unique: true,
                columns: vec!["n".into()],
            }],
            &[ForeignKeyMeta {
                name: "fk".into(),
                columns: vec!["n".into()],
                ref_table: "u".into(),
                ref_columns: vec!["id".into()],
                on_update: "NO ACTION".into(),
                on_delete: "CASCADE".into(),
            }],
        );
        assert_eq!(
            ddl,
            "CREATE TABLE `t` (\n  `id` int NOT NULL AUTO_INCREMENT,\n  `name` varchar(40) NOT NULL DEFAULT 'x',\n  \
             `n` int DEFAULT NULL,\n  PRIMARY KEY (`id`),\n  UNIQUE KEY `t_n` (`n`),\n  \
             CONSTRAINT `fk` FOREIGN KEY (`n`) REFERENCES `u` (`id`) ON DELETE CASCADE\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"
        );
    }

    fn state(create_sql: &str) -> TableState {
        TableState {
            name: "t".into(),
            create_sql: create_sql.into(),
            columns: vec!["id".into(), "p".into()],
            own_objects: vec!["CREATE INDEX \"i\" ON \"t\" (\"p\")".into()],
            dependents: Vec::new(),
        }
    }

    #[test]
    fn rebuild_add_and_drop_foreign_key() {
        let base =
            state("CREATE TABLE \"t\" ( \"id\" integer PRIMARY KEY autoincrement, \"p\" int )");
        let add = RebuildEdit::AddConstraint {
            name: Some("t_p_fk".into()),
            sql: "FOREIGN KEY (p) REFERENCES u (id)".into(),
            foreign_key: true,
        };
        let statements = plan_rebuild(&base, &add).unwrap();
        assert!(statements[0].starts_with("CREATE TABLE \"__novadb_rebuild_t\""));
        assert!(statements[0].contains("CONSTRAINT \"t_p_fk\" FOREIGN KEY (p) REFERENCES u (id)"));
        assert!(statements.iter().any(|s| s.starts_with("INSERT INTO")));
        assert!(statements.iter().any(|s| s == "DROP TABLE \"t\""));
        assert!(statements.iter().any(|s| s.contains("sqlite_sequence")));
        assert_eq!(
            statements.last().unwrap(),
            "CREATE INDEX \"i\" ON \"t\" (\"p\")"
        );

        let with_fk = state(&statements[0].replace("__novadb_rebuild_t", "t"));
        let dropped = plan_rebuild(
            &with_fk,
            &RebuildEdit::DropConstraint {
                name: "t_p_fk".into(),
                foreign_key_only: true,
            },
        )
        .unwrap();
        assert!(!dropped[0].contains("FOREIGN KEY"));
        let missing = plan_rebuild(
            &with_fk,
            &RebuildEdit::DropConstraint {
                name: "nope".into(),
                foreign_key_only: true,
            },
        );
        assert_eq!(missing.unwrap_err().0, 1091);
        let duplicate = plan_rebuild(&with_fk, &add);
        assert_eq!(duplicate.unwrap_err().0, 1826);
    }

    #[test]
    fn rebuild_modify_keeps_primary_key() {
        let base = state(
            "CREATE TABLE \"t\" ( \"id\" integer PRIMARY KEY autoincrement, \"p\" int UNIQUE )",
        );
        let edit = RebuildEdit::Column {
            column: "p".into(),
            definition: "\"p\" bigint NOT NULL".into(),
            auto_increment: false,
        };
        let statements = plan_rebuild(&base, &edit).unwrap();
        assert!(
            statements[0].contains("\"p\" bigint NOT NULL UNIQUE"),
            "{}",
            statements[0]
        );
    }
}
