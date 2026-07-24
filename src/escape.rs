//! ODBC escape-sequence translation (`{fn}`, `{d/t/ts}`, `{oj}`, `{escape}`).
//!
//! A single pure scanner shared by every backend. It rewrites escapes only in
//! real SQL text — string literals, quoted identifiers, and comments are copied
//! verbatim. Backends supply an [`EscapeDialect`] for the parts that differ.

use crate::errors::OdbcError;
use crate::types::SqlState;

/// Per-backend escape-translation rules.
pub struct EscapeDialect {
    /// Identifier-quote open→close pairs copied verbatim, e.g. `('"','"')`,
    /// `('`','`')`, `('[',']')`.
    pub identifier_quotes: &'static [(char, char)],
    /// Remap an ODBC `{fn NAME(...)}` scalar-function name to the backend's
    /// name; `None` passes the name through unchanged. Input is the raw name.
    pub remap_scalar_fn: fn(&str) -> Option<&'static str>,
    /// Render `{d <lit>}` given the raw inner literal text (e.g. `"'2020-01-01'"`).
    pub render_date: fn(&str) -> String,
    /// Render `{t <lit>}`.
    pub render_time: fn(&str) -> String,
    /// Render `{ts <lit>}`.
    pub render_timestamp: fn(&str) -> String,
}

impl EscapeDialect {
    /// Neutral ANSI dialect: `"`-quoted identifiers, no scalar remap,
    /// `DATE '…'`/`TIME '…'`/`TIMESTAMP '…'` literals. Used as the `Backend`
    /// default for backends that do not override `escape_dialect`.
    pub const fn ansi_default() -> EscapeDialect {
        EscapeDialect {
            identifier_quotes: &[('"', '"')],
            remap_scalar_fn: |_| None,
            render_date: ansi_date,
            render_time: ansi_time,
            render_timestamp: ansi_timestamp,
        }
    }

    fn ident_close(&self, open: char) -> Option<char> {
        self.identifier_quotes
            .iter()
            .find(|(o, _)| *o == open)
            .map(|(_, c)| *c)
    }
}

fn ansi_date(x: &str) -> String {
    format!("DATE {x}")
}
fn ansi_time(x: &str) -> String {
    format!("TIME {x}")
}
fn ansi_timestamp(x: &str) -> String {
    format!("TIMESTAMP {x}")
}

/// Translate ODBC escapes in `sql`. Returns the rewritten SQL, or an error for
/// an unsupported (`{call}` → HYC00) or malformed (unterminated → 42000) escape.
pub fn translate_escapes(sql: &str, dialect: &EscapeDialect) -> Result<String, OdbcError> {
    let chars: Vec<char> = sql.chars().collect();
    translate_slice(&chars, dialect)
}

fn translate_slice(chars: &[char], dialect: &EscapeDialect) -> Result<String, OdbcError> {
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            // An unterminated top-level string literal (no closing quote) is
            // copied verbatim by design — only escapes must be well-formed.
            copy_string(chars, &mut i, &mut out);
        } else if let Some(close) = dialect.ident_close(c) {
            copy_quoted_ident(chars, &mut i, &mut out, c, close);
        } else if c == '-' && chars.get(i + 1) == Some(&'-') {
            // An unterminated top-level line comment (no trailing '\n') is
            // copied verbatim by design — only escapes must be well-formed.
            copy_line_comment(chars, &mut i, &mut out);
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            // Likewise for an unterminated block comment (no closing `*/`).
            copy_block_comment(chars, &mut i, &mut out);
        } else if c == '{' {
            translate_escape(chars, &mut i, dialect, &mut out)?;
        } else {
            out.push(c);
            i += 1;
        }
    }
    Ok(out)
}

/// Copy a single-quoted string literal verbatim, including a `''` doubled quote.
fn copy_string(chars: &[char], i: &mut usize, out: &mut String) {
    out.push(chars[*i]); // opening '
    *i += 1;
    while *i < chars.len() {
        let c = chars[*i];
        out.push(c);
        *i += 1;
        if c == '\'' {
            if chars.get(*i) == Some(&'\'') {
                out.push('\''); // doubled quote — stays inside the string
                *i += 1;
            } else {
                break; // closing quote
            }
        }
    }
}

fn copy_quoted_ident(chars: &[char], i: &mut usize, out: &mut String, open: char, close: char) {
    out.push(chars[*i]); // opening quote
    *i += 1;
    while *i < chars.len() {
        let c = chars[*i];
        out.push(c);
        *i += 1;
        if c == close {
            // A doubled close-quote escapes it, but only for symmetric quote
            // styles (`"..."`, `` `...` ``). Bracket identifiers (`[...]`) have
            // no doubling — a `]` always closes them.
            if open == close && chars.get(*i) == Some(&close) {
                out.push(close);
                *i += 1;
            } else {
                break;
            }
        }
    }
}

fn copy_line_comment(chars: &[char], i: &mut usize, out: &mut String) {
    while *i < chars.len() {
        let c = chars[*i];
        out.push(c);
        *i += 1;
        if c == '\n' {
            break;
        }
    }
}

fn copy_block_comment(chars: &[char], i: &mut usize, out: &mut String) {
    out.push(chars[*i]); // '/'
    out.push(chars[*i + 1]); // '*'
    *i += 2;
    while *i < chars.len() {
        if chars[*i] == '*' && chars.get(*i + 1) == Some(&'/') {
            out.push('*');
            out.push('/');
            *i += 2;
            break;
        }
        out.push(chars[*i]);
        *i += 1;
    }
}

/// Find the index of the `}` matching the `{` at `open`, skipping strings,
/// quoted identifiers, comments and nested braces. Errors if unterminated.
fn find_matching_brace(
    chars: &[char],
    open: usize,
    dialect: &EscapeDialect,
) -> Result<usize, OdbcError> {
    let mut depth = 0usize;
    let mut i = open;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            let mut s = String::new();
            copy_string(chars, &mut i, &mut s);
            continue;
        } else if let Some(close) = dialect.ident_close(c) {
            let mut s = String::new();
            copy_quoted_ident(chars, &mut i, &mut s, c, close);
            continue;
        } else if c == '-' && chars.get(i + 1) == Some(&'-') {
            let mut s = String::new();
            copy_line_comment(chars, &mut i, &mut s);
            continue;
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            let mut s = String::new();
            copy_block_comment(chars, &mut i, &mut s);
            continue;
        } else if c == '{' {
            depth += 1;
        } else if c == '}' {
            depth -= 1;
            if depth == 0 {
                return Ok(i);
            }
        }
        i += 1;
    }
    Err(OdbcError::general(
        "unterminated ODBC escape sequence (missing '}')",
        SqlState::syntax_error_or_access_violation(),
    ))
}

/// Translate one `{...}` escape starting at `*i == '{'`. Advances `*i` past the
/// matching `}`.
fn translate_escape(
    chars: &[char],
    i: &mut usize,
    dialect: &EscapeDialect,
    out: &mut String,
) -> Result<(), OdbcError> {
    let open = *i;
    // Peek the keyword after '{' and optional whitespace.
    let mut k = open + 1;
    while k < chars.len() && chars[k].is_whitespace() {
        k += 1;
    }
    let kw_start = k;
    while k < chars.len() && (chars[k].is_ascii_alphanumeric()) {
        k += 1;
    }
    let keyword: String = chars[kw_start..k]
        .iter()
        .collect::<String>()
        .to_ascii_lowercase();
    // `{?= call ...}` — the return-value stored-procedure form.
    let is_return_call = keyword.is_empty() && chars.get(kw_start) == Some(&'?');

    let close = find_matching_brace(chars, open, dialect)?;
    // Inner text between the keyword end and the closing brace.
    let after_kw = k;

    match keyword.as_str() {
        "fn" => {
            // {fn NAME(args)} -> remap(NAME) + translated(args), braces dropped.
            let mut n = after_kw;
            while n < chars.len() && chars[n].is_whitespace() {
                n += 1;
            }
            let name_start = n;
            while n < chars.len() && (chars[n].is_ascii_alphanumeric() || chars[n] == '_') {
                n += 1;
            }
            let name: String = chars[name_start..n].iter().collect();
            if name.is_empty() {
                // malformed {fn} — copy verbatim
                out.extend(chars[open..=close].iter().copied());
            } else {
                let mapped = (dialect.remap_scalar_fn)(&name).unwrap_or(&name);
                out.push_str(mapped);
                let rest = translate_slice(&chars[n..close], dialect)?;
                out.push_str(&rest);
            }
        }
        "d" => out.push_str(&(dialect.render_date)(
            inner_trimmed(chars, after_kw, close).as_str(),
        )),
        "t" => out.push_str(&(dialect.render_time)(
            inner_trimmed(chars, after_kw, close).as_str(),
        )),
        "ts" => out.push_str(&(dialect.render_timestamp)(
            inner_trimmed(chars, after_kw, close).as_str(),
        )),
        "oj" => {
            let body = translate_slice(&chars[after_kw..close], dialect)?;
            out.push_str(body.trim());
        }
        "escape" => {
            out.push_str("ESCAPE ");
            out.push_str(inner_trimmed(chars, after_kw, close).as_str());
        }
        "call" => {
            return Err(OdbcError::general(
                "stored-procedure escape ({call ...}) is not supported by this driver",
                SqlState::optional_feature_not_implemented(),
            ));
        }
        _ if is_return_call => {
            return Err(OdbcError::general(
                "stored-procedure escape ({?= call ...}) is not supported by this driver",
                SqlState::optional_feature_not_implemented(),
            ));
        }
        _ => {
            // Unrecognized escape keyword — leave the whole {...} intact.
            out.extend(chars[open..=close].iter().copied());
        }
    }
    *i = close + 1;
    Ok(())
}

fn inner_trimmed(chars: &[char], from: usize, close: usize) -> String {
    chars[from..close]
        .iter()
        .collect::<String>()
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // A test dialect: double-quote + bracket identifiers, UCASE/LCASE remap, SQL-92-style date literals.
    fn remap(name: &str) -> Option<&'static str> {
        match name.to_ascii_uppercase().as_str() {
            "UCASE" => Some("upper"),
            "LCASE" => Some("lower"),
            _ => None,
        }
    }
    fn d(x: &str) -> String {
        format!("DATE {x}")
    }
    fn t(x: &str) -> String {
        format!("TIME {x}")
    }
    fn ts(x: &str) -> String {
        format!("TIMESTAMP {x}")
    }
    fn dialect() -> EscapeDialect {
        EscapeDialect {
            identifier_quotes: &[('"', '"'), ('[', ']')],
            remap_scalar_fn: remap,
            render_date: d,
            render_time: t,
            render_timestamp: ts,
        }
    }
    fn tr(sql: &str) -> String {
        translate_escapes(sql, &dialect()).unwrap()
    }

    #[test]
    fn plain_sql_unchanged() {
        assert_eq!(
            tr("SELECT a FROM t WHERE b = 1"),
            "SELECT a FROM t WHERE b = 1"
        );
    }
    #[test]
    fn fn_remapped() {
        assert_eq!(tr("SELECT {fn UCASE(name)}"), "SELECT upper(name)");
    }
    #[test]
    fn fn_unknown_passes_through() {
        assert_eq!(tr("{fn ABS(x)}"), "ABS(x)");
    }
    #[test]
    fn fn_nested() {
        assert_eq!(tr("{fn CONCAT({fn UCASE(a)}, b)}"), "CONCAT(upper(a), b)");
    }
    #[test]
    fn date_literal() {
        assert_eq!(tr("{d '2020-01-01'}"), "DATE '2020-01-01'");
    }
    #[test]
    fn ts_literal() {
        assert_eq!(
            tr("{ts '2020-01-01 10:00:00'}"),
            "TIMESTAMP '2020-01-01 10:00:00'"
        );
    }
    #[test]
    fn oj_strips_braces() {
        assert_eq!(
            tr("{oj a LEFT JOIN b ON a.id=b.id}"),
            "a LEFT JOIN b ON a.id=b.id"
        );
    }
    #[test]
    fn escape_clause() {
        assert_eq!(tr("LIKE 'a%' {escape '\\'}"), "LIKE 'a%' ESCAPE '\\'");
    }

    // --- the critical negative cases: escapes inside strings/comments/identifiers are NOT rewritten ---
    #[test]
    fn fn_inside_string_literal_untouched() {
        assert_eq!(tr("SELECT '{fn UCASE(x)}'"), "SELECT '{fn UCASE(x)}'");
    }
    #[test]
    fn doubled_quote_in_string() {
        assert_eq!(
            tr("SELECT 'it''s {fn UCASE(x)}'"),
            "SELECT 'it''s {fn UCASE(x)}'"
        );
    }
    #[test]
    fn fn_inside_line_comment_untouched() {
        assert_eq!(
            tr("SELECT 1 -- {fn UCASE(x)}\nFROM t"),
            "SELECT 1 -- {fn UCASE(x)}\nFROM t"
        );
    }
    #[test]
    fn fn_inside_block_comment_untouched() {
        assert_eq!(
            tr("/* {fn UCASE(x)} */ SELECT 1"),
            "/* {fn UCASE(x)} */ SELECT 1"
        );
    }
    #[test]
    fn fn_inside_quoted_identifier_untouched() {
        assert_eq!(tr("SELECT \"{fn UCASE(x)}\""), "SELECT \"{fn UCASE(x)}\"");
    }
    #[test]
    fn brace_in_string_not_a_delimiter() {
        assert_eq!(tr("SELECT '}' , {fn UCASE(a)}"), "SELECT '}' , upper(a)");
    }

    #[test]
    fn call_rejected_hyc00() {
        let e = translate_escapes("{call foo()}", &dialect()).unwrap_err();
        assert_eq!(e.sqlstate().as_str(), "HYC00");
    }
    #[test]
    fn return_call_rejected_hyc00() {
        let e = translate_escapes("{?= call foo()}", &dialect()).unwrap_err();
        assert_eq!(e.sqlstate().as_str(), "HYC00");
    }
    #[test]
    fn return_call_no_space_rejected_hyc00() {
        let e = translate_escapes("{?=call foo()}", &dialect()).unwrap_err();
        assert_eq!(e.sqlstate().as_str(), "HYC00");
    }
    #[test]
    fn empty_fn_copied_verbatim() {
        assert_eq!(tr("{fn }"), "{fn }");
    }
    #[test]
    fn unterminated_escape_errors() {
        assert!(translate_escapes("SELECT {fn UCASE(a)", &dialect()).is_err());
    }
    #[test]
    fn doubled_quote_in_identifier_then_fn_translated() {
        assert_eq!(
            tr("SELECT \"a\"\"b\", {fn UCASE(x)}"),
            "SELECT \"a\"\"b\", upper(x)"
        );
    }
    #[test]
    fn bracket_identifier_copied_verbatim_no_doubling() {
        assert_eq!(tr("SELECT [a{fn x}b]"), "SELECT [a{fn x}b]");
    }
    #[test]
    fn unrecognized_brace_left_intact() {
        assert_eq!(tr("SELECT {weird}"), "SELECT {weird}");
    }
    #[test]
    fn ansi_default_double_quotes_and_no_remap() {
        let out = translate_escapes(
            "SELECT {fn UCASE(a)}, {d '2020-01-01'}",
            &EscapeDialect::ansi_default(),
        )
        .unwrap();
        assert_eq!(out, "SELECT UCASE(a), DATE '2020-01-01'");
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        // The escape scanner must never panic on any input, however malformed
        // the escapes — a panic would cross the FFI boundary.
        #[test]
        fn translate_escapes_never_panics(s in ".*") {
            let _ = translate_escapes(&s, &EscapeDialect::ansi_default());
        }

        // Plain text with no escape braces, quotes or comments is copied through
        // unchanged.
        #[test]
        fn plain_text_is_unchanged(s in "[a-zA-Z0-9 ]*") {
            let out = translate_escapes(&s, &EscapeDialect::ansi_default()).ok();
            prop_assert_eq!(out, Some(s));
        }
    }
}
