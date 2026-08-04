//! Sorting for catalog result sets.
//!
//! Each catalog function's spec page states the order its result set must be
//! in. Core sorts rather than trusting a backend to, because core now holds
//! the rows and a backend that forgets an `ORDER BY` is silently
//! non-compliant in a way no test of its own would catch.

use crate::types::{ColumnValue, SQL_NC_LOW};
use std::cmp::Ordering;

/// Order two values of the same catalog column.
///
/// NULL placement comes from the data source's `SQL_NULL_COLLATION`, not from
/// a choice made here: `SQLGetInfo` already reports that value to the
/// application, and a sorter that disagreed with it would make the driver
/// contradict itself.
fn compare_values(a: &ColumnValue, b: &ColumnValue, null_collation: u16) -> Ordering {
    let nulls_first = null_collation == SQL_NC_LOW;
    match (a, b) {
        (ColumnValue::Null, ColumnValue::Null) => Ordering::Equal,
        (ColumnValue::Null, _) => {
            if nulls_first {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (_, ColumnValue::Null) => {
            if nulls_first {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (ColumnValue::String(x), ColumnValue::String(y)) => x.cmp(y),
        // Integer columns (KEY_SEQ, ORDINAL_POSITION, SCOPE, TYPE,
        // NON_UNIQUE) must compare numerically: as text, 10 sorts before 2,
        // which would corrupt the column order of any table with more than
        // nine columns.
        (ColumnValue::I16(x), ColumnValue::I16(y)) => x.cmp(y),
        (ColumnValue::I32(x), ColumnValue::I32(y)) => x.cmp(y),
        (ColumnValue::I64(x), ColumnValue::I64(y)) => x.cmp(y),
        // Mixed or non-comparable variants: treat as equal so the stable sort
        // preserves the backend's order rather than inventing one. A catalog
        // column holds one type across all rows, so this is unreachable in
        // practice; it exists so the comparator is total.
        _ => Ordering::Equal,
    }
}

/// Sort `rows` in place by `keys`, which are zero-based column indices, most
/// significant first.
///
/// `sort_by` is stable, which matters: rows equal on every spec key keep the
/// order the backend returned them in.
pub(crate) fn sort_rows(rows: &mut [Vec<ColumnValue>], keys: &[usize], null_collation: u16) {
    rows.sort_by(|a, b| {
        for &k in keys {
            match (a.get(k), b.get(k)) {
                (Some(x), Some(y)) => {
                    let ord = compare_values(x, y, null_collation);
                    if ord != Ordering::Equal {
                        return ord;
                    }
                }
                // A row too short for a declared key cannot be ordered
                // against; leave the pair alone rather than guess.
                _ => return Ordering::Equal,
            }
        }
        Ordering::Equal
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{SQL_NC_HIGH, SQL_NC_LOW};

    fn s(v: &str) -> ColumnValue {
        ColumnValue::String(v.into())
    }

    #[test]
    fn sorts_by_each_key_in_order() {
        // Spec, SQLTables: "ordered by TABLE_TYPE, TABLE_CAT, TABLE_SCHEM,
        // and TABLE_NAME", so TABLE_TYPE (index 3) dominates TABLE_NAME
        // (index 2), which is the whole point of a multi-key sort.
        let mut rows = vec![
            vec![s("c"), s("s"), s("z_table"), s("TABLE")],
            vec![s("c"), s("s"), s("a_table"), s("VIEW")],
            vec![s("c"), s("s"), s("m_table"), s("TABLE")],
        ];
        sort_rows(&mut rows, &[3, 0, 1, 2], SQL_NC_HIGH);
        let names: Vec<&ColumnValue> = rows.iter().map(|r| &r[2]).collect();
        assert_eq!(names, vec![&s("m_table"), &s("z_table"), &s("a_table")]);
    }

    #[test]
    fn nulls_sort_high_or_low_per_null_collation() {
        // SQL_NULL_COLLATION says where NULLs go for this data source. Core
        // must not pick: SQLGetInfo already told the application the answer,
        // and the two must not contradict each other.
        let mk = || vec![vec![s("b")], vec![ColumnValue::Null], vec![s("a")]];

        let mut high = mk();
        sort_rows(&mut high, &[0], SQL_NC_HIGH);
        assert_eq!(high[2][0], ColumnValue::Null, "NC_HIGH sorts NULLs last");

        let mut low = mk();
        sort_rows(&mut low, &[0], SQL_NC_LOW);
        assert_eq!(low[0][0], ColumnValue::Null, "NC_LOW sorts NULLs first");
    }

    #[test]
    fn numeric_keys_compare_numerically_not_lexically() {
        // KEY_SEQ and ORDINAL_POSITION are integer columns. Comparing them as
        // text puts 10 before 2, which silently corrupts the column order of
        // every table with more than nine columns.
        let mut rows = vec![
            vec![ColumnValue::I32(10)],
            vec![ColumnValue::I32(2)],
            vec![ColumnValue::I32(1)],
        ];
        sort_rows(&mut rows, &[0], SQL_NC_HIGH);
        assert_eq!(
            rows,
            vec![
                vec![ColumnValue::I32(1)],
                vec![ColumnValue::I32(2)],
                vec![ColumnValue::I32(10)],
            ]
        );
    }

    #[test]
    fn the_sort_is_stable_for_rows_equal_on_every_key() {
        // A backend may return meaningful order within a key group; the sort
        // must not scramble it.
        let mut rows = vec![
            vec![s("k"), s("first")],
            vec![s("k"), s("second")],
            vec![s("k"), s("third")],
        ];
        sort_rows(&mut rows, &[0], SQL_NC_HIGH);
        let tail: Vec<&ColumnValue> = rows.iter().map(|r| &r[1]).collect();
        assert_eq!(tail, vec![&s("first"), &s("second"), &s("third")]);
    }
}
