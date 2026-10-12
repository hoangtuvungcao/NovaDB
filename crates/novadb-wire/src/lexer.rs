//! PostgreSQL-flavoured SQL tokenizer.
//!
//! Every rewrite performed by the gateway operates on the token stream
//! produced here, never on raw SQL text, so string literals, dollar-quoted
//! bodies, quoted identifiers and comments are never touched by a rewrite.
//! Literals are decoded with PostgreSQL's rules (`''` doubling, `E'..'`
//! backslash escapes, `$tag$..$tag$`) and re-emitted as standard SQL literals.

use std::fmt::Write as _;

/// Error raised for SQL that cannot be tokenized or translated.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SqlError {
    /// Malformed SQL (SQLSTATE 42601).
    #[error("{0}")]
    Syntax(String),
    /// Valid PostgreSQL that NovaDB cannot express (SQLSTATE 0A000).
    #[error("{0}")]
    Unsupported(String),
    /// A parameter value that cannot be converted (SQLSTATE 22P02).
    #[error("{0}")]
    InvalidValue(String),
}

pub type SqlResult<T> = Result<T, SqlError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Bare identifier or keyword.
    Word,
    /// `"quoted identifier"`; `value` holds the unescaped name.
    QIdent,
    /// String literal; `value` holds the decoded content.
    Str,
    /// Numeric literal.
    Num,
    /// `$n` parameter placeholder; `value` holds `n`.
    Param,
    /// Operator or punctuation.
    Op,
    /// Pre-rendered SQLite SQL produced by a rewrite.
    Raw,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: Kind,
    /// Source spelling (or rendered SQL for `Raw`).
    pub text: String,
    /// Decoded value for strings, quoted identifiers and parameters.
    pub value: String,
    /// Whitespace preceding the token (comments collapse to one space).
    pub ws: String,
}

impl Token {
    pub fn new(kind: Kind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
            value: String::new(),
            ws: String::new(),
        }
    }

    pub fn raw(text: impl Into<String>) -> Self {
        Self::new(Kind::Raw, text).with_ws(" ")
    }

    pub fn word(text: &str) -> Self {
        Self::new(Kind::Word, text).with_ws(" ")
    }

    pub fn op(text: &str) -> Self {
        Self::new(Kind::Op, text)
    }

    pub fn string(value: &str) -> Self {
        let mut token = Self::new(Kind::Str, quote_str(value));
        token.value = value.to_owned();
        token.with_ws(" ")
    }

    #[must_use]
    pub fn with_ws(mut self, ws: &str) -> Self {
        self.ws = ws.into();
        self
    }

    pub fn is_word(&self, keyword: &str) -> bool {
        self.kind == Kind::Word && self.text.eq_ignore_ascii_case(keyword)
    }

    pub fn is_any_word(&self, keywords: &[&str]) -> bool {
        self.kind == Kind::Word && keywords.iter().any(|k| self.text.eq_ignore_ascii_case(k))
    }

    pub fn is_op(&self, op: &str) -> bool {
        self.kind == Kind::Op && self.text == op
    }

    /// Lowercase spelling of a bare word, empty otherwise.
    pub fn lower(&self) -> String {
        if self.kind == Kind::Word {
            self.text.to_ascii_lowercase()
        } else {
            String::new()
        }
    }

    /// Identifier name for bare or double-quoted identifiers (bare names
    /// are folded to lowercase as PostgreSQL does).
    pub fn ident(&self) -> Option<String> {
        match self.kind {
            Kind::Word => Some(self.text.to_ascii_lowercase()),
            Kind::QIdent => Some(self.value.clone()),
            _ => None,
        }
    }

    /// Renders the token as SQLite SQL (without leading whitespace).
    pub fn render(&self) -> String {
        match self.kind {
            Kind::QIdent => quote_ident(&self.value),
            Kind::Str => quote_str(&self.value),
            Kind::Param => "NULL".into(),
            Kind::Word | Kind::Num | Kind::Op | Kind::Raw => self.text.clone(),
        }
    }
}

/// Quotes an identifier for SQLite (`"` doubled).
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Renders text as a standard SQL string literal. SQLite does not treat
/// backslash specially, so only `'` needs escaping; NUL bytes (which cannot
/// appear inside a SQLite literal) are expressed through a hex BLOB cast.
pub fn quote_str(value: &str) -> String {
    if value.contains('\0') {
        format!("CAST({} AS TEXT)", hex_literal(value.as_bytes()))
    } else {
        format!("'{}'", value.replace('\'', "''"))
    }
}

/// Renders bytes as a SQLite BLOB literal.
pub fn hex_literal(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(bytes.len() * 2 + 3);
    hex.push_str("X'");
    for byte in bytes {
        let _ = write!(hex, "{byte:02X}");
    }
    hex.push('\'');
    hex
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c >= 0x80
}

fn is_ident_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Operators recognised as single tokens, longest first.
const OPERATORS: &[&str] = &[
    "!~~*", "->>", "#>>", "!~*", "~~*", "!~~", "::", "<=", ">=", "<>", "!=", "||", "->", "#>",
    "@>", "<@", "!~", "~*", "~~", ":=", "=>", "<<", ">>", "&&", "==",
];

/// Tokenizes PostgreSQL SQL. Comments are dropped (collapsed into whitespace).
pub fn tokenize(sql: &str) -> SqlResult<Vec<Token>> {
    let bytes = sql.as_bytes();
    let n = bytes.len();
    let mut tokens: Vec<Token> = Vec::new();
    let mut ws = String::new();
    let mut i = 0;

    macro_rules! push {
        ($kind:expr, $text:expr, $value:expr) => {{
            let mut token = Token::new($kind, $text);
            token.value = $value;
            token.ws = std::mem::take(&mut ws);
            tokens.push(token);
        }};
    }

    while i < n {
        let c = bytes[i];
        let next = bytes.get(i + 1).copied();
        if c.is_ascii_whitespace() || c == 0x0b {
            ws.push(c as char);
            i += 1;
            continue;
        }
        if c == b'-' && next == Some(b'-') {
            while i < n && bytes[i] != b'\n' {
                i += 1;
            }
            ws.push(' ');
            continue;
        }
        if c == b'/' && next == Some(b'*') {
            // Block comments nest in PostgreSQL.
            let mut depth = 1;
            let mut j = i + 2;
            while j < n && depth > 0 {
                if bytes[j] == b'/' && bytes.get(j + 1) == Some(&b'*') {
                    depth += 1;
                    j += 2;
                } else if bytes[j] == b'*' && bytes.get(j + 1) == Some(&b'/') {
                    depth -= 1;
                    j += 2;
                } else {
                    j += 1;
                }
            }
            if depth > 0 {
                return Err(SqlError::Syntax("unterminated /* comment".into()));
            }
            i = j;
            ws.push(' ');
            continue;
        }
        if c == b'\'' {
            let (value, end) = read_quoted(bytes, i, false)?;
            push!(Kind::Str, text(&bytes[i..end]), value);
            i = end;
            continue;
        }
        if c == b'"' {
            let (value, end) = read_quoted(bytes, i, false)?;
            if value.is_empty() {
                return Err(SqlError::Syntax("zero-length delimited identifier".into()));
            }
            push!(Kind::QIdent, text(&bytes[i..end]), value);
            i = end;
            continue;
        }
        if c == b'$' {
            if next.is_some_and(|d| d.is_ascii_digit()) {
                let mut j = i + 1;
                while j < n && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                push!(Kind::Param, text(&bytes[i..j]), text(&bytes[i + 1..j]));
                i = j;
                continue;
            }
            // Dollar-quoted string: $tag$ ... $tag$
            let mut j = i + 1;
            if j < n && is_ident_start(bytes[j]) && bytes[j] < 0x80 {
                while j < n && is_ident_byte(bytes[j]) && bytes[j] != b'$' {
                    j += 1;
                }
            }
            if bytes.get(j) == Some(&b'$') {
                let tag = &bytes[i..=j];
                let body_start = j + 1;
                let close = find_subslice(&bytes[body_start..], tag)
                    .ok_or_else(|| SqlError::Syntax("unterminated dollar-quoted string".into()))?;
                let value = text(&bytes[body_start..body_start + close]);
                let end = body_start + close + tag.len();
                push!(Kind::Str, text(&bytes[i..end]), value);
                i = end;
                continue;
            }
            return Err(SqlError::Syntax("syntax error at or near \"$\"".into()));
        }
        if c.is_ascii_digit() || (c == b'.' && next.is_some_and(|d| d.is_ascii_digit())) {
            let prev_is_name = tokens.last().is_some_and(|t: &Token| {
                ws.is_empty() && c == b'.' && matches!(t.kind, Kind::Word | Kind::QIdent)
            });
            if !prev_is_name {
                let end = read_number(bytes, i);
                push!(Kind::Num, text(&bytes[i..end]), String::new());
                i = end;
                continue;
            }
        }
        if is_ident_start(c) {
            let mut j = i;
            while j < n && is_ident_byte(bytes[j]) {
                j += 1;
            }
            // Prefixed literals: E'..', X'..', B'..', N'..'
            if j - i == 1 && bytes.get(j) == Some(&b'\'') {
                match c {
                    b'e' | b'E' => {
                        let (value, end) = read_quoted(bytes, j, true)?;
                        push!(Kind::Str, text(&bytes[i..end]), value);
                        i = end;
                        continue;
                    }
                    b'n' | b'N' => {
                        let (value, end) = read_quoted(bytes, j, false)?;
                        push!(Kind::Str, text(&bytes[i..end]), value);
                        i = end;
                        continue;
                    }
                    b'x' | b'X' => {
                        let (value, end) = read_quoted(bytes, j, false)?;
                        if value.len() % 2 != 0 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
                            return Err(SqlError::Syntax("invalid hexadecimal literal".into()));
                        }
                        push!(Kind::Raw, format!("X'{value}'"), String::new());
                        i = end;
                        continue;
                    }
                    b'b' | b'B' => {
                        let (value, end) = read_quoted(bytes, j, false)?;
                        if value.is_empty()
                            || value.len() > 64
                            || !value.bytes().all(|b| b == b'0' || b == b'1')
                        {
                            return Err(SqlError::Syntax("invalid binary literal".into()));
                        }
                        let number = u64::from_str_radix(&value, 2).unwrap_or(0);
                        push!(Kind::Num, number.to_string(), String::new());
                        i = end;
                        continue;
                    }
                    _ => {}
                }
            }
            push!(Kind::Word, text(&bytes[i..j]), String::new());
            i = j;
            continue;
        }
        let rest = &sql[i..];
        let op = OPERATORS
            .iter()
            .find(|op| rest.starts_with(**op))
            .map_or_else(
                || rest.chars().next().map(String::from).unwrap_or_default(),
                |op| (*op).to_owned(),
            );
        i += op.len();
        push!(Kind::Op, op, String::new());
    }
    Ok(tokens)
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn read_number(sql: &[u8], start: usize) -> usize {
    let n = sql.len();
    let mut j = start;
    while j < n && sql[j].is_ascii_digit() {
        j += 1;
    }
    if j < n && sql[j] == b'.' && sql.get(j + 1) != Some(&b'.') {
        j += 1;
        while j < n && sql[j].is_ascii_digit() {
            j += 1;
        }
    }
    if j < n && (sql[j] == b'e' || sql[j] == b'E') {
        let mut k = j + 1;
        if k < n && (sql[k] == b'+' || sql[k] == b'-') {
            k += 1;
        }
        if k < n && sql[k].is_ascii_digit() {
            while k < n && sql[k].is_ascii_digit() {
                k += 1;
            }
            j = k;
        }
    }
    j
}

/// Reads a `'..'` string or `".."` identifier starting at the opening quote.
/// With `escapes`, backslash sequences are decoded (`E'..'`).
fn read_quoted(sql: &[u8], start: usize, escapes: bool) -> SqlResult<(String, usize)> {
    let quote = sql[start];
    let mut value: Vec<u8> = Vec::new();
    let mut i = start + 1;
    while i < sql.len() {
        let c = sql[i];
        if escapes && c == b'\\' {
            let Some(&escaped) = sql.get(i + 1) else {
                break;
            };
            i += 2;
            match escaped {
                b'b' => value.push(0x08),
                b'f' => value.push(0x0c),
                b'n' => value.push(b'\n'),
                b'r' => value.push(b'\r'),
                b't' => value.push(b'\t'),
                b'0'..=b'7' => {
                    let mut code = u32::from(escaped - b'0');
                    let mut digits = 1;
                    while digits < 3 && i < sql.len() && (b'0'..=b'7').contains(&sql[i]) {
                        code = code * 8 + u32::from(sql[i] - b'0');
                        i += 1;
                        digits += 1;
                    }
                    value.push((code & 0xff) as u8);
                }
                b'x' => {
                    let mut code = 0_u32;
                    let mut digits = 0;
                    while digits < 2 && i < sql.len() && sql[i].is_ascii_hexdigit() {
                        code = code * 16 + (sql[i] as char).to_digit(16).unwrap_or(0);
                        i += 1;
                        digits += 1;
                    }
                    if digits == 0 {
                        value.push(b'x');
                    } else {
                        value.push(code as u8);
                    }
                }
                b'u' | b'U' => {
                    let width = if escaped == b'u' { 4 } else { 8 };
                    let hex = sql.get(i..i + width).map(text).unwrap_or_default();
                    match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                        Some(ch) if hex.len() == width => {
                            let mut buf = [0_u8; 4];
                            value.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                            i += width;
                        }
                        _ => return Err(SqlError::Syntax("invalid Unicode escape".into())),
                    }
                }
                other => value.push(other),
            }
            continue;
        }
        if c == quote {
            if sql.get(i + 1) == Some(&quote) {
                value.push(quote);
                i += 2;
                continue;
            }
            return Ok((text(&value), i + 1));
        }
        value.push(c);
        i += 1;
    }
    Err(SqlError::Syntax(if quote == b'"' {
        "unterminated quoted identifier".into()
    } else {
        "unterminated quoted string".into()
    }))
}

/// Joins tokens into SQLite SQL, inserting whitespace wherever adjacent
/// tokens could otherwise fuse (`a OR b`, `- -1` vs `--`, `/ *`, quotes).
pub fn render(tokens: &[Token]) -> String {
    let mut out = String::new();
    for token in tokens {
        let rendered = token.render();
        if let (Some(last), Some(first)) = (out.chars().last(), rendered.chars().next()) {
            let wordish = |c: char| {
                c.is_ascii_alphanumeric() || matches!(c, '_' | '$' | '\'' | '"') || !c.is_ascii()
            };
            let needs_space = (wordish(last) && wordish(first))
                || (last == '-' && first == '-')
                || (last == '/' && first == '*')
                || (last == '*' && first == '/');
            if !token.ws.is_empty() {
                out.push_str(&token.ws);
            } else if needs_space {
                out.push(' ');
            }
        } else if !out.is_empty() {
            out.push_str(&token.ws);
        }
        out.push_str(&rendered);
    }
    out
}

/// Splits a token stream into statements at top-level `;`. `BEGIN ... END`
/// blocks of `CREATE TRIGGER` bodies are kept together.
pub fn split_statements(tokens: Vec<Token>) -> Vec<Vec<Token>> {
    let mut statements = Vec::new();
    let mut current: Vec<Token> = Vec::new();
    let mut depth = 0_i32;
    let mut block = 0_i32;
    for token in tokens {
        if token.is_op(";") && depth <= 0 && block <= 0 {
            if !current.is_empty() {
                statements.push(std::mem::take(&mut current));
            }
            depth = 0;
            block = 0;
            continue;
        }
        if token.is_op("(") {
            depth += 1;
        } else if token.is_op(")") {
            depth -= 1;
        } else if token.kind == Kind::Word && is_trigger(&current) {
            if token.is_any_word(&["BEGIN", "CASE"]) {
                block += 1;
            } else if token.is_word("END") {
                block -= 1;
            }
        }
        current.push(token);
    }
    if !current.is_empty() {
        statements.push(current);
    }
    statements
}

fn is_trigger(statement: &[Token]) -> bool {
    statement.first().is_some_and(|t| t.is_word("CREATE"))
        && statement.iter().take(12).any(|t| t.is_word("TRIGGER"))
}

/// Lowercase leading keywords of a statement (stops at the first non-word).
pub fn leading_words(tokens: &[Token], count: usize) -> Vec<String> {
    tokens
        .iter()
        .take(count)
        .take_while(|t| t.kind == Kind::Word)
        .map(Token::lower)
        .collect()
}

/// Index of the `)` matching the `(` at `open`.
pub fn find_close(tokens: &[Token], open: usize) -> Option<usize> {
    let mut depth = 0_i32;
    for (index, token) in tokens.iter().enumerate().skip(open) {
        if token.is_op("(") {
            depth += 1;
        } else if token.is_op(")") {
            depth -= 1;
            if depth == 0 {
                return Some(index);
            }
        }
    }
    None
}

/// Index of the `(` matching the `)` at `close`.
pub fn find_open(tokens: &[Token], close: usize) -> Option<usize> {
    let mut depth = 0_i32;
    for index in (0..=close).rev() {
        if tokens[index].is_op(")") {
            depth += 1;
        } else if tokens[index].is_op("(") {
            depth -= 1;
            if depth == 0 {
                return Some(index);
            }
        }
    }
    None
}

/// Splits at top-level occurrences of `is_separator`.
pub fn split_top(tokens: &[Token], is_separator: impl Fn(&Token) -> bool) -> Vec<&[Token]> {
    let mut parts = Vec::new();
    let mut depth = 0_i32;
    let mut start = 0;
    for (index, token) in tokens.iter().enumerate() {
        if token.is_op("(") {
            depth += 1;
        } else if token.is_op(")") {
            depth -= 1;
        } else if depth == 0 && is_separator(token) {
            parts.push(&tokens[start..index]);
            start = index + 1;
        }
    }
    parts.push(&tokens[start..]);
    parts
}

/// Splits at top-level commas.
pub fn split_commas(tokens: &[Token]) -> Vec<&[Token]> {
    split_top(tokens, |t| t.is_op(","))
}

/// Index of the first top-level token matching `predicate` at or after `from`.
pub fn find_top(
    tokens: &[Token],
    from: usize,
    predicate: impl Fn(&Token) -> bool,
) -> Option<usize> {
    let mut depth = 0_i32;
    for (index, token) in tokens.iter().enumerate() {
        if index >= from && depth == 0 && predicate(token) {
            return Some(index);
        }
        if token.is_op("(") {
            depth += 1;
        } else if token.is_op(")") {
            depth -= 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sql(input: &str) -> String {
        render(&tokenize(input).unwrap())
    }

    #[test]
    fn literals_comments_and_identifiers_are_tokenized_not_rewritten() {
        let tokens =
            tokenize("SELECT 'a--b', \"x\"\"y\", $1 /* c /* nested */ */ -- tail").unwrap();
        let kinds: Vec<Kind> = tokens.iter().map(|t| t.kind).collect();
        assert_eq!(
            kinds,
            vec![
                Kind::Word,
                Kind::Str,
                Kind::Op,
                Kind::QIdent,
                Kind::Op,
                Kind::Param
            ]
        );
        assert_eq!(tokens[1].value, "a--b");
        assert_eq!(tokens[3].value, "x\"y");
    }

    #[test]
    fn escape_strings_and_dollar_quotes_become_standard_literals() {
        assert_eq!(sql(r"SELECT E'a\\\'b\n'"), "SELECT 'a\\''b\n'");
        assert_eq!(sql("SELECT $$it's$$, $f$a$$b$f$"), "SELECT 'it''s', 'a$$b'");
        assert_eq!(sql(r"SELECT E'\x41\101é'"), "SELECT 'AAé'");
        assert_eq!(sql("SELECT 'a\\b'"), "SELECT 'a\\b'");
    }

    #[test]
    fn operators_and_numbers() {
        let tokens = tokenize("a::int<=1.5e3 t.5").unwrap();
        let texts: Vec<&str> = tokens.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, ["a", "::", "int", "<=", "1.5e3", "t", ".", "5"]);
    }

    #[test]
    fn unterminated_input_is_an_error() {
        assert!(tokenize("SELECT 'abc").is_err());
        assert!(tokenize("SELECT /* x").is_err());
        assert!(tokenize("SELECT \"abc").is_err());
        assert!(tokenize("SELECT $x$abc").is_err());
    }

    #[test]
    fn statements_split_on_top_level_semicolons() {
        let parts = split_statements(tokenize("SELECT ';'; SELECT (1;2); ;SELECT 3").unwrap());
        assert_eq!(parts.len(), 2 + 1);
    }
}
