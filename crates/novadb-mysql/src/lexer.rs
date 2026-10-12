//! MySQL lexer.
//!
//! Every translation in this crate operates on the token stream produced
//! here, never on raw SQL text, so rewrites can never reach into string
//! literals, quoted identifiers or comments, and cannot match across quote
//! boundaries. String literals are decoded with MySQL's rules (backslash
//! escapes, doubled quotes, `"..."` strings) and re-emitted as standard SQL
//! literals in which only `'` needs escaping.

use std::fmt::Write as _;

/// Error raised for SQL that cannot be safely tokenized or translated.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SqlError {
    /// Malformed SQL (MySQL error 1064).
    #[error("{0}")]
    Syntax(String),
    /// Valid MySQL that NovaDB cannot express (MySQL error 1235).
    #[error("{0}")]
    Unsupported(String),
}

pub type SqlResult<T> = Result<T, SqlError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Bare identifier or keyword; `text` is the source spelling.
    Word,
    /// Backtick-quoted identifier; `value` is the unescaped name.
    QuotedIdent,
    /// String literal; `value` is the decoded content.
    Str,
    /// Hex string literal, already normalized to `X'..'`.
    HexStr,
    /// Numeric literal.
    Number,
    /// `?` parameter placeholder.
    Placeholder,
    /// `@name` user variable; `value` is the name.
    UserVar,
    /// `@@[scope.]name` system variable; `value` is the lowercase name.
    SysVar,
    /// Operator or punctuation.
    Op,
    /// A substituted literal value (already valid SQLite SQL).
    Literal,
    /// Pre-rendered SQLite SQL produced by a rewrite.
    Raw,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: Kind,
    /// Source text (or rendered SQL for `Literal` / `Raw`).
    pub text: String,
    /// Decoded value for strings, quoted identifiers and variables.
    pub value: String,
    /// Byte-exact contents of a string literal that is not valid UTF-8.
    pub bytes: Option<Vec<u8>>,
    /// Whitespace (comments collapse to one space) preceding the token.
    pub ws: String,
}

impl Token {
    pub fn raw(text: impl Into<String>) -> Self {
        Self {
            kind: Kind::Raw,
            text: text.into(),
            value: String::new(),
            bytes: None,
            ws: " ".into(),
        }
    }

    pub fn literal(text: impl Into<String>, ws: &str) -> Self {
        Self {
            kind: Kind::Literal,
            text: text.into(),
            value: String::new(),
            bytes: None,
            ws: ws.into(),
        }
    }

    pub fn op(text: &str) -> Self {
        Self {
            kind: Kind::Op,
            text: text.into(),
            value: String::new(),
            bytes: None,
            ws: String::new(),
        }
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

    pub fn upper(&self) -> String {
        if self.kind == Kind::Word {
            self.text.to_ascii_uppercase()
        } else {
            String::new()
        }
    }

    /// Identifier name for bare or backtick-quoted identifiers.
    pub fn ident(&self) -> Option<&str> {
        match self.kind {
            Kind::Word => Some(&self.text),
            Kind::QuotedIdent => Some(&self.value),
            _ => None,
        }
    }

    /// Renders the token as SQLite SQL (without leading whitespace).
    pub fn render(&self) -> String {
        match self.kind {
            Kind::Word => {
                if self.text.starts_with(|c: char| c.is_ascii_digit()) {
                    quote_ident(&self.text)
                } else {
                    self.text.clone()
                }
            }
            Kind::QuotedIdent => quote_ident(&self.value),
            Kind::Str => match &self.bytes {
                Some(bytes) => hex_literal(bytes),
                None => quote_str(&self.value),
            },
            // Variables and placeholders are always substituted before
            // rendering; NULL is a safe fallback.
            Kind::Placeholder | Kind::UserVar | Kind::SysVar => "NULL".into(),
            Kind::HexStr | Kind::Number | Kind::Op | Kind::Literal | Kind::Raw => self.text.clone(),
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
        let mut hex = String::with_capacity(value.len() * 2);
        for byte in value.bytes() {
            let _ = write!(hex, "{byte:02X}");
        }
        format!("CAST(X'{hex}' AS TEXT)")
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

fn is_ident_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80
}

fn utf8(bytes: &[u8]) -> SqlResult<String> {
    String::from_utf8(bytes.to_vec())
        .map_err(|_| SqlError::Syntax("invalid utf8mb4 character outside a string literal".into()))
}

/// Tokenizes MySQL SQL. Comments are dropped (collapsed into whitespace).
#[cfg(test)]
pub fn tokenize(sql: &str) -> SqlResult<Vec<Token>> {
    tokenize_bytes(sql.as_bytes())
}

/// Tokenizes MySQL SQL given as raw bytes. Only string literals may contain
/// bytes that are not valid UTF-8 (drivers send `_binary'...'` BLOB data
/// inline); such literals are kept byte-exact and rendered as `X'..'`.
pub fn tokenize_bytes(sql: &[u8]) -> SqlResult<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut ws = String::new();
    let mut i = 0;
    let n = sql.len();

    let push = |tokens: &mut Vec<Token>, ws: &mut String, kind, text: String, value: String| {
        tokens.push(Token {
            kind,
            text,
            value,
            bytes: None,
            ws: std::mem::take(ws),
        });
    };

    while i < n {
        let c = sql[i];
        let next = sql.get(i + 1).copied();
        // Whitespace.
        if c.is_ascii_whitespace() || c == 0x0b {
            ws.push(c as char);
            i += 1;
            continue;
        }
        // Comments: `#`, `-- ` (MySQL requires whitespace/control after `--`), `/* */`.
        if c == b'#'
            || (c == b'-'
                && next == Some(b'-')
                && sql
                    .get(i + 2)
                    .is_none_or(|c| c.is_ascii_whitespace() || c.is_ascii_control()))
        {
            while i < n && sql[i] != b'\n' {
                i += 1;
            }
            ws.push(' ');
            continue;
        }
        if c == b'/' && next == Some(b'*') {
            let mut j = i + 2;
            loop {
                if j + 1 >= n {
                    return Err(SqlError::Syntax("unterminated /* comment".into()));
                }
                if sql[j] == b'*' && sql[j + 1] == b'/' {
                    break;
                }
                j += 1;
            }
            i = j + 2;
            ws.push(' ');
            continue;
        }
        // String literals.
        if c == b'\'' || c == b'"' {
            let (value, end) = read_string(sql, i)?;
            tokens.push(string_token(&sql[i..end], value, std::mem::take(&mut ws)));
            i = end;
            continue;
        }
        // Quoted identifiers.
        if c == b'`' {
            let (value, end) = read_quoted(sql, i, b'`')?;
            push(
                &mut tokens,
                &mut ws,
                Kind::QuotedIdent,
                utf8(&sql[i..end])?,
                value,
            );
            i = end;
            continue;
        }
        if c == b'?' {
            push(
                &mut tokens,
                &mut ws,
                Kind::Placeholder,
                "?".into(),
                String::new(),
            );
            i += 1;
            continue;
        }
        // Variables.
        if c == b'@' {
            if next == Some(b'@') {
                let mut j = i + 2;
                while j < n && (is_ident_byte(sql[j]) || sql[j] == b'.') {
                    j += 1;
                }
                let raw = utf8(&sql[i + 2..j])?;
                if raw.is_empty() {
                    return Err(SqlError::Syntax("invalid system variable".into()));
                }
                let lower = raw.to_ascii_lowercase();
                let name = ["session.", "global.", "local.", "persist."]
                    .iter()
                    .find_map(|scope| lower.strip_prefix(scope))
                    .unwrap_or(&lower)
                    .to_owned();
                push(&mut tokens, &mut ws, Kind::SysVar, utf8(&sql[i..j])?, name);
                i = j;
                continue;
            }
            let (name, end) = match next {
                Some(b'\'' | b'"') => {
                    let (value, end) = read_string(sql, i + 1)?;
                    (utf8(&value)?, end)
                }
                Some(b'`') => read_quoted(sql, i + 1, b'`')?,
                _ => {
                    let mut j = i + 1;
                    while j < n && (is_ident_byte(sql[j]) || sql[j] == b'.') {
                        j += 1;
                    }
                    (utf8(&sql[i + 1..j])?, j)
                }
            };
            if name.is_empty() {
                return Err(SqlError::Syntax("invalid user variable".into()));
            }
            push(
                &mut tokens,
                &mut ws,
                Kind::UserVar,
                utf8(&sql[i..end])?,
                name.to_ascii_lowercase(),
            );
            i = end;
            continue;
        }
        // Numbers (including `.5`, but not `t.5` qualified names).
        let prev_is_name = tokens.last().is_some_and(|t: &Token| {
            ws.is_empty() && (matches!(t.kind, Kind::Word | Kind::QuotedIdent) || t.is_op(")"))
        });
        if c.is_ascii_digit()
            || (c == b'.' && next.is_some_and(|d| d.is_ascii_digit()) && !prev_is_name)
        {
            let (kind, text, end) = read_number(sql, i)?;
            push(&mut tokens, &mut ws, kind, text, String::new());
            i = end;
            continue;
        }
        // Words and prefixed literals (X'..', B'..', N'..').
        if is_ident_byte(c) {
            let mut j = i;
            while j < n && is_ident_byte(sql[j]) {
                j += 1;
            }
            if j - i == 1 && sql.get(j) == Some(&b'\'') {
                match c {
                    b'x' | b'X' => {
                        let (value, end) = read_string(sql, j)?;
                        if value.len() % 2 != 0 || !value.iter().all(u8::is_ascii_hexdigit) {
                            return Err(SqlError::Syntax("invalid hexadecimal literal".into()));
                        }
                        let text = format!("X'{}'", utf8(&value)?);
                        push(&mut tokens, &mut ws, Kind::HexStr, text, String::new());
                        i = end;
                        continue;
                    }
                    b'b' | b'B' => {
                        let (value, end) = read_string(sql, j)?;
                        let number = parse_bits(&utf8(&value)?)?;
                        push(
                            &mut tokens,
                            &mut ws,
                            Kind::Number,
                            number.to_string(),
                            String::new(),
                        );
                        i = end;
                        continue;
                    }
                    b'n' | b'N' => {
                        let (value, end) = read_string(sql, j)?;
                        tokens.push(string_token(&sql[i..end], value, std::mem::take(&mut ws)));
                        i = end;
                        continue;
                    }
                    _ => {}
                }
            }
            push(
                &mut tokens,
                &mut ws,
                Kind::Word,
                utf8(&sql[i..j])?,
                String::new(),
            );
            i = j;
            continue;
        }
        // Operators.
        let three = &sql[i..(i + 3).min(n)];
        let two = &sql[i..(i + 2).min(n)];
        let op: &[u8] = if three == b"<=>" || three == b"->>" {
            three
        } else if matches!(
            two,
            b"<=" | b">=" | b"<>" | b"!=" | b"||" | b"&&" | b":=" | b"<<" | b">>" | b"->" | b"=="
        ) {
            two
        } else {
            &sql[i..=i]
        };
        i += op.len();
        push(&mut tokens, &mut ws, Kind::Op, utf8(op)?, String::new());
    }
    Ok(tokens)
}

/// Builds a string-literal token; non-UTF-8 contents are kept as bytes.
fn string_token(source: &[u8], value: Vec<u8>, ws: String) -> Token {
    let text = String::from_utf8_lossy(source).into_owned();
    match String::from_utf8(value) {
        Ok(value) => Token {
            kind: Kind::Str,
            text,
            value,
            bytes: None,
            ws,
        },
        Err(error) => {
            let bytes = error.into_bytes();
            Token {
                kind: Kind::Str,
                text,
                value: String::from_utf8_lossy(&bytes).into_owned(),
                bytes: Some(bytes),
                ws,
            }
        }
    }
}

fn parse_bits(bits: &str) -> SqlResult<u64> {
    if bits.len() > 64 || !bits.bytes().all(|b| b == b'0' || b == b'1') {
        return Err(SqlError::Syntax("invalid bit literal".into()));
    }
    Ok(bits
        .bytes()
        .fold(0_u64, |acc, b| (acc << 1) | u64::from(b - b'0')))
}

fn read_number(sql: &[u8], start: usize) -> SqlResult<(Kind, String, usize)> {
    let n = sql.len();
    let mut j = start;
    // 0x.. hexadecimal and 0b.. binary numbers.
    if sql[start] == b'0' && matches!(sql.get(start + 1), Some(b'x' | b'X' | b'b' | b'B')) {
        let radix = sql[start + 1].to_ascii_lowercase();
        let mut k = start + 2;
        while k < n && sql[k].is_ascii_alphanumeric() {
            k += 1;
        }
        let digits = &sql[start + 2..k];
        let valid = !digits.is_empty()
            && if radix == b'x' {
                digits.iter().all(u8::is_ascii_hexdigit)
            } else {
                digits.iter().all(|c| *c == b'0' || *c == b'1')
            };
        if valid && !sql.get(k).is_some_and(|c| is_ident_byte(*c)) {
            let digits = utf8(digits)?;
            let text = if radix == b'x' {
                format!("0x{digits}")
            } else {
                parse_bits(&digits).map_or_else(|_| format!("0b{digits}"), |v| v.to_string())
            };
            return Ok((Kind::Number, text, k));
        }
    }
    while j < n && sql[j].is_ascii_digit() {
        j += 1;
    }
    if j < n && sql[j] == b'.' {
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
    // MySQL allows identifiers that start with digits (`1abc`).
    if j < n && is_ident_byte(sql[j]) && sql[start].is_ascii_digit() {
        let mut k = j;
        while k < n && is_ident_byte(sql[k]) {
            k += 1;
        }
        return Ok((Kind::Word, utf8(&sql[start..k])?, k));
    }
    Ok((Kind::Number, utf8(&sql[start..j])?, j))
}

/// Reads a MySQL string literal starting at the opening quote. Returns the
/// decoded bytes and the index after the closing quote.
fn read_string(sql: &[u8], start: usize) -> SqlResult<(Vec<u8>, usize)> {
    let quote = sql[start];
    let mut value = Vec::new();
    let mut i = start + 1;
    while i < sql.len() {
        let c = sql[i];
        if c == b'\\' {
            let Some(&escaped) = sql.get(i + 1) else {
                break;
            };
            match escaped {
                b'0' => value.push(0),
                b'b' => value.push(0x08),
                b'n' => value.push(b'\n'),
                b'r' => value.push(b'\r'),
                b't' => value.push(b'\t'),
                b'Z' => value.push(0x1a),
                // `\%` and `\_` keep the backslash (LIKE escapes).
                b'%' | b'_' => {
                    value.push(b'\\');
                    value.push(escaped);
                }
                other => value.push(other),
            }
            i += 2;
            continue;
        }
        if c == quote {
            if sql.get(i + 1) == Some(&quote) {
                value.push(quote);
                i += 2;
                continue;
            }
            return Ok((value, i + 1));
        }
        value.push(c);
        i += 1;
    }
    Err(SqlError::Syntax("unterminated string literal".into()))
}

fn read_quoted(sql: &[u8], start: usize, quote: u8) -> SqlResult<(String, usize)> {
    let mut value = Vec::new();
    let mut i = start + 1;
    while i < sql.len() {
        if sql[i] == quote {
            if sql.get(i + 1) == Some(&quote) {
                value.push(quote);
                i += 2;
                continue;
            }
            return Ok((utf8(&value)?, i + 1));
        }
        value.push(sql[i]);
        i += 1;
    }
    Err(SqlError::Syntax("unterminated quoted identifier".into()))
}

/// Joins tokens into SQLite SQL, inserting whitespace wherever adjacent
/// tokens could otherwise fuse (`a OR b`, `- -1` vs `--`, `/ *`, quotes).
pub fn render(tokens: &[Token]) -> String {
    let mut out = String::new();
    for token in tokens {
        let text = token.render();
        if let (Some(last), Some(first)) = (out.chars().last(), text.chars().next()) {
            let wordish = |c: char| {
                c.is_ascii_alphanumeric()
                    || matches!(c, '_' | '$' | '\'' | '"' | '`')
                    || !c.is_ascii()
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
        out.push_str(&text);
    }
    out
}

/// Original source text of a token range (used for MySQL column labels).
pub fn source_text(tokens: &[Token]) -> String {
    let mut out = String::new();
    for (index, token) in tokens.iter().enumerate() {
        if index > 0 {
            out.push_str(&token.ws);
        }
        out.push_str(&token.text);
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

/// Uppercase leading keywords of a statement (stops at the first non-word).
pub fn leading_words(tokens: &[Token], count: usize) -> Vec<String> {
    tokens
        .iter()
        .take(count)
        .take_while(|t| t.kind == Kind::Word)
        .map(Token::upper)
        .collect()
}

/// Index of the `)` matching the `(` at `open`.
pub fn find_close(tokens: &[Token], open: usize) -> SqlResult<usize> {
    let mut depth = 0_i32;
    for (index, token) in tokens.iter().enumerate().skip(open) {
        if token.is_op("(") {
            depth += 1;
        } else if token.is_op(")") {
            depth -= 1;
            if depth == 0 {
                return Ok(index);
            }
        }
    }
    Err(SqlError::Syntax("unbalanced parentheses".into()))
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

    fn kinds(sql: &str) -> Vec<(Kind, String)> {
        tokenize(sql)
            .unwrap()
            .into_iter()
            .map(|t| (t.kind, if t.value.is_empty() { t.text } else { t.value }))
            .collect()
    }

    #[test]
    fn decodes_mysql_string_escapes() {
        let tokens = tokenize(r#"'it\'s' "say \"hi\"" 'a''b' 'x\\y' 'n\nl' '\%\_' 'z\0'"#).unwrap();
        let values: Vec<&str> = tokens.iter().map(|t| t.value.as_str()).collect();
        assert_eq!(
            values,
            ["it's", "say \"hi\"", "a'b", "x\\y", "n\nl", "\\%\\_", "z\0"]
        );
        assert_eq!(tokens[0].render(), "'it''s'");
        assert_eq!(tokens[3].render(), r"'x\y'");
        assert_eq!(tokens[6].render(), "CAST(X'7A00' AS TEXT)");
    }

    #[test]
    fn rejects_unterminated_constructs() {
        for sql in [
            "SELECT 'abc",
            "SELECT 'abc\\'",
            "SELECT `a",
            "SELECT /* x",
            "SELECT \"x",
        ] {
            assert!(matches!(tokenize(sql), Err(SqlError::Syntax(_))), "{sql}");
        }
    }

    #[test]
    fn comments_are_dropped_and_mysql_dash_rule_applies() {
        assert_eq!(
            render(&tokenize("SELECT 1 -- c\n, 2 # d\n/* e */").unwrap()),
            "SELECT 1  \n, 2"
        );
        // `1--1` is arithmetic in MySQL, not a comment.
        let tokens = tokenize("SELECT 1--1").unwrap();
        assert_eq!(tokens.len(), 5);
        assert_eq!(render(&tokens), "SELECT 1- -1");
    }

    #[test]
    fn literals_and_variables() {
        assert_eq!(
            kinds("x'0aFF' b'101' 0x1F 0b11 1e3 .5 t.5 @@session.sql_mode @`u v` ?"),
            [
                (Kind::HexStr, "X'0aFF'".into()),
                (Kind::Number, "5".into()),
                (Kind::Number, "0x1F".into()),
                (Kind::Number, "3".into()),
                (Kind::Number, "1e3".into()),
                (Kind::Number, ".5".into()),
                (Kind::Word, "t".into()),
                (Kind::Op, ".".into()),
                (Kind::Number, "5".into()),
                (Kind::SysVar, "sql_mode".into()),
                (Kind::UserVar, "u v".into()),
                (Kind::Placeholder, "?".into()),
            ]
        );
        assert!(tokenize("x'abc'").is_err());
        assert_eq!(kinds("1abc")[0], (Kind::Word, "1abc".into()));
    }

    #[test]
    fn statement_splitting_respects_quotes_and_trigger_bodies() {
        let statements = split_statements(tokenize("SELECT ';'; SELECT 2;; ").unwrap());
        assert_eq!(statements.len(), 2);
        let statements = split_statements(
            tokenize(
                "CREATE TRIGGER t AFTER INSERT ON a BEGIN UPDATE b SET x = CASE WHEN 1 THEN 2 END; END; SELECT 1",
            )
            .unwrap(),
        );
        assert_eq!(statements.len(), 2);
    }

    #[test]
    fn render_never_fuses_tokens() {
        let tokens = vec![
            Token::raw("a").with_ws(""),
            Token::raw("OR").with_ws(""),
            Token::raw("b").with_ws(""),
        ];
        assert_eq!(render(&tokens), "a OR b");
        let tokens = vec![Token::op("-"), Token::op("-")];
        assert_eq!(render(&tokens), "- -");
    }
}
