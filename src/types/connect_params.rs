//! [`ConnectParams`]: the parser for ODBC connection strings
//! (`Key=Value;Key=Value`, with `{}`-quoted values), exposing the parsed
//! key/value pairs case-insensitively to each backend.

use std::collections::HashMap;

use crate::errors::OdbcError;

// ---------------------------------------------------------------------------
// ODBC spec connection string keyword constants
// ---------------------------------------------------------------------------

pub const USER: &str = "user";
pub const UID: &str = "uid";
pub const PASSWORD: &str = "password";
pub const PWD: &str = "pwd";
pub const DSN: &str = "dsn";
pub const DRIVER: &str = "driver";
pub const FILEDSN: &str = "filedsn";
pub const SAVEFILE: &str = "savefile";

/// A parsed ODBC connection string: a case-insensitive map of keyword to value.
///
/// `Debug` is implemented by hand to redact sensitive values (e.g. passwords).
#[derive(Clone)]
pub struct ConnectParams {
    params: HashMap<String, String>,
}

impl std::fmt::Debug for ConnectParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut map = f.debug_map();
        for (key, value) in &self.params {
            let displayed = if key.eq_ignore_ascii_case(PASSWORD) || key.eq_ignore_ascii_case(PWD) {
                "*****"
            } else {
                value.as_str()
            };
            map.entry(key, &displayed);
        }
        map.finish()
    }
}

impl ConnectParams {
    /// Parse an ODBC connection string into key-value pairs.
    ///
    /// Keys are stored in lowercase for case-insensitive lookup.
    /// Values wrapped in `{braces}` are taken literally (including `;` and `=`)
    /// up to the closing `}`, then the braces are stripped.
    /// When a keyword appears more than once, the first occurrence wins.
    pub fn parse(connection_string: &str) -> Result<Self, OdbcError> {
        let mut params: HashMap<String, String> = HashMap::new();
        let mut chars = connection_string.chars().peekable();

        loop {
            // Skip separators and leading whitespace between segments.
            while matches!(chars.peek(), Some(';' | ' ' | '\t' | '\r' | '\n')) {
                chars.next();
            }
            if chars.peek().is_none() {
                break;
            }

            // Read the key up to '=' or ';'.
            let mut key = String::new();
            let mut saw_eq = false;
            while let Some(&c) = chars.peek() {
                match c {
                    '=' => {
                        chars.next();
                        saw_eq = true;
                        break;
                    }
                    ';' => break, // segment with no '=' — skip below
                    _ => {
                        key.push(c);
                        chars.next();
                    }
                }
            }
            if !saw_eq {
                // Malformed segment (no '='): discard up to the next ';'.
                while matches!(chars.peek(), Some(&c) if c != ';') {
                    chars.next();
                }
                continue;
            }

            // Skip whitespace before the value.
            while matches!(chars.peek(), Some(' ' | '\t')) {
                chars.next();
            }

            // A value wrapped in {braces} is literal (including ';' and '=')
            // until the closing '}'. Otherwise it runs to the next ';'.
            let mut value = String::new();
            let braced = chars.peek() == Some(&'{');
            if braced {
                chars.next(); // consume '{'
                for c in chars.by_ref() {
                    if c == '}' {
                        break;
                    }
                    value.push(c);
                }
                // Discard any trailing characters up to the next ';'.
                while matches!(chars.peek(), Some(&c) if c != ';') {
                    chars.next();
                }
            } else {
                while let Some(&c) = chars.peek() {
                    if c == ';' {
                        break;
                    }
                    value.push(c);
                    chars.next();
                }
            }

            let key = key.trim().to_lowercase();
            if !key.is_empty() {
                // Braces preserve the value exactly; unbraced values are trimmed.
                let value = if braced {
                    value
                } else {
                    value.trim().to_string()
                };
                // ODBC spec: first occurrence of a keyword wins.
                params.entry(key).or_insert_with(|| value);
            }
        }

        Ok(Self { params })
    }

    /// Case-insensitive key lookup.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.params.get(&key.to_lowercase()).map(|s| s.as_str())
    }

    /// Returns the `user` parameter or an error if missing.
    ///
    /// Accepts both `User=` (our convention) and the ODBC-standard `UID=` alias.
    pub fn user(&self) -> Result<&str, OdbcError> {
        self.get(USER)
            .or_else(|| self.get(UID))
            .ok_or_else(|| OdbcError::MissingParameter { name: USER.into() })
    }

    /// Returns the `password` parameter if present.
    ///
    /// Accepts both `Password=` (our convention) and the ODBC-standard `PWD=` alias.
    pub fn password(&self) -> Option<&str> {
        self.get(PASSWORD).or_else(|| self.get(PWD))
    }

    /// Returns the `DSN` parameter if present (ODBC spec: data source name).
    pub fn dsn(&self) -> Option<&str> {
        self.get(DSN)
    }

    /// Returns the `DRIVER` parameter if present (ODBC spec: driver name or path).
    pub fn driver(&self) -> Option<&str> {
        self.get(DRIVER)
    }

    /// Returns the `FILEDSN` parameter if present (ODBC spec: file-based DSN path).
    pub fn filedsn(&self) -> Option<&str> {
        self.get(FILEDSN)
    }

    /// Returns the `SAVEFILE` parameter if present (ODBC spec: save connection string to file DSN).
    pub fn savefile(&self) -> Option<&str> {
        self.get(SAVEFILE)
    }

    /// Insert a key-value pair. The key is stored in lowercase for case-insensitive lookup.
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.params.insert(key.into().to_lowercase(), value.into());
    }

    /// Merge another `ConnectParams` into self. Existing keys are NOT overwritten.
    pub fn merge(&mut self, other: &ConnectParams) {
        for (key, value) in &other.params {
            self.params
                .entry(key.clone())
                .or_insert_with(|| value.clone());
        }
    }

    /// Reconstruct an ODBC connection string from the stored key-value pairs.
    /// Values containing `;` or `=` are wrapped in `{braces}`.
    pub fn to_connection_string(&self) -> String {
        let mut parts = Vec::new();
        for (key, value) in &self.params {
            if value.contains(';') || value.contains('=') {
                parts.push(format!("{key}={{{value}}}"));
            } else {
                parts.push(format!("{key}={value}"));
            }
        }
        parts.join(";")
    }

    /// Returns an iterator over all stored keys (lowercased).
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.params.keys().map(|s| s.as_str())
    }
}

impl FromIterator<(String, String)> for ConnectParams {
    fn from_iter<T: IntoIterator<Item = (String, String)>>(iter: T) -> Self {
        let mut params = HashMap::new();
        for (k, v) in iter {
            params.insert(k.to_lowercase(), v);
        }
        Self { params }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_connection_string_basic() {
        let params = ConnectParams::parse("Driver={MyDriver};Database=/tmp/test.db").unwrap();
        assert_eq!(params.get(DRIVER), Some("MyDriver"));
        assert_eq!(params.get("database"), Some("/tmp/test.db"));
    }

    #[test]
    fn parse_connection_string_case_insensitive() {
        let params = ConnectParams::parse("HOST=localhost;port=8080").unwrap();
        assert_eq!(params.get("host"), Some("localhost"));
        assert_eq!(params.get("PORT"), Some("8080"));
    }

    #[test]
    fn parse_connection_string_empty() {
        let params = ConnectParams::parse("").unwrap();
        assert_eq!(params.get("anything"), None);
    }

    #[test]
    fn connect_params_odbc_spec_methods() {
        let params = ConnectParams::parse(
            "DSN=mydsn;Driver={My Driver};FileDSN=/tmp/x.dsn;SaveFile=/tmp/y.dsn;User=u;PWD=p",
        )
        .unwrap();
        assert_eq!(params.dsn(), Some("mydsn"));
        assert_eq!(params.driver(), Some("My Driver"));
        assert_eq!(params.filedsn(), Some("/tmp/x.dsn"));
        assert_eq!(params.savefile(), Some("/tmp/y.dsn"));
        assert_eq!(params.user().unwrap(), "u");
        assert_eq!(params.password(), Some("p"));
    }

    #[test]
    fn user_accepts_uid_alias() {
        let params = ConnectParams::parse("UID=alice;PWD=secret").unwrap();
        assert_eq!(params.user().unwrap(), "alice");
        assert_eq!(params.password(), Some("secret"));
    }

    #[test]
    fn user_missing_returns_error() {
        let params = ConnectParams::parse("").unwrap();
        assert!(params.user().is_err());
    }

    #[test]
    fn debug_redacts_password_key() {
        let params =
            ConnectParams::parse("Host=localhost;Port=8080;User=admin;Password=s3cr3t").unwrap();
        let debug_str = format!("{params:?}");
        assert!(
            !debug_str.contains("s3cr3t"),
            "password value must be redacted in Debug output, got: {debug_str}"
        );
        assert!(
            debug_str.contains("*****"),
            "expected ***** in Debug output, got: {debug_str}"
        );
        assert!(
            debug_str.contains("localhost"),
            "host value should be visible, got: {debug_str}"
        );
    }

    #[test]
    fn debug_redacts_pwd_key() {
        let params = ConnectParams::parse(&format!("{PWD}=hunter2;{USER}=alice")).unwrap();
        let debug_str = format!("{params:?}");
        assert!(!debug_str.contains("hunter2"));
        assert!(debug_str.contains("*****"));
    }

    #[test]
    fn debug_shows_non_sensitive_values() {
        let params = ConnectParams::parse("Host=db.example.com;Port=5432").unwrap();
        let debug_str = format!("{params:?}");
        assert!(debug_str.contains("db.example.com"));
        assert!(debug_str.contains("5432"));
    }

    #[test]
    fn merge_does_not_overwrite_existing() {
        let mut base = ConnectParams::parse("Host=localhost;Port=8080").unwrap();
        let other = ConnectParams::parse("Host=remote;User=admin").unwrap();
        base.merge(&other);
        assert_eq!(base.get("host"), Some("localhost")); // not overwritten
        assert_eq!(base.get("user"), Some("admin")); // added
    }

    #[test]
    fn to_connection_string_roundtrip() {
        let params = ConnectParams::parse("Host=localhost;Port=8080").unwrap();
        let s = params.to_connection_string();
        assert!(s.contains("host=localhost"));
        assert!(s.contains("port=8080"));
    }

    #[test]
    fn to_connection_string_braces_special_chars() {
        let mut params = ConnectParams::parse("").unwrap();
        params.insert("driver", "My Driver;v2");
        let s = params.to_connection_string();
        assert!(s.contains("driver={My Driver;v2}"));
    }

    #[test]
    fn parse_braced_value_with_semicolon_roundtrips() {
        let mut params = ConnectParams::parse("").unwrap();
        params.insert("driver", "My Driver;v2");
        let s = params.to_connection_string(); // driver={My Driver;v2}
        let reparsed = ConnectParams::parse(&s).unwrap();
        assert_eq!(reparsed.get("driver"), Some("My Driver;v2"));
    }

    #[test]
    fn parse_braced_value_preserves_inner_semicolons() {
        let params = ConnectParams::parse("Key={a;b;c};Next=v").unwrap();
        assert_eq!(params.get("key"), Some("a;b;c"));
        assert_eq!(params.get("next"), Some("v"));
    }

    #[test]
    fn parse_duplicate_keys_first_wins() {
        // ODBC spec: the first occurrence of a keyword wins.
        let params = ConnectParams::parse("Host=first;Host=second").unwrap();
        assert_eq!(params.get("host"), Some("first"));
    }

    #[test]
    fn parse_braced_value_preserves_inner_equals() {
        let params = ConnectParams::parse("Key={a=b;c=d};Next=v").unwrap();
        assert_eq!(params.get("key"), Some("a=b;c=d"));
        assert_eq!(params.get("next"), Some("v"));
    }

    #[test]
    fn parse_unterminated_brace_consumes_remainder() {
        // An unterminated '{' has no closing '}', so the rest of the string
        // (including any later "key=value") becomes part of this value.
        let params = ConnectParams::parse("Key={abc;Next=v").unwrap();
        assert_eq!(params.get("key"), Some("abc;Next=v"));
        assert_eq!(params.get("next"), None);
    }
}

#[cfg(test)]
mod proptest_connect_params {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// ConnectParams::parse never panics on arbitrary input.
        #[test]
        fn parse_never_panics(s in ".*") {
            let _ = ConnectParams::parse(&s);
        }

        /// Valid K=V pairs are always accessible after parsing.
        #[test]
        fn valid_kv_always_accessible(
            key in "[a-zA-Z][a-zA-Z0-9]{0,15}",
            value in "[a-zA-Z0-9./_-]{0,50}",
        ) {
            let conn_str = format!("{key}={value}");
            let params = ConnectParams::parse(&conn_str).expect("valid K=V should not return Err");
            assert_eq!(
                params.get(&key.to_lowercase()),
                Some(value.as_str()),
                "value for key {key} should be {value}"
            );
        }

        /// Segments without '=' are silently ignored (not an error).
        #[test]
        fn no_equals_segment_is_silently_ignored(s in "[a-zA-Z0-9]{1,20}") {
            let result = ConnectParams::parse(&s);
            assert!(result.is_ok(), "expected Ok for no-equals input, got {result:?}");
        }
    }
}
