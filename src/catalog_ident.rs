//! `SQL_ATTR_METADATA_ID` argument normalisation, and the `SQLTables`
//! `TableType` value-list parser.
//!
//! When `SQL_ATTR_METADATA_ID` is `SQL_TRUE` the spec reclassifies most
//! catalog string arguments from pattern values to identifiers. Core resolves
//! that here rather than passing a flag down to the backend: it already knows
//! the data source's identifier case, quote characters and pattern-escape
//! character, and normalising to an ordinary pattern that matches exactly one
//! name means a backend needs no code for the feature at all.

use crate::types::{SQL_IC_LOWER, SQL_IC_UPPER};

/// Turn an identifier-valued catalog argument into a literal pattern.
///
/// Delimiters are stripped first, and a delimited identifier is **not**
/// folded — delimiting is how an application says its case is significant.
///
/// `quotes` comes from `EscapeDialect::identifier_quotes` and `escape` from
/// `Backend::search_pattern_escape`, so every input is a fact the backend
/// already declares.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "called by the catalog FFI functions once METADATA_ID is wired up"
    )
)]
pub(crate) fn normalise_identifier(
    value: &str,
    identifier_case: u16,
    quotes: &[(char, char)],
    escape: &str,
) -> String {
    let (unwrapped, was_delimited) = strip_delimiters(value, quotes);

    let folded = if was_delimited {
        unwrapped
    } else {
        match identifier_case {
            c if c == SQL_IC_UPPER => unwrapped.to_uppercase(),
            c if c == SQL_IC_LOWER => unwrapped.to_lowercase(),
            // SQL_IC_SENSITIVE and SQL_IC_MIXED both store the identifier as
            // written, so there is nothing to fold.
            _ => unwrapped,
        }
    };

    escape_pattern_metacharacters(&folded, escape)
}

/// Remove a matching open/close delimiter pair, reporting whether one was
/// found.
///
/// Requires at least two characters: a lone `"` is not a pair, and stripping
/// first-and-last unconditionally would turn it into an empty string.
fn strip_delimiters(value: &str, quotes: &[(char, char)]) -> (String, bool) {
    let mut chars = value.chars();
    let (Some(first), Some(last)) = (chars.next(), chars.next_back()) else {
        // Zero or one character: no room for a pair.
        return (value.to_string(), false);
    };
    if quotes
        .iter()
        .any(|&(open, close)| open == first && close == last)
    {
        (chars.as_str().to_string(), true)
    } else {
        (value.to_string(), false)
    }
}

/// Escape `%` and `_` so the value matches literally.
///
/// The escape character is escaped too, and in the same pass — doing it as a
/// separate earlier or later step would either miss the escapes this pass
/// inserts or double them.
fn escape_pattern_metacharacters(value: &str, escape: &str) -> String {
    // An empty escape string means the data source has no escape character
    // (`Backend::search_pattern_escape` may legitimately return one), so
    // there is nothing to escape with.
    let Some(esc) = escape.chars().next() else {
        return value.to_string();
    };
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch == esc || ch == '%' || ch == '_' {
            out.push(esc);
        }
        out.push(ch);
    }
    out
}

/// Split `SQLTables`' `TableType` value list.
///
/// Spec: "a list of comma-separated values for the types of interest; each
/// value can be enclosed in single quotation marks (') or unquoted".
///
/// `METADATA_ID` never applies here — the spec is explicit that `TableType`
/// "is a value list argument, regardless of the setting of
/// SQL_ATTR_METADATA_ID".
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "called by SQLTablesW once METADATA_ID is wired up"
    )
)]
pub(crate) fn parse_table_type_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|part| part.trim().trim_matches('\'').trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{SQL_IC_LOWER, SQL_IC_MIXED, SQL_IC_SENSITIVE, SQL_IC_UPPER};

    const QUOTES: &[(char, char)] = &[('"', '"')];

    #[test]
    fn an_undelimited_identifier_is_case_folded() {
        // SQL_IC_UPPER means the data source stores unquoted identifiers in
        // upper case, so that is what the application's "orders" must match.
        assert_eq!(
            normalise_identifier("orders", SQL_IC_UPPER, QUOTES, "\\"),
            "ORDERS"
        );
        assert_eq!(
            normalise_identifier("Orders", SQL_IC_LOWER, QUOTES, "\\"),
            "orders"
        );
    }

    #[test]
    fn a_delimited_identifier_is_unwrapped_but_not_folded() {
        // Delimiting is exactly how an application says "do not fold this".
        // Folding it anyway would make "MixedCase" unfindable.
        assert_eq!(
            normalise_identifier("\"MixedCase\"", SQL_IC_UPPER, QUOTES, "\\"),
            "MixedCase"
        );
    }

    #[test]
    fn case_preserving_sources_fold_nothing() {
        // SQL_IC_SENSITIVE and SQL_IC_MIXED both store the identifier as
        // written, so there is nothing to fold.
        assert_eq!(
            normalise_identifier("Orders", SQL_IC_SENSITIVE, QUOTES, "\\"),
            "Orders"
        );
        assert_eq!(
            normalise_identifier("Orders", SQL_IC_MIXED, QUOTES, "\\"),
            "Orders"
        );
    }

    #[test]
    fn pattern_metacharacters_are_escaped() {
        // Under METADATA_ID the value is an identifier, so % and _ are
        // literal. Without escaping, a table actually named "a_b" would also
        // match "axb" — the backend still matches with LIKE.
        assert_eq!(
            normalise_identifier("a_b%c", SQL_IC_SENSITIVE, QUOTES, "\\"),
            "a\\_b\\%c"
        );
    }

    #[test]
    fn the_escape_character_itself_is_escaped() {
        // Otherwise a literal backslash would escape whatever followed it.
        assert_eq!(
            normalise_identifier("a\\b", SQL_IC_SENSITIVE, QUOTES, "\\"),
            "a\\\\b"
        );
    }

    #[test]
    fn an_empty_escape_string_disables_escaping() {
        // `Backend::search_pattern_escape` may legitimately be empty when the
        // data source has no escape character; emitting a stray prefix would
        // corrupt the value.
        assert_eq!(
            normalise_identifier("a_b", SQL_IC_SENSITIVE, QUOTES, ""),
            "a_b"
        );
    }

    #[test]
    fn a_lone_quote_character_is_not_treated_as_a_delimiter() {
        // A single `"` is not an open/close pair. Stripping first and last
        // unconditionally would turn it into an empty string.
        assert_eq!(
            normalise_identifier("\"", SQL_IC_SENSITIVE, QUOTES, ""),
            "\""
        );
    }

    #[test]
    fn table_type_list_splits_and_unquotes() {
        // Spec, SQLTables TableType: "a list of comma-separated values ...
        // each value can be enclosed in single quotation marks (') or
        // unquoted, for example, 'TABLE', 'VIEW' or TABLE, VIEW."
        assert_eq!(
            parse_table_type_list("'TABLE', 'VIEW'"),
            vec!["TABLE".to_string(), "VIEW".to_string()]
        );
        assert_eq!(
            parse_table_type_list("TABLE, VIEW"),
            vec!["TABLE".to_string(), "VIEW".to_string()]
        );
        assert_eq!(parse_table_type_list(""), Vec::<String>::new());
    }
}
