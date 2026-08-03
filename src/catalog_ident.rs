//! `SQL_ATTR_METADATA_ID` argument normalisation, and the `SQLTables`
//! `TableType` value-list parser.
//!
//! When `SQL_ATTR_METADATA_ID` is `SQL_TRUE` the spec reclassifies most
//! catalog string arguments from pattern values to identifiers. Core resolves
//! that here rather than passing a flag down to the backend: it already knows
//! the data source's identifier case, quote characters and pattern-escape
//! character, and normalising to an ordinary pattern that matches exactly one
//! name means a backend needs no code for the feature at all.
//!
//! # The spec contradicts itself about quoting, and core follows its first half
//!
//! *Identifier Arguments* states the rule core implements: "If a string in an
//! identifier argument is quoted, the driver removes leading and trailing blanks
//! and treats literally the string within the quotation marks. If the string is
//! not quoted, the driver removes trailing blanks and folds the string to
//! uppercase."
//!
//! Two paragraphs later the same page says the opposite: identifiers "must not
//! be quoted when passed as catalog function arguments, because quote characters
//! passed to catalog functions are interpreted literally", with a worked example
//! in which `SQLTables` given `"\"Accounts Payable\""` looks for a table whose
//! name *includes* the quotation marks — "which is probably not what was
//! intended".
//!
//! The two implementing drivers are on the side of the second paragraph, or
//! silent: psqlODBC does not implement `SQL_ATTR_METADATA_ID` at all, and MySQL
//! Connector/ODBC implements it as a case-insensitive comparison with no
//! delimiter handling (see [`strip_delimiters`] for the citations). So core
//! recognising a delimiter pair is a deviation from both of them, taken from the
//! page's first paragraph.
//!
//! It is left as it stands deliberately, not by default: [`normalise_identifier`]
//! is the only reason a `SQL_IC_UPPER` data source can be asked about a
//! mixed-case name at all, and the fourth paragraph of the same page depends on
//! quoting being significant ("Quoted identifiers are used to distinguish a true
//! column name from a pseudo-column of the same name"). Reversing it would make
//! `SQL_ATTR_METADATA_ID` unable to express a case-sensitive name, which is what
//! an application sets it for.

use crate::types::{SQL_IC_LOWER, SQL_IC_UPPER};

/// Turn an identifier-valued catalog argument into a literal pattern.
///
/// Delimiters are stripped first, and a delimited identifier is **not**
/// folded — delimiting is how an application says its case is significant.
///
/// `quotes` comes from `EscapeDialect::identifier_quotes` and `escape` from
/// `Backend::search_pattern_escape`, so every input is a fact the backend
/// already declares.
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
///
/// # A doubled delimiter inside the identifier is not collapsed
///
/// `"a""b"` yields `a""b`, not `a"b`, so a table actually named `a"b` is not
/// found by that spelling. This was investigated rather than overlooked, and
/// the finding is that **nothing corroborates a doubling convention here**:
///
/// - *Quoted Identifiers* defines a quoted identifier as SQL-92's delimited
///   identifier but says nothing about escaping a delimiter inside one, and
///   *Identifier Arguments* — the page that governs `SQL_ATTR_METADATA_ID` —
///   does not mention it either.
/// - psqlODBC does not implement `SQL_ATTR_METADATA_ID` at all: it is absent
///   from `options.c`, whose `default` arm answers "Unknown statement option",
///   so the attribute cannot even be set.
/// - MySQL Connector/ODBC threads the flag through to
///   `add_name_condition_oa_id`/`add_name_condition_pv_id` (`driver/catalog.cc`)
///   and uses it for one thing only: comparing with `=` instead of `= BINARY`,
///   which is how it makes the match case-insensitive. It strips no delimiters,
///   recognises none, and carries a `/* Need also code to remove trailing
///   blanks */` TODO for another clause of the same paragraph.
///
/// So neither driver reaches the question, because neither strips delimiters in
/// the first place. That is a larger divergence than the doubling itself, and
/// it is recorded in the module docs above rather than settled here.
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
