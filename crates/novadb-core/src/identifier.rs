use crate::{Error, Result};

/// Validates an identifier accepted by `NovaDB`'s dynamic-schema APIs.
///
/// `SQLite` itself accepts a much wider set of identifiers. `NovaDB` deliberately
/// uses a conservative portable subset for table and primary-key arguments.
pub fn validate_identifier(identifier: &str) -> Result<()> {
    let mut bytes = identifier.bytes();
    let Some(first) = bytes.next() else {
        return Err(Error::InvalidIdentifier(identifier.to_owned()));
    };
    if !(first.is_ascii_alphabetic() || first == b'_')
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(Error::InvalidIdentifier(identifier.to_owned()));
    }
    Ok(())
}

/// Validates and quotes a portable SQL identifier.
pub fn quote_identifier(identifier: &str) -> Result<String> {
    validate_identifier(identifier)?;
    Ok(quote_schema_identifier(identifier))
}

pub(crate) fn quote_schema_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

pub(crate) fn quote_sql_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_identifiers_are_strict() {
        assert_eq!(quote_identifier("notes_2").unwrap(), "\"notes_2\"");
        assert!(quote_identifier("notes; DROP TABLE notes").is_err());
        assert!(quote_identifier("2notes").is_err());
        assert!(quote_identifier("").is_err());
    }

    #[test]
    fn valid_identifiers_are_accepted() {
        for valid in [
            "a",
            "A",
            "_private",
            "notes",
            "Table1",
            "my_table_2",
            "_",
            "__double",
            "UPPERCASE",
            "mixedCase",
        ] {
            validate_identifier(valid).unwrap_or_else(|_| panic!("should accept: {valid}"));
        }
    }

    #[test]
    fn invalid_identifiers_are_rejected() {
        for invalid in [
            "",           // empty
            "1starts",    // starts with digit
            "has space",  // contains space
            "has-dash",   // contains dash
            "has.dot",    // contains dot
            "tab\tle",    // contains tab
            "new\nline",  // contains newline
            "semi;colon", // contains semicolon
            "par(en",     // contains parenthesis
            "quo\"te",    // contains quote
        ] {
            assert!(
                validate_identifier(invalid).is_err(),
                "should reject: {invalid:?}"
            );
        }
    }

    #[test]
    fn quote_schema_identifier_escapes_double_quotes() {
        assert_eq!(quote_schema_identifier("normal"), "\"normal\"");
        assert_eq!(quote_schema_identifier("has\"quote"), "\"has\"\"quote\"");
    }

    #[test]
    fn quote_sql_string_escapes_single_quotes() {
        assert_eq!(quote_sql_string("hello"), "'hello'");
        assert_eq!(quote_sql_string("it's"), "'it''s'");
        assert_eq!(quote_sql_string("a'b'c"), "'a''b''c'");
    }

    #[test]
    fn quote_identifier_rejects_then_quotes() {
        // Valid identifier gets quoted
        let quoted = quote_identifier("users").unwrap();
        assert_eq!(quoted, "\"users\"");

        // Invalid identifier is rejected before quoting
        assert!(quote_identifier("drop;table").is_err());
    }
}
