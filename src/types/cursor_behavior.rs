//! `CursorBehavior`: what `SQLEndTran` does to open cursors on a connection.

use crate::types::{SQL_CB_CLOSE, SQL_CB_DELETE, SQL_CB_PRESERVE};

/// What `SQLEndTran` does to the open cursors on a connection.
///
/// This is the driver-side model of the `SQL_CB_*` values reported by the
/// `SQL_CURSOR_COMMIT_BEHAVIOR` and `SQL_CURSOR_ROLLBACK_BEHAVIOR` info types.
/// `odbc-sys` models neither the values nor an equivalent enum.
///
/// The variants correspond directly to the footnotes of the `SQLEndTran`
/// statement transition table in Appendix B of the ODBC specification.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/statement-transitions>
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorBehavior {
    /// `SQL_CB_DELETE`: cursors closed, access plans discarded. Prepared
    /// statements return to the allocated (unprepared) state S1.
    Delete,
    /// `SQL_CB_CLOSE`: cursors closed, access plans retained. Prepared
    /// statements return to their prepared state (S2/S3).
    Close,
    /// `SQL_CB_PRESERVE`: cursors and access plans both untouched.
    Preserve,
}

impl CursorBehavior {
    /// The `SQL_CB_*` value an application receives from `SQLGetInfoW`.
    pub fn as_u16(self) -> u16 {
        match self {
            CursorBehavior::Delete => SQL_CB_DELETE,
            CursorBehavior::Close => SQL_CB_CLOSE,
            CursorBehavior::Preserve => SQL_CB_PRESERVE,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{SQL_CB_CLOSE, SQL_CB_DELETE, SQL_CB_PRESERVE};

    #[test]
    fn as_u16_matches_the_sql_cb_constants() {
        assert_eq!(CursorBehavior::Delete.as_u16(), SQL_CB_DELETE);
        assert_eq!(CursorBehavior::Close.as_u16(), SQL_CB_CLOSE);
        assert_eq!(CursorBehavior::Preserve.as_u16(), SQL_CB_PRESERVE);
    }

    /// Ties the constants back to the ODBC specification's own source rather
    /// than to another expression in this crate. It uses raw
    /// literals on purpose, for the same reason
    /// `info_type_constants_match_sqlext_h` in `constants.rs` does: the
    /// literal *is* the check.
    #[test]
    fn sql_cb_constants_match_sql_h() {
        assert_eq!(SQL_CB_DELETE, 0, "SQL_CB_DELETE (sql.h)");
        assert_eq!(SQL_CB_CLOSE, 1, "SQL_CB_CLOSE (sql.h)");
        assert_eq!(SQL_CB_PRESERVE, 2, "SQL_CB_PRESERVE (sql.h)");
    }
}
