//! Static analysis of statement token streams: table references, select
//! lists, result-column typing and query-parameter type inference.
//!
//! The wire protocol needs types *before* rows exist (Describe) and SQLite
//! only reports declared types for plain column references, so this module
//! resolves what it can from the schema and leaves the rest to value-based
//! inference.

use std::collections::HashMap;
use std::time::Instant;

use crate::lexer::{Kind, Token, find_close, split_commas};
use crate::type_map::{self, oid};

/// One column of a table as reported by `pragma_table_info`.
#[derive(Debug, Clone)]
pub struct ColumnInfo {
    pub name: String,
    pub decl: String,
    pub default: Option<String>,
    pub pk: i64,
}

#[derive(Debug, Clone)]
pub struct TableInfo {
    pub columns: Vec<ColumnInfo>,
}

impl TableInfo {
    pub fn column(&self, name: &str) -> Option<&ColumnInfo> {
        self.columns
            .iter()
            .find(|column| column.name.eq_ignore_ascii_case(name))
    }
}

/// Session-level cache of table definitions. `None` marks a name that is not
/// a table or view.
#[derive(Debug)]
pub struct Schema {
    pub tables: HashMap<String, Option<TableInfo>>,
    pub version: i64,
    pub checked: Instant,
}

impl Default for Schema {
    fn default() -> Self {
        Self {
            tables: HashMap::new(),
            version: -1,
            checked: Instant::now(),
        }
    }
}

impl Schema {
    pub fn table(&self, name: &str) -> Option<&TableInfo> {
        let key = name.to_ascii_lowercase();
        match self.tables.get(&key) {
            Some(Some(found)) => Some(found),
            _ => crate::catalog::static_lookup(&key),
        }
    }

    pub fn clear(&mut self) {
        self.tables.clear();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    pub name: String,
    pub alias: String,
}

#[derive(Debug, Clone)]
pub struct SelectItem {
    /// Expression tokens without the trailing alias.
    pub expr: Vec<Token>,
    pub alias: Option<String>,
}

/// How an expression is shaped, for naming and typing purposes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExprShape {
    Star(Option<String>),
    Column(Option<String>, String),
    Call(String),
    Other,
}

impl SelectItem {
    pub fn shape(&self) -> ExprShape {
        shape_of(&self.expr)
    }
}

const NOT_ALIAS: &[&str] = &[
    "end",
    "null",
    "true",
    "false",
    "asc",
    "desc",
    "distinct",
    "all",
    "and",
    "or",
    "not",
    "is",
    "in",
    "like",
    "ilike",
    "between",
    "then",
    "else",
    "when",
    "case",
    "from",
    "by",
    "on",
    "using",
    "over",
    "filter",
    "within",
    "collate",
    "isnull",
    "notnull",
    "current_timestamp",
    "current_date",
    "current_time",
];

pub fn shape_of(expr: &[Token]) -> ExprShape {
    match expr {
        [star] if star.is_op("*") => ExprShape::Star(None),
        [qual, dot, star] if dot.is_op(".") && star.is_op("*") => ExprShape::Star(qual.ident()),
        [name] if name.ident().is_some() && !name.is_any_word(NOT_ALIAS) => {
            ExprShape::Column(None, name.ident().unwrap_or_default())
        }
        [qual, dot, name] if dot.is_op(".") && qual.ident().is_some() && name.ident().is_some() => {
            ExprShape::Column(qual.ident(), name.ident().unwrap_or_default())
        }
        [_, dot1, qual, dot2, name]
            if dot1.is_op(".")
                && dot2.is_op(".")
                && qual.ident().is_some()
                && name.ident().is_some() =>
        {
            ExprShape::Column(qual.ident(), name.ident().unwrap_or_default())
        }
        _ => {
            // Function call: name ( ... ) spanning the whole expression.
            if let Some(first) = expr.first()
                && first.kind == Kind::Word
                && expr.get(1).is_some_and(|t| t.is_op("("))
                && find_close(expr, 1) == Some(expr.len() - 1)
            {
                return ExprShape::Call(first.text.to_ascii_lowercase());
            }
            // Qualified function call: schema.name ( ... )
            if expr.len() > 4
                && expr[1].is_op(".")
                && expr[3].is_op("(")
                && find_close(expr, 3) == Some(expr.len() - 1)
                && let Some(name) = expr[2].ident()
            {
                return ExprShape::Call(name);
            }
            ExprShape::Other
        }
    }
}

/// Index of the first token after leading `WITH` clauses.
pub fn skip_with(tokens: &[Token]) -> usize {
    if !tokens.first().is_some_and(|t| t.is_word("WITH")) {
        return 0;
    }
    let mut i = 1;
    if tokens.get(i).is_some_and(|t| t.is_word("RECURSIVE")) {
        i += 1;
    }
    loop {
        // name [(cols)] AS [[NOT] MATERIALIZED] ( ... )
        i += 1;
        if tokens.get(i).is_some_and(|t| t.is_op("(")) {
            match find_close(tokens, i) {
                Some(close) => i = close + 1,
                None => return tokens.len(),
            }
        }
        while tokens
            .get(i)
            .is_some_and(|t| t.is_any_word(&["AS", "NOT", "MATERIALIZED"]))
        {
            i += 1;
        }
        if !tokens.get(i).is_some_and(|t| t.is_op("(")) {
            return tokens.len();
        }
        match find_close(tokens, i) {
            Some(close) => i = close + 1,
            None => return tokens.len(),
        }
        if tokens.get(i).is_some_and(|t| t.is_op(",")) {
            i += 1;
            continue;
        }
        return i;
    }
}

const FROM_END: &[&str] = &[
    "where",
    "group",
    "order",
    "limit",
    "having",
    "union",
    "intersect",
    "except",
    "offset",
    "fetch",
    "for",
    "window",
    "returning",
    "on",
];
const JOIN_WORDS: &[&str] = &[
    "join", "left", "right", "inner", "outer", "cross", "full", "natural", "lateral",
];

/// Parts of a top-level `SELECT`.
#[derive(Debug, Clone)]
pub struct SelectParts {
    /// Index of the first select-list token.
    pub items_start: usize,
    /// Index one past the last select-list token.
    pub items_end: usize,
    pub tables: Vec<TableRef>,
}

/// Locates the select list and table references of the statement's main
/// `SELECT` (after any `WITH` clause).
pub fn select_parts(tokens: &[Token]) -> Option<SelectParts> {
    let start = skip_with(tokens);
    if !tokens.get(start).is_some_and(|t| t.is_word("SELECT")) {
        return None;
    }
    let mut items_start = start + 1;
    match tokens.get(items_start) {
        Some(t) if t.is_word("ALL") => items_start += 1,
        Some(t) if t.is_word("DISTINCT") => {
            items_start += 1;
            if tokens.get(items_start).is_some_and(|t| t.is_word("ON")) {
                return None;
            }
        }
        _ => {}
    }
    let mut depth = 0_i32;
    let mut from = None;
    let mut end = tokens.len();
    for (index, token) in tokens.iter().enumerate().skip(items_start) {
        if token.is_op("(") {
            depth += 1;
        } else if token.is_op(")") {
            depth -= 1;
        } else if depth == 0 {
            if token.is_word("FROM") {
                from = Some(index);
                break;
            }
            if token.is_any_word(&[
                "WHERE",
                "GROUP",
                "ORDER",
                "LIMIT",
                "UNION",
                "INTERSECT",
                "EXCEPT",
                "HAVING",
                "OFFSET",
                "FOR",
                "WINDOW",
            ]) {
                end = index;
                break;
            }
        }
    }
    let items_end = from.unwrap_or(end);
    let tables = from.map(|f| parse_from(tokens, f + 1)).unwrap_or_default();
    Some(SelectParts {
        items_start,
        items_end,
        tables,
    })
}

/// Parses table references of a FROM clause starting at `start`.
pub fn parse_from(tokens: &[Token], start: usize) -> Vec<TableRef> {
    let mut tables = Vec::new();
    let mut i = start;
    let mut depth = 0_i32;
    let mut expect_table = true;
    while i < tokens.len() {
        let token = &tokens[i];
        if depth == 0 && token.kind == Kind::Word && FROM_END.contains(&token.lower().as_str()) {
            if token.is_word("ON") {
                // Skip the join condition up to the next JOIN / comma / clause.
                i += 1;
                let mut d = 0_i32;
                while i < tokens.len() {
                    let t = &tokens[i];
                    if t.is_op("(") {
                        d += 1;
                    } else if t.is_op(")") {
                        d -= 1;
                        if d < 0 {
                            return tables;
                        }
                    } else if d == 0
                        && (t.is_op(",")
                            || (t.kind == Kind::Word
                                && (JOIN_WORDS.contains(&t.lower().as_str())
                                    || FROM_END.contains(&t.lower().as_str()) && !t.is_word("ON"))))
                    {
                        break;
                    }
                    i += 1;
                }
                continue;
            }
            break;
        }
        if token.is_op("(") {
            if expect_table {
                // Subquery or parenthesised join: skip it and read its alias.
                let Some(close) = find_close(tokens, i) else {
                    break;
                };
                i = close + 1;
                if tokens.get(i).is_some_and(|t| t.is_word("AS")) {
                    i += 1;
                }
                if tokens
                    .get(i)
                    .is_some_and(|t| t.ident().is_some() && !is_clause_word(t))
                {
                    i += 1;
                }
                expect_table = false;
                continue;
            }
            depth += 1;
        } else if token.is_op(")") {
            if depth == 0 {
                break;
            }
            depth -= 1;
        } else if depth == 0 && token.is_op(",") {
            expect_table = true;
        } else if depth == 0
            && token.kind == Kind::Word
            && JOIN_WORDS.contains(&token.lower().as_str())
        {
            expect_table = true;
            if token.is_word("JOIN") || token.is_word("LATERAL") {
                // handled by next iteration with expect_table
            }
        } else if depth == 0 && token.is_word("USING") {
            if let Some(close) = tokens
                .get(i + 1)
                .filter(|t| t.is_op("("))
                .and_then(|_| find_close(tokens, i + 1))
            {
                i = close;
            }
        } else if depth == 0 && expect_table && token.ident().is_some() && !is_clause_word(token) {
            // [schema .] name [[AS] alias]
            let mut name = token.ident().unwrap_or_default();
            let mut j = i + 1;
            while tokens.get(j).is_some_and(|t| t.is_op(".")) {
                let Some(next) = tokens.get(j + 1).and_then(Token::ident) else {
                    break;
                };
                name = if name == "information_schema" {
                    format!("information_schema.{next}")
                } else {
                    next
                };
                j += 2;
            }
            if tokens.get(j).is_some_and(|t| t.is_op("(")) {
                // Table-valued function.
                if let Some(close) = find_close(tokens, j) {
                    j = close + 1;
                }
                tables.push(TableRef {
                    alias: name.clone(),
                    name: String::new(),
                });
            } else {
                let mut alias = name.clone();
                if tokens.get(j).is_some_and(|t| t.is_word("AS")) {
                    j += 1;
                    if let Some(a) = tokens.get(j).and_then(Token::ident) {
                        alias = a;
                        j += 1;
                    }
                } else if let Some(t) = tokens.get(j)
                    && t.ident().is_some()
                    && !is_clause_word(t)
                {
                    alias = t.ident().unwrap_or_default();
                    j += 1;
                }
                tables.push(TableRef { name, alias });
            }
            i = j;
            expect_table = false;
            continue;
        }
        i += 1;
    }
    tables
}

fn is_clause_word(token: &Token) -> bool {
    token.kind == Kind::Word && {
        let lower = token.lower();
        FROM_END.contains(&lower.as_str())
            || JOIN_WORDS.contains(&lower.as_str())
            || matches!(
                lower.as_str(),
                "using" | "as" | "set" | "values" | "select" | "default"
            )
    }
}

/// Splits a select list into items with optional aliases.
pub fn parse_items(tokens: &[Token]) -> Vec<SelectItem> {
    if tokens.is_empty() {
        return Vec::new();
    }
    split_commas(tokens)
        .into_iter()
        .map(|part| {
            let n = part.len();
            // A trailing `::type name` must not be mistaken for an alias.
            if let Some(cast) = part.iter().rposition(|t| t.is_op("::")) {
                if let Some((_, _, used)) = parse_type_name(&part[cast + 1..]) {
                    let end = cast + 1 + used;
                    let rest = &part[end.min(n)..];
                    let alias = match rest {
                        [] => Some(None),
                        [name] if name.ident().is_some() => Some(name.ident()),
                        [as_word, name] if as_word.is_word("AS") => Some(name.ident()),
                        _ => None,
                    };
                    if let Some(alias) = alias {
                        return SelectItem {
                            expr: part[..end.min(n)].to_vec(),
                            alias,
                        };
                    }
                }
            }
            if n >= 3 && part[n - 2].is_word("AS") {
                if let Some(alias) = part[n - 1].ident() {
                    return SelectItem {
                        expr: part[..n - 2].to_vec(),
                        alias: Some(alias),
                    };
                }
            }
            if n >= 2 {
                let last = &part[n - 1];
                let prev = &part[n - 2];
                let last_is_alias = last.ident().is_some()
                    && !last.is_any_word(NOT_ALIAS)
                    && matches!(
                        prev.kind,
                        Kind::Word | Kind::QIdent | Kind::Str | Kind::Num | Kind::Param | Kind::Raw
                    )
                    && !prev.is_any_word(&[
                        "AND", "OR", "NOT", "IS", "IN", "LIKE", "ILIKE", "BETWEEN", "THEN", "ELSE",
                        "WHEN", "CASE", "DISTINCT", "BY", "ON", "ALL", "ANY", "SOME", "COLLATE",
                    ])
                    || (last.ident().is_some()
                        && !last.is_any_word(NOT_ALIAS)
                        && prev.is_op(")")
                        && n > 2);
                if last_is_alias {
                    return SelectItem {
                        expr: part[..n - 1].to_vec(),
                        alias: last.ident(),
                    };
                }
            }
            SelectItem {
                expr: part.to_vec(),
                alias: None,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Typing
// ---------------------------------------------------------------------------

/// Parses a type name (possibly multi-word) starting at `tokens[0]`.
/// Returns the lowercase canonical name, whether it is an array type, and the
/// number of tokens consumed.
pub fn parse_type_name(tokens: &[Token]) -> Option<(String, bool, usize)> {
    let mut i = 0;
    // Optional pg_catalog. / public. qualifier.
    if tokens.get(1).is_some_and(|t| t.is_op("."))
        && tokens
            .first()
            .and_then(Token::ident)
            .is_some_and(|q| q == "pg_catalog" || q == "public")
    {
        i = 2;
    }
    let first = tokens.get(i)?;
    let mut name = first.ident()?;
    i += 1;
    let word = |j: usize| {
        tokens
            .get(j)
            .filter(|t| t.kind == Kind::Word)
            .map(Token::lower)
    };
    match name.as_str() {
        "double" if word(i).as_deref() == Some("precision") => {
            name = "double precision".into();
            i += 1;
        }
        "character" | "char" | "bit" | "national" if word(i).as_deref() == Some("varying") => {
            name = if name == "bit" {
                "bit varying"
            } else {
                "character varying"
            }
            .into();
            i += 1;
        }
        "timestamp" | "time" => {
            if matches!(word(i).as_deref(), Some("with" | "without"))
                && word(i + 1).as_deref() == Some("time")
                && word(i + 2).as_deref() == Some("zone")
            {
                let with = word(i).as_deref() == Some("with");
                name = format!("{name}{}", if with { "tz" } else { "" });
                i += 3;
            }
        }
        _ => {}
    }
    // Optional (n[, m]) modifier.
    if tokens.get(i).is_some_and(|t| t.is_op("(")) {
        let close = find_close(tokens, i)?;
        i = close + 1;
    }
    let mut array = false;
    while tokens.get(i).is_some_and(|t| t.is_op("[")) {
        let close = tokens[i..].iter().position(|t| t.is_op("]"))?;
        i += close + 1;
        array = true;
    }
    if word(i).as_deref() == Some("array") {
        array = true;
        i += 1;
    }
    Some((name, array, i))
}

/// OID for a cast / DDL type name; `None` for unknown types.
pub fn type_oid(name: &str) -> Option<i32> {
    type_map::oid_for_type_name(name)
}

/// Looks up a (qualified) column in the referenced tables.
pub fn resolve_column<'a>(
    qualifier: Option<&str>,
    name: &str,
    tables: &[TableRef],
    schema: &'a Schema,
) -> Option<&'a ColumnInfo> {
    for table in tables {
        if let Some(q) = qualifier
            && !table.alias.eq_ignore_ascii_case(q)
            && !table.name.eq_ignore_ascii_case(q)
        {
            continue;
        }
        if let Some(info) = schema.table(&table.name)
            && let Some(column) = info.column(name)
        {
            return Some(column);
        }
    }
    None
}

fn strip_parens(mut expr: &[Token]) -> &[Token] {
    while expr.len() >= 2
        && expr[0].is_op("(")
        && find_close(expr, 0) == Some(expr.len() - 1)
        && !expr[1].is_word("SELECT")
    {
        expr = &expr[1..expr.len() - 1];
    }
    expr
}

fn int_literal_oid(text: &str) -> i32 {
    match text.parse::<i64>() {
        Ok(n) if i32::try_from(n).is_ok() => oid::INT4,
        Ok(_) => oid::INT8,
        Err(_) => oid::NUMERIC,
    }
}

/// Statically determines the PostgreSQL type of an expression, if possible.
pub fn expr_type(expr: &[Token], tables: &[TableRef], schema: &Schema) -> Option<i32> {
    let expr = strip_parens(expr);
    if expr.is_empty() {
        return None;
    }
    // Trailing ::type cast.
    for (index, token) in expr.iter().enumerate().rev() {
        if token.is_op("::") {
            let (name, array, used) = parse_type_name(&expr[index + 1..])?;
            if index + 1 + used == expr.len() {
                return if array { None } else { type_oid(&name) };
            }
            break;
        }
        if token.is_op(")") {
            break; // casts inside parentheses do not apply to the whole item
        }
    }
    if expr[0].is_word("CAST")
        && expr.get(1).is_some_and(|t| t.is_op("("))
        && find_close(expr, 1) == Some(expr.len() - 1)
    {
        let as_at = expr.iter().position(|t| t.is_word("AS"))?;
        let (name, array, _) = parse_type_name(&expr[as_at + 1..expr.len() - 1])?;
        return if array { None } else { type_oid(&name) };
    }
    if expr[0].is_word("EXISTS")
        || (expr[0].is_word("NOT") && expr.get(1).is_some_and(|t| t.is_word("EXISTS")))
    {
        return Some(oid::BOOL);
    }
    if !expr[0].is_word("CASE") && has_boolean_operator(expr) {
        return Some(oid::BOOL);
    }
    match shape_of(expr) {
        ExprShape::Column(qualifier, name) => {
            resolve_column(qualifier.as_deref(), &name, tables, schema)
                .filter(|c| !c.decl.trim().is_empty())
                .map(|c| type_map::sqlite_type_to_pg_oid(&c.decl))
        }
        ExprShape::Call(name) => call_type(&name, expr, tables, schema),
        _ => {
            if expr.len() == 1 {
                let token = &expr[0];
                return match token.kind {
                    Kind::Num => Some(if token.text.contains(['.', 'e', 'E']) {
                        oid::NUMERIC
                    } else {
                        int_literal_oid(&token.text)
                    }),
                    Kind::Str => Some(oid::TEXT),
                    Kind::Word => match token.lower().as_str() {
                        "true" | "false" => Some(oid::BOOL),
                        "current_timestamp" | "localtimestamp" => Some(oid::TIMESTAMPTZ),
                        "current_date" => Some(oid::DATE),
                        _ => None,
                    },
                    _ => None,
                };
            }
            // -1 style signed literals.
            if expr.len() == 2 && expr[0].is_op("-") && expr[1].kind == Kind::Num {
                return Some(int_literal_oid(&expr[1].text));
            }
            // Concatenation always yields text.
            if has_top_level_op(expr, "||") {
                return Some(oid::TEXT);
            }
            None
        }
    }
}

fn has_top_level_op(expr: &[Token], op: &str) -> bool {
    let mut depth = 0;
    for token in expr {
        if token.is_op("(") {
            depth += 1;
        } else if token.is_op(")") {
            depth -= 1;
        } else if depth == 0 && token.is_op(op) {
            return true;
        }
    }
    false
}

fn has_boolean_operator(expr: &[Token]) -> bool {
    let mut depth = 0;
    for token in expr {
        if token.is_op("(") {
            depth += 1;
        } else if token.is_op(")") {
            depth -= 1;
        } else if depth == 0 {
            if matches!(token.kind, Kind::Op)
                && matches!(
                    token.text.as_str(),
                    "=" | "<"
                        | ">"
                        | "<="
                        | ">="
                        | "<>"
                        | "!="
                        | "~~"
                        | "!~~"
                        | "~~*"
                        | "@>"
                        | "<@"
                        | "&&"
                )
            {
                return true;
            }
            if token.is_any_word(&[
                "AND", "OR", "IS", "LIKE", "ILIKE", "IN", "BETWEEN", "ISNULL", "NOTNULL",
            ]) {
                return true;
            }
        }
    }
    false
}

fn call_type(name: &str, expr: &[Token], tables: &[TableRef], schema: &Schema) -> Option<i32> {
    let close = find_close(expr, 1).or_else(|| find_close(expr, 3))?;
    let open = if expr.get(1).is_some_and(|t| t.is_op("(")) {
        1
    } else {
        3
    };
    let args = if close > open + 1 {
        split_commas(&expr[open + 1..close])
    } else {
        Vec::new()
    };
    let arg_type = |index: usize| {
        args.get(index)
            .and_then(|arg| expr_type(arg, tables, schema))
    };
    Some(match name {
        "count" | "row_number" | "rank" | "dense_rank" | "ntile" | "count_big" => oid::INT8,
        "sum" | "total" => match arg_type(0)? {
            oid::INT2 | oid::INT4 => oid::INT8,
            oid::INT8 => oid::NUMERIC,
            other => other,
        },
        "avg" => match arg_type(0) {
            Some(oid::FLOAT4 | oid::FLOAT8) => oid::FLOAT8,
            _ => oid::NUMERIC,
        },
        "min" | "max" | "abs" | "round" | "floor" | "ceil" | "ceiling" | "trunc" | "nullif"
        | "lag" | "lead" | "first_value" | "last_value" => arg_type(0)?,
        "coalesce" | "ifnull" => (0..args.len()).find_map(arg_type)?,
        "length" | "char_length" | "character_length" | "octet_length" | "strpos" | "position"
        | "ascii" | "array_length" | "bit_length" | "sign" => oid::INT4,
        "lower" | "upper" | "trim" | "ltrim" | "rtrim" | "btrim" | "concat" | "concat_ws"
        | "substr" | "substring" | "replace" | "md5" | "to_char" | "format" | "initcap"
        | "left" | "right" | "lpad" | "rpad" | "repeat" | "reverse" | "version"
        | "current_database" | "current_schema" | "current_user" | "session_user"
        | "string_agg" | "group_concat" | "json_agg" | "jsonb_agg" | "typeof" => oid::TEXT,
        "now" | "statement_timestamp" | "transaction_timestamp" | "clock_timestamp" => {
            oid::TIMESTAMPTZ
        }
        "random" | "pow" | "power" | "sqrt" | "exp" | "ln" | "log" | "sin" | "cos" | "tan" => {
            oid::FLOAT8
        }
        "json_object" | "json_array" | "json_extract" | "json_build_object"
        | "jsonb_build_object" => oid::JSON,
        "bool_and" | "bool_or" | "every" | "exists" => oid::BOOL,
        "array_agg" => {
            let element = args
                .first()
                .and_then(|arg| {
                    let end = arg
                        .iter()
                        .position(|t| t.is_word("ORDER"))
                        .unwrap_or(arg.len());
                    expr_type(&arg[..end], tables, schema)
                })
                .unwrap_or(oid::TEXT);
            type_map::array_oid_of(element)
        }
        "pg_backend_pid" => oid::INT4,
        "gen_random_uuid" | "uuid_generate_v4" => oid::UUID,
        "date" => oid::DATE,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Parameter inference
// ---------------------------------------------------------------------------

/// Number of parameters of a statement (highest `$n`).
pub fn param_count(tokens: &[Token]) -> usize {
    tokens
        .iter()
        .filter(|t| t.kind == Kind::Param)
        .filter_map(|t| t.value.parse::<usize>().ok())
        .max()
        .unwrap_or(0)
}

fn column_before(
    tokens: &[Token],
    end: usize,
    tables: &[TableRef],
    schema: &Schema,
) -> Option<i32> {
    // tokens[end] is the last token of a (qualified) column reference.
    let name = tokens.get(end)?.ident()?;
    if tokens.get(end)?.kind == Kind::Word && tokens[end].is_any_word(NOT_ALIAS) {
        return None;
    }
    let qualifier = if end >= 2 && tokens[end - 1].is_op(".") {
        tokens[end - 2].ident()
    } else {
        None
    };
    resolve_column(qualifier.as_deref(), &name, tables, schema)
        .filter(|c| !c.decl.trim().is_empty())
        .map(|c| widen(type_map::sqlite_type_to_pg_oid(&c.decl)))
}

/// Integer parameters are widened: clients encode into the wider type happily
/// and SQLite integers are 64-bit.
fn widen(type_oid: i32) -> i32 {
    match type_oid {
        oid::INT2 | oid::INT4 => oid::INT8,
        other => other,
    }
}

/// Infers parameter type OIDs for a statement, honouring `declared` OIDs
/// (non-zero entries) given in the Parse message. Unresolvable parameters
/// stay `0` (unspecified).
pub fn infer_param_types(
    tokens: &[Token],
    tables: &[TableRef],
    schema: &Schema,
    declared: &[i32],
) -> Vec<i32> {
    let count = param_count(tokens).max(declared.len());
    let mut types: Vec<i32> = (0..count)
        .map(|i| declared.get(i).copied().unwrap_or(0))
        .collect();

    // INSERT INTO t (c1, c2) VALUES ($1, $2), ($3, $4)
    let insert_columns = insert_target(tokens);
    for (i, token) in tokens.iter().enumerate() {
        if token.kind != Kind::Param {
            continue;
        }
        let Ok(n) = token.value.parse::<usize>() else {
            continue;
        };
        if n == 0 || n > count || types[n - 1] != 0 {
            continue;
        }
        let mut found = None;
        // $n::type
        if tokens.get(i + 1).is_some_and(|t| t.is_op("::"))
            && let Some((name, array, _)) = parse_type_name(&tokens[i + 2..])
            && !array
        {
            found = type_oid(&name);
        }
        let prev = i.checked_sub(1).and_then(|p| tokens.get(p));
        if found.is_none()
            && let Some(prev) = prev
        {
            if prev.is_any_word(&["LIMIT", "OFFSET", "FIRST", "NEXT"]) {
                found = Some(oid::INT8);
            } else if (prev.kind == Kind::Op
                && matches!(
                    prev.text.as_str(),
                    "=" | "<>" | "!=" | "<" | ">" | "<=" | ">=" | "~~" | "!~~" | "~~*"
                ))
                || prev.is_any_word(&["LIKE", "ILIKE"])
            {
                if i >= 2 {
                    found = column_before(tokens, i - 2, tables, schema);
                }
                // $n = column
                if found.is_none()
                    && let Some(next) = tokens.get(i + 1)
                    && next.kind == Kind::Op
                {
                    // handled below by the reversed form
                }
            } else if prev.is_word("BETWEEN") && i >= 2 {
                found = column_before(tokens, i - 2, tables, schema);
            } else if prev.is_word("AND") && i >= 4 && tokens[i - 3].is_word("BETWEEN") {
                found = column_before(tokens, i - 4, tables, schema);
            }
        }
        // $n op column
        if found.is_none()
            && let Some(next) = tokens.get(i + 1)
            && next.kind == Kind::Op
            && matches!(
                next.text.as_str(),
                "=" | "<>" | "!=" | "<" | ">" | "<=" | ">="
            )
            && let Some(col) = tokens.get(i + 2)
            && col.ident().is_some()
        {
            let end = if tokens.get(i + 3).is_some_and(|t| t.is_op(".")) {
                i + 4
            } else {
                i + 2
            };
            if tokens.get(end).is_some() {
                found = column_before(tokens, end, tables, schema);
            }
        }
        // col IN ($1, $2)
        if found.is_none() {
            found = in_list_type(tokens, i, tables, schema);
        }
        // INSERT column position
        if found.is_none()
            && let Some((table, columns, values_start)) = &insert_columns
        {
            found = insert_param_type(tokens, i, table, columns, *values_start, schema);
        }
        if let Some(found) = found {
            types[n - 1] = found;
        }
    }
    types
}

fn in_list_type(
    tokens: &[Token],
    index: usize,
    tables: &[TableRef],
    schema: &Schema,
) -> Option<i32> {
    // Walk back to the enclosing "(" of a parameter list preceded by IN.
    let mut depth = 0_i32;
    let mut j = index;
    while j > 0 {
        j -= 1;
        let t = &tokens[j];
        if t.is_op(")") {
            depth += 1;
        } else if t.is_op("(") {
            if depth == 0 {
                if j >= 2 && tokens[j - 1].is_word("IN") {
                    return column_before(tokens, j - 2, tables, schema);
                }
                return None;
            }
            depth -= 1;
        } else if depth == 0 && !(t.is_op(",") || t.kind == Kind::Param) {
            return None;
        }
    }
    None
}

/// For `INSERT INTO t (cols) VALUES ...` returns (table, columns, index of VALUES).
pub fn insert_target(tokens: &[Token]) -> Option<(String, Vec<String>, usize)> {
    if !(tokens.first()?.is_word("INSERT") && tokens.get(1)?.is_word("INTO")) {
        return None;
    }
    let mut i = 2;
    let mut name = tokens.get(i)?.ident()?;
    i += 1;
    while tokens.get(i).is_some_and(|t| t.is_op(".")) {
        name = tokens.get(i + 1)?.ident()?;
        i += 2;
    }
    if tokens.get(i).is_some_and(|t| t.is_word("AS")) {
        i += 2;
    }
    let mut columns = Vec::new();
    if tokens.get(i)?.is_op("(") {
        let close = find_close(tokens, i)?;
        for part in split_commas(&tokens[i + 1..close]) {
            columns.push(part.first()?.ident()?);
        }
        i = close + 1;
    }
    if tokens.get(i)?.is_word("VALUES") {
        return Some((name, columns, i));
    }
    None
}

fn insert_param_type(
    tokens: &[Token],
    index: usize,
    table: &str,
    columns: &[String],
    values_start: usize,
    schema: &Schema,
) -> Option<i32> {
    if index <= values_start {
        return None;
    }
    // Find the row group containing `index`.
    let mut depth = 0_i32;
    let mut row_start = None;
    for (j, token) in tokens.iter().enumerate().take(index).skip(values_start + 1) {
        if token.is_op("(") {
            if depth == 0 {
                row_start = Some(j);
            }
            depth += 1;
        } else if token.is_op(")") {
            depth -= 1;
        }
    }
    if depth != 1 {
        return None;
    }
    let row_start = row_start?;
    let close = find_close(tokens, row_start)?;
    let items = split_commas(&tokens[row_start + 1..close]);
    let position = items
        .iter()
        .position(|item| item.len() == 1 && std::ptr::eq(&item[0], &tokens[index]))?;
    let info = schema.table(table)?;
    let column = if columns.is_empty() {
        info.columns.get(position)?
    } else {
        info.column(columns.get(position)?)?
    };
    (!column.decl.trim().is_empty()).then(|| widen(type_map::sqlite_type_to_pg_oid(&column.decl)))
}

/// Identifier names mentioned anywhere in a statement (candidate tables).
pub fn candidate_names(tokens: &[Token]) -> Vec<String> {
    let mut names: Vec<String> = tokens
        .iter()
        .filter(|t| matches!(t.kind, Kind::Word | Kind::QIdent))
        .filter_map(Token::ident)
        .collect();
    names.sort();
    names.dedup();
    names.truncate(256);
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::tokenize;

    fn schema() -> Schema {
        let mut schema = Schema::default();
        schema.tables.insert(
            "users".into(),
            Some(TableInfo {
                columns: vec![
                    ColumnInfo {
                        name: "id".into(),
                        decl: "INTEGER".into(),

                        default: None,
                        pk: 1,
                    },
                    ColumnInfo {
                        name: "name".into(),
                        decl: "TEXT".into(),

                        default: None,
                        pk: 0,
                    },
                    ColumnInfo {
                        name: "active".into(),
                        decl: "BOOLEAN".into(),

                        default: None,
                        pk: 0,
                    },
                ],
            }),
        );
        schema
    }

    #[test]
    fn select_parts_finds_items_and_tables() {
        let tokens = tokenize("SELECT u.id, count(*) AS n FROM public.users u LEFT JOIN posts p ON p.user_id = u.id WHERE x = 1").unwrap();
        let parts = select_parts(&tokens).unwrap();
        let items = parse_items(&tokens[parts.items_start..parts.items_end]);
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].alias.as_deref(), Some("n"));
        assert_eq!(
            items[0].shape(),
            ExprShape::Column(Some("u".into()), "id".into())
        );
        assert_eq!(
            parts.tables,
            vec![
                TableRef {
                    name: "users".into(),
                    alias: "u".into()
                },
                TableRef {
                    name: "posts".into(),
                    alias: "p".into()
                }
            ]
        );
    }

    #[test]
    fn expression_types() {
        let schema = schema();
        let tokens = tokenize("SELECT id, active, count(*), sum(id), 1, 1.5, 'x', now(), id = 1, x::bigint FROM users").unwrap();
        let parts = select_parts(&tokens).unwrap();
        let items = parse_items(&tokens[parts.items_start..parts.items_end]);
        let types: Vec<Option<i32>> = items
            .iter()
            .map(|item| expr_type(&item.expr, &parts.tables, &schema))
            .collect();
        assert_eq!(
            types,
            vec![
                Some(oid::INT4),
                Some(oid::BOOL),
                Some(oid::INT8),
                Some(oid::INT8),
                Some(oid::INT4),
                Some(oid::NUMERIC),
                Some(oid::TEXT),
                Some(oid::TIMESTAMPTZ),
                Some(oid::BOOL),
                Some(oid::INT8),
            ]
        );
    }

    #[test]
    fn infers_parameter_types() {
        let schema = schema();
        let tokens =
            tokenize("SELECT * FROM users WHERE id = $1 AND name LIKE $2 LIMIT $3 OFFSET $4::int")
                .unwrap();
        let parts = select_parts(&tokens).unwrap();
        let types = infer_param_types(&tokens, &parts.tables, &schema, &[]);
        assert_eq!(types, vec![oid::INT8, oid::TEXT, oid::INT8, oid::INT4]);

        let tokens =
            tokenize("INSERT INTO users (name, active) VALUES ($1, $2), ($3, $4)").unwrap();
        let types = infer_param_types(&tokens, &[], &schema, &[]);
        assert_eq!(types, vec![oid::TEXT, oid::BOOL, oid::TEXT, oid::BOOL]);

        let tokens = tokenize("SELECT 1 WHERE $1 = id").unwrap();
        let tables = vec![TableRef {
            name: "users".into(),
            alias: "users".into(),
        }];
        assert_eq!(
            infer_param_types(&tokens, &tables, &schema, &[]),
            vec![oid::INT8]
        );
        // Declared OIDs always win.
        assert_eq!(
            infer_param_types(&tokens, &tables, &schema, &[25]),
            vec![25]
        );
    }

    #[test]
    fn with_clauses_are_skipped() {
        let tokens = tokenize("WITH a AS (SELECT 1), b(x) AS (SELECT 2) SELECT x FROM b").unwrap();
        let parts = select_parts(&tokens).unwrap();
        assert_eq!(parts.tables[0].name, "b");
    }
}
