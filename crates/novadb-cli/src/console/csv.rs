//! Small RFC 4180 CSV reader and SQL generation for `.import`.

use anyhow::{Result, bail};

/// Parses CSV text into records. Supports quoted fields with embedded
/// delimiters, doubled quotes, and newlines; CRLF or LF line endings; and a
/// leading UTF-8 BOM. Blank lines are skipped.
pub fn parse_csv(text: &str, delimiter: char) -> Result<Vec<Vec<String>>> {
    Ok(parse_csv_lined(text, delimiter)?
        .into_iter()
        .map(|(_, record)| record)
        .collect())
}

/// Like [`parse_csv`], but pairs each record with the 1-based line number on
/// which it starts (for error reporting).
pub fn parse_csv_lined(text: &str, delimiter: char) -> Result<Vec<(usize, Vec<String>)>> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut records: Vec<(usize, Vec<String>)> = Vec::new();
    let mut rec_line = 1usize;
    let mut record: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut field_started = false; // distinguishes `""` from an empty line
    let mut line = 1usize;
    let mut quote_line = 0usize;
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        if in_quotes {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    field.push('"');
                }
                '"' => in_quotes = false,
                '\n' => {
                    line += 1;
                    field.push(c);
                }
                _ => field.push(c),
            }
            continue;
        }
        if !field_started && record.is_empty() && field.is_empty() {
            rec_line = line;
        }
        match c {
            '"' if field.is_empty() => {
                in_quotes = true;
                field_started = true;
                quote_line = line;
            }
            c if c == delimiter => {
                record.push(std::mem::take(&mut field));
                field_started = true;
            }
            '\r' if chars.peek() == Some(&'\n') => {}
            '\n' | '\r' => {
                line += 1;
                if field_started || !field.is_empty() || !record.is_empty() {
                    record.push(std::mem::take(&mut field));
                    records.push((rec_line, std::mem::take(&mut record)));
                }
                field_started = false;
            }
            _ => {
                field.push(c);
                field_started = true;
            }
        }
    }
    if in_quotes {
        bail!("unterminated quoted field starting on line {quote_line}");
    }
    if field_started || !field.is_empty() || !record.is_empty() {
        record.push(field);
        records.push((rec_line, record));
    }
    Ok(records)
}

/// Rows rejected by [`check_field_counts`]: `(line, fields found)`.
pub type BadRows = Vec<(usize, usize)>;

/// Validates that every record has as many fields as the header. Returns the
/// accepted records (header first) and the rejected rows. Unless `skip_bad`
/// is set, the first mismatch is an error naming its line number, so nothing
/// is silently dropped.
pub fn check_field_counts(
    lined: Vec<(usize, Vec<String>)>,
    skip_bad: bool,
) -> Result<(Vec<Vec<String>>, BadRows)> {
    let mut iter = lined.into_iter();
    let Some((_, header)) = iter.next() else {
        bail!("CSV file is empty");
    };
    let expected = header.len();
    let mut good = vec![header];
    let mut bad = Vec::new();
    for (line, record) in iter {
        if record.len() == expected {
            good.push(record);
        } else if skip_bad {
            bad.push((line, record.len()));
        } else {
            bail!(
                "line {line}: expected {expected} field{} but found {}; nothing was imported \
                 (use --skip-bad-rows to skip such rows)",
                if expected == 1 { "" } else { "s" },
                record.len()
            );
        }
    }
    Ok((good, bad))
}

/// Infers a column type from sample values: INTEGER, REAL, or TEXT.
/// Empty values are ignored; values with leading zeros (e.g. `007`) are TEXT.
pub fn infer_type<'a>(values: impl IntoIterator<Item = &'a str>) -> &'static str {
    let mut ty = "";
    for value in values {
        let v = value.trim();
        if v.is_empty() {
            continue;
        }
        let digits = v.strip_prefix('-').unwrap_or(v);
        let leading_zero = digits.len() > 1 && digits.starts_with('0') && !digits.starts_with("0.");
        if !leading_zero && v.parse::<i64>().is_ok() && !v.starts_with('+') {
            if ty.is_empty() {
                ty = "INTEGER";
            }
        } else if !leading_zero
            && !v.starts_with('+')
            && v.parse::<f64>().is_ok_and(f64::is_finite)
            && v.bytes().any(|b| b.is_ascii_digit())
        {
            if ty.is_empty() || ty == "INTEGER" {
                ty = "REAL";
            }
        } else {
            return "TEXT";
        }
    }
    if ty.is_empty() { "TEXT" } else { ty }
}

pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

pub fn quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Builds the SQL to import `records` (first record is the header) into
/// `table`. When `create` is set, a `CREATE TABLE` with inferred column types
/// is emitted first. Empty fields become NULL; other values are inserted as
/// string literals so the column affinity decides the stored type.
pub fn build_import_sql(table: &str, records: &[Vec<String>], create: bool) -> Result<String> {
    let Some((header, rows)) = records.split_first() else {
        bail!("CSV file is empty");
    };
    if header.iter().all(|h| h.trim().is_empty()) {
        bail!("CSV header row is empty");
    }
    let names: Vec<String> = header
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let h = h.trim();
            if h.is_empty() {
                format!("column{}", i + 1)
            } else {
                h.to_owned()
            }
        })
        .collect();
    for (index, row) in rows.iter().enumerate() {
        if row.len() > names.len() {
            bail!(
                "record {} has {} fields but the header has {}",
                index + 2,
                row.len(),
                names.len()
            );
        }
    }

    let table_sql = quote_ident(table);
    let mut sql = String::new();
    if create {
        let defs: Vec<String> = names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let ty = infer_type(rows.iter().filter_map(|r| r.get(i).map(String::as_str)));
                format!("{} {ty}", quote_ident(name))
            })
            .collect();
        sql.push_str(&format!(
            "CREATE TABLE IF NOT EXISTS {table_sql} ({});\n",
            defs.join(", ")
        ));
    }
    let column_list: Vec<String> = names.iter().map(|n| quote_ident(n)).collect();
    let column_list = column_list.join(", ");
    for chunk in rows.chunks(500) {
        let tuples: Vec<String> = chunk
            .iter()
            .map(|row| {
                let values: Vec<String> = (0..names.len())
                    .map(|i| match row.get(i) {
                        Some(v) if !v.is_empty() => quote_literal(v),
                        _ => "NULL".to_owned(),
                    })
                    .collect();
                format!("({})", values.join(", "))
            })
            .collect();
        sql.push_str(&format!(
            "INSERT INTO {table_sql} ({column_list}) VALUES\n{};\n",
            tuples.join(",\n")
        ));
    }
    Ok(sql)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(fields: &[&str]) -> Vec<String> {
        fields.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn parses_quoted_fields_and_line_endings() {
        let text = "\u{feff}id,name,note\r\n1,\"Smith, J\",\"say \"\"hi\"\"\"\r\n\n2,bob,\"multi\nline\"\n3,,\n";
        let records = parse_csv(text, ',').unwrap();
        assert_eq!(
            records,
            vec![
                rec(&["id", "name", "note"]),
                rec(&["1", "Smith, J", "say \"hi\""]),
                rec(&["2", "bob", "multi\nline"]),
                rec(&["3", "", ""]),
            ]
        );
    }

    #[test]
    fn parses_without_trailing_newline_and_tabs() {
        let records = parse_csv("a\tb\nx\ty", '\t').unwrap();
        assert_eq!(records, vec![rec(&["a", "b"]), rec(&["x", "y"])]);
        let records = parse_csv("a\n\"\"", ',').unwrap();
        assert_eq!(records, vec![rec(&["a"]), rec(&[""])]);
    }

    #[test]
    fn tracks_record_start_lines_and_checks_counts() {
        let text = "a,b\n\n1,\"x\ny\"\n2\n3,4\n";
        let lined = parse_csv_lined(text, ',').unwrap();
        let lines: Vec<usize> = lined.iter().map(|(l, _)| *l).collect();
        assert_eq!(lines, vec![1, 3, 5, 6]);
        let err = check_field_counts(lined.clone(), false).unwrap_err();
        assert!(err.to_string().contains("line 5"), "{err}");
        let (good, bad) = check_field_counts(lined, true).unwrap();
        assert_eq!(good.len(), 3);
        assert_eq!(bad, vec![(5, 1)]);
    }

    #[test]
    fn rejects_unterminated_quotes() {
        let err = parse_csv("a,b\n1,\"oops\n", ',').unwrap_err();
        assert!(err.to_string().contains("line 2"), "{err}");
    }

    #[test]
    fn infers_column_types() {
        assert_eq!(infer_type(["1", "-2", ""]), "INTEGER");
        assert_eq!(infer_type(["1", "2.5"]), "REAL");
        assert_eq!(infer_type(["0.5", "1e3"]), "REAL");
        assert_eq!(infer_type(["007", "8"]), "TEXT");
        assert_eq!(infer_type(["1", "x"]), "TEXT");
        assert_eq!(infer_type(["", ""]), "TEXT");
        assert_eq!(infer_type(["NaN"]), "TEXT");
    }

    #[test]
    fn builds_create_and_insert_sql() {
        let records = vec![rec(&["id", "na\"me"]), rec(&["1", "O'Brien"]), rec(&["2"])];
        let sql = build_import_sql("my table", &records, true).unwrap();
        assert_eq!(
            sql,
            "CREATE TABLE IF NOT EXISTS \"my table\" (\"id\" INTEGER, \"na\"\"me\" TEXT);\n\
             INSERT INTO \"my table\" (\"id\", \"na\"\"me\") VALUES\n('1', 'O''Brien'),\n('2', NULL);\n"
        );
        let too_many = vec![rec(&["a"]), rec(&["1", "2"])];
        assert!(build_import_sql("t", &too_many, false).is_err());
    }
}
