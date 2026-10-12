//! Result rendering for the console: Unicode box tables, expanded records,
//! JSON, and CSV.

use std::fmt::Write as _;

use serde_json::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Maximum display width of one table cell before it is truncated with `…`.
pub const MAX_CELL_WIDTH: usize = 48;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Table,
    Line,
    Json,
    Csv,
}

impl OutputMode {
    pub fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "table" | "box" => Some(Self::Table),
            "line" | "lines" | "vertical" | "expanded" => Some(Self::Line),
            "json" => Some(Self::Json),
            "csv" => Some(Self::Csv),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Table => "table",
            Self::Line => "line",
            Self::Json => "json",
            Self::Csv => "csv",
        }
    }
}

/// ANSI styling that collapses to plain text when color is disabled.
#[derive(Debug, Clone, Copy)]
pub struct Style {
    pub color: bool,
}

impl Style {
    pub fn paint(self, code: &str, text: &str) -> String {
        if self.color && !text.is_empty() {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }
    pub fn bold(self, text: &str) -> String {
        self.paint("1", text)
    }
    pub fn dim(self, text: &str) -> String {
        self.paint("2", text)
    }
    pub fn border(self, text: &str) -> String {
        self.paint("90", text)
    }
    pub fn header(self, text: &str) -> String {
        self.paint("1;36", text)
    }
    pub fn red(self, text: &str) -> String {
        self.paint("1;31", text)
    }
    pub fn green(self, text: &str) -> String {
        self.paint("32", text)
    }
    pub fn yellow(self, text: &str) -> String {
        self.paint("33", text)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CellKind {
    Null,
    Number,
    Text,
}

struct Cell {
    text: String,
    kind: CellKind,
}

fn blob_len(value: &Value) -> Option<usize> {
    let object = value.as_object()?;
    if object.get("$novadb_type")?.as_str()? != "blob" {
        return None;
    }
    let b64 = object.get("base64")?.as_str()?;
    let padding = b64.bytes().rev().take_while(|b| *b == b'=').count();
    Some(b64.len() / 4 * 3 - padding)
}

/// Plain text for a JSON value (strings unquoted; NULL as `NULL`).
pub fn value_text(value: Option<&Value>) -> String {
    cell(value).text
}

fn cell(value: Option<&Value>) -> Cell {
    match value {
        None | Some(Value::Null) => Cell {
            text: "NULL".to_owned(),
            kind: CellKind::Null,
        },
        Some(Value::Number(n)) => Cell {
            text: n.to_string(),
            kind: CellKind::Number,
        },
        Some(Value::String(s)) => Cell {
            text: s.clone(),
            kind: CellKind::Text,
        },
        Some(Value::Bool(b)) => Cell {
            text: b.to_string(),
            kind: CellKind::Text,
        },
        Some(other) => {
            if let Some(len) = blob_len(other) {
                return Cell {
                    text: format!("<blob {len} bytes>"),
                    kind: CellKind::Text,
                };
            }
            if let Some(v) = other
                .get("$novadb_type")
                .filter(|t| t.as_str() == Some("real"))
                .and_then(|_| other.get("value"))
                .and_then(Value::as_str)
            {
                return Cell {
                    text: v.to_owned(),
                    kind: CellKind::Number,
                };
            }
            Cell {
                text: other.to_string(),
                kind: CellKind::Text,
            }
        }
    }
}

/// Makes a value safe for single-line display (newlines, tabs, control chars).
fn single_line(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\n' => '↵',
            '\t' => ' ',
            c if c.is_control() => '·',
            c => c,
        })
        .collect()
}

/// Truncates `text` to at most `max` display columns, appending `…`.
pub fn truncate(text: &str, max: usize) -> String {
    if text.width() <= max {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut width = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if width + w + 1 > max {
            break;
        }
        width += w;
        out.push(c);
    }
    out.push('…');
    out
}

fn pad(text: &str, width: usize, right_align: bool) -> String {
    let fill = " ".repeat(width.saturating_sub(text.width()));
    if right_align {
        format!("{fill}{text}")
    } else {
        format!("{text}{fill}")
    }
}

/// Renders rows as a Unicode box-drawing table.
pub fn render_table(columns: &[String], rows: &[Value], style: Style) -> String {
    if columns.is_empty() {
        return format!("{}\n", style.dim("(no columns)"));
    }
    let headers: Vec<String> = columns
        .iter()
        .map(|c| truncate(&single_line(c), MAX_CELL_WIDTH))
        .collect();
    let body: Vec<Vec<Cell>> = rows
        .iter()
        .map(|row| {
            columns
                .iter()
                .map(|c| {
                    let mut cell = cell(row.get(c));
                    cell.text = truncate(&single_line(&cell.text), MAX_CELL_WIDTH);
                    cell
                })
                .collect()
        })
        .collect();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.width()).collect();
    for row in &body {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.text.width());
        }
    }
    // Numeric columns are right-aligned when every non-NULL value is numeric.
    let numeric: Vec<bool> = (0..columns.len())
        .map(|i| {
            let mut any = false;
            for row in &body {
                match row[i].kind {
                    CellKind::Number => any = true,
                    CellKind::Text => return false,
                    CellKind::Null => {}
                }
            }
            any
        })
        .collect();

    let rule = |left: &str, mid: &str, right: &str| {
        let parts: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
        style.border(&format!("{left}{}{right}", parts.join(mid)))
    };
    let bar = style.border("│");

    let mut out = String::new();
    let _ = writeln!(out, "{}", rule("┌", "┬", "┐"));
    let mut line = bar.clone();
    for (i, h) in headers.iter().enumerate() {
        let _ = write!(
            line,
            " {} {bar}",
            style.header(&pad(h, widths[i], numeric[i]))
        );
    }
    let _ = writeln!(out, "{line}");
    let _ = writeln!(out, "{}", rule("├", "┼", "┤"));
    for row in &body {
        let mut line = bar.clone();
        for (i, cell) in row.iter().enumerate() {
            let right = numeric[i] || cell.kind == CellKind::Number;
            let padded = pad(&cell.text, widths[i], right);
            let shown = if cell.kind == CellKind::Null {
                style.dim(&padded)
            } else {
                padded
            };
            let _ = write!(line, " {shown} {bar}");
        }
        let _ = writeln!(out, "{line}");
    }
    let _ = writeln!(out, "{}", rule("└", "┴", "┘"));
    out
}

/// Renders rows as expanded records, one `column │ value` line per field.
pub fn render_lines(columns: &[String], rows: &[Value], style: Style) -> String {
    let name_width = columns.iter().map(|c| c.width()).max().unwrap_or(0);
    let mut out = String::new();
    for (index, row) in rows.iter().enumerate() {
        let title = format!("─[ RECORD {} ]", index + 1);
        let fill = "─".repeat((name_width + 3).saturating_sub(title.width()).max(4));
        let _ = writeln!(out, "{}", style.border(&format!("{title}{fill}")));
        for column in columns {
            let cell = cell(row.get(column));
            let value = match cell.kind {
                CellKind::Null => style.dim(&cell.text),
                _ => cell
                    .text
                    .replace('\n', &format!("\n{} │ ", " ".repeat(name_width))),
            };
            let _ = writeln!(
                out,
                "{} {} {value}",
                style.bold(&pad(column, name_width, false)),
                style.border("│")
            );
        }
    }
    out
}

/// Renders rows as a JSON array of objects, keys in column order.
pub fn render_json(columns: &[String], rows: &[Value]) -> String {
    if rows.is_empty() {
        return "[]\n".to_owned();
    }
    let mut out = String::from("[\n");
    for (index, row) in rows.iter().enumerate() {
        out.push_str("  {");
        for (i, column) in columns.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            let key = serde_json::to_string(column).unwrap_or_default();
            let value = row.get(column).unwrap_or(&Value::Null);
            let _ = write!(out, "{key}: {value}");
        }
        out.push('}');
        if index + 1 < rows.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str("]\n");
    out
}

/// Quotes a CSV field when needed (RFC 4180).
pub fn csv_field(text: &str) -> String {
    if text.contains([',', '"', '\n', '\r']) || text.starts_with(' ') || text.ends_with(' ') {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_owned()
    }
}

/// Renders rows as CSV with a header line. NULL becomes an empty field.
pub fn render_csv(columns: &[String], rows: &[Value]) -> String {
    let mut out = String::new();
    let header: Vec<String> = columns.iter().map(|c| csv_field(c)).collect();
    let _ = writeln!(out, "{}", header.join(","));
    for row in rows {
        let fields: Vec<String> = columns
            .iter()
            .map(|c| match row.get(c) {
                None | Some(Value::Null) => String::new(),
                value => csv_field(&value_text(value)),
            })
            .collect();
        let _ = writeln!(out, "{}", fields.join(","));
    }
    out
}

pub fn render(mode: OutputMode, columns: &[String], rows: &[Value], style: Style) -> String {
    match mode {
        OutputMode::Table => render_table(columns, rows, style),
        OutputMode::Line => render_lines(columns, rows, style),
        OutputMode::Json => render_json(columns, rows),
        OutputMode::Csv => render_csv(columns, rows),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PLAIN: Style = Style { color: false };

    fn cols(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn table_aligns_numbers_right_and_text_left() {
        let rows = vec![
            json!({"id": 1, "name": "alice"}),
            json!({"id": 1000, "name": null}),
        ];
        let out = render_table(&cols(&["id", "name"]), &rows, PLAIN);
        let expected = "\
┌──────┬───────┐
│   id │ name  │
├──────┼───────┤
│    1 │ alice │
│ 1000 │ NULL  │
└──────┴───────┘
";
        assert_eq!(out, expected);
        assert!(!out.contains('\x1b'));
    }

    #[test]
    fn table_truncates_long_values_and_flattens_newlines() {
        let long = "x".repeat(100);
        let rows = vec![json!({"v": long, "w": "a\nb"})];
        let out = render_table(&cols(&["v", "w"]), &rows, PLAIN);
        let expected_cell = format!("{}…", "x".repeat(MAX_CELL_WIDTH - 1));
        assert!(out.contains(&expected_cell), "{out}");
        assert!(out.contains("a↵b"));
    }

    #[test]
    fn table_handles_wide_characters() {
        let rows = vec![json!({"name": "日本"}), json!({"name": "Việt"})];
        let out = render_table(&cols(&["name"]), &rows, PLAIN);
        let widths: Vec<usize> = out.lines().map(UnicodeWidthStr::width).collect();
        assert!(widths.iter().all(|w| *w == widths[0]), "{out}");
    }

    #[test]
    fn table_with_no_rows_shows_header() {
        let out = render_table(&cols(&["a"]), &[], PLAIN);
        assert_eq!(out, "┌───┐\n│ a │\n├───┤\n└───┘\n");
    }

    #[test]
    fn color_mode_dims_null() {
        let rows = vec![json!({"a": null})];
        let out = render_table(&cols(&["a"]), &rows, Style { color: true });
        assert!(out.contains("\x1b[2mNULL\x1b[0m"));
    }

    #[test]
    fn blobs_render_as_size() {
        let rows = vec![json!({"b": {"$novadb_type": "blob", "base64": "AAEC"}})];
        let out = render_table(&cols(&["b"]), &rows, PLAIN);
        assert!(out.contains("<blob 3 bytes>"), "{out}");
    }

    #[test]
    fn line_mode_renders_records() {
        let rows = vec![json!({"id": 1, "name": "bob"})];
        let out = render_lines(&cols(&["id", "name"]), &rows, PLAIN);
        assert!(out.starts_with("─[ RECORD 1 ]"));
        assert!(out.contains("id   │ 1\n"));
        assert!(out.contains("name │ bob\n"));
    }

    #[test]
    fn csv_and_json_modes() {
        let rows = vec![json!({"b": "x,y", "a": 1}), json!({"b": null, "a": 2.5})];
        let columns = cols(&["b", "a"]);
        assert_eq!(render_csv(&columns, &rows), "b,a\n\"x,y\",1\n,2.5\n");
        assert_eq!(
            render_json(&columns, &rows),
            "[\n  {\"b\": \"x,y\", \"a\": 1},\n  {\"b\": null, \"a\": 2.5}\n]\n"
        );
    }
}
