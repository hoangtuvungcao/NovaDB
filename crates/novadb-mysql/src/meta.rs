//! Catalog emulation: system variables, `SHOW ...` statements and a subset
//! of `information_schema`, all expressed as SQLite queries over
//! `sqlite_schema` and the `pragma_*` table-valued functions.

use crate::lexer::{Kind, SqlError, SqlResult, Token, quote_ident, quote_str};

/// A system or user variable value.
#[derive(Debug, Clone, PartialEq)]
pub enum VarValue {
    Int(i64),
    Text(String),
    Null,
}

impl VarValue {
    /// SQL literal for substitution into queries.
    pub fn to_sql(&self) -> String {
        match self {
            Self::Int(value) => value.to_string(),
            Self::Text(value) => quote_str(value),
            Self::Null => "NULL".into(),
        }
    }

    /// Text shown by `SHOW VARIABLES`.
    pub fn display(&self) -> String {
        match self {
            Self::Int(value) => value.to_string(),
            Self::Text(value) => value.clone(),
            Self::Null => String::new(),
        }
    }
}

/// Default session/global variables reported to clients.
pub fn default_variables(max_connections: usize) -> Vec<(&'static str, VarValue)> {
    use VarValue::{Int, Null, Text};
    let text = |value: &str| Text(value.to_owned());
    vec![
        ("auto_increment_increment", Int(1)),
        ("basedir", text("/usr/")),
        ("big_tables", Int(0)),
        ("datadir", text("/var/lib/mysql/")),
        ("event_scheduler", text("OFF")),
        ("group_concat_max_len", Int(1_024)),
        ("hostname", text("novadb")),
        ("innodb_buffer_pool_size", Int(134_217_728)),
        ("innodb_lock_wait_timeout", Int(50)),
        ("innodb_version", text("8.0.36")),
        ("lc_messages", text("en_US")),
        ("lc_time_names", text("en_US")),
        ("local_infile", Int(0)),
        ("max_execution_time", Int(0)),
        ("max_join_size", Int(i64::MAX)),
        ("max_prepared_stmt_count", Int(16_382)),
        ("net_read_timeout", Int(30)),
        ("port", Int(3306)),
        ("read_only", Int(0)),
        ("secure_file_priv", Null),
        ("socket", text("/tmp/mysql.sock")),
        ("sql_big_selects", Int(1)),
        ("sql_notes", Int(1)),
        ("sql_quote_show_create", Int(1)),
        ("sql_require_primary_key", Int(0)),
        ("sql_safe_updates", Int(0)),
        ("sql_warnings", Int(0)),
        ("super_read_only", Int(0)),
        ("table_open_cache", Int(4_000)),
        ("thread_handling", text("one-thread-per-connection")),
        ("auto_increment_offset", Int(1)),
        ("autocommit", Int(1)),
        ("character_set_client", text("utf8mb4")),
        ("character_set_connection", text("utf8mb4")),
        ("character_set_database", text("utf8mb4")),
        ("character_set_filesystem", text("binary")),
        ("character_set_results", text("utf8mb4")),
        ("character_set_server", text("utf8mb4")),
        ("character_set_system", text("utf8mb3")),
        ("collation_connection", text("utf8mb4_general_ci")),
        ("collation_database", text("utf8mb4_general_ci")),
        ("collation_server", text("utf8mb4_general_ci")),
        (
            "default_authentication_plugin",
            text("mysql_native_password"),
        ),
        ("default_storage_engine", text("InnoDB")),
        ("error_count", Int(0)),
        ("explicit_defaults_for_timestamp", Int(1)),
        ("foreign_key_checks", Int(1)),
        ("have_ssl", text("DISABLED")),
        ("init_connect", text("")),
        ("interactive_timeout", Int(28_800)),
        ("license", text("Apache-2.0")),
        ("lower_case_table_names", Int(0)),
        ("max_allowed_packet", Int(crate::packet::MAX_PAYLOAD as i64)),
        (
            "max_connections",
            Int(i64::try_from(max_connections).unwrap_or(i64::MAX)),
        ),
        ("net_buffer_length", Int(16_384)),
        ("net_write_timeout", Int(60)),
        ("performance_schema", Int(0)),
        ("protocol_version", Int(10)),
        ("query_cache_size", Int(0)),
        ("query_cache_type", text("OFF")),
        ("sql_auto_is_null", Int(0)),
        (
            "sql_mode",
            text("STRICT_TRANS_TABLES,NO_ENGINE_SUBSTITUTION"),
        ),
        ("sql_select_limit", Int(i64::MAX)),
        ("storage_engine", text("InnoDB")),
        ("system_time_zone", text("UTC")),
        ("time_zone", text("+00:00")),
        ("transaction_isolation", text("SERIALIZABLE")),
        ("transaction_read_only", Int(0)),
        ("tx_isolation", text("SERIALIZABLE")),
        ("tx_read_only", Int(0)),
        ("unique_checks", Int(1)),
        ("version", text(crate::SERVER_VERSION)),
        (
            "version_comment",
            text("NovaDB MySQL-compatible gateway (SQLite storage)"),
        ),
        ("version_compile_machine", text(std::env::consts::ARCH)),
        ("version_compile_os", text(std::env::consts::OS)),
        ("wait_timeout", Int(28_800)),
        ("warning_count", Int(0)),
    ]
}

/// SQL predicate selecting user tables/views from `sqlite_schema` alias `m`.
const USER_OBJECTS: &str = "m.type IN ('table', 'view') AND m.name NOT LIKE 'sqlite\\_%' ESCAPE '\\' \
     AND m.name NOT LIKE '\\_novadb\\_%' ESCAPE '\\'";

/// Name of the CTE that emulates `information_schema.<table>`.
pub fn information_schema_cte(table: &str) -> String {
    quote_ident(&format!("information_schema.{table}"))
}

/// `pk = 1 AND type = INTEGER AND single-column pk` on `pragma_table_info` alias `p` of table expr `t`.
pub(crate) fn rowid_alias(p: &str, table: &str) -> String {
    format!(
        "({p}.pk = 1 AND upper({p}.type) = 'INTEGER' AND (SELECT count(*) FROM pragma_table_info({table}) AS k WHERE k.pk > 0) = 1)"
    )
}

/// SQL expression for the declared MySQL type of a column: the type as stored,
/// or the `/*my:type*/` comment kept for rowid aliases and by older gateways.
/// `tsql`, `cn`, `ptype` are SQL expressions for the table DDL, the column name
/// and the SQLite column type.
fn declared_type_sql(tsql: &str, cn: &str, ptype: &str) -> String {
    let annotation = |pattern: &str| {
        format!(
            "CASE WHEN instr({tsql}, {pattern}) > 0 THEN substr({tsql}, instr({tsql}, {pattern}) + length({pattern}), \
             instr(substr({tsql}, instr({tsql}, {pattern}) + length({pattern})), '*/') - 1) END"
        )
    };
    let legacy = annotation(&format!("('\"' || {cn} || '\" ' || {ptype} || ' /*my:')"));
    let alias = annotation(&format!(
        "('\"' || {cn} || '\" INTEGER PRIMARY KEY AUTOINCREMENT /*my:')"
    ));
    format!("coalesce({alias}, {legacy}, {ptype})")
}

/// Foreign key constraint names: `(table, SQLite foreign key id, name)`.
pub type FkNames = [(String, i64, String)];

/// SQL expression for the name of the foreign key `f` of table `m`: the name
/// declared in the DDL (as `SHOW CREATE TABLE` reports it), else `<t>_ibfk_<n>`.
fn fk_name_expr(fk_names: &FkNames) -> String {
    if fk_names.is_empty() {
        return "m.name || '_ibfk_' || (f.id + 1)".to_owned();
    }
    let mut sql = String::from("CASE m.name || '#' || f.id");
    for (table, id, name) in fk_names {
        sql.push_str(&format!(
            " WHEN {} THEN {}",
            quote_str(&format!("{table}#{id}")),
            quote_str(name)
        ));
    }
    sql.push_str(" ELSE m.name || '_ibfk_' || (f.id + 1) END");
    sql
}

fn information_schema_definition(table: &str, db: &str, fk_names: &FkNames) -> SqlResult<String> {
    let db = quote_str(db);
    let fk_name = fk_name_expr(fk_names);
    let definition = match table {
        "schemata" => format!(
            "SELECT 'def' AS CATALOG_NAME, {db} AS SCHEMA_NAME, 'utf8mb4' AS DEFAULT_CHARACTER_SET_NAME, \
             'utf8mb4_general_ci' AS DEFAULT_COLLATION_NAME, NULL AS SQL_PATH, 'NO' AS DEFAULT_ENCRYPTION"
        ),
        "tables" => format!(
            "SELECT 'def' AS TABLE_CATALOG, {db} AS TABLE_SCHEMA, m.name AS TABLE_NAME, \
             CASE m.type WHEN 'view' THEN 'VIEW' ELSE 'BASE TABLE' END AS TABLE_TYPE, \
             CASE m.type WHEN 'view' THEN NULL ELSE 'InnoDB' END AS ENGINE, 10 AS VERSION, \
             'Dynamic' AS ROW_FORMAT, NULL AS TABLE_ROWS, NULL AS AVG_ROW_LENGTH, NULL AS DATA_LENGTH, \
             NULL AS MAX_DATA_LENGTH, NULL AS INDEX_LENGTH, NULL AS DATA_FREE, NULL AS \"AUTO_INCREMENT\", \
             NULL AS CREATE_TIME, NULL AS UPDATE_TIME, NULL AS CHECK_TIME, \
             'utf8mb4_general_ci' AS TABLE_COLLATION, NULL AS CHECKSUM, '' AS CREATE_OPTIONS, \
             CASE m.type WHEN 'view' THEN 'VIEW' ELSE '' END AS TABLE_COMMENT \
             FROM sqlite_schema AS m WHERE {USER_OBJECTS}"
        ),
        "columns" => {
            // Level 1: one row per column. Level 2: the declared MySQL type
            // (`dt`): the type as written in the DDL, or the `/*my:..*/`
            // comment kept for rowid aliases and by older gateways.
            let declared = declared_type_sql("tsql", "cn", "ptype");
            let base = "lower(CASE WHEN instr(dt, '(') > 0 THEN trim(substr(dt, 1, instr(dt, '(') - 1)) ELSE trim(dt) END)";
            let args = "substr(dt, instr(dt, '(') + 1, instr(dt, ')') - instr(dt, '(') - 1)";
            format!(
                "SELECT 'def' AS TABLE_CATALOG, {db} AS TABLE_SCHEMA, tn AS TABLE_NAME, cn AS COLUMN_NAME, \
                 cid + 1 AS ORDINAL_POSITION, dv AS COLUMN_DEFAULT, \
                 CASE WHEN nn = 1 OR pk > 0 THEN 'NO' ELSE 'YES' END AS IS_NULLABLE, \
                 CASE b WHEN 'integer' THEN 'int' WHEN 'bool' THEN 'tinyint' WHEN 'boolean' THEN 'tinyint' ELSE b END AS DATA_TYPE, \
                 CASE WHEN b IN ('varchar', 'char') AND instr(dt, '(') > 0 THEN CAST({args} AS INTEGER) \
                   WHEN b = 'tinytext' THEN 255 WHEN b = 'text' THEN 65535 WHEN b = 'mediumtext' THEN 16777215 \
                   WHEN b = 'longtext' THEN 4294967295 END AS CHARACTER_MAXIMUM_LENGTH, \
                 CASE WHEN b IN ('varchar', 'char') AND instr(dt, '(') > 0 THEN CAST({args} AS INTEGER) * 4 \
                   WHEN b = 'tinytext' THEN 255 WHEN b = 'text' THEN 65535 WHEN b = 'mediumtext' THEN 16777215 \
                   WHEN b = 'longtext' THEN 4294967295 END AS CHARACTER_OCTET_LENGTH, \
                 CASE WHEN b IN ('tinyint', 'bool', 'boolean') THEN 3 WHEN b = 'smallint' THEN 5 WHEN b = 'mediumint' THEN 7 \
                   WHEN b IN ('int', 'integer') THEN 10 WHEN b = 'bigint' THEN 19 \
                   WHEN b IN ('decimal', 'numeric') THEN CASE WHEN instr(dt, '(') > 0 THEN CAST(substr({args}, 1, instr({args} || ',', ',') - 1) AS INTEGER) ELSE 10 END \
                   WHEN b = 'float' THEN 12 WHEN b = 'double' THEN 22 END AS NUMERIC_PRECISION, \
                 CASE WHEN b IN ('tinyint', 'bool', 'boolean', 'smallint', 'mediumint', 'int', 'integer', 'bigint') THEN 0 \
                   WHEN b IN ('decimal', 'numeric') THEN CASE WHEN instr(dt, ',') > 0 THEN CAST(trim(substr({args}, instr({args}, ',') + 1)) AS INTEGER) ELSE 0 END END AS NUMERIC_SCALE, \
                 CASE WHEN b IN ('datetime', 'timestamp', 'time') THEN CASE WHEN instr(dt, '(') > 0 THEN CAST({args} AS INTEGER) ELSE 0 END END AS DATETIME_PRECISION, \
                 CASE WHEN b IN ('varchar', 'char', 'tinytext', 'text', 'mediumtext', 'longtext') THEN 'utf8mb4' END AS CHARACTER_SET_NAME, \
                 CASE WHEN b IN ('varchar', 'char', 'tinytext', 'text', 'mediumtext', 'longtext') THEN 'utf8mb4_general_ci' END AS COLLATION_NAME, \
                 CASE WHEN lower(trim(dt)) = 'integer' THEN 'int' WHEN lower(trim(dt)) IN ('bool', 'boolean') THEN 'tinyint(1)' ELSE lower(trim(dt)) END AS COLUMN_TYPE, \
                 CASE WHEN pk > 0 THEN 'PRI' ELSE '' END AS COLUMN_KEY, \
                 CASE WHEN ra THEN 'auto_increment' ELSE '' END AS EXTRA, \
                 'select,insert,update,references' AS PRIVILEGES, '' AS COLUMN_COMMENT, \
                 '' AS GENERATION_EXPRESSION, NULL AS SRS_ID \
                 FROM (SELECT *, {base} AS b FROM (SELECT *, {declared} AS dt FROM \
                   (SELECT m.name AS tn, m.sql AS tsql, p.name AS cn, p.cid AS cid, p.type AS ptype, \
                    p.\"notnull\" AS nn, p.dflt_value AS dv, p.pk AS pk, {rowid} AS ra \
                    FROM sqlite_schema AS m JOIN pragma_table_info(m.name) AS p WHERE {USER_OBJECTS}))) AS c",
                rowid = rowid_alias("p", "m.name")
            )
        }
        "statistics" => format!(
            "SELECT 'def' AS TABLE_CATALOG, {db} AS TABLE_SCHEMA, m.name AS TABLE_NAME, \
             CASE WHEN il.\"unique\" THEN 0 ELSE 1 END AS NON_UNIQUE, {db} AS INDEX_SCHEMA, \
             CASE WHEN il.origin = 'pk' THEN 'PRIMARY' ELSE il.name END AS INDEX_NAME, \
             ii.seqno + 1 AS SEQ_IN_INDEX, ii.name AS COLUMN_NAME, 'A' AS COLLATION, NULL AS CARDINALITY, \
             NULL AS SUB_PART, NULL AS PACKED, '' AS NULLABLE, 'BTREE' AS INDEX_TYPE, '' AS COMMENT, \
             '' AS INDEX_COMMENT, 'YES' AS IS_VISIBLE, NULL AS EXPRESSION \
             FROM sqlite_schema AS m JOIN pragma_index_list(m.name) AS il JOIN pragma_index_info(il.name) AS ii \
             WHERE {USER_OBJECTS} \
             UNION ALL SELECT 'def', {db}, m.name, 0, {db}, 'PRIMARY', 1, p.name, 'A', NULL, NULL, NULL, '', \
             'BTREE', '', '', 'YES', NULL FROM sqlite_schema AS m JOIN pragma_table_info(m.name) AS p \
             WHERE {USER_OBJECTS} AND {rowid}",
            rowid = rowid_alias("p", "m.name")
        ),
        "key_column_usage" => format!(
            "SELECT 'def' AS CONSTRAINT_CATALOG, {db} AS CONSTRAINT_SCHEMA, 'PRIMARY' AS CONSTRAINT_NAME, \
             'def' AS TABLE_CATALOG, {db} AS TABLE_SCHEMA, m.name AS TABLE_NAME, p.name AS COLUMN_NAME, \
             p.pk AS ORDINAL_POSITION, NULL AS POSITION_IN_UNIQUE_CONSTRAINT, NULL AS REFERENCED_TABLE_SCHEMA, \
             NULL AS REFERENCED_TABLE_NAME, NULL AS REFERENCED_COLUMN_NAME \
             FROM sqlite_schema AS m JOIN pragma_table_info(m.name) AS p WHERE {USER_OBJECTS} AND p.pk > 0 \
             UNION ALL SELECT 'def', {db}, {fk_name}, 'def', {db}, m.name, f.\"from\", \
             f.seq + 1, f.seq + 1, {db}, f.\"table\", f.\"to\" \
             FROM sqlite_schema AS m JOIN pragma_foreign_key_list(m.name) AS f WHERE {USER_OBJECTS}"
        ),
        "table_constraints" => format!(
            "SELECT 'def' AS CONSTRAINT_CATALOG, {db} AS CONSTRAINT_SCHEMA, 'PRIMARY' AS CONSTRAINT_NAME, \
             {db} AS TABLE_SCHEMA, m.name AS TABLE_NAME, 'PRIMARY KEY' AS CONSTRAINT_TYPE, 'YES' AS ENFORCED \
             FROM sqlite_schema AS m WHERE {USER_OBJECTS} AND m.type = 'table' \
             AND EXISTS (SELECT 1 FROM pragma_table_info(m.name) AS p WHERE p.pk > 0) \
             UNION ALL SELECT 'def', {db}, il.name, {db}, m.name, 'UNIQUE', 'YES' \
             FROM sqlite_schema AS m JOIN pragma_index_list(m.name) AS il \
             WHERE {USER_OBJECTS} AND il.\"unique\" AND il.origin <> 'pk' \
             UNION ALL SELECT DISTINCT 'def', {db}, {fk_name}, {db}, m.name, 'FOREIGN KEY', 'YES' \
             FROM sqlite_schema AS m JOIN pragma_foreign_key_list(m.name) AS f WHERE {USER_OBJECTS}"
        ),
        "referential_constraints" => format!(
            "SELECT 'def' AS CONSTRAINT_CATALOG, {db} AS CONSTRAINT_SCHEMA, \
             {fk_name} AS CONSTRAINT_NAME, 'def' AS UNIQUE_CONSTRAINT_CATALOG, \
             {db} AS UNIQUE_CONSTRAINT_SCHEMA, 'PRIMARY' AS UNIQUE_CONSTRAINT_NAME, 'NONE' AS MATCH_OPTION, \
             f.on_update AS UPDATE_RULE, f.on_delete AS DELETE_RULE, m.name AS TABLE_NAME, \
             f.\"table\" AS REFERENCED_TABLE_NAME \
             FROM sqlite_schema AS m JOIN pragma_foreign_key_list(m.name) AS f WHERE {USER_OBJECTS} AND f.seq = 0"
        ),
        "views" => format!(
            "SELECT 'def' AS TABLE_CATALOG, {db} AS TABLE_SCHEMA, m.name AS TABLE_NAME, m.sql AS VIEW_DEFINITION, \
             'NONE' AS CHECK_OPTION, 'NO' AS IS_UPDATABLE, 'root@%' AS DEFINER, 'DEFINER' AS SECURITY_TYPE, \
             'utf8mb4' AS CHARACTER_SET_CLIENT, 'utf8mb4_general_ci' AS COLLATION_CONNECTION \
             FROM sqlite_schema AS m WHERE {USER_OBJECTS} AND m.type = 'view'"
        ),
        "check_constraints" => empty(&[
            "CONSTRAINT_CATALOG", "CONSTRAINT_SCHEMA", "CONSTRAINT_NAME", "CHECK_CLAUSE",
        ]),
        "routines" => empty(&[
            "SPECIFIC_NAME", "ROUTINE_CATALOG", "ROUTINE_SCHEMA", "ROUTINE_NAME", "ROUTINE_TYPE",
            "DATA_TYPE", "ROUTINE_DEFINITION", "CREATED", "LAST_ALTERED", "ROUTINE_COMMENT", "DEFINER",
        ]),
        "parameters" => empty(&[
            "SPECIFIC_CATALOG", "SPECIFIC_SCHEMA", "SPECIFIC_NAME", "ORDINAL_POSITION", "PARAMETER_MODE",
            "PARAMETER_NAME", "DATA_TYPE", "DTD_IDENTIFIER", "ROUTINE_TYPE",
        ]),
        "triggers" => empty(&[
            "TRIGGER_CATALOG", "TRIGGER_SCHEMA", "TRIGGER_NAME", "EVENT_MANIPULATION",
            "EVENT_OBJECT_SCHEMA", "EVENT_OBJECT_TABLE", "ACTION_STATEMENT", "ACTION_TIMING",
        ]),
        "events" => empty(&["EVENT_CATALOG", "EVENT_SCHEMA", "EVENT_NAME", "DEFINER", "EVENT_TYPE", "STATUS"]),
        "partitions" => empty(&["TABLE_CATALOG", "TABLE_SCHEMA", "TABLE_NAME", "PARTITION_NAME", "TABLE_ROWS"]),
        "character_sets" => "SELECT 'utf8mb4' AS CHARACTER_SET_NAME, 'utf8mb4_general_ci' AS DEFAULT_COLLATE_NAME, \
             'UTF-8 Unicode' AS DESCRIPTION, 4 AS MAXLEN"
            .to_owned(),
        "collations" => "SELECT 'utf8mb4_general_ci' AS COLLATION_NAME, 'utf8mb4' AS CHARACTER_SET_NAME, \
             45 AS ID, 'Yes' AS IS_DEFAULT, 'Yes' AS IS_COMPILED, 1 AS SORTLEN, 'PAD SPACE' AS PAD_ATTRIBUTE"
            .to_owned(),
        "engines" => "SELECT 'InnoDB' AS ENGINE, 'DEFAULT' AS SUPPORT, \
             'NovaDB SQLite storage' AS COMMENT, 'YES' AS TRANSACTIONS, 'NO' AS XA, 'YES' AS SAVEPOINTS"
            .to_owned(),
        other => {
            return Err(SqlError::Unsupported(format!(
                "information_schema.{other} is not supported by NovaDB"
            )));
        }
    };
    Ok(definition)
}

fn empty(columns: &[&str]) -> String {
    let columns: Vec<String> = columns
        .iter()
        .map(|c| format!("NULL AS {}", quote_ident(c)))
        .collect();
    format!("SELECT {} WHERE 0", columns.join(", "))
}

/// Prepends CTEs for the referenced `information_schema` tables.
pub fn with_information_schema(
    sql: &str,
    tables: &[String],
    db: &str,
    fk_names: &FkNames,
) -> SqlResult<String> {
    if tables.is_empty() {
        return Ok(sql.to_owned());
    }
    let mut definitions = Vec::with_capacity(tables.len());
    for table in tables {
        definitions.push(format!(
            "{} AS ({})",
            information_schema_cte(table),
            information_schema_definition(table, db, fk_names)?
        ));
    }
    let definitions = definitions.join(", ");
    let upper = sql.get(..15).unwrap_or(sql).to_ascii_uppercase();
    if upper.starts_with("WITH RECURSIVE ") {
        Ok(format!("WITH RECURSIVE {definitions}, {}", &sql[15..]))
    } else if upper.starts_with("WITH ") {
        Ok(format!("WITH {definitions}, {}", &sql[5..]))
    } else {
        Ok(format!("WITH {definitions} {sql}"))
    }
}

/// Builds a SELECT that yields constant rows with the given column labels.
pub fn values_sql(columns: &[&str], rows: &[Vec<VarValue>]) -> String {
    if rows.is_empty() {
        return empty(columns);
    }
    rows.iter()
        .map(|row| {
            let cells: Vec<String> = row
                .iter()
                .zip(columns)
                .map(|(value, column)| format!("{} AS {}", value.to_sql(), quote_ident(column)))
                .collect();
            format!("SELECT {}", cells.join(", "))
        })
        .collect::<Vec<_>>()
        .join(" UNION ALL ")
}

/// Optional `LIKE` / `WHERE` filter of a SHOW statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    Like(String),
    Where(Vec<Token>),
}

/// Parsed `SHOW` statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Show {
    Databases,
    Tables {
        full: bool,
        database: Option<String>,
    },
    Columns {
        full: bool,
        table: String,
        database: Option<String>,
    },
    Index {
        table: String,
        database: Option<String>,
    },
    CreateTable {
        table: String,
        database: Option<String>,
    },
    CreateDatabase {
        name: String,
    },
    TableStatus {
        database: Option<String>,
    },
    Variables,
    Status,
    Warnings,
    Engines,
    Charset,
    Collation,
    Processlist,
    Grants,
    /// A recognized statement that always returns no rows.
    Empty(&'static [&'static str]),
}

/// Parses a SHOW (or DESCRIBE) statement.
pub fn parse_show(tokens: &[Token]) -> SqlResult<(Show, Option<Filter>)> {
    let mut i = 1;
    let word = |index: usize| tokens.get(index).map(Token::upper).unwrap_or_default();
    let ident = |index: usize| -> SqlResult<String> {
        tokens
            .get(index)
            .and_then(Token::ident)
            .map(str::to_owned)
            .ok_or_else(|| SqlError::Syntax("expected an identifier".into()))
    };
    // `db.table` or `table`.
    let qualified = |index: usize| -> SqlResult<(String, Option<String>, usize)> {
        let first = ident(index)?;
        if tokens.get(index + 1).is_some_and(|t| t.is_op(".")) {
            Ok((ident(index + 2)?, Some(first), index + 3))
        } else {
            Ok((first, None, index + 1))
        }
    };
    let mut full = false;
    while matches!(
        word(i).as_str(),
        "FULL" | "GLOBAL" | "SESSION" | "EXTENDED" | "STORAGE" | "LOCAL"
    ) {
        full |= word(i) == "FULL";
        i += 1;
    }
    let from_db = |index: usize| -> SqlResult<(Option<String>, usize)> {
        if matches!(word(index).as_str(), "FROM" | "IN") {
            Ok((Some(ident(index + 1)?), index + 2))
        } else {
            Ok((None, index))
        }
    };
    let show = match word(i).as_str() {
        "DATABASES" | "SCHEMAS" => {
            i += 1;
            Show::Databases
        }
        "TABLES" => {
            let (database, next) = from_db(i + 1)?;
            i = next;
            Show::Tables { full, database }
        }
        "COLUMNS" | "FIELDS" => {
            if !matches!(word(i + 1).as_str(), "FROM" | "IN") {
                return Err(SqlError::Syntax("expected FROM after SHOW COLUMNS".into()));
            }
            let (table, mut database, next) = qualified(i + 2)?;
            let (db, next) = from_db(next)?;
            database = db.or(database);
            i = next;
            Show::Columns {
                full,
                table,
                database,
            }
        }
        "INDEX" | "INDEXES" | "KEYS" => {
            if !matches!(word(i + 1).as_str(), "FROM" | "IN") {
                return Err(SqlError::Syntax("expected FROM after SHOW INDEX".into()));
            }
            let (table, mut database, next) = qualified(i + 2)?;
            let (db, next) = from_db(next)?;
            database = db.or(database);
            i = next;
            Show::Index { table, database }
        }
        "CREATE" => match word(i + 1).as_str() {
            "TABLE" | "VIEW" => {
                let (table, database, next) = qualified(i + 2)?;
                i = next;
                Show::CreateTable { table, database }
            }
            "DATABASE" | "SCHEMA" => {
                let mut index = i + 2;
                if word(index) == "IF" {
                    index += 3;
                }
                let name = ident(index)?;
                i = index + 1;
                Show::CreateDatabase { name }
            }
            other => {
                return Err(SqlError::Unsupported(format!(
                    "SHOW CREATE {other} is not supported"
                )));
            }
        },
        "TABLE" if word(i + 1) == "STATUS" => {
            let (database, next) = from_db(i + 2)?;
            i = next;
            Show::TableStatus { database }
        }
        "VARIABLES" => {
            i += 1;
            Show::Variables
        }
        "STATUS" => {
            i += 1;
            Show::Status
        }
        "WARNINGS" | "ERRORS" => {
            i = tokens.len();
            Show::Warnings
        }
        "ENGINES" => {
            i += 1;
            Show::Engines
        }
        "CHARACTER" | "CHARSET" => {
            i += if word(i) == "CHARACTER" { 2 } else { 1 };
            Show::Charset
        }
        "COLLATION" => {
            i += 1;
            Show::Collation
        }
        "PROCESSLIST" => {
            i += 1;
            Show::Processlist
        }
        "GRANTS" => {
            i = tokens.len();
            Show::Grants
        }
        "TRIGGERS" => {
            let (_, next) = from_db(i + 1)?;
            i = next;
            Show::Empty(&[
                "Trigger",
                "Event",
                "Table",
                "Statement",
                "Timing",
                "Created",
                "sql_mode",
                "Definer",
            ])
        }
        "PROCEDURE" | "FUNCTION" if word(i + 1) == "STATUS" => {
            i += 2;
            Show::Empty(&[
                "Db",
                "Name",
                "Type",
                "Definer",
                "Modified",
                "Created",
                "Security_type",
                "Comment",
                "character_set_client",
                "collation_connection",
                "Database Collation",
            ])
        }
        "EVENTS" => {
            let (_, next) = from_db(i + 1)?;
            i = next;
            Show::Empty(&["Db", "Name", "Definer", "Time zone", "Type", "Status"])
        }
        "OPEN" | "PLUGINS" | "PRIVILEGES" | "MASTER" | "BINARY" | "SLAVE" | "REPLICA"
        | "PROFILES" => {
            i = tokens.len();
            Show::Empty(&["Name", "Status"])
        }
        other => {
            return Err(SqlError::Unsupported(format!(
                "SHOW {other} is not supported by NovaDB"
            )));
        }
    };
    let filter = match word(i).as_str() {
        "LIKE" => match tokens.get(i + 1) {
            Some(token) if token.kind == Kind::Str => Some(Filter::Like(token.value.clone())),
            _ => return Err(SqlError::Syntax("LIKE requires a string pattern".into())),
        },
        "WHERE" => Some(Filter::Where(tokens[i + 1..].to_vec())),
        _ => None,
    };
    Ok((show, filter))
}

/// Parses `DESCRIBE t` / `DESC t` / `EXPLAIN t` into SHOW COLUMNS.
pub fn parse_describe(tokens: &[Token]) -> Option<(Show, Option<Filter>)> {
    if tokens.get(1)?.is_any_word(&[
        "SELECT",
        "INSERT",
        "UPDATE",
        "DELETE",
        "REPLACE",
        "WITH",
        "FORMAT",
        "ANALYZE",
        "EXTENDED",
        "PARTITIONS",
        "QUERY",
        "VALUES",
        "TABLE",
    ]) {
        return None;
    }
    let first = tokens.get(1)?.ident()?.to_owned();
    let (table, database, next) = if tokens.get(2).is_some_and(|t| t.is_op(".")) {
        (tokens.get(3)?.ident()?.to_owned(), Some(first), 4)
    } else {
        (first, None, 2)
    };
    let filter = tokens.get(next).and_then(|t| match t.kind {
        Kind::Str => Some(Filter::Like(t.value.clone())),
        Kind::Word | Kind::QuotedIdent => t.ident().map(|c| Filter::Like(c.to_owned())),
        _ => None,
    });
    if tokens.len() > next + 1 {
        return None;
    }
    Some((
        Show::Columns {
            full: false,
            table,
            database,
        },
        filter,
    ))
}

/// SQL listing user tables.
pub fn tables_sql(db: &str, full: bool) -> String {
    let label = quote_ident(&format!("Tables_in_{db}"));
    let kind = if full {
        ", CASE m.type WHEN 'view' THEN 'VIEW' ELSE 'BASE TABLE' END AS \"Table_type\""
    } else {
        ""
    };
    format!(
        "SELECT m.name AS {label}{kind} FROM sqlite_schema AS m WHERE {USER_OBJECTS} ORDER BY m.name"
    )
}

/// SQL checking whether a user table or view exists.
pub fn table_exists_sql(table: &str) -> String {
    format!(
        "SELECT m.type AS t FROM sqlite_schema AS m WHERE {USER_OBJECTS} AND m.name = {}",
        quote_str(table)
    )
}

/// SQL for `SHOW [FULL] COLUMNS`.
pub fn columns_sql(table: &str, full: bool) -> String {
    let t = quote_str(table);
    let collation = if full {
        "CASE WHEN upper(p.type) LIKE '%CHAR%' OR upper(p.type) LIKE '%TEXT%' OR upper(p.type) LIKE '%CLOB%' \
         THEN 'utf8mb4_general_ci' ELSE NULL END AS \"Collation\", "
    } else {
        ""
    };
    let extra = if full {
        ", 'select,insert,update,references' AS \"Privileges\", '' AS \"Comment\""
    } else {
        ""
    };
    format!(
        "SELECT p.name AS \"Field\", CASE WHEN lower(trim(p.dt)) = 'integer' THEN 'int' \
         WHEN lower(trim(p.dt)) IN ('bool', 'boolean') THEN 'tinyint(1)' ELSE lower(trim(p.dt)) END AS \"Type\", {collation}\
         CASE WHEN p.\"notnull\" = 1 OR p.pk > 0 THEN 'NO' ELSE 'YES' END AS \"Null\", \
         CASE WHEN p.pk > 0 THEN 'PRI' \
         WHEN EXISTS (SELECT 1 FROM pragma_index_list({t}) AS il WHERE il.\"unique\" = 1 \
           AND (SELECT count(*) FROM pragma_index_info(il.name)) = 1 \
           AND (SELECT ii.name FROM pragma_index_info(il.name) AS ii) = p.name) THEN 'UNI' \
         WHEN EXISTS (SELECT 1 FROM pragma_index_list({t}) AS il JOIN pragma_index_info(il.name) AS ii \
           WHERE ii.seqno = 0 AND ii.name = p.name) THEN 'MUL' ELSE '' END AS \"Key\", \
         p.dflt_value AS \"Default\", \
         CASE WHEN {rowid} THEN 'auto_increment' ELSE '' END AS \"Extra\"{extra} \
         FROM (SELECT q.*, {declared} AS dt FROM (SELECT p.*, p.name AS cn, p.type AS ptype, \
           (SELECT sql FROM sqlite_schema WHERE name = {t}) AS tsql FROM pragma_table_info({t}) AS p) AS q) AS p \
         ORDER BY p.cid",
        rowid = rowid_alias("p", &t),
        declared = declared_type_sql("tsql", "cn", "ptype")
    )
}

/// SQL listing `name`, `dflt` and `rowid` for each column of `table`.
pub fn column_defaults_sql(table: &str) -> String {
    let t = quote_str(table);
    format!(
        "SELECT p.name AS name, p.dflt_value AS dflt, {rowid} AS rowid \
         FROM (SELECT q.*, {declared} AS dt FROM (SELECT p.*, p.name AS cn, p.type AS ptype, \
           (SELECT sql FROM sqlite_schema WHERE name = {t}) AS tsql FROM pragma_table_info({t}) AS p) AS q) AS p \
         ORDER BY p.cid",
        rowid = rowid_alias("p", &t),
        declared = declared_type_sql("tsql", "cn", "ptype")
    )
}

/// SQL for `SHOW INDEX`.
pub fn index_sql(table: &str) -> String {
    let t = quote_str(table);
    format!(
        "SELECT * FROM (SELECT {t} AS \"Table\", CASE WHEN il.\"unique\" THEN 0 ELSE 1 END AS \"Non_unique\", \
         CASE WHEN il.origin = 'pk' THEN 'PRIMARY' ELSE il.name END AS \"Key_name\", \
         ii.seqno + 1 AS \"Seq_in_index\", ii.name AS \"Column_name\", 'A' AS \"Collation\", \
         NULL AS \"Cardinality\", NULL AS \"Sub_part\", NULL AS \"Packed\", '' AS \"Null\", \
         'BTREE' AS \"Index_type\", '' AS \"Comment\", '' AS \"Index_comment\", 'YES' AS \"Visible\", \
         NULL AS \"Expression\" \
         FROM pragma_index_list({t}) AS il JOIN pragma_index_info(il.name) AS ii \
         UNION ALL SELECT {t}, 0, 'PRIMARY', 1, p.name, 'A', NULL, NULL, NULL, '', 'BTREE', '', '', 'YES', NULL \
         FROM pragma_table_info({t}) AS p WHERE {rowid}) \
         ORDER BY \"Key_name\" <> 'PRIMARY', \"Key_name\", \"Seq_in_index\"",
        rowid = rowid_alias("p", &t)
    )
}

/// SQL for `SHOW CREATE TABLE` (returns SQLite DDL).
pub fn create_table_sql(table: &str) -> String {
    format!(
        "SELECT m.name AS \"Table\", m.sql AS \"Create Table\" FROM sqlite_schema AS m \
         WHERE {USER_OBJECTS} AND m.name = {}",
        quote_str(table)
    )
}

/// SQL for `SHOW TABLE STATUS`.
pub fn table_status_sql() -> String {
    format!(
        "SELECT m.name AS \"Name\", CASE m.type WHEN 'view' THEN NULL ELSE 'InnoDB' END AS \"Engine\", \
         10 AS \"Version\", 'Dynamic' AS \"Row_format\", NULL AS \"Rows\", NULL AS \"Avg_row_length\", \
         NULL AS \"Data_length\", NULL AS \"Max_data_length\", NULL AS \"Index_length\", NULL AS \"Data_free\", \
         NULL AS \"Auto_increment\", NULL AS \"Create_time\", NULL AS \"Update_time\", NULL AS \"Check_time\", \
         'utf8mb4_general_ci' AS \"Collation\", NULL AS \"Checksum\", '' AS \"Create_options\", \
         CASE m.type WHEN 'view' THEN 'VIEW' ELSE '' END AS \"Comment\" \
         FROM sqlite_schema AS m WHERE {USER_OBJECTS} ORDER BY m.name"
    )
}

/// Wraps `base` with a LIKE filter on `column`.
pub fn like_filter(base: &str, column: &str, pattern: &str) -> String {
    let escape = if pattern.contains('\\') {
        " ESCAPE '\\'"
    } else {
        ""
    };
    format!(
        "SELECT * FROM ({base}) WHERE {} LIKE {}{escape}",
        quote_ident(column),
        quote_str(pattern)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::tokenize;

    fn show(sql: &str) -> (Show, Option<Filter>) {
        parse_show(&tokenize(sql).unwrap()).unwrap()
    }

    #[test]
    fn parses_common_show_statements() {
        assert_eq!(show("SHOW DATABASES").0, Show::Databases);
        assert_eq!(
            show("SHOW FULL TABLES FROM `shop` LIKE 'u%'"),
            (
                Show::Tables {
                    full: true,
                    database: Some("shop".into())
                },
                Some(Filter::Like("u%".into()))
            )
        );
        assert_eq!(
            show("SHOW FULL COLUMNS FROM `users` FROM shop").0,
            Show::Columns {
                full: true,
                table: "users".into(),
                database: Some("shop".into())
            }
        );
        assert_eq!(
            show("SHOW INDEX FROM shop.users").0,
            Show::Index {
                table: "users".into(),
                database: Some("shop".into())
            }
        );
        assert_eq!(
            show("SHOW CREATE TABLE `t`").0,
            Show::CreateTable {
                table: "t".into(),
                database: None
            }
        );
        assert!(matches!(
            show("SHOW SESSION VARIABLES WHERE Variable_name = 'x'").1,
            Some(Filter::Where(_))
        ));
        assert_eq!(show("SHOW WARNINGS LIMIT 10").0, Show::Warnings);
        assert!(matches!(
            parse_show(&tokenize("SHOW NONSENSE").unwrap()),
            Err(SqlError::Unsupported(_))
        ));
        assert_eq!(
            parse_describe(&tokenize("DESCRIBE `users`").unwrap()),
            Some((
                Show::Columns {
                    full: false,
                    table: "users".into(),
                    database: None
                },
                None
            ))
        );
        assert_eq!(parse_describe(&tokenize("EXPLAIN SELECT 1").unwrap()), None);
    }

    #[test]
    fn values_and_filters() {
        assert_eq!(
            values_sql(
                &["a", "b"],
                &[vec![VarValue::Int(1), VarValue::Text("x'y".into())]]
            ),
            "SELECT 1 AS \"a\", 'x''y' AS \"b\""
        );
        assert_eq!(values_sql(&["a"], &[]), "SELECT NULL AS \"a\" WHERE 0");
        assert_eq!(
            like_filter("SELECT 1 AS \"v\"", "v", "a'%"),
            "SELECT * FROM (SELECT 1 AS \"v\") WHERE \"v\" LIKE 'a''%'"
        );
        assert!(with_information_schema("SELECT 1", &["nope".into()], "d", &[]).is_err());
        assert_eq!(
            with_information_schema(
                "WITH x AS (SELECT 1) SELECT 1",
                &["engines".into()],
                "d",
                &[]
            )
            .unwrap()
            .matches("WITH ")
            .count(),
            1
        );
    }
}
