//! Result column typing from declared table column types.
//!
//! SQLite results carry no type metadata, so the gateway maps simple
//! `SELECT` result columns back to the table columns they came from and
//! uses the column's declared MySQL type (kept in an annotation comment in
//! the stored DDL, see [`crate::ddl::annotation`]) to describe them: `JSON`
//! columns become `MYSQL_TYPE_JSON`, `DATETIME` columns `MYSQL_TYPE_DATETIME`,
//! `DECIMAL(p,s)` `MYSQL_TYPE_NEWDECIMAL`, and so on. Anything that cannot be
//! resolved safely keeps the value-based inference.

use std::collections::HashMap;

use crate::lexer::{Kind, Token, find_top, split_commas, split_top};
use crate::packet::{
    CHARSET_BINARY, CHARSET_UTF8MB4, ColumnDef, FLAG_BINARY, FLAG_BLOB, FLAG_NUM, TYPE_BLOB,
    TYPE_DATE, TYPE_DATETIME, TYPE_DOUBLE, TYPE_FLOAT, TYPE_INT24, TYPE_LONG, TYPE_LONGLONG,
    TYPE_NEWDECIMAL, TYPE_SHORT, TYPE_TINY, TYPE_VAR_STRING,
};

/// `MYSQL_TYPE_JSON`.
pub const TYPE_JSON: u8 = 0xf5;

/// Table schemas keyed by lowercase table name: `(lowercase column, declared type)`.
pub type Schemas = HashMap<String, Vec<(String, String)>>;

// ---------------------------------------------------------------------------
// SELECT analysis
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum SelectItem {
    Star(Option<String>),
    Column {
        qualifier: Option<String>,
        column: String,
        label: String,
    },
}

/// A plain table in the `FROM` clause (`table` is `None` for derived tables).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Source {
    name: String,
    table: Option<String>,
}

/// The shape of a simple `SELECT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectInfo {
    items: Vec<Option<SelectItem>>,
    sources: Vec<Source>,
}

const FROM_END: &[&str] = &[
    "WHERE",
    "GROUP",
    "HAVING",
    "ORDER",
    "LIMIT",
    "WINDOW",
    "FOR",
    "LOCK",
    "INTO",
    "PROCEDURE",
];
const SELECT_MODIFIERS: &[&str] = &[
    "DISTINCT",
    "DISTINCTROW",
    "ALL",
    "SQL_CALC_FOUND_ROWS",
    "SQL_NO_CACHE",
    "SQL_CACHE",
    "HIGH_PRIORITY",
    "STRAIGHT_JOIN",
    "SQL_SMALL_RESULT",
    "SQL_BIG_RESULT",
    "SQL_BUFFER_RESULT",
];

impl SelectInfo {
    /// Analyses a `SELECT ... FROM ...` statement; `None` when the shape is
    /// not understood (set operations, CTEs, no `FROM`).
    pub fn parse(tokens: &[Token]) -> Option<Self> {
        if !tokens.first()?.is_word("SELECT") {
            return None;
        }
        if find_top(tokens, 0, |t| {
            t.is_any_word(&["UNION", "INTERSECT", "EXCEPT"])
        })
        .is_some()
        {
            return None;
        }
        let from = find_top(tokens, 1, |t| t.is_word("FROM"))?;
        let mut start = 1;
        while tokens
            .get(start)
            .is_some_and(|t| t.is_any_word(SELECT_MODIFIERS))
        {
            start += 1;
        }
        let items = split_commas(&tokens[start..from])
            .into_iter()
            .map(parse_item)
            .collect();

        let end = find_top(tokens, from + 1, |t| t.is_any_word(FROM_END)).unwrap_or(tokens.len());
        let clause = &tokens[from + 1..end];
        let mut sources = Vec::new();
        for segment in split_top(clause, |t| t.is_word("JOIN") || t.is_op(",")) {
            if let Some(source) = parse_source(segment) {
                sources.push(source);
            }
        }
        (!sources.is_empty()).then_some(Self { items, sources })
    }

    /// Lowercase names of the plain tables the statement reads.
    pub fn tables(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .sources
            .iter()
            .filter_map(|s| s.table.clone())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Declared MySQL type for each result label (lowercase).
    pub fn resolve(&self, schemas: &Schemas) -> HashMap<String, String> {
        let mut types = HashMap::new();
        let schema_of = |source: &Source| {
            source
                .table
                .as_ref()
                .and_then(|table| schemas.get(table))
                .map(Vec::as_slice)
        };
        for item in self.items.iter().flatten() {
            match item {
                SelectItem::Star(qualifier) => {
                    for source in &self.sources {
                        let matches = qualifier
                            .as_ref()
                            .is_none_or(|q| source.name == *q || source.table.as_ref() == Some(q));
                        if let (true, Some(columns)) = (matches, schema_of(source)) {
                            for (column, declared) in columns {
                                types
                                    .entry(column.clone())
                                    .or_insert_with(|| declared.clone());
                            }
                        }
                    }
                }
                SelectItem::Column {
                    qualifier,
                    column,
                    label,
                } => {
                    let found = self.sources.iter().find_map(|source| {
                        if let Some(q) = qualifier {
                            if source.name != *q && source.table.as_ref() != Some(q) {
                                return None;
                            }
                        }
                        schema_of(source)?
                            .iter()
                            .find(|(name, _)| name == column)
                            .map(|(_, declared)| declared.clone())
                    });
                    if let Some(declared) = found {
                        types.entry(label.clone()).or_insert(declared);
                    }
                }
            }
        }
        types
    }
}

fn ident(token: &Token) -> Option<String> {
    match token.kind {
        Kind::Word | Kind::QuotedIdent => token.ident().map(str::to_lowercase),
        _ => None,
    }
}

fn parse_item(tokens: &[Token]) -> Option<SelectItem> {
    // Optional alias: `... AS label` or `... label`.
    let mut end = tokens.len();
    let mut alias = None;
    if end >= 3 && tokens[end - 2].is_word("AS") {
        alias = ident(&tokens[end - 1]).or_else(|| {
            (tokens[end - 1].kind == Kind::Str).then(|| tokens[end - 1].value.to_lowercase())
        });
        end -= 2;
    }
    let body = &tokens[..end];
    let is_star = |t: &Token| t.is_op("*");
    match body {
        [star] if is_star(star) => Some(SelectItem::Star(None)),
        [q, dot, star] if dot.is_op(".") && is_star(star) => Some(SelectItem::Star(ident(q))),
        [_, d1, q, d2, star] if d1.is_op(".") && d2.is_op(".") && is_star(star) => {
            Some(SelectItem::Star(ident(q)))
        }
        [c] => {
            let column = ident(c)?;
            Some(SelectItem::Column {
                qualifier: None,
                label: alias.unwrap_or_else(|| column.clone()),
                column,
            })
        }
        [q, dot, c] if dot.is_op(".") => {
            let column = ident(c)?;
            Some(SelectItem::Column {
                qualifier: ident(q),
                label: alias.unwrap_or_else(|| column.clone()),
                column,
            })
        }
        [_, d1, q, d2, c] if d1.is_op(".") && d2.is_op(".") => {
            let column = ident(c)?;
            Some(SelectItem::Column {
                qualifier: ident(q),
                label: alias.unwrap_or_else(|| column.clone()),
                column,
            })
        }
        _ => None,
    }
}

fn parse_source(segment: &[Token]) -> Option<Source> {
    let mut tokens = segment;
    while tokens.first().is_some_and(|t| {
        t.is_any_word(&[
            "INNER",
            "LEFT",
            "RIGHT",
            "FULL",
            "OUTER",
            "CROSS",
            "NATURAL",
            "STRAIGHT_JOIN",
        ])
    }) {
        tokens = &tokens[1..];
    }
    let cut = tokens
        .iter()
        .position(|t| t.is_any_word(&["ON", "USING"]))
        .unwrap_or(tokens.len());
    let mut tokens = &tokens[..cut];
    // Join modifiers that precede the next `JOIN` keyword.
    while tokens.last().is_some_and(|t| {
        t.is_any_word(&[
            "INNER",
            "LEFT",
            "RIGHT",
            "FULL",
            "OUTER",
            "CROSS",
            "NATURAL",
            "STRAIGHT_JOIN",
        ])
    }) {
        tokens = &tokens[..tokens.len() - 1];
    }
    if tokens.first()?.is_op("(") {
        let alias = tokens.last().and_then(ident)?;
        return Some(Source {
            name: alias,
            table: None,
        });
    }
    let (table, rest) = match tokens {
        [_, dot, table, rest @ ..] if dot.is_op(".") => (ident(table)?, rest),
        [table, rest @ ..] => (ident(table)?, rest),
        [] => return None,
    };
    let alias = match rest {
        [] => None,
        [as_word, alias, ..] if as_word.is_word("AS") => ident(alias),
        [alias, ..] => ident(alias).filter(|_| !alias_is_hint(alias)),
    };
    Some(Source {
        name: alias.unwrap_or_else(|| table.clone()),
        table: Some(table),
    })
}

fn alias_is_hint(token: &Token) -> bool {
    token.is_any_word(&["USE", "IGNORE", "FORCE", "PARTITION"])
}

// ---------------------------------------------------------------------------
// Declared type -> wire description
// ---------------------------------------------------------------------------

/// How a declared MySQL type is described on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Declared {
    Json,
    Tiny { bool_like: bool },
    Short,
    Int24,
    Long,
    LongLong,
    Float,
    Double,
    Decimal { precision: u32, scale: u8 },
    DateTime { fraction: u8 },
    Date,
    Text { length: u32 },
    VarChar { length: u32 },
    Blob,
    Binary { length: u32 },
}

fn arguments(declared: &str) -> Vec<u32> {
    let Some(open) = declared.find('(') else {
        return Vec::new();
    };
    let close = declared.find(')').unwrap_or(declared.len());
    declared[open + 1..close]
        .split(',')
        .filter_map(|n| n.trim().parse().ok())
        .collect()
}

/// Parses a declared type such as `varchar(255)` or `decimal(10,2)`.
pub fn parse_declared(declared: &str) -> Option<Declared> {
    let lower = declared.trim().to_ascii_lowercase();
    let base = lower.split('(').next().unwrap_or_default().trim();
    let base = base
        .trim_end_matches(" unsigned")
        .trim_end_matches(" zerofill");
    let args = arguments(&lower);
    let first = args.first().copied();
    Some(match base {
        "json" => Declared::Json,
        "bool" | "boolean" => Declared::Tiny { bool_like: true },
        "tinyint" => Declared::Tiny {
            bool_like: first == Some(1),
        },
        "smallint" => Declared::Short,
        "mediumint" => Declared::Int24,
        "int" | "integer" => Declared::Long,
        "bigint" => Declared::LongLong,
        "float" => Declared::Float,
        "double" | "double precision" | "real" => Declared::Double,
        "decimal" | "numeric" | "dec" | "fixed" => Declared::Decimal {
            precision: first.unwrap_or(10),
            scale: u8::try_from(args.get(1).copied().unwrap_or(0)).unwrap_or(0),
        },
        "datetime" | "timestamp" => Declared::DateTime {
            fraction: u8::try_from(first.unwrap_or(0)).unwrap_or(0).min(6),
        },
        "date" => Declared::Date,
        "tinytext" => Declared::Text { length: 255 },
        "text" => Declared::Text { length: 65_535 },
        "mediumtext" => Declared::Text { length: 16_777_215 },
        "longtext" => Declared::Text { length: u32::MAX },
        "varchar" | "char" | "nvarchar" | "nchar" => Declared::VarChar {
            length: first.unwrap_or(255).saturating_mul(4),
        },
        "tinyblob" | "blob" | "mediumblob" | "longblob" => Declared::Blob,
        "binary" | "varbinary" => Declared::Binary {
            length: first.unwrap_or(255),
        },
        _ => return None,
    })
}

/// A decoded result cell, as far as typing is concerned.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CellKind<'a> {
    Null,
    Int(i64),
    Float(f64),
    Text(&'a str),
    Bytes,
}

/// Parses `YYYY-MM-DD[ HH:MM:SS[.ffffff]]`.
pub fn parse_datetime(text: &str) -> Option<(u16, u8, u8, u8, u8, u8, u32)> {
    let bytes = text.as_bytes();
    let digits = |range: std::ops::Range<usize>| -> Option<u32> {
        let slice = bytes.get(range)?;
        slice
            .iter()
            .all(u8::is_ascii_digit)
            .then(|| slice.iter().fold(0, |n, d| n * 10 + u32::from(d - b'0')))
    };
    if bytes.len() < 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let (year, month, day) = (digits(0..4)?, digits(5..7)?, digits(8..10)?);
    if month > 12 || day > 31 {
        return None;
    }
    let (mut hour, mut minute, mut second, mut micro) = (0, 0, 0, 0);
    if bytes.len() > 10 {
        if bytes.len() < 19 || bytes[10] != b' ' || bytes[13] != b':' || bytes[16] != b':' {
            return None;
        }
        hour = digits(11..13)?;
        minute = digits(14..16)?;
        second = digits(17..19)?;
        if hour > 23 || minute > 59 || second > 59 {
            return None;
        }
        if bytes.len() > 19 {
            if bytes[19] != b'.' || bytes.len() > 26 || bytes.len() == 20 {
                return None;
            }
            let fraction = digits(20..bytes.len())?;
            micro = fraction * 10_u32.pow(u32::try_from(26 - bytes.len()).ok()?);
        }
    }
    Some((
        u16::try_from(year).ok()?,
        u8::try_from(month).ok()?,
        u8::try_from(day).ok()?,
        u8::try_from(hour).ok()?,
        u8::try_from(minute).ok()?,
        u8::try_from(second).ok()?,
        micro,
    ))
}

impl Declared {
    fn accepts(self, cell: CellKind<'_>) -> bool {
        let int_in = |value: i64, low: i64, high: i64| (low..=high).contains(&value);
        match (self, cell) {
            (_, CellKind::Null) => true,
            (Self::Tiny { .. }, CellKind::Int(v)) => int_in(v, -128, 127),
            (Self::Short, CellKind::Int(v)) => int_in(v, -32_768, 32_767),
            (Self::Int24, CellKind::Int(v)) => int_in(v, -8_388_608, 8_388_607),
            (Self::Long, CellKind::Int(v)) => int_in(v, i64::from(i32::MIN), i64::from(i32::MAX)),
            (Self::LongLong, CellKind::Int(_)) => true,
            (Self::Float | Self::Double, CellKind::Int(_) | CellKind::Float(_)) => true,
            (Self::Decimal { .. }, CellKind::Int(_) | CellKind::Float(_)) => true,
            (Self::Decimal { .. }, CellKind::Text(t)) => t.trim().parse::<f64>().is_ok(),
            (Self::DateTime { .. }, CellKind::Text(t)) => parse_datetime(t).is_some(),
            (Self::Date, CellKind::Text(t)) => t.len() == 10 && parse_datetime(t).is_some(),
            (Self::Json, CellKind::Text(_)) => true,
            (
                Self::Text { .. } | Self::VarChar { .. },
                CellKind::Text(_) | CellKind::Int(_) | CellKind::Float(_),
            ) => true,
            (Self::Blob | Self::Binary { .. }, CellKind::Bytes | CellKind::Text(_)) => true,
            _ => false,
        }
    }

    /// Wire description when every value is compatible with the declared
    /// type; `None` makes the caller fall back to value-based inference.
    pub fn column_def<'a>(
        self,
        name: &str,
        cells: impl IntoIterator<Item = CellKind<'a>>,
    ) -> Option<ColumnDef> {
        if !cells.into_iter().all(|cell| self.accepts(cell)) {
            return None;
        }
        let mut def = ColumnDef::new(name, TYPE_VAR_STRING);
        let numeric = FLAG_NUM | FLAG_BINARY;
        let (column_type, charset, length, flags, decimals) = match self {
            Self::Json => (
                TYPE_JSON,
                CHARSET_BINARY,
                u32::MAX,
                FLAG_BLOB | FLAG_BINARY,
                0,
            ),
            Self::Tiny { bool_like } => (
                TYPE_TINY,
                CHARSET_BINARY,
                if bool_like { 1 } else { 4 },
                numeric,
                0,
            ),
            Self::Short => (TYPE_SHORT, CHARSET_BINARY, 6, numeric, 0),
            Self::Int24 => (TYPE_INT24, CHARSET_BINARY, 9, numeric, 0),
            Self::Long => (TYPE_LONG, CHARSET_BINARY, 11, numeric, 0),
            Self::LongLong => (TYPE_LONGLONG, CHARSET_BINARY, 20, numeric, 0),
            Self::Float => (TYPE_FLOAT, CHARSET_BINARY, 12, numeric, 31),
            Self::Double => (TYPE_DOUBLE, CHARSET_BINARY, 22, numeric, 31),
            Self::Decimal { precision, scale } => (
                TYPE_NEWDECIMAL,
                CHARSET_BINARY,
                precision + u32::from(scale > 0) + 1,
                numeric,
                scale,
            ),
            Self::DateTime { fraction } => (
                TYPE_DATETIME,
                CHARSET_BINARY,
                19 + if fraction > 0 {
                    u32::from(fraction) + 1
                } else {
                    0
                },
                FLAG_BINARY,
                fraction,
            ),
            Self::Date => (TYPE_DATE, CHARSET_BINARY, 10, FLAG_BINARY, 0),
            Self::Text { length } => (TYPE_BLOB, u16::from(CHARSET_UTF8MB4), length, FLAG_BLOB, 0),
            Self::VarChar { length } => (TYPE_VAR_STRING, u16::from(CHARSET_UTF8MB4), length, 0, 0),
            Self::Blob => (
                TYPE_BLOB,
                CHARSET_BINARY,
                u32::MAX,
                FLAG_BLOB | FLAG_BINARY,
                0,
            ),
            Self::Binary { length } => (TYPE_VAR_STRING, CHARSET_BINARY, length, FLAG_BINARY, 0),
        };
        def.column_type = column_type;
        def.charset = charset;
        def.length = length;
        def.flags = flags;
        def.decimals = decimals;
        Some(def)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::tokenize;

    fn info(sql: &str) -> Option<SelectInfo> {
        SelectInfo::parse(&tokenize(sql).unwrap())
    }

    fn schemas() -> Schemas {
        let mut schemas = Schemas::new();
        schemas.insert(
            "users".into(),
            vec![("id".into(), "int".into()), ("meta".into(), "json".into())],
        );
        schemas.insert(
            "posts".into(),
            vec![
                ("id".into(), "bigint".into()),
                ("created".into(), "datetime".into()),
            ],
        );
        schemas
    }

    #[test]
    fn resolves_simple_selects() {
        let types = info("SELECT * FROM users").unwrap().resolve(&schemas());
        assert_eq!(types["meta"], "json");
        let types = info("SELECT `u`.`meta` AS m, p.created, 1 AS one FROM `db`.`users` AS u LEFT JOIN posts p ON p.id = u.id WHERE u.id > 1 ORDER BY u.id")
            .unwrap()
            .resolve(&schemas());
        assert_eq!(types["m"], "json");
        assert_eq!(types["created"], "datetime");
        assert!(!types.contains_key("one"));
        let types = info("SELECT id FROM posts, users")
            .unwrap()
            .resolve(&schemas());
        assert_eq!(types["id"], "bigint", "first matching source wins");
        let types = info("SELECT p.* FROM users u JOIN posts p")
            .unwrap()
            .resolve(&schemas());
        assert_eq!(types["id"], "bigint");
        assert!(info("SELECT 1").is_none());
        assert!(info("SELECT id FROM users UNION SELECT id FROM posts").is_none());
        let derived = info("SELECT meta FROM (SELECT meta FROM users) AS d").unwrap();
        assert!(derived.resolve(&schemas()).is_empty());
        assert_eq!(
            info("SELECT a FROM users u1 JOIN posts ON 1 JOIN users u2")
                .unwrap()
                .tables(),
            ["posts", "users"]
        );
    }

    #[test]
    fn declared_types_map_to_wire_types() {
        let cases = [
            ("json", TYPE_JSON),
            ("tinyint(1)", TYPE_TINY),
            ("smallint", TYPE_SHORT),
            ("int(11)", TYPE_LONG),
            ("bigint", TYPE_LONGLONG),
            ("float", TYPE_FLOAT),
            ("double", TYPE_DOUBLE),
            ("decimal(10,2)", TYPE_NEWDECIMAL),
            ("datetime(6)", TYPE_DATETIME),
            ("timestamp", TYPE_DATETIME),
            ("date", TYPE_DATE),
            ("text", TYPE_BLOB),
            ("varchar(20)", TYPE_VAR_STRING),
            ("longblob", TYPE_BLOB),
        ];
        for (declared, wire) in cases {
            let def = parse_declared(declared)
                .unwrap()
                .column_def("c", [CellKind::Null])
                .unwrap();
            assert_eq!(def.column_type, wire, "{declared}");
        }
        let decimal = parse_declared("decimal(10,2)")
            .unwrap()
            .column_def("c", [])
            .unwrap();
        assert_eq!((decimal.decimals, decimal.length), (2, 12));
        let json = parse_declared("json").unwrap().column_def("c", []).unwrap();
        assert_eq!(json.charset, CHARSET_BINARY);
        assert_eq!(json.flags, FLAG_BLOB | FLAG_BINARY);
        assert!(parse_declared("geometry").is_none());
    }

    #[test]
    fn incompatible_values_fall_back() {
        let int = parse_declared("int").unwrap();
        assert!(
            int.column_def("c", [CellKind::Int(3_000_000_000)])
                .is_none()
        );
        assert!(int.column_def("c", [CellKind::Text("x")]).is_none());
        let dt = parse_declared("datetime").unwrap();
        assert!(
            dt.column_def("c", [CellKind::Text("2024-01-02 03:04:05")])
                .is_some()
        );
        assert!(
            dt.column_def("c", [CellKind::Text("2024-01-02T03:04:05")])
                .is_none()
        );
        assert!(dt.column_def("c", [CellKind::Int(5)]).is_none());
        assert_eq!(
            parse_datetime("2024-01-02 03:04:05.5"),
            Some((2024, 1, 2, 3, 4, 5, 500_000))
        );
        assert_eq!(parse_datetime("2024-01-02"), Some((2024, 1, 2, 0, 0, 0, 0)));
    }
}
