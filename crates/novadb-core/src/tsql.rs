//! T-SQL procedural layer.
//!
//! The regex transpiler in `lib.rs` rewrites *single statements* from T-SQL
//! into SQLite. Control flow cannot be expressed that way, so batches that
//! use it are executed here instead: the batch is parsed into a small
//! statement tree and interpreted, while every leaf statement (and every
//! expression, evaluated as `SELECT <expr>`) still goes through the
//! transpiler and then SQLite.
//!
//! Supported: `DECLARE` (scalars, `TABLE` variables, cursors), `SET @v`,
//! `SELECT @v = ...`, `IF`/`ELSE`, `WHILE`/`BREAK`/`CONTINUE`,
//! `BEGIN`/`END`, `BEGIN TRY`/`BEGIN CATCH` with `ERROR_*()` functions,
//! `THROW`, `RAISERROR`, `RETURN`, `GOTO`/labels, `PRINT` (discarded),
//! `BEGIN TRAN`/`SAVE TRAN`/`COMMIT`/`ROLLBACK` (as savepoints inside the
//! atomic batch), `EXEC sp_executesql` / `EXEC (...)`, cursors
//! (`OPEN`/`FETCH`/`CLOSE`/`DEALLOCATE`), sequences (`CREATE`/`ALTER`/`DROP
//! SEQUENCE`, `NEXT VALUE FOR`) and `WAITFOR DELAY`.
//!
//! Anything that cannot be executed faithfully returns [`Error::Unsupported`]
//! rather than silently succeeding.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use rusqlite::Connection;
use rusqlite::types::Value;

use crate::functions::tsql_functions::try_cast;
use crate::{
    Error, MaskedSpans, Result, matching_paren, normalize_single_batch, split_go_batches,
    split_top_level_commas,
};

// ---------------------------------------------------------------------------
// Tokenizer (operates on masked SQL)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Word,
    Var,
    SysVar,
    Number,
    Literal,
    Comment,
    Punct(char),
}

#[derive(Debug, Clone, Copy)]
struct Token {
    kind: Kind,
    start: usize,
    end: usize,
}

fn is_placeholder(ch: char) -> bool {
    ('\u{E000}'..='\u{E1FF}').contains(&ch)
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '_' | '#' | '$')
}

fn tokenize(sql: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let chars: Vec<(usize, char)> = sql.char_indices().collect();
    let end_of = |idx: usize| chars.get(idx).map_or(sql.len(), |(pos, _)| *pos);
    let mut idx = 0;
    while idx < chars.len() {
        let (start, ch) = chars[idx];
        if ch.is_whitespace() {
            idx += 1;
            continue;
        }
        let mut next = idx + 1;
        let kind = match ch {
            '\'' | '"' | '[' => {
                let close = match ch {
                    '[' => ']',
                    other => other,
                };
                while next < chars.len() && chars[next].1 != close {
                    next += 1;
                }
                next = (next + 1).min(chars.len());
                Kind::Literal
            }
            '\u{E002}' => {
                while next < chars.len() && chars[next].1 != '\u{E003}' {
                    next += 1;
                }
                next = (next + 1).min(chars.len());
                Kind::Comment
            }
            '\u{E004}' => {
                // Generated marker comment: runs to end of line.
                while next < chars.len() && chars[next].1 != '\n' {
                    next += 1;
                }
                Kind::Comment
            }
            '@' => {
                let sys = chars.get(next).is_some_and(|(_, c)| *c == '@');
                if sys {
                    next += 1;
                }
                while next < chars.len() && is_word_char(chars[next].1) {
                    next += 1;
                }
                if sys { Kind::SysVar } else { Kind::Var }
            }
            c if c.is_ascii_digit() => {
                while next < chars.len()
                    && (chars[next].1.is_ascii_alphanumeric() || chars[next].1 == '.')
                {
                    next += 1;
                }
                Kind::Number
            }
            c if is_word_char(c) => {
                while next < chars.len() && is_word_char(chars[next].1) {
                    next += 1;
                }
                Kind::Word
            }
            c if is_placeholder(c) => {
                while next < chars.len() && is_placeholder(chars[next].1) {
                    next += 1;
                }
                Kind::Word
            }
            other => Kind::Punct(other),
        };
        tokens.push(Token {
            kind,
            start,
            end: end_of(next),
        });
        idx = next;
    }
    tokens
}

/// Keywords that start a statement (used to find where an `IF`/`WHILE`
/// condition ends).
const STATEMENT_START: &[&str] = &[
    "BEGIN",
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "MERGE",
    "SET",
    "DECLARE",
    "PRINT",
    "RETURN",
    "BREAK",
    "CONTINUE",
    "THROW",
    "RAISERROR",
    "EXEC",
    "EXECUTE",
    "IF",
    "WHILE",
    "CREATE",
    "DROP",
    "ALTER",
    "TRUNCATE",
    "WITH",
    "ROLLBACK",
    "COMMIT",
    "SAVE",
    "GOTO",
    "WAITFOR",
    "OPEN",
    "FETCH",
    "CLOSE",
    "DEALLOCATE",
    "USE",
    "GRANT",
    "REVOKE",
    "DENY",
    "REPLACE",
    "VALUES",
    "PRAGMA",
    "VACUUM",
    "ANALYZE",
    "REINDEX",
];

/// Keywords that always end a statement written without a semicolon.
const HARD_BOUNDARY: &[&str] = &[
    "DECLARE",
    "PRINT",
    "WHILE",
    "BREAK",
    "CONTINUE",
    "RETURN",
    "THROW",
    "RAISERROR",
    "GOTO",
    "EXEC",
    "EXECUTE",
    "WAITFOR",
    "BEGIN",
    "DEALLOCATE",
];

/// Words after which `IF` belongs to the statement (`DROP TABLE IF EXISTS`).
const IF_OWNERS: &[&str] = &[
    "TABLE",
    "VIEW",
    "INDEX",
    "TRIGGER",
    "SCHEMA",
    "SEQUENCE",
    "TYPE",
    "PROCEDURE",
    "PROC",
    "FUNCTION",
    "DATABASE",
    "COLUMN",
    "CONSTRAINT",
    "USER",
    "ROLE",
    "LOGIN",
    "STATISTICS",
    "SYNONYM",
    "DEFAULT",
    "RULE",
    "POLICY",
    "ASSEMBLY",
    "KEY",
    "CERTIFICATE",
];

// ---------------------------------------------------------------------------
// Statement tree
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Stmt {
    /// A statement handed to the transpiler + SQLite (masked text).
    Leaf(String),
    Block(Vec<Stmt>),
    If {
        cond: String,
        then: Box<Stmt>,
        otherwise: Option<Box<Stmt>>,
    },
    While {
        cond: String,
        body: Box<Stmt>,
    },
    TryCatch {
        body: Vec<Stmt>,
        handler: Vec<Stmt>,
    },
    Declare(String),
    DeclareCursor {
        name: String,
        query: String,
    },
    SetVar {
        name: String,
        op: String,
        expr: String,
    },
    SelectAssign(String),
    Break,
    Continue,
    Return,
    Goto(String),
    Label(String),
    Throw(Option<String>),
    Raiserror(String),
    BeginTran,
    Commit,
    Rollback(Option<String>),
    SaveTran(String),
    Open(String),
    Fetch {
        direction: String,
        cursor: String,
        into: Vec<String>,
    },
    Close(String),
    Deallocate(String),
    Exec(String),
    WaitFor(String),
    CreateSequence(String),
    CreateType(String),
    DropType(String),
    AlterSequence(String),
    DropSequence(String),
    /// `CREATE PROCEDURE/FUNCTION/TRIGGER` (T-SQL): the rest of the batch is
    /// the routine body. Routines are recorded, not executed.
    Routine,
    Noop,
}

struct Parser<'a> {
    src: &'a str,
    tokens: Vec<Token>,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Self {
        let tokens = tokenize(src)
            .into_iter()
            .filter(|token| token.kind != Kind::Comment)
            .collect();
        Self {
            src,
            tokens,
            pos: 0,
        }
    }

    fn text(&self, token: Token) -> &'a str {
        &self.src[token.start..token.end]
    }

    fn peek(&self, offset: usize) -> Option<Token> {
        self.tokens.get(self.pos + offset).copied()
    }

    fn is_word(&self, offset: usize, word: &str) -> bool {
        self.peek(offset)
            .is_some_and(|t| t.kind == Kind::Word && self.text(t).eq_ignore_ascii_case(word))
    }

    fn is_punct(&self, offset: usize, ch: char) -> bool {
        self.peek(offset).is_some_and(|t| t.kind == Kind::Punct(ch))
    }

    fn word_upper(&self, offset: usize) -> Option<String> {
        self.peek(offset)
            .filter(|t| t.kind == Kind::Word)
            .map(|t| self.text(t).to_ascii_uppercase())
    }

    fn expect_word(&mut self, word: &str) -> Result<()> {
        if self.is_word(0, word) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.syntax_error(&format!("expected `{word}`")))
        }
    }

    fn syntax_error(&self, message: &str) -> Error {
        let near = self.peek(0).map_or("end of batch".to_string(), |t| {
            format!("`{}`", self.text(t))
        });
        Error::Unsupported(format!("T-SQL syntax: {message} near {near}"))
    }

    fn skip_semicolons(&mut self) {
        while self.is_punct(0, ';') {
            self.pos += 1;
        }
    }

    fn span(&self, from: usize, to: usize) -> String {
        if from >= to {
            return String::new();
        }
        let start = self.tokens[from].start;
        let end = self.tokens[to - 1].end;
        self.src[start..end].to_string()
    }

    fn at_block_end(&self) -> bool {
        self.is_word(0, "END")
    }

    fn parse_list(&mut self, in_block: bool) -> Result<Vec<Stmt>> {
        let mut list = Vec::new();
        loop {
            self.skip_semicolons();
            if self.peek(0).is_none() {
                if in_block {
                    return Err(self.syntax_error("missing `END`"));
                }
                return Ok(list);
            }
            if self.at_block_end() {
                if in_block {
                    return Ok(list);
                }
                return Err(self.syntax_error("unexpected `END`"));
            }
            list.push(self.parse_statement()?);
        }
    }

    /// Reads tokens of a plain statement until `;` or a statement boundary;
    /// returns the token range.
    fn statement_range(&mut self) -> (usize, usize) {
        let start = self.pos;
        let mut depth = 0usize;
        let mut case_depth = 0usize;
        while let Some(token) = self.peek(0) {
            match token.kind {
                Kind::Punct('(') => depth += 1,
                Kind::Punct(')') => depth = depth.saturating_sub(1),
                Kind::Punct(';') if depth == 0 => break,
                Kind::Word if depth == 0 && self.pos > start => {
                    let word = self.text(token).to_ascii_uppercase();
                    let prev = self
                        .tokens
                        .get(self.pos - 1)
                        .map(|t| self.text(*t).to_ascii_uppercase())
                        .unwrap_or_default();
                    match word.as_str() {
                        "CASE" => case_depth += 1,
                        "END" if case_depth > 0 => case_depth -= 1,
                        "ELSE" if case_depth > 0 => {}
                        "END" | "ELSE" => break,
                        "IF" if !IF_OWNERS.contains(&prev.as_str()) => break,
                        "ROLLBACK" | "COMMIT"
                            if !matches!(prev.as_str(), "OR" | "CONFLICT" | "(") =>
                        {
                            break;
                        }
                        "SAVE" if self.is_word(1, "TRAN") || self.is_word(1, "TRANSACTION") => {
                            break;
                        }
                        w if HARD_BOUNDARY.contains(&w) => break,
                        _ => {}
                    }
                }
                Kind::Word if depth == 0 => {
                    if self.text(token).eq_ignore_ascii_case("CASE") {
                        case_depth += 1;
                    }
                }
                _ => {}
            }
            self.pos += 1;
        }
        (start, self.pos)
    }

    fn leaf(&mut self) -> Stmt {
        let (start, end) = self.statement_range();
        Stmt::Leaf(self.span(start, end))
    }

    /// Condition of IF/WHILE: everything up to the first statement keyword at
    /// parenthesis depth 0 (outside CASE).
    fn condition(&mut self) -> Result<String> {
        let start = self.pos;
        let mut depth = 0usize;
        let mut case_depth = 0usize;
        while let Some(token) = self.peek(0) {
            match token.kind {
                Kind::Punct('(') => depth += 1,
                Kind::Punct(')') => depth = depth.saturating_sub(1),
                Kind::Punct(';') if depth == 0 => break,
                Kind::Word if depth == 0 => {
                    let word = self.text(token).to_ascii_uppercase();
                    match word.as_str() {
                        "CASE" => case_depth += 1,
                        "END" if case_depth > 0 => case_depth -= 1,
                        w if case_depth == 0
                            && self.pos > start
                            && STATEMENT_START.contains(&w) =>
                        {
                            break;
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
            self.pos += 1;
        }
        if self.pos == start {
            return Err(self.syntax_error("missing condition"));
        }
        Ok(self.span(start, self.pos))
    }

    fn rest_of_statement(&mut self) -> String {
        let (start, end) = self.statement_range();
        self.span(start, end)
    }

    fn parse_statement(&mut self) -> Result<Stmt> {
        let Some(first) = self.peek(0) else {
            return Err(self.syntax_error("unexpected end of batch"));
        };
        // Labels: `name:`
        if first.kind == Kind::Word && self.is_punct(1, ':') && !self.is_punct(2, ':') {
            let label = self.text(first).to_ascii_uppercase();
            self.pos += 2;
            return Ok(Stmt::Label(label));
        }
        let Some(word) = self.word_upper(0) else {
            return Ok(self.leaf());
        };
        match word.as_str() {
            "BEGIN" => {
                if self.is_word(1, "TRY") {
                    self.pos += 2;
                    let body = self.parse_list(true)?;
                    self.expect_word("END")?;
                    self.expect_word("TRY")?;
                    self.skip_semicolons();
                    self.expect_word("BEGIN")?;
                    self.expect_word("CATCH")?;
                    let handler = self.parse_list(true)?;
                    self.expect_word("END")?;
                    self.expect_word("CATCH")?;
                    return Ok(Stmt::TryCatch { body, handler });
                }
                let tran = |p: &Self, o: usize| p.is_word(o, "TRAN") || p.is_word(o, "TRANSACTION");
                if tran(self, 1) || (self.is_word(1, "DISTRIBUTED") && tran(self, 2)) {
                    self.rest_of_statement();
                    return Ok(Stmt::BeginTran);
                }
                if self.is_word(1, "DIALOG") || self.is_word(1, "CONVERSATION") {
                    return Err(Error::Unsupported("Service Broker".into()));
                }
                if self.peek(1).is_none_or(|t| t.kind == Kind::Punct(';'))
                    || ["DEFERRED", "IMMEDIATE", "EXCLUSIVE", "WORK"]
                        .iter()
                        .any(|w| self.is_word(1, w))
                {
                    // SQLite transaction control: let the authorizer reject it.
                    return Ok(self.leaf());
                }
                self.pos += 1;
                let body = self.parse_list(true)?;
                self.expect_word("END")?;
                Ok(Stmt::Block(body))
            }
            "IF" => {
                self.pos += 1;
                let cond = self.condition()?;
                let then = Box::new(self.parse_statement()?);
                let save = self.pos;
                self.skip_semicolons();
                let otherwise = if self.is_word(0, "ELSE") {
                    self.pos += 1;
                    Some(Box::new(self.parse_statement()?))
                } else {
                    self.pos = save;
                    None
                };
                Ok(Stmt::If {
                    cond,
                    then,
                    otherwise,
                })
            }
            "WHILE" => {
                self.pos += 1;
                let cond = self.condition()?;
                let body = Box::new(self.parse_statement()?);
                Ok(Stmt::While { cond, body })
            }
            "ELSE" => Err(self.syntax_error("`ELSE` without `IF`")),
            "DECLARE" => {
                let is_cursor = self.peek(1).is_some_and(|t| t.kind == Kind::Word)
                    && (2..6).any(|offset| self.is_word(offset, "CURSOR"))
                    && !self.peek(1).is_some_and(|t| t.kind == Kind::Var);
                if is_cursor {
                    let name = self.text(self.tokens[self.pos + 1]).to_ascii_uppercase();
                    let text = self.rest_of_statement();
                    let upper = text.to_ascii_uppercase();
                    let query = find_keyword(&text, "FOR")
                        .map(|idx| text[idx + 3..].to_string())
                        .ok_or_else(|| {
                            Error::Unsupported(format!("cursor without query: {upper}"))
                        })?;
                    let query = strip_trailing_for_update(&query);
                    return Ok(Stmt::DeclareCursor { name, query });
                }
                self.pos += 1;
                Ok(Stmt::Declare(self.rest_of_statement()))
            }
            "SET" if self.peek(1).is_some_and(|t| t.kind == Kind::Var) => {
                let name = self.text(self.tokens[self.pos + 1]).to_string();
                self.pos += 2;
                let mut op = String::new();
                while let Some(token) = self.peek(0) {
                    match token.kind {
                        Kind::Punct('=') => {
                            self.pos += 1;
                            break;
                        }
                        Kind::Punct(c) if "+-*/%&|^".contains(c) => {
                            op.push(c);
                            self.pos += 1;
                        }
                        Kind::Punct('.') => {
                            return Err(Error::Unsupported("XML data type methods".into()));
                        }
                        _ => return Err(self.syntax_error("expected `=` in SET")),
                    }
                }
                let expr = self.rest_of_statement();
                Ok(Stmt::SetVar { name, op, expr })
            }
            "SELECT"
                if self.peek(1).is_some_and(|t| t.kind == Kind::Var)
                    && (self.is_punct(2, '=')
                        || (self.peek(2).is_some_and(
                            |t| matches!(t.kind, Kind::Punct(c) if "+-*/%&|^".contains(c)),
                        ) && self.is_punct(3, '='))) =>
            {
                Ok(Stmt::SelectAssign(self.rest_of_statement()))
            }
            "PRINT" => {
                self.rest_of_statement();
                Ok(Stmt::Noop)
            }
            "RETURN" => {
                self.rest_of_statement();
                Ok(Stmt::Return)
            }
            "BREAK" => {
                self.pos += 1;
                Ok(Stmt::Break)
            }
            "CONTINUE" => {
                self.pos += 1;
                Ok(Stmt::Continue)
            }
            "GOTO" => {
                let label = self
                    .word_upper(1)
                    .ok_or_else(|| self.syntax_error("GOTO without label"))?;
                self.pos += 2;
                Ok(Stmt::Goto(label))
            }
            "THROW" => {
                self.pos += 1;
                let args = self.rest_of_statement();
                Ok(Stmt::Throw((!args.trim().is_empty()).then_some(args)))
            }
            "RAISERROR" => {
                self.pos += 1;
                Ok(Stmt::Raiserror(self.rest_of_statement()))
            }
            "COMMIT" => {
                self.rest_of_statement();
                Ok(Stmt::Commit)
            }
            "ROLLBACK" => {
                self.pos += 1;
                if self.is_word(0, "TRAN")
                    || self.is_word(0, "TRANSACTION")
                    || self.is_word(0, "WORK")
                {
                    self.pos += 1;
                }
                let name = self.rest_of_statement();
                let name = name.trim();
                Ok(Stmt::Rollback(
                    (!name.is_empty()).then(|| name.to_ascii_uppercase()),
                ))
            }
            "SAVE" if self.is_word(1, "TRAN") || self.is_word(1, "TRANSACTION") => {
                self.pos += 2;
                let name = self.rest_of_statement().trim().to_ascii_uppercase();
                Ok(Stmt::SaveTran(name))
            }
            "OPEN" | "CLOSE"
                if ["SYMMETRIC", "ASYMMETRIC", "MASTER", "ALL"]
                    .iter()
                    .any(|w| self.is_word(1, w)) =>
            {
                Ok(self.leaf())
            }
            "OPEN" => {
                self.pos += 1;
                Ok(Stmt::Open(cursor_name(&self.rest_of_statement())))
            }
            "CLOSE" => {
                self.pos += 1;
                Ok(Stmt::Close(cursor_name(&self.rest_of_statement())))
            }
            "DEALLOCATE" => {
                self.pos += 1;
                Ok(Stmt::Deallocate(cursor_name(&self.rest_of_statement())))
            }
            "FETCH" => {
                self.pos += 1;
                let text = self.rest_of_statement();
                parse_fetch(&text)
            }
            "EXEC" | "EXECUTE" => {
                self.pos += 1;
                Ok(Stmt::Exec(self.rest_of_statement()))
            }
            "WAITFOR" if self.is_word(1, "DELAY") || self.is_word(1, "TIME") => {
                self.pos += 1;
                Ok(Stmt::WaitFor(self.rest_of_statement()))
            }
            "CREATE" | "ALTER" | "DROP" => {
                let mut offset = 1;
                if self.is_word(1, "OR") && self.is_word(2, "ALTER") {
                    offset = 3;
                }
                let object = self.word_upper(offset).unwrap_or_default();
                match (word.as_str(), object.as_str()) {
                    ("CREATE" | "ALTER", "TRIGGER") => Err(Error::Unsupported(
                        "T-SQL triggers (use SQLite CREATE TRIGGER ... BEGIN ... END)".into(),
                    )),
                    ("CREATE" | "ALTER", "PROCEDURE" | "PROC" | "FUNCTION") => {
                        self.pos = self.tokens.len();
                        Ok(Stmt::Routine)
                    }
                    ("CREATE", "RULE" | "DEFAULT") => Err(Error::Unsupported(
                        "legacy CREATE RULE / CREATE DEFAULT objects are not supported by NovaDB"
                            .into(),
                    )),
                    ("CREATE", "SEQUENCE") => Ok(Stmt::CreateSequence(self.rest_of_statement())),
                    ("ALTER", "SEQUENCE") => Ok(Stmt::AlterSequence(self.rest_of_statement())),
                    ("DROP", "SEQUENCE") => Ok(Stmt::DropSequence(self.rest_of_statement())),
                    ("CREATE", "TYPE") => Ok(Stmt::CreateType(self.rest_of_statement())),
                    ("DROP", "TYPE") => Ok(Stmt::DropType(self.rest_of_statement())),
                    _ => Ok(self.leaf()),
                }
            }
            _ => Ok(self.leaf()),
        }
    }
}

fn cursor_name(text: &str) -> String {
    text.split_whitespace()
        .rfind(|w| !w.eq_ignore_ascii_case("GLOBAL"))
        .unwrap_or("")
        .to_ascii_uppercase()
}

fn strip_trailing_for_update(query: &str) -> String {
    match find_keyword(query, "FOR") {
        Some(idx)
            if query[idx..].to_ascii_uppercase().contains("UPDATE")
                || query[idx..].to_ascii_uppercase().contains("READ") =>
        {
            query[..idx].to_string()
        }
        _ => query.to_string(),
    }
}

/// Byte offset of `keyword` as a whole word at parenthesis depth 0.
fn find_keyword(text: &str, keyword: &str) -> Option<usize> {
    let mut depth = 0usize;
    for token in tokenize(text) {
        match token.kind {
            Kind::Punct('(') => depth += 1,
            Kind::Punct(')') => depth = depth.saturating_sub(1),
            Kind::Word
                if depth == 0 && text[token.start..token.end].eq_ignore_ascii_case(keyword) =>
            {
                return Some(token.start);
            }
            _ => {}
        }
    }
    None
}

fn parse_fetch(text: &str) -> Result<Stmt> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut idx = 0;
    let mut direction = "NEXT".to_string();
    if let Some(first) = words.first() {
        let upper = first.to_ascii_uppercase();
        if matches!(upper.as_str(), "NEXT" | "PRIOR" | "FIRST" | "LAST") {
            direction = upper;
            idx = 1;
        } else if matches!(upper.as_str(), "ABSOLUTE" | "RELATIVE") {
            return Err(Error::Unsupported(format!(
                "FETCH {upper} is not supported"
            )));
        }
    }
    if words
        .get(idx)
        .is_some_and(|w| w.eq_ignore_ascii_case("FROM"))
    {
        idx += 1;
    }
    let cursor = words
        .get(idx)
        .filter(|w| !w.eq_ignore_ascii_case("GLOBAL"))
        .or_else(|| words.get(idx + 1))
        .map(|w| w.trim_end_matches(',').to_ascii_uppercase())
        .ok_or_else(|| Error::Unsupported("FETCH without cursor".to_string()))?;
    let into = match find_keyword(text, "INTO") {
        Some(pos) => text[pos + 4..]
            .split(',')
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .collect(),
        None => Vec::new(),
    };
    Ok(Stmt::Fetch {
        direction,
        cursor,
        into,
    })
}

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

/// Does this (masked) script need the procedural executor?
pub(crate) fn requires_executor(masked: &str) -> bool {
    let tokens = tokenize(masked);
    let text = |t: &Token| &masked[t.start..t.end];
    for (idx, token) in tokens.iter().enumerate() {
        match token.kind {
            Kind::Var => return true,
            Kind::SysVar => {
                let upper = text(token).to_ascii_uppercase();
                if matches!(
                    upper.as_str(),
                    "@@ROWCOUNT" | "@@FETCH_STATUS" | "@@TRANCOUNT" | "@@ERROR"
                ) {
                    return true;
                }
            }
            Kind::Word => {
                let upper = text(token).to_ascii_uppercase();
                let prev = idx
                    .checked_sub(1)
                    .map(|i| text(&tokens[i]).to_ascii_uppercase())
                    .unwrap_or_default();
                let next = tokens
                    .get(idx + 1)
                    .map(|t| text(t).to_ascii_uppercase())
                    .unwrap_or_default();
                let marker = match upper.as_str() {
                    "DECLARE" | "WHILE" | "THROW" | "RAISERROR" | "PRINT" | "GOTO" | "BREAK"
                    | "CONTINUE" | "WAITFOR" | "TRY" | "CATCH" | "TRAN" | "TRANSACTION"
                    | "EXEC" | "EXECUTE" | "SEQUENCE" | "DEALLOCATE" => true,
                    "IF" => !IF_OWNERS.contains(&prev.as_str()),
                    "BEGIN" => !matches!(
                        next.as_str(),
                        "" | ";" | "DEFERRED" | "IMMEDIATE" | "EXCLUSIVE"
                    ),
                    "VALUE" => prev == "NEXT",
                    "TYPE" => prev == "CREATE" || prev == "DROP",
                    "OUTPUT" => true,
                    "RETURN" => true,
                    "FETCH" => {
                        prev != "ONLY" && next != "FIRST" && next != "NEXT"
                            || next == "NEXT" && prev != "ROWS"
                    }
                    _ => false,
                };
                if marker {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Constructs the single-statement transpiler cannot execute faithfully.
/// Batches executed by [`execute_script`] never reach the transpiler with
/// these; they only arrive through paths such as `query()`.
pub(crate) fn transpiler_rejection(masked: &str) -> Option<(String, bool)> {
    let tokens = tokenize(masked);
    let text = |t: &Token| masked[t.start..t.end].to_ascii_uppercase();
    for (idx, token) in tokens.iter().enumerate() {
        if token.kind != Kind::Word {
            continue;
        }
        let upper = text(token);
        let prev = idx
            .checked_sub(1)
            .map(|i| text(&tokens[i]))
            .unwrap_or_default();
        let next = tokens.get(idx + 1).map(&text).unwrap_or_default();
        let construct = match upper.as_str() {
            "WHILE" => Some("WHILE loops"),
            "TRY" if prev == "BEGIN" => Some("TRY/CATCH"),
            "THROW" => Some("THROW"),
            "RAISERROR" => Some("RAISERROR"),
            "GOTO" => Some("GOTO"),
            "BREAK" | "CONTINUE" => Some("loop control"),
            "CURSOR" => Some("cursors"),
            "IF" if !IF_OWNERS.contains(&prev.as_str()) => Some("IF/ELSE"),
            "VALUE" if prev == "NEXT" => Some("NEXT VALUE FOR"),
            "SEQUENCE" if prev == "CREATE" || prev == "ALTER" => Some("sequences"),
            "TYPE" if prev == "CREATE" || prev == "DROP" => Some("user-defined types"),
            "OUTPUT" if masked[token.end..].to_ascii_uppercase().contains("INTO") => {
                Some("OUTPUT ... INTO")
            }
            "SET" if tokens.get(idx + 1).is_some_and(|t| t.kind == Kind::Var) => {
                Some("SET @variable")
            }
            "TRAN" | "TRANSACTION"
                if matches!(prev.as_str(), "BEGIN" | "COMMIT" | "ROLLBACK" | "SAVE") =>
            {
                Some("T-SQL transaction control")
            }
            _ => None,
        };
        let prev2 = idx
            .checked_sub(2)
            .map(|i| text(&tokens[i]))
            .unwrap_or_default();
        let created =
            matches!(prev.as_str(), "CREATE" | "ALTER") || (prev == "ALTER" && prev2 == "OR");
        let after_dot = idx > 0 && tokens[idx - 1].kind == Kind::Punct('.');
        let before_paren = tokens
            .get(idx + 1)
            .is_some_and(|t| t.kind == Kind::Punct('('));
        if let Some(construct) = construct {
            return Some((construct.to_string(), true));
        }
        let construct = match upper.as_str() {
            "POLICY" if prev == "SECURITY" && matches!(prev2.as_str(), "CREATE" | "ALTER") => {
                Some("row-level security policies")
            }
            "GRANT" | "DENY" | "REVOKE" | "REVERT" => Some("T-SQL permissions (GRANT/DENY/REVOKE)"),
            "USER" | "ROLE" | "LOGIN" if created => Some("database users, roles and logins"),
            "AS" if matches!(prev.as_str(), "EXECUTE" | "EXEC") => Some("EXECUTE AS impersonation"),
            "MASKED" if next == "WITH" => Some("dynamic data masking"),
            "ENCRYPTBYKEY"
            | "DECRYPTBYKEY"
            | "ENCRYPTBYPASSPHRASE"
            | "DECRYPTBYPASSPHRASE"
            | "ENCRYPTBYCERT"
            | "DECRYPTBYCERT"
            | "ENCRYPTBYASYMKEY"
            | "DECRYPTBYASYMKEY"
            | "SIGNBYCERT" => Some("T-SQL encryption functions"),
            "SYMMETRIC" | "ASYMMETRIC" | "CERTIFICATE"
                if next == "KEY"
                    || matches!(prev.as_str(), "CREATE" | "OPEN" | "CLOSE" | "ALTER")
                    || upper == "CERTIFICATE" =>
            {
                Some("encryption keys and certificates")
            }
            "KEY"
                if prev == "MASTER"
                    && matches!(prev2.as_str(), "CREATE" | "OPEN" | "CLOSE" | "ALTER") =>
            {
                Some("encryption keys and certificates")
            }
            "SYSTEM_TIME" | "SYSTEM_VERSIONING" => Some("system-versioned temporal tables"),
            "VALUE" | "QUERY" | "EXIST" | "NODES" | "MODIFY" if after_dot && before_paren => {
                Some("XML data type methods")
            }
            "OPENXML" => Some("OPENXML"),
            "DATABASEPROPERTYEX" if before_paren => Some(
                "DATABASEPROPERTYEX (database properties are not tracked; no value is invented)",
            ),
            "TRIGGER" if created => {
                Some("T-SQL triggers (use SQLite CREATE TRIGGER ... BEGIN ... END)")
            }
            "RULE" | "DEFAULT" if prev == "CREATE" => {
                Some("legacy CREATE RULE / CREATE DEFAULT objects")
            }
            "CONVERSATION" | "DIALOG" => Some("Service Broker"),
            "QUEUE" | "SERVICE" | "CONTRACT" if prev == "CREATE" => Some("Service Broker"),
            "MESSAGE" if prev == "CREATE" && next == "TYPE" => Some("Service Broker"),
            "PERCENT" if (1..=4).any(|back| idx >= back && text(&tokens[idx - back]) == "TOP") => {
                Some("TOP ... PERCENT")
            }
            "TIES" if prev == "WITH" => Some("TOP ... WITH TIES"),
            // SQLite has no GROUPING SETS; expanding them with UNION ALL would
            // need a full SELECT parser, and silently dropping the subtotal
            // rows (what a plain GROUP BY does) returns wrong results.
            "ROLLUP" | "CUBE" if next == "(" && matches!(prev.as_str(), "BY" | ",") => {
                Some("GROUP BY ROLLUP / CUBE")
            }
            "ROLLUP" if prev == "WITH" => Some("GROUP BY ... WITH ROLLUP"),
            "GROUPING" if next == "SETS" => Some("GROUPING SETS"),
            "GROUPING" | "GROUPING_ID" if next == "(" => Some("GROUPING() / GROUPING_ID()"),
            // Evaluated once at DDL time instead of per inserted row.
            "DEFAULT"
                if next == "NEXT" && tokens.get(idx + 2).is_some_and(|t| text(t) == "VALUE") =>
            {
                Some("DEFAULT NEXT VALUE FOR (a sequence default)")
            }
            "SCHEMA" if prev == "CREATE" => {
                Some("CREATE SCHEMA (only the implicit `dbo` schema exists)")
            }
            "STDISTANCE" | "STINTERSECTS" | "STAREA" | "STLENGTH" | "STBUFFER" | "STCONTAINS"
            | "STWITHIN" | "STEQUALS" | "STASBINARY" | "STSRID" | "STGEOMETRYTYPE"
                if after_dot && before_paren =>
            {
                Some("spatial type methods")
            }
            _ if upper.starts_with("FN_")
                && after_dot
                && idx >= 2
                && text(&tokens[idx - 2]) == "SYS" =>
            {
                Some("sys.fn_* system functions")
            }
            "COLLATE" if next.contains("_AI") && !next.contains("_AS") => {
                Some("accent-insensitive collations")
            }
            "REGEXP_MATCHES"
            | "REGEXP_SPLIT_TO_TABLE"
            | "EDIT_DISTANCE"
            | "EDIT_DISTANCE_SIMILARITY"
            | "JARO_WINKLER_DISTANCE"
            | "JARO_WINKLER_SIMILARITY"
            | "VECTOR_DISTANCE"
                if next == "(" =>
            {
                Some(
                    "REGEXP_MATCHES / REGEXP_SPLIT_TO_TABLE / EDIT_DISTANCE / JARO_WINKLER_* / VECTOR_DISTANCE",
                )
            }
            "SWITCHOFFSET" | "TODATETIMEOFFSET" if next == "(" => {
                Some("SWITCHOFFSET / TODATETIMEOFFSET (time-zone offsets are not modelled)")
            }
            "AT" if next == "TIME" && tokens.get(idx + 2).is_some_and(|t| text(t) == "ZONE") => {
                Some("AT TIME ZONE (time zones are not modelled)")
            }
            _ => None,
        };
        if let Some(construct) = construct {
            return Some((construct.to_string(), false));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Executor
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct ErrorInfo {
    number: i64,
    severity: i64,
    state: i64,
    message: String,
}

impl ErrorInfo {
    fn from_error(error: &Error) -> Self {
        match error {
            Error::Raised {
                number,
                severity,
                state,
                message,
            } => Self {
                number: *number,
                severity: *severity,
                state: *state,
                message: message.clone(),
            },
            Error::Sqlite(sqlite) => {
                let message = match sqlite {
                    rusqlite::Error::SqliteFailure(_, Some(message)) => message.clone(),
                    rusqlite::Error::SqlInputError { msg, .. } => msg.clone(),
                    other => other.to_string(),
                };
                let number = match sqlite.sqlite_error_code() {
                    Some(rusqlite::ErrorCode::ConstraintViolation) => {
                        let lower = message.to_ascii_lowercase();
                        if lower.contains("unique") || lower.contains("primary key") {
                            2627
                        } else if lower.contains("not null") {
                            515
                        } else {
                            547
                        }
                    }
                    _ => 50000,
                };
                Self {
                    number,
                    severity: 16,
                    state: 1,
                    message,
                }
            }
            other => Self {
                number: 50000,
                severity: 16,
                state: 1,
                message: other.to_string(),
            },
        }
    }
}

enum Flow {
    Normal,
    Break,
    Continue,
    Return,
    Goto(String),
}

struct Cursor {
    query: String,
    rows: Option<Vec<Vec<Value>>>,
    position: isize,
}

/// Lets the executor issue its own savepoints and internal-table writes past
/// the batch authorizer, which rejects them for user SQL.
pub(crate) struct InternalAccess<'a>(pub(crate) &'a AtomicBool);

struct Executor<'c> {
    conn: &'c Connection,
    internal: &'c AtomicBool,
    spans: MaskedSpans,
    vars: HashMap<String, (Value, Option<String>)>,
    table_vars: HashMap<String, String>,
    cursors: HashMap<String, Cursor>,
    errors: Vec<ErrorInfo>,
    trancount: Vec<String>,
    saved: Vec<String>,
    savepoint_seq: usize,
    rowcount: i64,
    fetch_status: i64,
    last_error: i64,
    temp_tables: Vec<String>,
    depth: usize,
}

/// Executes a script containing T-SQL procedural constructs.
pub(crate) fn execute_script(
    conn: &Connection,
    sql: &str,
    access: &InternalAccess<'_>,
) -> Result<()> {
    let (masked, mut spans) = MaskedSpans::mask(sql);
    let masked = spans.protect_structures(&masked);
    let raw = masked.replace("\r\n", "\n").replace('\r', "\n");
    let mut executor = Executor {
        conn,
        internal: access.0,
        spans,
        vars: HashMap::new(),
        table_vars: HashMap::new(),
        cursors: HashMap::new(),
        errors: Vec::new(),
        trancount: Vec::new(),
        saved: Vec::new(),
        savepoint_seq: 0,
        rowcount: 0,
        fetch_status: -1,
        last_error: 0,
        temp_tables: Vec::new(),
        depth: 0,
    };
    let result = (|| {
        for batch in split_go_batches(&raw) {
            if batch.trim().is_empty() {
                continue;
            }
            executor.run_batch(batch)?;
        }
        if !executor.trancount.is_empty() {
            return Err(Error::Tsql(
                "script ended with an open transaction (BEGIN TRANSACTION without COMMIT or \
                 ROLLBACK); the whole batch was rolled back"
                    .to_string(),
            ));
        }
        Ok(())
    })();
    let cleanup = executor.drop_temp_tables();
    result.and(cleanup)
}

/// `true` when `sql` (unmasked) needs [`execute_script`].
/// Whether the script needs the T-SQL interpreter, and rejects constructs the engine cannot
/// execute at all (the interpreter would otherwise run e.g. `EXECUTE AS` as a
/// silent no-op).
pub(crate) fn classify_script(sql: &str) -> Result<bool> {
    let (masked, mut spans) = MaskedSpans::mask(sql);
    let masked = spans.protect_structures(&masked);
    if !requires_executor(&masked) {
        return Ok(false);
    }
    if let Some((construct, false)) = transpiler_rejection(&masked) {
        return Err(Error::Unsupported(format!(
            "{construct} is not supported by NovaDB"
        )));
    }
    Ok(true)
}

impl Executor<'_> {
    fn run_batch(&mut self, batch: &str) -> Result<()> {
        // Variables, cursors and table variables are batch-scoped.
        self.vars.clear();
        self.table_vars.clear();
        self.cursors.clear();
        let mut parser = Parser::new(batch);
        let statements = parser.parse_list(false)?;
        let flow = self.exec_list(&statements)?;
        if let Flow::Goto(label) = flow {
            return Err(Error::Unsupported(format!(
                "GOTO target `{label}` not found"
            )));
        }
        Ok(())
    }

    fn internal_exec(&self, sql: &str) -> Result<()> {
        self.internal.store(true, Ordering::Release);
        let result = self.conn.execute_batch(sql);
        self.internal.store(false, Ordering::Release);
        result.map_err(Error::from)
    }

    fn exec_list(&mut self, statements: &[Stmt]) -> Result<Flow> {
        let mut idx = 0;
        while idx < statements.len() {
            match self.exec(&statements[idx])? {
                Flow::Normal => idx += 1,
                Flow::Goto(label) => {
                    match statements
                        .iter()
                        .position(|s| matches!(s, Stmt::Label(l) if *l == label))
                    {
                        Some(target) => idx = target + 1,
                        None => return Ok(Flow::Goto(label)),
                    }
                }
                other => return Ok(other),
            }
        }
        Ok(Flow::Normal)
    }

    fn exec(&mut self, stmt: &Stmt) -> Result<Flow> {
        match stmt {
            Stmt::Leaf(text) => {
                self.run_leaf(text)?;
                Ok(Flow::Normal)
            }
            Stmt::Block(list) => self.exec_list(list),
            Stmt::If {
                cond,
                then,
                otherwise,
            } => {
                if self.condition(cond)? {
                    self.exec(then)
                } else if let Some(otherwise) = otherwise {
                    self.exec(otherwise)
                } else {
                    Ok(Flow::Normal)
                }
            }
            Stmt::While { cond, body } => {
                let mut iterations = 0u64;
                while self.condition(cond)? {
                    iterations += 1;
                    if iterations > 10_000_000 {
                        return Err(Error::Tsql(
                            "WHILE loop exceeded 10,000,000 iterations".into(),
                        ));
                    }
                    match self.exec(body)? {
                        Flow::Break => break,
                        Flow::Normal | Flow::Continue => {}
                        other => return Ok(other),
                    }
                }
                Ok(Flow::Normal)
            }
            Stmt::TryCatch { body, handler } => match self.exec_list(body) {
                Ok(flow) => Ok(flow),
                Err(error) if is_catchable(&error) => {
                    let info = ErrorInfo::from_error(&error);
                    self.last_error = info.number;
                    self.errors.push(info);
                    let result = self.exec_list(handler);
                    self.errors.pop();
                    result
                }
                Err(error) => Err(error),
            },
            Stmt::Declare(text) => {
                self.declare(text)?;
                Ok(Flow::Normal)
            }
            Stmt::DeclareCursor { name, query } => {
                self.cursors.insert(
                    name.clone(),
                    Cursor {
                        query: query.clone(),
                        rows: None,
                        position: -1,
                    },
                );
                Ok(Flow::Normal)
            }
            Stmt::SetVar { name, op, expr } => {
                let expr = if op.is_empty() {
                    expr.clone()
                } else {
                    format!("({name}) {op} ({expr})")
                };
                let value = self.eval(&expr)?;
                self.assign(name, value)?;
                Ok(Flow::Normal)
            }
            Stmt::SelectAssign(text) => {
                self.select_assign(text)?;
                Ok(Flow::Normal)
            }
            Stmt::Break => Ok(Flow::Break),
            Stmt::Continue => Ok(Flow::Continue),
            Stmt::Return => Ok(Flow::Return),
            Stmt::Goto(label) => Ok(Flow::Goto(label.clone())),
            Stmt::Label(_) | Stmt::Noop | Stmt::Routine => Ok(Flow::Normal),
            Stmt::Throw(args) => Err(self.throw(args.as_deref())?),
            Stmt::Raiserror(args) => {
                self.raiserror(args)?;
                Ok(Flow::Normal)
            }
            Stmt::BeginTran => {
                self.savepoint_seq += 1;
                let name = format!("tsql_tran_{}", self.savepoint_seq);
                self.internal_exec(&format!("SAVEPOINT {name};"))?;
                self.trancount.push(name);
                Ok(Flow::Normal)
            }
            Stmt::Commit => {
                let Some(name) = self.trancount.pop() else {
                    return Err(Error::TransactionControlNotAllowed);
                };
                if self.trancount.is_empty() {
                    self.saved.clear();
                }
                self.internal_exec(&format!("RELEASE {name};"))?;
                Ok(Flow::Normal)
            }
            Stmt::Rollback(target) => {
                self.rollback(target.as_deref())?;
                Ok(Flow::Normal)
            }
            Stmt::SaveTran(name) => {
                if self.trancount.is_empty() {
                    return Err(Error::TransactionControlNotAllowed);
                }
                let savepoint = format!("tsql_save_{}", sanitize(name));
                self.internal_exec(&format!("SAVEPOINT {savepoint};"))?;
                self.saved.push(savepoint);
                Ok(Flow::Normal)
            }
            Stmt::Open(name) => {
                let query = self
                    .cursors
                    .get(name)
                    .map(|c| c.query.clone())
                    .ok_or_else(|| Error::Tsql(format!("cursor `{name}` does not exist")))?;
                let rows = self.query_rows(&query)?;
                if let Some(cursor) = self.cursors.get_mut(name) {
                    cursor.rows = Some(rows);
                    cursor.position = -1;
                }
                Ok(Flow::Normal)
            }
            Stmt::Fetch {
                direction,
                cursor,
                into,
            } => {
                self.fetch(direction, cursor, into)?;
                Ok(Flow::Normal)
            }
            Stmt::Close(name) => {
                match self.cursors.get_mut(name) {
                    Some(cursor) if cursor.rows.is_some() => cursor.rows = None,
                    _ => return Err(Error::Tsql(format!("cursor `{name}` is not open"))),
                }
                Ok(Flow::Normal)
            }
            Stmt::Deallocate(name) => {
                self.cursors.remove(name);
                Ok(Flow::Normal)
            }
            Stmt::Exec(text) => {
                self.exec_procedure(text)?;
                Ok(Flow::Normal)
            }
            Stmt::WaitFor(text) => {
                self.wait_for(text)?;
                Ok(Flow::Normal)
            }
            Stmt::CreateSequence(text) => {
                self.create_sequence(text)?;
                Ok(Flow::Normal)
            }
            Stmt::AlterSequence(text) => {
                self.alter_sequence(text)?;
                Ok(Flow::Normal)
            }
            Stmt::DropSequence(text) => {
                self.drop_sequence(text)?;
                Ok(Flow::Normal)
            }
            Stmt::CreateType(text) => {
                self.create_type(text)?;
                Ok(Flow::Normal)
            }
            Stmt::DropType(text) => {
                self.drop_type(text)?;
                Ok(Flow::Normal)
            }
        }
    }

    fn rollback(&mut self, target: Option<&str>) -> Result<()> {
        if self.trancount.is_empty() {
            return Err(Error::TransactionControlNotAllowed);
        }
        if let Some(target) = target {
            let savepoint = format!("tsql_save_{}", sanitize(target));
            // ROLLBACK TRAN <savepoint name> keeps the transaction open.
            if let Some(position) = self.saved.iter().position(|s| *s == savepoint) {
                self.saved.truncate(position + 1);
                return self.internal_exec(&format!("ROLLBACK TO {savepoint};"));
            }
        }
        // ROLLBACK [TRAN] undoes everything back to the outermost BEGIN TRAN.
        let outermost = self.trancount[0].clone();
        self.trancount.clear();
        self.saved.clear();
        self.internal_exec(&format!("ROLLBACK TO {outermost}; RELEASE {outermost};"))
    }

    fn drop_temp_tables(&mut self) -> Result<()> {
        for table in std::mem::take(&mut self.temp_tables) {
            self.internal_exec(&format!("DROP TABLE IF EXISTS temp.\"{table}\";"))?;
        }
        Ok(())
    }

    // -- variables --------------------------------------------------------

    fn declare(&mut self, text: &str) -> Result<()> {
        for item in split_top_level_commas(text) {
            let item = item.trim();
            if item.is_empty() {
                continue;
            }
            let Some(name_end) = item.find(|c: char| c.is_whitespace() || c == '=') else {
                return Err(Error::Unsupported(format!(
                    "DECLARE needs a type: `{item}`"
                )));
            };
            let name = &item[..name_end];
            if !name.starts_with('@') {
                return Err(Error::Unsupported(format!("unsupported DECLARE: `{item}`")));
            }
            let rest = item[name_end..].trim_start();
            let rest = rest
                .strip_prefix("AS ")
                .or_else(|| rest.strip_prefix("as "))
                .unwrap_or(rest)
                .trim_start();
            if rest.len() >= 5 && rest[..5].eq_ignore_ascii_case("TABLE") {
                let body = rest[5..].trim();
                self.declare_table(name, body)?;
                continue;
            }
            let (type_text, init) = match find_top_level_eq(rest) {
                Some(eq) => (rest[..eq].trim(), Some(rest[eq + 1..].trim())),
                None => (rest.trim(), None),
            };
            let mut type_name = (!type_text.is_empty()).then(|| type_text.to_string());
            if let Some(definition) = type_name
                .as_deref()
                .map(|t| self.user_type(t))
                .transpose()?
                .flatten()
            {
                if let Some(table) = definition.strip_prefix("TABLE") {
                    if init.is_some() {
                        return Err(Error::Tsql(format!(
                            "table variable {name} cannot have an initializer"
                        )));
                    }
                    self.declare_table(name, table.trim())?;
                    continue;
                }
                type_name = Some(definition.trim_start_matches("FROM").trim().to_string());
            }
            let value = match init {
                Some(expr) => coerce(self.eval(expr)?, type_name.as_deref())?,
                None => Value::Null,
            };
            self.vars
                .insert(name.to_ascii_lowercase(), (value, type_name));
        }
        Ok(())
    }

    fn declare_table(&mut self, name: &str, body: &str) -> Result<()> {
        let table = format!("tsql_tv_{}", sanitize(&name[1..]));
        self.table_vars
            .insert(name.to_ascii_lowercase(), table.clone());
        self.internal_exec(&format!("DROP TABLE IF EXISTS temp.\"{table}\";"))?;
        let create = format!("CREATE TEMP TABLE \"{table}\" {body}");
        let sql = self.transpile(&create)?;
        self.conn.execute_batch(&sql)?;
        self.temp_tables.push(table);
        Ok(())
    }

    fn assign(&mut self, name: &str, value: Value) -> Result<()> {
        let key = name.to_ascii_lowercase();
        let Some((slot, type_name)) = self.vars.get_mut(&key) else {
            return Err(Error::Tsql(format!(
                "Must declare the scalar variable \"{name}\"."
            )));
        };
        *slot = coerce(value, type_name.as_deref())?;
        Ok(())
    }

    fn select_assign(&mut self, text: &str) -> Result<()> {
        // text = "SELECT @a = expr, @b += expr FROM ..."
        let body = text.trim_start()[6..].trim_start();
        let from = find_keyword(body, "FROM");
        let (list, tail) = match from {
            Some(idx) => (&body[..idx], &body[idx..]),
            None => (body, ""),
        };
        let mut targets = Vec::new();
        let mut exprs = Vec::new();
        for item in split_top_level_commas(list) {
            let item = item.trim();
            let Some(eq) = find_top_level_eq(item) else {
                return Err(Error::Unsupported(
                    "SELECT mixing variable assignment and result columns".to_string(),
                ));
            };
            let mut target = item[..eq].trim();
            let mut op = "";
            if let Some(last) = target.chars().last().filter(|c| "+-*/%&|^".contains(*c)) {
                op = &item[eq - 1..eq];
                target = target[..target.len() - last.len_utf8()].trim();
            }
            if !target.starts_with('@') {
                return Err(Error::Unsupported(
                    "SELECT mixing variable assignment and result columns".to_string(),
                ));
            }
            let expr = item[eq + 1..].trim();
            exprs.push(if op.is_empty() {
                expr.to_string()
            } else {
                format!("({target}) {op} ({expr})")
            });
            targets.push(target.to_string());
        }
        let query = format!("SELECT {} {tail}", exprs.join(", "));
        let rows = self.query_rows(&query)?;
        self.rowcount = rows.len() as i64;
        if let Some(row) = rows.last() {
            for (target, value) in targets.iter().zip(row.iter()) {
                self.assign(target, value.clone())?;
            }
        }
        Ok(())
    }

    // -- evaluation -------------------------------------------------------

    /// Replaces variables, T-SQL system functions with run-time values and
    /// `NEXT VALUE FOR` with fresh sequence values.
    fn substitute(&mut self, text: &str) -> Result<String> {
        let mut out = String::with_capacity(text.len());
        let tokens = tokenize(text);
        let mut last = 0;
        let mut idx = 0;
        while idx < tokens.len() {
            let token = tokens[idx];
            let word = &text[token.start..token.end];
            let replacement = match token.kind {
                Kind::Var => {
                    let key = word.to_ascii_lowercase();
                    if let Some(table) = self.table_vars.get(&key) {
                        Some(format!("\"{table}\""))
                    } else if let Some((value, _)) = self.vars.get(&key) {
                        let value = value.clone();
                        Some(self.literal(&value))
                    } else {
                        return Err(Error::Tsql(format!(
                            "Must declare the scalar variable \"{word}\"."
                        )));
                    }
                }
                Kind::SysVar => match word.to_ascii_uppercase().as_str() {
                    "@@ROWCOUNT" => Some(self.rowcount.to_string()),
                    "@@FETCH_STATUS" => Some(self.fetch_status.to_string()),
                    "@@TRANCOUNT" => Some(self.trancount.len().to_string()),
                    "@@ERROR" => Some(self.last_error.to_string()),
                    _ => None,
                },
                Kind::Word => {
                    let upper = word.to_ascii_uppercase();
                    let is_call = |offset: usize| {
                        tokens
                            .get(idx + offset)
                            .is_some_and(|t| t.kind == Kind::Punct('('))
                            && tokens
                                .get(idx + offset + 1)
                                .is_some_and(|t| t.kind == Kind::Punct(')'))
                    };
                    let error_fn = match upper.as_str() {
                        "ERROR_NUMBER" | "ERROR_SEVERITY" | "ERROR_STATE" | "ERROR_MESSAGE"
                        | "ERROR_LINE" | "ERROR_PROCEDURE"
                            if is_call(1) =>
                        {
                            let info = self.errors.last().cloned();
                            let value = match (upper.as_str(), info) {
                                (_, None) | ("ERROR_PROCEDURE", _) => Value::Null,
                                ("ERROR_NUMBER", Some(i)) => Value::Integer(i.number),
                                ("ERROR_SEVERITY", Some(i)) => Value::Integer(i.severity),
                                ("ERROR_STATE", Some(i)) => Value::Integer(i.state),
                                ("ERROR_LINE", Some(_)) => Value::Integer(1),
                                (_, Some(i)) => Value::Text(i.message),
                            };
                            Some((self.literal(&value), 3))
                        }
                        "XACT_STATE" if is_call(1) => {
                            Some((i64::from(!self.trancount.is_empty()).to_string(), 3))
                        }
                        "OBJECT_ID" => self.object_id_call(text, &tokens, idx)?,
                        "TYPE_ID" => self.type_id_call(text, &tokens, idx)?,
                        "NEXT"
                            if tokens.get(idx + 1).is_some_and(|t| {
                                text[t.start..t.end].eq_ignore_ascii_case("VALUE")
                            }) && tokens.get(idx + 2).is_some_and(|t| {
                                text[t.start..t.end].eq_ignore_ascii_case("FOR")
                            }) =>
                        {
                            let mut end = idx + 3;
                            let mut name = String::new();
                            while let Some(t) = tokens.get(end) {
                                let part = &text[t.start..t.end];
                                if t.kind == Kind::Punct('.')
                                    || (name.is_empty() || name.ends_with('.'))
                                        && matches!(t.kind, Kind::Word | Kind::Literal)
                                {
                                    name.push_str(part);
                                    end += 1;
                                } else {
                                    break;
                                }
                            }
                            if tokens
                                .get(end)
                                .is_some_and(|t| text[t.start..t.end].eq_ignore_ascii_case("OVER"))
                            {
                                return Err(Error::Unsupported(
                                    "NEXT VALUE FOR ... OVER".to_string(),
                                ));
                            }
                            if tokens.iter().any(|t| {
                                t.kind == Kind::Word
                                    && text[t.start..t.end].eq_ignore_ascii_case("FROM")
                            }) {
                                return Err(Error::Unsupported(
                                    "NEXT VALUE FOR in a statement that reads rows (one value per row is not supported)"
                                        .into(),
                                ));
                            }
                            let value = self.next_sequence_value(&name)?;
                            Some((value.to_string(), end - idx))
                        }
                        _ => None,
                    };
                    if let Some((replacement, consumed)) = error_fn {
                        out.push_str(&text[last..token.start]);
                        out.push_str(&replacement);
                        let end_token = tokens[idx + consumed - 1];
                        last = end_token.end;
                        idx += consumed;
                        continue;
                    }
                    None
                }
                _ => None,
            };
            if let Some(replacement) = replacement {
                out.push_str(&text[last..token.start]);
                out.push_str(&replacement);
                last = token.end;
            }
            idx += 1;
        }
        out.push_str(&text[last..]);
        Ok(out)
    }

    /// `OBJECT_ID('name' [, 'type'])` resolved against the real schema.
    fn object_id_call(
        &mut self,
        text: &str,
        tokens: &[Token],
        idx: usize,
    ) -> Result<Option<(String, usize)>> {
        let token = |offset: usize| tokens.get(idx + offset);
        if !token(1).is_some_and(|t| t.kind == Kind::Punct('(')) {
            return Ok(None);
        }
        let Some(name_token) = token(2).filter(|t| t.kind == Kind::Literal) else {
            return Ok(None);
        };
        let mut consumed = 3;
        let mut kind_filter = None;
        if token(3).is_some_and(|t| t.kind == Kind::Punct(',')) {
            let Some(kind) = token(4).filter(|t| t.kind == Kind::Literal) else {
                return Ok(None);
            };
            kind_filter = self
                .spans
                .literal_text(&text[kind.start..kind.end])
                .map(str::to_string);
            consumed = 5;
        }
        if !token(consumed).is_some_and(|t| t.kind == Kind::Punct(')')) {
            return Ok(None);
        }
        let Some(raw_name) = self
            .spans
            .literal_text(&text[name_token.start..name_token.end])
            .map(str::to_string)
        else {
            return Ok(None);
        };
        let mut name = raw_name
            .rsplit('.')
            .next()
            .unwrap_or(&raw_name)
            .trim_matches(|c| c == '[' || c == ']' || c == '"')
            .to_string();
        if let Some(temp) = name.strip_prefix("##").or_else(|| name.strip_prefix('#')) {
            name = format!("temp_{temp}");
        }
        let types: &[&str] = match kind_filter
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_uppercase)
            .as_deref()
        {
            Some("U") => &["table"],
            Some("V") => &["view"],
            Some("TR") => &["trigger"],
            Some("P" | "FN" | "IF" | "TF" | "SO") => &[],
            _ => &["table", "view", "trigger", "index"],
        };
        let mut id: Option<i64> = None;
        for schema in ["main", "temp"] {
            for kind in types {
                let found = self.conn.query_row(
                    &format!("SELECT rowid FROM {schema}.sqlite_schema WHERE type = ?1 AND name = ?2 COLLATE NOCASE"),
                    rusqlite::params![kind, name],
                    |row| row.get::<_, i64>(0),
                );
                if let Ok(rowid) = found {
                    id = Some(if schema == "temp" { -rowid } else { rowid });
                }
            }
        }
        let literal = id.map_or("NULL".to_string(), |id| id.to_string());
        Ok(Some((literal, consumed + 1)))
    }

    /// `TYPE_ID('name')`: built-in types and user types created with
    /// `CREATE TYPE` resolve, anything else is NULL.
    fn type_id_call(
        &mut self,
        text: &str,
        tokens: &[Token],
        idx: usize,
    ) -> Result<Option<(String, usize)>> {
        let token = |offset: usize| tokens.get(idx + offset);
        let (Some(open), Some(name), Some(close)) = (token(1), token(2), token(3)) else {
            return Ok(None);
        };
        if open.kind != Kind::Punct('(')
            || name.kind != Kind::Literal
            || close.kind != Kind::Punct(')')
        {
            return Ok(None);
        }
        let Some(raw) = self
            .spans
            .literal_text(&text[name.start..name.end])
            .map(str::to_string)
        else {
            return Ok(None);
        };
        const BUILTIN: &[&str] = &[
            "bigint",
            "binary",
            "bit",
            "char",
            "date",
            "datetime",
            "datetime2",
            "datetimeoffset",
            "decimal",
            "float",
            "geography",
            "geometry",
            "hierarchyid",
            "image",
            "int",
            "money",
            "nchar",
            "ntext",
            "numeric",
            "nvarchar",
            "real",
            "smalldatetime",
            "smallint",
            "smallmoney",
            "sql_variant",
            "sysname",
            "text",
            "time",
            "timestamp",
            "tinyint",
            "uniqueidentifier",
            "varbinary",
            "varchar",
            "xml",
        ];
        let name = self.object_name(&raw);
        let id = if let Some(position) = BUILTIN.iter().position(|b| *b == name) {
            Some(position as i64 + 34)
        } else if self.user_type(&raw)?.is_some() {
            Some(self.conn.query_row(
                "SELECT rowid + 256 FROM _novadb_types WHERE name = ?1",
                [&name],
                |row| row.get::<_, i64>(0),
            )?)
        } else {
            None
        };
        Ok(Some((
            id.map_or("NULL".to_string(), |id| id.to_string()),
            4,
        )))
    }

    /// Executes INSERT/UPDATE/DELETE/MERGE carrying a T-SQL `OUTPUT` clause.
    fn run_dml_with_output(&mut self, text: &str, output: OutputClause) -> Result<()> {
        let dml = format!("{} {}", &text[..output.start], &text[output.end..]);
        let Some((target, columns)) = output.into.clone() else {
            // OUTPUT without INTO only returns a result set, which a batch
            // discards.
            return self.run_leaf(&dml);
        };
        let verb = top_level_words(text)
            .first()
            .map(|w| w.2.clone())
            .unwrap_or_default();
        if verb == "MERGE" {
            return Err(Error::Unsupported("MERGE ... OUTPUT ... INTO".into()));
        }
        // Classify every OUTPUT expression.
        let mut items = Vec::new(); // (source, expr): source = "inserted" | "deleted"
        for item in split_top_level_commas(&output.exprs) {
            let item = strip_output_alias(item.trim());
            let lower = item.to_ascii_lowercase();
            let uses_inserted = lower.contains("inserted.");
            let uses_deleted = lower.contains("deleted.");
            if lower.contains("$action") {
                return Err(Error::Unsupported("OUTPUT $action".into()));
            }
            let source = match (uses_inserted, uses_deleted) {
                (true, false) => "inserted",
                (false, true) => "deleted",
                (false, false) => {
                    if verb == "DELETE" {
                        "deleted"
                    } else {
                        "inserted"
                    }
                }
                (true, true) => {
                    return Err(Error::Unsupported(
                        "OUTPUT expression mixing inserted and deleted".into(),
                    ));
                }
            };
            if (verb == "INSERT" && source == "deleted")
                || (verb == "DELETE" && source == "inserted")
            {
                return Err(Error::Tsql(format!(
                    "OUTPUT {source}.* is not available for {verb}"
                )));
            }
            let expr = strip_pseudo_table(&item, source);
            items.push((source, expr));
        }
        let returning: Vec<String> = items
            .iter()
            .filter(|(source, _)| verb != "UPDATE" || *source == "inserted")
            .map(|(_, expr)| expr.clone())
            .collect();
        let needs_old = verb == "UPDATE" && items.iter().any(|(source, _)| *source == "deleted");
        let base_sql = self.transpile(&format!("{dml};"))?;
        let base_sql = base_sql.trim().trim_end_matches(';').to_string();

        let mut old_rows: HashMap<i64, Vec<Value>> = HashMap::new();
        let mut old_exprs = Vec::new();
        let mut returning_sql = returning.clone();
        if needs_old {
            let words = top_level_words(&base_sql);
            let set = words
                .iter()
                .find(|w| w.2 == "SET")
                .ok_or_else(|| Error::Unsupported("UPDATE without SET".into()))?;
            let target_text = base_sql[words[0].1..set.0].trim().to_string();
            let (table, alias) = split_alias(&target_text);
            let alias = alias.unwrap_or_else(|| table.clone());
            let from = words.iter().find(|w| w.2 == "FROM" && w.0 > set.0);
            let where_ = words.iter().find(|w| w.2 == "WHERE" && w.0 > set.0);
            let mut select = format!("SELECT {alias}.rowid");
            for (source, expr) in &items {
                if *source == "deleted" {
                    let qualified = qualify_columns(expr, &alias);
                    old_exprs.push(qualified.clone());
                    select.push_str(", ");
                    select.push_str(&qualified);
                }
            }
            select.push_str(&format!(" FROM {table} AS {alias}"));
            if let Some(from) = from {
                let end = where_.map_or(base_sql.len(), |w| w.0);
                select.push_str(", ");
                select.push_str(base_sql[from.1..end].trim());
            }
            if let Some(where_) = where_ {
                select.push_str(" WHERE ");
                select.push_str(&base_sql[where_.1..]);
            }
            for row in self.sqlite_rows(&select, &[])? {
                if let Some(Value::Integer(rowid)) = row.first() {
                    old_rows.insert(*rowid, row[1..].to_vec());
                }
            }
            returning_sql.insert(0, "rowid".to_string());
        }
        let returning_text = if returning_sql.is_empty() {
            "1".to_string()
        } else {
            returning_sql.join(", ")
        };
        let returned = self.sqlite_rows(&format!("{base_sql} RETURNING {returning_text}"), &[])?;
        self.rowcount = returned.len() as i64;

        // Assemble output rows in the original expression order.
        let mut output_rows = Vec::new();
        for row in returned {
            let (old, new) = if needs_old {
                let rowid = match row.first() {
                    Some(Value::Integer(rowid)) => *rowid,
                    _ => {
                        return Err(Error::Unsupported(
                            "OUTPUT deleted.* on a WITHOUT ROWID table".into(),
                        ));
                    }
                };
                (
                    old_rows.get(&rowid).cloned().unwrap_or_default(),
                    row[1..].to_vec(),
                )
            } else {
                (Vec::new(), row)
            };
            let (mut old_iter, mut new_iter) = (old.into_iter(), new.into_iter());
            let values: Vec<Value> = items
                .iter()
                .map(|(source, _)| {
                    if needs_old && *source == "deleted" {
                        old_iter.next().unwrap_or(Value::Null)
                    } else {
                        new_iter.next().unwrap_or(Value::Null)
                    }
                })
                .collect();
            output_rows.push(values);
        }
        let table = match self.table_vars.get(&target.to_ascii_lowercase()) {
            Some(table) => format!("\"{table}\""),
            None if target.starts_with('@') => {
                return Err(Error::Tsql(format!(
                    "Must declare the table variable \"{target}\"."
                )));
            }
            None => self.spans.unmask(&target),
        };
        let columns = columns.map_or(String::new(), |c| self.spans.unmask(&c));
        for values in output_rows {
            let placeholders: Vec<String> = (1..=values.len()).map(|i| format!("?{i}")).collect();
            let insert = format!(
                "INSERT INTO {table} {columns} VALUES ({})",
                placeholders.join(", ")
            );
            self.conn
                .execute(&insert, rusqlite::params_from_iter(values.iter()))?;
        }
        Ok(())
    }

    fn literal(&mut self, value: &Value) -> String {
        match value {
            Value::Null => "NULL".to_string(),
            Value::Integer(int) if *int < 0 => format!("({int})"),
            Value::Integer(int) => int.to_string(),
            Value::Real(real) if real.is_finite() => {
                let text = format!("{real:?}");
                if *real < 0.0 {
                    format!("({text})")
                } else {
                    text
                }
            }
            Value::Real(_) => "NULL".to_string(),
            Value::Text(text) => {
                format!("'{}'", self.spans.push_literal(&text.replace('\'', "''")))
            }
            Value::Blob(bytes) => {
                let hex: String = bytes.iter().map(|b| format!("{b:02X}")).collect();
                format!("X'{hex}'")
            }
        }
    }

    fn transpile(&mut self, masked: &str) -> Result<String> {
        let substituted = self.substitute(masked)?;
        crate::reject_unsupported_statements(&substituted)?;
        let normalized = normalize_single_batch(&substituted, &self.spans)?;
        Ok(self.spans.unmask(&normalized))
    }

    fn run_leaf(&mut self, text: &str) -> Result<()> {
        if let Some(output) = extract_output(text)? {
            return self.run_dml_with_output(text, output);
        }
        // Many transpiler passes expect statements to be `;`-terminated.
        let sql = self.transpile(&format!("{text};"))?;
        if is_blank_sql(&sql) {
            return Ok(());
        }
        let result = self.conn.execute_batch(&sql);
        match result {
            Ok(()) => {
                self.rowcount = self.conn.changes() as i64;
                self.last_error = 0;
                Ok(())
            }
            Err(error) => {
                let error = Error::from(error);
                self.last_error = ErrorInfo::from_error(&error).number;
                Err(error)
            }
        }
    }

    fn query_rows(&mut self, masked_query: &str) -> Result<Vec<Vec<Value>>> {
        let sql = self.transpile(&format!("{masked_query};"))?;
        self.sqlite_rows(&sql, &[])
    }

    fn sqlite_rows(&self, sql: &str, params: &[Value]) -> Result<Vec<Vec<Value>>> {
        let sql = sql.trim().trim_end_matches(';');
        let mut statement = self.conn.prepare(sql)?;
        let columns = statement.column_count();
        let rows = statement.query_map(rusqlite::params_from_iter(params.iter()), |row| {
            (0..columns)
                .map(|idx| row.get::<_, Value>(idx))
                .collect::<rusqlite::Result<Vec<_>>>()
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn eval(&mut self, expr: &str) -> Result<Value> {
        let rows = self.query_rows(&format!("SELECT {expr}"))?;
        Ok(rows
            .into_iter()
            .next()
            .and_then(|row| row.into_iter().next())
            .unwrap_or(Value::Null))
    }

    fn condition(&mut self, cond: &str) -> Result<bool> {
        let value = self.eval(&format!("CASE WHEN ({cond}) THEN 1 ELSE 0 END"))?;
        Ok(matches!(value, Value::Integer(1)))
    }

    // -- errors -----------------------------------------------------------

    fn throw(&mut self, args: Option<&str>) -> Result<Error> {
        let Some(args) = args else {
            return Ok(match self.errors.last() {
                Some(info) => Error::Raised {
                    number: info.number,
                    severity: info.severity,
                    state: info.state,
                    message: info.message.clone(),
                },
                None => Error::Raised {
                    number: 10704,
                    severity: 16,
                    state: 1,
                    message: "THROW without parameters must be used inside a CATCH block"
                        .to_string(),
                },
            });
        };
        let parts = split_top_level_commas(args);
        if parts.len() != 3 {
            return Err(Error::Unsupported(format!(
                "THROW expects 3 arguments: `{args}`"
            )));
        }
        let number = self.eval(parts[0])?;
        let message = self.eval(parts[1])?;
        let state = self.eval(parts[2])?;
        Ok(Error::Raised {
            number: as_i64(&number).unwrap_or(50000),
            severity: 16,
            state: as_i64(&state).unwrap_or(1),
            message: value_to_string(&message),
        })
    }

    fn raiserror(&mut self, text: &str) -> Result<()> {
        let text = text.trim();
        let open = text
            .find('(')
            .ok_or_else(|| Error::Unsupported("RAISERROR without arguments".into()))?;
        let close = matching_paren(text, open)
            .ok_or_else(|| Error::Unsupported("RAISERROR: unbalanced".into()))?;
        let args = split_top_level_commas(&text[open + 1..close]);
        if args.len() < 3 {
            return Err(Error::Unsupported(
                "RAISERROR expects (message, severity, state, ...)".into(),
            ));
        }
        let message = self.eval(args[0])?;
        let severity = as_i64(&self.eval(args[1])?).unwrap_or(16);
        let state = as_i64(&self.eval(args[2])?).unwrap_or(1);
        let mut values = Vec::new();
        for arg in &args[3..] {
            values.push(self.eval(arg)?);
        }
        let (number, message) = match message {
            Value::Integer(id) => (id, format!("message {id}")),
            other => (50000, printf(&value_to_string(&other), &values)),
        };
        if severity >= 11 {
            return Err(Error::Raised {
                number,
                severity,
                state,
                message,
            });
        }
        Ok(())
    }

    // -- cursors ----------------------------------------------------------

    fn fetch(&mut self, direction: &str, name: &str, into: &[String]) -> Result<()> {
        let cursor = self
            .cursors
            .get_mut(name)
            .ok_or_else(|| Error::Tsql(format!("cursor `{name}` does not exist")))?;
        let Some(rows) = cursor.rows.as_ref() else {
            return Err(Error::Tsql(format!("cursor `{name}` is not open")));
        };
        let len = rows.len() as isize;
        cursor.position = match direction {
            "NEXT" => cursor.position + 1,
            "PRIOR" => cursor.position - 1,
            "FIRST" => 0,
            "LAST" => len - 1,
            _ => cursor.position + 1,
        };
        let row = (0..len)
            .contains(&cursor.position)
            .then(|| rows[cursor.position as usize].clone());
        cursor.position = cursor.position.clamp(-1, len);
        match row {
            Some(row) => {
                self.fetch_status = 0;
                if !into.is_empty() && into.len() != row.len() {
                    return Err(Error::Tsql(format!(
                        "FETCH INTO lists {} variables but cursor `{name}` returns {} columns",
                        into.len(),
                        row.len()
                    )));
                }
                for (target, value) in into.iter().zip(row) {
                    self.assign(target, value)?;
                }
            }
            None => self.fetch_status = -1,
        }
        Ok(())
    }

    // -- EXEC ---------------------------------------------------------------

    fn exec_procedure(&mut self, text: &str) -> Result<()> {
        let text = text.trim();
        if text.starts_with('(') {
            let close = matching_paren(text, 0)
                .ok_or_else(|| Error::Unsupported("EXEC: unbalanced".into()))?;
            let sql = value_to_string(&self.eval(&text[1..close])?);
            return self.run_dynamic(&sql, Vec::new()).map(|_| ());
        }
        // `EXEC @ret = proc ...`
        let (ret_var, call) = match text.find('=') {
            Some(eq)
                if text.starts_with('@')
                    && text[..eq]
                        .trim()
                        .chars()
                        .all(|c| is_word_char(c) || c == '@') =>
            {
                (Some(text[..eq].trim().to_string()), text[eq + 1..].trim())
            }
            _ => (None, text),
        };
        let name_end = call
            .find(|c: char| c.is_whitespace() || c == ';')
            .unwrap_or(call.len());
        let name = &call[..name_end];
        let args = call[name_end..].trim();
        let bare = name.rsplit('.').next().unwrap_or(name).to_ascii_lowercase();
        if bare == "sp_executesql" {
            return self.sp_executesql(args);
        }
        if name.starts_with('@') {
            // EXEC @procedure_name_variable
            return Err(Error::Unsupported(
                "EXEC of a procedure name stored in a variable".into(),
            ));
        }
        if bare == "sp_set_session_context" {
            // sp_set_session_context [@key =] k, [@value =] v [, [@read_only =] ro]
            let mut parts = split_top_level_commas(args)
                .into_iter()
                .map(|part| {
                    let part = part.trim();
                    match part.split_once('=') {
                        Some((name, value)) if name.trim().starts_with('@') => value.trim(),
                        _ => part,
                    }
                    .to_string()
                })
                .collect::<Vec<_>>();
            if parts.len() < 2 || parts.len() > 3 {
                return Err(Error::Unsupported(
                    "sp_set_session_context expects @key, @value[, @read_only]".into(),
                ));
            }
            let read_only = if parts.len() == 3 { parts.pop() } else { None };
            let value = parts.pop().unwrap_or_default();
            let key = parts.pop().unwrap_or_default();
            let call = match read_only {
                Some(read_only) => format!("tsql_set_session_context({key}, {value}, {read_only})"),
                None => format!("tsql_set_session_context({key}, {value})"),
            };
            let result = self.eval(&call)?;
            if let Some(ret) = ret_var {
                self.assign(&ret, Value::Integer(0))?;
            }
            drop(result);
            return Ok(());
        }
        // Stored procedures are recorded, not executed (NovaDB has no
        // procedure runtime); the call is an explicit no-op.
        if let Some(ret) = ret_var {
            self.assign(&ret, Value::Integer(0))?;
        }
        Ok(())
    }

    fn sp_executesql(&mut self, args: &str) -> Result<()> {
        let parts = split_top_level_commas(args);
        let Some(stmt_expr) = parts.first().map(|p| p.trim()).filter(|p| !p.is_empty()) else {
            return Err(Error::Unsupported("sp_executesql without statement".into()));
        };
        let stmt_expr = stmt_expr
            .strip_prefix("@stmt")
            .map_or(stmt_expr, |r| r.trim_start().trim_start_matches('=').trim());
        let sql = value_to_string(&self.eval(stmt_expr)?);
        let mut params: Vec<(String, Option<String>, bool)> = Vec::new();
        if let Some(definition) = parts.get(1) {
            let definition = value_to_string(&self.eval(definition.trim())?);
            for param in definition.split(',') {
                let param = param.trim();
                if param.is_empty() {
                    continue;
                }
                let mut words = param.split_whitespace().collect::<Vec<_>>();
                let output = words.last().is_some_and(|w| {
                    w.eq_ignore_ascii_case("OUTPUT") || w.eq_ignore_ascii_case("OUT")
                });
                if output {
                    words.pop();
                }
                let name = words.first().copied().unwrap_or_default().to_string();
                let type_name = (words.len() > 1).then(|| words[1..].join(" "));
                params.push((name, type_name, output));
            }
        }
        let mut bindings: Vec<(String, Value, Option<String>)> = Vec::new();
        let mut outputs: Vec<(String, String)> = Vec::new();
        for (position, arg) in parts.iter().skip(2).enumerate() {
            let mut arg = arg.trim();
            let is_output = arg.to_ascii_uppercase().ends_with(" OUTPUT")
                || arg.to_ascii_uppercase().ends_with(" OUT");
            if is_output {
                arg = arg[..arg.rfind(char::is_whitespace).unwrap_or(arg.len())].trim();
            }
            let (param_name, expr) = match find_top_level_eq(arg) {
                Some(eq) if arg.starts_with('@') => {
                    (arg[..eq].trim().to_string(), arg[eq + 1..].trim())
                }
                _ => (
                    params
                        .get(position)
                        .map(|p| p.0.clone())
                        .unwrap_or_default(),
                    arg,
                ),
            };
            let Some((_, type_name, _)) = params
                .iter()
                .find(|p| p.0.eq_ignore_ascii_case(&param_name))
            else {
                return Err(Error::Tsql(format!(
                    "sp_executesql: unknown parameter `{param_name}`"
                )));
            };
            let value = self.eval(expr)?;
            if is_output {
                outputs.push((param_name.clone(), expr.to_string()));
            }
            bindings.push((param_name, value, type_name.clone()));
        }
        for (name, type_name, _) in &params {
            if !bindings.iter().any(|b| b.0.eq_ignore_ascii_case(name)) {
                bindings.push((name.clone(), Value::Null, type_name.clone()));
            }
        }
        let result_vars = self.run_dynamic(&sql, bindings)?;
        for (param, target) in outputs {
            if let Some((value, _)) = result_vars.get(&param.to_ascii_lowercase()) {
                self.assign(&target, value.clone())?;
            }
        }
        Ok(())
    }

    /// Runs dynamic SQL in its own variable scope.
    fn run_dynamic(
        &mut self,
        sql: &str,
        bindings: Vec<(String, Value, Option<String>)>,
    ) -> Result<HashMap<String, (Value, Option<String>)>> {
        if self.depth > 32 {
            return Err(Error::Tsql("dynamic SQL nested too deeply".into()));
        }
        let masked = self.spans.mask_into(sql);
        let masked = self.spans.protect_structures(&masked);
        let mut scope = HashMap::new();
        for (name, value, type_name) in bindings {
            let value = coerce(value, type_name.as_deref())?;
            scope.insert(name.to_ascii_lowercase(), (value, type_name));
        }
        let saved_vars = std::mem::replace(&mut self.vars, scope);
        let saved_tables = std::mem::take(&mut self.table_vars);
        let saved_cursors = std::mem::take(&mut self.cursors);
        self.depth += 1;
        let result = (|| {
            let statements = Parser::new(&masked).parse_list(false)?;
            match self.exec_list(&statements)? {
                Flow::Goto(label) => Err(Error::Unsupported(format!(
                    "GOTO target `{label}` not found"
                ))),
                _ => Ok(()),
            }
        })();
        self.depth -= 1;
        let vars = std::mem::replace(&mut self.vars, saved_vars);
        self.table_vars = saved_tables;
        self.cursors = saved_cursors;
        result.map(|()| vars)
    }

    fn wait_for(&mut self, text: &str) -> Result<()> {
        let mut words = text.split_whitespace();
        let kind = words.next().unwrap_or_default().to_ascii_uppercase();
        let expr: String = words.collect::<Vec<_>>().join(" ");
        if kind != "DELAY" {
            return Err(Error::Unsupported("WAITFOR TIME".into()));
        }
        let value = value_to_string(&self.eval(&expr)?);
        let mut seconds = 0f64;
        for (idx, part) in value.split(':').rev().enumerate() {
            let number: f64 = part
                .trim()
                .parse()
                .map_err(|_| Error::Tsql(format!("invalid WAITFOR DELAY `{value}`")))?;
            seconds += number * 60f64.powi(idx as i32);
        }
        if seconds > 5.0 {
            return Err(Error::Unsupported(
                "WAITFOR DELAY longer than 5 seconds".into(),
            ));
        }
        std::thread::sleep(std::time::Duration::from_secs_f64(seconds.max(0.0)));
        Ok(())
    }

    // -- user-defined types ----------------------------------------------

    fn ensure_type_table(&self) -> Result<()> {
        self.internal_exec(
            "CREATE TABLE IF NOT EXISTS _novadb_types ( \
                 name TEXT PRIMARY KEY COLLATE NOCASE, \
                 definition TEXT NOT NULL \
             );",
        )
    }

    fn object_name(&self, raw: &str) -> String {
        self.sequence_name(raw)
    }

    /// Definition of a user type: `TABLE (...)` (unmasked SQL) or `FROM base`.
    fn user_type(&mut self, raw: &str) -> Result<Option<String>> {
        let name = self.object_name(raw);
        if name.is_empty() || name.contains(char::is_whitespace) {
            return Ok(None);
        }
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_schema WHERE name = '_novadb_types')",
            [],
            |row| row.get(0),
        )?;
        if !exists {
            return Ok(None);
        }
        let definition = self.conn.query_row(
            "SELECT definition FROM _novadb_types WHERE name = ?1",
            [&name],
            |row| row.get::<_, String>(0),
        );
        match definition {
            Ok(definition) => Ok(Some(self.spans.mask_into(&definition))),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn create_type(&mut self, text: &str) -> Result<()> {
        // CREATE TYPE name AS TABLE (...) | CREATE TYPE name FROM base [NULL | NOT NULL]
        let words = top_level_words(text);
        let name_word = words
            .get(2)
            .ok_or_else(|| Error::Unsupported("CREATE TYPE without name".into()))?;
        // The name may be schema-qualified (`dbo.T` is several tokens).
        let after_name = words
            .iter()
            .skip(3)
            .find(|w| w.2 == "AS" || w.2 == "FROM")
            .ok_or_else(|| {
                Error::Unsupported(format!(
                    "unsupported CREATE TYPE: {}",
                    self.spans.unmask(text)
                ))
            })?;
        let name = self.object_name(&text[name_word.0..after_name.0]);
        let definition = if after_name.2 == "AS" {
            let table = words
                .iter()
                .find(|w| w.0 > after_name.0 && w.2 == "TABLE")
                .ok_or_else(|| Error::Unsupported("CREATE TYPE ... AS must be AS TABLE".into()))?;
            format!("TABLE {}", self.spans.unmask(text[table.1..].trim()))
        } else {
            let base = text[after_name.1..].trim();
            let base = base
                .trim_end_matches(|c: char| c.is_whitespace())
                .trim_end_matches("NOT NULL")
                .trim_end_matches("not null")
                .trim_end_matches("NULL")
                .trim();
            format!("FROM {}", self.spans.unmask(base))
        };
        self.ensure_type_table()?;
        self.internal.store(true, Ordering::Release);
        let inserted = self.conn.execute(
            "INSERT INTO _novadb_types(name, definition) VALUES (?1, ?2)",
            rusqlite::params![name, definition],
        );
        self.internal.store(false, Ordering::Release);
        match inserted {
            Ok(_) => Ok(()),
            Err(error)
                if error.sqlite_error_code() == Some(rusqlite::ErrorCode::ConstraintViolation) =>
            {
                Err(Error::Tsql(format!(
                    "There is already an object named '{name}' in the database."
                )))
            }
            Err(error) => Err(error.into()),
        }
    }

    fn drop_type(&mut self, text: &str) -> Result<()> {
        let words: Vec<&str> = text.split_whitespace().collect();
        let if_exists = words.get(2).is_some_and(|w| w.eq_ignore_ascii_case("IF"));
        let raw = if if_exists {
            words[4..].join(" ")
        } else {
            words[2..].join(" ")
        };
        let name = self.object_name(&raw);
        self.ensure_type_table()?;
        self.internal.store(true, Ordering::Release);
        let changed = self
            .conn
            .execute("DELETE FROM _novadb_types WHERE name = ?1", [&name]);
        self.internal.store(false, Ordering::Release);
        if changed? == 0 && !if_exists {
            return Err(Error::Tsql(format!(
                "Cannot drop the type '{name}', because it does not exist."
            )));
        }
        Ok(())
    }

    // -- sequences --------------------------------------------------------

    fn ensure_sequence_table(&self) -> Result<()> {
        self.internal_exec(
            "CREATE TABLE IF NOT EXISTS _novadb_sequences ( \
                 name TEXT PRIMARY KEY COLLATE NOCASE, \
                 current INTEGER, \
                 start_value INTEGER NOT NULL, \
                 increment INTEGER NOT NULL, \
                 min_value INTEGER NOT NULL, \
                 max_value INTEGER NOT NULL, \
                 cycle INTEGER NOT NULL \
             );",
        )
    }

    fn sequence_name(&self, raw: &str) -> String {
        let raw = raw.trim();
        let last = raw.rsplit('.').next().unwrap_or(raw).trim();
        let unquoted = if (last.starts_with('[') && last.ends_with(']'))
            || (last.starts_with('"') && last.ends_with('"'))
        {
            let inner = &last[1..last.len() - 1];
            self.spans
                .literal_text(&format!("'{inner}'"))
                .map_or(inner.to_string(), str::to_string)
        } else {
            last.to_string()
        };
        unquoted.to_ascii_lowercase()
    }

    fn create_sequence(&mut self, text: &str) -> Result<()> {
        // CREATE SEQUENCE name [AS type] [START WITH n] [INCREMENT BY n]
        // [MINVALUE n | NO MINVALUE] [MAXVALUE n | NO MAXVALUE] [[NO] CYCLE] [CACHE n | NO CACHE]
        let words: Vec<String> = text.split_whitespace().map(str::to_string).collect();
        let name = self.sequence_name(words.get(2).map_or("", String::as_str));
        let upper: Vec<String> = words.iter().map(|w| w.to_ascii_uppercase()).collect();
        let number = |idx: usize| -> Result<i64> {
            words
                .get(idx)
                .and_then(|w| w.trim_end_matches(';').parse::<i64>().ok())
                .ok_or_else(|| {
                    Error::Unsupported(format!("CREATE SEQUENCE: expected a number in `{text}`"))
                })
        };
        let (mut min, mut max) = (i64::MIN, i64::MAX);
        let (mut start, mut increment, mut cycle) = (None, 1i64, false);
        let mut idx = 3;
        while idx < upper.len() {
            match upper[idx].as_str() {
                "AS" => {
                    let type_name = upper.get(idx + 1).cloned().unwrap_or_default();
                    (min, max) = match type_name.as_str() {
                        "TINYINT" => (0, 255),
                        "SMALLINT" => (i64::from(i16::MIN), i64::from(i16::MAX)),
                        "INT" | "INTEGER" => (i64::from(i32::MIN), i64::from(i32::MAX)),
                        "BIGINT" => (i64::MIN, i64::MAX),
                        other if other.starts_with("DECIMAL") || other.starts_with("NUMERIC") => {
                            (i64::MIN, i64::MAX)
                        }
                        other => return Err(Error::Unsupported(format!("sequence type {other}"))),
                    };
                    idx += 2;
                }
                "START" => {
                    start = Some(number(idx + 2)?);
                    idx += 3;
                }
                "INCREMENT" => {
                    increment = number(idx + 2)?;
                    idx += 3;
                }
                "MINVALUE" => {
                    min = number(idx + 1)?;
                    idx += 2;
                }
                "MAXVALUE" => {
                    max = number(idx + 1)?;
                    idx += 2;
                }
                "NO" => idx += 2,
                "CYCLE" => {
                    cycle = true;
                    idx += 1;
                }
                "CACHE" => {
                    idx += if upper.get(idx + 1).is_some_and(|w| w.parse::<i64>().is_ok()) {
                        2
                    } else {
                        1
                    }
                }
                other => {
                    return Err(Error::Unsupported(format!(
                        "CREATE SEQUENCE option `{other}`"
                    )));
                }
            }
        }
        if increment == 0 {
            return Err(Error::Tsql("sequence increment cannot be 0".into()));
        }
        let start = start.unwrap_or(if increment > 0 { min } else { max });
        if start < min || start > max {
            return Err(Error::Tsql(
                "sequence START WITH is outside MINVALUE..MAXVALUE".into(),
            ));
        }
        self.ensure_sequence_table()?;
        self.internal.store(true, Ordering::Release);
        let inserted = self.conn.execute(
            "INSERT INTO _novadb_sequences(name, current, start_value, increment, min_value, max_value, cycle) \
             VALUES (?1, NULL, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![name, start, increment, min, max, cycle],
        );
        self.internal.store(false, Ordering::Release);
        match inserted {
            Ok(_) => Ok(()),
            Err(error)
                if error.sqlite_error_code() == Some(rusqlite::ErrorCode::ConstraintViolation) =>
            {
                Err(Error::Tsql(format!(
                    "There is already an object named '{name}' in the database."
                )))
            }
            Err(error) => Err(error.into()),
        }
    }

    fn alter_sequence(&mut self, text: &str) -> Result<()> {
        let words: Vec<&str> = text.split_whitespace().collect();
        let name = self.sequence_name(words.get(2).copied().unwrap_or(""));
        let upper: Vec<String> = words.iter().map(|w| w.to_ascii_uppercase()).collect();
        if upper.get(3).map(String::as_str) != Some("RESTART") {
            return Err(Error::Unsupported(
                "ALTER SEQUENCE other than RESTART [WITH n]".into(),
            ));
        }
        let restart = match upper.get(4).map(String::as_str) {
            Some("WITH") => Some(
                words
                    .get(5)
                    .and_then(|w| w.trim_end_matches(';').parse::<i64>().ok())
                    .ok_or_else(|| {
                        Error::Unsupported("ALTER SEQUENCE RESTART WITH needs a number".into())
                    })?,
            ),
            None => None,
            Some(_) => return Err(Error::Unsupported("ALTER SEQUENCE option".into())),
        };
        self.ensure_sequence_table()?;
        self.internal.store(true, Ordering::Release);
        let changed = self.conn.execute(
            "UPDATE _novadb_sequences SET current = NULL, start_value = COALESCE(?2, start_value) WHERE name = ?1",
            rusqlite::params![name, restart],
        );
        self.internal.store(false, Ordering::Release);
        if changed? == 0 {
            return Err(Error::Tsql(format!("sequence `{name}` does not exist")));
        }
        Ok(())
    }

    fn drop_sequence(&mut self, text: &str) -> Result<()> {
        let words: Vec<&str> = text.split_whitespace().collect();
        let if_exists = words.get(2).is_some_and(|w| w.eq_ignore_ascii_case("IF"));
        let names = words.get(if if_exists { 4 } else { 2 }..).unwrap_or(&[]);
        if names.is_empty() {
            return Err(Error::Unsupported(format!(
                "DROP SEQUENCE: missing name in `{text}`"
            )));
        }
        self.ensure_sequence_table()?;
        for name in names.join(" ").split(',') {
            let name = self.sequence_name(name);
            self.internal.store(true, Ordering::Release);
            let changed = self
                .conn
                .execute("DELETE FROM _novadb_sequences WHERE name = ?1", [&name]);
            self.internal.store(false, Ordering::Release);
            if changed? == 0 && !if_exists {
                return Err(Error::Tsql(format!(
                    "Cannot drop the sequence '{name}', because it does not exist."
                )));
            }
        }
        Ok(())
    }

    fn next_sequence_value(&mut self, raw_name: &str) -> Result<i64> {
        let name = self.sequence_name(raw_name);
        self.ensure_sequence_table()?;
        let row = self.conn.query_row(
            "SELECT current, start_value, increment, min_value, max_value, cycle FROM _novadb_sequences WHERE name = ?1",
            [&name],
            |row| {
                Ok((
                    row.get::<_, Option<i64>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, bool>(5)?,
                ))
            },
        );
        let (current, start, increment, min, max, cycle) = match row {
            Ok(row) => row,
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                return Err(Error::Tsql(format!(
                    "Invalid object name '{name}' (sequence)"
                )));
            }
            Err(error) => return Err(error.into()),
        };
        let next = match current {
            None => start,
            Some(current) => match current.checked_add(increment) {
                Some(next) if (min..=max).contains(&next) => next,
                _ if cycle => {
                    if increment > 0 {
                        min
                    } else {
                        max
                    }
                }
                _ => {
                    return Err(Error::Tsql(format!(
                        "The sequence object '{name}' has reached its minimum or maximum value."
                    )));
                }
            },
        };
        self.internal.store(true, Ordering::Release);
        let updated = self.conn.execute(
            "UPDATE _novadb_sequences SET current = ?2 WHERE name = ?1",
            rusqlite::params![name, next],
        );
        self.internal.store(false, Ordering::Release);
        updated?;
        Ok(next)
    }
}

struct OutputClause {
    start: usize,
    end: usize,
    exprs: String,
    /// `INTO target [(columns)]`
    into: Option<(String, Option<String>)>,
}

/// Locates a T-SQL `OUTPUT` clause in an INSERT/UPDATE/DELETE/MERGE statement.
fn extract_output(text: &str) -> Result<Option<OutputClause>> {
    let words = top_level_words(text);
    if !matches!(
        words.first().map(|w| w.2.as_str()),
        Some("INSERT" | "UPDATE" | "DELETE" | "MERGE")
    ) {
        return Ok(None);
    }
    let Some(output_idx) = words.iter().position(|w| w.2 == "OUTPUT") else {
        return Ok(None);
    };
    let output = &words[output_idx];
    let terminator = words[output_idx + 1..].iter().find(|w| {
        matches!(
            w.2.as_str(),
            "INTO"
                | "VALUES"
                | "SELECT"
                | "DEFAULT"
                | "FROM"
                | "WHERE"
                | "EXEC"
                | "EXECUTE"
                | "OPTION"
        )
    });
    let exprs_end = terminator.map_or(text.len(), |w| w.0);
    let exprs = text[output.1..exprs_end].trim().to_string();
    let Some(into) = terminator.filter(|w| w.2 == "INTO") else {
        return Ok(Some(OutputClause {
            start: output.0,
            end: exprs_end,
            exprs,
            into: None,
        }));
    };
    // Target: next token(s) up to whitespace or `(`.
    let after = &text[into.1..];
    let trimmed = after.trim_start();
    let offset = into.1 + (after.len() - trimmed.len());
    let target_len = trimmed
        .find(|c: char| c.is_whitespace() || c == '(' || c == ';')
        .unwrap_or(trimmed.len());
    let target = trimmed[..target_len].to_string();
    let mut end = offset + target_len;
    let rest = &text[end..];
    let rest_trimmed = rest.trim_start();
    let mut columns = None;
    if rest_trimmed.starts_with('(') {
        let open = end + (rest.len() - rest_trimmed.len());
        let close = matching_paren(text, open)
            .ok_or_else(|| Error::Unsupported("OUTPUT INTO: unbalanced".into()))?;
        // `INSERT ... OUTPUT ... INTO @t (cols) VALUES (...)`: the group after the
        // target is the column list only if VALUES/SELECT/... follows it.
        columns = Some(text[open..=close].to_string());
        end = close + 1;
    }
    Ok(Some(OutputClause {
        start: output.0,
        end,
        exprs,
        into: Some((target, columns)),
    }))
}

/// Removes a trailing `AS alias` / bare alias from an OUTPUT expression.
fn strip_output_alias(item: &str) -> String {
    let words = top_level_words(item);
    if let Some(as_word) = words.iter().rev().find(|w| w.2 == "AS") {
        return item[..as_word.0].trim().to_string();
    }
    item.to_string()
}

/// `inserted.Col` -> `Col`, `inserted.*` -> `*`.
fn strip_pseudo_table(expr: &str, source: &str) -> String {
    let pattern = format!(r"(?i)\b{source}\.");
    match regex::Regex::new(&pattern) {
        Ok(re) => re.replace_all(expr, "").into_owned(),
        Err(_) => expr.to_string(),
    }
}

/// Qualifies bare column names of a simple expression with `alias.`.
fn qualify_columns(expr: &str, alias: &str) -> String {
    let trimmed = expr.trim();
    if trimmed
        .chars()
        .all(|c| is_word_char(c) || is_placeholder(c) || c == '"' || c == '[' || c == ']')
    {
        format!("{alias}.{trimmed}")
    } else {
        trimmed.to_string()
    }
}

fn is_catchable(error: &Error) -> bool {
    matches!(
        error,
        Error::Sqlite(_) | Error::Raised { .. } | Error::Tsql(_)
    )
}

fn is_blank_sql(sql: &str) -> bool {
    let mut rest = sql;
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == ';');
        if let Some(after) = rest.strip_prefix("--") {
            rest = after.split_once('\n').map_or("", |(_, tail)| tail);
        } else if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/").map_or("", |(_, tail)| tail);
        } else {
            return rest.is_empty();
        }
    }
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn find_top_level_eq(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (idx, ch) in text.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            '=' if depth == 0 => {
                let prev = text[..idx].chars().last();
                let next = text[idx + 1..].chars().next();
                if !matches!(prev, Some('<' | '>' | '!')) && next != Some('=') {
                    return Some(idx);
                }
            }
            _ => {}
        }
    }
    None
}

fn as_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Integer(int) => Some(*int),
        Value::Real(real) => Some(*real as i64),
        Value::Text(text) => text.trim().parse().ok(),
        _ => None,
    }
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Integer(int) => int.to_string(),
        Value::Real(real) => real.to_string(),
        Value::Text(text) => text.clone(),
        Value::Blob(bytes) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// Coerces a value to a declared T-SQL type; conversion failures are errors.
fn coerce(value: Value, type_name: Option<&str>) -> Result<Value> {
    let Some(type_name) = type_name else {
        return Ok(value);
    };
    if matches!(value, Value::Null) {
        return Ok(value);
    }
    let upper = type_name.trim().to_ascii_uppercase();
    match try_cast(value.clone(), &upper) {
        Ok(Value::Null) => Err(Error::Tsql(format!(
            "Conversion failed when converting the value '{}' to data type {type_name}.",
            value_to_string(&value)
        ))),
        Ok(Value::Real(real)) if upper.starts_with("DECIMAL") || upper.starts_with("NUMERIC") => {
            let scale = upper
                .split_once(',')
                .and_then(|(_, s)| s.trim_end_matches(')').trim().parse::<i32>().ok())
                .unwrap_or(0);
            let factor = 10f64.powi(scale);
            Ok(Value::Real((real * factor).round() / factor))
        }
        Ok(converted) => Ok(converted),
        // Types the cast helper does not know (XML, SQL_VARIANT, user types)
        // keep their value.
        Err(_) => Ok(value),
    }
}

/// Minimal `RAISERROR` message formatting (`%s`, `%d`, `%i`, `%u`, `%%`).
fn printf(format: &str, args: &[Value]) -> String {
    let mut out = String::new();
    let mut args = args.iter();
    let mut chars = format.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        if chars.peek() == Some(&'%') {
            chars.next();
            out.push('%');
            continue;
        }
        // Skip flags/width.
        while chars
            .peek()
            .is_some_and(|c| c.is_ascii_digit() || "-+ #.".contains(*c))
        {
            chars.next();
        }
        match chars.next() {
            Some('s' | 'd' | 'i' | 'u' | 'x' | 'X' | 'o') => {
                out.push_str(
                    &args
                        .next()
                        .map(value_to_string)
                        .unwrap_or_else(|| "(null)".into()),
                );
            }
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Statement rewrites used by the transpiler
// ---------------------------------------------------------------------------

/// Applies `rewrite` to every `;`-separated top-level statement of a masked
/// batch. `rewrite` returns `None` to keep the statement unchanged.
fn for_each_statement(
    sql: &str,
    mut rewrite: impl FnMut(&str) -> Result<Option<String>>,
) -> Result<String> {
    let mut out = String::with_capacity(sql.len());
    let mut depth = 0usize;
    let mut start = 0;
    for (idx, ch) in sql.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ';' if depth == 0 => {
                let statement = &sql[start..idx];
                match rewrite(statement)? {
                    Some(new) => out.push_str(&new),
                    None => out.push_str(statement),
                }
                out.push(';');
                start = idx + 1;
            }
            _ => {}
        }
    }
    let statement = &sql[start..];
    match rewrite(statement)? {
        Some(new) => out.push_str(&new),
        None => out.push_str(statement),
    }
    Ok(out)
}

/// Positions of top-level (depth 0) keywords in `text`.
fn top_level_words(text: &str) -> Vec<(usize, usize, String)> {
    let mut depth = 0usize;
    let mut words = Vec::new();
    for token in tokenize(text) {
        match token.kind {
            Kind::Punct('(') => depth += 1,
            Kind::Punct(')') => depth = depth.saturating_sub(1),
            Kind::Word if depth == 0 => {
                words.push((
                    token.start,
                    token.end,
                    text[token.start..token.end].to_ascii_uppercase(),
                ));
            }
            _ => {}
        }
    }
    words
}

/// Splits `source [AS] alias` into (source, alias).
fn split_alias(text: &str) -> (String, Option<String>) {
    let text = text.trim();
    let tokens = tokenize(text);
    let Some(last) = tokens.last() else {
        return (String::new(), None);
    };
    if tokens.len() >= 2 && matches!(last.kind, Kind::Word | Kind::Literal) {
        let prev = tokens[tokens.len() - 2];
        let prev_text = &text[prev.start..prev.end];
        let source_end = if prev_text.eq_ignore_ascii_case("AS") {
            prev.start
        } else if matches!(prev.kind, Kind::Word | Kind::Literal | Kind::Punct(')')) {
            prev.end
        } else {
            return (text.to_string(), None);
        };
        return (
            text[..source_end].trim().to_string(),
            Some(text[last.start..last.end].to_string()),
        );
    }
    (text.to_string(), None)
}

/// Removes `alias.` qualifiers from the left-hand side of `SET` assignments.
fn unqualify_assignments(set_list: &str, alias: &str) -> String {
    split_top_level_commas(set_list)
        .into_iter()
        .map(|assignment| match find_top_level_eq(assignment) {
            Some(eq) => {
                let lhs = assignment[..eq].trim();
                let column = match lhs.split_once('.') {
                    Some((qualifier, column)) if qualifier.trim().eq_ignore_ascii_case(alias) => {
                        column.trim()
                    }
                    _ => lhs,
                };
                format!("{column} = {}", assignment[eq + 1..].trim())
            }
            None => assignment.trim().to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// T-SQL `MERGE` -> equivalent sequence of SQLite statements.
pub(crate) fn rewrite_merge_statements(sql: &str, _spans: &MaskedSpans) -> Result<String> {
    let has_merge = tokenize(sql)
        .iter()
        .any(|t| t.kind == Kind::Word && sql[t.start..t.end].eq_ignore_ascii_case("MERGE"));
    if !has_merge {
        return Ok(sql.to_string());
    }
    for_each_statement(sql, |statement| {
        let words = top_level_words(statement);
        if words.first().map(|w| w.2.as_str()) != Some("MERGE") {
            return Ok(None);
        }
        rewrite_merge(statement, &words).map(Some)
    })
}

struct MergeClause {
    kind: &'static str,
    condition: Option<String>,
    action: String,
}

fn rewrite_merge(statement: &str, words: &[(usize, usize, String)]) -> Result<String> {
    let unsupported = |what: &str| Error::Unsupported(format!("MERGE: {what}"));
    let find = |word: &str, from: usize| words.iter().position(|w| w.2 == word && w.0 >= from);
    let merge_end = words[0].1;
    let mut target_start = merge_end;
    if words.get(1).is_some_and(|w| w.2 == "TOP") {
        return Err(unsupported("TOP is not supported"));
    }
    if words.get(1).is_some_and(|w| w.2 == "INTO") {
        target_start = words[1].1;
    }
    let using = find("USING", 0).ok_or_else(|| unsupported("missing USING"))?;
    let on = find("ON", words[using].1).ok_or_else(|| unsupported("missing ON"))?;
    let first_when = find("WHEN", words[on].1).ok_or_else(|| unsupported("missing WHEN"))?;
    let (target, target_alias) = split_alias(&statement[target_start..words[using].0]);
    let target_alias = target_alias.unwrap_or_else(|| target.clone());
    let source_text = statement[words[using].1..words[on].0].trim().to_string();
    // Source alias may carry a column list: `(VALUES ...) AS S(a, b)`.
    let (source, source_alias) = {
        let trimmed = source_text.trim_end();
        if trimmed.ends_with(')') {
            let open = trimmed.rfind('(').unwrap_or(0);
            let before = trimmed[..open].trim_end();
            let alias_tokens = tokenize(before);
            match alias_tokens.last() {
                Some(t) if t.kind == Kind::Word && open > 0 && !before.ends_with(')') => {
                    let (src, alias) = split_alias(before);
                    match alias {
                        Some(alias) => (src, format!("{alias}{}", &trimmed[open..])),
                        None => (trimmed.to_string(), String::new()),
                    }
                }
                _ => {
                    let (src, alias) = split_alias(trimmed);
                    (src, alias.unwrap_or_default())
                }
            }
        } else {
            let (src, alias) = split_alias(trimmed);
            (src, alias.unwrap_or_default())
        }
    };
    if source_alias.is_empty() {
        return Err(unsupported("the USING source needs an alias"));
    }
    let source_name = source_alias
        .split('(')
        .next()
        .unwrap_or(&source_alias)
        .trim()
        .to_string();
    let on_condition = statement[words[on].1..words[first_when].0]
        .trim()
        .to_string();

    // WHEN clauses.
    let mut clauses = Vec::new();
    let mut idx = first_when;
    let end_of_clauses = find("OUTPUT", words[first_when].0)
        .or_else(|| find("OPTION", words[first_when].0))
        .map_or(statement.len(), |i| words[i].0);
    if let Some(output) = find("OUTPUT", words[first_when].0) {
        if find("INTO", words[output].1).is_some() {
            return Err(unsupported("OUTPUT ... INTO is not supported"));
        }
    }
    while idx < words.len() && words[idx].2 == "WHEN" && words[idx].0 < end_of_clauses {
        let next_when = words[idx + 1..]
            .iter()
            .position(|w| w.2 == "WHEN" && w.0 < end_of_clauses)
            .map(|p| p + idx + 1);
        let clause_end = next_when.map_or(end_of_clauses, |n| words[n].0);
        let then = words[idx..]
            .iter()
            .position(|w| w.2 == "THEN")
            .map(|p| p + idx)
            .filter(|&t| words[t].0 < clause_end)
            .ok_or_else(|| unsupported("WHEN without THEN"))?;
        let header: Vec<&str> = words[idx + 1..then].iter().map(|w| w.2.as_str()).collect();
        let kind = match header.as_slice() {
            ["MATCHED", ..] => "matched",
            ["NOT", "MATCHED", "BY", "SOURCE", ..] => "by_source",
            ["NOT", "MATCHED", ..] => "by_target",
            _ => return Err(unsupported("unknown WHEN clause")),
        };
        let condition = words[idx + 1..then]
            .iter()
            .find(|w| w.2 == "AND")
            .map(|and| statement[and.1..words[then].0].trim().to_string());
        let action = statement[words[then].1..clause_end].trim().to_string();
        clauses.push(MergeClause {
            kind,
            condition,
            action,
        });
        match next_when {
            Some(n) => idx = n,
            None => break,
        }
    }

    let matched = format!("EXISTS (SELECT 1 FROM {source} AS {source_alias} WHERE {on_condition})");
    let mut deletes = Vec::new();
    let mut updates = Vec::new();
    let mut inserts = Vec::new();
    let mut previous: HashMap<&str, Vec<String>> = HashMap::new();
    for clause in &clauses {
        let earlier = previous.entry(clause.kind).or_default();
        let mut predicate: Vec<String> = earlier
            .iter()
            .map(|c| format!("NOT COALESCE(({c}), 0)"))
            .collect();
        if let Some(condition) = &clause.condition {
            predicate.push(format!("({condition})"));
            earlier.push(condition.clone());
        } else {
            earlier.push("1".to_string());
        }
        let extra = if predicate.is_empty() {
            String::new()
        } else {
            format!(" AND {}", predicate.join(" AND "))
        };
        let action_words = top_level_words(&clause.action);
        let verb = action_words
            .first()
            .map(|w| w.2.clone())
            .unwrap_or_default();
        match (clause.kind, verb.as_str()) {
            ("matched", "DELETE") => deletes.push(format!(
                "DELETE FROM {target} AS {target_alias} WHERE EXISTS (SELECT 1 FROM {source} AS {source_alias} WHERE ({on_condition}){extra})"
            )),
            ("by_source", "DELETE") => deletes.push(format!(
                "DELETE FROM {target} AS {target_alias} WHERE NOT {matched}{extra}"
            )),
            ("matched", "UPDATE") => {
                let set = action_words.iter().find(|w| w.2 == "SET").ok_or_else(|| unsupported("UPDATE without SET"))?;
                let assignments = unqualify_assignments(&clause.action[set.1..], &target_alias);
                updates.push(format!(
                    "UPDATE {target} AS {target_alias} SET {assignments} FROM {source} AS {source_alias} WHERE ({on_condition}){extra}"
                ));
            }
            ("by_source", "UPDATE") => {
                let set = action_words.iter().find(|w| w.2 == "SET").ok_or_else(|| unsupported("UPDATE without SET"))?;
                let assignments = unqualify_assignments(&clause.action[set.1..], &target_alias);
                updates.push(format!(
                    "UPDATE {target} AS {target_alias} SET {assignments} WHERE NOT {matched}{extra}"
                ));
            }
            ("by_target", "INSERT") => {
                let values = action_words.iter().find(|w| w.2 == "VALUES");
                let default_values = action_words.iter().any(|w| w.2 == "DEFAULT");
                let after_insert = action_words[0].1;
                let (columns, select_list) = match values {
                    Some(values) => {
                        let columns = clause.action[after_insert..values.0].trim().to_string();
                        let list = clause.action[values.1..].trim();
                        let list = list
                            .strip_prefix('(')
                            .and_then(|l| l.strip_suffix(')'))
                            .ok_or_else(|| unsupported("INSERT VALUES must be a single row"))?;
                        (columns, list.to_string())
                    }
                    None if default_values => {
                        return Err(unsupported("INSERT DEFAULT VALUES is not supported"));
                    }
                    None => return Err(unsupported("INSERT without VALUES")),
                };
                let condition = extra.trim_start_matches(" AND ");
                let where_clause = if condition.is_empty() { String::new() } else { format!(" WHERE {condition}") };
                inserts.push(format!(
                    "INSERT INTO {target} {columns} SELECT {select_list} FROM temp.novadb_merge_source AS {source_name}{where_clause}"
                ));
            }
            (_, other) => return Err(unsupported(&format!("unsupported action `{other}`"))),
        }
    }

    let mut statements = Vec::new();
    if !inserts.is_empty() {
        statements.push("DROP TABLE IF EXISTS temp.novadb_merge_source".to_string());
        statements.push(format!(
            "CREATE TEMP TABLE novadb_merge_source AS SELECT {source_name}.* FROM {source} AS {source_alias} \
             WHERE NOT EXISTS (SELECT 1 FROM {target} AS {target_alias} WHERE {on_condition})"
        ));
    }
    statements.extend(deletes);
    statements.extend(updates);
    statements.extend(inserts.iter().cloned());
    if !inserts.is_empty() {
        statements.push("DROP TABLE IF EXISTS temp.novadb_merge_source".to_string());
    }
    Ok(format!("\n{}", statements.join(";\n")))
}

/// Splits a `FROM` clause into `(join kind, source [alias], ON condition)`.
fn split_sources(from_clause: &str) -> Vec<(String, String, Option<String>)> {
    let mut sources: Vec<(String, String, Option<String>)> = Vec::new(); // (join kind, source+alias, on)
    let mut pending_kind = String::new();
    let mut start = 0;
    let fwords = top_level_words(from_clause);
    let mut cuts: Vec<(usize, usize, String)> = Vec::new();
    let mut idx = 0;
    while idx < fwords.len() {
        let w = &fwords[idx];
        if w.2 == "JOIN" {
            // include preceding INNER/LEFT/... words
            let mut kind_start = w.0;
            let mut kind = String::from("JOIN");
            let mut back = idx;
            while back > 0
                && matches!(
                    fwords[back - 1].2.as_str(),
                    "INNER" | "LEFT" | "RIGHT" | "FULL" | "OUTER" | "CROSS"
                )
            {
                back -= 1;
                kind_start = fwords[back].0;
                kind = format!("{} {kind}", fwords[back].2);
            }
            cuts.push((kind_start, w.1, kind));
        }
        idx += 1;
    }
    // Commas at depth 0 also separate sources.
    let mut depth = 0usize;
    for (pos, ch) in from_clause.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => cuts.push((pos, pos + 1, ",".to_string())),
            _ => {}
        }
    }
    cuts.sort_by_key(|c| c.0);
    for (cut_start, cut_end, kind) in cuts.iter().cloned().chain(std::iter::once((
        from_clause.len(),
        from_clause.len(),
        String::new(),
    ))) {
        let segment = &from_clause[start..cut_start];
        let seg_words = top_level_words(segment);
        let (source, on) = match seg_words.iter().find(|w| w.2 == "ON") {
            Some(on) => (
                segment[..on.0].trim().to_string(),
                Some(segment[on.1..].trim().to_string()),
            ),
            None => (segment.trim().to_string(), None),
        };
        sources.push((std::mem::take(&mut pending_kind), source, on));
        pending_kind = kind;
        start = cut_end;
    }

    sources
}

/// T-SQL multi-table and limited `DELETE`:
///
/// * `DELETE x FROM t x JOIN s ON ... [WHERE ...]` and
///   `DELETE FROM x FROM t x JOIN s ON ... [WHERE ...]` become
///   `DELETE FROM t WHERE rowid IN (SELECT x.rowid FROM t x JOIN s ON ... [WHERE ...])`
///   (works for any join kind; `WITHOUT ROWID` tables fail loudly with "no such
///   column: rowid").
/// * `DELETE TOP (n) FROM t [WHERE ...]` becomes
///   `DELETE FROM t WHERE rowid IN (SELECT rowid FROM t [WHERE ...] LIMIT n)`.
pub(crate) fn rewrite_delete_from(sql: &str) -> Result<String> {
    for_each_statement(sql, |statement| {
        let words = top_level_words(statement);
        if words.first().map(|w| w.2.as_str()) != Some("DELETE") {
            return Ok(None);
        }
        let mut idx = 1;
        let mut limit: Option<String> = None;
        if words.get(idx).is_some_and(|w| w.2 == "TOP") {
            let top_end = words[idx].1;
            let after = words.get(idx + 1).map_or(statement.len(), |w| w.0);
            let amount = statement[top_end..after]
                .trim()
                .trim_start_matches('(')
                .trim_end_matches(')')
                .trim();
            if amount.is_empty() || !amount.bytes().all(|b| b.is_ascii_digit()) {
                return Err(Error::Unsupported(
                    "DELETE TOP requires a literal row count".into(),
                ));
            }
            limit = Some(amount.to_string());
            idx += 1;
        }
        let first_is_from = words.get(idx).is_some_and(|w| w.2 == "FROM");
        let from_positions: Vec<usize> = words
            .iter()
            .enumerate()
            .filter(|(_, w)| w.2 == "FROM")
            .map(|(i, _)| i)
            .collect();
        // Locate the clause boundaries.
        let where_idx = words.iter().position(|w| w.2 == "WHERE");
        let tail_idx = words
            .iter()
            .position(|w| matches!(w.2.as_str(), "OPTION" | "OUTPUT" | "RETURNING"));
        let where_start = where_idx.map(|i| words[i].0);
        let tail_start = tail_idx.map(|i| words[i].0);
        let cut = |from: usize| -> usize {
            where_start
                .or(tail_start)
                .filter(|pos| *pos >= from)
                .unwrap_or(statement.len())
        };
        let where_text = where_idx.map(|i| {
            let end = tail_start
                .filter(|pos| *pos > words[i].1)
                .unwrap_or(statement.len());
            statement[words[i].1..end].trim().to_string()
        });
        let tail = tail_start
            .map(|pos| format!(" {}", statement[pos..].trim()))
            .unwrap_or_default();

        let (table_ref, from_clause, target_name): (String, Option<String>, String) =
            if first_is_from && from_positions.len() == 1 {
                // DELETE [TOP n] FROM t [WHERE ...]
                let start = words[idx].1;
                let table = statement[start..cut(start)].trim().to_string();
                if limit.is_none() {
                    return Ok(None);
                }
                (table.clone(), None, table)
            } else {
                // Two FROMs (`DELETE FROM x FROM ...`) or `DELETE x FROM ...`.
                let second_from = from_positions
                    .iter()
                    .copied()
                    .find(|i| *i > idx)
                    .ok_or_else(|| Error::Unsupported("DELETE without a FROM clause".into()))?;
                let target_start = if first_is_from {
                    words[idx].1
                } else {
                    words[idx - 1].1
                };
                let target = statement[target_start..words[second_from].0]
                    .trim()
                    .to_string();
                let from_start = words[second_from].1;
                let clause = statement[from_start..cut(from_start)].trim().to_string();
                (String::new(), Some(clause), target)
            };

        match from_clause {
            None => {
                let mut inner = format!("SELECT rowid FROM {table_ref}");
                if let Some(condition) = &where_text {
                    inner.push_str(&format!(" WHERE {condition}"));
                }
                if let Some(limit) = &limit {
                    inner.push_str(&format!(" LIMIT {limit}"));
                }
                Ok(Some(format!(
                    "DELETE FROM {table_ref} WHERE rowid IN ({inner}){tail}"
                )))
            }
            Some(clause) => {
                let sources = split_sources(&clause);
                if sources.len() == 1 && limit.is_none() {
                    // `DELETE x FROM t [AS] x [WHERE ...]`: a plain aliased delete.
                    let (table, alias) = split_alias(&sources[0].1);
                    let names_target = alias
                        .as_deref()
                        .unwrap_or(&table)
                        .eq_ignore_ascii_case(&target_name)
                        || table.eq_ignore_ascii_case(&target_name);
                    if names_target && !table.starts_with('(') {
                        let alias_clause = alias.map(|a| format!(" AS {a}")).unwrap_or_default();
                        let where_clause = where_text
                            .as_ref()
                            .map(|w| format!(" WHERE {w}"))
                            .unwrap_or_default();
                        return Ok(Some(format!(
                            "DELETE FROM {table}{alias_clause}{where_clause}{tail}"
                        )));
                    }
                }
                let matched = sources.iter().find_map(|(_, source, _)| {
                    let (table, alias) = split_alias(source);
                    let refers = alias
                        .as_deref()
                        .unwrap_or(&table)
                        .eq_ignore_ascii_case(&target_name)
                        || table.eq_ignore_ascii_case(&target_name);
                    refers.then_some((table, alias))
                });
                let Some((table, alias)) = matched else {
                    return Err(Error::Unsupported(format!(
                        "DELETE target `{target_name}` is not one of the FROM sources"
                    )));
                };
                if table.starts_with('(') {
                    return Err(Error::Unsupported(
                        "DELETE ... FROM where the target is a derived table".into(),
                    ));
                }
                let reference = alias.unwrap_or_else(|| table.clone());
                let mut inner = format!("SELECT {reference}.rowid FROM {clause}");
                if let Some(condition) = &where_text {
                    inner.push_str(&format!(" WHERE {condition}"));
                }
                if let Some(limit) = &limit {
                    inner.push_str(&format!(" LIMIT {limit}"));
                }
                Ok(Some(format!(
                    "DELETE FROM {table} WHERE rowid IN ({inner}){tail}"
                )))
            }
        }
    })
}

/// T-SQL `UPDATE alias SET ... FROM table alias JOIN ...` -> SQLite
/// `UPDATE table AS alias SET ... FROM <other tables> WHERE <join> AND <where>`.
pub(crate) fn rewrite_update_from(sql: &str) -> Result<String> {
    for_each_statement(sql, |statement| {
        let words = top_level_words(statement);
        if words.first().map(|w| w.2.as_str()) != Some("UPDATE") {
            return Ok(None);
        }
        let Some(set) = words.iter().position(|w| w.2 == "SET") else {
            return Ok(None);
        };
        let Some(from) = words[set..]
            .iter()
            .position(|w| w.2 == "FROM")
            .map(|p| p + set)
        else {
            return Ok(None);
        };
        let target = statement[words[0].1..words[set].0].trim();
        if target.split_whitespace().count() != 1 {
            // Already `UPDATE table AS alias` (SQLite form) or `UPDATE TOP (n)`.
            return Ok(None);
        }
        let where_idx = words[from..]
            .iter()
            .position(|w| w.2 == "WHERE")
            .map(|p| p + from);
        let tail_idx = words[from..]
            .iter()
            .position(|w| matches!(w.2.as_str(), "OPTION" | "RETURNING"))
            .map(|p| p + from);
        let from_end = where_idx
            .or(tail_idx)
            .map_or(statement.len(), |i| words[i].0);
        let where_text = where_idx.map(|i| {
            let end = tail_idx.map_or(statement.len(), |t| words[t].0);
            statement[words[i].1..end].trim().to_string()
        });
        let tail = tail_idx.map_or(String::new(), |t| statement[words[t].0..].to_string());
        let set_list = &statement[words[set].1..words[from].0];
        let from_clause = &statement[words[from].1..from_end];

        let sources = split_sources(from_clause);

        let target_index = sources.iter().position(|(_, source, _)| {
            let (table, alias) = split_alias(source);
            alias
                .as_deref()
                .unwrap_or(&table)
                .eq_ignore_ascii_case(target)
                || (alias.is_none() && table.eq_ignore_ascii_case(target))
                || table.eq_ignore_ascii_case(target) && alias.is_none()
        });
        let assignments_alias;
        let (update_target, mut conditions, rest_sources) = match target_index {
            None => {
                // `UPDATE t SET ... FROM other WHERE ...` is already valid SQLite.
                assignments_alias = target.to_string();
                (target.to_string(), Vec::new(), sources)
            }
            Some(index) => {
                let (kind, source, on) = sources[index].clone();
                let (table, alias) = split_alias(&source);
                if table.starts_with('(') {
                    return Err(Error::Unsupported(
                        "UPDATE ... FROM where the target is a derived table".into(),
                    ));
                }
                let kind_upper = kind.to_ascii_uppercase();
                if kind_upper.contains("LEFT")
                    || kind_upper.contains("RIGHT")
                    || kind_upper.contains("FULL")
                {
                    return Err(Error::Unsupported(
                        "UPDATE ... FROM with the target on the outer side of a JOIN".into(),
                    ));
                }
                let mut conditions = Vec::new();
                if let Some(on) = on {
                    conditions.push(on);
                }
                let mut rest: Vec<(String, String, Option<String>)> = sources
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != index)
                    .map(|(_, s)| s.clone())
                    .collect();
                if let Some(first) = rest.first_mut() {
                    let first_kind = first.0.to_ascii_uppercase();
                    if first_kind.contains("LEFT")
                        || first_kind.contains("RIGHT")
                        || first_kind.contains("FULL")
                    {
                        return Err(Error::Unsupported(
                            "UPDATE ... FROM with LEFT/RIGHT/FULL JOIN against the target".into(),
                        ));
                    }
                    if let Some(on) = first.2.take() {
                        conditions.push(on);
                    }
                    first.0 = String::new();
                }
                let alias_name = alias.clone().unwrap_or_else(|| table.clone());
                assignments_alias = alias_name.clone();
                let update_target = match alias {
                    Some(alias) if !alias.eq_ignore_ascii_case(&table) => {
                        format!("{table} AS {alias}")
                    }
                    _ => table,
                };
                (update_target, conditions, rest)
            }
        };
        if let Some(where_text) = where_text {
            conditions.push(where_text);
        }
        let mut from_sql = String::new();
        for (kind, source, on) in &rest_sources {
            if !kind.is_empty() {
                from_sql.push(' ');
                from_sql.push_str(kind);
                from_sql.push(' ');
            }
            from_sql.push_str(source);
            if let Some(on) = on {
                from_sql.push_str(" ON ");
                from_sql.push_str(on);
            }
        }
        let assignments = unqualify_assignments(set_list, &assignments_alias);
        let mut out = format!("UPDATE {update_target} SET {assignments}");
        if !from_sql.trim().is_empty() {
            out.push_str(" FROM ");
            out.push_str(from_sql.trim());
        }
        if !conditions.is_empty() {
            out.push_str(" WHERE ");
            out.push_str(
                &conditions
                    .iter()
                    .map(|c| format!("({c})"))
                    .collect::<Vec<_>>()
                    .join(" AND "),
            );
        }
        if !tail.is_empty() {
            out.push(' ');
            out.push_str(&tail);
        }
        let leading = &statement[..statement.len() - statement.trim_start().len()];
        Ok(Some(format!("{leading}{out}")))
    })
}

/// `CROSS APPLY STRING_SPLIT(text, sep [, ordinal]) [AS] alias` (and OUTER
/// APPLY) -> correlated `json_each` join; `alias.ordinal` becomes
/// `(alias.key + 1)`.
pub(crate) fn rewrite_apply_string_split(sql: &str, spans: &MaskedSpans) -> Result<String> {
    let Ok(re) = cached_regex!(r"(?is)\b(CROSS|OUTER)\s+APPLY\s+STRING_SPLIT\s*\(") else {
        return Ok(sql.to_string());
    };
    let mut out = sql.to_string();
    let mut steps = 0usize;
    while let Some(caps) = re.captures(&out) {
        steps += 1;
        if steps > 256 {
            return Err(Error::Unsupported(
                "too many CROSS/OUTER APPLY STRING_SPLIT rewrite steps in one statement".into(),
            ));
        }
        let (Some(whole), Some(kind)) = (caps.get(0), caps.get(1)) else {
            break;
        };
        let outer = kind.as_str().eq_ignore_ascii_case("OUTER");
        let open = whole.end() - 1;
        let close = matching_paren(&out, open)
            .ok_or_else(|| Error::Unsupported("STRING_SPLIT: unbalanced".into()))?;
        let args = split_top_level_commas(&out[open + 1..close]);
        if args.len() < 2 || args.len() > 3 {
            return Err(Error::Unsupported(
                "STRING_SPLIT expects 2 or 3 arguments".into(),
            ));
        }
        let text = args[0].trim().to_string();
        let delim = args[1].trim().to_string();
        if let Some(content) = spans.literal_text(&delim) {
            if content == "\"" || content == "\\" || content.is_empty() {
                return Err(Error::Unsupported(format!(
                    "STRING_SPLIT separator '{content}'"
                )));
            }
        }
        let rest = &out[close + 1..];
        let alias_re = cached_regex!(r"(?is)\A\s*(?:AS\s+)?([a-zA-Z_][a-zA-Z0-9_]*)")
            .map_err(|e| Error::Unsupported(e.to_string()))?;
        let (alias, consumed) = match alias_re.captures(rest) {
            Some(c) if !crate::is_clause_keyword(&c[1]) => {
                (c[1].to_string(), c.get(0).map_or(0, |m| m.end()))
            }
            _ => ("split_tbl".to_string(), 0),
        };
        let escaped = format!(
            "replace(replace(replace(replace(replace(CAST({text} AS TEXT), '\\', '\\\\'), '\"', '\\\"'), char(10), '\\n'), char(13), '\\r'), char(9), '\\t')"
        );
        let json = format!("'[\"' || replace({escaped}, {delim}, '\",\"') || '\"]'");
        let join = if outer { "LEFT JOIN" } else { "CROSS JOIN" };
        let on = if outer { " ON 1 = 1" } else { "" };
        let replacement = format!("{join} json_each({json}) AS {alias}{on}");
        let end = close + 1 + consumed;
        out.replace_range(whole.start()..end, &replacement);
        out =
            crate::alias_bare_select_refs(&out, &alias, &[("ordinal".to_string(), String::new())]);
        let pattern = format!(r"(?i)\b{}\.ordinal\b", regex::escape(&alias));
        if let Ok(re_ordinal) = regex::Regex::new(&pattern) {
            out = re_ordinal
                .replace_all(&out, regex::NoExpand(&format!("({alias}.key + 1)")))
                .into_owned();
        }
    }
    Ok(out)
}
