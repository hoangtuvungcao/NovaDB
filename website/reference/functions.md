# Functions

NovaDB registers these functions on every connection, in addition to SQLite's
[built-in scalar](https://www.sqlite.org/lang_corefunc.html), [aggregate](https://www.sqlite.org/lang_aggfunc.html),
[date/time](https://www.sqlite.org/lang_datefunc.html), [math](https://www.sqlite.org/lang_mathfunc.html) and
[JSON](https://www.sqlite.org/json1.html) functions. Function names are case-insensitive.

## Vector Search and AI Embeddings

| Function | Arguments | Returns | Description |
|---|---|---|---|
| `VECTOR_COSINE_DISTANCE(v1, v2)` | `JSON/BLOB, JSON/BLOB` | `REAL` | Cosine distance `1.0 - cosine_similarity` in range `[0.0, 2.0]` |
| `VECTOR_COSINE_SIMILARITY(v1, v2)` | `JSON/BLOB, JSON/BLOB` | `REAL` | Cosine similarity in range `[-1.0, 1.0]` |
| `VECTOR_L2_DISTANCE(v1, v2)` | `JSON/BLOB, JSON/BLOB` | `REAL` | Euclidean L2 distance `sqrt(sum((a - b)^2))` |
| `VECTOR_DOT_PRODUCT(v1, v2)` | `JSON/BLOB, JSON/BLOB` | `REAL` | Inner dot product `sum(a * b)` |
| `VECTOR_NORM(v)` | `JSON/BLOB` | `REAL` | Vector magnitude / L2 norm `sqrt(sum(a^2))` |
| `VECTOR_DIM(v)` | `JSON/BLOB` | `INTEGER` | Dimensionality count of vector elements |
| `VECTOR_NORMALIZE(v)` | `JSON/BLOB` | `TEXT` | Normalizes vector to unit length as JSON array |
| `VECTOR_TO_BLOB(json_array)` | `TEXT` | `BLOB` | Serializes vector to compact float32 binary BLOB |
| `VECTOR_FROM_BLOB(blob)` | `BLOB` | `TEXT` | Deserializes float32 binary BLOB to JSON array |

**Example:**
```sql
-- Find top 5 semantic matches for a query embedding
SELECT id, title, vector_cosine_similarity(embedding, vector_to_blob('[0.12, -0.45, 0.88, 0.33]')) as sim
FROM articles
ORDER BY sim DESC
LIMIT 5;
```

## UUID Functions

| Function | Arguments | Returns | Description |
|---|---|---|---|
| `UUID_V4()` | None | `TEXT` | Generates random RFC 4122 UUID v4 |
| `UUID_V7()` | None | `TEXT` | Generates time-ordered monotonic RFC 9562 UUID v7 |
| `UUID_NIL()` | None | `TEXT` | Returns `00000000-0000-0000-0000-000000000000` |
| `UUID_IS_VALID(uuid)` | `TEXT` | `INTEGER` | Returns `1` if valid UUID, `0` otherwise |
| `UUID_VERSION(uuid)` | `TEXT` | `INTEGER` | Returns version number (`4`, `7`, etc.) |
| `UUID_TO_BLOB(uuid)` | `TEXT` | `BLOB` | Converts 36-character UUID string to 16-byte raw blob |
| `UUID_FROM_BLOB(blob)` | `BLOB` | `TEXT` | Converts 16-byte blob to canonical UUID string |

## Date and Time Functions

| Function | Arguments | Returns | Description |
|---|---|---|---|
| `NOW_ISO()` | None | `TEXT` | Current UTC timestamp (`YYYY-MM-DDTHH:MM:SS.mmmZ`) |
| `NOW_MS()` | None | `INTEGER` | Current Unix timestamp in milliseconds |
| `NOW_US()` | None | `INTEGER` | Current Unix timestamp in microseconds |
| `EPOCH_MS(iso_text)` | `TEXT` | `INTEGER` | Converts ISO 8601 string to Unix milliseconds |
| `FROM_EPOCH_MS(ms)` | `INTEGER` | `TEXT` | Converts Unix milliseconds to ISO 8601 UTC string |
| `DATE_PART(part, text)` | `TEXT, TEXT` | `INTEGER` | Extracts `'year'`, `'month'`, `'day'`, `'hour'`, `'minute'`, `'second'`, `'dow'` |
| `DATE_TRUNC(part, text)` | `TEXT, TEXT` | `TEXT` | Truncates timestamp to `'year'`, `'month'`, `'day'`, `'hour'`, `'minute'`, `'second'` |
| `AGE_MS(ts1, ts2)` | `TEXT, TEXT` | `INTEGER` | Calculates difference between two timestamps in milliseconds |

## JSON Functions (RFC 7396)

| Function | Arguments | Returns | Description |
|---|---|---|---|
| `JSON_EXTRACT(json, path)` | `TEXT, TEXT` | `ANY` | Extracts value at JSONPath (e.g. `'$.user.id'`) |
| `JSON_PRETTY(json)` | `TEXT` | `TEXT` | Formats JSON text with 2-space indentation |
| `JSON_VALID_STRICT(json)` | `TEXT` | `INTEGER` | Returns `1` if strict valid JSON, `0` otherwise |
| `JSON_DEPTH(json)` | `TEXT` | `INTEGER` | Returns maximum nesting depth |
| `JSON_KEYS(json_obj)` | `TEXT` | `TEXT` | Returns top-level keys as JSON array |
| `JSON_MERGE_PATCH(tgt, patch)` | `TEXT, TEXT` | `TEXT` | RFC 7396 JSON merge patch |
| `JSON_CONTAINS(json, val)` | `TEXT, TEXT` | `INTEGER` | Returns `1` if container contains candidate value |
| `JSON_TYPEOF(json)` | `TEXT` | `TEXT` | Returns `'null'`, `'boolean'`, `'number'`, `'string'`, `'array'`, `'object'` |
| `JSON_ARRAY_LENGTH(json)` | `TEXT` | `INTEGER` | Returns number of elements in JSON array |
| `JSON_OBJECT_LENGTH(json)` | `TEXT` | `INTEGER` | Returns number of key-value pairs in JSON object |
| `JSON_STRIP_NULLS(json)` | `TEXT` | `TEXT` | Recursively removes all keys with `null` values |

## String, Pattern, and Regex Functions

| Function | Arguments | Returns | Description |
|---|---|---|---|
| `REGEXP(pattern, text)` | `TEXT, TEXT` | `INTEGER` | Regular expression match (Rust `regex` syntax), usually written `text REGEXP pattern` |
| `ILIKE(text, pattern)` | `TEXT, TEXT` | `INTEGER` | Case-insensitive pattern match with `%` and `_` |
| `REVERSE(text)` | `TEXT` | `TEXT` | Reverses Unicode string |
| `LEFT(text, n)` / `RIGHT(text, n)` | `TEXT, INT` | `TEXT` | Returns first or last `n` characters |
| `SPLIT_PART(text, sep, pos)` | `TEXT, TEXT, INT` | `TEXT` | PostgreSQL-compatible string split (1-indexed) |
| `REPEAT(text, count)` | `TEXT, INT` | `TEXT` | Repeats string `count` times |
| `LPAD(text, len, pad)` | `TEXT, INT, TEXT` | `TEXT` | Left-pads string to specified length |
| `RPAD(text, len, pad)` | `TEXT, INT, TEXT` | `TEXT` | Right-pads string to specified length |
| `INITCAP(text)` | `TEXT` | `TEXT` | Capitalizes first letter of each word |
| `CHAR_LENGTH(text)` | `TEXT` | `INTEGER` | Unicode character count (not raw byte count) |

## Mathematical, Trigonometric, and Geospatial Functions

| Function | Arguments | Returns | Description |
|---|---|---|---|
| `PI()` | None | `REAL` | Mathematical constant Pi (`3.141592653589793`) |
| `POWER(x, y)` / `POW(x, y)` | `REAL, REAL` | `REAL` | Raises `x` to the power of `y` |
| `SQRT(x)` | `REAL` | `REAL` | Square root of non-negative number |
| `CBRT(x)` | `REAL` | `REAL` | Cube root of number |
| `EXP(x)` | `REAL` | `REAL` | Natural exponential (e^x) |
| `LN(x)` | `REAL` | `REAL` | Natural logarithm |
| `LOG10(x)` | `REAL` | `REAL` | Base-10 logarithm |
| `LOG2(x)` | `REAL` | `REAL` | Base-2 logarithm |
| `SIN(x)`, `COS(x)`, `TAN(x)` | `REAL` | `REAL` | Standard trigonometric functions (radians) |
| `ASIN(x)`, `ACOS(x)`, `ATAN(x)` | `REAL` | `REAL` | Inverse trigonometric functions |
| `ATAN2(y, x)` | `REAL, REAL` | `REAL` | Arc tangent of two variables |
| `DEGREES(rad)` / `RADIANS(deg)` | `REAL` | `REAL` | Conversion between degrees and radians |
| `FLOOR(x)` / `CEIL(x)` / `CEILING(x)` | `REAL` | `REAL` | Floor and ceiling integer rounding |
| `TRUNC(x)` | `REAL` | `REAL` | Truncates decimal digits toward zero |
| `SIGN(x)` | `REAL` | `INTEGER` | Returns `1` if positive, `-1` if negative, `0` if zero |
| `MOD(x, y)` | `INT, INT` | `INTEGER` | Modulo (x mod y) |
| `GEO_HAVERSINE_DISTANCE(lat1, lon1, lat2, lon2)` | `4 x REAL` | `REAL` | Great-circle distance between coordinates in **meters** |
| `GEO_DISTANCE_KM(lat1, lon1, lat2, lon2)` | `4 x REAL` | `REAL` | Great-circle distance between coordinates in **kilometers** |

## Cryptography, Hashing, Encoding, and Randomness

| Function | Arguments | Returns | Description |
|---|---|---|---|
| `SHA256(data)` | `TEXT/BLOB` | `TEXT` | Cryptographic SHA-256 hash (hex string) |
| `SHA512(data)` | `TEXT/BLOB` | `TEXT` | Cryptographic SHA-512 hash (hex string) |
| `MD5(data)` | `TEXT/BLOB` | `TEXT` | MD5 hash (hex string) |
| `SHA1(data)` | `TEXT/BLOB` | `TEXT` | SHA-1 hash (hex string) |
| `HMAC_SHA256(key, msg)` | `TEXT, TEXT` | `TEXT` | HMAC-SHA256 keyed signature (hex string) |
| `BASE64_ENCODE(data)` | `TEXT/BLOB` | `TEXT` | Encodes binary/text to Base64 format |
| `BASE64_DECODE(b64_str)` | `TEXT` | `TEXT/BLOB` | Decodes Base64 string to original payload |
| `HEX_ENCODE(blob)` | `BLOB` | `TEXT` | Encodes binary blob to hex string |
| `HEX_DECODE(hex_str)` | `TEXT` | `BLOB` | Decodes hex string to binary byte blob |
| `RANDOM_STRING(len)` | `INTEGER` | `TEXT` | Generates cryptographically strong random string |

## Extended Aggregations

| Function | Arguments | Returns | Description |
|---|---|---|---|
| `STRING_AGG(col, sep)` | `TEXT, TEXT` | `TEXT` | Concatenates column values with separator |
| `JSON_AGG(col)` | `ANY` | `TEXT` | Aggregates column values into JSON array |
| `JSON_OBJECT_AGG(k, v)` | `TEXT, ANY` | `TEXT` | Aggregates key-value pairs into a JSON object (the key must be `TEXT`; cast numbers with `CAST(k AS TEXT)`) |
| `ARRAY_AGG(col)` | `ANY` | `TEXT` | Alias for `JSON_AGG` |
| `BIT_AND(int_col)` | `INTEGER` | `INTEGER` | Bitwise AND across all rows |
| `BIT_OR(int_col)` | `INTEGER` | `INTEGER` | Bitwise OR across all rows |
| `BIT_XOR(int_col)` | `INTEGER` | `INTEGER` | Bitwise XOR across all rows |
| `BOOL_AND(bool_col)` | `BOOLEAN` | `BOOLEAN` | Logical AND across all rows (TRUE if all true) |
| `BOOL_OR(bool_col)` | `BOOLEAN` | `BOOLEAN` | Logical OR across all rows (TRUE if at least one true) |
| `EVERY(bool_col)` | `BOOLEAN` | `BOOLEAN` | SQL-standard alias for `BOOL_AND` |

## SQL Server compatibility functions

Available for T-SQL scripts (see [SQL dialect](/reference/sql-dialect#t-sql-compatibility)). They aim at common
usage, not exact SQL Server semantics: date functions return text in NovaDB's formats.

Worked as expected in our tests: `GETDATE()`, `SYSDATETIME()`, `NEWID()`, `NEWSEQUENTIALID()`, `ISNULL()`,
`ISNUMERIC()`, `ISJSON()`, `JSON_VALUE()`, `JSON_QUERY()`, `JSON_MODIFY()`, `IIF()`, `CHOOSE()`, `LEN()`,
`REPLICATE()`, `DATEADD()`, `DATEDIFF()`, `DATEFROMPARTS()`, `EOMONTH()`, `DATETRUNC()`, `DAY()`, `MONTH()`,
`YEAR()`, `GREATEST()`, `LEAST()`, `TRIM()`, `CONCAT()`, `STRING_SPLIT()`, `OPENJSON()`, `TRY_CAST()`,
`TRY_CONVERT()`, `TRY_PARSE()` (`NULL` on failure) and `FORMAT()` (numeric `N`/`F`/`P`/`C`/`D`/`X`/`E`/`G`, custom
date patterns, some cultures).

Implemented (checked on the 2026-10-12 development build): `HASHBYTES` (`MD5`, `SHA1`, `SHA2_256`, `SHA2_512`; text
hashed as UTF-8), `DATEPART`, `DATENAME`, `DATEADD`, `DATEDIFF`, `DATEDIFF_BIG`, `EOMONTH`, `DATETRUNC`,
`DATE_BUCKET`, `CONVERT` styles 0-14, 20-25, 100-114, 120, 121, 125-127 (other styles error), `RAND([seed])`,
`CHECKSUM`/`BINARY_CHECKSUM` (**not** SQL Server's algorithm) and `CHECKSUM_AGG` (XOR), `TRANSLATE`,
`STRING_ESCAPE(x, 'json')`, `CONCAT_WS` (skips `NULL`), `COMPRESS`/`DECOMPRESS` (real GZIP), `STDEV`, `STDEVP`, `VAR`,
`VARP`, `PRODUCT`, `PERCENTILE_CONT/DISC`, `APPROX_PERCENTILE_*` (computed exactly), `PERCENT_RANK`,
`geometry::Point`/`geography::Point` (text `POINT (x y)`), `SESSION_CONTEXT` with `sp_set_session_context`,
`OBJECT_ID`, `OBJECT_NAME`, `IDENT_CURRENT`, `STRING_AGG ... WITHIN GROUP (ORDER BY ...)`, `REGEXP_LIKE/COUNT/INSTR/
SUBSTR/REPLACE`, `PATINDEX`, `UNISTR`. `SYSDATETIMEOFFSET()` and `CURRENT_TIMEZONE()` report UTC.

Metadata: `DB_NAME()` returns the database file name without extension (`main` in memory). `SERVERPROPERTY()` returns fixed values and `XACT_STATE()` is always 0.

Unsupported on purpose (clear error): `SWITCHOFFSET`, `TODATETIMEOFFSET`, `AT TIME ZONE`, spatial methods
(`STDistance`, ...), `EDIT_DISTANCE*`, `JARO_WINKLER_*`, `VECTOR_DISTANCE`, `REGEXP_MATCHES`,
`REGEXP_SPLIT_TO_TABLE`, `GROUPING()`/`GROUPING_ID()`, `SESSIONPROPERTY`, `COLUMNPROPERTY`, `INDEXPROPERTY`,
`FILE_NAME`, `TYPE_ID`, `TYPE_NAME`, `DATABASEPROPERTYEX`, `sys.fn_*`. Implemented: `PARSENAME`, `ISDATE` (ISO and
time-only strings), `COL_LENGTH` (`INT`, `FLOAT`, `DECIMAL`/`NUMERIC` only) and `APPROX_COUNT_DISTINCT` (exact). Details: [From SQL Server](/migration/from-sql-server#sql-server-functions-implemented-approximate-missing).

## PostgreSQL-style helpers

`now()`, `gen_random_uuid()`, `string_agg()` (also `string_agg(x, ',' ORDER BY y)`), `split_part()`, `date_part()`,
`date_trunc()`, `starts_with()`, `generate_series()` (via the rewriter; the column is named `value`), `ilike(text, pattern)`
(function form only; the `ILIKE` operator is not supported).
