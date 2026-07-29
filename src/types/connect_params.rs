//! [`ConnectParams`]: the parser for ODBC connection strings
//! (`Key=Value;Key=Value`, with `{}`-quoted values), exposing the parsed
//! key/value pairs case-insensitively to each backend.

use std::borrow::Cow;
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
    /// Keyword names the backend has declared as carrying secrets. Additive to
    /// `SENSITIVE_KEYWORD_MARKERS`; see [`ConnectParams::declare_sensitive_keywords`].
    sensitive_keywords: Cow<'static, [Cow<'static, str>]>,
    /// `SQL_ATTR_LOGIN_TIMEOUT`, in seconds, if the application set one.
    ///
    /// A separate field rather than a synthetic entry in `params` on purpose:
    /// this did not come from the connection string, and `to_connection_string`
    /// echoes `params` back to the application through
    /// `SQLDriverConnect`'s *OutConnectionString*. A key invented here would
    /// appear in that echo as though the application had written it.
    login_timeout: Option<u32>,
    /// `SQL_ATTR_CONNECTION_TIMEOUT`, in seconds, if the application set one.
    /// Kept out of `params` for the same reason as [`Self::login_timeout`].
    connection_timeout: Option<u32>,
}

/// Substrings that mark a connection-string keyword as carrying a credential.
///
/// Matched as substrings, not whole names, because the keyword set is
/// backend-defined: core sees `sslkeypassword`, `clientsecret` and
/// `accesstoken` as readily as `pwd`, and a driver is free to invent more.
/// Over-matching is harmless — redacting `authentication=kerberos` costs a line
/// of diagnostics — whereas under-matching writes a credential to the log file.
///
/// This is a safety net, not the primary mechanism, and it cannot be complete
/// for exactly the reason above. A backend names its own secret keywords
/// through [`Backend::sensitive_connect_keywords`](crate::backend::Backend::sensitive_connect_keywords),
/// which the generic FFI entry points feed to
/// [`ConnectParams::declare_sensitive_keywords`]. These markers stay in force
/// underneath that, so a driver that declares nothing is still covered for the
/// common shapes.
const SENSITIVE_KEYWORD_MARKERS: &[&str] = &[
    "pass", // password, passwd, pass, sslkeypassword
    "pwd",
    "secret", // secret, clientsecret, client_secret
    "token",  // token, accesstoken, auth_token
    "credential",
    "apikey",
    "api_key",
    "auth", // auth, authtoken, authorization
    "privatekey",
    "private_key",
    "signature",
];

/// Whether a (already lowercased) keyword's value must be redacted.
fn is_sensitive_keyword(key: &str) -> bool {
    SENSITIVE_KEYWORD_MARKERS
        .iter()
        .any(|marker| key.contains(marker))
}

impl std::fmt::Debug for ConnectParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut map = f.debug_map();
        for (key, value) in &self.params {
            // Keys are lowercased by `parse` and `insert`, so a plain
            // `contains` is enough; the explicit comparisons keep the two
            // spec-named keywords obvious.
            let displayed = if key.eq_ignore_ascii_case(PASSWORD)
                || key.eq_ignore_ascii_case(PWD)
                || is_sensitive_keyword(&key.to_lowercase())
                || self
                    .sensitive_keywords
                    .iter()
                    .any(|declared| key.eq_ignore_ascii_case(declared))
            {
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

        Ok(Self {
            params,
            sensitive_keywords: Cow::Borrowed(&[]),
            // Not in the connection string by definition; core fills these in
            // from the connection attributes just before calling
            // `Backend::connect`.
            login_timeout: None,
            connection_timeout: None,
        })
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

    /// The `SQL_ATTR_LOGIN_TIMEOUT` the application set, in seconds.
    ///
    /// `None` means the application set nothing and the backend should use its
    /// own default; the spec says "the default is driver-dependent". `Some(0)`
    /// is **not** the same thing and must not be treated as unset — the spec is
    /// explicit that "if *ValuePtr* is 0, the timeout is disabled and a
    /// connection attempt will wait indefinitely".
    ///
    /// This is the number of seconds "to wait for a login request to complete
    /// before returning to the application", so it bounds
    /// [`Backend::connect`](crate::backend::Backend::connect) itself. Core
    /// cannot enforce it — `connect` is synchronous and there is no cancel
    /// token before a connection exists — so a backend that wants a login
    /// timeout has to pass this to its own client library.
    ///
    /// A backend that clamps this to a maximum it supports should report
    /// `01S02`, which the spec names for exactly that: "if the specified
    /// timeout exceeds the maximum login timeout in the data source, the driver
    /// substitutes that value and returns SQLSTATE 01S02".
    pub fn login_timeout(&self) -> Option<u32> {
        self.login_timeout
    }

    /// The `SQL_ATTR_CONNECTION_TIMEOUT` the application set, in seconds.
    ///
    /// `None` means unset; `Some(0)` means "no timeout", which the spec states
    /// directly: "if *ValuePtr* is equal to 0 (the default), there is no
    /// timeout."
    ///
    /// Unlike [`Self::login_timeout`] this bounds "any request on the
    /// connection", not just the login, and the spec tells a driver what to
    /// report when it expires: "the driver should return SQLSTATE HYT00
    /// (Timeout expired) anytime that it is possible to time out in a situation
    /// not associated with query execution or login." Core does not enforce it;
    /// a backend that honours it does so in its own client library.
    ///
    /// The spec lists this attribute as settable either side of a connection,
    /// so a value set *after* connecting never passes through here — this
    /// carries only what was set before, which is what `Backend::connect` can
    /// act on.
    pub fn connection_timeout(&self) -> Option<u32> {
        self.connection_timeout
    }

    /// Record the connection attributes that are not connection-string
    /// keywords, just before [`Backend::connect`](crate::backend::Backend::connect).
    ///
    /// Called by the three connect entry points; not public, because these
    /// values come from `SQLSetConnectAttr` rather than from anything a driver
    /// parses.
    pub(crate) fn set_timeouts(&mut self, login: Option<u32>, connection: Option<u32>) {
        self.login_timeout = login;
        self.connection_timeout = connection;
    }

    /// Insert a key-value pair. The key is stored in lowercase for case-insensitive lookup.
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.params.insert(key.into().to_lowercase(), value.into());
    }

    /// Declare backend-specific keywords whose values must be redacted from
    /// `Debug` output.
    ///
    /// The backend owns its connection-string vocabulary, so it is the only
    /// party that can name a secret core would never guess — an `OAuthAssertion`
    /// or a `WalletLocation` looks like any other keyword from here. Core's
    /// built-in substring heuristic stays in force underneath: this
    /// list is additive, never a replacement, so declaring nothing is safe and
    /// declaring the wrong thing cannot un-redact a password.
    ///
    /// Matched case-insensitively against the whole keyword name, unlike the
    /// built-in markers, which match as substrings. A backend naming its own
    /// keywords knows them exactly.
    ///
    /// The generic FFI entry points call this for every `ConnectParams` they
    /// build, using [`Backend::sensitive_connect_keywords`](crate::backend::Backend::sensitive_connect_keywords),
    /// so a driver only has to declare the list.
    pub fn declare_sensitive_keywords(&mut self, keywords: Cow<'static, [Cow<'static, str>]>) {
        self.sensitive_keywords = keywords;
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
        Self {
            params,
            sensitive_keywords: Cow::Borrowed(&[]),
            login_timeout: None,
            connection_timeout: None,
        }
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
    fn debug_redacts_backend_declared_keywords() {
        // A backend owns its own keyword vocabulary, so it can name secrets
        // core's heuristic would never guess.
        let mut params = ConnectParams::parse("Host=localhost;Wallet=abc123").unwrap();
        params.declare_sensitive_keywords(Cow::Borrowed(&[Cow::Borrowed("wallet")]));
        let debug_str = format!("{params:?}");
        assert!(
            !debug_str.contains("abc123"),
            "backend-declared keyword must be redacted, got: {debug_str}"
        );
        assert!(
            debug_str.contains("localhost"),
            "unrelated values must stay visible, got: {debug_str}"
        );
    }

    #[test]
    fn declared_keywords_are_matched_case_insensitively() {
        let mut params = ConnectParams::parse("KeyStorePin=1234").unwrap();
        params.declare_sensitive_keywords(Cow::Borrowed(&[Cow::Borrowed("KEYSTOREPIN")]));
        assert!(!format!("{params:?}").contains("1234"));
    }

    #[test]
    fn declaring_keywords_does_not_disable_the_built_in_markers() {
        // The backend's list adds to core's safeguard, it does not replace it.
        let mut params = ConnectParams::parse("Wallet=abc123;Password=s3cr3t").unwrap();
        params.declare_sensitive_keywords(Cow::Borrowed(&[Cow::Borrowed("wallet")]));
        let debug_str = format!("{params:?}");
        assert!(!debug_str.contains("abc123"));
        assert!(!debug_str.contains("s3cr3t"));
    }

    #[test]
    fn debug_redacts_credential_keywords_beyond_password_and_pwd() {
        // The keyword set is backend-defined, so core sees names it has never
        // heard of. These are the shapes a credential realistically takes.
        for keyword in [
            "Token",
            "AccessToken",
            "ApiKey",
            "Api_Key",
            "Secret",
            "ClientSecret",
            "Passwd",
            "Pass",
            "SslKeyPassword",
            "Credential",
            "PrivateKey",
            "AuthToken",
        ] {
            let params = ConnectParams::parse(&format!("Host=localhost;{keyword}=s3cr3t")).unwrap();
            let debug_str = format!("{params:?}");
            assert!(
                !debug_str.contains("s3cr3t"),
                "{keyword} value must be redacted, got: {debug_str}"
            );
            assert!(
                debug_str.contains("localhost"),
                "{keyword} redaction must not hide unrelated values, got: {debug_str}"
            );
        }
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
