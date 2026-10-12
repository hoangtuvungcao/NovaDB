//! Session-level commands handled by the gateway itself rather than SQLite:
//! transaction control, `SET` / `RESET` / `SHOW`, `DISCARD`, `TRUNCATE`, ...

use crate::lexer::{Kind, SqlError, SqlResult, Token, split_commas};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Begin,
    Commit,
    Rollback,
    Savepoint(String),
    Release(String),
    RollbackTo(String),
    /// `SET name TO value` (the value is already unquoted and joined).
    Set {
        name: String,
        value: String,
    },
    /// `SET name TO DEFAULT` / `RESET name` / `RESET ALL`.
    Reset(Option<String>),
    /// Accepted and ignored (`SET TRANSACTION ...`, `SET CONSTRAINTS ...`).
    SetIgnored,
    Show(String),
    ShowAll,
    Discard,
    Deallocate(Option<String>),
    Comment,
    Truncate {
        tables: Vec<String>,
        restart: bool,
    },
    /// Statements that have no SQLite equivalent and are rejected clearly.
    Unsupported(String),
}

fn lower_words(tokens: &[Token]) -> Vec<String> {
    tokens
        .iter()
        .map(|t| match t.kind {
            Kind::Word => t.lower(),
            _ => String::new(),
        })
        .collect()
}

fn value_text(tokens: &[Token]) -> String {
    tokens
        .iter()
        .map(|t| match t.kind {
            Kind::Str => t.value.clone(),
            Kind::QIdent => t.value.clone(),
            _ => t.text.clone(),
        })
        .collect::<Vec<_>>()
        .join("")
}

/// Parses a gateway-handled command; `None` means "run it as SQL".
pub fn parse(tokens: &[Token]) -> Option<SqlResult<Command>> {
    let words = lower_words(tokens);
    let first = words.first()?.as_str();
    let ident_at = |i: usize| tokens.get(i).and_then(Token::ident);
    match first {
        "begin" => Some(Ok(Command::Begin)),
        "start" if words.get(1).map(String::as_str) == Some("transaction") => {
            Some(Ok(Command::Begin))
        }
        "commit" | "end" => {
            if words.get(1).map(String::as_str) == Some("prepared") {
                return Some(Ok(Command::Unsupported(
                    "two-phase commit is not supported by NovaDB".into(),
                )));
            }
            Some(Ok(Command::Commit))
        }
        "abort" => Some(Ok(Command::Rollback)),
        "rollback" => {
            let mut i = 1;
            if matches!(words.get(i).map(String::as_str), Some("work" | "transaction")) {
                i += 1;
            }
            if words.get(i).map(String::as_str) == Some("prepared") {
                return Some(Ok(Command::Unsupported(
                    "two-phase commit is not supported by NovaDB".into(),
                )));
            }
            if words.get(i).map(String::as_str) == Some("to") {
                i += 1;
                if words.get(i).map(String::as_str) == Some("savepoint") {
                    i += 1;
                }
                return Some(
                    ident_at(i)
                        .map(Command::RollbackTo)
                        .ok_or_else(|| SqlError::Syntax("syntax error at end of input".into())),
                );
            }
            Some(Ok(Command::Rollback))
        }
        "savepoint" => Some(
            ident_at(1)
                .map(Command::Savepoint)
                .ok_or_else(|| SqlError::Syntax("syntax error at end of input".into())),
        ),
        "release" => {
            let i = if words.get(1).map(String::as_str) == Some("savepoint") { 2 } else { 1 };
            Some(
                ident_at(i)
                    .map(Command::Release)
                    .ok_or_else(|| SqlError::Syntax("syntax error at end of input".into())),
            )
        }
        "set" => Some(parse_set(tokens, &words)),
        "reset" => {
            let name = value_text(&tokens[1..]);
            if name.eq_ignore_ascii_case("all") {
                Some(Ok(Command::Reset(None)))
            } else if name.is_empty() {
                Some(Err(SqlError::Syntax("syntax error at end of input".into())))
            } else {
                Some(Ok(Command::Reset(Some(normalize_name(&name)))))
            }
        }
        "show" => {
            let name = words[1..]
                .iter()
                .zip(&tokens[1..])
                .map(|(w, t)| if w.is_empty() { t.text.clone() } else { w.clone() })
                .collect::<Vec<_>>()
                .join(" ");
            if name.is_empty() {
                return Some(Err(SqlError::Syntax("syntax error at end of input".into())));
            }
            if name == "all" {
                return Some(Ok(Command::ShowAll));
            }
            Some(Ok(Command::Show(normalize_name(&name))))
        }
        "discard" => Some(Ok(Command::Discard)),
        "deallocate" => {
            let rest: Vec<&Token> = tokens[1..]
                .iter()
                .filter(|t| !t.is_word("PREPARE"))
                .collect();
            match rest.first() {
                None => Some(Err(SqlError::Syntax("syntax error at end of input".into()))),
                Some(t) if t.is_word("ALL") => Some(Ok(Command::Deallocate(None))),
                Some(t) => Some(Ok(Command::Deallocate(t.ident()))),
            }
        }
        "comment" if words.get(1).map(String::as_str) == Some("on") => Some(Ok(Command::Comment)),
        "listen" | "unlisten" | "notify" => Some(Ok(Command::Unsupported(
            "LISTEN / NOTIFY is not supported by NovaDB".into(),
        ))),
        "prepare" | "execute" => Some(Ok(Command::Unsupported(
            "SQL-level PREPARE / EXECUTE is not supported; use the protocol-level Parse / Bind / Execute"
                .into(),
        ))),
        "lock" => Some(Ok(Command::Unsupported(
            "LOCK TABLE is not supported by NovaDB (writes are serialised)".into(),
        ))),
        "truncate" => {
            let mut i = 1;
            if words.get(i).map(String::as_str) == Some("table") {
                i += 1;
            }
            if words.get(i).map(String::as_str) == Some("only") {
                i += 1;
            }
            let mut end = tokens.len();
            let mut restart = false;
            for (j, w) in words.iter().enumerate().skip(i) {
                match w.as_str() {
                    "restart" => {
                        restart = true;
                        end = end.min(j);
                    }
                    "continue" | "cascade" | "restrict" => end = end.min(j),
                    _ => {}
                }
            }
            let tables: Vec<String> = split_commas(&tokens[i..end])
                .iter()
                .filter_map(|part| part.last().and_then(Token::ident))
                .collect();
            if tables.is_empty() {
                return Some(Err(SqlError::Syntax("syntax error at end of input".into())));
            }
            Some(Ok(Command::Truncate { tables, restart }))
        }
        _ => None,
    }
}

fn normalize_name(name: &str) -> String {
    let lower = name.trim().to_ascii_lowercase();
    match lower.as_str() {
        "transaction isolation level" | "transaction isolation" => "transaction_isolation".into(),
        "time zone" => "timezone".into(),
        "session authorization" => "session_authorization".into(),
        other => other.replace(' ', "_"),
    }
}

fn parse_set(tokens: &[Token], words: &[String]) -> SqlResult<Command> {
    let mut i = 1;
    if matches!(words.get(i).map(String::as_str), Some("session" | "local")) {
        // SET SESSION CHARACTERISTICS AS TRANSACTION ... / SET SESSION AUTHORIZATION
        if words.get(i + 1).map(String::as_str) == Some("characteristics") {
            return Ok(Command::SetIgnored);
        }
        if words.get(i + 1).map(String::as_str) == Some("authorization") {
            return Ok(Command::SetIgnored);
        }
        i += 1;
    }
    match words.get(i).map(String::as_str) {
        Some("transaction" | "constraints") => return Ok(Command::SetIgnored),
        Some("role") => return Ok(Command::SetIgnored),
        Some("names") => {
            let value = value_text(&tokens[i + 1..]);
            return Ok(Command::Set {
                name: "client_encoding".into(),
                value,
            });
        }
        Some("time") if words.get(i + 1).map(String::as_str) == Some("zone") => {
            let rest = &tokens[i + 2..];
            let rest = if rest.first().is_some_and(|t| t.is_word("INTERVAL")) {
                &rest[1..]
            } else {
                rest
            };
            // Stop at the INTERVAL field qualifier (HOUR TO MINUTE).
            let end = rest
                .iter()
                .position(|t| t.kind == Kind::Word && !t.is_word("DEFAULT") && !t.is_word("LOCAL"))
                .unwrap_or(rest.len());
            let rest = &rest[..end];
            if rest.is_empty() {
                return Err(SqlError::Syntax("syntax error at end of input".into()));
            }
            if rest[0].is_word("DEFAULT") || rest[0].is_word("LOCAL") {
                return Ok(Command::Reset(Some("timezone".into())));
            }
            let value = value_text(rest);
            let value = if value.trim_start_matches(['+', '-', '0', ':']).is_empty() {
                "UTC".to_owned()
            } else {
                value
            };
            return Ok(Command::Set {
                name: "timezone".into(),
                value,
            });
        }
        _ => {}
    }
    // name { TO | = } value[, value...]
    let mut name = String::new();
    while let Some(token) = tokens.get(i) {
        if token.is_word("TO") || token.is_op("=") {
            break;
        }
        match token.kind {
            Kind::Word | Kind::QIdent => name.push_str(&token.ident().unwrap_or_default()),
            Kind::Op if token.text == "." => name.push('.'),
            _ => {
                return Err(SqlError::Syntax(format!(
                    "syntax error at or near \"{}\"",
                    token.text
                )));
            }
        }
        i += 1;
    }
    if name.is_empty() || i >= tokens.len() {
        return Err(SqlError::Syntax("syntax error at end of input".into()));
    }
    let values = &tokens[i + 1..];
    if values.is_empty() {
        return Err(SqlError::Syntax("syntax error at end of input".into()));
    }
    if values.len() == 1 && values[0].is_word("DEFAULT") {
        return Ok(Command::Reset(Some(name.to_ascii_lowercase())));
    }
    let joined = split_commas(values)
        .iter()
        .map(|part| value_text(part))
        .collect::<Vec<_>>()
        .join(", ");
    Ok(Command::Set {
        name: name.to_ascii_lowercase(),
        value: joined,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::tokenize;

    fn parse_sql(sql: &str) -> Option<Command> {
        parse(&tokenize(sql).unwrap()).map(|r| r.unwrap())
    }

    #[test]
    fn transaction_commands() {
        assert_eq!(
            parse_sql("BEGIN ISOLATION LEVEL SERIALIZABLE"),
            Some(Command::Begin)
        );
        assert_eq!(
            parse_sql("start transaction read only"),
            Some(Command::Begin)
        );
        assert_eq!(parse_sql("COMMIT WORK"), Some(Command::Commit));
        assert_eq!(parse_sql("end"), Some(Command::Commit));
        assert_eq!(parse_sql("ABORT"), Some(Command::Rollback));
        assert_eq!(
            parse_sql("ROLLBACK TO SAVEPOINT \"sa_1\""),
            Some(Command::RollbackTo("sa_1".into()))
        );
        assert_eq!(parse_sql("ROLLBACK"), Some(Command::Rollback));
        assert_eq!(
            parse_sql("SAVEPOINT s1"),
            Some(Command::Savepoint("s1".into()))
        );
        assert_eq!(parse_sql("RELEASE s1"), Some(Command::Release("s1".into())));
        assert_eq!(parse_sql("SELECT 1"), None);
    }

    #[test]
    fn set_show_reset() {
        assert_eq!(
            parse_sql("SET client_min_messages TO warning"),
            Some(Command::Set {
                name: "client_min_messages".into(),
                value: "warning".into()
            })
        );
        assert_eq!(
            parse_sql("SET SESSION search_path = public, \"$user\""),
            Some(Command::Set {
                name: "search_path".into(),
                value: "public, $user".into()
            })
        );
        assert_eq!(
            parse_sql("SET LOCAL statement_timeout = 5000"),
            Some(Command::Set {
                name: "statement_timeout".into(),
                value: "5000".into()
            })
        );
        assert_eq!(
            parse_sql("SET TIME ZONE INTERVAL '+00:00' HOUR TO MINUTE"),
            Some(Command::Set {
                name: "timezone".into(),
                value: "UTC".into()
            })
        );
        assert_eq!(
            parse_sql("SET TIME ZONE 'Europe/Paris'"),
            Some(Command::Set {
                name: "timezone".into(),
                value: "Europe/Paris".into()
            })
        );
        assert_eq!(
            parse_sql("SET NAMES 'UTF8'"),
            Some(Command::Set {
                name: "client_encoding".into(),
                value: "UTF8".into()
            })
        );
        assert_eq!(
            parse_sql("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE"),
            Some(Command::SetIgnored)
        );
        assert_eq!(
            parse_sql("SET x TO DEFAULT"),
            Some(Command::Reset(Some("x".into())))
        );
        assert_eq!(parse_sql("RESET ALL"), Some(Command::Reset(None)));
        assert_eq!(
            parse_sql("RESET TimeZone"),
            Some(Command::Reset(Some("timezone".into())))
        );
        assert_eq!(
            parse_sql("SHOW server_version"),
            Some(Command::Show("server_version".into()))
        );
        assert_eq!(
            parse_sql("show transaction isolation level"),
            Some(Command::Show("transaction_isolation".into()))
        );
        assert_eq!(
            parse_sql("SHOW TIME ZONE"),
            Some(Command::Show("timezone".into()))
        );
        assert_eq!(parse_sql("SHOW ALL"), Some(Command::ShowAll));
    }

    #[test]
    fn truncate_options() {
        assert_eq!(
            parse_sql("TRUNCATE TABLE a, public.b RESTART IDENTITY CASCADE"),
            Some(Command::Truncate {
                tables: vec!["a".into(), "b".into()],
                restart: true
            })
        );
    }
}
