//! `information_schema` / `pg_catalog` emulation.
//!
//! Catalog tables are recognised in the token stream and replaced by
//! common table expressions (CTEs) built from `sqlite_schema` and the
//! `pragma_*` table-valued functions, in the same spirit as the MySQL
//! gateway's `information_schema` emulation. Only the current database
//! (schema `public`) is visible.
//!
//! Array-valued catalog columns (`indkey`, `conkey`, ...) are JSON text
//! inside SQLite and are converted to PostgreSQL array text by the result
//! encoder, guided by the declared types listed here (`INT2[]`, `TEXT[]`).
//! Set-returning functions in select lists (`unnest`, `generate_subscripts`)
//! are rewritten into `json_each` joins.
//!
//! Covered: `pg_namespace`, `pg_class`, `pg_attribute`, `pg_attrdef`,
//! `pg_constraint` (p, u, f), `pg_index`, `pg_type`, `pg_am`, `pg_database`,
//! `pg_roles`, `pg_user`, `pg_settings`, `pg_tables`, `pg_views`,
//! `pg_indexes`, empty `pg_description` / `pg_sequence` / `pg_collation` /
//! `pg_range` / `pg_enum` / `pg_proc` / `pg_opclass` / `pg_inherits` /
//! `pg_depend` / `pg_trigger` / `pg_extension`, and `information_schema`
//! `tables`, `columns`, `table_constraints`, `key_column_usage`,
//! `referential_constraints`, `constraint_column_usage`, `schemata`, `views`,
//! `check_constraints`, `sequences`, `routines`.

use std::collections::BTreeSet;

use crate::analysis::{ColumnInfo, TableInfo};
use crate::lexer::{
    Kind, SqlError, SqlResult, Token, find_close, quote_ident, quote_str, split_commas,
};
use crate::translate::Env;
use crate::type_map::{self, oid};

/// One catalog table: declared columns (for result typing) and the SQL of
/// each `UNION ALL` branch, with one expression per column.
struct Def {
    columns: Vec<(&'static str, &'static str)>,
    branches: Vec<(Vec<String>, String)>,
    deps: &'static [&'static str],
}

const USER_OBJECTS: &str =
    "m.name NOT LIKE 'sqlite\\_%' ESCAPE '\\' AND m.name NOT LIKE '\\_novadb\\_%' ESCAPE '\\'";

const PG_TABLES: &[&str] = &[
    "pg_namespace",
    "pg_class",
    "pg_attribute",
    "pg_attrdef",
    "pg_constraint",
    "pg_index",
    "pg_type",
    "pg_am",
    "pg_database",
    "pg_roles",
    "pg_user",
    "pg_settings",
    "pg_tables",
    "pg_views",
    "pg_indexes",
    "pg_description",
    "pg_sequence",
    "pg_collation",
    "pg_range",
    "pg_enum",
    "pg_proc",
    "pg_opclass",
    "pg_inherits",
    "pg_depend",
    "pg_trigger",
    "pg_extension",
    "pg_statio_all_tables",
    "pg_tablespace",
    "pg_authid",
    "pg_auth_members",
    "pg_namespace_acl",
];

const INFO_TABLES: &[&str] = &[
    "tables",
    "columns",
    "table_constraints",
    "key_column_usage",
    "referential_constraints",
    "constraint_column_usage",
    "schemata",
    "views",
    "check_constraints",
    "sequences",
    "routines",
];

fn info_name(table: &str) -> String {
    format!("information_schema.{table}")
}

fn cols(spec: &[(&'static str, &'static str)]) -> Vec<(&'static str, &'static str)> {
    spec.to_vec()
}

fn exprs(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

/// SQL expression mapping a declared SQLite type to a PostgreSQL type OID.
fn type_oid_sql(decl: &str) -> String {
    let base = format!(
        "upper(trim(CASE WHEN instr({decl}, '(') > 0 THEN substr({decl}, 1, instr({decl}, '(') - 1) ELSE {decl} END))"
    );
    format!(
        "(CASE WHEN {base} IN ('BOOLEAN','BOOL') THEN 16 \
         WHEN {base} IN ('SMALLINT','INT2','SMALLSERIAL') THEN 21 \
         WHEN {base} IN ('INTEGER','INT','INT4','MEDIUMINT','SERIAL','SERIAL4') THEN 23 \
         WHEN {base} IN ('BIGINT','INT8','BIGSERIAL','SERIAL8') THEN 20 \
         WHEN {base} IN ('REAL','FLOAT4') THEN 700 \
         WHEN {base} IN ('FLOAT','DOUBLE','DOUBLE PRECISION','FLOAT8') THEN 701 \
         WHEN {base} IN ('TEXT','CLOB','CITEXT') THEN 25 \
         WHEN {base} IN ('VARCHAR','CHARACTER VARYING') THEN 1043 \
         WHEN {base} IN ('CHAR','CHARACTER') THEN 1042 \
         WHEN {base} IN ('BLOB','BYTEA') THEN 17 \
         WHEN {base} = 'JSON' THEN 114 WHEN {base} = 'JSONB' THEN 3802 WHEN {base} = 'UUID' THEN 2950 \
         WHEN {base} IN ('TIMESTAMP','DATETIME','TIMESTAMP WITHOUT TIME ZONE') THEN 1114 \
         WHEN {base} IN ('TIMESTAMPTZ','TIMESTAMP WITH TIME ZONE') THEN 1184 \
         WHEN {base} = 'DATE' THEN 1082 WHEN {base} IN ('TIME','TIME WITHOUT TIME ZONE') THEN 1083 \
         WHEN {base} IN ('NUMERIC','DECIMAL') THEN 1700 \
         WHEN {base} LIKE '%INT%' THEN 20 WHEN {base} LIKE '%CHAR%' OR {base} LIKE '%TEXT%' THEN 25 \
         WHEN {base} LIKE '%REAL%' OR {base} LIKE '%FLOA%' OR {base} LIKE '%DOUB%' THEN 701 \
         ELSE 25 END)"
    )
}

/// `atttypmod` for `VARCHAR(n)` / `NUMERIC(p,s)` declarations.
fn typmod_sql(decl: &str) -> String {
    format!(
        "(CASE WHEN instr({decl}, '(') > 0 AND (upper({decl}) LIKE 'VARCHAR%' OR upper({decl}) LIKE 'CHAR%' OR upper({decl}) LIKE 'CHARACTER%') \
         THEN CAST(substr({decl}, instr({decl}, '(') + 1, instr({decl}, ')') - instr({decl}, '(') - 1) AS INTEGER) + 4 \
         WHEN instr({decl}, '(') > 0 AND (upper({decl}) LIKE 'NUMERIC%' OR upper({decl}) LIKE 'DECIMAL%') \
         THEN (CAST(substr({decl}, instr({decl}, '(') + 1, instr({decl}, ',') - instr({decl}, '(') - 1) AS INTEGER) * 65536) + \
         CAST(substr({decl}, instr({decl}, ',') + 1, instr({decl}, ')') - instr({decl}, ',') - 1) AS INTEGER) + 4 \
         ELSE -1 END)"
    )
}

fn definition(name: &str, env: &Env<'_>) -> Option<Def> {
    let db = quote_str(env.database);
    let user = quote_str(env.user);
    Some(match name {
        "pg_namespace" => Def {
            columns: cols(&[
                ("oid", "OID"),
                ("nspname", "NAME"),
                ("nspowner", "OID"),
                ("nspacl", "TEXT"),
            ]),
            branches: vec![
                (exprs(&["11", "'pg_catalog'", "10", "NULL"]), String::new()),
                (exprs(&["2200", "'public'", "10", "NULL"]), String::new()),
                (
                    exprs(&["13000", "'information_schema'", "10", "NULL"]),
                    String::new(),
                ),
            ],
            deps: &[],
        },
        "pg_am" => Def {
            columns: cols(&[("oid", "OID"), ("amname", "NAME"), ("amtype", "TEXT")]),
            branches: vec![
                (exprs(&["403", "'btree'", "'i'"]), String::new()),
                (exprs(&["2", "'heap'", "'t'"]), String::new()),
            ],
            deps: &[],
        },
        // Internal: one row per PostgreSQL-style index (incl. constraints).
        "pg_nv_columns" => Def {
            columns: cols(&[]),
            branches: vec![(
                exprs(&[
                    "m.name",
                    "16384 + m.rowid",
                    "p.cid + 1",
                    "p.name",
                    "p.type",
                    &type_oid_sql("p.type"),
                    &typmod_sql("p.type"),
                    "CASE WHEN p.\"notnull\" = 1 OR p.pk > 0 THEN 1 ELSE 0 END",
                    "p.dflt_value",
                    "p.pk",
                    "m.type",
                ]),
                format!(
                    "FROM sqlite_schema AS m JOIN pragma_table_info(m.name) AS p WHERE m.type IN ('table', 'view') AND {USER_OBJECTS}"
                ),
            )],
            deps: &[],
        },
        "pg_nv_indexes" => Def {
            columns: cols(&[]),
            branches: vec![
                (
                    exprs(&[
                        "m.name",
                        "16384 + m.rowid",
                        "CASE il.origin WHEN 'pk' THEN m.name || '_pkey' WHEN 'u' THEN m.name || '_' || \
                         (SELECT group_concat(name, '_') FROM pragma_index_info(il.name)) || '_key' ELSE il.name END",
                        "16384 + COALESCE((SELECT x.rowid FROM sqlite_schema AS x WHERE x.name = il.name AND x.type = 'index'), 0)",
                        "il.\"unique\"",
                        "il.origin",
                        "(SELECT json_group_array(name) FROM (SELECT name FROM pragma_index_info(il.name) ORDER BY seqno))",
                    ]),
                    format!(
                        "FROM sqlite_schema AS m JOIN pragma_index_list(m.name) AS il WHERE m.type = 'table' AND {USER_OBJECTS}"
                    ),
                ),
                (
                    exprs(&[
                        "m.name",
                        "16384 + m.rowid",
                        "m.name || '_pkey'",
                        "5000000 + m.rowid",
                        "1",
                        "'pk'",
                        "json_array(p.name)",
                    ]),
                    format!(
                        "FROM sqlite_schema AS m JOIN pragma_table_info(m.name) AS p WHERE m.type = 'table' AND {USER_OBJECTS} \
                         AND p.pk = 1 AND upper(p.type) = 'INTEGER' \
                         AND (SELECT count(*) FROM pragma_table_info(m.name) AS k WHERE k.pk > 0) = 1 \
                         AND NOT EXISTS (SELECT 1 FROM pragma_index_list(m.name) AS i WHERE i.origin = 'pk')"
                    ),
                ),
            ],
            deps: &[],
        },
        "pg_class" => Def {
            columns: cols(&[
                ("oid", "OID"),
                ("relname", "NAME"),
                ("relnamespace", "OID"),
                ("reltype", "OID"),
                ("reloftype", "OID"),
                ("relowner", "OID"),
                ("relam", "OID"),
                ("relfilenode", "OID"),
                ("reltablespace", "OID"),
                ("relpages", "INTEGER"),
                ("reltuples", "REAL"),
                ("relallvisible", "INTEGER"),
                ("reltoastrelid", "OID"),
                ("relhasindex", "BOOLEAN"),
                ("relisshared", "BOOLEAN"),
                ("relpersistence", "TEXT"),
                ("relkind", "TEXT"),
                ("relnatts", "SMALLINT"),
                ("relchecks", "SMALLINT"),
                ("relhasrules", "BOOLEAN"),
                ("relhastriggers", "BOOLEAN"),
                ("relhassubclass", "BOOLEAN"),
                ("relrowsecurity", "BOOLEAN"),
                ("relforcerowsecurity", "BOOLEAN"),
                ("relispartition", "BOOLEAN"),
                ("relreplident", "TEXT"),
                ("relacl", "TEXT"),
                ("reloptions", "TEXT[]"),
                ("relpartbound", "TEXT"),
            ]),
            branches: vec![
                (
                    exprs(&[
                        "16384 + m.rowid",
                        "m.name",
                        "2200",
                        "0",
                        "0",
                        "10",
                        "2",
                        "0",
                        "0",
                        "0",
                        "-1.0",
                        "0",
                        "0",
                        "CASE WHEN m.type = 'table' AND EXISTS (SELECT 1 FROM pg_nv_indexes AS n WHERE n.tbl = m.name) THEN 1 ELSE 0 END",
                        "0",
                        "'p'",
                        "CASE m.type WHEN 'view' THEN 'v' ELSE 'r' END",
                        "(SELECT count(*) FROM pragma_table_info(m.name))",
                        "0",
                        "0",
                        "0",
                        "0",
                        "0",
                        "0",
                        "0",
                        "'d'",
                        "NULL",
                        "NULL",
                        "NULL",
                    ]),
                    format!(
                        "FROM sqlite_schema AS m WHERE m.type IN ('table', 'view') AND {USER_OBJECTS}"
                    ),
                ),
                (
                    exprs(&[
                        "n.idx_oid",
                        "n.idx_name",
                        "2200",
                        "0",
                        "0",
                        "10",
                        "403",
                        "0",
                        "0",
                        "0",
                        "-1.0",
                        "0",
                        "0",
                        "0",
                        "0",
                        "'p'",
                        "'i'",
                        "json_array_length(n.cols)",
                        "0",
                        "0",
                        "0",
                        "0",
                        "0",
                        "0",
                        "0",
                        "'d'",
                        "NULL",
                        "NULL",
                        "NULL",
                    ]),
                    "FROM pg_nv_indexes AS n".to_owned(),
                ),
            ],
            deps: &["pg_nv_indexes"],
        },
        "pg_attribute" => Def {
            columns: cols(&[
                ("attrelid", "OID"),
                ("attname", "NAME"),
                ("atttypid", "OID"),
                ("attstattarget", "INTEGER"),
                ("attlen", "SMALLINT"),
                ("attnum", "SMALLINT"),
                ("attndims", "INTEGER"),
                ("atttypmod", "INTEGER"),
                ("attnotnull", "BOOLEAN"),
                ("atthasdef", "BOOLEAN"),
                ("attidentity", "TEXT"),
                ("attgenerated", "TEXT"),
                ("attisdropped", "BOOLEAN"),
                ("attislocal", "BOOLEAN"),
                ("attinhcount", "INTEGER"),
                ("attcollation", "OID"),
                ("attacl", "TEXT"),
                ("attoptions", "TEXT[]"),
                ("atthasmissing", "BOOLEAN"),
                ("attstorage", "TEXT"),
                ("attcompression", "TEXT"),
            ]),
            branches: vec![(
                exprs(&[
                    "c.tbl_oid",
                    "c.name",
                    "c.type_oid",
                    "-1",
                    "-1",
                    "c.attnum",
                    "0",
                    "c.typmod",
                    "c.nn",
                    "CASE WHEN c.dflt IS NOT NULL THEN 1 ELSE 0 END",
                    "''",
                    "''",
                    "0",
                    "1",
                    "0",
                    "0",
                    "NULL",
                    "NULL",
                    "0",
                    "'p'",
                    "''",
                ]),
                "FROM pg_nv_columns AS c".to_owned(),
            )],
            deps: &["pg_nv_columns"],
        },
        "pg_attrdef" => Def {
            columns: cols(&[
                ("oid", "OID"),
                ("adrelid", "OID"),
                ("adnum", "SMALLINT"),
                ("adbin", "TEXT"),
            ]),
            branches: vec![(
                exprs(&[
                    "30000000 + c.tbl_oid * 100 + c.attnum",
                    "c.tbl_oid",
                    "c.attnum",
                    "c.dflt",
                ]),
                "FROM pg_nv_columns AS c WHERE c.dflt IS NOT NULL".to_owned(),
            )],
            deps: &["pg_nv_columns"],
        },
        "pg_index" => Def {
            columns: cols(&[
                ("indexrelid", "OID"),
                ("indrelid", "OID"),
                ("indnatts", "SMALLINT"),
                ("indnkeyatts", "SMALLINT"),
                ("indisunique", "BOOLEAN"),
                ("indnullsnotdistinct", "BOOLEAN"),
                ("indisprimary", "BOOLEAN"),
                ("indisexclusion", "BOOLEAN"),
                ("indimmediate", "BOOLEAN"),
                ("indisclustered", "BOOLEAN"),
                ("indisvalid", "BOOLEAN"),
                ("indcheckxmin", "BOOLEAN"),
                ("indisready", "BOOLEAN"),
                ("indislive", "BOOLEAN"),
                ("indisreplident", "BOOLEAN"),
                ("indkey", "INT2VECTOR"),
                ("indcollation", "OIDVECTOR"),
                ("indclass", "OIDVECTOR"),
                ("indoption", "INT2VECTOR"),
                ("indexprs", "TEXT"),
                ("indpred", "TEXT"),
                ("indexdef", "TEXT"),
                ("indcolnames", "TEXT[]"),
            ]),
            branches: vec![(
                exprs(&[
                    "n.idx_oid",
                    "n.tbl_oid",
                    "json_array_length(n.cols)",
                    "json_array_length(n.cols)",
                    "n.is_unique",
                    "0",
                    "CASE WHEN n.origin = 'pk' THEN 1 ELSE 0 END",
                    "0",
                    "1",
                    "0",
                    "1",
                    "0",
                    "1",
                    "1",
                    "0",
                    "(SELECT json_group_array(COALESCE((SELECT k.cid + 1 FROM pragma_table_info(n.tbl) AS k WHERE k.name = e.value), 0)) FROM json_each(n.cols) AS e)",
                    "(SELECT json_group_array(0) FROM json_each(n.cols))",
                    "(SELECT json_group_array(0) FROM json_each(n.cols))",
                    "(SELECT json_group_array(0) FROM json_each(n.cols))",
                    "NULL",
                    "NULL",
                    "'CREATE ' || CASE WHEN n.is_unique THEN 'UNIQUE ' ELSE '' END || 'INDEX ' || n.idx_name || ' ON public.' || n.tbl || \
                     ' USING btree (' || (SELECT group_concat(value, ', ') FROM json_each(n.cols)) || ')'",
                    "n.cols",
                ]),
                "FROM pg_nv_indexes AS n".to_owned(),
            )],
            deps: &["pg_nv_indexes"],
        },
        "pg_constraint" => Def {
            columns: cols(&[
                ("oid", "OID"),
                ("conname", "NAME"),
                ("connamespace", "OID"),
                ("contype", "TEXT"),
                ("condeferrable", "BOOLEAN"),
                ("condeferred", "BOOLEAN"),
                ("convalidated", "BOOLEAN"),
                ("conrelid", "OID"),
                ("contypid", "OID"),
                ("conindid", "OID"),
                ("conparentid", "OID"),
                ("confrelid", "OID"),
                ("confupdtype", "TEXT"),
                ("confdeltype", "TEXT"),
                ("confmatchtype", "TEXT"),
                ("conislocal", "BOOLEAN"),
                ("coninhcount", "INTEGER"),
                ("connoinherit", "BOOLEAN"),
                ("conkey", "INT2[]"),
                ("confkey", "INT2[]"),
                ("condef", "TEXT"),
            ]),
            branches: vec![
                (
                    exprs(&[
                        "100000000 + n.idx_oid",
                        "n.idx_name",
                        "2200",
                        "CASE n.origin WHEN 'pk' THEN 'p' ELSE 'u' END",
                        "0",
                        "0",
                        "1",
                        "n.tbl_oid",
                        "0",
                        "n.idx_oid",
                        "0",
                        "0",
                        "' '",
                        "' '",
                        "' '",
                        "1",
                        "0",
                        "1",
                        "(SELECT json_group_array(COALESCE((SELECT k.cid + 1 FROM pragma_table_info(n.tbl) AS k WHERE k.name = e.value), 0)) FROM json_each(n.cols) AS e)",
                        "NULL",
                        "CASE n.origin WHEN 'pk' THEN 'PRIMARY KEY (' ELSE 'UNIQUE (' END || (SELECT group_concat(value, ', ') FROM json_each(n.cols)) || ')'",
                    ]),
                    "FROM pg_nv_indexes AS n WHERE n.origin IN ('pk', 'u')".to_owned(),
                ),
                (
                    exprs(&[
                        "200000000 + m.rowid * 1000 + f.id",
                        "m.name || '_' || group_concat(f.\"from\", '_') || '_fkey'",
                        "2200",
                        "'f'",
                        "0",
                        "0",
                        "1",
                        "16384 + m.rowid",
                        "0",
                        "0",
                        "0",
                        "16384 + COALESCE((SELECT r.rowid FROM sqlite_schema AS r WHERE lower(r.name) = lower(f.\"table\") AND r.type = 'table'), 0)",
                        "CASE f.on_update WHEN 'CASCADE' THEN 'c' WHEN 'SET NULL' THEN 'n' WHEN 'SET DEFAULT' THEN 'd' WHEN 'RESTRICT' THEN 'r' ELSE 'a' END",
                        "CASE f.on_delete WHEN 'CASCADE' THEN 'c' WHEN 'SET NULL' THEN 'n' WHEN 'SET DEFAULT' THEN 'd' WHEN 'RESTRICT' THEN 'r' ELSE 'a' END",
                        "'s'",
                        "1",
                        "0",
                        "1",
                        "json_group_array((SELECT p.cid + 1 FROM pragma_table_info(m.name) AS p WHERE p.name = f.\"from\"))",
                        "json_group_array(COALESCE((SELECT p.cid + 1 FROM pragma_table_info(f.\"table\") AS p WHERE p.name = f.\"to\"), \
                         (SELECT p.cid + 1 FROM pragma_table_info(f.\"table\") AS p WHERE p.pk = 1)))",
                        "'FOREIGN KEY (' || group_concat(f.\"from\", ', ') || ') REFERENCES ' || f.\"table\" || '(' || \
                         group_concat(COALESCE(f.\"to\", (SELECT p.name FROM pragma_table_info(f.\"table\") AS p WHERE p.pk = 1)), ', ') || ')' || \
                         CASE WHEN f.on_update IN ('NO ACTION', 'NONE') THEN '' ELSE ' ON UPDATE ' || f.on_update END || \
                         CASE WHEN f.on_delete IN ('NO ACTION', 'NONE') THEN '' ELSE ' ON DELETE ' || f.on_delete END",
                    ]),
                    format!(
                        "FROM sqlite_schema AS m JOIN pragma_foreign_key_list(m.name) AS f WHERE m.type = 'table' AND {USER_OBJECTS} GROUP BY m.rowid, f.id"
                    ),
                ),
            ],
            deps: &["pg_nv_indexes"],
        },
        "pg_type" => Def {
            columns: cols(&[
                ("oid", "OID"),
                ("typname", "NAME"),
                ("typnamespace", "OID"),
                ("typowner", "OID"),
                ("typlen", "SMALLINT"),
                ("typbyval", "BOOLEAN"),
                ("typtype", "TEXT"),
                ("typcategory", "TEXT"),
                ("typisdefined", "BOOLEAN"),
                ("typdelim", "TEXT"),
                ("typrelid", "OID"),
                ("typelem", "OID"),
                ("typarray", "OID"),
                ("typnotnull", "BOOLEAN"),
                ("typbasetype", "OID"),
                ("typtypmod", "INTEGER"),
                ("typcollation", "OID"),
            ]),
            branches: builtin_types(),
            deps: &[],
        },
        "pg_database" => Def {
            columns: cols(&[
                ("oid", "OID"),
                ("datname", "NAME"),
                ("datdba", "OID"),
                ("encoding", "INTEGER"),
                ("datlocprovider", "TEXT"),
                ("datistemplate", "BOOLEAN"),
                ("datallowconn", "BOOLEAN"),
                ("datconnlimit", "INTEGER"),
                ("dattablespace", "OID"),
                ("datcollate", "TEXT"),
                ("datctype", "TEXT"),
                ("daticulocale", "TEXT"),
                ("datcollversion", "TEXT"),
                ("datacl", "TEXT[]"),
            ]),
            branches: vec![(
                exprs(&[
                    "16380",
                    &db,
                    "10",
                    "6",
                    "'c'",
                    "0",
                    "1",
                    "-1",
                    "1663",
                    "'en_US.UTF-8'",
                    "'en_US.UTF-8'",
                    "NULL",
                    "NULL",
                    "NULL",
                ]),
                String::new(),
            )],
            deps: &[],
        },
        "pg_roles" | "pg_authid" => Def {
            columns: cols(&[
                ("oid", "OID"),
                ("rolname", "NAME"),
                ("rolsuper", "BOOLEAN"),
                ("rolinherit", "BOOLEAN"),
                ("rolcreaterole", "BOOLEAN"),
                ("rolcreatedb", "BOOLEAN"),
                ("rolcanlogin", "BOOLEAN"),
                ("rolreplication", "BOOLEAN"),
                ("rolconnlimit", "INTEGER"),
                ("rolpassword", "TEXT"),
                ("rolvaliduntil", "TEXT"),
                ("rolbypassrls", "BOOLEAN"),
            ]),
            branches: vec![(
                exprs(&[
                    "10", &user, "1", "1", "1", "1", "1", "0", "-1", "NULL", "NULL", "0",
                ]),
                String::new(),
            )],
            deps: &[],
        },
        "pg_user" => Def {
            columns: cols(&[
                ("usename", "NAME"),
                ("usesysid", "OID"),
                ("usecreatedb", "BOOLEAN"),
                ("usesuper", "BOOLEAN"),
                ("userepl", "BOOLEAN"),
                ("usebypassrls", "BOOLEAN"),
                ("passwd", "TEXT"),
            ]),
            branches: vec![(
                exprs(&[&user, "10", "1", "1", "0", "0", "NULL"]),
                String::new(),
            )],
            deps: &[],
        },
        "pg_settings" => {
            let rows: Vec<(Vec<String>, String)> = env
                .vars
                .all()
                .into_iter()
                .map(|(k, v)| {
                    (
                        vec![
                            quote_str(&k),
                            quote_str(&v),
                            "''".to_owned(),
                            "'user'".to_owned(),
                        ],
                        String::new(),
                    )
                })
                .collect();
            Def {
                columns: cols(&[
                    ("name", "TEXT"),
                    ("setting", "TEXT"),
                    ("unit", "TEXT"),
                    ("context", "TEXT"),
                ]),
                branches: rows,
                deps: &[],
            }
        }
        "pg_tables" => Def {
            columns: cols(&[
                ("schemaname", "NAME"),
                ("tablename", "NAME"),
                ("tableowner", "NAME"),
                ("tablespace", "TEXT"),
                ("hasindexes", "BOOLEAN"),
                ("hasrules", "BOOLEAN"),
                ("hastriggers", "BOOLEAN"),
                ("rowsecurity", "BOOLEAN"),
            ]),
            branches: vec![(
                exprs(&[
                    "'public'",
                    "m.name",
                    &user,
                    "NULL",
                    "CASE WHEN EXISTS (SELECT 1 FROM pg_nv_indexes AS n WHERE n.tbl = m.name) THEN 1 ELSE 0 END",
                    "0",
                    "0",
                    "0",
                ]),
                format!("FROM sqlite_schema AS m WHERE m.type = 'table' AND {USER_OBJECTS}"),
            )],
            deps: &["pg_nv_indexes"],
        },
        "pg_views" => Def {
            columns: cols(&[
                ("schemaname", "NAME"),
                ("viewname", "NAME"),
                ("viewowner", "NAME"),
                ("definition", "TEXT"),
            ]),
            branches: vec![(
                exprs(&["'public'", "m.name", &user, "m.sql"]),
                format!("FROM sqlite_schema AS m WHERE m.type = 'view' AND {USER_OBJECTS}"),
            )],
            deps: &[],
        },
        "pg_indexes" => Def {
            columns: cols(&[
                ("schemaname", "NAME"),
                ("tablename", "NAME"),
                ("indexname", "NAME"),
                ("tablespace", "TEXT"),
                ("indexdef", "TEXT"),
            ]),
            branches: vec![(
                exprs(&["'public'", "i.tbl", "i.idx_name", "NULL", "pi.indexdef"]),
                "FROM pg_nv_indexes AS i JOIN pg_index AS pi ON pi.indexrelid = i.idx_oid"
                    .to_owned(),
            )],
            deps: &["pg_nv_indexes", "pg_index"],
        },
        "pg_statio_all_tables" => Def {
            columns: cols(&[
                ("relid", "OID"),
                ("schemaname", "NAME"),
                ("relname", "NAME"),
            ]),
            branches: vec![(
                exprs(&["16384 + m.rowid", "'public'", "m.name"]),
                format!("FROM sqlite_schema AS m WHERE m.type = 'table' AND {USER_OBJECTS}"),
            )],
            deps: &[],
        },
        "pg_tablespace" => Def {
            columns: cols(&[("oid", "OID"), ("spcname", "NAME"), ("spcowner", "OID")]),
            branches: vec![(exprs(&["1663", "'pg_default'", "10"]), String::new())],
            deps: &[],
        },
        // Empty tables.
        "pg_description" => empty(&[
            ("objoid", "OID"),
            ("classoid", "OID"),
            ("objsubid", "INTEGER"),
            ("description", "TEXT"),
        ]),
        "pg_sequence" => empty(&[
            ("seqrelid", "OID"),
            ("seqtypid", "OID"),
            ("seqstart", "BIGINT"),
            ("seqincrement", "BIGINT"),
            ("seqmax", "BIGINT"),
            ("seqmin", "BIGINT"),
            ("seqcache", "BIGINT"),
            ("seqcycle", "BOOLEAN"),
        ]),
        "pg_collation" => empty(&[
            ("oid", "OID"),
            ("collname", "NAME"),
            ("collnamespace", "OID"),
        ]),
        "pg_range" => empty(&[("rngtypid", "OID"), ("rngsubtype", "OID")]),
        "pg_enum" => empty(&[
            ("oid", "OID"),
            ("enumtypid", "OID"),
            ("enumsortorder", "REAL"),
            ("enumlabel", "NAME"),
        ]),
        "pg_proc" => empty(&[
            ("oid", "OID"),
            ("proname", "NAME"),
            ("pronamespace", "OID"),
            ("prokind", "TEXT"),
        ]),
        "pg_opclass" => empty(&[
            ("oid", "OID"),
            ("opcmethod", "OID"),
            ("opcname", "NAME"),
            ("opcnamespace", "OID"),
            ("opcdefault", "BOOLEAN"),
        ]),
        "pg_inherits" => empty(&[
            ("inhrelid", "OID"),
            ("inhparent", "OID"),
            ("inhseqno", "INTEGER"),
        ]),
        "pg_depend" => empty(&[
            ("classid", "OID"),
            ("objid", "OID"),
            ("objsubid", "INTEGER"),
            ("refclassid", "OID"),
            ("refobjid", "OID"),
            ("refobjsubid", "INTEGER"),
            ("deptype", "TEXT"),
        ]),
        "pg_trigger" => empty(&[
            ("oid", "OID"),
            ("tgrelid", "OID"),
            ("tgname", "NAME"),
            ("tgisinternal", "BOOLEAN"),
        ]),
        "pg_extension" => empty(&[("oid", "OID"), ("extname", "NAME"), ("extversion", "TEXT")]),
        "pg_auth_members" => empty(&[("roleid", "OID"), ("member", "OID")]),
        "pg_namespace_acl" => empty(&[("oid", "OID")]),
        _ => return info_definition(name, env),
    })
}

fn empty(spec: &[(&'static str, &'static str)]) -> Def {
    Def {
        columns: cols(spec),
        branches: vec![(
            spec.iter().map(|_| "NULL".to_owned()).collect(),
            "WHERE 0".to_owned(),
        )],
        deps: &[],
    }
}

fn builtin_types() -> Vec<(Vec<String>, String)> {
    // (oid, name, array oid, category, typlen, collation)
    const TYPES: &[(i32, &str, i32, &str, i32, i32)] = &[
        (16, "bool", 1000, "B", 1, 0),
        (17, "bytea", 1001, "U", -1, 0),
        (18, "char", 1002, "Z", 1, 100),
        (19, "name", 1003, "S", 64, 950),
        (20, "int8", 1016, "N", 8, 0),
        (21, "int2", 1005, "N", 2, 0),
        (23, "int4", 1007, "N", 4, 0),
        (25, "text", 1009, "S", -1, 100),
        (26, "oid", 1028, "N", 4, 0),
        (114, "json", 199, "U", -1, 0),
        (700, "float4", 1021, "N", 4, 0),
        (701, "float8", 1022, "N", 8, 0),
        (1042, "bpchar", 1014, "S", -1, 100),
        (1043, "varchar", 1015, "S", -1, 100),
        (1082, "date", 1182, "D", 4, 0),
        (1083, "time", 1183, "D", 8, 0),
        (1114, "timestamp", 1115, "D", 8, 0),
        (1184, "timestamptz", 1185, "D", 8, 0),
        (1186, "interval", 1187, "T", 16, 0),
        (1700, "numeric", 1231, "N", -1, 0),
        (2950, "uuid", 2951, "U", 16, 0),
        (3802, "jsonb", 3807, "U", -1, 0),
    ];
    let mut rows = Vec::new();
    for (type_oid, name, array_oid, category, len, collation) in TYPES {
        rows.push((
            vec![
                type_oid.to_string(),
                quote_str(name),
                "11".into(),
                "10".into(),
                len.to_string(),
                i32::from(*len > 0 && *len <= 8).to_string(),
                "'b'".into(),
                quote_str(category),
                "1".into(),
                "','".into(),
                "0".into(),
                "0".into(),
                array_oid.to_string(),
                "0".into(),
                "0".into(),
                "-1".into(),
                collation.to_string(),
            ],
            String::new(),
        ));
        rows.push((
            vec![
                array_oid.to_string(),
                quote_str(&format!("_{name}")),
                "11".into(),
                "10".into(),
                "-1".into(),
                "0".into(),
                "'b'".into(),
                "'A'".into(),
                "1".into(),
                "','".into(),
                "0".into(),
                type_oid.to_string(),
                "0".into(),
                "0".into(),
                "0".into(),
                "-1".into(),
                "0".into(),
            ],
            String::new(),
        ));
    }
    rows
}

fn info_definition(name: &str, env: &Env<'_>) -> Option<Def> {
    let table = name.strip_prefix("information_schema.")?;
    let db = quote_str(env.database);
    let user = quote_str(env.user);
    Some(match table {
        "schemata" => Def {
            columns: cols(&[
                ("catalog_name", "TEXT"), ("schema_name", "TEXT"), ("schema_owner", "TEXT"),
                ("default_character_set_name", "TEXT"),
            ]),
            branches: vec![
                (exprs(&[&db, "'public'", &user, "NULL"]), String::new()),
                (exprs(&[&db, "'pg_catalog'", &user, "NULL"]), String::new()),
                (exprs(&[&db, "'information_schema'", &user, "NULL"]), String::new()),
            ],
            deps: &[],
        },
        "tables" => Def {
            columns: cols(&[
                ("table_catalog", "TEXT"), ("table_schema", "TEXT"), ("table_name", "TEXT"), ("table_type", "TEXT"),
                ("self_referencing_column_name", "TEXT"), ("reference_generation", "TEXT"),
                ("user_defined_type_catalog", "TEXT"), ("user_defined_type_schema", "TEXT"),
                ("user_defined_type_name", "TEXT"), ("is_insertable_into", "TEXT"), ("is_typed", "TEXT"),
                ("commit_action", "TEXT"),
            ]),
            branches: vec![(
                exprs(&[
                    &db, "'public'", "m.name", "CASE m.type WHEN 'view' THEN 'VIEW' ELSE 'BASE TABLE' END", "NULL", "NULL",
                    "NULL", "NULL", "NULL", "CASE m.type WHEN 'view' THEN 'NO' ELSE 'YES' END", "'NO'", "NULL",
                ]),
                format!("FROM sqlite_schema AS m WHERE m.type IN ('table', 'view') AND {USER_OBJECTS}"),
            )],
            deps: &[],
        },
        "columns" => Def {
            columns: cols(&[
                ("table_catalog", "TEXT"), ("table_schema", "TEXT"), ("table_name", "TEXT"), ("column_name", "TEXT"),
                ("ordinal_position", "INTEGER"), ("column_default", "TEXT"), ("is_nullable", "TEXT"), ("data_type", "TEXT"),
                ("character_maximum_length", "INTEGER"), ("character_octet_length", "INTEGER"),
                ("numeric_precision", "INTEGER"), ("numeric_precision_radix", "INTEGER"), ("numeric_scale", "INTEGER"),
                ("datetime_precision", "INTEGER"), ("interval_type", "TEXT"), ("character_set_name", "TEXT"),
                ("collation_name", "TEXT"), ("domain_name", "TEXT"), ("udt_catalog", "TEXT"), ("udt_schema", "TEXT"),
                ("udt_name", "TEXT"), ("dtd_identifier", "TEXT"), ("is_self_referencing", "TEXT"),
                ("is_identity", "TEXT"), ("identity_generation", "TEXT"), ("identity_start", "TEXT"),
                ("identity_increment", "TEXT"), ("identity_maximum", "TEXT"), ("identity_minimum", "TEXT"),
                ("identity_cycle", "TEXT"), ("is_generated", "TEXT"), ("generation_expression", "TEXT"),
                ("is_updatable", "TEXT"),
            ]),
            branches: vec![(
                exprs(&[
                    &db, "'public'", "c.tbl", "c.name", "c.attnum", "c.dflt", "CASE WHEN c.nn = 1 THEN 'NO' ELSE 'YES' END",
                    &format!("CASE c.type_oid {} END", info_type_cases("data")),
                    "CASE WHEN c.type_oid IN (1043, 1042) AND c.typmod > 4 THEN c.typmod - 4 END",
                    "CASE WHEN c.type_oid IN (1043, 1042, 25) THEN 1073741824 END",
                    "CASE c.type_oid WHEN 21 THEN 16 WHEN 23 THEN 32 WHEN 20 THEN 64 WHEN 700 THEN 24 WHEN 701 THEN 53 WHEN 1700 THEN (c.typmod - 4) / 65536 END",
                    "CASE WHEN c.type_oid IN (21, 23, 20, 1700) THEN 10 WHEN c.type_oid IN (700, 701) THEN 2 END",
                    "CASE WHEN c.type_oid IN (21, 23, 20) THEN 0 WHEN c.type_oid = 1700 THEN (c.typmod - 4) % 65536 END",
                    "CASE WHEN c.type_oid IN (1082, 1083, 1114, 1184) THEN 6 END", "NULL", "NULL", "NULL", "NULL", &db,
                    "'pg_catalog'", &format!("CASE c.type_oid {} END", info_type_cases("udt")), "CAST(c.attnum AS TEXT)", "'NO'",
                    "'NO'", "NULL", "NULL", "NULL", "NULL", "NULL", "'NO'", "'NEVER'", "NULL", "'YES'",
                ]),
                "FROM pg_nv_columns AS c WHERE c.kind = 'table' OR c.kind = 'view'".to_owned(),
            )],
            deps: &["pg_nv_columns"],
        },
        "table_constraints" => Def {
            columns: cols(&[
                ("constraint_catalog", "TEXT"), ("constraint_schema", "TEXT"), ("constraint_name", "TEXT"),
                ("table_catalog", "TEXT"), ("table_schema", "TEXT"), ("table_name", "TEXT"), ("constraint_type", "TEXT"),
                ("is_deferrable", "TEXT"), ("initially_deferred", "TEXT"), ("enforced", "TEXT"),
            ]),
            branches: vec![(
                exprs(&[
                    &db, "'public'", "n.idx_name", &db, "'public'", "n.tbl",
                    "CASE n.origin WHEN 'pk' THEN 'PRIMARY KEY' ELSE 'UNIQUE' END", "'NO'", "'NO'", "'YES'",
                ]),
                "FROM pg_nv_indexes AS n WHERE n.origin IN ('pk', 'u')".to_owned(),
            ), (
                exprs(&[
                    &db, "'public'", "k.conname", &db, "'public'", "k.tbl", "'FOREIGN KEY'", "'NO'", "'NO'", "'YES'",
                ]),
                "FROM (SELECT m.name AS tbl, m.name || '_' || group_concat(f.\"from\", '_') || '_fkey' AS conname \
                 FROM sqlite_schema AS m JOIN pragma_foreign_key_list(m.name) AS f WHERE m.type = 'table' AND m.name NOT LIKE 'sqlite\\_%' ESCAPE '\\' \
                 GROUP BY m.rowid, f.id) AS k"
                    .to_owned(),
            )],
            deps: &["pg_nv_indexes"],
        },
        "key_column_usage" => Def {
            columns: cols(&[
                ("constraint_catalog", "TEXT"), ("constraint_schema", "TEXT"), ("constraint_name", "TEXT"),
                ("table_catalog", "TEXT"), ("table_schema", "TEXT"), ("table_name", "TEXT"), ("column_name", "TEXT"),
                ("ordinal_position", "INTEGER"), ("position_in_unique_constraint", "INTEGER"),
            ]),
            branches: vec![(
                exprs(&[&db, "'public'", "n.idx_name", &db, "'public'", "n.tbl", "e.value", "e.key + 1", "NULL"]),
                "FROM pg_nv_indexes AS n JOIN json_each(n.cols) AS e WHERE n.origin IN ('pk', 'u')".to_owned(),
            ), (
                exprs(&[
                    &db, "'public'", "m.name || '_' || (SELECT group_concat(g.\"from\", '_') FROM pragma_foreign_key_list(m.name) AS g WHERE g.id = f.id) || '_fkey'",
                    &db, "'public'", "m.name", "f.\"from\"", "f.seq + 1", "f.seq + 1",
                ]),
                "FROM sqlite_schema AS m JOIN pragma_foreign_key_list(m.name) AS f WHERE m.type = 'table' AND m.name NOT LIKE 'sqlite\\_%' ESCAPE '\\'"
                    .to_owned(),
            )],
            deps: &["pg_nv_indexes"],
        },
        "referential_constraints" => Def {
            columns: cols(&[
                ("constraint_catalog", "TEXT"), ("constraint_schema", "TEXT"), ("constraint_name", "TEXT"),
                ("unique_constraint_catalog", "TEXT"), ("unique_constraint_schema", "TEXT"),
                ("unique_constraint_name", "TEXT"), ("match_option", "TEXT"), ("update_rule", "TEXT"),
                ("delete_rule", "TEXT"),
            ]),
            branches: vec![(
                exprs(&[
                    &db, "'public'", "m.name || '_' || (SELECT group_concat(g.\"from\", '_') FROM pragma_foreign_key_list(m.name) AS g WHERE g.id = f.id) || '_fkey'",
                    &db, "'public'", "NULL", "'NONE'", "f.on_update", "f.on_delete",
                ]),
                "FROM sqlite_schema AS m JOIN pragma_foreign_key_list(m.name) AS f WHERE m.type = 'table' AND m.name NOT LIKE 'sqlite\\_%' ESCAPE '\\' AND f.seq = 0"
                    .to_owned(),
            )],
            deps: &[],
        },
        "constraint_column_usage" => Def {
            columns: cols(&[
                ("table_catalog", "TEXT"), ("table_schema", "TEXT"), ("table_name", "TEXT"), ("column_name", "TEXT"),
                ("constraint_catalog", "TEXT"), ("constraint_schema", "TEXT"), ("constraint_name", "TEXT"),
            ]),
            branches: vec![(
                exprs(&[&db, "'public'", "n.tbl", "e.value", &db, "'public'", "n.idx_name"]),
                "FROM pg_nv_indexes AS n JOIN json_each(n.cols) AS e WHERE n.origin IN ('pk', 'u')".to_owned(),
            ), (
                exprs(&[
                    &db, "'public'", "f.\"table\"", "COALESCE(f.\"to\", (SELECT p.name FROM pragma_table_info(f.\"table\") AS p WHERE p.pk = 1))", &db, "'public'",
                    "m.name || '_' || (SELECT group_concat(g.\"from\", '_') FROM pragma_foreign_key_list(m.name) AS g WHERE g.id = f.id) || '_fkey'",
                ]),
                "FROM sqlite_schema AS m JOIN pragma_foreign_key_list(m.name) AS f WHERE m.type = 'table' AND m.name NOT LIKE 'sqlite\\_%' ESCAPE '\\'"
                    .to_owned(),
            )],
            deps: &["pg_nv_indexes"],
        },
        "views" => Def {
            columns: cols(&[
                ("table_catalog", "TEXT"), ("table_schema", "TEXT"), ("table_name", "TEXT"), ("view_definition", "TEXT"),
                ("check_option", "TEXT"), ("is_updatable", "TEXT"),
            ]),
            branches: vec![(
                exprs(&[&db, "'public'", "m.name", "m.sql", "'NONE'", "'NO'"]),
                format!("FROM sqlite_schema AS m WHERE m.type = 'view' AND {USER_OBJECTS}"),
            )],
            deps: &[],
        },
        "check_constraints" => empty(&[
            ("constraint_catalog", "TEXT"), ("constraint_schema", "TEXT"), ("constraint_name", "TEXT"), ("check_clause", "TEXT"),
        ]),
        "sequences" => empty(&[
            ("sequence_catalog", "TEXT"), ("sequence_schema", "TEXT"), ("sequence_name", "TEXT"), ("data_type", "TEXT"),
        ]),
        "routines" => empty(&[
            ("routine_catalog", "TEXT"), ("routine_schema", "TEXT"), ("routine_name", "TEXT"), ("routine_type", "TEXT"),
        ]),
        _ => return None,
    })
}

/// `WHEN <oid> THEN '<name>'` list for information_schema type columns.
fn info_type_cases(kind: &str) -> String {
    let mut out = String::new();
    for (type_oid, udt, data) in [
        (16, "bool", "boolean"),
        (17, "bytea", "bytea"),
        (20, "int8", "bigint"),
        (21, "int2", "smallint"),
        (23, "int4", "integer"),
        (25, "text", "text"),
        (114, "json", "json"),
        (700, "float4", "real"),
        (701, "float8", "double precision"),
        (1042, "bpchar", "character"),
        (1043, "varchar", "character varying"),
        (1082, "date", "date"),
        (1083, "time", "time without time zone"),
        (1114, "timestamp", "timestamp without time zone"),
        (1184, "timestamptz", "timestamp with time zone"),
        (1700, "numeric", "numeric"),
        (2950, "uuid", "uuid"),
        (3802, "jsonb", "jsonb"),
    ] {
        let name = if kind == "udt" { udt } else { data };
        out.push_str(&format!("WHEN {type_oid} THEN '{name}' "));
    }
    out.push_str("ELSE 'text'");
    out
}

/// Renders a catalog table as a CTE body.
fn cte_sql(name: &str, def: &Def) -> String {
    // Internal helper CTEs carry fixed, hand-written column names.
    let names: Vec<&str> = match name {
        "pg_nv_columns" => vec![
            "tbl", "tbl_oid", "attnum", "name", "decl", "type_oid", "typmod", "nn", "dflt", "pk",
            "kind",
        ],
        "pg_nv_indexes" => vec![
            "tbl",
            "tbl_oid",
            "idx_name",
            "idx_oid",
            "is_unique",
            "origin",
            "cols",
        ],
        _ => def.columns.iter().map(|(n, _)| *n).collect(),
    };
    let branch_sql: Vec<String> = def
        .branches
        .iter()
        .enumerate()
        .map(|(index, (columns, from))| {
            let list: Vec<String> = columns
                .iter()
                .zip(&names)
                .map(|(expr, column)| {
                    if index == 0 {
                        format!("{expr} AS {}", quote_ident(column))
                    } else {
                        expr.clone()
                    }
                })
                .collect();
            format!("SELECT {} {from}", list.join(", "))
        })
        .collect();
    branch_sql.join(" UNION ALL ")
}

fn dependencies(name: &str, env: &Env<'_>, ordered: &mut Vec<String>) {
    if ordered.iter().any(|n| n == name) {
        return;
    }
    if let Some(def) = definition(name, env) {
        for dep in def.deps {
            dependencies(dep, env, ordered);
        }
        ordered.push(name.to_owned());
    }
}

/// Cached column declarations of catalog tables.
pub fn static_lookup(name: &str) -> Option<&'static TableInfo> {
    static TABLES: std::sync::OnceLock<std::collections::HashMap<String, TableInfo>> =
        std::sync::OnceLock::new();
    let tables = TABLES.get_or_init(|| {
        let mut all = std::collections::HashMap::new();
        for name in PG_TABLES {
            if let Some(info) = static_table(name) {
                all.insert((*name).to_owned(), info);
            }
        }
        for table in INFO_TABLES {
            let key = info_name(table);
            if let Some(info) = static_table(&key) {
                all.insert(key, info);
            }
        }
        all
    });
    tables.get(name)
}

/// Column declarations of a catalog table (for result typing).
fn static_table(name: &str) -> Option<TableInfo> {
    // Catalog definitions do not depend on the session for their columns.
    let vars = crate::vars::Vars::new("");
    let schema = crate::analysis::Schema::default();
    let env = Env {
        database: "",
        user: "",
        pid: 0,
        vars: &vars,
        schema: &schema,
    };
    let key = name.to_ascii_lowercase();
    let def = definition(&key, &env)?;
    if def.columns.is_empty() {
        return None;
    }
    Some(TableInfo {
        columns: def
            .columns
            .iter()
            .map(|(column, decl)| ColumnInfo {
                name: (*column).to_owned(),
                decl: (*decl).to_owned(),
                default: None,
                pk: 0,
            })
            .collect(),
    })
}

fn is_pg_table(word: &str) -> bool {
    PG_TABLES.contains(&word)
}

/// Collects catalog tables referenced by the statement while normalising
/// qualified names (`pg_catalog.pg_class` -> `pg_class`,
/// `information_schema.tables` -> `"information_schema.tables"`).
fn normalize_references(tokens: &mut Vec<Token>, needed: &mut BTreeSet<String>) {
    let mut i = 0;
    while i < tokens.len() {
        let prefixed = i + 2 < tokens.len()
            && tokens[i + 1].is_op(".")
            && matches!(tokens[i].kind, Kind::Word | Kind::QIdent)
            && (i == 0 || !tokens[i - 1].is_op("."));
        if prefixed {
            let schema = tokens[i].ident().unwrap_or_default();
            let object = tokens[i + 2].ident().unwrap_or_default();
            let is_call = tokens.get(i + 3).is_some_and(|t| t.is_op("("));
            if schema == "pg_catalog" && is_pg_table(&object) && !is_call {
                let ws = tokens[i].ws.clone();
                tokens.drain(i..i + 2);
                tokens[i].ws = ws;
                needed.insert(object);
                continue;
            }
            if schema == "information_schema" && INFO_TABLES.contains(&object.as_str()) && !is_call
            {
                let name = info_name(&object);
                let mut replacement =
                    Token::new(Kind::QIdent, quote_ident(&name)).with_ws(&tokens[i].ws);
                replacement.value.clone_from(&name);
                tokens.splice(i..i + 3, [replacement]);
                needed.insert(name);
                continue;
            }
        }
        if tokens[i].kind == Kind::Word
            && is_pg_table(&tokens[i].lower())
            && !tokens.get(i + 1).is_some_and(|t| t.is_op("("))
            && !(i > 0 && tokens[i - 1].is_op("."))
        {
            needed.insert(tokens[i].lower());
        }
        i += 1;
    }
}

/// Tokenizes `sql` and substitutes `__ARGn__` words by parenthesised
/// argument tokens.
fn template(sql: &str, args: &[Vec<Token>]) -> Vec<Token> {
    let mut out = Vec::new();
    for token in crate::lexer::tokenize(sql).unwrap_or_default() {
        let placeholder = token
            .text
            .strip_prefix("__ARG")
            .and_then(|rest| rest.strip_suffix("__"))
            .and_then(|n| n.parse::<usize>().ok());
        match placeholder {
            Some(n) if token.kind == Kind::Word => {
                out.push(Token::op("(").with_ws(&token.ws));
                out.extend(args.get(n).cloned().unwrap_or_default());
                out.push(Token::op(")"));
            }
            _ => out.push(token),
        }
    }
    out
}

/// Rewrites functions that need catalog data into correlated subqueries.
fn rewrite_catalog_functions(tokens: &mut Vec<Token>, needed: &mut BTreeSet<String>) {
    let mut i = tokens.len();
    while i > 0 {
        i -= 1;
        if tokens[i].kind != Kind::Word || !tokens.get(i + 1).is_some_and(|t| t.is_op("(")) {
            continue;
        }
        let name = tokens[i].lower();
        let Some(close) = find_close(tokens, i + 1) else {
            continue;
        };
        // Strip a pg_catalog. qualifier on the call itself.
        let start = if i >= 2
            && tokens[i - 1].is_op(".")
            && tokens[i - 2].ident().as_deref() == Some("pg_catalog")
        {
            i - 2
        } else {
            i
        };
        let args: Vec<Vec<Token>> = if close > i + 2 {
            split_commas(&tokens[i + 2..close])
                .into_iter()
                .map(<[Token]>::to_vec)
                .collect()
        } else {
            Vec::new()
        };
        let sql = match name.as_str() {
            "pg_get_constraintdef" if !args.is_empty() => {
                needed.insert("pg_constraint".into());
                Some(
                    "(SELECT _pc.condef FROM pg_constraint AS _pc WHERE _pc.oid = __ARG0__)"
                        .to_owned(),
                )
            }
            "pg_get_indexdef" if !args.is_empty() => {
                needed.insert("pg_index".into());
                let column = if args.len() >= 2 { "__ARG1__" } else { "0" };
                Some(format!(
                    "(SELECT CASE WHEN {column} = 0 THEN _pi.indexdef ELSE json_extract(_pi.indcolnames, '$[' || ({column} - 1) || ']') END \
                     FROM pg_index AS _pi WHERE _pi.indexrelid = __ARG0__)"
                ))
            }
            "to_regtype" if args.len() == 1 => {
                needed.insert("pg_type".into());
                Some(
                    "(SELECT _pt.oid FROM pg_type AS _pt WHERE _pt.typname = lower(__ARG0__))"
                        .to_owned(),
                )
            }
            "to_regclass" if args.len() == 1 => {
                needed.insert("pg_class".into());
                Some(
                    "(SELECT _pcl.oid FROM pg_class AS _pcl WHERE _pcl.relname = __ARG0__)"
                        .to_owned(),
                )
            }
            _ => None,
        };
        if let Some(sql) = sql {
            let ws = tokens[start].ws.clone();
            let mut replacement = template(&sql, &args);
            if let Some(first) = replacement.first_mut() {
                first.ws = ws;
            }
            tokens.splice(start..=close, replacement);
            i = start;
        }
    }
}

const CLAUSE_WORDS: &[&str] = &[
    "WHERE",
    "GROUP",
    "ORDER",
    "LIMIT",
    "UNION",
    "HAVING",
    "WINDOW",
    "OFFSET",
    "INTERSECT",
    "EXCEPT",
];

/// Locates, for the SELECT at `select`, the index of its FROM keyword and the
/// index one past its FROM clause.
fn select_extent(tokens: &[Token], select: usize) -> Option<(usize, usize)> {
    let mut depth = 0_i32;
    let mut from = None;
    for (j, token) in tokens.iter().enumerate().skip(select + 1) {
        if token.is_op("(") {
            depth += 1;
        } else if token.is_op(")") {
            depth -= 1;
            if depth < 0 {
                return from.map(|f| (f, j));
            }
        } else if depth == 0 {
            if token.is_word("FROM") && from.is_none() {
                from = Some(j);
            } else if from.is_some()
                && token.kind == Kind::Word
                && CLAUSE_WORDS.contains(&token.text.to_ascii_uppercase().as_str())
            {
                return from.map(|f| (f, j));
            }
        }
    }
    from.map(|f| (f, tokens.len()))
}

/// Finds a set-returning call at depth 0 of the select list.
fn find_srf(tokens: &[Token], select: usize, from: usize) -> Option<(usize, usize)> {
    let mut depth = 0_i32;
    for j in select + 1..from {
        let token = &tokens[j];
        if token.is_op("(") {
            depth += 1;
        } else if token.is_op(")") {
            depth -= 1;
        } else if depth == 0
            && token.kind == Kind::Word
            && tokens.get(j + 1).is_some_and(|n| n.is_op("("))
            && matches!(token.lower().as_str(), "unnest" | "generate_subscripts")
        {
            return find_close(tokens, j + 1).map(|close| (j, close));
        }
    }
    None
}

/// Rewrites `unnest(x)` / `generate_subscripts(x, 1)` in select lists into
/// joins against `json_each` (all calls of one SELECT run in lockstep).
fn rewrite_set_returning(tokens: &mut Vec<Token>) {
    let mut counter = 0;
    let mut i = 0;
    while i < tokens.len() {
        if !tokens[i].is_word("SELECT") {
            i += 1;
            continue;
        }
        let mut state: Option<(Vec<Token>, String)> = None;
        while let Some((from, _)) = select_extent(tokens, i) {
            let Some((call, close)) = find_srf(tokens, i, from) else {
                break;
            };
            let name = tokens[call].lower();
            let args: Vec<Vec<Token>> = split_commas(&tokens[call + 2..close])
                .into_iter()
                .map(<[Token]>::to_vec)
                .collect();
            let target = args.first().cloned().unwrap_or_default();
            let text = crate::lexer::render(&target);
            let (first, alias) = state.get_or_insert_with(|| {
                let alias = format!("_je{counter}");
                counter += 1;
                (target.clone(), alias)
            });
            let alias = alias.clone();
            let same_array = crate::lexer::render(first) == text;
            let replacement = if name == "generate_subscripts" {
                format!("({alias}.key + 1)")
            } else if same_array {
                format!("{alias}.value")
            } else {
                format!("json_extract({text}, '$[' || {alias}.key || ']')")
            };
            let ws = tokens[call].ws.clone();
            tokens.splice(call..=close, [Token::raw(replacement).with_ws(&ws)]);
        }
        if let Some((target, alias)) = state {
            if let Some((_, end)) = select_extent(tokens, i) {
                let join = Token::raw(format!(
                    " JOIN json_each({}) AS {alias}",
                    crate::lexer::render(&target)
                ));
                tokens.insert(end, join);
            }
        }
        i += 1;
    }
}

/// Entry point: replaces catalog tables by CTEs.
pub fn rewrite(tokens: &mut Vec<Token>, env: &Env<'_>) -> SqlResult<()> {
    let mut needed = BTreeSet::new();
    normalize_references(tokens, &mut needed);
    rewrite_catalog_functions(tokens, &mut needed);
    if needed.is_empty() {
        return Ok(());
    }
    rewrite_set_returning(tokens);

    let mut ordered: Vec<String> = Vec::new();
    for name in &needed {
        if definition(name, env).is_none() {
            return Err(SqlError::Unsupported(format!(
                "catalog table \"{name}\" is not supported by NovaDB"
            )));
        }
        dependencies(name, env, &mut ordered);
    }
    let mut definitions = Vec::new();
    for name in &ordered {
        let Some(def) = definition(name, env) else {
            continue;
        };
        definitions.push(format!(
            "{} AS ({})",
            quote_ident(name),
            cte_sql(name, &def)
        ));
    }
    let definitions = definitions.join(", ");
    if tokens.first().is_some_and(|t| t.is_word("WITH")) {
        let at = if tokens.get(1).is_some_and(|t| t.is_word("RECURSIVE")) {
            2
        } else {
            1
        };
        tokens.insert(at, Token::raw(format!("{definitions},")));
    } else {
        tokens.insert(0, Token::raw(format!("WITH {definitions}")).with_ws(""));
        if let Some(next) = tokens.get_mut(1) {
            if next.ws.is_empty() {
                next.ws = " ".into();
            }
        }
    }
    Ok(())
}

/// Declared result type OID of a catalog column (used by tests).
#[allow(dead_code)]
pub fn column_oid(table: &str, column: &str) -> Option<i32> {
    static_table(table)?
        .column(column)
        .map(|c| type_map::sqlite_type_to_pg_oid(&c.decl))
}

#[allow(dead_code)]
const _: i32 = oid::OID;
