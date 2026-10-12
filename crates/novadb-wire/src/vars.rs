//! Session variables (`SET` / `SHOW` / `RESET`) and server identity.

use std::collections::HashMap;

/// Version string reported by `version()`.
pub fn version_string() -> String {
    format!(
        "PostgreSQL 16.0 (NovaDB {}) on x86_64-pc-linux-gnu, compiled by rustc, 64-bit",
        env!("CARGO_PKG_VERSION")
    )
}

/// Parameters the server reports with `ParameterStatus` (GUC_REPORT), with
/// their canonical spelling.
pub const REPORTED: &[&str] = &[
    "server_version",
    "server_encoding",
    "client_encoding",
    "application_name",
    "DateStyle",
    "IntervalStyle",
    "integer_datetimes",
    "standard_conforming_strings",
    "TimeZone",
    "is_superuser",
    "session_authorization",
];

/// Session-scoped configuration parameters.
#[derive(Debug, Clone)]
pub struct Vars {
    values: HashMap<String, String>,
    defaults: HashMap<String, String>,
}

impl Vars {
    pub fn new(user: &str) -> Self {
        let defaults: HashMap<String, String> = [
            ("server_version", "16.0 (NovaDB)"),
            ("server_version_num", "160000"),
            ("server_encoding", "UTF8"),
            ("client_encoding", "UTF8"),
            ("application_name", ""),
            ("datestyle", "ISO, MDY"),
            ("intervalstyle", "postgres"),
            ("integer_datetimes", "on"),
            ("standard_conforming_strings", "on"),
            ("timezone", "UTC"),
            ("is_superuser", "on"),
            ("session_authorization", user),
            ("transaction_isolation", "read committed"),
            ("default_transaction_isolation", "read committed"),
            ("transaction_read_only", "off"),
            ("default_transaction_read_only", "off"),
            ("search_path", "\"$user\", public"),
            ("default_table_access_method", "heap"),
            ("max_identifier_length", "63"),
            ("lc_collate", "en_US.UTF-8"),
            ("lc_ctype", "en_US.UTF-8"),
            ("lc_messages", "en_US.UTF-8"),
            ("lc_monetary", "en_US.UTF-8"),
            ("lc_numeric", "en_US.UTF-8"),
            ("lc_time", "en_US.UTF-8"),
            ("extra_float_digits", "1"),
            ("bytea_output", "hex"),
            ("standard_conforming_strings", "on"),
            ("statement_timeout", "0"),
            ("lock_timeout", "0"),
            ("idle_in_transaction_session_timeout", "0"),
            ("synchronous_commit", "on"),
            ("client_min_messages", "notice"),
            ("row_security", "on"),
            ("max_connections", "100"),
            ("block_size", "8192"),
            ("ssl", "off"),
            ("default_with_oids", "off"),
            ("escape_string_warning", "on"),
            ("xmloption", "content"),
            ("role", "none"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        Self {
            values: defaults.clone(),
            defaults,
        }
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.values
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    /// Sets a parameter. Returns the canonical name and value when it is a
    /// reported parameter (a `ParameterStatus` message must follow).
    pub fn set(&mut self, name: &str, value: &str) -> Option<(String, String)> {
        let key = name.to_ascii_lowercase();
        self.values.insert(key.clone(), value.to_owned());
        reported_name(&key).map(|canonical| (canonical.to_owned(), value.to_owned()))
    }

    /// Restores a parameter to its default. `None` resets everything.
    pub fn reset(&mut self, name: Option<&str>) -> Vec<(String, String)> {
        let mut changed = Vec::new();
        match name {
            Some(name) => {
                let key = name.to_ascii_lowercase();
                match self.defaults.get(&key) {
                    Some(default) => {
                        self.values.insert(key.clone(), default.clone());
                    }
                    None => {
                        self.values.remove(&key);
                    }
                }
                if let (Some(canonical), Some(value)) = (reported_name(&key), self.get(&key)) {
                    changed.push((canonical.to_owned(), value.to_owned()));
                }
            }
            None => {
                self.values = self.defaults.clone();
                for canonical in REPORTED {
                    if let Some(value) = self.get(canonical) {
                        changed.push(((*canonical).to_owned(), value.to_owned()));
                    }
                }
            }
        }
        changed
    }

    /// All parameters sorted by name (for `SHOW ALL`).
    pub fn all(&self) -> Vec<(String, String)> {
        let mut all: Vec<(String, String)> = self
            .values
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        all.sort();
        all
    }
}

fn reported_name(key: &str) -> Option<&'static str> {
    REPORTED
        .iter()
        .find(|name| name.eq_ignore_ascii_case(key))
        .copied()
}

/// Validates a value for parameters that must stay compatible with NovaDB.
pub fn validate(name: &str, value: &str) -> Result<String, String> {
    match name.to_ascii_lowercase().as_str() {
        "client_encoding" => {
            let normalized = value.to_ascii_uppercase().replace('-', "");
            if matches!(normalized.as_str(), "UTF8" | "UNICODE") {
                Ok("UTF8".into())
            } else {
                Err(format!(
                    "invalid value for parameter \"client_encoding\": \"{value}\" (NovaDB only supports UTF8)"
                ))
            }
        }
        "standard_conforming_strings" if !value.eq_ignore_ascii_case("on") => {
            Err("standard_conforming_strings = off is not supported by NovaDB".into())
        }
        _ => Ok(value.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_reset_and_reported_parameters() {
        let mut vars = Vars::new("app");
        assert_eq!(vars.get("TimeZone"), Some("UTC"));
        assert_eq!(
            vars.set("timezone", "Europe/Paris"),
            Some(("TimeZone".into(), "Europe/Paris".into()))
        );
        assert_eq!(vars.get("TIMEZONE"), Some("Europe/Paris"));
        assert_eq!(vars.set("my.custom", "1"), None);
        assert_eq!(vars.get("my.custom"), Some("1"));
        vars.reset(Some("timezone"));
        assert_eq!(vars.get("timezone"), Some("UTC"));
        vars.reset(None);
        assert_eq!(vars.get("my.custom"), None);
        assert!(validate("client_encoding", "latin1").is_err());
        assert_eq!(validate("client_encoding", "utf-8").unwrap(), "UTF8");
    }
}
