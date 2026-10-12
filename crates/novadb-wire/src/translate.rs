//! PostgreSQL -> SQLite statement translation.
//!
//! Operates on the token stream from [`crate::lexer`], so literals, quoted
//! identifiers and comments are never rewritten. The pipeline is:
//! parameter substitution, catalog emulation, select-list labelling,
//! schema-prefix stripping, function rewrites, `ANY(ARRAY[..])`, `::` casts,
//! miscellaneous syntax (`ILIKE`, `FOR UPDATE`, ...) and DDL fix-ups.

use crate::analysis::{
    self, ExprShape, Schema, SelectItem, parse_items, parse_type_name, select_parts, type_oid,
};
use crate::catalog;
use crate::lexer::{
    Kind, SqlError, SqlResult, Token, find_close, find_open, hex_literal, quote_ident, quote_str,
    split_commas,
};
use crate::type_map::{self, ParamValue, oid};
use crate::vars::{Vars, version_string};

/// Everything the rewrites need to know about the session.
pub struct Env<'a> {
    pub database: &'a str,
    pub user: &'a str,
    pub pid: i32,
    pub vars: &'a Vars,
    pub schema: &'a Schema,
}

/// `ALTER TABLE .. ADD PRIMARY KEY / FOREIGN KEY / CHECK`: SQLite cannot add
/// these in place, so the session rebuilds the table with the extra clause.
#[derive(Debug, Clone)]
pub struct Rebuild {
    pub table: String,
    /// Table-constraint clause, e.g. `CONSTRAINT "fk" FOREIGN KEY (a) REFERENCES t(id)`.
    pub clause: String,
    pub kind: &'static str,
}

/// A fully translated statement.
#[derive(Debug, Default)]
pub struct Rewritten {
    /// Statement tokens (without the `RETURNING` clause).
    pub tokens: Vec<Token>,
    /// Translated `RETURNING` items, if the statement had that clause.
    pub returning: Option<Vec<Token>>,
    /// PostgreSQL names of the `RETURNING` items (stars expanded).
    pub returning_names: Option<Vec<String>>,
    /// `set_config(..)` calls found in the statement.
    pub set_configs: Vec<(String, String)>,
    /// PostgreSQL names of the main select list, when it could be parsed
    /// (the SQL has been relabelled so each item has a unique SQLite label).
    pub labels: Option<Vec<String>>,
    /// Extra statements to run after the main one (multi-table DROP).
    pub extra: Vec<Vec<Token>>,
    /// Constraint addition implemented by rebuilding the table.
    pub rebuild: Option<Rebuild>,
}

// ---------------------------------------------------------------------------
// Parameter substitution
// ---------------------------------------------------------------------------

fn param_token(value: &ParamValue, ws: &str) -> Token {
    let raw = |text: String| Token::new(Kind::Raw, text).with_ws(ws);
    let signed = |text: String| {
        if text.starts_with('-') {
            raw(format!("({text})"))
        } else {
            raw(text)
        }
    };
    match value {
        ParamValue::Null => raw("NULL".into()),
        ParamValue::Int(n) => signed(n.to_string()),
        ParamValue::Float(f) => signed(format!("{f:?}")),
        ParamValue::Bool(b) => raw(u8::from(*b).to_string()),
        ParamValue::Numeric(text) => signed(text.clone()),
        ParamValue::Text(text) => Token::string(text).with_ws(ws),
        ParamValue::Untyped(text) => match text.parse::<i64>() {
            // Only canonical integers become numbers: '007' must stay text.
            Ok(n) if n.to_string() == *text && text.len() < 19 => signed(text.clone()),
            _ => Token::string(text).with_ws(ws),
        },
        ParamValue::Bytes(bytes) => raw(hex_literal(bytes)),
    }
}

/// Replaces `$n` placeholders by literal tokens in a single pass. The values
/// are tokens, never re-scanned, so a value can not inject SQL.
pub fn substitute_params(tokens: &[Token], values: &[ParamValue]) -> Vec<Token> {
    tokens
        .iter()
        .map(|token| {
            if token.kind == Kind::Param {
                let value = token
                    .value
                    .parse::<usize>()
                    .ok()
                    .filter(|n| *n >= 1)
                    .and_then(|n| values.get(n - 1));
                return match value {
                    Some(value) => param_token(value, &token.ws),
                    None => Token::new(Kind::Raw, "NULL").with_ws(&token.ws),
                };
            }
            token.clone()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn splice(tokens: &mut Vec<Token>, start: usize, end: usize, mut new: Vec<Token>) {
    if let (Some(first), Some(old)) = (new.first_mut(), tokens.get(start)) {
        first.ws.clone_from(&old.ws);
    }
    tokens.splice(start..end, new);
}

fn qident(name: &str) -> Token {
    let mut token = Token::new(Kind::QIdent, quote_ident(name)).with_ws(" ");
    token.value = name.to_owned();
    token
}

fn raw(text: &str) -> Token {
    Token::raw(text)
}

fn op(text: &str) -> Token {
    Token::op(text)
}

fn word(text: &str) -> Token {
    Token::word(text)
}

fn call_args(tokens: &[Token], name_index: usize) -> Option<(usize, Vec<Vec<Token>>)> {
    if !tokens.get(name_index + 1)?.is_op("(") {
        return None;
    }
    let close = find_close(tokens, name_index + 1)?;
    let inner = &tokens[name_index + 2..close];
    let args = if inner.is_empty() {
        Vec::new()
    } else {
        split_commas(inner)
            .into_iter()
            .map(<[Token]>::to_vec)
            .collect()
    };
    Some((close, args))
}

fn str_arg(arg: &[Token]) -> Option<String> {
    match arg {
        [token] if token.kind == Kind::Str => Some(token.value.clone()),
        // 'literal'::type
        [token, cast, ..] if token.kind == Kind::Str && cast.is_op("::") => {
            Some(token.value.clone())
        }
        _ => None,
    }
}

fn paren(mut inner: Vec<Token>) -> Vec<Token> {
    if let Some(first) = inner.first_mut() {
        first.ws.clear();
    }
    let mut out = vec![op("(")];
    out.extend(inner);
    out.push(op(")"));
    out
}

fn leading_lower(tokens: &[Token], count: usize) -> Vec<String> {
    crate::lexer::leading_words(tokens, count)
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Translates one statement (already parameter-substituted).
pub fn rewrite(tokens: Vec<Token>, env: &Env<'_>) -> SqlResult<Rewritten> {
    let mut out = Rewritten::default();
    let lead = leading_lower(&tokens, 3);
    let is_ddl = matches!(
        lead.first().map(String::as_str),
        Some("create" | "alter" | "drop")
    );

    let (mut main, returning) = split_returning(tokens);
    let mut returning = returning;
    if let (Some(items), Some(target)) = (returning.as_ref(), target_table(&main)) {
        out.returning_names = Some(returning_names(items, &target, env.schema));
    }

    // 1. Relabel the main select list so every output column is unique.
    out.labels = relabel_select(&mut main, env);

    // 2. Catalog tables (information_schema / pg_catalog).
    catalog::rewrite(&mut main, env)?;
    if let Some(items) = returning.as_mut() {
        catalog::rewrite(items, env)?;
    }

    // 3. Expression-level rewrites, applied to both parts.
    for part in std::iter::once(&mut main).chain(returning.iter_mut()) {
        strip_schema_prefix(part);
        rewrite_functions(part, env, &mut out.set_configs)?;
        rewrite_any_array(part)?;
        rewrite_values_aliases(part)?;
        rewrite_casts(part)?;
        rewrite_regex_ops(part)?;
        rewrite_misc(part)?;
    }

    // 4. Statement-level fix-ups.
    if is_ddl {
        out.extra = rewrite_ddl(&mut main, &mut out.rebuild)?;
    }
    rewrite_defaults(&mut main, env)?;
    reject_unsupported(&main)?;

    out.tokens = main;
    out.returning = returning;
    Ok(out)
}

/// Splits `... RETURNING items` off an INSERT / UPDATE / DELETE statement.
pub fn split_returning(tokens: Vec<Token>) -> (Vec<Token>, Option<Vec<Token>>) {
    let first = tokens.first().map(Token::lower).unwrap_or_default();
    let skip = analysis::skip_with(&tokens);
    let leading = tokens.get(skip).map(Token::lower).unwrap_or_default();
    if !matches!(first.as_str(), "insert" | "update" | "delete" | "with")
        || !matches!(leading.as_str(), "insert" | "update" | "delete")
    {
        return (tokens, None);
    }
    let mut depth = 0_i32;
    for (index, token) in tokens.iter().enumerate() {
        if token.is_op("(") {
            depth += 1;
        } else if token.is_op(")") {
            depth -= 1;
        } else if depth == 0 && token.is_word("RETURNING") {
            let returning = tokens[index + 1..].to_vec();
            let mut main = tokens;
            main.truncate(index);
            return (main, Some(returning));
        }
    }
    (tokens, None)
}

fn reject_unsupported(tokens: &[Token]) -> SqlResult<()> {
    let lead = leading_lower(tokens, 4);
    let joined = lead.join(" ");
    let unsupported = [
        (
            "create extension",
            "CREATE EXTENSION is not supported by NovaDB",
        ),
        (
            "create type",
            "CREATE TYPE (enums, composite types) is not supported by NovaDB",
        ),
        (
            "create sequence",
            "CREATE SEQUENCE is not supported by NovaDB; use SERIAL / INTEGER PRIMARY KEY",
        ),
        (
            "alter sequence",
            "ALTER SEQUENCE is not supported by NovaDB",
        ),
        ("drop sequence", "DROP SEQUENCE is not supported by NovaDB"),
        (
            "create function",
            "CREATE FUNCTION is not supported by NovaDB",
        ),
        (
            "create or replace function",
            "CREATE FUNCTION is not supported by NovaDB",
        ),
        ("create domain", "CREATE DOMAIN is not supported by NovaDB"),
        ("create role", "roles are not supported by NovaDB"),
        ("create user", "roles are not supported by NovaDB"),
        ("grant", "GRANT is not supported by NovaDB"),
        ("revoke", "REVOKE is not supported by NovaDB"),
        ("copy", "COPY is not supported by NovaDB"),
    ];
    for (prefix, message) in unsupported {
        if joined.starts_with(prefix) {
            return Err(SqlError::Unsupported(message.into()));
        }
    }
    if tokens.iter().any(|t| t.is_op("["))
        && tokens
            .first()
            .is_some_and(|t| t.is_any_word(&["CREATE", "ALTER"]))
    {
        return Err(SqlError::Unsupported(
            "array column types are not supported by NovaDB".into(),
        ));
    }
    if lead.first().map(String::as_str) == Some("select")
        && tokens
            .windows(2)
            .any(|w| w[0].is_word("DISTINCT") && w[1].is_word("ON"))
    {
        return Err(SqlError::Unsupported(
            "SELECT DISTINCT ON is not supported by NovaDB; use GROUP BY".into(),
        ));
    }
    if tokens
        .windows(3)
        .any(|w| w[0].is_word("ON") && w[1].is_word("CONFLICT") && w[2].is_word("ON"))
    {
        return Err(SqlError::Unsupported(
            "ON CONFLICT ON CONSTRAINT is not supported; name the conflict columns instead".into(),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Select-list labelling
// ---------------------------------------------------------------------------

/// PostgreSQL's name for an unaliased select item.
fn pg_name(item: &SelectItem) -> String {
    if let Some(alias) = &item.alias {
        return alias.clone();
    }
    match item.shape() {
        ExprShape::Column(_, name) => name,
        ExprShape::Call(name) => name,
        _ => {
            if item.expr.first().is_some_and(|t| t.is_word("CASE")) {
                "case".into()
            } else if let Some(cast) = cast_name(&item.expr) {
                cast
            } else {
                "?column?".into()
            }
        }
    }
}

/// Name PostgreSQL gives `x::type` / `CAST(x AS type)` items.
fn cast_name(expr: &[Token]) -> Option<String> {
    let index = expr.iter().rposition(|t| t.is_op("::"))?;
    let (name, array, used) = parse_type_name(&expr[index + 1..])?;
    if array || index + 1 + used != expr.len() {
        return None;
    }
    let operand = &expr[..index];
    match analysis::shape_of(operand) {
        ExprShape::Column(_, name) | ExprShape::Call(name) => return Some(name),
        _ => {}
    }
    Some(match name.as_str() {
        "integer" | "int" => "int4".into(),
        "bigint" => "int8".into(),
        "smallint" => "int2".into(),
        "boolean" => "bool".into(),
        "character varying" => "varchar".into(),
        "double precision" => "float8".into(),
        other => other.to_owned(),
    })
}

/// Names the items of the main select list. Items whose SQLite label would
/// differ from the PostgreSQL name, or clash with another item, get an
/// explicit alias; the PostgreSQL names are returned for the RowDescription.
fn relabel_select(tokens: &mut Vec<Token>, env: &Env<'_>) -> Option<Vec<String>> {
    let parts = select_parts(tokens)?;
    let items = parse_items(&tokens[parts.items_start..parts.items_end]);
    if items.is_empty() {
        return None;
    }
    let mut names = Vec::with_capacity(items.len());
    for item in &items {
        if let ExprShape::Star(qualifier) = item.shape() {
            // Expand stars when the tables are known; otherwise give up
            // (SQLite names the columns itself).
            let expanded = expand_star(qualifier.as_deref(), &parts.tables, env.schema)?;
            names.extend(expanded);
        } else {
            names.push(pg_name(item));
        }
    }
    let has_star = items
        .iter()
        .any(|item| matches!(item.shape(), ExprShape::Star(_)));
    if has_star {
        // Stars cannot be relabelled item-by-item. Duplicate names collapse
        // in the result map (documented limitation); names stay as SQLite
        // reports them.
        return Some(names);
    }
    let mut label_counts = std::collections::HashMap::new();
    for name in &names {
        *label_counts.entry(name.clone()).or_insert(0) += 1;
    }
    // Rebuild the select list with explicit unique aliases where needed.
    let mut rebuilt: Vec<Token> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            rebuilt.push(op(","));
        }
        let name = &names[index];
        let unique = label_counts[name] == 1;
        let label = if unique {
            name.clone()
        } else {
            format!("{name}__{index}")
        };
        let mut expr = item.expr.clone();
        if let Some(first) = expr.first_mut() {
            first.ws = " ".into();
        }
        rebuilt.extend(expr);
        let plain_column =
            item.alias.is_none() && unique && matches!(item.shape(), ExprShape::Column(..));
        if !plain_column {
            rebuilt.push(word("AS"));
            rebuilt.push(qident(&label));
        }
    }
    // Column references that are unique and unaliased are left as-is.
    tokens.splice(parts.items_start..parts.items_end, rebuilt);
    Some(names)
}

fn expand_star(
    qualifier: Option<&str>,
    tables: &[analysis::TableRef],
    schema: &Schema,
) -> Option<Vec<String>> {
    let mut names = Vec::new();
    let mut matched = false;
    for table in tables {
        if let Some(q) = qualifier
            && !table.alias.eq_ignore_ascii_case(q)
            && !table.name.eq_ignore_ascii_case(q)
        {
            continue;
        }
        matched = true;
        let info = schema.table(&table.name)?;
        names.extend(info.columns.iter().map(|c| c.name.clone()));
    }
    matched.then_some(names)
}

/// Result-column names of a `RETURNING` clause (stars expanded via `table`).
pub fn returning_names(
    items: &[Token],
    table: &analysis::TableRef,
    schema: &Schema,
) -> Vec<String> {
    let mut names = Vec::new();
    for item in parse_items(items) {
        if let ExprShape::Star(_) = item.shape() {
            if let Some(info) = schema.table(&table.name) {
                names.extend(info.columns.iter().map(|c| c.name.clone()));
            }
        } else {
            names.push(pg_name(&item));
        }
    }
    names
}

/// Rebuilds a RETURNING list as `expr AS "cN"` items (stars expanded when
/// the table is known); returns the display names alongside.
pub fn relabel_returning(
    items: &[Token],
    table: &analysis::TableRef,
    schema: &Schema,
) -> (Vec<Token>, Vec<String>) {
    let parsed = parse_items(items);
    let mut names: Vec<String> = Vec::new();
    let mut exprs: Vec<Vec<Token>> = Vec::new();
    for item in &parsed {
        if let ExprShape::Star(_) = item.shape() {
            if let Some(info) = schema.table(&table.name) {
                for column in &info.columns {
                    names.push(column.name.clone());
                    exprs.push(vec![qident(&column.name)]);
                }
                continue;
            }
        }
        names.push(pg_name(item));
        exprs.push(item.expr.clone());
    }
    let mut rebuilt = Vec::new();
    for (index, mut expr) in exprs.into_iter().enumerate() {
        if index > 0 {
            rebuilt.push(op(","));
        }
        if let Some(first) = expr.first_mut() {
            first.ws = " ".into();
        }
        rebuilt.extend(expr);
        rebuilt.push(word("AS"));
        rebuilt.push(qident(&format!("c{index}")));
    }
    (rebuilt, names)
}

// ---------------------------------------------------------------------------
// Schema prefixes
// ---------------------------------------------------------------------------

fn strip_schema_prefix(tokens: &mut Vec<Token>) {
    let mut i = 0;
    while i + 2 < tokens.len() + 1 {
        if i + 1 < tokens.len()
            && tokens[i + 1].is_op(".")
            && tokens[i]
                .ident()
                .is_some_and(|name| name == "public" || name == "pg_catalog")
            && matches!(tokens[i].kind, Kind::Word | Kind::QIdent)
            && tokens.get(i + 2).is_some_and(|t| t.ident().is_some())
            && (i == 0 || !tokens[i - 1].is_op("."))
        {
            // pg_catalog.<name> is only a prefix for functions / types here;
            // catalog tables were already replaced by the catalog pass.
            let ws = tokens[i].ws.clone();
            tokens.drain(i..i + 2);
            if let Some(first) = tokens.get_mut(i) {
                first.ws = ws;
            }
            continue;
        }
        i += 1;
    }
}

// ---------------------------------------------------------------------------
// Functions
// ---------------------------------------------------------------------------

const UUID_V4_SQL: &str = "lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' || \
     substr(hex(randomblob(2)), 2) || '-' || substr('89ab', 1 + (abs(random() % 4)), 1) || \
     substr(hex(randomblob(2)), 2) || '-' || hex(randomblob(6)))";

fn rewrite_functions(
    tokens: &mut Vec<Token>,
    env: &Env<'_>,
    set_configs: &mut Vec<(String, String)>,
) -> SqlResult<()> {
    let mut i = tokens.len();
    while i > 0 {
        i -= 1;
        if tokens[i].kind != Kind::Word {
            continue;
        }
        if i > 0 && tokens[i - 1].is_op(".") {
            continue; // column or qualified name, not a bare function
        }
        let name = tokens[i].lower();
        let is_call = tokens.get(i + 1).is_some_and(|t| t.is_op("("));

        // Bare keywords.
        if !is_call {
            let replacement = match name.as_str() {
                "current_user" | "session_user" | "current_role" => {
                    Some(vec![Token::string(env.user)])
                }
                "current_catalog" => Some(vec![Token::string(env.database)]),
                "current_schema" => Some(vec![Token::string("public")]),
                "localtimestamp" => Some(vec![raw("CURRENT_TIMESTAMP")]),
                _ => None,
            };
            if let Some(replacement) = replacement {
                let after_as = i > 0 && tokens[i - 1].is_word("AS");
                if !after_as {
                    splice(tokens, i, i + 1, replacement);
                }
            }
            continue;
        }

        let Some((close, args)) = call_args(tokens, i) else {
            continue;
        };
        let replacement: Option<Vec<Token>> = match (name.as_str(), args.len()) {
            ("version", 0) => Some(vec![Token::string(&version_string())]),
            ("current_schema" | "current_schemas", _) if name == "current_schema" => {
                Some(vec![Token::string("public")])
            }
            ("current_database", 0) => Some(vec![Token::string(env.database)]),
            ("current_user" | "session_user", 0) => Some(vec![Token::string(env.user)]),
            ("pg_backend_pid", 0) => Some(vec![raw(&env.pid.to_string())]),
            ("pg_is_in_recovery", 0) => Some(vec![raw("0")]),
            ("inet_client_addr" | "inet_server_addr", 0) => Some(vec![raw("NULL")]),
            ("now" | "transaction_timestamp" | "statement_timestamp" | "clock_timestamp", 0)
            | ("current_timestamp" | "localtimestamp", _) => Some(vec![raw("CURRENT_TIMESTAMP")]),
            ("gen_random_uuid" | "uuid_generate_v4", 0) => Some(paren(vec![raw(UUID_V4_SQL)])),
            ("random", 0) => Some(vec![raw(
                "(((random() >> 11) & 9007199254740991) / 9007199254740992.0)",
            )]),
            ("lastval", 0) => Some(vec![raw("last_insert_rowid()")]),
            ("currval", 1) => Some(vec![raw("last_insert_rowid()")]),
            ("nextval" | "setval", _) => {
                return Err(SqlError::Unsupported(format!(
                    "{name}() is not supported: NovaDB has no sequences; use SERIAL or INTEGER PRIMARY KEY"
                )));
            }
            ("pg_encoding_to_char", 1) => Some(vec![Token::string("UTF8")]),
            (
                "has_table_privilege"
                | "has_schema_privilege"
                | "has_database_privilege"
                | "has_column_privilege"
                | "has_any_column_privilege"
                | "has_sequence_privilege"
                | "pg_has_role",
                _,
            ) => Some(vec![raw("1")]),
            (
                "obj_description"
                | "col_description"
                | "shobj_description"
                | "pg_get_serial_sequence",
                _,
            ) => Some(vec![raw("NULL")]),
            (
                "pg_table_is_visible"
                | "pg_type_is_visible"
                | "pg_function_is_visible"
                | "pg_collation_is_visible",
                1,
            ) => Some(vec![raw("1")]),
            ("pg_get_userbyid", 1) => Some(vec![Token::string(env.user)]),
            ("pg_get_expr", 2 | 3) => Some(paren(args[0].clone())),
            ("pg_get_viewdef", _) => Some(vec![raw("NULL")]),
            ("array_agg", 1) => {
                // NULL (not an empty array) when there are no input rows.
                let mut inner = vec![
                    word("CASE"),
                    word("WHEN"),
                    word("count"),
                    op("("),
                    raw("*"),
                    op(")"),
                    op("="),
                    raw("0"),
                    word("THEN"),
                    raw("NULL"),
                    word("ELSE"),
                    Token::raw("char(1)"),
                    op("||"),
                    word("json_group_array"),
                    op("("),
                ];
                inner.extend(args[0].clone());
                inner.extend([op(")"), word("END")]);
                Some(paren(inner))
            }
            ("bool_and" | "every", 1) => Some(rename_call("min", &args)),
            ("bool_or", 1) => Some(rename_call("max", &args)),
            ("json_build_object" | "jsonb_build_object", _) => {
                Some(rename_call("json_object", &args))
            }
            ("format_type", 2) => Some(format_type_case(&args[0], &args[1])),
            ("set_config", 3) => {
                let key = str_arg(&args[0]);
                let value = str_arg(&args[1]);
                match (key, value) {
                    (Some(key), Some(value)) => {
                        set_configs.push((key, value.clone()));
                        Some(vec![Token::string(&value)])
                    }
                    _ => {
                        return Err(SqlError::Unsupported(
                            "set_config() requires string literal arguments".into(),
                        ));
                    }
                }
            }
            ("current_setting", 1 | 2) => {
                let key = str_arg(&args[0]).ok_or_else(|| {
                    SqlError::Unsupported("current_setting() requires a string literal".into())
                })?;
                match env.vars.get(&key) {
                    Some(value) => Some(vec![Token::string(value)]),
                    None if args.len() == 2 => Some(vec![raw("NULL")]),
                    None => {
                        return Err(SqlError::Syntax(format!(
                            "unrecognized configuration parameter \"{key}\""
                        )));
                    }
                }
            }
            ("string_agg", 2) => {
                let mut out = vec![word("group_concat"), op("(")];
                out.extend(args[0].clone());
                out.push(op(","));
                out.extend(args[1].clone());
                out.push(op(")"));
                Some(out)
            }
            ("greatest", n) if n >= 2 => Some(rename_call("max", &args)),
            ("least", n) if n >= 2 => Some(rename_call("min", &args)),
            ("char_length" | "character_length", 1) => Some(rename_call("length", &args)),
            ("strpos", 2) => Some(rename_call("instr", &args)),
            ("octet_length", 1) => {
                let mut inner = vec![word("CAST"), op("(")];
                inner.extend(args[0].clone());
                inner.extend([word("AS"), word("BLOB"), op(")")]);
                Some(
                    vec![word("length"), op("(")]
                        .into_iter()
                        .chain(inner)
                        .chain([op(")")])
                        .collect(),
                )
            }
            ("left", 2) => {
                let mut out = vec![word("substr"), op("(")];
                out.extend(args[0].clone());
                out.extend([op(","), raw("1"), op(",")]);
                out.extend(args[1].clone());
                out.push(op(")"));
                Some(out)
            }
            ("right", 2) => {
                let mut out = vec![word("substr"), op("(")];
                out.extend(args[0].clone());
                out.push(op(","));
                out.push(op("-"));
                out.extend(paren(args[1].clone()));
                out.push(op(")"));
                Some(out)
            }
            ("mod", 2) => {
                let mut out = paren(args[0].clone());
                out.push(op("%"));
                out.extend(paren(args[1].clone()));
                Some(paren(out))
            }
            _ => None,
        };
        if let Some(replacement) = replacement {
            let end = close + 1;
            // A function name qualified by pg_catalog was stripped earlier.
            splice(tokens, i, end, replacement);
        }
    }
    Ok(())
}

fn rename_call(name: &str, args: &[Vec<Token>]) -> Vec<Token> {
    let mut out = vec![word(name), op("(")];
    for (index, arg) in args.iter().enumerate() {
        if index > 0 {
            out.push(op(","));
        }
        out.extend(arg.clone());
    }
    out.push(op(")"));
    out
}

/// `format_type(oid, typmod)` as a CASE over the known OIDs.
fn format_type_case(oid_expr: &[Token], typmod: &[Token]) -> Vec<Token> {
    let oid_sql = crate::lexer::render(oid_expr);
    let typmod_sql = crate::lexer::render(typmod);
    let mut names = String::new();
    for type_oid in [
        oid::BOOL,
        oid::BYTEA,
        oid::INT8,
        oid::INT2,
        oid::INT4,
        oid::OID,
        oid::JSON,
        oid::FLOAT4,
        oid::FLOAT8,
        oid::VARCHAR,
        oid::BPCHAR,
        oid::DATE,
        oid::TIME,
        oid::TIMESTAMP,
        oid::TIMESTAMPTZ,
        oid::NUMERIC,
        oid::UUID,
        oid::JSONB,
    ] {
        names.push_str(&format!(
            "WHEN {type_oid} THEN {} ",
            quote_str(type_map::type_name(type_oid))
        ));
    }
    let sql = format!(
        "((CASE ({oid_sql}) {names}ELSE 'text' END) || CASE \
         WHEN ({oid_sql}) IN (1043, 1042) AND ({typmod_sql}) > 4 THEN '(' || (({typmod_sql}) - 4) || ')' \
         WHEN ({oid_sql}) = 1700 AND ({typmod_sql}) > 4 THEN '(' || ((({typmod_sql}) - 4) / 65536) || ',' || ((({typmod_sql}) - 4) % 65536) || ')' \
         ELSE '' END)"
    );
    crate::lexer::tokenize(&sql).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// ANY / ALL
// ---------------------------------------------------------------------------

/// Parses PostgreSQL array text (`{1,2,"a b"}`) into elements.
fn parse_array_text(text: &str) -> Option<Vec<Option<String>>> {
    let inner = text.trim().strip_prefix('{')?.strip_suffix('}')?;
    let mut elements = Vec::new();
    let mut chars = inner.chars().peekable();
    if inner.trim().is_empty() {
        return Some(elements);
    }
    loop {
        let mut value = String::new();
        let mut quoted = false;
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek() == Some(&'"') {
            quoted = true;
            chars.next();
            loop {
                match chars.next()? {
                    '\\' => value.push(chars.next()?),
                    '"' => break,
                    c => value.push(c),
                }
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c == ',' {
                    break;
                }
                value.push(c);
                chars.next();
            }
            value = value.trim().to_owned();
        }
        if !quoted && value.eq_ignore_ascii_case("null") {
            elements.push(None);
        } else {
            elements.push(Some(value));
        }
        match chars.next() {
            Some(',') => {}
            None => return Some(elements),
            Some(_) => return None,
        }
    }
}

/// Rewrites `x = ANY (ARRAY[..] | 'array text' | subquery)` to `x IN (..)`.
fn rewrite_any_array(tokens: &mut Vec<Token>) -> SqlResult<()> {
    let mut i = tokens.len();
    while i > 1 {
        i -= 1;
        if !tokens[i].is_any_word(&["ANY", "ALL", "SOME"])
            || !tokens.get(i + 1).is_some_and(|t| t.is_op("("))
        {
            continue;
        }
        let Some(close) = find_close(tokens, i + 1) else {
            continue;
        };
        let comparison = tokens[i - 1].text.clone();
        let negate = match comparison.as_str() {
            "=" if !tokens[i].is_word("ALL") => false,
            "<>" | "!=" if tokens[i].is_word("ALL") => true,
            _ => continue,
        };
        let inner: Vec<Token> = tokens[i + 2..close].to_vec();
        let list: Vec<Token> = if inner
            .first()
            .is_some_and(|t| t.is_word("SELECT") || t.is_word("WITH"))
        {
            inner
        } else if inner.first().is_some_and(|t| t.is_word("ARRAY"))
            && inner.get(1).is_some_and(|t| t.is_op("["))
        {
            let end = inner
                .iter()
                .rposition(|t| t.is_op("]"))
                .ok_or_else(|| SqlError::Syntax("unterminated ARRAY[".into()))?;
            let mut list = inner[2..end].to_vec();
            // Strip a trailing ::type[] cast; element types follow SQLite.
            for token in &mut list {
                if token.is_op("::") {
                    break;
                }
            }
            list.retain(|t| !t.is_op("["));
            list
        } else if let Some(array) = inner.first().filter(|t| t.kind == Kind::Str) {
            let numeric = inner.get(1).is_some_and(|t| t.is_op("::"))
                && parse_type_name(&inner[2..])
                    .and_then(|(name, _, _)| type_oid(&name))
                    .is_some_and(|o| {
                        matches!(
                            o,
                            oid::INT2
                                | oid::INT4
                                | oid::INT8
                                | oid::FLOAT4
                                | oid::FLOAT8
                                | oid::NUMERIC
                        )
                    });
            let elements = parse_array_text(&array.value).ok_or_else(|| {
                SqlError::InvalidValue(format!("malformed array literal: \"{}\"", array.value))
            })?;
            let mut list = Vec::new();
            for (index, element) in elements.into_iter().enumerate() {
                if index > 0 {
                    list.push(op(","));
                }
                list.push(match element {
                    None => raw("NULL").with_ws(""),
                    Some(text) if numeric && text.parse::<f64>().is_ok() => raw(&text).with_ws(""),
                    Some(text) => Token::string(&text).with_ws(""),
                });
            }
            list
        } else {
            continue;
        };
        let mut replacement = Vec::new();
        if negate {
            replacement.push(word("NOT"));
        }
        replacement.push(word("IN"));
        replacement.push(op("(").with_ws(" "));
        replacement.extend(list);
        replacement.push(op(")"));
        // Replace "<op> ANY ( ... )" (operator included).
        let start = i - 1;
        splice(tokens, start, close + 1, replacement);
        i = start;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// VALUES (..) AS alias(col, ..)
// ---------------------------------------------------------------------------

/// SQLite has no column alias lists on subqueries; for `(VALUES ..) AS t(a, b)`
/// (used by SQLAlchemy's "insertmanyvalues") select `columnN AS a` instead.
fn rewrite_values_aliases(tokens: &mut Vec<Token>) -> SqlResult<()> {
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i].is_op("(") && tokens.get(i + 1).is_some_and(|t| t.is_word("VALUES")) {
            if let Some(close) = find_close(tokens, i) {
                let mut j = close + 1;
                if tokens.get(j).is_some_and(|t| t.is_word("AS")) {
                    j += 1;
                }
                if let (Some(alias), true) = (
                    tokens.get(j).and_then(Token::ident),
                    tokens.get(j + 1).is_some_and(|t| t.is_op("(")),
                ) {
                    if let Some(list_close) = find_close(tokens, j + 1) {
                        let names: Vec<String> = split_commas(&tokens[j + 2..list_close])
                            .iter()
                            .filter_map(|part| part.first().and_then(Token::ident))
                            .collect();
                        let select: Vec<String> = names
                            .iter()
                            .enumerate()
                            .map(|(n, name)| format!("column{} AS {}", n + 1, quote_ident(name)))
                            .collect();
                        let mut replacement = vec![
                            op("("),
                            word("SELECT"),
                            raw(&select.join(", ")),
                            word("FROM"),
                        ];
                        replacement.extend_from_slice(&tokens[i..=close]);
                        replacement.push(op(")"));
                        replacement.push(word("AS"));
                        replacement.push(qident(&alias));
                        splice(tokens, i, list_close + 1, replacement);
                    }
                }
            }
        }
        i += 1;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Casts
// ---------------------------------------------------------------------------

const NON_FUNCTION_WORDS: &[&str] = &[
    "in",
    "and",
    "or",
    "not",
    "when",
    "then",
    "else",
    "select",
    "where",
    "on",
    "as",
    "by",
    "from",
    "join",
    "limit",
    "offset",
    "values",
    "set",
    "having",
    "any",
    "all",
    "case",
    "between",
    "like",
    "ilike",
    "is",
    "using",
    "returning",
    "distinct",
    "exists",
    "some",
    "union",
    "intersect",
    "except",
    "end",
    "over",
    "filter",
    "into",
];

/// Start index of the operand that ends right before `end` (exclusive).
fn operand_start(tokens: &[Token], end: usize) -> Option<usize> {
    if end == 0 {
        return None;
    }
    let mut j = end - 1;
    let last = &tokens[j];
    if last.is_op(")") {
        j = find_open(tokens, j)?;
        if j > 0
            && match tokens[j - 1].kind {
                Kind::Word => !NON_FUNCTION_WORDS.contains(&tokens[j - 1].lower().as_str()),
                Kind::QIdent => true,
                _ => false,
            }
        {
            j -= 1;
        }
    } else if last.is_op("]") {
        // Array subscript: the operand is the subscripted expression.
        let mut depth = 0;
        let mut k = j;
        loop {
            if tokens[k].is_op("]") {
                depth += 1;
            } else if tokens[k].is_op("[") {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            if k == 0 {
                return None;
            }
            k -= 1;
        }
        return operand_start(tokens, k);
    } else if matches!(
        last.kind,
        Kind::Word | Kind::QIdent | Kind::Str | Kind::Num | Kind::Raw | Kind::Param
    ) {
        if last.kind == Kind::Word && NON_FUNCTION_WORDS.contains(&last.lower().as_str()) {
            return None;
        }
    } else {
        return None;
    }
    // Qualified names: a.b.c
    while j >= 2 && tokens[j - 1].is_op(".") && tokens[j - 2].ident().is_some() {
        j -= 2;
    }
    Some(j)
}

fn cast_to(mut operand: Vec<Token>, sql_type: &str) -> Vec<Token> {
    if let Some(first) = operand.first_mut() {
        first.ws.clear();
    }
    let mut out = vec![word("CAST"), op("(")];
    out.extend(operand);
    out.push(word("AS"));
    out.push(word(sql_type));
    out.push(op(")"));
    out
}

fn bool_cast(operand: Vec<Token>) -> Vec<Token> {
    let simple = operand.len() <= 3 && !operand.iter().any(|t| t.is_op("("));
    let inner = if simple {
        let text = |operand: &[Token]| {
            let mut v = vec![word("lower"), op("(")];
            v.extend(cast_to(operand.to_vec(), "TEXT"));
            v.push(op(")"));
            v
        };
        let mut v = vec![word("CASE"), word("WHEN")];
        v.extend(text(&operand));
        v.extend([word("IN"), op("(")]);
        for (i, t) in ["t", "true", "yes", "y", "on", "1"].iter().enumerate() {
            if i > 0 {
                v.push(op(","));
            }
            v.push(Token::string(t));
        }
        v.extend([op(")"), word("THEN"), raw("1"), word("WHEN")]);
        v.extend(text(&operand));
        v.extend([word("IN"), op("(")]);
        for (i, t) in ["f", "false", "no", "n", "off", "0"].iter().enumerate() {
            if i > 0 {
                v.push(op(","));
            }
            v.push(Token::string(t));
        }
        v.extend([op(")"), word("THEN"), raw("0"), word("ELSE")]);
        v.extend(cast_to(operand, "INTEGER"));
        v.push(word("END"));
        v
    } else {
        cast_to(operand, "INTEGER")
    };
    cast_to(paren(inner), "BOOLEAN")
}

fn cast_expression(operand: Vec<Token>, type_name: &str) -> SqlResult<Vec<Token>> {
    let single_str = match operand.as_slice() {
        [token] if token.kind == Kind::Str => Some(token.value.clone()),
        _ => None,
    };
    Ok(match type_name {
        "int" | "integer" | "int4" | "int2" | "smallint" | "int8" | "bigint" | "serial"
        | "bigserial" | "smallserial" | "oid" => cast_to(operand, "INTEGER"),
        "real" | "float4" | "float8" | "float" | "double precision" => cast_to(operand, "REAL"),
        "numeric" | "decimal" => cast_to(operand, "NUMERIC"),
        "text" | "varchar" | "character varying" | "char" | "character" | "bpchar" | "name"
        | "citext" => cast_to(operand, "TEXT"),
        "bool" | "boolean" => bool_cast(operand),
        "bytea" => match single_str {
            Some(text) if text.starts_with("\\x") => {
                let bytes = type_map::parse_bytea_text(&text)?;
                vec![raw(&hex_literal(&bytes))]
            }
            Some(text) => vec![raw(&hex_literal(text.as_bytes()))],
            None => cast_to(operand, "BLOB"),
        },
        "date" => {
            if single_str.is_some() {
                operand
            } else {
                let mut out = vec![word("date"), op("(")];
                out.extend(operand);
                out.push(op(")"));
                out
            }
        }
        "timestamp" | "timestamptz" | "time" | "timetz" | "json" | "jsonb" | "uuid"
        | "regclass" | "regtype" | "regproc" | "inet" | "cidr" | "macaddr" | "xml" | "money"
        | "interval" => operand,
        other => {
            return Err(SqlError::Unsupported(format!(
                "type \"{other}\" is not supported by NovaDB"
            )));
        }
    })
}

fn rewrite_casts(tokens: &mut Vec<Token>) -> SqlResult<()> {
    // Rewrite the left-most "::" first; chained casts nest naturally.
    while let Some(position) = tokens.iter().position(|t| t.is_op("::")) {
        let start = operand_start(tokens, position)
            .ok_or_else(|| SqlError::Syntax("syntax error at or near \"::\"".into()))?;
        let (name, array, used) = parse_type_name(&tokens[position + 1..])
            .ok_or_else(|| SqlError::Syntax("syntax error at or near \"::\"".into()))?;
        if array {
            return Err(SqlError::Unsupported(
                "array types (::type[]) are not supported by NovaDB".into(),
            ));
        }
        let end = position + 1 + used;
        let operand: Vec<Token> = tokens[start..position].to_vec();
        let replacement = cast_expression(operand, &name)?;
        splice(tokens, start, end, replacement);
    }
    // CAST(x AS type) with PostgreSQL-only type names.
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i].is_word("CAST") && tokens.get(i + 1).is_some_and(|t| t.is_op("(")) {
            if let Some(close) = find_close(tokens, i + 1) {
                if let Some(as_at) = tokens[i + 2..close].iter().rposition(|t| t.is_word("AS")) {
                    let as_at = i + 2 + as_at;
                    if let Some((name, array, used)) = parse_type_name(&tokens[as_at + 1..close]) {
                        let native = matches!(
                            name.as_str(),
                            "blob" | "integer" | "real" | "text" | "numeric"
                        );
                        if native && !array {
                            i += 1;
                            continue;
                        }
                        if array {
                            return Err(SqlError::Unsupported(
                                "array types are not supported by NovaDB".into(),
                            ));
                        }
                        if as_at + 1 + used == close {
                            let operand = tokens[i + 2..as_at].to_vec();
                            let replacement = cast_expression(operand, &name)?;
                            splice(tokens, i, close + 1, replacement);
                        }
                    }
                }
            }
        }
        i += 1;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Regex operators
// ---------------------------------------------------------------------------

/// Parses a simple anchored literal regex (`^(a|b)$`, `^abc`, `abc$`, `abc`).
/// Returns (alternatives, anchored at start, anchored at end).
fn simple_regex(pattern: &str) -> Option<(Vec<String>, bool, bool)> {
    let mut body = pattern;
    let start = body.starts_with('^');
    if start {
        body = &body[1..];
    }
    let end = body.ends_with('$') && !body.ends_with("\\$");
    if end {
        body = &body[..body.len() - 1];
    }
    if let Some(inner) = body.strip_prefix('(').and_then(|b| b.strip_suffix(')')) {
        if !inner.contains(['(', ')']) {
            body = inner;
        }
    }
    let mut alternatives = Vec::new();
    for alternative in body.split('|') {
        let mut literal = String::new();
        let mut chars = alternative.chars();
        while let Some(ch) = chars.next() {
            match ch {
                '\\' => literal.push(chars.next().filter(|c| !c.is_alphanumeric())?),
                '.' | '*' | '+' | '?' | '[' | ']' | '(' | ')' | '{' | '}' | '^' | '$' => {
                    return None;
                }
                other => literal.push(other),
            }
        }
        alternatives.push(literal);
    }
    Some((alternatives, start, end))
}

/// `x ~ 'pat'` / `x !~* 'pat'` for patterns that are literals with optional
/// anchors and alternation; other regular expressions are rejected.
fn rewrite_regex_ops(tokens: &mut Vec<Token>) -> SqlResult<()> {
    // OPERATOR(pg_catalog.~) -> ~ ; COLLATE pg_catalog.default is dropped.
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i].is_word("OPERATOR")
            && tokens.get(i + 1).is_some_and(|t| t.is_op("("))
            && let Some(close) = find_close(tokens, i + 1)
            && close >= i + 3
            && let Some(last) = tokens
                .get(close - 1)
                .filter(|t| t.kind == Kind::Op)
                .cloned()
        {
            let ws = tokens[i].ws.clone();
            tokens.splice(i..=close, [Token::op(&last.text).with_ws(&ws)]);
        }
        i += 1;
    }
    let mut i = tokens.len();
    while i > 0 {
        i -= 1;
        if !(tokens[i].kind == Kind::Op
            && matches!(tokens[i].text.as_str(), "~" | "~*" | "!~" | "!~*"))
        {
            continue;
        }
        let negate = tokens[i].text.starts_with('!');
        let insensitive = tokens[i].text.ends_with('*');
        let unsupported = || {
            SqlError::Unsupported(
                "regular expression operators are only supported for literal patterns with optional ^ $ anchors and | alternation".into(),
            )
        };
        // Right-hand side: 'literal' or CAST('literal' AS TEXT), then optional COLLATE.
        let (pattern, rhs_end) = match tokens.get(i + 1) {
            Some(t) if t.kind == Kind::Str => (t.value.clone(), i + 2),
            Some(t) if t.is_word("CAST") => {
                let close = find_close(tokens, i + 2).ok_or_else(unsupported)?;
                match tokens.get(i + 3) {
                    Some(s) if s.kind == Kind::Str => (s.value.clone(), close + 1),
                    _ => return Err(unsupported()),
                }
            }
            _ => return Err(unsupported()),
        };
        let start = operand_start(tokens, i).ok_or_else(unsupported)?;
        let lhs: Vec<Token> = tokens[start..i].to_vec();
        let (alternatives, anchored_start, anchored_end) =
            simple_regex(&pattern).ok_or_else(unsupported)?;
        let lhs_sql = crate::lexer::render(&lhs);
        let mut parts = Vec::new();
        for literal in alternatives {
            let part = if anchored_start && anchored_end {
                if insensitive {
                    format!("lower({lhs_sql}) = lower({})", quote_str(&literal))
                } else {
                    format!("({lhs_sql}) = {}", quote_str(&literal))
                }
            } else if insensitive {
                let escaped = literal
                    .replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_");
                let pat = format!(
                    "{}{escaped}{}",
                    if anchored_start { "" } else { "%" },
                    if anchored_end { "" } else { "%" }
                );
                format!("({lhs_sql}) LIKE {} ESCAPE '\\'", quote_str(&pat))
            } else {
                let escaped: String = literal
                    .chars()
                    .map(|c| {
                        if matches!(c, '*' | '?' | '[') {
                            format!("[{c}]")
                        } else {
                            c.to_string()
                        }
                    })
                    .collect();
                let pat = format!(
                    "{}{escaped}{}",
                    if anchored_start { "" } else { "*" },
                    if anchored_end { "" } else { "*" }
                );
                format!("({lhs_sql}) GLOB {}", quote_str(&pat))
            };
            parts.push(part);
        }
        let joined = format!("({})", parts.join(" OR "));
        let sql = if negate {
            format!("(NOT {joined})")
        } else {
            joined
        };
        let ws = tokens[start].ws.clone();
        tokens.splice(start..rhs_end, [Token::raw(sql).with_ws(&ws)]);
        i = start;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Miscellaneous syntax
// ---------------------------------------------------------------------------

fn rewrite_misc(tokens: &mut Vec<Token>) -> SqlResult<()> {
    let mut i = 0;
    while i < tokens.len() {
        let token = &tokens[i];
        if token.is_word("ILIKE") {
            tokens[i].text = "LIKE".into();
        } else if token.is_word("IS") {
            // IS [NOT] DISTINCT FROM
            let mut j = i + 1;
            let negated = tokens.get(j).is_some_and(|t| t.is_word("NOT"));
            if negated {
                j += 1;
            }
            if tokens.get(j).is_some_and(|t| t.is_word("DISTINCT"))
                && tokens.get(j + 1).is_some_and(|t| t.is_word("FROM"))
            {
                let replacement = if negated {
                    vec![word("IS")]
                } else {
                    vec![word("IS"), word("NOT")]
                };
                splice(tokens, i, j + 2, replacement);
            }
        } else if token.is_word("FOR")
            && tokens
                .get(i + 1)
                .is_some_and(|t| t.is_any_word(&["UPDATE", "SHARE", "NO", "KEY"]))
        {
            // Row locks have no SQLite equivalent (writes are serialised).
            let mut end = i + 1;
            let mut depth = 0_i32;
            while end < tokens.len() {
                let t = &tokens[end];
                if t.is_op("(") {
                    depth += 1;
                } else if t.is_op(")") {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                } else if depth == 0
                    && t.is_any_word(&["LIMIT", "OFFSET", "ORDER", "UNION", "RETURNING"])
                {
                    break;
                }
                end += 1;
            }
            tokens.drain(i..end);
            continue;
        } else if token.is_word("COLLATE") {
            if let Some(name) = tokens.get(i + 1).and_then(Token::ident) {
                if !matches!(name.as_str(), "nocase" | "binary" | "rtrim") {
                    let mut end = i + 2;
                    while tokens.get(end).is_some_and(|t| t.is_op("."))
                        && tokens.get(end + 1).is_some_and(|t| t.ident().is_some())
                    {
                        end += 2;
                    }
                    tokens.drain(i..end);
                    continue;
                }
            }
        } else if token.is_word("LIMIT") && tokens.get(i + 1).is_some_and(|t| t.is_word("ALL")) {
            tokens.drain(i..i + 2);
            continue;
        } else if token.is_word("ARRAY") && tokens.get(i + 1).is_some_and(|t| t.is_op("[")) {
            return Err(SqlError::Unsupported(
                "ARRAY[...] expressions are not supported by NovaDB".into(),
            ));
        } else if token.is_word("AT")
            && tokens.get(i + 1).is_some_and(|t| t.is_word("TIME"))
            && tokens.get(i + 2).is_some_and(|t| t.is_word("ZONE"))
        {
            return Err(SqlError::Unsupported(
                "AT TIME ZONE is not supported by NovaDB (timestamps are stored as UTC text)"
                    .into(),
            ));
        }
        i += 1;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// DDL
// ---------------------------------------------------------------------------

fn unique_index_name(table: &str, columns: &[String]) -> String {
    format!("{table}_{}_key", columns.join("_"))
}

fn rewrite_ddl(
    tokens: &mut Vec<Token>,
    rebuild: &mut Option<Rebuild>,
) -> SqlResult<Vec<Vec<Token>>> {
    let lead = leading_lower(tokens, 3);
    let first = lead.first().map(String::as_str).unwrap_or("");
    let mut extra = Vec::new();
    match first {
        "create" => {
            let is_table = tokens.iter().take(8).any(|t| t.is_word("TABLE"));
            let is_index = tokens.iter().take(4).any(|t| t.is_word("INDEX"));
            if is_index {
                // CONCURRENTLY and USING <method> have no SQLite equivalent.
                tokens.retain(|t| !t.is_word("CONCURRENTLY"));
                if let Some(at) = tokens.iter().position(|t| t.is_word("USING")) {
                    if at > 0 && tokens.get(at + 1).is_some_and(|t| t.kind == Kind::Word) {
                        tokens.drain(at..at + 2);
                    }
                }
            }
            if is_table {
                rewrite_create_table(tokens)?;
            }
            if tokens.iter().take(3).any(|t| t.is_word("SCHEMA")) {
                return Err(SqlError::Unsupported(
                    "CREATE SCHEMA is not supported by NovaDB (only the public schema exists)"
                        .into(),
                ));
            }
        }
        "drop" => {
            tokens.retain(|t| !t.is_any_word(&["CASCADE", "RESTRICT"]));
            // DROP TABLE a, b -> separate statements.
            if lead
                .get(1)
                .is_some_and(|w| matches!(w.as_str(), "table" | "view" | "index"))
            {
                let mut head = 2;
                if tokens.get(head).is_some_and(|t| t.is_word("IF")) {
                    head += 3;
                }
                let names = split_commas(&tokens[head..]);
                if names.len() > 1 {
                    let prefix: Vec<Token> = tokens[..head].to_vec();
                    let rest: Vec<Vec<Token>> = names.iter().map(|n| n.to_vec()).collect();
                    tokens.truncate(head);
                    tokens.extend(rest[0].clone());
                    for name in &rest[1..] {
                        let mut statement = prefix.clone();
                        statement.extend(name.clone());
                        extra.push(statement);
                    }
                }
            }
        }
        "alter" => rewrite_alter(tokens, rebuild)?,
        _ => {}
    }
    Ok(extra)
}

fn rewrite_create_table(tokens: &mut Vec<Token>) -> SqlResult<()> {
    // Drop "(n)" before WITH/WITHOUT TIME ZONE and GENERATED .. AS IDENTITY.
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i].is_any_word(&["timestamp", "time"])
            && tokens.get(i + 1).is_some_and(|t| t.is_op("("))
            && let Some(close) = find_close(tokens, i + 1)
            && tokens
                .get(close + 1)
                .is_some_and(|t| t.is_any_word(&["WITH", "WITHOUT"]))
        {
            tokens.drain(i + 1..=close);
        }
        if tokens[i].is_word("GENERATED")
            && tokens
                .get(i + 1)
                .is_some_and(|t| t.is_any_word(&["ALWAYS", "BY"]))
        {
            // GENERATED { ALWAYS | BY DEFAULT } AS IDENTITY [(options)]
            let mut end = i + 1;
            while end < tokens.len() && !tokens[end].is_word("IDENTITY") {
                end += 1;
            }
            if end < tokens.len() {
                end += 1;
                if tokens.get(end).is_some_and(|t| t.is_op("(")) {
                    if let Some(close) = find_close(tokens, end) {
                        end = close + 1;
                    }
                }
                tokens.drain(i..end);
                // Turn the preceding integer type into a SERIAL type.
                if i > 0 && tokens[i - 1].kind == Kind::Word {
                    let serial = match tokens[i - 1].lower().as_str() {
                        "int" | "integer" | "int4" => Some("SERIAL"),
                        "bigint" | "int8" => Some("BIGSERIAL"),
                        "smallint" | "int2" => Some("SMALLSERIAL"),
                        _ => None,
                    };
                    if let Some(serial) = serial {
                        tokens[i - 1].text = serial.into();
                    }
                }
                continue;
            }
        }
        i += 1;
    }
    rewrite_serials(tokens);
    // A bare JSON type has NUMERIC affinity in SQLite (a JSON string `'123'`
    // would turn into a number); a quoted type name keeps the column reported
    // as json without that affinity quirk.
    for i in 0..tokens.len() {
        if tokens[i].is_word("json") && !tokens.get(i + 1).is_some_and(|t| t.is_op("(")) {
            let ws = tokens[i].ws.clone();
            let mut quoted = qident("json");
            quoted.ws = ws;
            tokens[i] = quoted;
        }
    }
    // Defaults that are expressions need parentheses in SQLite.
    wrap_defaults(tokens);
    Ok(())
}

/// `SERIAL`-family types: SQLite only auto-assigns ids for exactly
/// `INTEGER PRIMARY KEY`, so a serial primary key becomes
/// `INTEGER PRIMARY KEY AUTOINCREMENT` (ids are never reused, like a
/// sequence); a serial column that is not the primary key keeps its width.
fn rewrite_serials(tokens: &mut Vec<Token>) {
    let mut i = 0;
    while i < tokens.len() {
        let width = match tokens[i].lower().as_str() {
            "serial" | "serial4" if tokens[i].kind == Kind::Word => Some("INTEGER"),
            "bigserial" | "serial8" if tokens[i].kind == Kind::Word => Some("BIGINT"),
            "smallserial" | "serial2" if tokens[i].kind == Kind::Word => Some("SMALLINT"),
            _ => None,
        };
        if let Some(width) = width {
            let mut at = i + 1;
            if tokens.get(at).is_some_and(|t| t.is_word("NOT"))
                && tokens.get(at + 1).is_some_and(|t| t.is_word("NULL"))
            {
                at += 2;
            }
            if tokens.get(at).is_some_and(|t| t.is_word("PRIMARY"))
                && tokens.get(at + 1).is_some_and(|t| t.is_word("KEY"))
            {
                tokens[i].text = "INTEGER".into();
                let mut auto = word("AUTOINCREMENT");
                auto.ws = " ".into();
                tokens.insert(at + 2, auto);
            } else {
                tokens[i].text = width.into();
            }
        }
        i += 1;
    }
}

/// `DEFAULT <expr>` -> `DEFAULT (<expr>)` unless the expression is a literal.
fn wrap_defaults(tokens: &mut Vec<Token>) {
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i].is_word("DEFAULT") {
            let start = i + 1;
            let Some(first) = tokens.get(start) else {
                break;
            };
            let literal = matches!(first.kind, Kind::Str | Kind::Num)
                || first.is_op("(")
                || (first.is_op("-") && tokens.get(start + 1).is_some_and(|t| t.kind == Kind::Num))
                || first.is_any_word(&[
                    "NULL",
                    "TRUE",
                    "FALSE",
                    "CURRENT_TIMESTAMP",
                    "CURRENT_DATE",
                    "CURRENT_TIME",
                ])
                || (first.kind == Kind::Raw
                    && (first.text == "CURRENT_TIMESTAMP"
                        || first.text.starts_with("X'")
                        || first.text.parse::<f64>().is_ok()));
            if !literal {
                let mut end = start;
                let mut depth = 0_i32;
                while end < tokens.len() {
                    let t = &tokens[end];
                    if t.is_op("(") {
                        depth += 1;
                    } else if t.is_op(")") {
                        if depth == 0 {
                            break;
                        }
                        depth -= 1;
                    } else if depth == 0
                        && (t.is_op(",")
                            || t.is_any_word(&[
                                "NOT",
                                "NULL",
                                "PRIMARY",
                                "UNIQUE",
                                "REFERENCES",
                                "CHECK",
                                "CONSTRAINT",
                                "COLLATE",
                                "GENERATED",
                                "DEFAULT",
                            ]))
                    {
                        break;
                    }
                    end += 1;
                }
                if end > start {
                    let inner: Vec<Token> = tokens[start..end].to_vec();
                    splice(tokens, start, end, paren(inner));
                }
            }
        }
        i += 1;
    }
}

fn rewrite_alter(tokens: &mut Vec<Token>, rebuild: &mut Option<Rebuild>) -> SqlResult<()> {
    if !tokens.get(1).is_some_and(|t| t.is_word("TABLE")) {
        return Ok(());
    }
    let mut i = 2;
    if tokens.get(i).is_some_and(|t| t.is_word("IF")) {
        i += 3; // IF EXISTS
    }
    if tokens.get(i).is_some_and(|t| t.is_word("ONLY")) {
        tokens.remove(i);
    }
    let table = tokens
        .get(i)
        .and_then(Token::ident)
        .ok_or_else(|| SqlError::Syntax("syntax error in ALTER TABLE".into()))?;
    let action = i + 1;
    let action_word = tokens.get(action).map(Token::lower).unwrap_or_default();
    let second = tokens.get(action + 1).map(Token::lower).unwrap_or_default();
    match (action_word.as_str(), second.as_str()) {
        ("add", "constraint")
        | ("add", "unique")
        | ("add", "primary")
        | ("add", "foreign")
        | ("add", "check")
        | ("add", "exclude") => {
            let mut j = action + 1;
            let mut name = None;
            if tokens[j].is_word("CONSTRAINT") {
                name = tokens.get(j + 1).and_then(Token::ident);
                j += 2;
            }
            let kind = tokens.get(j).map(Token::lower).unwrap_or_default();
            if kind == "unique" {
                let open = j + 1;
                if !tokens.get(open).is_some_and(|t| t.is_op("(")) {
                    return Err(SqlError::Syntax(
                        "syntax error in ADD CONSTRAINT UNIQUE".into(),
                    ));
                }
                let close = find_close(tokens, open)
                    .ok_or_else(|| SqlError::Syntax("unbalanced parentheses".into()))?;
                let columns: Vec<String> = split_commas(&tokens[open + 1..close])
                    .iter()
                    .filter_map(|part| part.first().and_then(Token::ident))
                    .collect();
                let index = name.unwrap_or_else(|| unique_index_name(&table, &columns));
                let mut out = vec![
                    word("CREATE"),
                    word("UNIQUE"),
                    word("INDEX"),
                    Token::new(Kind::QIdent, "").with_ws(" "),
                    word("ON"),
                    Token::new(Kind::QIdent, "").with_ws(" "),
                ];
                out[3].value = index;
                out[5].value = table;
                out.extend(tokens[open..=close].iter().cloned());
                *tokens = out;
                return Ok(());
            }
            let what = match kind.as_str() {
                "primary" => "PRIMARY KEY",
                "foreign" => "FOREIGN KEY",
                "check" => "CHECK",
                _ => {
                    return Err(SqlError::Unsupported(
                        "ALTER TABLE ... ADD EXCLUDE is not supported by NovaDB".into(),
                    ));
                }
            };
            // Drop PostgreSQL-only trailing options.
            let mut end = tokens.len();
            if let Some(at) = tokens[j..]
                .windows(2)
                .position(|w| w[0].is_word("NOT") && w[1].is_word("VALID"))
            {
                end = j + at;
            }
            let start = if name.is_some() { j - 2 } else { j };
            let mut clause_tokens = tokens[start..end].to_vec();
            if let Some(first) = clause_tokens.first_mut() {
                first.ws.clear();
            }
            *rebuild = Some(Rebuild {
                table,
                clause: crate::lexer::render(&clause_tokens),
                kind: what,
            });
            tokens.clear();
            return Ok(());
        }
        ("add", "column") | ("add", _) => {
            let mut j = action + 1;
            if tokens.get(j).is_some_and(|t| t.is_word("COLUMN")) {
                j += 1;
            }
            // ADD COLUMN IF NOT EXISTS is not supported by SQLite.
            if tokens.get(j).is_some_and(|t| t.is_word("IF")) {
                tokens.drain(j..j + 3);
            }
            let mut sub = tokens[j..].to_vec();
            rewrite_create_table(&mut sub)?;
            tokens.truncate(j);
            tokens.extend(sub);
        }
        ("alter", _) => {
            return Err(SqlError::Unsupported(
                "ALTER TABLE ... ALTER COLUMN is not supported by SQLite; create a new table and copy the data"
                    .into(),
            ));
        }
        ("drop", "constraint") => {
            let mut j = action + 2;
            let mut if_exists = false;
            if tokens.get(j).is_some_and(|t| t.is_word("IF")) {
                if_exists = true;
                j += 2;
            }
            let name = tokens
                .get(j)
                .and_then(Token::ident)
                .ok_or_else(|| SqlError::Syntax("syntax error in DROP CONSTRAINT".into()))?;
            let mut out = vec![word("DROP"), word("INDEX")];
            if if_exists {
                out.extend([word("IF"), word("EXISTS")]);
            }
            let mut ident = Token::new(Kind::QIdent, "").with_ws(" ");
            ident.value = name;
            out.push(ident);
            *tokens = out;
        }
        ("rename", "constraint") => {
            return Err(SqlError::Unsupported(
                "ALTER TABLE ... RENAME CONSTRAINT is not supported by NovaDB".into(),
            ));
        }
        _ => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// DEFAULT in VALUES / SET
// ---------------------------------------------------------------------------

fn default_tokens(
    schema: &Schema,
    table: &str,
    column: Option<&str>,
    position: usize,
) -> Vec<Token> {
    let default = schema.table(table).and_then(|info| {
        let column = match column {
            Some(name) => info.column(name),
            None => info.columns.get(position),
        };
        column.and_then(|c| c.default.clone())
    });
    match default {
        Some(text) => vec![raw(&format!("({text})"))],
        None => vec![raw("NULL")],
    }
}

fn is_bytea_column(schema: &Schema, table: &str, column: Option<&str>, position: usize) -> bool {
    schema.table(table).is_some_and(|info| {
        let column = match column {
            Some(name) => info.column(name),
            None => info.columns.get(position),
        };
        column.is_some_and(|c| type_map::sqlite_type_to_pg_oid(&c.decl) == oid::BYTEA)
    })
}

/// A `'\x..'` string literal bound for a bytea column becomes a BLOB literal.
fn bytea_literal(token: &Token) -> Option<Token> {
    if token.kind != Kind::Str || !token.value.starts_with("\\x") {
        return None;
    }
    let bytes = type_map::parse_bytea_text(&token.value).ok()?;
    Some(raw(&hex_literal(&bytes)).with_ws(&token.ws))
}

/// Resolves `DEFAULT` keywords in `VALUES` rows / `SET col = DEFAULT`, and
/// turns hex string literals into BLOBs for bytea columns (clients that
/// interpolate parameters client-side send bytea as `'\x..'`).
fn rewrite_defaults(tokens: &mut Vec<Token>, env: &Env<'_>) -> SqlResult<()> {
    let first = tokens.first().map(Token::lower).unwrap_or_default();
    if first == "insert" {
        let Some(values_at) = tokens.iter().position(|t| t.is_word("VALUES")) else {
            return Ok(());
        };
        let needs_work = tokens[values_at..]
            .iter()
            .any(|t| t.is_word("DEFAULT") || (t.kind == Kind::Str && t.value.starts_with("\\x")));
        if !needs_work {
            return Ok(());
        }
        // Table and column list.
        let mut i = 2;
        let mut table = tokens.get(i).and_then(Token::ident).unwrap_or_default();
        i += 1;
        while tokens.get(i).is_some_and(|t| t.is_op(".")) {
            table = tokens.get(i + 1).and_then(Token::ident).unwrap_or_default();
            i += 2;
        }
        if tokens.get(i).is_some_and(|t| t.is_word("AS")) {
            i += 2;
        }
        let mut columns: Vec<String> = Vec::new();
        if tokens.get(i).is_some_and(|t| t.is_op("(")) {
            if let Some(close) = find_close(tokens, i) {
                columns = split_commas(&tokens[i + 1..close])
                    .iter()
                    .filter_map(|p| p.first().and_then(Token::ident))
                    .collect();
            }
        }
        // Walk the row groups after VALUES.
        let mut j = values_at + 1;
        while j < tokens.len() {
            if tokens[j].is_op("(") {
                let Some(close) = find_close(tokens, j) else {
                    break;
                };
                let mut position = 0;
                let mut k = j + 1;
                let mut item_start = k;
                let mut depth = 0_i32;
                let mut replacements: Vec<(usize, Vec<Token>)> = Vec::new();
                while k <= close {
                    let t = &tokens[k];
                    let at_end = k == close;
                    if t.is_op("(") {
                        depth += 1;
                    } else if t.is_op(")") && !at_end {
                        depth -= 1;
                    }
                    if (at_end || (depth == 0 && t.is_op(","))) && k > item_start {
                        let column = columns.get(position).map(String::as_str);
                        if k - item_start == 1 {
                            if tokens[item_start].is_word("DEFAULT") {
                                replacements.push((
                                    item_start,
                                    default_tokens(env.schema, &table, column, position),
                                ));
                            } else if let Some(blob) = bytea_literal(&tokens[item_start]) {
                                if is_bytea_column(env.schema, &table, column, position) {
                                    replacements.push((item_start, vec![blob]));
                                }
                            }
                        }
                        position += 1;
                        item_start = k + 1;
                    }
                    k += 1;
                }
                for (at, replacement) in replacements.into_iter().rev() {
                    splice(tokens, at, at + 1, replacement);
                }
                let close = find_close(tokens, j).unwrap_or(close);
                j = close + 1;
            } else {
                j += 1;
            }
        }
    } else if first == "update" {
        // SET col = DEFAULT / SET col = '\x..'
        let table = tokens.get(1).and_then(Token::ident).unwrap_or_default();
        let mut i = 0;
        while i + 2 < tokens.len() {
            if tokens[i + 1].is_op("=") {
                if let Some(column) = tokens[i].ident() {
                    if tokens[i + 2].is_word("DEFAULT") {
                        let replacement = default_tokens(env.schema, &table, Some(&column), 0);
                        splice(tokens, i + 2, i + 3, replacement);
                    } else if let Some(blob) = bytea_literal(&tokens[i + 2]) {
                        if is_bytea_column(env.schema, &table, Some(&column), 0) {
                            splice(tokens, i + 2, i + 3, vec![blob]);
                        }
                    }
                }
            }
            i += 1;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Statement properties
// ---------------------------------------------------------------------------

/// Statement kind used to choose the execution path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementKind {
    Query,
    Insert,
    Update,
    Delete,
    /// CREATE / DROP / ALTER and other schema or administrative statements.
    Other,
}

pub fn statement_kind(tokens: &[Token]) -> StatementKind {
    let skip = analysis::skip_with(tokens);
    let word = tokens.get(skip).map(Token::lower).unwrap_or_default();
    match word.as_str() {
        "select" | "values" | "explain" | "pragma" | "table" => StatementKind::Query,
        "insert" | "replace" => StatementKind::Insert,
        "update" => StatementKind::Update,
        "delete" => StatementKind::Delete,
        _ if tokens.first().is_some_and(|t| t.is_op("(")) => StatementKind::Query,
        _ => StatementKind::Other,
    }
}

/// Command tag for a non-row statement (without the affected-row suffix).
pub fn command_tag(tokens: &[Token]) -> String {
    let words = leading_lower(tokens, 6);
    let first = words.first().cloned().unwrap_or_default();
    let object = |from: usize| {
        words
            .iter()
            .skip(from)
            .find(|w| {
                matches!(
                    w.as_str(),
                    "table"
                        | "index"
                        | "view"
                        | "trigger"
                        | "schema"
                        | "sequence"
                        | "type"
                        | "extension"
                        | "database"
                )
            })
            .map(|w| w.to_ascii_uppercase())
    };
    match first.as_str() {
        "create" | "drop" | "alter" => match object(1) {
            Some(object) => format!("{} {object}", first.to_ascii_uppercase()),
            None => first.to_ascii_uppercase(),
        },
        "" => "OK".into(),
        other => other.to_ascii_uppercase(),
    }
}

/// Name of the object created by `CREATE [UNIQUE] TABLE|INDEX|VIEW name`
/// when the statement lacks `IF NOT EXISTS`.
pub fn created_object(tokens: &[Token]) -> Option<String> {
    if !tokens.first()?.is_word("CREATE") {
        return None;
    }
    let mut i = 1;
    while tokens
        .get(i)
        .is_some_and(|t| t.is_any_word(&["UNIQUE", "TEMP", "TEMPORARY"]))
    {
        i += 1;
    }
    if !tokens.get(i)?.is_any_word(&["TABLE", "INDEX", "VIEW"]) {
        return None;
    }
    i += 1;
    if tokens.get(i).is_some_and(|t| t.is_word("IF")) {
        return None;
    }
    let mut name = tokens.get(i)?.ident()?;
    i += 1;
    while tokens.get(i).is_some_and(|t| t.is_op(".")) {
        name = tokens.get(i + 1)?.ident()?;
        i += 2;
    }
    Some(name)
}

/// The table written by an INSERT / UPDATE / DELETE statement.
pub fn target_table(tokens: &[Token]) -> Option<analysis::TableRef> {
    let skip = analysis::skip_with(tokens);
    let kind = tokens.get(skip)?.lower();
    let mut i = skip
        + match kind.as_str() {
            "insert" => 2,
            "delete" => 2,
            "update" => 1,
            _ => return None,
        };
    if kind == "insert" && tokens.get(skip + 1).is_some_and(|t| t.is_word("OR")) {
        i += 2;
    }
    if tokens.get(i).is_some_and(|t| t.is_word("ONLY")) {
        i += 1;
    }
    let mut name = tokens.get(i)?.ident()?;
    i += 1;
    while tokens.get(i).is_some_and(|t| t.is_op(".")) {
        name = tokens.get(i + 1)?.ident()?;
        i += 2;
    }
    let mut alias = name.clone();
    if tokens.get(i).is_some_and(|t| t.is_word("AS")) {
        alias = tokens.get(i + 1)?.ident()?;
    } else if let Some(t) = tokens.get(i)
        && t.ident().is_some()
        && !t.is_any_word(&[
            "SET",
            "VALUES",
            "SELECT",
            "WHERE",
            "DEFAULT",
            "USING",
            "ON",
            "WITH",
            "OVERRIDING",
        ])
    {
        alias = t.ident().unwrap_or_default();
    }
    Some(analysis::TableRef { name, alias })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::{render, tokenize};

    fn translate_with(sql: &str, schema: &Schema) -> SqlResult<String> {
        let vars = Vars::new("app");
        let env = Env {
            database: "db1",
            user: "app",
            pid: 4242,
            vars: &vars,
            schema,
        };
        let rewritten = rewrite(tokenize(sql)?, &env)?;
        let mut sql = render(&rewritten.tokens);
        if let Some(returning) = rewritten.returning {
            sql.push_str(" RETURNING");
            sql.push_str(&render(&returning));
        }
        Ok(sql)
    }

    fn t(sql: &str) -> SqlResult<String> {
        translate_with(sql, &Schema::default())
    }

    #[test]
    fn casts_become_sqlite_casts() {
        assert_eq!(
            t("SELECT 1::int").unwrap(),
            "SELECT CAST(1 AS INTEGER) AS \"int4\""
        );
        assert_eq!(
            t("SELECT x::varchar(10), y::double precision FROM t").unwrap(),
            "SELECT CAST(x AS TEXT) AS \"x\", CAST(y AS REAL) AS \"y\" FROM t"
        );
        assert_eq!(
            t("SELECT 'a'::text || 'b'").unwrap(),
            "SELECT CAST('a' AS TEXT) || 'b' AS \"?column?\""
        );
        assert_eq!(
            t("SELECT f(a, b)::bigint, (1 + 2)::int, t.c::text FROM t").unwrap(),
            "SELECT CAST(f(a, b) AS INTEGER) AS \"f\", CAST((1 + 2) AS INTEGER) AS \"int4\", CAST(t.c AS TEXT) AS \"c\" FROM t"
        );
    }

    #[test]
    fn literals_and_comments_are_never_rewritten() {
        let sql = t("SELECT '::int', \"a::int\", 1 /* x::int */ -- y::int\n FROM t").unwrap();
        assert!(sql.contains("'::int'"), "{sql}");
        assert!(sql.contains("\"a::int\""), "{sql}");
        assert!(!sql.contains("CAST"), "{sql}");
    }

    #[test]
    fn bytea_literal_cast_becomes_blob() {
        assert_eq!(
            t("SELECT '\\x00ff'::bytea").unwrap(),
            "SELECT X'00FF' AS \"bytea\""
        );
    }

    #[test]
    fn arrays_are_rejected_with_clear_errors() {
        assert!(matches!(
            t("SELECT x::int[]"),
            Err(SqlError::Unsupported(_))
        ));
        assert!(matches!(
            t("SELECT ARRAY[1,2]"),
            Err(SqlError::Unsupported(_))
        ));
    }

    #[test]
    fn any_array_becomes_in_list() {
        assert_eq!(
            t("SELECT 1 FROM t WHERE c = ANY (ARRAY['r','p'])").unwrap(),
            "SELECT 1 AS \"?column?\" FROM t WHERE c IN ('r','p')"
        );
        assert_eq!(
            t("SELECT 1 FROM t WHERE c = ANY ('{1,2}'::int[])").unwrap(),
            "SELECT 1 AS \"?column?\" FROM t WHERE c IN (1,2)"
        );
        assert_eq!(
            t("SELECT 1 FROM t WHERE c <> ALL ('{a,\"b c\"}')").unwrap(),
            "SELECT 1 AS \"?column?\" FROM t WHERE c NOT IN ('a','b c')"
        );
    }

    #[test]
    fn identity_functions_and_schema_prefixes() {
        let sql = t("SELECT version(), pg_catalog.version(), current_schema(), current_database(), current_user, pg_backend_pid()").unwrap();
        assert!(sql.contains("'PostgreSQL 16.0 (NovaDB"), "{sql}");
        assert!(sql.contains("'public'"), "{sql}");
        assert!(sql.contains("'db1'"), "{sql}");
        assert!(sql.contains("'app'"), "{sql}");
        assert!(sql.contains("4242"), "{sql}");
        assert_eq!(
            t("SELECT * FROM public.users u WHERE \"public\".\"users\".id = 1").unwrap(),
            "SELECT * FROM users u WHERE \"users\".id = 1"
        );
    }

    #[test]
    fn misc_syntax() {
        assert_eq!(
            t("SELECT a FROM t WHERE b ILIKE 'x%' AND c IS DISTINCT FROM 1 FOR UPDATE SKIP LOCKED")
                .unwrap(),
            "SELECT a FROM t WHERE b LIKE 'x%' AND c IS NOT 1"
        );
        assert_eq!(
            t("SELECT now()").unwrap(),
            "SELECT CURRENT_TIMESTAMP AS \"now\""
        );
    }

    #[test]
    fn ddl_rewrites() {
        assert_eq!(
            t("ALTER TABLE users ADD CONSTRAINT uq_email UNIQUE (email)").unwrap(),
            "CREATE UNIQUE INDEX \"uq_email\" ON \"users\" (email)"
        );
        // Rebuilt by the session; the main statement is empty.
        assert_eq!(
            t("ALTER TABLE users ADD CONSTRAINT fk FOREIGN KEY (a) REFERENCES b(id)").unwrap(),
            ""
        );
        assert_eq!(
            t("CREATE TABLE t (id int, at timestamp(6) with time zone DEFAULT now(), v varchar DEFAULT 'x'::varchar NOT NULL)").unwrap(),
            "CREATE TABLE t (id int, at timestamp with time zone DEFAULT CURRENT_TIMESTAMP, v varchar DEFAULT (CAST('x' AS TEXT)) NOT NULL)"
        );
        assert_eq!(
            t("CREATE TABLE t (id integer GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY)").unwrap(),
            "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT)"
        );
        assert_eq!(
            t("CREATE INDEX CONCURRENTLY ix ON t USING btree (a)").unwrap(),
            "CREATE INDEX ix ON t (a)"
        );
        assert_eq!(
            t("DROP TABLE IF EXISTS a CASCADE").unwrap(),
            "DROP TABLE IF EXISTS a"
        );
        assert!(matches!(
            t("CREATE EXTENSION pgcrypto"),
            Err(SqlError::Unsupported(_))
        ));
    }

    #[test]
    fn select_list_labels_are_unique_and_postgres_named() {
        let vars = Vars::new("app");
        let schema = Schema::default();
        let env = Env {
            database: "d",
            user: "u",
            pid: 1,
            vars: &vars,
            schema: &schema,
        };
        let rewritten = rewrite(
            tokenize("SELECT count(*), a.id, b.id, 1, 2 FROM a, b").unwrap(),
            &env,
        )
        .unwrap();
        assert_eq!(
            rewritten.labels.unwrap(),
            vec!["count", "id", "id", "?column?", "?column?"]
        );
        let sql = render(&rewritten.tokens);
        assert!(sql.contains("count(*) AS \"count\""), "{sql}");
        assert!(sql.contains("a.id AS \"id__1\""), "{sql}");
    }

    #[test]
    fn substitution_is_single_pass_and_escapes_values() {
        let tokens = tokenize("SELECT $1, $2, $3, 1 -$4, '$1', \"$2\" -- $1").unwrap();
        let values = vec![
            ParamValue::Text("x$2".into()),
            ParamValue::Text("' OR 1=1 --".into()),
            ParamValue::Null,
            ParamValue::Int(-5),
        ];
        assert_eq!(
            render(&substitute_params(&tokens, &values)),
            "SELECT 'x$2', ''' OR 1=1 --', NULL, 1 -(-5), '$1', \"$2\""
        );
        let tokens = tokenize("SELECT $10").unwrap();
        assert_eq!(render(&substitute_params(&tokens, &values)), "SELECT NULL");
        let tokens = tokenize("SELECT $1, $2, $3").unwrap();
        let values = vec![
            ParamValue::Untyped("007".into()),
            ParamValue::Untyped("42".into()),
            ParamValue::Bytes(vec![0xFF, 0x00]),
        ];
        assert_eq!(
            render(&substitute_params(&tokens, &values)),
            "SELECT '007', 42, X'FF00'"
        );
    }

    #[test]
    fn split_returning_extracts_clause() {
        let (main, returning) =
            split_returning(tokenize("INSERT INTO t (a) VALUES (1) RETURNING id, a").unwrap());
        assert_eq!(render(&main), "INSERT INTO t (a) VALUES (1)");
        assert_eq!(returning.unwrap().len(), 3);
        let (main, returning) = split_returning(tokenize("SELECT 1 RETURNING").unwrap());
        assert!(returning.is_none());
        assert_eq!(main.len(), 3);
    }

    #[test]
    fn command_tags() {
        assert_eq!(
            command_tag(&tokenize("CREATE UNIQUE INDEX i ON t(a)").unwrap()),
            "CREATE INDEX"
        );
        assert_eq!(
            command_tag(&tokenize("create table t(a int)").unwrap()),
            "CREATE TABLE"
        );
        assert_eq!(command_tag(&tokenize("DROP VIEW v").unwrap()), "DROP VIEW");
        assert_eq!(
            command_tag(&tokenize("ALTER TABLE t ADD COLUMN a int").unwrap()),
            "ALTER TABLE"
        );
    }
}
