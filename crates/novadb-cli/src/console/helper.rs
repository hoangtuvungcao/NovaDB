//! rustyline helper: completion and syntax highlighting.

use std::borrow::Cow;

use rustyline::completion::{Completer, FilenameCompleter, Pair};
use rustyline::highlight::{CmdKind, Highlighter};
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{Context, Helper};

use super::commands::{COMMAND_NAMES, FILE_COMMANDS, TABLE_COMMANDS};
use super::sql::{self, KEYWORDS};

pub struct ConsoleHelper {
    pub color: bool,
    pub tables: Vec<String>,
    pub columns: Vec<String>,
    files: FilenameCompleter,
}

impl ConsoleHelper {
    pub fn new(color: bool) -> Self {
        Self {
            color,
            tables: Vec::new(),
            columns: Vec::new(),
            files: FilenameCompleter::new(),
        }
    }

    /// Completion candidates for `line` with the cursor at `pos`.
    pub fn candidates(&self, line: &str, pos: usize) -> (usize, Vec<String>) {
        let before = &line[..pos];
        let start = before
            .char_indices()
            .rev()
            .find(|(_, c)| !(c.is_alphanumeric() || *c == '_' || *c == '.' || *c == '\\'))
            .map_or(0, |(i, c)| i + c.len_utf8());
        let word = &before[start..];
        let trimmed = before.trim_start();

        // Dot / backslash command names.
        if (trimmed.starts_with('.') || trimmed.starts_with('\\'))
            && !trimmed.contains(char::is_whitespace)
        {
            let mut out: Vec<String> = COMMAND_NAMES
                .iter()
                .filter(|c| c.starts_with(word))
                .map(|c| (*c).to_owned())
                .collect();
            out.dedup();
            return (start, out);
        }
        let in_command = trimmed.starts_with('.') || trimmed.starts_with('\\');
        if in_command {
            let cmd = trimmed.split_whitespace().next().unwrap_or("");
            if TABLE_COMMANDS.contains(&cmd) {
                return (start, prefix_matches(&self.tables, word));
            }
            return (start, Vec::new());
        }

        // `table.col` -> complete the column part.
        let (offset, word) = match word.rfind('.') {
            Some(dot) => (start + dot + 1, &word[dot + 1..]),
            None => (start, word),
        };
        if offset != start {
            return (offset, prefix_matches(&self.columns, word));
        }

        let previous = sql::tokenize(&before[..start])
            .into_iter()
            .rev()
            .find(|t| t.kind != sql::TokenKind::Whitespace)
            .map(|t| before[t.start..t.end].to_ascii_uppercase());
        let wants_table = matches!(
            previous.as_deref(),
            Some("FROM" | "JOIN" | "INTO" | "UPDATE" | "TABLE")
        );
        if wants_table {
            return (start, prefix_matches(&self.tables, word));
        }
        if word.is_empty() {
            return (start, Vec::new());
        }
        let mut out = prefix_matches(&self.tables, word);
        out.extend(prefix_matches(&self.columns, word));
        let lower = word.chars().next().is_some_and(char::is_lowercase);
        out.extend(
            KEYWORDS
                .iter()
                .filter(|k| k.len() > word.len() || !k.eq_ignore_ascii_case(word))
                .filter(|k| starts_with_ignore_case(k, word))
                .map(|k| {
                    if lower {
                        k.to_ascii_lowercase()
                    } else {
                        (*k).to_owned()
                    }
                }),
        );
        let mut seen = std::collections::HashSet::new();
        out.retain(|c| seen.insert(c.clone()));
        (start, out)
    }
}

fn starts_with_ignore_case(candidate: &str, prefix: &str) -> bool {
    candidate.len() >= prefix.len()
        && candidate.is_char_boundary(prefix.len())
        && candidate[..prefix.len()].eq_ignore_ascii_case(prefix)
}

fn prefix_matches(names: &[String], prefix: &str) -> Vec<String> {
    let mut out: Vec<String> = names
        .iter()
        .filter(|n| starts_with_ignore_case(n, prefix))
        .cloned()
        .collect();
    out.sort();
    out.dedup();
    out
}

impl Completer for ConsoleHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let trimmed = line[..pos].trim_start();
        if let Some(cmd) = trimmed.split_whitespace().next() {
            if FILE_COMMANDS.contains(&cmd) && trimmed.contains(char::is_whitespace) {
                // Only the first argument is a file name.
                let args: Vec<&str> = trimmed.split_whitespace().collect();
                let on_first_arg =
                    args.len() == 1 || (args.len() == 2 && !trimmed.ends_with(char::is_whitespace));
                if on_first_arg {
                    return self.files.complete(line, pos, ctx);
                }
            }
        }
        let (start, names) = self.candidates(line, pos);
        Ok((
            start,
            names
                .into_iter()
                .map(|n| Pair {
                    display: n.clone(),
                    replacement: n,
                })
                .collect(),
        ))
    }
}

impl Hinter for ConsoleHelper {
    type Hint = String;
}

impl Validator for ConsoleHelper {}

impl Highlighter for ConsoleHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        if self.color {
            Cow::Owned(sql::highlight(line))
        } else {
            Cow::Borrowed(line)
        }
    }

    fn highlight_char(&self, _line: &str, _pos: usize, _kind: CmdKind) -> bool {
        self.color
    }
}

impl Helper for ConsoleHelper {}

#[cfg(test)]
mod tests {
    use super::*;

    fn helper() -> ConsoleHelper {
        let mut h = ConsoleHelper::new(false);
        h.tables = vec!["users".into(), "orders".into()];
        h.columns = vec!["user_id".into(), "email".into()];
        h
    }

    #[test]
    fn completes_commands_tables_columns_and_keywords() {
        let h = helper();
        let (start, c) = h.candidates(".ta", 3);
        assert_eq!(start, 0);
        assert!(c.contains(&".tables".to_owned()));
        let (_, c) = h.candidates("\\d", 2);
        assert!(c.contains(&"\\dt".to_owned()));
        let (start, c) = h.candidates("SELECT * FROM u", 15);
        assert_eq!((start, c), (14, vec!["users".to_owned()]));
        let (_, c) = h.candidates("sel", 3);
        assert_eq!(c, vec!["select".to_owned()]);
        let (start, c) = h.candidates("SELECT u.em", 11);
        assert_eq!((start, c), (9, vec!["email".to_owned()]));
        let (_, c) = h.candidates(".schema o", 9);
        assert_eq!(c, vec!["orders".to_owned()]);
    }
}
