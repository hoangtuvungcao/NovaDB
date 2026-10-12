//! MySQL → SQLite dialect translation on top of the token stream.
//!
//! Translation is structural (statement-level rewrites of DDL and DML
//! shapes) followed by an expression pass (functions, operators, system
//! variables). Placeholders are substituted first, in a single pass, so bound
//! values are never re-scanned and can never be mistaken for SQL.

use std::collections::HashMap;

use crate::ddl::RebuildEdit;
use crate::lexer::{
    Kind, SqlError, SqlResult, Token, find_close, find_top, quote_ident, quote_str, render,
    source_text, split_commas, split_top,
};
use crate::meta;

/// Session context used to resolve variables and session functions.
pub struct Env<'a> {
    pub database: &'a str,
    pub user: &'a str,
    pub host: &'a str,
    pub connection_id: u32,
    pub last_insert_id: u64,
    /// Resolves a system variable to a SQL literal.
    pub system_var: &'a dyn Fn(&str) -> Option<String>,
    /// User variables as SQL literals.
    pub user_vars: &'a HashMap<String, String>,
    /// Column metadata of tables the statement refers to, pre-fetched by the
    /// session (used to resolve `DEFAULT` in `INSERT ... VALUES`).
    pub tables: &'a HashMap<String, Vec<ColumnInfo>>,
    /// Declared foreign key names for `information_schema` (see `meta::FkNames`).
    pub fk_names: &'a meta::FkNames,
}

/// Declared column facts needed to translate `DEFAULT` keywords.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: String,
    /// Declared default as a SQLite expression.
    pub default: Option<String>,
    /// `INTEGER PRIMARY KEY` (rowid alias): `NULL` assigns the next id.
    pub rowid: bool,
}

/// One stage of an `ALTER TABLE`.
enum AlterStep {
    /// Statements SQLite can run in place.
    Statements(Vec<Vec<Token>>),
    /// A change applied by rebuilding the table (`rename` is an optional
    /// `RENAME COLUMN old TO new` to run first).
    Rebuild {
        edit: RebuildEdit,
        rename: Option<(String, String)>,
    },
}

fn flush(statements: &mut Vec<Vec<Token>>, steps: &mut Vec<AlterStep>) {
    if !statements.is_empty() {
        steps.push(AlterStep::Statements(std::mem::take(statements)));
    }
}

/// A rendered `ALTER TABLE` step (see [`alter_plan`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanStep {
    Sql(Vec<String>),
    Rebuild {
        edit: RebuildEdit,
        rename: Option<(String, String)>,
    },
}

/// For an `ALTER TABLE` that needs a table rebuild (`ADD/DROP FOREIGN KEY`,
/// `MODIFY`/`CHANGE COLUMN`, ...) returns the ordered steps; `None` when the
/// statement can be translated directly.
pub fn alter_plan(
    statement: &[Token],
    env: &Env<'_>,
    params: &[String],
) -> SqlResult<Option<(String, Vec<PlanStep>)>> {
    let tokens = substitute(statement, env, params)?;
    let translator = Translator { env };
    let (table, steps) = translator.alter_steps(&tokens)?;
    if !steps
        .iter()
        .any(|step| matches!(step, AlterStep::Rebuild { .. }))
    {
        return Ok(None);
    }
    let mut plan = Vec::new();
    for step in steps {
        match step {
            AlterStep::Statements(batch) => {
                let mut sql = Vec::new();
                for statement in batch {
                    let rewritten = translator.expressions(&statement, true)?;
                    sql.push(render(&rewritten).trim().to_owned());
                }
                plan.push(PlanStep::Sql(sql));
            }
            AlterStep::Rebuild { edit, rename } => plan.push(PlanStep::Rebuild { edit, rename }),
        }
    }
    Ok(Some((table, plan)))
}

/// Table an `INSERT`/`REPLACE` statement writes to.
pub fn insert_target(tokens: &[Token], database: &str) -> Option<String> {
    let mut i = 1;
    while tokens.get(i).is_some_and(|t| {
        t.is_any_word(&["LOW_PRIORITY", "DELAYED", "HIGH_PRIORITY", "IGNORE", "INTO"])
    }) {
        i += 1;
    }
    table_name_at(tokens, i, database).map(|(table, _)| table)
}

/// Keywords that end a SELECT list at the top level.
const SELECT_LIST_END: &[&str] = &[
    "FROM",
    "WHERE",
    "GROUP",
    "ORDER",
    "LIMIT",
    "HAVING",
    "WINDOW",
    "UNION",
    "INTO",
    "EXCEPT",
    "INTERSECT",
    "FOR",
    "LOCK",
];
/// Words after which `name(` is a table reference, not a function call.
const TABLE_POSITION: &[&str] = &[
    "INTO",
    "TABLE",
    "FROM",
    "JOIN",
    "UPDATE",
    "REFERENCES",
    "ON",
    "EXISTS",
];
const INT_TYPES: &[&str] = &[
    "INT",
    "INTEGER",
    "TINYINT",
    "SMALLINT",
    "MEDIUMINT",
    "BIGINT",
    "SERIAL",
];
const SQLITE_COLLATIONS: &[&str] = &["BINARY", "NOCASE", "RTRIM"];
const CHARSET_INTRODUCERS: &[&str] = &[
    "_utf8mb4", "_utf8mb3", "_utf8", "_latin1", "_binary", "_ascii", "_ucs2", "_utf16", "_utf32",
];

/// Translates one MySQL statement into one or more SQLite statements.
pub fn translate(statement: &[Token], env: &Env<'_>, params: &[String]) -> SqlResult<Vec<String>> {
    let tokens = substitute(statement, env, params)?;
    let tokens = rewrite_limit(tokens);
    let tokens = rewrite_having(tokens)?;
    let translator = Translator { env };
    let statements = translator.structural(tokens)?;
    let mut output = Vec::with_capacity(statements.len());
    for statement in statements {
        let (statement, tables) = rewrite_information_schema(statement);
        let rewritten = translator.expressions(&statement, true)?;
        let sql = render(&rewritten).trim().to_owned();
        if sql.is_empty() {
            continue;
        }
        output.push(meta::with_information_schema(
            &sql,
            &tables,
            env.database,
            env.fk_names,
        )?);
    }
    Ok(output)
}

/// Counts `?` placeholders.
pub fn placeholder_count(statement: &[Token]) -> usize {
    statement
        .iter()
        .filter(|t| t.kind == Kind::Placeholder)
        .count()
}

/// Replaces placeholders and user variables with literal tokens.
fn substitute(statement: &[Token], env: &Env<'_>, params: &[String]) -> SqlResult<Vec<Token>> {
    let mut next_param = 0;
    let mut out = Vec::with_capacity(statement.len());
    for token in statement {
        match token.kind {
            Kind::Placeholder => {
                let value = params.get(next_param).ok_or_else(|| {
                    SqlError::Syntax(
                        "statement contains '?' placeholders but no parameters were bound".into(),
                    )
                })?;
                next_param += 1;
                out.push(Token::literal(value.clone(), &token.ws));
            }
            Kind::UserVar => {
                let value = env.user_vars.get(&token.value).cloned();
                out.push(Token::literal(
                    value.unwrap_or_else(|| "NULL".into()),
                    &token.ws,
                ));
            }
            _ => out.push(token.clone()),
        }
    }
    if next_param != params.len() {
        return Err(SqlError::Syntax(format!(
            "statement has {next_param} placeholders but {} parameters were bound",
            params.len()
        )));
    }
    Ok(out)
}

fn is_atom(token: &Token) -> bool {
    matches!(token.kind, Kind::Number | Kind::Literal)
}

const AGGREGATES: &[&str] = &[
    "COUNT",
    "SUM",
    "MIN",
    "MAX",
    "AVG",
    "GROUP_CONCAT",
    "TOTAL",
    "JSON_ARRAYAGG",
    "JSON_OBJECTAGG",
    "BIT_AND",
    "BIT_OR",
    "BIT_XOR",
    "STD",
    "STDDEV",
    "VARIANCE",
];

/// MySQL accepts `HAVING` without `GROUP BY` on non-aggregate queries as a
/// filter over the select list (Connector/J metadata queries rely on it);
/// SQLite rejects it. `SELECT .. [ORDER BY ..] HAVING h [LIMIT ..]` becomes
/// `SELECT * FROM (SELECT .. [ORDER BY ..]) WHERE h [LIMIT ..]`.
fn rewrite_having(tokens: Vec<Token>) -> SqlResult<Vec<Token>> {
    // Rewrite parenthesized subqueries first.
    let mut flat = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i].is_op("(") {
            let close = find_close(&tokens, i)?;
            flat.push(tokens[i].clone());
            flat.extend(rewrite_having(tokens[i + 1..close].to_vec())?);
            flat.push(tokens[close].clone());
            i = close + 1;
        } else {
            flat.push(tokens[i].clone());
            i += 1;
        }
    }
    let top = |predicate: &dyn Fn(&Token) -> bool| find_top(&flat, 0, predicate);
    let Some(having) = top(&|t| t.is_word("HAVING")) else {
        return Ok(flat);
    };
    let select = top(&|t| t.is_word("SELECT"));
    let blocked = top(&|t| t.is_any_word(&["GROUP", "UNION", "EXCEPT", "INTERSECT", "WINDOW"]));
    let aggregate = flat
        .windows(2)
        .any(|pair| pair[0].is_any_word(AGGREGATES) && pair[1].is_op("("));
    let Some(select) = select.filter(|&s| s < having) else {
        return Ok(flat);
    };
    if blocked.is_some() || aggregate {
        return Ok(flat);
    }
    let after = find_top(&flat, having + 1, |t: &Token| {
        t.is_any_word(&["ORDER", "LIMIT"])
    })
    .unwrap_or(flat.len());
    let condition = &flat[having + 1..after];
    let (order, limit) = match flat.get(after) {
        Some(t) if t.is_word("ORDER") => {
            let limit =
                find_top(&flat, after, |t: &Token| t.is_word("LIMIT")).unwrap_or(flat.len());
            (&flat[after..limit], &flat[limit..])
        }
        _ => (&[][..], &flat[after..]),
    };
    let mut out: Vec<Token> = flat[..select].to_vec();
    out.push(Token::raw("SELECT * FROM (").with_ws(&flat[select].ws));
    let mut inner = flat[select..having].to_vec();
    if let Some(first) = inner.first_mut() {
        first.ws = String::new();
    }
    out.extend(inner);
    out.extend(order.iter().cloned());
    out.push(Token::op(")"));
    out.push(Token::raw("WHERE"));
    out.extend(condition.iter().cloned());
    out.extend(limit.iter().cloned());
    Ok(out)
}

/// `LIMIT offset, count` → `LIMIT count OFFSET offset`.
fn rewrite_limit(tokens: Vec<Token>) -> Vec<Token> {
    let mut out = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i].is_word("LIMIT")
            && tokens.get(i + 1).is_some_and(is_atom)
            && tokens.get(i + 2).is_some_and(|t| t.is_op(","))
            && tokens.get(i + 3).is_some_and(is_atom)
        {
            out.push(tokens[i].clone());
            out.push(tokens[i + 3].clone().with_ws(" "));
            out.push(Token::raw("OFFSET"));
            out.push(tokens[i + 1].clone().with_ws(" "));
            i += 4;
            continue;
        }
        out.push(tokens[i].clone());
        i += 1;
    }
    out
}

/// Replaces `information_schema.<table>` with CTE names.
fn rewrite_information_schema(tokens: Vec<Token>) -> (Vec<Token>, Vec<String>) {
    let mut tables = Vec::new();
    let mut out = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        let is_schema = tokens[i]
            .ident()
            .is_some_and(|name| name.eq_ignore_ascii_case("information_schema"));
        if is_schema && tokens.get(i + 1).is_some_and(|t| t.is_op(".")) {
            if let Some(table) = tokens.get(i + 2).and_then(Token::ident) {
                let table = table.to_ascii_lowercase();
                out.push(Token::raw(meta::information_schema_cte(&table)).with_ws(&tokens[i].ws));
                if !tables.contains(&table) {
                    tables.push(table);
                }
                i += 3;
                continue;
            }
        }
        out.push(tokens[i].clone());
        i += 1;
    }
    (out, tables)
}

/// Strips the current-database qualifier from `db.table` at `index`.
fn table_name_at(tokens: &[Token], index: usize, database: &str) -> Option<(String, usize)> {
    let first = tokens.get(index)?.ident()?.to_owned();
    if tokens.get(index + 1).is_some_and(|t| t.is_op(".")) {
        let second = tokens.get(index + 2)?.ident()?.to_owned();
        if first != database {
            return None;
        }
        return Some((second, index + 3));
    }
    Some((first, index + 1))
}

struct Translator<'a> {
    env: &'a Env<'a>,
}

impl Translator<'_> {
    // ----------------------------------------------------------------------
    // Structural rewrites
    // ----------------------------------------------------------------------

    fn structural(&self, tokens: Vec<Token>) -> SqlResult<Vec<Vec<Token>>> {
        let words: Vec<String> = tokens
            .iter()
            .take(6)
            .filter(|t| t.kind == Kind::Word)
            .map(Token::upper)
            .collect();
        let first = words.first().map(String::as_str).unwrap_or_default();
        let has = |word: &str| words.iter().any(|w| w == word);
        match first {
            "CREATE" if has("TABLE") && !has("TRIGGER") && !has("VIEW") => {
                self.create_table(&tokens)
            }
            "CREATE" if has("INDEX") => self.create_index(&tokens),
            "CREATE" if has("VIEW") => self.create_view(&tokens),
            "ALTER" if has("TABLE") => self.alter_table(&tokens),
            "DROP" if has("TABLE") => self.drop_table(&tokens),
            "DROP" if has("INDEX") => self.drop_index(&tokens),
            "TRUNCATE" => self.truncate(&tokens),
            "RENAME" => self.rename_table(&tokens),
            "INSERT" | "REPLACE" => Ok(vec![self.insert(&tokens)?]),
            "UPDATE" | "DELETE" => Ok(vec![strip_dml_modifiers(&tokens)]),
            _ => Ok(vec![tokens]),
        }
    }

    fn create_table(&self, tokens: &[Token]) -> SqlResult<Vec<Vec<Token>>> {
        let mut i = 1;
        let mut head = vec![Token::raw("CREATE").with_ws("")];
        if tokens
            .get(i)
            .is_some_and(|t| t.is_any_word(&["TEMPORARY", "TEMP"]))
        {
            head.push(Token::raw("TEMP"));
            i += 1;
        }
        if !tokens.get(i).is_some_and(|t| t.is_word("TABLE")) {
            return Ok(vec![tokens.to_vec()]);
        }
        head.push(Token::raw("TABLE"));
        i += 1;
        let mut if_not_exists = false;
        if tokens.get(i).is_some_and(|t| t.is_word("IF"))
            && tokens.get(i + 1).is_some_and(|t| t.is_word("NOT"))
            && tokens.get(i + 2).is_some_and(|t| t.is_word("EXISTS"))
        {
            head.push(Token::raw("IF NOT EXISTS"));
            if_not_exists = true;
            i += 3;
        }
        let Some((table, next)) = table_name_at(tokens, i, self.env.database) else {
            return Ok(vec![tokens.to_vec()]);
        };
        i = next;
        head.push(Token::raw(quote_ident(&table)));
        if tokens.get(i).is_some_and(|t| t.is_word("LIKE")) {
            return Err(SqlError::Unsupported(
                "CREATE TABLE ... LIKE is not supported".into(),
            ));
        }
        if !tokens.get(i).is_some_and(|t| t.is_op("(")) {
            // CREATE TABLE ... AS SELECT and similar forms pass through.
            let mut statement = head;
            statement.extend(tokens[i..].iter().cloned());
            return Ok(vec![statement]);
        }
        let close = find_close(tokens, i)?;
        if tokens[close + 1..]
            .iter()
            .any(|t| t.is_any_word(&["SELECT", "AS"]))
        {
            return Err(SqlError::Unsupported(
                "CREATE TABLE with both column definitions and AS SELECT is not supported".into(),
            ));
        }
        let items = split_commas(&tokens[i + 1..close]);

        // Single-column table-level primary key (for AUTO_INCREMENT fusion).
        let mut table_pk: Option<String> = None;
        for item in &items {
            let offset = constraint_offset(item);
            if item.get(offset).is_some_and(|t| t.is_word("PRIMARY")) {
                if let Some(open) = item.iter().position(|t| t.is_op("(")) {
                    let close = find_close(item, open)?;
                    let columns = index_columns(&item[open + 1..close]);
                    if columns.len() == 1 {
                        table_pk = Some(columns[0].0.clone());
                    }
                }
            }
        }

        let mut definitions: Vec<Vec<Token>> = Vec::new();
        let mut indexes: Vec<Vec<Token>> = Vec::new();
        let mut fused_pk = false;
        let mut index_names: Vec<String> = Vec::new();
        for item in &items {
            if item.is_empty() {
                continue;
            }
            match self.table_item(item, &table, if_not_exists, &mut index_names)? {
                TableItem::Column(column) => {
                    let (tokens, fused) =
                        self.column_definition(column, table_pk.as_deref(), true)?;
                    fused_pk |= fused;
                    definitions.push(tokens);
                }
                TableItem::Constraint(tokens) => definitions.push(tokens),
                TableItem::PrimaryKey(tokens) => definitions.push(tokens),
                TableItem::Index(statement) => indexes.push(statement),
                TableItem::Skip => {}
            }
        }
        if fused_pk {
            // The AUTO_INCREMENT column became `INTEGER PRIMARY KEY`.
            definitions.retain(|tokens| tokens.first().is_none_or(|t| t.text != "PRIMARY KEY"));
        }

        let mut statement = head;
        statement.push(Token::op("(").with_ws(" "));
        for (index, definition) in definitions.into_iter().enumerate() {
            if index > 0 {
                statement.push(Token::op(","));
            }
            let mut definition = definition;
            if let Some(first) = definition.first_mut() {
                first.ws = " ".into();
            }
            statement.extend(definition);
        }
        statement.push(Token::op(")").with_ws(" "));
        let mut statements = vec![statement];
        statements.extend(indexes);
        Ok(statements)
    }

    fn table_item<'t>(
        &self,
        item: &'t [Token],
        table: &str,
        if_not_exists: bool,
        index_names: &mut Vec<String>,
    ) -> SqlResult<TableItem<'t>> {
        let offset = constraint_offset(item);
        let Some(head) = item.get(offset) else {
            return Ok(TableItem::Skip);
        };
        let upper = head.upper();
        match upper.as_str() {
            "PRIMARY" => {
                let open = item
                    .iter()
                    .position(|t| t.is_op("("))
                    .ok_or_else(|| SqlError::Syntax("PRIMARY KEY requires a column list".into()))?;
                let close = find_close(item, open)?;
                Ok(TableItem::PrimaryKey(vec![
                    Token::raw("PRIMARY KEY"),
                    Token::raw(format!(
                        "({})",
                        render_index_columns(&item[open + 1..close])
                    )),
                ]))
            }
            "UNIQUE" => {
                let open = item
                    .iter()
                    .position(|t| t.is_op("("))
                    .ok_or_else(|| SqlError::Syntax("UNIQUE requires a column list".into()))?;
                let close = find_close(item, open)?;
                Ok(TableItem::Constraint(vec![
                    Token::raw("UNIQUE"),
                    Token::raw(format!(
                        "({})",
                        render_index_columns(&item[open + 1..close])
                    )),
                ]))
            }
            "KEY" | "INDEX" if offset == 0 => {
                let open = item
                    .iter()
                    .position(|t| t.is_op("("))
                    .ok_or_else(|| SqlError::Syntax("KEY requires a column list".into()))?;
                let close = find_close(item, open)?;
                let columns = index_columns(&item[open + 1..close]);
                let declared = item[1..open]
                    .iter()
                    .take_while(|t| !t.is_word("USING"))
                    .find_map(Token::ident)
                    .map(str::to_owned)
                    .or_else(|| columns.first().map(|c| c.0.clone()))
                    .unwrap_or_else(|| "idx".into());
                let mut name = format!("{table}_{declared}");
                let mut suffix = 2;
                while index_names.contains(&name) {
                    name = format!("{table}_{declared}_{suffix}");
                    suffix += 1;
                }
                index_names.push(name.clone());
                Ok(TableItem::Index(vec![Token::raw(format!(
                    "CREATE INDEX {}{} ON {} ({})",
                    if if_not_exists { "IF NOT EXISTS " } else { "" },
                    quote_ident(&name),
                    quote_ident(table),
                    render_index_columns(&item[open + 1..close])
                ))]))
            }
            "FULLTEXT" | "SPATIAL" if offset == 0 => Ok(TableItem::Skip),
            "FOREIGN" => {
                // FOREIGN KEY [index_name] (cols) REFERENCES ...
                let open = item
                    .iter()
                    .position(|t| t.is_op("("))
                    .ok_or_else(|| SqlError::Syntax("FOREIGN KEY requires a column list".into()))?;
                let mut tokens = Vec::new();
                if offset == 2 {
                    // Keep the constraint name (Knex/Sequelize drop FKs by name).
                    if let Some(name) = item[1].ident() {
                        tokens.push(Token::raw(format!("CONSTRAINT {}", quote_ident(name))));
                    }
                }
                tokens.push(Token::raw("FOREIGN KEY"));
                tokens.extend(item[open..].iter().cloned());
                Ok(TableItem::Constraint(tokens))
            }
            "CHECK" | "CONSTRAINT" => Ok(TableItem::Constraint(item[offset..].to_vec())),
            _ if offset > 0 => Ok(TableItem::Constraint(item[offset..].to_vec())),
            _ => Ok(TableItem::Column(item)),
        }
    }

    /// Rewrites a column definition. Returns the tokens and whether the
    /// column was fused into `INTEGER PRIMARY KEY AUTOINCREMENT`.
    fn column_definition(
        &self,
        item: &[Token],
        table_pk: Option<&str>,
        allow_autoincrement: bool,
    ) -> SqlResult<(Vec<Token>, bool)> {
        let name = item[0]
            .ident()
            .ok_or_else(|| SqlError::Syntax("expected a column name".into()))?
            .to_owned();
        let mut out = vec![Token::raw(quote_ident(&name)).with_ws(&item[0].ws)];
        let mut i = 1;
        // Type.
        let mut type_text = String::new();
        let mut type_upper = String::new();
        let mut check = None;
        if let Some(type_token) = item.get(i).filter(|t| t.kind == Kind::Word) {
            type_upper = type_token.upper();
            type_text = type_token.text.clone();
            i += 1;
            let continuation = match type_upper.as_str() {
                "DOUBLE" => &["PRECISION"][..],
                "CHARACTER" | "CHAR" | "NATIONAL" | "LONG" => {
                    &["VARYING", "VARCHAR", "CHAR", "CHARACTER"][..]
                }
                _ => &[][..],
            };
            while item.get(i).is_some_and(|t| t.is_any_word(continuation)) {
                type_text.push(' ');
                type_text.push_str(&item[i].text);
                i += 1;
            }
            if item.get(i).is_some_and(|t| t.is_op("(")) {
                let close = find_close(item, i)?;
                if matches!(type_upper.as_str(), "ENUM" | "SET") {
                    let values: Vec<String> = item[i + 1..close]
                        .iter()
                        .filter(|t| t.kind == Kind::Str)
                        .map(|t| quote_str(&t.value))
                        .collect();
                    if type_upper == "ENUM" && !values.is_empty() {
                        check = Some(format!(
                            "CHECK ({} IN ({}))",
                            quote_ident(&name),
                            values.join(", ")
                        ));
                    }
                    type_text = "TEXT".into();
                } else {
                    type_text.push_str(&render(&item[i..=close]).replace(' ', ""));
                }
                i = close + 1;
            }
        }
        // Options.
        let mut options: Vec<Token> = Vec::new();
        let mut auto_increment = false;
        let mut inline_pk = false;
        while i < item.len() {
            let token = &item[i];
            let upper = token.upper();
            match upper.as_str() {
                "UNSIGNED" | "SIGNED" | "ZEROFILL" | "VISIBLE" | "INVISIBLE" | "BINARY" => i += 1,
                "AUTO_INCREMENT" => {
                    auto_increment = true;
                    i += 1;
                }
                "CHARACTER" if item.get(i + 1).is_some_and(|t| t.is_word("SET")) => i += 3,
                "CHARSET" | "COLUMN_FORMAT" | "STORAGE" | "SRID" => i += 2,
                "COLLATE" => {
                    let keep = item.get(i + 1).and_then(Token::ident).is_some_and(|c| {
                        SQLITE_COLLATIONS.iter().any(|s| s.eq_ignore_ascii_case(c))
                    });
                    if keep {
                        options.extend(item[i..i + 2].iter().cloned());
                    }
                    i += 2;
                }
                "COMMENT" if item.get(i + 1).is_some_and(|t| t.kind == Kind::Str) => i += 2,
                "ON" if item.get(i + 1).is_some_and(|t| t.is_word("UPDATE"))
                    && item.get(i + 2).is_some_and(|t| {
                        t.is_any_word(&["CURRENT_TIMESTAMP", "NOW", "LOCALTIMESTAMP", "LOCALTIME"])
                    }) =>
                {
                    i += 3;
                    if item.get(i).is_some_and(|t| t.is_op("(")) {
                        i = find_close(item, i)? + 1;
                    }
                }
                "PRIMARY" if item.get(i + 1).is_some_and(|t| t.is_word("KEY")) => {
                    inline_pk = true;
                    options.push(Token::raw("PRIMARY KEY"));
                    i += 2;
                }
                "KEY" => {
                    inline_pk = true;
                    options.push(Token::raw("PRIMARY KEY"));
                    i += 1;
                }
                "UNIQUE" => {
                    options.push(Token::raw("UNIQUE"));
                    i += 1;
                    if item.get(i).is_some_and(|t| t.is_word("KEY")) {
                        i += 1;
                    }
                }
                _ => {
                    options.push(token.clone());
                    i += 1;
                }
            }
        }
        let integer = INT_TYPES.contains(&type_upper.as_str());
        let pk_matches = inline_pk || table_pk.is_some_and(|pk| pk.eq_ignore_ascii_case(&name));
        // Statements run in `DialectMode::Passthrough`, so SQLite keeps the
        // declared MySQL type verbatim (`varchar(255)`, `bigint`, ...). The one
        // exception is a rowid alias: SQLite only accepts exactly
        // `INTEGER PRIMARY KEY AUTOINCREMENT`, so the real type is kept in a
        // comment (see `ddl::annotation`) for typing and `SHOW CREATE TABLE`.
        if allow_autoincrement && auto_increment && integer && pk_matches {
            out.push(Token::raw("INTEGER PRIMARY KEY AUTOINCREMENT"));
            if type_upper != "INTEGER" {
                out.push(Token::raw(crate::ddl::annotation(&type_text)));
            }
            out.extend(options.into_iter().filter(|t| t.text != "PRIMARY KEY"));
            return Ok((out, !inline_pk));
        }
        if !type_text.is_empty() {
            out.push(Token::raw(type_text));
        }
        if let Some(check) = check {
            out.push(Token::raw(check));
        }
        out.extend(options);
        Ok((out, false))
    }

    fn create_index(&self, tokens: &[Token]) -> SqlResult<Vec<Vec<Token>>> {
        let mut i = 1;
        let mut unique = false;
        while let Some(token) = tokens.get(i) {
            if token.is_word("UNIQUE") {
                unique = true;
            } else if token.is_any_word(&["FULLTEXT", "SPATIAL"]) {
                // Not available in SQLite; treated as a no-op.
                return Ok(Vec::new());
            } else if token.is_word("INDEX") {
                break;
            }
            i += 1;
        }
        i += 1;
        let mut statement = vec![
            Token::raw(if unique {
                "CREATE UNIQUE INDEX"
            } else {
                "CREATE INDEX"
            })
            .with_ws(""),
        ];
        if tokens.get(i).is_some_and(|t| t.is_word("IF")) {
            statement.push(Token::raw("IF NOT EXISTS"));
            i += 3;
        }
        let name = tokens
            .get(i)
            .and_then(Token::ident)
            .ok_or_else(|| SqlError::Syntax("expected an index name".into()))?
            .to_owned();
        i += 1;
        while tokens.get(i).is_some_and(|t| t.is_word("USING")) {
            i += 2;
        }
        if !tokens.get(i).is_some_and(|t| t.is_word("ON")) {
            return Err(SqlError::Syntax("expected ON after the index name".into()));
        }
        let (table, next) = table_name_at(tokens, i + 1, self.env.database)
            .ok_or_else(|| SqlError::Syntax("expected a table name".into()))?;
        i = next;
        let open = i;
        if !tokens.get(open).is_some_and(|t| t.is_op("(")) {
            return Err(SqlError::Syntax("expected an index column list".into()));
        }
        let close = find_close(tokens, open)?;
        statement.push(Token::raw(format!(
            "{} ON {} ({})",
            quote_ident(&name),
            quote_ident(&table),
            render_index_columns(&tokens[open + 1..close])
        )));
        // Trailing `USING BTREE`, `ALGORITHM=...`, `LOCK=...`, comments are dropped,
        // except a partial-index WHERE clause.
        if let Some(where_index) = find_top(tokens, close + 1, |t| t.is_word("WHERE")) {
            statement.extend(tokens[where_index..].iter().cloned());
        }
        Ok(vec![statement])
    }

    fn create_view(&self, tokens: &[Token]) -> SqlResult<Vec<Vec<Token>>> {
        let view_index = tokens
            .iter()
            .position(|t| t.is_word("VIEW"))
            .ok_or_else(|| SqlError::Syntax("expected VIEW".into()))?;
        let replace = tokens[..view_index].iter().any(|t| t.is_word("REPLACE"));
        let temporary = tokens[..view_index]
            .iter()
            .any(|t| t.is_any_word(&["TEMP", "TEMPORARY"]));
        let as_index = find_top(tokens, view_index, |t| t.is_word("AS"))
            .ok_or_else(|| SqlError::Syntax("expected AS in CREATE VIEW".into()))?;
        let mut name_tokens: Vec<Token> = tokens[view_index + 1..as_index].to_vec();
        if name_tokens.len() >= 3 && name_tokens[0].is_word("IF") {
            name_tokens.drain(..3);
        }
        let (name, next) = table_name_at(&name_tokens, 0, self.env.database)
            .ok_or_else(|| SqlError::Syntax("expected a view name".into()))?;
        let columns = if name_tokens.get(next).is_some_and(|t| t.is_op("(")) {
            render(&name_tokens[next..])
        } else {
            String::new()
        };
        let mut body: Vec<Token> = tokens[as_index + 1..].to_vec();
        // Drop `WITH [CASCADED|LOCAL] CHECK OPTION`.
        if let Some(position) = find_top(&body, 0, |t| t.is_word("CHECK")) {
            if body.get(position + 1).is_some_and(|t| t.is_word("OPTION")) {
                let start =
                    if position >= 1 && body[position - 1].is_any_word(&["CASCADED", "LOCAL"]) {
                        position - 2
                    } else {
                        position - 1
                    };
                body.truncate(start);
            }
        }
        let mut statements = Vec::new();
        if replace {
            statements.push(vec![Token::raw(format!(
                "DROP VIEW IF EXISTS {}",
                quote_ident(&name)
            ))]);
        }
        let mut statement = vec![Token::raw(format!(
            "CREATE {}VIEW {} {} AS",
            if temporary { "TEMP " } else { "" },
            quote_ident(&name),
            columns
        ))];
        statement.extend(body);
        statements.push(statement);
        Ok(statements)
    }

    fn alter_table(&self, tokens: &[Token]) -> SqlResult<Vec<Vec<Token>>> {
        let (_, steps) = self.alter_steps(tokens)?;
        let mut statements = Vec::new();
        for step in steps {
            match step {
                AlterStep::Statements(batch) => statements.extend(batch),
                AlterStep::Rebuild { .. } => {
                    return Err(SqlError::Unsupported(
                        "this ALTER TABLE change requires rebuilding the table".into(),
                    ));
                }
            }
        }
        Ok(statements)
    }

    /// Splits an `ALTER TABLE` into plain statements and table rebuilds.
    fn alter_steps(&self, tokens: &[Token]) -> SqlResult<(String, Vec<AlterStep>)> {
        let table_index = tokens
            .iter()
            .position(|t| t.is_word("TABLE"))
            .ok_or_else(|| SqlError::Syntax("expected TABLE".into()))?;
        let (table, next) = table_name_at(tokens, table_index + 1, self.env.database)
            .ok_or_else(|| SqlError::Syntax("expected a table name".into()))?;
        let quoted = quote_ident(&table);
        let mut statements: Vec<Vec<Token>> = Vec::new();
        let mut steps: Vec<AlterStep> = Vec::new();
        for action in split_commas(&tokens[next..]) {
            let Some(first) = action.first() else {
                continue;
            };
            let words: Vec<String> = action.iter().take(4).map(Token::upper).collect();
            let word = |index: usize| words.get(index).map(String::as_str).unwrap_or_default();
            match first.upper().as_str() {
                "ADD" => {
                    let mut i = 1;
                    if word(1) == "CONSTRAINT" {
                        i += if action.get(2).is_some_and(|t| {
                            !t.is_any_word(&["PRIMARY", "UNIQUE", "FOREIGN", "CHECK"])
                        }) {
                            2
                        } else {
                            1
                        };
                    }
                    let head = action.get(i).map(Token::upper).unwrap_or_default();
                    match head.as_str() {
                        "INDEX" | "KEY" | "UNIQUE" | "FULLTEXT" | "SPATIAL" => {
                            if matches!(head.as_str(), "FULLTEXT" | "SPATIAL") {
                                continue;
                            }
                            let unique = head == "UNIQUE";
                            let open = action
                                .iter()
                                .position(|t| t.is_op("("))
                                .ok_or_else(|| SqlError::Syntax("expected a column list".into()))?;
                            let close = find_close(action, open)?;
                            let explicit = action[i + 1..open]
                                .iter()
                                .filter(|t| {
                                    !t.is_any_word(&["INDEX", "KEY", "USING", "BTREE", "HASH"])
                                })
                                .find_map(Token::ident)
                                .map(str::to_owned);
                            let columns = index_columns(&action[open + 1..close]);
                            let name = explicit.unwrap_or_else(|| {
                                format!(
                                    "{table}_{}",
                                    columns.first().map_or("idx", |c| c.0.as_str())
                                )
                            });
                            statements.push(vec![Token::raw(format!(
                                "CREATE {}INDEX {} ON {quoted} ({})",
                                if unique { "UNIQUE " } else { "" },
                                quote_ident(&name),
                                render_index_columns(&action[open + 1..close])
                            ))]);
                        }
                        "FOREIGN" | "CHECK" => {
                            let name = (word(1) == "CONSTRAINT" && i == 3)
                                .then(|| action.get(2).and_then(Token::ident).map(str::to_owned))
                                .flatten();
                            let TableItem::Constraint(constraint) =
                                self.table_item(&action[i..], &table, false, &mut Vec::new())?
                            else {
                                return Err(SqlError::Syntax("invalid constraint".into()));
                            };
                            let constraint = self.expressions(&constraint, false)?;
                            flush(&mut statements, &mut steps);
                            steps.push(AlterStep::Rebuild {
                                edit: RebuildEdit::AddConstraint {
                                    name,
                                    sql: render(&constraint).trim().to_owned(),
                                    foreign_key: head == "FOREIGN",
                                },
                                rename: None,
                            });
                        }
                        "PRIMARY" => {
                            return Err(SqlError::Unsupported(
                                "ALTER TABLE ... ADD PRIMARY KEY is not supported by SQLite; recreate the table"
                                    .into(),
                            ));
                        }
                        _ => {
                            if head == "COLUMN" {
                                i += 1;
                            }
                            let mut column: Vec<Token> = action[i..].to_vec();
                            if let Some(position) = column.iter().position(|t| t.is_word("AFTER")) {
                                column.truncate(position);
                            }
                            if column.last().is_some_and(|t| t.is_word("FIRST")) {
                                column.pop();
                            }
                            if column.first().is_some_and(|t| t.is_op("(")) {
                                return Err(SqlError::Unsupported(
                                    "ALTER TABLE ... ADD (multiple columns) is not supported"
                                        .into(),
                                ));
                            }
                            let (definition, _) = self.column_definition(&column, None, false)?;
                            let mut statement = vec![
                                Token::raw(format!("ALTER TABLE {quoted} ADD COLUMN")).with_ws(""),
                            ];
                            statement.extend(definition);
                            statements.push(statement);
                        }
                    }
                }
                "DROP" => match word(1) {
                    "INDEX" | "KEY" => {
                        let name = action
                            .get(2)
                            .and_then(Token::ident)
                            .ok_or_else(|| SqlError::Syntax("expected an index name".into()))?;
                        statements.extend(drop_index_statements(&table, name));
                    }
                    "FOREIGN" | "CHECK" | "CONSTRAINT" => {
                        let at = if word(1) == "FOREIGN" { 3 } else { 2 };
                        let name = action
                            .get(at)
                            .and_then(Token::ident)
                            .ok_or_else(|| SqlError::Syntax("expected a constraint name".into()))?
                            .to_owned();
                        flush(&mut statements, &mut steps);
                        steps.push(AlterStep::Rebuild {
                            edit: RebuildEdit::DropConstraint {
                                name,
                                foreign_key_only: word(1) == "FOREIGN",
                            },
                            rename: None,
                        });
                    }
                    "PRIMARY" => {
                        return Err(SqlError::Unsupported(
                            "ALTER TABLE ... DROP PRIMARY KEY is not supported by SQLite; recreate the table"
                                .into(),
                        ));
                    }
                    _ => {
                        let index = if word(1) == "COLUMN" { 2 } else { 1 };
                        let column = action
                            .get(index)
                            .and_then(Token::ident)
                            .ok_or_else(|| SqlError::Syntax("expected a column name".into()))?;
                        statements.push(vec![Token::raw(format!(
                            "ALTER TABLE {quoted} DROP COLUMN {}",
                            quote_ident(column)
                        ))]);
                    }
                },
                "RENAME" => match word(1) {
                    "COLUMN" => {
                        let mut statement =
                            vec![Token::raw(format!("ALTER TABLE {quoted}")).with_ws("")];
                        statement.extend(action.iter().cloned());
                        statements.push(statement);
                    }
                    "INDEX" | "KEY" => {
                        return Err(SqlError::Unsupported(
                            "ALTER TABLE ... RENAME INDEX is not supported".into(),
                        ));
                    }
                    _ => {
                        let index = if matches!(word(1), "TO" | "AS") { 2 } else { 1 };
                        let (new_name, _) = table_name_at(action, index, self.env.database)
                            .ok_or_else(|| SqlError::Syntax("expected a new table name".into()))?;
                        statements.push(vec![Token::raw(format!(
                            "ALTER TABLE {quoted} RENAME TO {}",
                            quote_ident(&new_name)
                        ))]);
                    }
                },
                // Table options and online-DDL hints are no-ops.
                "ENGINE" | "AUTO_INCREMENT" | "COMMENT" | "ALGORITHM" | "LOCK" | "ROW_FORMAT"
                | "CONVERT" | "CHARACTER" | "CHARSET" | "COLLATE" | "DEFAULT" | "FORCE" => {}
                "MODIFY" | "CHANGE" => {
                    let change = first.is_word("CHANGE");
                    let mut i = 1;
                    if word(1) == "COLUMN" {
                        i += 1;
                    }
                    let old = action
                        .get(i)
                        .and_then(Token::ident)
                        .ok_or_else(|| SqlError::Syntax("expected a column name".into()))?
                        .to_owned();
                    let start = if change { i + 1 } else { i };
                    let mut column: Vec<Token> = action.get(start..).unwrap_or_default().to_vec();
                    if let Some(position) = column.iter().position(|t| t.is_word("AFTER")) {
                        column.truncate(position);
                    }
                    if column.last().is_some_and(|t| t.is_word("FIRST")) {
                        column.pop();
                    }
                    if column.is_empty() {
                        return Err(SqlError::Syntax("expected a column definition".into()));
                    }
                    let auto_increment = column.iter().any(|t| t.is_word("AUTO_INCREMENT"));
                    let new_name = column[0].ident().unwrap_or_default().to_owned();
                    let (definition, _) = self.column_definition(&column, None, false)?;
                    let definition = self.expressions(&definition, false)?;
                    let rename =
                        (!new_name.eq_ignore_ascii_case(&old)).then(|| (old.clone(), new_name));
                    flush(&mut statements, &mut steps);
                    steps.push(AlterStep::Rebuild {
                        edit: RebuildEdit::Column {
                            column: rename.as_ref().map_or(old, |(_, new)| new.clone()),
                            definition: render(&definition).trim().to_owned(),
                            auto_increment,
                        },
                        rename,
                    });
                }
                "ALTER" => {
                    return Err(SqlError::Unsupported(
                        "ALTER TABLE ... ALTER COLUMN is not supported by SQLite; use MODIFY COLUMN"
                            .into(),
                    ));
                }
                other => {
                    return Err(SqlError::Unsupported(format!(
                        "ALTER TABLE ... {other} is not supported"
                    )));
                }
            }
        }
        flush(&mut statements, &mut steps);
        Ok((table, steps))
    }

    fn drop_table(&self, tokens: &[Token]) -> SqlResult<Vec<Vec<Token>>> {
        let table_index = tokens
            .iter()
            .position(|t| t.is_word("TABLE"))
            .ok_or_else(|| SqlError::Syntax("expected TABLE".into()))?;
        let mut i = table_index + 1;
        let mut if_exists = "";
        if tokens.get(i).is_some_and(|t| t.is_word("IF")) {
            if_exists = "IF EXISTS ";
            i += 2;
        }
        let mut statements = Vec::new();
        for part in split_commas(&tokens[i..]) {
            let part: Vec<Token> = part
                .iter()
                .filter(|t| !t.is_any_word(&["RESTRICT", "CASCADE"]))
                .cloned()
                .collect();
            let (name, _) = table_name_at(&part, 0, self.env.database)
                .ok_or_else(|| SqlError::Syntax("expected a table name".into()))?;
            statements.push(vec![Token::raw(format!(
                "DROP TABLE {if_exists}{}",
                quote_ident(&name)
            ))]);
        }
        Ok(statements)
    }

    fn drop_index(&self, tokens: &[Token]) -> SqlResult<Vec<Vec<Token>>> {
        let index = tokens
            .iter()
            .position(|t| t.is_word("INDEX"))
            .ok_or_else(|| SqlError::Syntax("expected INDEX".into()))?;
        let mut i = index + 1;
        if tokens.get(i).is_some_and(|t| t.is_word("IF")) {
            i += 2;
        }
        let name = tokens
            .get(i)
            .and_then(Token::ident)
            .ok_or_else(|| SqlError::Syntax("expected an index name".into()))?;
        match tokens.get(i + 1) {
            Some(on) if on.is_word("ON") => {
                let (table, _) = table_name_at(tokens, i + 2, self.env.database)
                    .ok_or_else(|| SqlError::Syntax("expected a table name".into()))?;
                Ok(drop_index_statements(&table, name))
            }
            _ => Ok(vec![vec![Token::raw(format!(
                "DROP INDEX {}",
                quote_ident(name)
            ))]]),
        }
    }

    fn truncate(&self, tokens: &[Token]) -> SqlResult<Vec<Vec<Token>>> {
        let index = if tokens.get(1).is_some_and(|t| t.is_word("TABLE")) {
            2
        } else {
            1
        };
        let (table, _) = table_name_at(tokens, index, self.env.database)
            .ok_or_else(|| SqlError::Syntax("expected a table name".into()))?;
        Ok(vec![vec![Token::raw(format!(
            "DELETE FROM {}",
            quote_ident(&table)
        ))]])
    }

    fn rename_table(&self, tokens: &[Token]) -> SqlResult<Vec<Vec<Token>>> {
        if !tokens.get(1).is_some_and(|t| t.is_word("TABLE")) {
            return Err(SqlError::Unsupported(
                "only RENAME TABLE is supported".into(),
            ));
        }
        let mut statements = Vec::new();
        for pair in split_commas(&tokens[2..]) {
            let (from, next) = table_name_at(pair, 0, self.env.database)
                .ok_or_else(|| SqlError::Syntax("expected a table name".into()))?;
            if !pair.get(next).is_some_and(|t| t.is_word("TO")) {
                return Err(SqlError::Syntax("expected TO".into()));
            }
            let (to, _) = table_name_at(pair, next + 1, self.env.database)
                .ok_or_else(|| SqlError::Syntax("expected a table name".into()))?;
            statements.push(vec![Token::raw(format!(
                "ALTER TABLE {} RENAME TO {}",
                quote_ident(&from),
                quote_ident(&to)
            ))]);
        }
        Ok(statements)
    }

    fn insert(&self, tokens: &[Token]) -> SqlResult<Vec<Token>> {
        let mut out = vec![tokens[0].clone()];
        let mut i = 1;
        while tokens
            .get(i)
            .is_some_and(|t| t.is_any_word(&["LOW_PRIORITY", "DELAYED", "HIGH_PRIORITY"]))
        {
            i += 1;
        }
        if tokens.get(i).is_some_and(|t| t.is_word("IGNORE")) {
            if tokens[0].is_word("INSERT") {
                out.push(Token::raw("OR IGNORE"));
            }
            i += 1;
        }
        if tokens.get(i).is_some_and(|t| t.is_word("INTO")) {
            i += 1;
        }
        out.push(Token::raw("INTO"));
        let (table, next) = table_name_at(tokens, i, self.env.database)
            .ok_or_else(|| SqlError::Syntax("expected a table name after INSERT INTO".into()))?;
        out.push(Token::raw(quote_ident(&table)));
        i = next;

        let duplicate = find_top(tokens, i, |t| t.is_word("DUPLICATE")).and_then(|index| {
            let on = index.checked_sub(1)?;
            (tokens[on].is_word("ON")
                && tokens.get(index + 1).is_some_and(|t| t.is_word("KEY"))
                && tokens.get(index + 2).is_some_and(|t| t.is_word("UPDATE")))
            .then_some(on)
        });
        let body_end = duplicate.unwrap_or(tokens.len());
        let mut body: Vec<Token> = tokens[i..body_end].to_vec();

        // INSERT ... SET a = 1, b = 2
        if body.first().is_some_and(|t| t.is_word("SET")) {
            let mut columns = Vec::new();
            let mut values: Vec<Token> = Vec::new();
            for (index, assignment) in split_commas(&body[1..]).into_iter().enumerate() {
                let equals = assignment
                    .iter()
                    .position(|t| t.is_op("=") || t.is_op(":="))
                    .ok_or_else(|| SqlError::Syntax("expected column = value".into()))?;
                let column = assignment[..equals]
                    .last()
                    .and_then(Token::ident)
                    .ok_or_else(|| SqlError::Syntax("expected a column name".into()))?;
                columns.push(quote_ident(column));
                if index > 0 {
                    values.push(Token::op(","));
                }
                values.extend(assignment[equals + 1..].iter().cloned());
            }
            body = vec![Token::raw(format!("({}) VALUES (", columns.join(", ")))];
            body.extend(values);
            body.push(Token::op(")"));
        } else {
            // `VALUE` is a MySQL synonym for `VALUES`; `() VALUES ()` → DEFAULT VALUES.
            for token in &mut body {
                if token.is_word("VALUE") {
                    token.text = "VALUES".into();
                }
            }
            body = self.default_values(&table, body);
            let empty_columns = body.len() >= 2 && body[0].is_op("(") && body[1].is_op(")");
            let rest = if empty_columns { &body[2..] } else { &body[..] };
            if rest.len() == 3
                && rest[0].is_word("VALUES")
                && rest[1].is_op("(")
                && rest[2].is_op(")")
            {
                body = vec![Token::raw("DEFAULT VALUES")];
            }
        }

        let Some(on) = duplicate else {
            out.extend(body);
            return Ok(out);
        };

        // Row alias: `VALUES (...) AS new [(cols)] ON DUPLICATE KEY UPDATE`.
        let mut alias = None;
        if let Some(as_index) = find_top(&body, 0, |t| t.is_word("AS")) {
            if let Some(name) = body.get(as_index + 1).and_then(Token::ident) {
                let tail_ok = body.len() == as_index + 2
                    || (body[as_index + 2].is_op("(")
                        && find_close(&body, as_index + 2)? == body.len() - 1);
                if tail_ok {
                    alias = Some(name.to_owned());
                    body.truncate(as_index);
                }
            }
        }
        // INSERT ... SELECT needs a WHERE clause before ON CONFLICT in SQLite.
        if let Some(select) = find_top(&body, 0, |t| t.is_word("SELECT")) {
            let clauses = find_top(&body, select, |t| {
                t.is_any_word(&["WHERE", "GROUP", "ORDER", "LIMIT", "HAVING", "UNION"])
            });
            if clauses.is_none() {
                body.push(Token::raw("WHERE true"));
            }
        }
        out.extend(body);
        out.push(Token::raw("ON CONFLICT DO UPDATE SET"));
        let update = &tokens[on + 4..];
        let mut j = 0;
        while j < update.len() {
            let token = &update[j];
            if token.is_word("VALUES") && update.get(j + 1).is_some_and(|t| t.is_op("(")) {
                if let (Some(column), Some(_)) = (
                    update.get(j + 2).and_then(Token::ident),
                    update.get(j + 3).filter(|t| t.is_op(")")),
                ) {
                    out.push(
                        Token::raw(format!("excluded.{}", quote_ident(column))).with_ws(&token.ws),
                    );
                    j += 4;
                    continue;
                }
            }
            if let Some(alias) = &alias {
                if token.ident() == Some(alias.as_str())
                    && update.get(j + 1).is_some_and(|t| t.is_op("."))
                {
                    if let Some(column) = update.get(j + 2).and_then(Token::ident) {
                        out.push(
                            Token::raw(format!("excluded.{}", quote_ident(column)))
                                .with_ws(&token.ws),
                        );
                        j += 3;
                        continue;
                    }
                }
            }
            out.push(token.clone());
            j += 1;
        }
        Ok(out)
    }

    /// Resolves the `DEFAULT` keyword inside `VALUES (...)` lists. A column
    /// that is `DEFAULT` in every row is omitted (SQLite then applies the
    /// declared default or assigns the rowid); otherwise `DEFAULT` becomes
    /// the declared default expression, or `NULL` (which also assigns an id
    /// to an `INTEGER PRIMARY KEY`).
    fn default_values(&self, table: &str, body: Vec<Token>) -> Vec<Token> {
        let is_default = |item: &[Token]| item.len() == 1 && item[0].is_word("DEFAULT");
        let Some(values) = find_top(&body, 0, |t| t.is_word("VALUES")) else {
            return body;
        };
        let mut names: Option<Vec<String>> = None;
        if values > 0 {
            let columns_ok =
                body[0].is_op("(") && find_close(&body, 0).is_ok_and(|close| close == values - 1);
            if !columns_ok {
                return body;
            }
            let list: Option<Vec<String>> = split_commas(&body[1..values - 1])
                .iter()
                .map(|c| c.last().and_then(Token::ident).map(str::to_owned))
                .collect();
            let Some(list) = list else { return body };
            names = Some(list);
        }
        let mut rows: Vec<Vec<&[Token]>> = Vec::new();
        for row in split_commas(&body[values + 1..]) {
            let well_formed = row.first().is_some_and(|t| t.is_op("("))
                && find_close(row, 0).is_ok_and(|close| close == row.len() - 1);
            if !well_formed {
                return body;
            }
            rows.push(split_commas(&row[1..row.len() - 1]));
        }
        if rows.is_empty() || !rows.iter().flatten().any(|item| is_default(item)) {
            return body;
        }
        let width = rows[0].len();
        if rows.iter().any(|row| row.len() != width) {
            return body;
        }
        let info = self.env.tables.get(table);
        let column_names: Option<Vec<String>> = names.clone().or_else(|| {
            info.filter(|columns| columns.len() == width)
                .map(|columns| columns.iter().map(|c| c.name.clone()).collect())
        });
        let mut omit = vec![false; width];
        if let Some(names) = &names {
            for (index, flag) in omit.iter_mut().enumerate() {
                *flag = rows.iter().all(|row| is_default(row[index]));
            }
            if omit.iter().all(|flag| *flag) {
                if rows.len() == 1 {
                    return vec![Token::raw("DEFAULT VALUES")];
                }
                omit.iter_mut().for_each(|flag| *flag = false);
            }
            let _ = names;
        }
        let substitute = |index: usize| -> Token {
            let expression = column_names
                .as_ref()
                .and_then(|names| names.get(index))
                .and_then(|name| {
                    info.and_then(|columns| {
                        columns.iter().find(|c| c.name.eq_ignore_ascii_case(name))
                    })
                })
                .filter(|column| !column.rowid)
                .and_then(|column| column.default.clone())
                .unwrap_or_else(|| "NULL".into());
            Token::raw(expression)
        };
        let mut out = Vec::new();
        if let Some(names) = &names {
            let kept: Vec<String> = names
                .iter()
                .zip(&omit)
                .filter(|(_, omitted)| !**omitted)
                .map(|(name, _)| quote_ident(name))
                .collect();
            out.push(Token::raw(format!("({})", kept.join(", "))));
        }
        out.push(Token::raw("VALUES"));
        for (row_index, row) in rows.iter().enumerate() {
            if row_index > 0 {
                out.push(Token::op(","));
            }
            out.push(Token::op("(").with_ws(" "));
            let mut first = true;
            for (index, item) in row.iter().enumerate() {
                if omit[index] {
                    continue;
                }
                if !first {
                    out.push(Token::op(","));
                }
                first = false;
                if is_default(item) {
                    out.push(substitute(index));
                } else {
                    out.extend(item.iter().cloned());
                }
            }
            out.push(Token::op(")"));
        }
        out
    }

    // ----------------------------------------------------------------------
    // Expression pass
    // ----------------------------------------------------------------------

    fn expressions(&self, tokens: &[Token], top: bool) -> SqlResult<Vec<Token>> {
        let mut out: Vec<Token> = Vec::with_capacity(tokens.len());
        let mut depth = 0_i32;
        let mut in_select = false;
        let mut i = 0;
        while i < tokens.len() {
            let token = &tokens[i];
            let next = tokens.get(i + 1);
            // `db.table.column` / `db.table.*` with the current database: drop `db.`
            // (Prisma qualifies every column this way).
            if matches!(token.kind, Kind::Word | Kind::QuotedIdent)
                && token.ident() == Some(self.env.database)
                && next.is_some_and(|t| t.is_op("."))
                && tokens.get(i + 2).and_then(Token::ident).is_some()
                && tokens.get(i + 3).is_some_and(|t| t.is_op("."))
                && tokens
                    .get(i + 4)
                    .is_some_and(|t| t.ident().is_some() || t.is_op("*"))
            {
                let mut table = tokens[i + 2].clone();
                table.ws = token.ws.clone();
                out.push(table);
                i += 3;
                continue;
            }
            match token.kind {
                Kind::Op => {
                    if token.is_op("(") {
                        depth += 1;
                    } else if token.is_op(")") {
                        depth -= 1;
                    }
                    let mapped = match token.text.as_str() {
                        "||" => Some("OR"),
                        "&&" => Some("AND"),
                        "<=>" => Some("IS"),
                        ":=" => Some("="),
                        _ => None,
                    };
                    let mut token = token.clone();
                    if let Some(mapped) = mapped {
                        token.kind = Kind::Raw;
                        token.text = mapped.into();
                    }
                    out.push(token);
                    i += 1;
                }
                Kind::SysVar => {
                    let value =
                        (self.env.system_var)(&token.value).unwrap_or_else(|| "NULL".into());
                    let alias = self.alias(tokens, i, i, top && depth == 0 && in_select);
                    out.push(Token::raw(format!("{value}{alias}")).with_ws(&token.ws));
                    i += 1;
                }
                Kind::Word => {
                    let upper = token.upper();
                    if top && depth == 0 {
                        if upper == "SELECT" {
                            in_select = true;
                        } else if SELECT_LIST_END.contains(&upper.as_str()) {
                            in_select = false;
                        }
                    }
                    // SELECT modifiers without SQLite equivalents.
                    if matches!(
                        upper.as_str(),
                        "SQL_CALC_FOUND_ROWS"
                            | "SQL_NO_CACHE"
                            | "SQL_CACHE"
                            | "HIGH_PRIORITY"
                            | "SQL_SMALL_RESULT"
                            | "SQL_BIG_RESULT"
                            | "SQL_BUFFER_RESULT"
                    ) && out.last().is_some_and(|t| {
                        t.is_any_word(&["SELECT", "DISTINCT", "ALL", "DISTINCTROW"])
                    }) {
                        i += 1;
                        continue;
                    }
                    if upper == "STRAIGHT_JOIN" {
                        if !out.last().is_some_and(|t| t.is_word("SELECT")) {
                            out.push(Token::raw("JOIN").with_ws(&token.ws));
                        }
                        i += 1;
                        continue;
                    }
                    // Prefix operator `BINARY expr` (byte-wise comparison): SQLite's
                    // default collation is already binary.
                    if upper == "BINARY"
                        && next.is_some_and(|t| !t.is_op(")") && !t.is_op(","))
                        && !out
                            .last()
                            .is_some_and(|t| t.is_any_word(&["AS", "COLLATE", "SET", "CHARSET"]))
                    {
                        i += 1;
                        continue;
                    }
                    if upper == "FROM" && next.is_some_and(|t| t.is_word("DUAL")) {
                        i += 2;
                        continue;
                    }
                    if upper == "FOR" && next.is_some_and(|t| t.is_any_word(&["UPDATE", "SHARE"])) {
                        i += 2;
                        if tokens.get(i).is_some_and(|t| t.is_word("OF")) {
                            i += 2;
                            while tokens.get(i).is_some_and(|t| t.is_op(",")) {
                                i += 2;
                            }
                        }
                        while tokens
                            .get(i)
                            .is_some_and(|t| t.is_any_word(&["NOWAIT", "SKIP", "LOCKED"]))
                        {
                            i += 1;
                        }
                        continue;
                    }
                    if upper == "LOCK"
                        && next.is_some_and(|t| t.is_word("IN"))
                        && tokens.get(i + 2).is_some_and(|t| t.is_word("SHARE"))
                        && tokens.get(i + 3).is_some_and(|t| t.is_word("MODE"))
                    {
                        i += 4;
                        continue;
                    }
                    if upper == "COLLATE" {
                        let keep = next
                            .and_then(|t| {
                                if t.kind == Kind::Str {
                                    Some(t.value.as_str())
                                } else {
                                    t.ident()
                                }
                            })
                            .is_some_and(|c| {
                                SQLITE_COLLATIONS.iter().any(|s| s.eq_ignore_ascii_case(c))
                            });
                        if !keep {
                            i += 2;
                            continue;
                        }
                    }
                    if CHARSET_INTRODUCERS
                        .iter()
                        .any(|c| c.eq_ignore_ascii_case(&token.text))
                        && next.is_some_and(|t| matches!(t.kind, Kind::Str | Kind::HexStr))
                    {
                        let mut literal = tokens[i + 1].clone();
                        literal.ws = token.ws.clone();
                        // `_binary'...'` is a binary string: store it as a BLOB.
                        if token.text.eq_ignore_ascii_case("_binary")
                            && literal.kind == Kind::Str
                            && literal.bytes.is_none()
                        {
                            literal.bytes = Some(literal.value.as_bytes().to_vec());
                        }
                        out.push(literal);
                        i += 2;
                        continue;
                    }
                    // Strip the current-database qualifier in table positions.
                    if TABLE_POSITION.contains(&upper.as_str())
                        && next.and_then(Token::ident) == Some(self.env.database)
                        && tokens.get(i + 2).is_some_and(|t| t.is_op("."))
                        && tokens.get(i + 3).and_then(Token::ident).is_some()
                    {
                        out.push(token.clone());
                        let mut table = tokens[i + 3].clone();
                        table.ws = tokens[i + 1].ws.clone();
                        out.push(table);
                        i += 4;
                        continue;
                    }
                    if next.is_some_and(|t| t.is_op("(")) && self.is_function_position(&out) {
                        let close = find_close(tokens, i + 1)?;
                        if let Some(text) = self.function(&upper, &tokens[i + 2..close])? {
                            let alias =
                                self.alias(tokens, i, close, top && depth == 0 && in_select);
                            out.push(Token::raw(format!("{text}{alias}")).with_ws(&token.ws));
                            i = close + 1;
                            continue;
                        }
                    }
                    let replacement = match upper.as_str() {
                        "CURRENT_USER" if !next.is_some_and(|t| t.is_op("(")) => {
                            Some(quote_str(&format!("{}@{}", self.env.user, self.env.host)))
                        }
                        "MOD" if !next.is_some_and(|t| t.is_op("(")) => Some("%".into()),
                        "DIV" => Some("/".into()),
                        "RLIKE" => Some("REGEXP".into()),
                        _ => None,
                    };
                    if let Some(replacement) = replacement {
                        out.push(Token::raw(replacement).with_ws(&token.ws));
                        i += 1;
                        continue;
                    }
                    out.push(token.clone());
                    i += 1;
                    // MySQL LIKE uses `\` as the default escape character.
                    if upper == "LIKE"
                        && next.is_some_and(|t| t.kind == Kind::Str && t.value.contains('\\'))
                        && !tokens.get(i + 1).is_some_and(|t| t.is_word("ESCAPE"))
                    {
                        out.push(tokens[i].clone());
                        out.push(Token::raw("ESCAPE '\\'"));
                        i += 1;
                    }
                }
                _ => {
                    out.push(token.clone());
                    i += 1;
                }
            }
        }
        Ok(out)
    }

    fn is_function_position(&self, out: &[Token]) -> bool {
        match out.last() {
            Some(previous) => !(previous.is_op(".") || previous.is_any_word(TABLE_POSITION)),
            None => true,
        }
    }

    /// ` AS "<original text>"` when the expression is a whole select item
    /// without an alias, so clients see MySQL's column label.
    fn alias(&self, tokens: &[Token], start: usize, end: usize, select_list: bool) -> String {
        if !select_list || start == 0 {
            return String::new();
        }
        let previous = &tokens[start - 1];
        let starts_item =
            previous.is_op(",") || previous.is_any_word(&["SELECT", "DISTINCT", "ALL"]);
        let ends_item = match tokens.get(end + 1) {
            None => true,
            Some(next) => next.is_op(",") || next.is_op(";") || next.is_any_word(SELECT_LIST_END),
        };
        if starts_item && ends_item {
            format!(
                " AS {}",
                quote_ident(source_text(&tokens[start..=end]).trim())
            )
        } else {
            String::new()
        }
    }

    fn arg(&self, tokens: &[Token]) -> SqlResult<String> {
        Ok(render(&self.expressions(tokens, false)?).trim().to_owned())
    }

    fn args(&self, tokens: &[Token]) -> SqlResult<Vec<String>> {
        if tokens.is_empty() {
            return Ok(Vec::new());
        }
        split_commas(tokens)
            .into_iter()
            .map(|a| self.arg(a))
            .collect()
    }

    /// Rewrites a function call; `None` keeps it unchanged.
    fn function(&self, name: &str, inner: &[Token]) -> SqlResult<Option<String>> {
        let env = self.env;
        // Functions with special argument syntax.
        match name {
            "CAST" => {
                let Some(as_index) = find_top(inner, 0, |t| t.is_word("AS")) else {
                    return Ok(None);
                };
                return Ok(Some(cast(
                    &self.arg(&inner[..as_index])?,
                    &inner[as_index + 1..],
                )));
            }
            "CONVERT" => {
                if let Some(using) = find_top(inner, 0, |t| t.is_word("USING")) {
                    return Ok(Some(format!("({})", self.arg(&inner[..using])?)));
                }
                let parts = split_commas(inner);
                if parts.len() == 2 {
                    return Ok(Some(cast(&self.arg(parts[0])?, parts[1])));
                }
                return Ok(None);
            }
            "GROUP_CONCAT" => return self.group_concat(inner).map(Some),
            "DATE_ADD" | "DATE_SUB" | "ADDDATE" | "SUBDATE" => {
                let parts = split_commas(inner);
                if parts.len() != 2 {
                    return Ok(None);
                }
                let subtract = name.contains("SUB");
                return self.date_arith(parts[0], parts[1], subtract);
            }
            "SUBSTRING" | "SUBSTR" | "MID"
                if find_top(inner, 0, |t| t.is_word("FROM")).is_some() =>
            {
                let parts = split_top(inner, |t| t.is_any_word(&["FROM", "FOR"]));
                let args: Vec<String> = parts
                    .into_iter()
                    .map(|p| self.arg(p))
                    .collect::<SqlResult<_>>()?;
                return Ok(Some(format!("substr({})", args.join(", "))));
            }
            "TRIM"
                if find_top(inner, 0, |t| {
                    t.is_any_word(&["BOTH", "LEADING", "TRAILING", "FROM"])
                })
                .is_some() =>
            {
                let Some(from) = find_top(inner, 0, |t| t.is_word("FROM")) else {
                    return Ok(None);
                };
                let mut spec = &inner[..from];
                let function = match spec.first().map(Token::upper).as_deref() {
                    Some("LEADING") => "ltrim",
                    Some("TRAILING") => "rtrim",
                    _ => "trim",
                };
                if spec
                    .first()
                    .is_some_and(|t| t.is_any_word(&["BOTH", "LEADING", "TRAILING"]))
                {
                    spec = &spec[1..];
                }
                let target = self.arg(&inner[from + 1..])?;
                return Ok(Some(if spec.is_empty() {
                    format!("{function}({target})")
                } else {
                    format!("{function}({target}, {})", self.arg(spec)?)
                }));
            }
            _ => {}
        }

        let args = self.args(inner)?;
        let count = args.len();
        let arg = |index: usize| args[index].as_str();
        let text = match (name, count) {
            ("NOW" | "CURRENT_TIMESTAMP" | "LOCALTIME" | "LOCALTIMESTAMP" | "SYSDATE" | "UTC_TIMESTAMP", 0) => {
                "(datetime('now'))".to_owned()
            }
            ("NOW" | "CURRENT_TIMESTAMP" | "LOCALTIME" | "LOCALTIMESTAMP" | "SYSDATE" | "UTC_TIMESTAMP", 1) => {
                if arg(0) == "0" {
                    "(datetime('now'))".to_owned()
                } else {
                    "(strftime('%Y-%m-%d %H:%M:%f', 'now'))".to_owned()
                }
            }
            ("CURDATE" | "CURRENT_DATE" | "UTC_DATE", 0) => "(date('now'))".to_owned(),
            ("CURTIME" | "CURRENT_TIME" | "UTC_TIME", 0 | 1) => "(time('now'))".to_owned(),
            ("UNIX_TIMESTAMP", 0) => "(CAST(strftime('%s', 'now') AS INTEGER))".to_owned(),
            ("UNIX_TIMESTAMP", 1) => format!("(CAST(strftime('%s', {}) AS INTEGER))", arg(0)),
            ("FROM_UNIXTIME", 1) => format!("(datetime({}, 'unixepoch'))", arg(0)),
            ("CONCAT", 1..) => {
                let joined: Vec<String> = args.iter().map(|a| format!("({a})")).collect();
                if count == 1 {
                    format!("({} || '')", joined[0])
                } else {
                    format!("({})", joined.join(" || "))
                }
            }
            ("IF", 3) => format!("iif({}, {}, {})", arg(0), arg(1), arg(2)),
            ("LAST_INSERT_ID", 0) => env.last_insert_id.to_string(),
            ("DATABASE" | "SCHEMA", 0) => quote_str(env.database),
            ("VERSION", 0) => quote_str(crate::SERVER_VERSION),
            ("USER" | "CURRENT_USER" | "SESSION_USER" | "SYSTEM_USER", 0) => {
                quote_str(&format!("{}@{}", env.user, env.host))
            }
            ("CONNECTION_ID", 0) => env.connection_id.to_string(),
            ("RAND", 0) => "(abs(random()) / 9223372036854775808.0)".to_owned(),
            ("UUID", 0) => "(SELECT lower(substr(h, 1, 8) || '-' || substr(h, 9, 4) || '-4' || substr(h, 14, 3) || '-' || substr('89ab', 1 + (abs(random()) % 4), 1) || substr(h, 18, 3) || '-' || substr(h, 21, 12)) FROM (SELECT hex(randomblob(16)) AS h))".to_owned(),
            ("LENGTH" | "OCTET_LENGTH", 1) => format!("octet_length({})", arg(0)),
            ("CHAR_LENGTH" | "CHARACTER_LENGTH", 1) => format!("length({})", arg(0)),
            ("LCASE", 1) => format!("lower({})", arg(0)),
            ("UCASE", 1) => format!("upper({})", arg(0)),
            ("SUBSTRING" | "MID", 2 | 3) => format!("substr({})", args.join(", ")),
            ("LOCATE", 2) => format!("instr({}, {})", arg(1), arg(0)),
            ("POSITION", 1) => {
                // POSITION(a IN b)
                let parts = split_top(inner, |t| t.is_word("IN"));
                if parts.len() != 2 {
                    return Ok(None);
                }
                format!("instr({}, {})", self.arg(parts[1])?, self.arg(parts[0])?)
            }
            ("CEILING", 1) => format!("ceil({})", arg(0)),
            ("GREATEST", 2..) => format!("max({})", args.join(", ")),
            ("LEAST", 2..) => format!("min({})", args.join(", ")),
            ("ISNULL", 1) => format!("(({}) IS NULL)", arg(0)),
            ("YEAR", 1) => format!("CAST(strftime('%Y', {}) AS INTEGER)", arg(0)),
            ("MONTH", 1) => format!("CAST(strftime('%m', {}) AS INTEGER)", arg(0)),
            ("DAY" | "DAYOFMONTH", 1) => format!("CAST(strftime('%d', {}) AS INTEGER)", arg(0)),
            ("HOUR", 1) => format!("CAST(strftime('%H', {}) AS INTEGER)", arg(0)),
            ("MINUTE", 1) => format!("CAST(strftime('%M', {}) AS INTEGER)", arg(0)),
            ("SECOND", 1) => format!("CAST(strftime('%S', {}) AS INTEGER)", arg(0)),
            ("DAYOFWEEK", 1) => format!("(CAST(strftime('%w', {}) AS INTEGER) + 1)", arg(0)),
            ("DAYOFYEAR", 1) => format!("CAST(strftime('%j', {}) AS INTEGER)", arg(0)),
            ("DATEDIFF", 2) => format!(
                "CAST(julianday(date({})) - julianday(date({})) AS INTEGER)",
                arg(0),
                arg(1)
            ),
            ("DATE_FORMAT", 2) => {
                let Some(format) = inner.last().filter(|t| t.kind == Kind::Str) else {
                    return Ok(None);
                };
                let Some(converted) = strftime_format(&format.value) else {
                    return Ok(None);
                };
                format!("strftime({}, {})", quote_str(&converted), arg(0))
            }
            ("JSON_UNQUOTE", 1) => format!(
                "(CASE WHEN json_valid({0}) AND json_type({0}) = 'text' THEN json_extract({0}, '$') ELSE {0} END)",
                arg(0)
            ),
            _ => return Ok(None),
        };
        Ok(Some(text))
    }

    fn group_concat(&self, inner: &[Token]) -> SqlResult<String> {
        let separator_index = find_top(inner, 0, |t| t.is_word("SEPARATOR"));
        let (body, separator) = match separator_index {
            Some(index) => (&inner[..index], Some(self.arg(&inner[index + 1..])?)),
            None => (inner, None),
        };
        let order_index = find_top(body, 0, |t| t.is_word("ORDER"));
        let (expression, order) = match order_index {
            Some(index) => (&body[..index], Some(self.arg(&body[index..])?)),
            None => (body, None),
        };
        let distinct = expression.first().is_some_and(|t| t.is_word("DISTINCT"));
        let expression = if distinct {
            &expression[1..]
        } else {
            expression
        };
        let parts = split_commas(expression);
        let value = if parts.len() > 1 {
            let rendered: Vec<String> = parts
                .into_iter()
                .map(|p| self.arg(p).map(|a| format!("({a})")))
                .collect::<SqlResult<_>>()?;
            format!("({})", rendered.join(" || "))
        } else {
            self.arg(expression)?
        };
        let mut text = format!(
            "group_concat({}{value}",
            if distinct { "DISTINCT " } else { "" }
        );
        let separator = separator.unwrap_or_else(|| "','".into());
        if !distinct || separator != "','" {
            text.push_str(", ");
            text.push_str(&separator);
        }
        if let Some(order) = order {
            text.push(' ');
            text.push_str(&order);
        }
        text.push(')');
        Ok(text)
    }

    fn date_arith(
        &self,
        date: &[Token],
        interval: &[Token],
        subtract: bool,
    ) -> SqlResult<Option<String>> {
        let date = self.arg(date)?;
        let (amount, unit) =
            if interval.first().is_some_and(|t| t.is_word("INTERVAL")) && interval.len() >= 3 {
                let unit = interval[interval.len() - 1].upper();
                (self.arg(&interval[1..interval.len() - 1])?, unit)
            } else {
                (self.arg(interval)?, "DAY".to_owned())
            };
        let (modifier, multiplier) = match unit.as_str() {
            "SECOND" => ("seconds", ""),
            "MINUTE" => ("minutes", ""),
            "HOUR" => ("hours", ""),
            "DAY" => ("days", ""),
            "WEEK" => ("days", " * 7"),
            "MONTH" => ("months", ""),
            "QUARTER" => ("months", " * 3"),
            "YEAR" => ("years", ""),
            _ => return Ok(None),
        };
        let sign = if subtract { "-" } else { "" };
        Ok(Some(format!(
            "datetime({date}, printf('%+d {modifier}', {sign}({amount}){multiplier}))"
        )))
    }
}

enum TableItem<'t> {
    Column(&'t [Token]),
    PrimaryKey(Vec<Token>),
    Constraint(Vec<Token>),
    Index(Vec<Token>),
    Skip,
}

/// Number of tokens of a leading `CONSTRAINT [name]` prefix.
fn constraint_offset(item: &[Token]) -> usize {
    if !item.first().is_some_and(|t| t.is_word("CONSTRAINT")) {
        return 0;
    }
    match item.get(1) {
        Some(t) if t.is_any_word(&["PRIMARY", "UNIQUE", "FOREIGN", "CHECK"]) => 1,
        Some(_) => 2,
        None => 1,
    }
}

/// Index column names (and optional ASC/DESC) with MySQL prefix lengths
/// such as `name(10)` removed.
fn index_columns(tokens: &[Token]) -> Vec<(String, Option<String>)> {
    split_commas(tokens)
        .into_iter()
        .filter_map(|part| {
            let name = part.first()?.ident()?.to_owned();
            let order = part
                .iter()
                .find(|t| t.is_any_word(&["ASC", "DESC"]))
                .map(Token::upper);
            Some((name, order))
        })
        .collect()
}

fn render_index_columns(tokens: &[Token]) -> String {
    index_columns(tokens)
        .into_iter()
        .map(|(name, order)| match order {
            Some(order) => format!("{} {order}", quote_ident(&name)),
            None => quote_ident(&name),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Indexes declared inline by `CREATE TABLE` are named `<table>_<name>`;
/// dropping by the MySQL name tries both spellings.
fn drop_index_statements(table: &str, name: &str) -> Vec<Vec<Token>> {
    vec![
        vec![Token::raw(format!(
            "DROP INDEX IF EXISTS {}",
            quote_ident(&format!("{table}_{name}"))
        ))],
        vec![Token::raw(format!(
            "DROP INDEX IF EXISTS {}",
            quote_ident(name)
        ))],
    ]
}

fn strip_dml_modifiers(tokens: &[Token]) -> Vec<Token> {
    let mut out = vec![tokens[0].clone()];
    let mut i = 1;
    while let Some(token) = tokens.get(i) {
        if token.is_any_word(&["LOW_PRIORITY", "QUICK"]) {
            i += 1;
        } else if token.is_word("IGNORE") {
            if tokens[0].is_word("UPDATE") {
                out.push(Token::raw("OR IGNORE"));
            }
            i += 1;
        } else {
            break;
        }
    }
    out.extend(tokens[i..].iter().cloned());
    out
}

/// Maps a MySQL `CAST` target type to SQLite.
fn cast(expression: &str, type_tokens: &[Token]) -> String {
    let words: Vec<String> = type_tokens
        .iter()
        .filter(|t| t.kind == Kind::Word)
        .map(Token::upper)
        .collect();
    match words.first().map(String::as_str).unwrap_or_default() {
        "SIGNED" | "UNSIGNED" | "INT" | "INTEGER" | "BIGINT" => {
            format!("CAST({expression} AS INTEGER)")
        }
        "CHAR" | "NCHAR" | "VARCHAR" | "TEXT" | "NATIONAL" => format!("CAST({expression} AS TEXT)"),
        "DATE" => format!("date({expression})"),
        "DATETIME" | "TIMESTAMP" => format!("datetime({expression})"),
        "TIME" => format!("time({expression})"),
        "DECIMAL" | "NUMERIC" => format!("CAST({expression} AS NUMERIC)"),
        "DOUBLE" | "FLOAT" | "REAL" => format!("CAST({expression} AS REAL)"),
        "BINARY" => format!("CAST({expression} AS BLOB)"),
        "JSON" => format!("json({expression})"),
        _ => format!("CAST({expression} AS {})", render(type_tokens).trim()),
    }
}

/// Converts a MySQL `DATE_FORMAT` pattern to `strftime`, when every
/// specifier has an equivalent.
fn strftime_format(mysql: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = mysql.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let spec = chars.next()?;
        out.push_str(match spec {
            'Y' => "%Y",
            'm' => "%m",
            'd' => "%d",
            'H' => "%H",
            'i' => "%M",
            's' | 'S' => "%S",
            'f' => "%f",
            'j' => "%j",
            'T' => "%H:%M:%S",
            'w' => "%w",
            '%' => "%%",
            _ => return None,
        });
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::{split_statements, tokenize};

    fn env_translate(sql: &str, params: &[&str]) -> SqlResult<String> {
        let user_vars = HashMap::from([("x".to_owned(), "42".to_owned())]);
        let column = |name: &str, default: Option<&str>, rowid: bool| ColumnInfo {
            name: name.into(),
            default: default.map(str::to_owned),
            rowid,
        };
        let tables = HashMap::from([(
            "d1".to_owned(),
            vec![
                column("id", None, true),
                column("v", Some("'dv'"), false),
                column("n", None, false),
            ],
        )]);
        let system_var = |name: &str| match name {
            "version_comment" => Some("'NovaDB'".to_owned()),
            "max_allowed_packet" => Some("67108864".to_owned()),
            _ => None,
        };
        let env = Env {
            database: "shop",
            user: "root",
            host: "localhost",
            connection_id: 7,
            last_insert_id: 99,
            system_var: &system_var,
            user_vars: &user_vars,
            tables: &tables,
            fk_names: &[],
        };
        let params: Vec<String> = params.iter().map(|p| (*p).to_owned()).collect();
        let statements = split_statements(tokenize(sql)?);
        let mut out = Vec::new();
        for statement in statements {
            out.extend(translate(&statement, &env, &params)?);
        }
        Ok(out.join(";\n"))
    }

    fn t(sql: &str) -> String {
        env_translate(sql, &[]).unwrap()
    }

    #[test]
    fn default_keyword_in_values() {
        // Column omitted when DEFAULT in every row.
        assert_eq!(
            t("INSERT INTO d1 (id, v) VALUES (DEFAULT, 'x')"),
            "INSERT INTO \"d1\" (\"v\") VALUES ( 'x')"
        );
        assert_eq!(
            t("INSERT INTO d1 (id, v) VALUES (DEFAULT, 'x'), (DEFAULT, 'y')"),
            "INSERT INTO \"d1\" (\"v\") VALUES ( 'x'), ( 'y')"
        );
        // Mixed rows: NULL for the rowid column, declared default otherwise.
        assert_eq!(
            t("INSERT INTO d1 (id, v) VALUES (1, DEFAULT), (DEFAULT, 'y')"),
            "INSERT INTO \"d1\" (\"id\", \"v\") VALUES (1, 'dv'), ( NULL, 'y')"
        );
        assert_eq!(
            t("INSERT INTO d1 (id, n) VALUES (1, DEFAULT), (2, 5)"),
            "INSERT INTO \"d1\" (\"id\", \"n\") VALUES (1, NULL), (2, 5)"
        );
        // No column list.
        assert_eq!(
            t("INSERT INTO d1 VALUES (DEFAULT, DEFAULT, 3)"),
            "INSERT INTO \"d1\" VALUES ( NULL, 'dv', 3)"
        );
        assert_eq!(
            t("INSERT INTO d1 (id, v) VALUES (DEFAULT, DEFAULT)"),
            "INSERT INTO \"d1\" DEFAULT VALUES"
        );
        // Unknown table: NULL.
        assert_eq!(
            t("INSERT INTO other (a, b) VALUES (DEFAULT, 1), (2, DEFAULT)"),
            "INSERT INTO \"other\" (\"a\", \"b\") VALUES ( NULL, 1), (2, NULL)"
        );
        // DEFAULT inside a string or a function call is untouched.
        assert_eq!(
            t("INSERT INTO d1 (v) VALUES ('DEFAULT')"),
            "INSERT INTO \"d1\" (v) VALUES ('DEFAULT')"
        );
    }

    #[test]
    fn identifiers_and_strings() {
        assert_eq!(
            t("SELECT `id`, `we\"ird` FROM `users`"),
            "SELECT \"id\", \"we\"\"ird\" FROM \"users\""
        );
        assert_eq!(t(r#"SELECT "text", 'it\'s'"#), "SELECT 'text', 'it''s'");
        assert_eq!(t("SELECT _utf8mb4'x', N'y'"), "SELECT 'x', 'y'");
    }

    #[test]
    fn string_contents_are_never_rewritten() {
        // Payloads that look like SQL, functions or MySQL syntax stay literal.
        let cases = [
            (
                "SELECT 'NOW() LIMIT 1, 2 `x` || AUTO_INCREMENT'",
                "SELECT 'NOW() LIMIT 1, 2 `x` || AUTO_INCREMENT'",
            ),
            (
                "INSERT INTO t VALUES ('); DROP TABLE x; --')",
                "INSERT INTO \"t\" VALUES ('); DROP TABLE x; --')",
            ),
            (
                r"INSERT INTO t VALUES ('\'); DROP TABLE x; --')",
                "INSERT INTO \"t\" VALUES ('''); DROP TABLE x; --')",
            ),
            (
                r#"SELECT "\"); DROP TABLE x; --""#,
                r#"SELECT '"); DROP TABLE x; --'"#,
            ),
            (r"SELECT 'a\\', 'b'", r"SELECT 'a\', 'b'"),
            (
                "SELECT '/* not a comment */', '-- nor this', '# nor this'",
                "SELECT '/* not a comment */', '-- nor this', '# nor this'",
            ),
            (
                "SELECT 'ON DUPLICATE KEY UPDATE' FROM t",
                "SELECT 'ON DUPLICATE KEY UPDATE' FROM t",
            ),
        ];
        for (sql, expected) in cases {
            assert_eq!(t(sql), expected, "{sql}");
        }
        // The statement count is decided by the tokenizer, not by `;` inside strings.
        assert_eq!(
            split_statements(tokenize("SELECT '); DROP TABLE x; --'").unwrap()).len(),
            1
        );
        // A backslash before the closing quote cannot terminate the literal early.
        assert!(tokenize(r"SELECT 'abc\'; DROP TABLE x; -- ").is_err());
    }

    #[test]
    fn binary_literals_are_byte_exact() {
        let tokens = crate::lexer::tokenize_bytes(
            b"INSERT INTO t VALUES (_binary'\0\xff\x10''', '\xc3\xa9', 'a\\'\xfe')",
        )
        .unwrap();
        let user_vars = HashMap::new();
        let tables = HashMap::new();
        let system_var = |_: &str| None;
        let env = Env {
            database: "d",
            user: "u",
            host: "h",
            connection_id: 1,
            last_insert_id: 0,
            system_var: &system_var,
            user_vars: &user_vars,
            tables: &tables,
            fk_names: &[],
        };
        assert_eq!(
            translate(&tokens, &env, &[]).unwrap(),
            ["INSERT INTO \"t\" VALUES (X'00FF1027', '\u{e9}', X'6127FE')"]
        );
        // Bytes outside string literals must be valid UTF-8.
        assert!(crate::lexer::tokenize_bytes(b"SELECT \xff").is_err());
        assert_eq!(t("SELECT _binary'ab'"), "SELECT X'6162'");
    }

    #[test]
    fn having_without_group_by_becomes_an_outer_filter() {
        assert_eq!(
            t("SELECT a AS x FROM t WHERE b = 1 HAVING x IN ('p') ORDER BY x LIMIT 5"),
            "SELECT * FROM (SELECT a AS x FROM t WHERE b = 1 ORDER BY x) WHERE x IN ('p') LIMIT 5"
        );
        assert_eq!(
            t("SELECT count(*) AS n FROM t HAVING n > 1"),
            "SELECT count(*) AS n FROM t HAVING n > 1"
        );
        assert_eq!(
            t("SELECT a FROM t GROUP BY a HAVING a > 1"),
            "SELECT a FROM t GROUP BY a HAVING a > 1"
        );
        assert_eq!(
            t("SELECT * FROM (SELECT a FROM t HAVING a > 1) s"),
            "SELECT * FROM (SELECT * FROM (SELECT a FROM t) WHERE a > 1) s"
        );
    }

    #[test]
    fn limit_offset_and_placeholders() {
        assert_eq!(
            t("SELECT * FROM t LIMIT 10, 20"),
            "SELECT * FROM t LIMIT 20 OFFSET 10"
        );
        assert_eq!(
            env_translate(
                "SELECT * FROM t WHERE a = ? LIMIT ?, ?",
                &["'x'", "5", "10"]
            )
            .unwrap(),
            "SELECT * FROM t WHERE a = 'x' LIMIT 10 OFFSET 5"
        );
        // Values are substituted exactly once, never re-scanned.
        assert_eq!(
            env_translate("SELECT ?, ?", &["'?'", "'@x'"]).unwrap(),
            "SELECT '?', '@x'"
        );
        assert_eq!(
            env_translate("SELECT 1 - ?", &["(-5)"]).unwrap(),
            "SELECT 1 - (-5)"
        );
        assert!(env_translate("SELECT ?", &[]).is_err());
        assert!(env_translate("SELECT 1", &["1"]).is_err());
        assert_eq!(t("SELECT '?' , @x, @missing"), "SELECT '?' , 42, NULL");
    }

    #[test]
    fn create_table_translation() {
        let sql = t("CREATE TABLE IF NOT EXISTS `users` (
            `id` int(11) unsigned NOT NULL AUTO_INCREMENT,
            `email` varchar(255) CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci NOT NULL COMMENT 'login',
            `status` enum('active','banned') DEFAULT 'active',
            `created_at` timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
            PRIMARY KEY (`id`),
            UNIQUE KEY `email_unique` (`email`),
            KEY `idx_status` (`status`(10)),
            FULLTEXT KEY `ft` (`email`)
        ) ENGINE=InnoDB AUTO_INCREMENT=5 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci");
        assert_eq!(
            sql,
            "CREATE TABLE IF NOT EXISTS \"users\" ( \"id\" INTEGER PRIMARY KEY AUTOINCREMENT /*my:int(11)*/ NOT NULL, \
             \"email\" varchar(255) NOT NULL, \
             \"status\" TEXT CHECK (\"status\" IN ('active', 'banned')) DEFAULT 'active', \
             \"created_at\" timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP, \
             UNIQUE (\"email\") );\n\
             CREATE INDEX IF NOT EXISTS \"users_idx_status\" ON \"users\" (\"status\")"
        );
        assert_eq!(
            t("CREATE TABLE t (id BIGINT PRIMARY KEY AUTO_INCREMENT, n INT DEFAULT NOW())"),
            "CREATE TABLE \"t\" ( \"id\" INTEGER PRIMARY KEY AUTOINCREMENT /*my:BIGINT*/, \"n\" INT DEFAULT (datetime('now')) )"
        );
        // Composite keys cannot be AUTOINCREMENT in SQLite: the flag is dropped.
        assert_eq!(
            t("CREATE TABLE t (a INT AUTO_INCREMENT, b INT, PRIMARY KEY (a, b))"),
            "CREATE TABLE \"t\" ( \"a\" INT, \"b\" INT, PRIMARY KEY (\"a\", \"b\") )"
        );
        assert_eq!(
            t("CREATE TABLE shop.t (`year` YEAR(4), d DATETIME(6))"),
            "CREATE TABLE \"t\" ( \"year\" YEAR(4), \"d\" DATETIME(6) )"
        );
    }

    #[test]
    fn dml_translation() {
        assert_eq!(
            t("INSERT IGNORE INTO t (a) VALUES (1)"),
            "INSERT OR IGNORE INTO \"t\" (a) VALUES (1)"
        );
        assert_eq!(
            t(
                "INSERT INTO t (a, b) VALUES (1, 2) ON DUPLICATE KEY UPDATE b = VALUES(b), c = c + 1"
            ),
            "INSERT INTO \"t\" (a, b) VALUES (1, 2) ON CONFLICT DO UPDATE SET b = excluded.\"b\", c = c + 1"
        );
        assert_eq!(
            t("INSERT INTO t (a) VALUES (1) AS new ON DUPLICATE KEY UPDATE a = new.a"),
            "INSERT INTO \"t\" (a) VALUES (1) ON CONFLICT DO UPDATE SET a = excluded.\"a\""
        );
        assert_eq!(
            t("INSERT INTO t (a) SELECT a FROM s ON DUPLICATE KEY UPDATE a = 1"),
            "INSERT INTO \"t\" (a) SELECT a FROM s WHERE true ON CONFLICT DO UPDATE SET a = 1"
        );
        assert_eq!(
            t("INSERT INTO t SET a = 1, b = 'x'"),
            "INSERT INTO \"t\" (\"a\", \"b\") VALUES ( 1, 'x')"
        );
        assert_eq!(
            t("INSERT INTO t () VALUES ()"),
            "INSERT INTO \"t\" DEFAULT VALUES"
        );
        assert_eq!(
            t("REPLACE INTO t VALUES (1)"),
            "REPLACE INTO \"t\" VALUES (1)"
        );
        assert_eq!(
            t("UPDATE IGNORE t SET a = 1"),
            "UPDATE OR IGNORE t SET a = 1"
        );
        assert_eq!(t("TRUNCATE TABLE `t`"), "DELETE FROM \"t\"");
        assert_eq!(
            t("DROP TABLE IF EXISTS a, `b`"),
            "DROP TABLE IF EXISTS \"a\";\nDROP TABLE IF EXISTS \"b\""
        );
        assert_eq!(t("SELECT * FROM t FOR UPDATE"), "SELECT * FROM t");
        assert_eq!(
            t("SELECT * FROM shop.t JOIN `shop`.u"),
            "SELECT * FROM t JOIN u"
        );
    }

    #[test]
    fn functions_and_operators() {
        assert_eq!(t("SELECT NOW()"), "SELECT (datetime('now')) AS \"NOW()\"");
        assert_eq!(t("SELECT CURDATE() AS d"), "SELECT (date('now')) AS d");
        assert_eq!(
            t("SELECT CONCAT(a, '-', b) FROM t"),
            "SELECT ((a) || ('-') || (b)) AS \"CONCAT(a, '-', b)\" FROM t"
        );
        assert_eq!(
            t("SELECT IFNULL(a, 0), IF(a > 1, 'y', 'n') x FROM t"),
            "SELECT IFNULL(a, 0), iif(a > 1, 'y', 'n') x FROM t"
        );
        assert_eq!(
            t("SELECT * FROM t WHERE a = 1 || b = 2 && c <=> NULL"),
            "SELECT * FROM t WHERE a = 1 OR b = 2 AND c IS NULL"
        );
        assert_eq!(
            t("SELECT UNIX_TIMESTAMP(), LAST_INSERT_ID(), DATABASE()"),
            "SELECT (CAST(strftime('%s', 'now') AS INTEGER)) AS \"UNIX_TIMESTAMP()\", 99 AS \"LAST_INSERT_ID()\", 'shop' AS \"DATABASE()\""
        );
        assert_eq!(
            t("SELECT @@version_comment LIMIT 1"),
            "SELECT 'NovaDB' AS \"@@version_comment\" LIMIT 1"
        );
        assert_eq!(
            t("SELECT @@session.max_allowed_packet AS m, @@nope"),
            "SELECT 67108864 AS m, NULL AS \"@@nope\""
        );
        assert_eq!(
            t("SELECT CAST(a AS SIGNED), CAST(b AS CHAR(10)) c FROM t"),
            "SELECT CAST(a AS INTEGER) AS \"CAST(a AS SIGNED)\", CAST(b AS TEXT) c FROM t"
        );
        assert_eq!(
            t("SELECT GROUP_CONCAT(name ORDER BY name SEPARATOR '; ') g FROM t"),
            "SELECT group_concat(name, '; ' ORDER BY name) g FROM t"
        );
        assert_eq!(
            t("SELECT DATE_ADD(d, INTERVAL 2 DAY) x FROM t"),
            "SELECT datetime(d, printf('%+d days', (2))) x FROM t"
        );
        assert_eq!(t("SELECT 1 FROM DUAL"), "SELECT 1");
        assert_eq!(
            t("SELECT a FROM t WHERE a LIKE 'x\\_%'"),
            "SELECT a FROM t WHERE a LIKE 'x\\_%' ESCAPE '\\'"
        );
        assert_eq!(t("SELECT a COLLATE utf8mb4_bin FROM t"), "SELECT a FROM t");
        // A table named like a function is not rewritten.
        assert_eq!(
            t("INSERT INTO now (a) VALUES (1)"),
            "INSERT INTO \"now\" (a) VALUES (1)"
        );
    }

    #[test]
    fn alter_and_index_translation() {
        assert_eq!(
            t(
                "ALTER TABLE `t` ADD COLUMN `c` INT UNSIGNED NOT NULL DEFAULT 0 AFTER `b`, ADD INDEX `i` (`c`)"
            ),
            "ALTER TABLE \"t\" ADD COLUMN \"c\" INT NOT NULL DEFAULT 0;\nCREATE INDEX \"i\" ON \"t\" (\"c\")"
        );
        assert_eq!(t("ALTER TABLE t ENGINE=InnoDB"), "");
        assert!(matches!(
            env_translate("ALTER TABLE t MODIFY a INT", &[]),
            Err(SqlError::Unsupported(_))
        ));
        assert_eq!(
            t("CREATE UNIQUE INDEX `u` USING BTREE ON `t` (`a`(5) DESC, b)"),
            "CREATE UNIQUE INDEX \"u\" ON \"t\" (\"a\" DESC, \"b\")"
        );
        assert_eq!(
            t("DROP INDEX i ON t"),
            "DROP INDEX IF EXISTS \"t_i\";\nDROP INDEX IF EXISTS \"i\""
        );
        assert_eq!(
            t("RENAME TABLE a TO b"),
            "ALTER TABLE \"a\" RENAME TO \"b\""
        );
    }

    #[test]
    fn current_database_column_qualifier_is_dropped() {
        assert_eq!(
            t(
                "SELECT `shop`.`User`.`id`, shop.u.* FROM `shop`.`User` WHERE `shop`.`User`.`id` = 1"
            ),
            "SELECT \"User\".\"id\", u.* FROM \"User\" WHERE \"User\".\"id\" = 1"
        );
        // Other databases are left alone.
        assert!(t("SELECT other.u.id FROM u").contains("other.u.id"));
    }

    #[test]
    fn binary_prefix_operator_is_dropped() {
        assert_eq!(
            t(
                "SELECT DISTINCT BINARY a.n, BINARY(b) FROM a WHERE BINARY x = 'y' ORDER BY BINARY a.n"
            ),
            "SELECT DISTINCT a.n,(b) FROM a WHERE x = 'y' ORDER BY a.n"
        );
        // `AS BINARY` / `COLLATE BINARY` are types and collations, not the operator.
        assert!(t("SELECT CAST(x AS BINARY) FROM a").contains("CAST"));
        assert!(t("SELECT x FROM a ORDER BY y COLLATE BINARY").ends_with("COLLATE BINARY"));
    }

    #[test]
    fn alter_plan_detects_rebuild_actions() {
        let user_vars = HashMap::new();
        let tables = HashMap::new();
        let system_var = |_: &str| None;
        let env = Env {
            database: "shop",
            user: "root",
            host: "localhost",
            connection_id: 1,
            last_insert_id: 0,
            system_var: &system_var,
            user_vars: &user_vars,
            tables: &tables,
            fk_names: &[],
        };
        let plan = |sql: &str| {
            let tokens = tokenize(sql).unwrap();
            alter_plan(&tokens, &env, &[]).unwrap()
        };
        assert!(plan("ALTER TABLE t ADD COLUMN c INT").is_none());
        let (table, steps) = plan(
            "ALTER TABLE `t` ADD CONSTRAINT `fk1` FOREIGN KEY (`a`) REFERENCES `u` (`id`) ON DELETE CASCADE",
        )
        .unwrap();
        assert_eq!(table, "t");
        let [PlanStep::Rebuild { edit, rename: None }] = steps.as_slice() else {
            panic!("{steps:?}")
        };
        assert_eq!(
            edit,
            &RebuildEdit::AddConstraint {
                name: Some("fk1".into()),
                sql: "FOREIGN KEY (\"a\") REFERENCES \"u\" (\"id\") ON DELETE CASCADE".into(),
                foreign_key: true,
            }
        );
        let (_, steps) = plan(
            "ALTER TABLE t ADD COLUMN c INT, CHANGE a b VARCHAR(10) NOT NULL, DROP FOREIGN KEY fk1",
        )
        .unwrap();
        assert_eq!(steps.len(), 3);
        assert!(matches!(&steps[0], PlanStep::Sql(sql) if sql.len() == 1));
        assert!(matches!(
            &steps[1],
            PlanStep::Rebuild { rename: Some((old, new)), .. } if old == "a" && new == "b"
        ));
        assert!(matches!(
            &steps[2],
            PlanStep::Rebuild { edit: RebuildEdit::DropConstraint { name, foreign_key_only: true }, .. } if name == "fk1"
        ));
        // Unsupported forms still fail with a clear error.
        let tokens = tokenize("ALTER TABLE t DROP PRIMARY KEY").unwrap();
        assert!(matches!(
            alter_plan(&tokens, &env, &[]),
            Err(SqlError::Unsupported(_))
        ));
    }

    #[test]
    fn information_schema_queries_use_ctes() {
        let sql =
            t("SELECT TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE()");
        assert!(
            sql.starts_with("WITH \"information_schema.tables\" AS ("),
            "{sql}"
        );
        assert!(
            sql.ends_with(
                "SELECT TABLE_NAME FROM \"information_schema.tables\" WHERE TABLE_SCHEMA = 'shop'"
            ),
            "{sql}"
        );
    }
}
