//! [`Prompter`]: the hook a driver supplies so it can reach the user while a
//! connection is being established.
//!
//! Core carries the trait definition and decides *whether* prompting is
//! permitted; a driver supplies *how*. The split is what makes
//! `SQLDriverConnect`'s *DriverCompletion* enforceable: under
//! `SQL_DRIVER_NOPROMPT` the backend is handed `None` and has nothing to call,
//! so the spec's "do not prompt" cannot be forgotten at a call site.
//!
//! Core ships no implementation. Every one it could offer, whether opening a
//! browser or putting up a dialog, needs a dependency and a platform, and
//! neither belongs in the database-independent half of a driver.

use crate::errors::OdbcError;

/// Presents a URL to the user while a connection is being established.
///
/// Implemented by a driver that needs interactive authentication, such as an
/// OAuth 2.0 external-authentication flow, where the data source answers the
/// initial request with a login URL a human has to visit. Obtained from
/// [`Backend::prompter`](crate::backend::Backend::prompter), and reaches the
/// backend through [`ConnectParams::prompter`](crate::types::ConnectParams::prompter)
/// only when the entry point's *DriverCompletion* permits prompting.
///
/// `Send + Sync` because one implementation is shared, through an
/// `Arc<dyn Prompter>`, across every connection a driver makes, and because
/// core's own `Backend` bound requires it.
pub trait Prompter: Send + Sync {
    /// Show `url` to the user.
    ///
    /// Must return promptly: it *presents* the URL, it does not wait for the
    /// user to act on it. A driver polling for the result of an interactive
    /// login does that separately, in its own
    /// [`Backend::connect`](crate::backend::Backend::connect).
    ///
    /// The spec's `IM008` ("Dialog failed") is the state for a prompt that
    /// could not be shown, and it is the *driver's* to return: core never
    /// calls this method, so it never sees the failure. A driver mapping this
    /// error is choosing the SQLSTATE its own connect reports.
    ///
    /// # Errors
    ///
    /// Whatever the implementation could not do: no display, no browser, or no
    /// permission to launch one.
    fn present_url(&self, url: &str) -> Result<(), OdbcError>;
}
