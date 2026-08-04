//! `QueryTimeout`: who enforces `SQL_ATTR_QUERY_TIMEOUT` once a backend
//! accepts it.

/// A backend's answer to [`Backend::set_query_timeout`], naming which side of
/// the boundary will actually stop an over-running statement.
///
/// The distinction exists because core cannot enforce a deadline on its own.
/// [`Backend::execute`] and its neighbours are **synchronous**: they block the
/// calling thread, and nothing core can do will make one of them return early.
/// The only lever core has is [`Backend::cancel`], whose default does nothing
/// at all, so a backend has to say whether that lever is connected before core
/// can promise the application a timeout it will honour.
///
/// Getting this wrong is not a small error. If core accepts a 30-second timeout
/// and then cannot enforce it, the application waits indefinitely on a runaway
/// query having been told, at `SQLSetStmtAttr` time, that its timeout was in
/// force. That is the defect this mechanism removes, so a backend
/// that is unsure should return `Err(NotImplemented)` and let core substitute
/// `0` with `01S02`. "No timeout" is a disappointing answer but a true one.
///
/// [`Backend::set_query_timeout`]: crate::backend::Backend::set_query_timeout
/// [`Backend::execute`]: crate::backend::Backend::execute
/// [`Backend::cancel`]: crate::backend::Backend::cancel
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryTimeout {
    /// The data source itself enforces the deadline, through a server-side
    /// statement timeout, a session variable or a per-query option.
    ///
    /// Core arms no timer. This is the better answer wherever it is available:
    /// the data source knows when the query really started, it stops the work
    /// rather than abandoning a client waiting on it, and it needs no
    /// [`Backend::cancel`] implementation.
    ///
    /// [`Backend::cancel`]: crate::backend::Backend::cancel
    DataSource,

    /// Core enforces the deadline, by arming a timer that calls
    /// [`Backend::cancel`] when it expires.
    ///
    /// **Returning this asserts that `cancel` really cancels.** Core has no way
    /// to check: `cancel`'s default returns `NotImplemented`, which `SQLCancel`
    /// treats as "there was nothing to cancel", and that is indistinguishable
    /// from a successful cancel of an idle statement. A backend that returns
    /// this without a working `cancel` gets a timer that fires into a void and
    /// an application that waits forever anyway.
    ///
    /// Implement [`Backend::is_cancelled`] too. Core asks it after the
    /// cancelled call fails, and it turns the backend's own symptom (a closed
    /// socket, an aborted query) into the `HYT00` the application is waiting to
    /// see.
    ///
    /// [`Backend::cancel`]: crate::backend::Backend::cancel
    /// [`Backend::is_cancelled`]: crate::backend::Backend::is_cancelled
    CoreCancels,
}
