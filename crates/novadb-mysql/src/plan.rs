//! Statement plan cache.
//!
//! Games and web apps send the same few dozen statement shapes millions of
//! times with different literals. Lexing, translating and re-typing them for
//! every call costs far more than running them, so the gateway keeps one
//! [`Plan`] per statement shape: the translated SQL with the parameters cut
//! out (a template), the facts about the statement that the executor needs
//! (query or write, which verb) and a cache of result column typing.
//!
//! Plans are shared by all sessions (sharded, bounded, generation based
//! "approximate LRU"). Only plain `SELECT` / `INSERT` / `UPDATE` / `DELETE` /
//! `REPLACE` statements whose translation does not depend on session state
//! are cached; everything else keeps taking the generic path.
//!
//! * Prepared statements (`COM_STMT_PREPARE`): the statement is translated once
//!   with placeholder markers in place of the `?` parameters. The translator
//!   never inspects parameter values, so filling the template with the real
//!   values is exactly what translating with the values substituted gives.
//! * Text protocol (`COM_QUERY`): literals in plain value positions (right of a
//!   comparison operator, in `VALUES` / `IN` lists, after `LIMIT`, ...) are
//!   abstracted to parameters, the key is the abstracted token stream, and a
//!   new plan is only stored when filling the template with the statement's own
//!   literals reproduces the generic translation byte for byte.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use novadb_core::Param;

use crate::lexer::{Kind, Token, hex_literal, quote_str};
use crate::translate::{self, Env};
use crate::typing::{Declared, SelectInfo};

/// One statement argument: a bound parameter value or, for statements that
/// cannot be bound, a SQL literal.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Arg {
    Null,
    Int(i64),
    Real(f64),
    /// A numeric literal rendered as SQL (`(-5)`, `(12.50)`, `18446744073709551615`).
    Num(String),
    Text(String),
    Blob(Vec<u8>),
}

impl Arg {
    /// The argument as SQLite SQL text.
    pub(crate) fn sql(&self) -> String {
        match self {
            Self::Null => "NULL".into(),
            Self::Int(number) => int_literal(number.to_string()),
            Self::Real(number) => float_literal(*number),
            Self::Num(text) => text.clone(),
            Self::Text(text) => quote_str(text),
            Self::Blob(bytes) => hex_literal(bytes),
        }
    }

    /// The argument as a core bound parameter.
    pub(crate) fn param(self) -> Param {
        match self {
            Self::Null => Param::Null,
            Self::Int(number) => Param::Int(number),
            Self::Real(number) if number.is_finite() => Param::Real(number),
            Self::Real(_) => Param::Null,
            Self::Num(text) => {
                let digits = text.trim().trim_start_matches('(').trim_end_matches(')');
                match digits.parse::<i64>() {
                    Ok(number) => Param::Int(number),
                    Err(_) => digits
                        .parse::<f64>()
                        .ok()
                        .filter(|number| number.is_finite())
                        .map_or(Param::Null, Param::Real),
                }
            }
            Self::Text(text) => Param::Text(text),
            Self::Blob(bytes) => Param::Blob(bytes),
        }
    }
}

/// Renders an integer (negative ones in parentheses so `- -5` cannot fuse).
pub(crate) fn int_literal(text: String) -> String {
    if text.starts_with('-') {
        format!("({text})")
    } else {
        text
    }
}

/// Renders a float as a SQL literal (`NULL` for NaN / infinities).
pub(crate) fn float_literal(number: f64) -> String {
    if !number.is_finite() {
        return "NULL".into();
    }
    let text = format!("{number:?}");
    if number.is_sign_negative() {
        format!("({text})")
    } else {
        text
    }
}

const MARK: &str = "NVMARK";
const SHARDS: usize = 16;
/// Plans per shard and generation (two generations are kept).
const SHARD_CAPACITY: usize = 512;
/// Statements with more parameters than this are not cached.
const MAX_PARAMS: usize = 512;
/// Cached result typing is re-resolved at least this often, which bounds how
/// long a schema change made through another door (HTTP API, PostgreSQL
/// gateway) can go unnoticed. DDL run through this gateway invalidates at once.
pub(crate) const TYPING_TTL: Duration = Duration::from_secs(10);

/// One piece of a translated statement.
#[derive(Debug)]
enum Seg {
    Text(String),
    Param(usize),
}

/// Cached result typing of a plan (see `Session::typed_result`).
pub(crate) struct Typed {
    pub epoch: u64,
    pub built: Instant,
    /// Result column labels and raw declared types this entry is valid for.
    pub columns: Vec<String>,
    pub declared_in: Vec<Option<String>>,
    /// Parsed declared MySQL type per column (after schema resolution).
    pub parsed: Vec<Option<Declared>>,
}

/// Binding layout of a plan: the SQL text with `?N` placeholders, values are
/// bound instead of being part of the text.
pub(crate) struct Bind {
    /// Parameter index -> bound position (`None`: spliced into the text, as
    /// `LIMIT` / `OFFSET` values are: SQLite plans `ORDER BY .. LIMIT n` with a
    /// literal `n` markedly better than with a bound one).
    pub position: Vec<Option<usize>>,
    /// The text when nothing is spliced.
    pub fixed_sql: Option<Arc<str>>,
}

/// Translation template of one statement shape.
pub(crate) struct Plan {
    /// `SELECT`, `INSERT`, `UPDATE`, `DELETE` or `REPLACE`.
    pub first: &'static str,
    pub is_query: bool,
    pub select_info: Option<SelectInfo>,
    pub n_params: usize,
    segs: Vec<Seg>,
    sql_len: usize,
    /// How the parameters are bound, when they can be.
    pub bind: Option<Bind>,
    /// Columns reported at `COM_STMT_PREPARE` (epoch they were computed in).
    pub described: Mutex<Option<(u64, Vec<crate::packet::ColumnDef>)>>,
    pub typing: Mutex<Option<Arc<Typed>>>,
    /// Moving average of the database time of this plan in microseconds
    /// (`u32::MAX` until measured). Cheap reads run on the connection task
    /// itself; anything slower goes through the blocking pool.
    exec_us: AtomicU32,
}

/// Reads whose average database time stays below this many microseconds run
/// inline on the runtime worker (a blocking-pool hop costs about as much);
/// `NOVADB_MYSQL_INLINE_US=0` sends every statement through the blocking pool.
fn inline_max_us() -> u32 {
    static VALUE: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *VALUE.get_or_init(|| {
        std::env::var("NOVADB_MYSQL_INLINE_US")
            .ok()
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or(100)
    })
}

impl Plan {
    /// Whether the plan's database call is cheap enough to run inline.
    pub(crate) fn run_inline(&self) -> bool {
        self.is_query && self.exec_us.load(Ordering::Relaxed) <= inline_max_us()
    }

    /// Records the database time of one execution.
    pub(crate) fn record(&self, elapsed: Duration) {
        let micros = u32::try_from(elapsed.as_micros()).unwrap_or(u32::MAX - 1);
        let old = self.exec_us.load(Ordering::Relaxed);
        let new = if old == u32::MAX || micros > old.saturating_mul(4) {
            micros // first sample, or a sudden slowdown: react at once
        } else {
            (old * 3 + micros) / 4
        };
        self.exec_us.store(new, Ordering::Relaxed);
    }

    /// Substitutes SQL literals into the template.
    pub(crate) fn fill(&self, literals: &[String]) -> String {
        let mut out = String::with_capacity(self.sql_len + 16 * literals.len());
        for seg in &self.segs {
            match seg {
                Seg::Text(text) => out.push_str(text),
                Seg::Param(index) => {
                    let literal = literals.get(*index).map_or("NULL", String::as_str);
                    // `-` + `-5` must not fuse into a comment.
                    if out.ends_with('-') && literal.starts_with('-') {
                        out.push(' ');
                    }
                    out.push_str(literal);
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Eligibility, literal abstraction and keys
// ---------------------------------------------------------------------------

/// Words whose translation depends on the session (or that read server
/// state), so a cached translation could be wrong for another session.
const SESSION_WORDS: &[&str] = &[
    "LAST_INSERT_ID",
    "CONNECTION_ID",
    "CURRENT_USER",
    "SESSION_USER",
    "SYSTEM_USER",
    "ROW_COUNT",
    "FOUND_ROWS",
    "DEFAULT",
    "INFORMATION_SCHEMA",
    "PERFORMANCE_SCHEMA",
    "MYSQL",
    "SYS",
];
/// Session functions that are also common column names: only a call counts.
const SESSION_CALLS: &[&str] = &["USER", "DATABASE", "SCHEMA"];

/// The leading verb when the statement may be cached at all.
pub(crate) fn eligible(tokens: &[Token]) -> Option<&'static str> {
    let first = tokens.first()?;
    if first.kind != Kind::Word {
        return None;
    }
    let verb = ["SELECT", "INSERT", "UPDATE", "DELETE", "REPLACE"]
        .into_iter()
        .find(|verb| first.text.eq_ignore_ascii_case(verb))?;
    for (index, token) in tokens.iter().enumerate() {
        match token.kind {
            Kind::UserVar | Kind::SysVar | Kind::Raw | Kind::Literal => return None,
            Kind::Word | Kind::QuotedIdent => {
                let text = if token.kind == Kind::Word {
                    &token.text
                } else {
                    &token.value
                };
                if text.len() >= MARK.len() && text[..MARK.len()].eq_ignore_ascii_case(MARK) {
                    return None;
                }
                if token.kind == Kind::Word
                    && (SESSION_WORDS
                        .iter()
                        .any(|word| text.eq_ignore_ascii_case(word))
                        || (verb == "SELECT" && text.eq_ignore_ascii_case("INTO"))
                        || (SESSION_CALLS
                            .iter()
                            .any(|word| text.eq_ignore_ascii_case(word))
                            && tokens.get(index + 1).is_some_and(|t| t.is_op("("))))
                {
                    return None;
                }
                if token.kind == Kind::QuotedIdent
                    && SESSION_WORDS[8..]
                        .iter()
                        .any(|word| text.eq_ignore_ascii_case(word))
                {
                    return None;
                }
            }
            _ => {}
        }
    }
    Some(verb)
}

fn upper_in(token: &Token, words: &[&str]) -> bool {
    token.kind == Kind::Word && words.iter().any(|w| token.text.eq_ignore_ascii_case(w))
}

/// Index of the `(` matching the `)` at `close`.
fn open_of(tokens: &[Token], close: usize) -> Option<usize> {
    let mut depth = 0_usize;
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

/// True when the token at `index` is a value position whose literal cannot
/// change how the statement is translated: right of a comparison/arithmetic
/// operator or a few keywords, or an element of a `VALUES (...)` / `IN (...)`
/// list.
fn value_position(tokens: &[Token], index: usize) -> bool {
    let Some(prev) = index.checked_sub(1).map(|i| &tokens[i]) else {
        return false;
    };
    if prev.kind == Kind::Word {
        return upper_in(
            prev,
            &[
                "LIKE", "REGEXP", "RLIKE", "LIMIT", "OFFSET", "AND", "OR", "XOR", "NOT", "BETWEEN",
                "WHEN", "THEN", "ELSE",
            ],
        );
    }
    if prev.kind != Kind::Op {
        return false;
    }
    match prev.text.as_str() {
        "=" | "<>" | "!=" | "<" | ">" | "<=" | ">=" | "<=>" | "*" | "/" | "%" => true,
        "+" | "-" => {
            // Binary operator after an operand, otherwise a sign: look through it.
            let operand_before = index.checked_sub(2).map(|i| &tokens[i]).is_some_and(|t| {
                matches!(t.kind, Kind::Number | Kind::Str | Kind::Placeholder)
                    || t.is_op(")")
                    || (t.kind == Kind::Word
                        && !upper_in(t, &["SELECT", "WHERE", "AND", "OR", "NOT", "BY", "ON"]))
            });
            operand_before || value_position(tokens, index - 1)
        }
        "(" | "," => list_position(tokens, index - 1),
        _ => false,
    }
}

/// `at` is a `(` or `,`: true when it belongs to a `VALUES (...)` or
/// `IN (...)` list.
fn list_position(tokens: &[Token], at: usize) -> bool {
    // Find the enclosing open paren.
    let mut open = if tokens[at].is_op("(") {
        at
    } else {
        let mut depth = 0_usize;
        let mut found = None;
        for index in (0..at).rev() {
            if tokens[index].is_op(")") {
                depth += 1;
            } else if tokens[index].is_op("(") {
                if depth == 0 {
                    found = Some(index);
                    break;
                }
                depth -= 1;
            }
        }
        match found {
            Some(open) => open,
            None => return false,
        }
    };
    // `VALUES (..), (..), (..)`: walk back over the preceding tuples.
    loop {
        let Some(before) = open.checked_sub(1).map(|i| &tokens[i]) else {
            return false;
        };
        if upper_in(before, &["VALUES", "VALUE", "IN"]) {
            return true;
        }
        if before.is_op(",") && open >= 2 && tokens[open - 2].is_op(")") {
            match open_of(tokens, open - 2) {
                Some(previous) => open = previous,
                None => return false,
            }
        } else {
            return false;
        }
    }
}

/// A literal taken out of a statement.
pub(crate) struct Lit<'a>(pub &'a Token);

impl Lit<'_> {
    /// The literal as SQLite SQL text (what the translator would emit).
    pub(crate) fn sql(&self) -> String {
        self.0.render()
    }

    /// The literal as a statement argument.
    pub(crate) fn arg(&self) -> Arg {
        let token = self.0;
        if token.kind == Kind::Number {
            if let Ok(number) = token.text.parse::<i64>() {
                return Arg::Int(number);
            }
            return token
                .text
                .parse::<f64>()
                .map_or_else(|_| Arg::Num(token.text.clone()), Arg::Real);
        }
        Arg::Text(token.value.clone())
    }
}

/// Replaces literals in value positions with placeholder tokens. The
/// placeholder's `value` records the literal kind so that numbers and strings
/// do not share a key.
pub(crate) fn abstract_literals(tokens: &[Token]) -> (Vec<Token>, Vec<Lit<'_>>) {
    let mut out = Vec::with_capacity(tokens.len());
    let mut literals = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        let simple_number = token.kind == Kind::Number
            && token.text.bytes().all(|b| b.is_ascii_digit() || b == b'.')
            && !token.text.is_empty();
        let simple_string =
            token.kind == Kind::Str && token.bytes.is_none() && !token.value.contains(['\\', '\0']);
        if (simple_number || simple_string) && value_position(tokens, index) {
            out.push(Token {
                kind: Kind::Placeholder,
                text: "?".into(),
                value: if simple_number { "n" } else { "s" }.into(),
                bytes: None,
                ws: token.ws.clone(),
            });
            literals.push(Lit(token));
        } else {
            out.push(token.clone());
        }
    }
    (out, literals)
}

/// Canonical text of a (possibly abstracted) token stream, prefixed with the
/// protocol tag and database name. `None` when a token cannot be keyed.
pub(crate) fn key_of(database: &str, tag: char, tokens: &[Token]) -> Option<String> {
    use std::fmt::Write as _;
    let mut key = String::with_capacity(32 + tokens.len() * 8);
    key.push(tag);
    key.push_str(database);
    for token in tokens {
        key.push('\u{1f}');
        match token.kind {
            Kind::Word | Kind::Number | Kind::HexStr | Kind::Op => key.push_str(&token.text),
            Kind::QuotedIdent => {
                let _ = write!(key, "`{}:{}", token.value.len(), token.value);
            }
            Kind::Str => {
                let _ = write!(key, "'{}:{}", token.value.len(), token.value);
                if let Some(bytes) = &token.bytes {
                    for byte in bytes {
                        let _ = write!(key, "{byte:02x}");
                    }
                }
            }
            Kind::Placeholder => {
                key.push('?');
                key.push_str(&token.value);
            }
            Kind::UserVar | Kind::SysVar | Kind::Literal | Kind::Raw => return None,
        }
    }
    Some(key)
}

// ---------------------------------------------------------------------------
// Building plans
// ---------------------------------------------------------------------------

/// True when no marker sits inside a quoted string or identifier (where the
/// generic path would escape the value differently, e.g. unaliased select
/// list expressions that become column labels).
fn markers_unquoted(sql: &str) -> bool {
    let mut quote: Option<char> = None;
    let mut rest = sql;
    while let Some(c) = rest.chars().next() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {
                if rest.starts_with(MARK) {
                    return false;
                }
            }
            None => {
                if c == '\'' || c == '"' || c == '`' {
                    quote = Some(c);
                }
            }
        }
        rest = &rest[c.len_utf8()..];
    }
    true
}

fn split_markers(sql: &str, params: usize) -> Option<Vec<Seg>> {
    if !markers_unquoted(sql) {
        return None;
    }
    let mut segs = Vec::new();
    let mut rest = sql;
    while let Some(at) = rest.find(MARK) {
        if at > 0 {
            segs.push(Seg::Text(rest[..at].to_owned()));
        }
        let digits = &rest[at + MARK.len()..];
        let end = digits.find('Z')?;
        let index: usize = digits[..end].parse().ok()?;
        if index >= params {
            return None;
        }
        segs.push(Seg::Param(index));
        rest = &digits[end + 1..];
    }
    if !rest.is_empty() {
        segs.push(Seg::Text(rest.to_owned()));
    }
    Some(segs)
}

/// Parameters that follow `LIMIT` / `OFFSET` (spliced into the text).
fn limit_params(tokens: &[Token]) -> Vec<bool> {
    let mut flags = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        if token.kind != Kind::Placeholder {
            continue;
        }
        let before = index.checked_sub(1).map(|i| &tokens[i]);
        let direct = before.is_some_and(|b| upper_in(b, &["LIMIT", "OFFSET"]));
        let pair = before.is_some_and(|b| b.is_op(","))
            && index >= 3
            && tokens[index - 2].kind == Kind::Placeholder
            && upper_in(&tokens[index - 3], &["LIMIT"]);
        flags.push(direct || pair);
    }
    flags
}

fn make_bind(tokens: &[Token], segs: &[Seg], len: usize) -> Bind {
    let spliced = limit_params(tokens);
    let mut position = Vec::with_capacity(spliced.len());
    let mut next = 0;
    for &is_spliced in &spliced {
        position.push((!is_spliced).then(|| {
            next += 1;
            next - 1
        }));
    }
    let fixed_sql = (!spliced.contains(&true)).then(|| render_bound(segs, &position, &[], len));
    Bind {
        position,
        fixed_sql,
    }
}

/// SQL text with `?1`, `?2`, ... for bound parameters and the given literals
/// for spliced ones.
fn render_bound(
    segs: &[Seg],
    position: &[Option<usize>],
    literals: &[String],
    len: usize,
) -> Arc<str> {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(len + 8);
    for seg in segs {
        match seg {
            Seg::Text(text) => out.push_str(text),
            Seg::Param(index) => match position.get(*index).copied().flatten() {
                Some(bound) => {
                    let _ = write!(out, "?{}", bound + 1);
                }
                None => out.push_str(literals.get(*index).map_or("NULL", String::as_str)),
            },
        }
    }
    Arc::from(out)
}

impl Plan {
    /// The bound-parameter form of the statement for one execution: SQL text
    /// and the values to bind. `args` are consumed.
    pub(crate) fn bound(&self, args: Vec<Arg>) -> Option<(Arc<str>, Vec<Param>)> {
        let bind = self.bind.as_ref()?;
        let sql = match &bind.fixed_sql {
            Some(sql) => Arc::clone(sql),
            None => {
                let literals: Vec<String> = args
                    .iter()
                    .zip(&bind.position)
                    .map(|(arg, position)| {
                        if position.is_none() {
                            arg.sql()
                        } else {
                            String::new()
                        }
                    })
                    .collect();
                render_bound(&self.segs, &bind.position, &literals, self.sql_len)
            }
        };
        let params = args
            .into_iter()
            .zip(&bind.position)
            .filter(|(_, position)| position.is_some())
            .map(|(arg, _)| arg.param())
            .collect();
        Some((sql, params))
    }
}

/// Whether the parameters can be bound instead of spliced into the text.
///
/// A bound value differs from a literal only where SQLite reads the literal
/// itself: `ORDER BY 2` / `GROUP BY 2` are column ordinals. Placeholders
/// after ORDER/GROUP are therefore only bound as `LIMIT` / `OFFSET` values.
/// Every parameter must also appear in the translated text (the bound count
/// has to match exactly) and none may sit inside a quoted string.
fn bindable(tokens: &[Token], segs: &[Seg], n_params: usize) -> bool {
    let mut seen = vec![false; n_params];
    for seg in segs {
        if let Seg::Param(index) = seg {
            seen[*index] = true;
        }
    }
    if seen.iter().any(|used| !used) {
        return false;
    }
    let ordering = tokens
        .iter()
        .position(|t| upper_in(t, &["ORDER", "GROUP", "WINDOW", "PARTITION"]));
    let Some(ordering) = ordering else {
        return true;
    };
    tokens.iter().enumerate().skip(ordering).all(|(index, t)| {
        if t.kind != Kind::Placeholder {
            return true;
        }
        let before = tokens.get(index.wrapping_sub(1));
        before.is_some_and(|b| upper_in(b, &["LIMIT", "OFFSET"]))
            || (before.is_some_and(|b| b.is_op(","))
                && index >= 3
                && tokens[index - 2].kind == Kind::Placeholder
                && upper_in(&tokens[index - 3], &["LIMIT"]))
    })
}

/// Builds a plan from tokens whose placeholders stand for parameters.
/// `None` when the statement is not cacheable.
pub(crate) fn build(tokens: &[Token], env: &Env<'_>) -> Option<Plan> {
    let first = eligible(tokens)?;
    let n_params = translate::placeholder_count(tokens);
    if n_params > MAX_PARAMS {
        return None;
    }
    let marks: Vec<String> = (0..n_params).map(|i| format!("{MARK}{i}Z")).collect();
    let mut statements = translate::translate(tokens, env, &marks).ok()?;
    if statements.len() != 1 {
        return None;
    }
    let sql = statements.remove(0);
    let segs = split_markers(&sql, n_params)?;
    let bind = bindable(tokens, &segs, n_params).then(|| make_bind(tokens, &segs, sql.len()));
    Some(Plan {
        first,
        is_query: first == "SELECT",
        select_info: (first == "SELECT")
            .then(|| SelectInfo::parse(tokens))
            .flatten(),
        n_params,
        segs,
        sql_len: sql.len(),
        bind,
        described: Mutex::new(None),
        typing: Mutex::new(None),
        exec_us: AtomicU32::new(u32::MAX),
    })
}

/// Builds a plan for a text-protocol statement. `abstracted` / `literals`
/// come from [`abstract_literals`]; the plan is only returned when it
/// reproduces the generic translation of `tokens` exactly.
pub(crate) fn build_text(
    tokens: &[Token],
    abstracted: &[Token],
    literals: &[Lit<'_>],
    env: &Env<'_>,
) -> Option<Plan> {
    let plan = build(abstracted, env)?;
    if plan.n_params != literals.len() {
        return None;
    }
    let reference = translate::translate(tokens, env, &[]).ok()?;
    let values: Vec<String> = literals.iter().map(Lit::sql).collect();
    (reference.len() == 1 && reference[0] == plan.fill(&values)).then_some(plan)
}

// ---------------------------------------------------------------------------
// The shared cache
// ---------------------------------------------------------------------------

/// What the cache knows about a statement shape.
#[derive(Clone)]
pub(crate) enum Entry {
    Plan(Arc<Plan>),
    /// Building failed or is unsafe: do not try again.
    Uncacheable,
}

#[derive(Default)]
struct Shard {
    hot: HashMap<String, Entry>,
    cold: HashMap<String, Entry>,
}

/// Bounded, sharded plan cache shared by all sessions of a server.
pub(crate) struct PlanCache {
    shards: Vec<Mutex<Shard>>,
    epoch: AtomicU64,
}

impl PlanCache {
    pub(crate) fn new() -> Self {
        Self {
            shards: (0..SHARDS).map(|_| Mutex::new(Shard::default())).collect(),
            epoch: AtomicU64::new(1),
        }
    }

    fn shard(&self, key: &str) -> &Mutex<Shard> {
        // FNV-1a: cheap and good enough to spread statement shapes.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in key.bytes() {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
        }
        &self.shards[(hash >> 32) as usize % SHARDS]
    }

    pub(crate) fn get(&self, key: &str) -> Option<Entry> {
        let mut shard = self.shard(key).lock().ok()?;
        if let Some(entry) = shard.hot.get(key) {
            return Some(entry.clone());
        }
        let (name, entry) = shard.cold.remove_entry(key)?;
        shard.hot.insert(name, entry.clone());
        Some(entry)
    }

    pub(crate) fn put(&self, key: String, entry: Entry) {
        let Ok(mut shard) = self.shard(&key).lock() else {
            return;
        };
        if shard.hot.len() >= SHARD_CAPACITY {
            shard.cold = std::mem::take(&mut shard.hot);
        }
        shard.hot.insert(key, entry);
    }

    /// Schema generation used to validate cached typing.
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    /// Called around every DDL statement.
    pub(crate) fn bump_epoch(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|s| {
                let s = s.lock().unwrap();
                s.hot.len() + s.cold.len()
            })
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::tokenize;

    fn with_env<T>(run: impl FnOnce(&Env<'_>) -> T) -> T {
        let user_vars = HashMap::new();
        let tables = HashMap::new();
        let system_var = |_: &str| None;
        let env = Env {
            database: "game",
            user: "root",
            host: "localhost",
            connection_id: 7,
            last_insert_id: 0,
            system_var: &system_var,
            user_vars: &user_vars,
            tables: &tables,
            fk_names: &[],
            indexes: &[],
        };
        run(&env)
    }

    /// Plan-based translation of `sql`, or `None` when it is not cacheable.
    fn via_plan(sql: &str) -> Option<String> {
        let tokens = tokenize(sql).unwrap();
        let (abstracted, literals) = abstract_literals(&tokens);
        let plan = with_env(|env| build_text(&tokens, &abstracted, &literals, env))?;
        let values: Vec<String> = literals.iter().map(Lit::sql).collect();
        Some(plan.fill(&values))
    }

    fn generic(sql: &str) -> String {
        let tokens = tokenize(sql).unwrap();
        with_env(|env| translate::translate(&tokens, env, &[]).unwrap().remove(0))
    }

    fn shape(sql: &str) -> String {
        let tokens = tokenize(sql).unwrap();
        let (abstracted, _) = abstract_literals(&tokens);
        key_of("game", 't', &abstracted).unwrap()
    }

    #[test]
    fn statements_differing_in_literals_share_a_shape() {
        assert_eq!(
            shape("SELECT * FROM characters WHERE id = 5"),
            shape("SELECT * FROM characters WHERE id = 99999")
        );
        assert_eq!(
            shape("UPDATE c SET x = 1, y = 2 WHERE id = 3"),
            shape("UPDATE c SET x = 7, y = 8 WHERE id = 9")
        );
        assert_eq!(
            shape("INSERT INTO t (a, b) VALUES (1, 'x'), (2, 'y')"),
            shape("INSERT INTO t (a, b) VALUES (3, 'q'), (4, 'it''s')")
        );
        assert_ne!(
            shape("SELECT * FROM c WHERE id = 5"),
            shape("SELECT * FROM c WHERE id = 'five'"),
            "number and string literals do not share a shape"
        );
        assert_ne!(
            shape("SELECT * FROM c WHERE id = 5"),
            shape("SELECT * FROM c WHERE name = 5")
        );
    }

    #[test]
    fn plan_translation_equals_generic_translation() {
        for sql in [
            "SELECT * FROM characters WHERE id = 5",
            "SELECT id, name FROM c WHERE a = 1 AND b <> 'x' OR c >= -3.5",
            "SELECT id FROM inv WHERE character_id IN (1, 2, 3) ORDER BY id DESC LIMIT 10",
            "SELECT id FROM inv ORDER BY id LIMIT 20, 10",
            "SELECT id FROM inv WHERE n BETWEEN 1 AND 9 LIMIT 5 OFFSET 2",
            "INSERT INTO t (a, b, c) VALUES (1, 'it''s', 3), (4, 'z', 6)",
            "UPDATE c SET map = 3, x = 10, y = 20 WHERE id = 77",
            "DELETE FROM inv WHERE id = 12",
            "INSERT INTO log (m) VALUES ('back\\\\slash')",
            "SELECT * FROM t WHERE name LIKE 'a%'",
            "SELECT * FROM t WHERE name LIKE 'a\\\\_%'",
            "REPLACE INTO t (a) VALUES (1)",
            "UPDATE t SET a = a + 1 WHERE id = 3",
        ] {
            if let Some(planned) = via_plan(sql) {
                assert_eq!(planned, generic(sql), "{sql}");
            }
        }
        assert!(via_plan("SELECT * FROM t WHERE id = 5").is_some());
        assert!(via_plan("UPDATE t SET a = 1 WHERE id = 2").is_some());
        assert!(via_plan("INSERT INTO t (a) VALUES (1)").is_some());
    }

    #[test]
    fn session_dependent_statements_are_not_cached() {
        for sql in [
            "SELECT LAST_INSERT_ID()",
            "SELECT DATABASE()",
            "SELECT @x",
            "SELECT @@autocommit",
            "SELECT * FROM information_schema.tables",
            "SELECT CONNECTION_ID()",
            "SELECT USER()",
            "INSERT INTO t (a) VALUES (DEFAULT)",
            "SELECT a INTO @v FROM t",
        ] {
            let tokens = tokenize(sql).unwrap();
            assert!(eligible(&tokens).is_none(), "{sql}");
        }
        for sql in [
            "SELECT user FROM t",
            "SELECT `database` FROM t WHERE id = 1",
        ] {
            let tokens = tokenize(sql).unwrap();
            assert!(eligible(&tokens).is_some(), "{sql}");
        }
        assert!(eligible(&tokenize("CREATE TABLE t (a INT)").unwrap()).is_none());
    }

    #[test]
    fn literals_with_special_translation_stay_in_the_key() {
        // Backslashes switch LIKE to `ESCAPE '\'`; they must not be abstracted.
        let plain = shape("SELECT * FROM t WHERE n LIKE 'a%'");
        let escaped = shape("SELECT * FROM t WHERE n LIKE 'a\\\\%'");
        assert_ne!(plain, escaped);
    }

    #[test]
    fn prepared_plans_fill_parameters() {
        let tokens = tokenize("UPDATE c SET x = ?, y = ? WHERE id = ?").unwrap();
        let plan = with_env(|env| build(&tokens, env)).unwrap();
        assert_eq!(plan.n_params, 3);
        let sql = plan.fill(&["1".into(), "-2".into(), "'k'".into()]);
        assert_eq!(sql, "UPDATE c SET x = 1, y = -2 WHERE id = 'k'");
        let tokens = tokenize("SELECT a FROM t WHERE id = - ?").unwrap();
        let plan = with_env(|env| build(&tokens, env)).unwrap();
        assert_eq!(plan.fill(&["-5".into()]), "SELECT a FROM t WHERE id = - -5");
        let tokens = tokenize("SELECT a FROM t LIMIT ?, ?").unwrap();
        let plan = with_env(|env| build(&tokens, env)).unwrap();
        assert_eq!(
            plan.fill(&["10".into(), "5".into()]),
            "SELECT a FROM t LIMIT 5 OFFSET 10"
        );
    }

    #[test]
    fn placeholders_are_bound_except_where_a_literal_is_read_as_an_ordinal() {
        let bound = |sql: &str| {
            let tokens = tokenize(sql).unwrap();
            let plan = with_env(|env| build(&tokens, env)).unwrap();
            let args = (0..plan.n_params)
                .map(|i| Arg::Int(i as i64 + 10))
                .collect();
            plan.bound(args)
                .map(|(sql, params)| (sql.to_string(), params.len()))
        };
        assert_eq!(
            bound("UPDATE c SET x = ?, y = ? WHERE id = ?"),
            Some(("UPDATE c SET x = ?1, y = ?2 WHERE id = ?3".into(), 3))
        );
        // LIMIT / OFFSET values are spliced as literals, the rest is bound.
        assert_eq!(
            bound("SELECT a FROM t WHERE id IN (?, ?) ORDER BY a LIMIT ? OFFSET ?"),
            Some((
                "SELECT a FROM t WHERE id IN (?1, ?2) ORDER BY a LIMIT 12 OFFSET 13".into(),
                2
            ))
        );
        assert_eq!(
            bound("SELECT a FROM t ORDER BY a LIMIT ?, ?"),
            Some(("SELECT a FROM t ORDER BY a LIMIT 11 OFFSET 10".into(), 0))
        );
        assert!(bound("SELECT a, b FROM t ORDER BY ?").is_none());
        assert!(bound("SELECT a FROM t GROUP BY ?").is_none());
        let tokens = tokenize("SELECT a FROM t LIMIT ?").unwrap();
        let plan = with_env(|env| build(&tokens, env)).unwrap();
        let (first, _) = plan.bound(vec![Arg::Int(5)]).unwrap();
        let (second, _) = plan.bound(vec![Arg::Int(6)]).unwrap();
        assert_eq!(
            (&*first, &*second),
            ("SELECT a FROM t LIMIT 5", "SELECT a FROM t LIMIT 6")
        );
    }

    #[test]
    fn literals_become_arguments() {
        let tokens = tokenize("SELECT a FROM t WHERE n = 12 AND f > 1.5 AND s = 'x'").unwrap();
        let (_, literals) = abstract_literals(&tokens);
        let args: Vec<Arg> = literals.iter().map(Lit::arg).collect();
        assert_eq!(
            args,
            vec![Arg::Int(12), Arg::Real(1.5), Arg::Text("x".into())]
        );
    }

    #[test]
    fn cache_is_bounded() {
        let cache = PlanCache::new();
        for i in 0..(SHARDS * SHARD_CAPACITY * 4) {
            cache.put(format!("key{i}"), Entry::Uncacheable);
        }
        assert!(cache.len() <= SHARDS * SHARD_CAPACITY * 2);
        cache.put("hot".into(), Entry::Uncacheable);
        assert!(cache.get("hot").is_some());
        let before = cache.epoch();
        cache.bump_epoch();
        assert!(cache.epoch() > before);
    }

    #[test]
    fn plan_timing_switches_between_inline_and_blocking() {
        let tokens = tokenize("SELECT a FROM t WHERE id = ?").unwrap();
        let plan = with_env(|env| build(&tokens, env)).unwrap();
        assert!(!plan.run_inline(), "unmeasured plans use the blocking pool");
        plan.record(Duration::from_micros(20));
        assert!(plan.run_inline());
        plan.record(Duration::from_millis(30));
        assert!(!plan.run_inline(), "a slow run is noticed at once");
    }
}
