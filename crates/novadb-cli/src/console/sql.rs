//! Minimal SQL lexer used by the console for statement splitting, keyword
//! detection, and syntax highlighting.

use std::fmt::Write as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Whitespace,
    LineComment,
    BlockComment,
    String,
    QuotedIdent,
    Number,
    Word,
    Punct,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub start: usize,
    pub end: usize,
    /// `false` for a string, quoted identifier, or block comment that is not
    /// closed before the end of input.
    pub terminated: bool,
}

impl Token {
    fn is_trivia(self) -> bool {
        matches!(
            self.kind,
            TokenKind::Whitespace | TokenKind::LineComment | TokenKind::BlockComment
        )
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$' || !c.is_ascii()
}

/// Splits `sql` into tokens. Never fails: unterminated constructs are marked
/// with `terminated == false` and extend to the end of input.
pub fn tokenize(sql: &str) -> Vec<Token> {
    let bytes = sql.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < sql.len() {
        let start = i;
        let c = sql[i..].chars().next().unwrap_or('\0');
        let next = sql[i + c.len_utf8()..].chars().next();
        let mut terminated = true;
        let kind = if c.is_whitespace() {
            i += c.len_utf8();
            while let Some(ch) = sql[i..].chars().next() {
                if !ch.is_whitespace() {
                    break;
                }
                i += ch.len_utf8();
            }
            TokenKind::Whitespace
        } else if c == '-' && next == Some('-') {
            i = sql[i..].find('\n').map_or(sql.len(), |p| i + p);
            TokenKind::LineComment
        } else if c == '/' && next == Some('*') {
            match sql[i + 2..].find("*/") {
                Some(p) => i += 2 + p + 2,
                None => {
                    i = sql.len();
                    terminated = false;
                }
            }
            TokenKind::BlockComment
        } else if matches!(c, '\'' | '"' | '`' | '[') {
            let close = if c == '[' { b']' } else { c as u8 };
            i += 1;
            terminated = false;
            while i < bytes.len() {
                if bytes[i] == close {
                    // Doubled quote is an escaped quote (not for `[...]`).
                    if close != b']' && bytes.get(i + 1) == Some(&close) {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    terminated = true;
                    break;
                }
                i += 1;
            }
            if c == '\'' {
                TokenKind::String
            } else {
                TokenKind::QuotedIdent
            }
        } else if c.is_ascii_digit() || (c == '.' && next.is_some_and(|n| n.is_ascii_digit())) {
            i += 1;
            while i < bytes.len() {
                let b = bytes[i];
                let exponent_sign = (b == b'+' || b == b'-') && matches!(bytes[i - 1], b'e' | b'E');
                if b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || exponent_sign {
                    i += 1;
                } else {
                    break;
                }
            }
            TokenKind::Number
        } else if is_word_char(c) {
            i += c.len_utf8();
            while let Some(ch) = sql[i..].chars().next() {
                if !is_word_char(ch) {
                    break;
                }
                i += ch.len_utf8();
            }
            TokenKind::Word
        } else {
            i += c.len_utf8();
            TokenKind::Punct
        };
        tokens.push(Token {
            kind,
            start,
            end: i,
            terminated,
        });
    }
    tokens
}

/// Returns `true` when `sql` contains only whitespace and (closed) comments.
pub fn is_blank(sql: &str) -> bool {
    tokenize(sql).iter().all(|t| t.is_trivia() && t.terminated)
}

/// The first `n` significant words of `sql`, upper-cased.
pub fn keywords(sql: &str, n: usize) -> Vec<String> {
    tokenize(sql)
        .into_iter()
        .filter(|t| !t.is_trivia())
        .take(n)
        .take_while(|t| t.kind == TokenKind::Word)
        .map(|t| sql[t.start..t.end].to_ascii_uppercase())
        .collect()
}

/// Splits buffered input into complete statements (without the terminating
/// `;`) and the remaining incomplete text.
///
/// Semicolons inside string literals, quoted identifiers, comments, and
/// `CREATE TRIGGER ... BEGIN ... END` bodies do not terminate a statement.
/// Statements consisting only of comments/whitespace are dropped, and the
/// remainder is returned as an empty string when it is blank.
pub fn split_statements(input: &str) -> (Vec<String>, String) {
    let tokens = tokenize(input);
    let mut statements = Vec::new();
    let mut stmt_start = 0;

    // Per-statement state for trigger bodies.
    let mut leading: Vec<String> = Vec::new();
    let mut in_trigger_body = false;
    let mut trigger_body_done = false;
    let mut case_depth = 0usize;

    for token in &tokens {
        if token.is_trivia() {
            continue;
        }
        let text = &input[token.start..token.end];
        let is_trigger = leading.first().is_some_and(|w| w == "CREATE")
            && leading.iter().skip(1).take(2).any(|w| w == "TRIGGER");
        if token.kind == TokenKind::Punct && text == ";" {
            if is_trigger && in_trigger_body && !trigger_body_done {
                continue;
            }
            let statement = input[stmt_start..token.start].trim();
            if !is_blank(statement) {
                statements.push(statement.to_owned());
            }
            stmt_start = token.end;
            leading.clear();
            in_trigger_body = false;
            trigger_body_done = false;
            case_depth = 0;
            continue;
        }
        if token.kind == TokenKind::Word {
            let upper = text.to_ascii_uppercase();
            if leading.len() < 3 {
                leading.push(upper.clone());
            }
            let is_trigger = leading.first().is_some_and(|w| w == "CREATE")
                && leading.iter().skip(1).take(2).any(|w| w == "TRIGGER");
            if is_trigger {
                match upper.as_str() {
                    "BEGIN" if !in_trigger_body => in_trigger_body = true,
                    "CASE" if in_trigger_body => case_depth += 1,
                    "END" if in_trigger_body => {
                        if case_depth > 0 {
                            case_depth -= 1;
                        } else {
                            trigger_body_done = true;
                        }
                    }
                    _ => {}
                }
            }
        } else if leading.len() < 3 {
            // A non-word token ends the "leading keywords" window.
            leading.push(String::new());
        }
    }

    let rest = &input[stmt_start..];
    let rest = if is_blank(rest) {
        String::new()
    } else {
        rest.trim_start().to_owned()
    };
    (statements, rest)
}

/// `true` for a line consisting only of the T-SQL batch separator `GO`
/// (optionally followed by `;`).
pub fn is_go_line(line: &str) -> bool {
    let t = line.trim().trim_end_matches(';').trim_end();
    t.eq_ignore_ascii_case("go")
}

/// Whether a statement starting with these significant tokens (upper-cased
/// words; `None` for non-word tokens) is T-SQL procedural code that SQLite
/// cannot run statement by statement.
fn starts_procedural(first: &[Option<String>]) -> bool {
    let w = |i: usize| first.get(i).and_then(|x| x.as_deref());
    match w(0) {
        Some(
            "DECLARE" | "WHILE" | "IF" | "PRINT" | "RAISERROR" | "THROW" | "GOTO" | "WAITFOR"
            | "RETURN" | "EXEC" | "EXECUTE" | "SET",
        ) => true,
        Some("BEGIN") => {
            !matches!(
                w(1),
                None | Some("TRANSACTION" | "TRAN" | "DEFERRED" | "IMMEDIATE" | "EXCLUSIVE")
            ) && first.get(1).is_some()
        }
        _ => false,
    }
}

/// Byte offset of the first statement in `input` that is a T-SQL procedural
/// statement (variables / control flow). Only statement starts are considered
/// (the beginning of the input or right after a `;`).
pub fn procedural_start(input: &str) -> Option<usize> {
    let tokens: Vec<Token> = tokenize(input)
        .into_iter()
        .filter(|t| !t.is_trivia())
        .collect();
    let mut i = 0;
    let mut in_trigger = false;
    while i < tokens.len() {
        // Gather the leading tokens of the statement starting at `i`.
        let first: Vec<Option<String>> = tokens[i..]
            .iter()
            .take(3)
            .map(|t| {
                (t.kind == TokenKind::Word).then(|| input[t.start..t.end].to_ascii_uppercase())
            })
            .collect();
        let is_trigger = first.first().and_then(|x| x.as_deref()) == Some("CREATE")
            && first
                .iter()
                .skip(1)
                .any(|x| x.as_deref() == Some("TRIGGER"));
        if !is_trigger && starts_procedural(&first) {
            return Some(tokens[i].start);
        }
        in_trigger |= is_trigger;
        // Advance to the next statement start.
        let mut depth_end = false;
        while i < tokens.len() {
            let t = tokens[i];
            let text = &input[t.start..t.end];
            i += 1;
            if in_trigger && t.kind == TokenKind::Word && text.eq_ignore_ascii_case("end") {
                depth_end = true;
            }
            if t.kind == TokenKind::Punct && text == ";" && (!in_trigger || depth_end) {
                in_trigger = false;
                break;
            }
        }
    }
    None
}

/// `true` when a T-SQL batch contains a statement that would normally yield
/// rows (`SELECT`, `OUTPUT`, `RETURNING`).
pub fn batch_yields_rows(batch: &str) -> bool {
    let mut prev: Option<String> = None;
    for t in tokenize(batch).into_iter().filter(|t| !t.is_trivia()) {
        let text = &batch[t.start..t.end];
        let upper = text.to_ascii_uppercase();
        if t.kind == TokenKind::Word {
            if matches!(upper.as_str(), "OUTPUT" | "RETURNING") {
                return true;
            }
            if upper == "SELECT"
                && matches!(
                    prev.as_deref(),
                    None | Some(";" | "BEGIN" | "TRY" | "CATCH" | "ELSE" | ")")
                )
            {
                return true;
            }
        }
        prev = Some(upper);
    }
    false
}

/// `true` when `sql` mentions a row-producing clause (`SELECT`, `OUTPUT`,
/// `RETURNING`) anywhere; used to warn that `novadb exec` discards rows.
pub fn mentions_rows(sql: &str) -> bool {
    tokenize(sql).into_iter().any(|t| {
        t.kind == TokenKind::Word
            && matches!(
                sql[t.start..t.end].to_ascii_uppercase().as_str(),
                "SELECT" | "OUTPUT" | "RETURNING"
            )
    })
}

/// SQL keywords (SQLite dialect) used for highlighting and completion.
pub const KEYWORDS: &[&str] = &[
    "ABORT",
    "ACTION",
    "ADD",
    "AFTER",
    "ALL",
    "ALTER",
    "ALWAYS",
    "ANALYZE",
    "AND",
    "AS",
    "ASC",
    "ATTACH",
    "AUTOINCREMENT",
    "BEFORE",
    "BEGIN",
    "BETWEEN",
    "BY",
    "CASCADE",
    "CASE",
    "CAST",
    "CHECK",
    "COLLATE",
    "COLUMN",
    "COMMIT",
    "CONFLICT",
    "CONSTRAINT",
    "CREATE",
    "CROSS",
    "CURRENT",
    "CURRENT_DATE",
    "CURRENT_TIME",
    "CURRENT_TIMESTAMP",
    "DATABASE",
    "DEFAULT",
    "DEFERRABLE",
    "DEFERRED",
    "DELETE",
    "DESC",
    "DETACH",
    "DISTINCT",
    "DO",
    "DROP",
    "EACH",
    "ELSE",
    "END",
    "ESCAPE",
    "EXCEPT",
    "EXCLUDE",
    "EXCLUSIVE",
    "EXISTS",
    "EXPLAIN",
    "FAIL",
    "FILTER",
    "FIRST",
    "FOLLOWING",
    "FOR",
    "FOREIGN",
    "FROM",
    "FULL",
    "GENERATED",
    "GLOB",
    "GROUP",
    "GROUPS",
    "HAVING",
    "IF",
    "IGNORE",
    "IMMEDIATE",
    "IN",
    "INDEX",
    "INDEXED",
    "INITIALLY",
    "INNER",
    "INSERT",
    "INSTEAD",
    "INTERSECT",
    "INTO",
    "IS",
    "ISNULL",
    "JOIN",
    "KEY",
    "LAST",
    "LEFT",
    "LIKE",
    "LIMIT",
    "MATCH",
    "MATERIALIZED",
    "NATURAL",
    "NO",
    "NOT",
    "NOTHING",
    "NOTNULL",
    "NULL",
    "NULLS",
    "OF",
    "OFFSET",
    "ON",
    "OR",
    "ORDER",
    "OTHERS",
    "OUTER",
    "OVER",
    "PARTITION",
    "PLAN",
    "PRAGMA",
    "PRECEDING",
    "PRIMARY",
    "QUERY",
    "RAISE",
    "RANGE",
    "RECURSIVE",
    "REFERENCES",
    "REGEXP",
    "REINDEX",
    "RELEASE",
    "RENAME",
    "REPLACE",
    "RESTRICT",
    "RETURNING",
    "RIGHT",
    "ROLLBACK",
    "ROW",
    "ROWS",
    "SAVEPOINT",
    "SELECT",
    "SET",
    "STRICT",
    "TABLE",
    "TEMP",
    "TEMPORARY",
    "THEN",
    "TIES",
    "TO",
    "TRANSACTION",
    "TRIGGER",
    "UNBOUNDED",
    "UNION",
    "UNIQUE",
    "UPDATE",
    "USING",
    "VACUUM",
    "VALUES",
    "VIEW",
    "VIRTUAL",
    "WHEN",
    "WHERE",
    "WINDOW",
    "WITH",
    "WITHOUT",
    // Common type names and functions, for completion and highlighting.
    "INTEGER",
    "TEXT",
    "REAL",
    "BLOB",
    "NUMERIC",
    "BOOLEAN",
    "TRUE",
    "FALSE",
    "COUNT",
    "SUM",
    "AVG",
    "MIN",
    "MAX",
    "COALESCE",
    "IFNULL",
    "NULLIF",
    "LENGTH",
    "LOWER",
    "UPPER",
    "SUBSTR",
    "TRIM",
    "ROUND",
    "ABS",
    "GROUP_CONCAT",
    "JSON_EXTRACT",
    "JSON_OBJECT",
    "JSON_ARRAY",
    "DATETIME",
    "DATE",
    "TIME",
    "STRFTIME",
    "ROW_NUMBER",
    "RANK",
    "DENSE_RANK",
];

pub fn is_keyword(word: &str) -> bool {
    KEYWORDS.iter().any(|k| k.eq_ignore_ascii_case(word))
}

const RESET: &str = "\x1b[0m";

/// Returns `line` with ANSI colors for keywords, strings, numbers, and
/// comments. The display width is unchanged.
pub fn highlight(line: &str) -> String {
    let trimmed = line.trim_start();
    if trimmed.starts_with('.') || trimmed.starts_with('\\') {
        let indent = line.len() - trimmed.len();
        let cmd_end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
        return format!(
            "{}\x1b[1;35m{}{RESET}{}",
            &line[..indent],
            &trimmed[..cmd_end],
            &trimmed[cmd_end..]
        );
    }
    let mut out = String::with_capacity(line.len() + 32);
    for token in tokenize(line) {
        let text = &line[token.start..token.end];
        let color = match token.kind {
            TokenKind::Word if is_keyword(text) => Some("\x1b[1;34m"),
            TokenKind::String => Some("\x1b[32m"),
            TokenKind::Number => Some("\x1b[33m"),
            TokenKind::LineComment | TokenKind::BlockComment => Some("\x1b[90m"),
            TokenKind::QuotedIdent => Some("\x1b[36m"),
            _ => None,
        };
        match color {
            Some(c) => {
                let _ = write!(out, "{c}{text}{RESET}");
            }
            None => out.push_str(text),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(s: &str) -> (Vec<String>, String) {
        split_statements(s)
    }

    #[test]
    fn splits_multiple_statements_per_line() {
        let (stmts, rest) = split("SELECT 1; SELECT 2;SELECT 3");
        assert_eq!(stmts, vec!["SELECT 1", "SELECT 2"]);
        assert_eq!(rest, "SELECT 3");
    }

    #[test]
    fn semicolons_in_strings_and_identifiers_are_ignored() {
        let (stmts, rest) = split(
            "INSERT INTO t VALUES ('a;b', 'it''s; ok'); SELECT \"we;ird\", [x;y], `z;` FROM t;",
        );
        assert_eq!(
            stmts,
            vec![
                "INSERT INTO t VALUES ('a;b', 'it''s; ok')",
                "SELECT \"we;ird\", [x;y], `z;` FROM t"
            ]
        );
        assert_eq!(rest, "");
    }

    #[test]
    fn semicolons_in_comments_are_ignored() {
        let (stmts, rest) = split("SELECT 1 -- trailing; comment\n, 2; /* a;b */ SELECT 3;");
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].starts_with("SELECT 1 -- trailing; comment"));
        assert_eq!(stmts[1], "/* a;b */ SELECT 3");
        assert_eq!(rest, "");
    }

    #[test]
    fn unterminated_string_stays_incomplete() {
        let (stmts, rest) = split("SELECT 'abc;\n");
        assert!(stmts.is_empty());
        assert_eq!(rest, "SELECT 'abc;\n");
        let (stmts, rest) = split("SELECT 'abc;\ndef';");
        assert_eq!(stmts, vec!["SELECT 'abc;\ndef'"]);
        assert_eq!(rest, "");
    }

    #[test]
    fn unterminated_block_comment_stays_incomplete() {
        let (stmts, rest) = split("/* still; open");
        assert!(stmts.is_empty());
        assert_eq!(rest, "/* still; open");
    }

    #[test]
    fn blank_and_comment_only_input_is_dropped() {
        assert_eq!(split("  ;; -- nothing\n"), (vec![], String::new()));
        assert!(is_blank("-- hi\n /* x */ "));
        assert!(!is_blank("/* open"));
    }

    #[test]
    fn trigger_bodies_are_kept_together() {
        let sql = "CREATE TRIGGER trg AFTER INSERT ON t BEGIN \
                   UPDATE t SET a = CASE WHEN new.a > 0 THEN 1 ELSE 0 END; \
                   INSERT INTO log VALUES (1); END; SELECT 1;";
        let (stmts, rest) = split(sql);
        assert_eq!(stmts.len(), 2, "{stmts:?}");
        assert!(stmts[0].ends_with("INSERT INTO log VALUES (1); END"));
        assert_eq!(stmts[1], "SELECT 1");
        assert_eq!(rest, "");

        let (stmts, rest) = split("CREATE TEMP TRIGGER x BEFORE DELETE ON t BEGIN SELECT 1;");
        assert!(stmts.is_empty());
        assert!(rest.starts_with("CREATE TEMP TRIGGER"));
    }

    #[test]
    fn first_keyword_skips_comments() {
        assert_eq!(
            keywords("-- c\n/* d */ insert into t", 2),
            ["INSERT", "INTO"]
        );
        assert!(keywords("(SELECT 1)", 1).is_empty());
    }

    #[test]
    fn highlight_preserves_text() {
        let line = "SELECT 'x', 42 FROM t -- c";
        let colored = highlight(line);
        let stripped: String = strip_ansi(&colored);
        assert_eq!(stripped, line);
        assert!(colored.contains("\x1b[1;34mSELECT"));
    }

    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn detects_procedural_batches() {
        assert_eq!(
            procedural_start("SELECT 1; INSERT INTO t VALUES (1);"),
            None
        );
        assert_eq!(procedural_start("BEGIN; COMMIT;"), None);
        assert_eq!(procedural_start("BEGIN TRANSACTION; SELECT 1"), None);
        assert_eq!(procedural_start("DECLARE @x INT = 5;"), Some(0));
        assert_eq!(procedural_start("SELECT 1; DECLARE @x INT;"), Some(10));
        assert_eq!(procedural_start("while 1=1 BEGIN BREAK END"), Some(0));
        assert_eq!(procedural_start("BEGIN TRY SELECT 1 END TRY"), Some(0));
        assert_eq!(procedural_start("IF 1=1 PRINT 'x'"), Some(0));
        assert_eq!(procedural_start("SELECT 'DECLARE @x'; -- IF\n"), None);
        assert_eq!(
            procedural_start("CREATE TRIGGER g AFTER INSERT ON t BEGIN SET x = 1; END;"),
            None
        );
    }

    #[test]
    fn go_lines_and_row_detection() {
        assert!(is_go_line(" go "));
        assert!(is_go_line("GO;"));
        assert!(!is_go_line("GO TO"));
        assert!(batch_yields_rows("DECLARE @x INT = 1; SELECT @x;"));
        assert!(!batch_yields_rows(
            "DECLARE @x INT = 1; INSERT INTO t SELECT 1;"
        ));
        assert!(mentions_rows("insert into t values (1) returning id"));
        assert!(!mentions_rows("CREATE TABLE t (x)"));
    }
}
