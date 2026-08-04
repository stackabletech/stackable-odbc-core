//! [`Redacted`]: a `Debug` wrapper that prints `*****` instead of its value,
//! so sensitive fields such as passwords never appear in log output.

/// Wrapper that always prints as `*****` in [`Debug`] output, regardless of the contained value.
///
/// Use this for sensitive fields (e.g. passwords) in structs that derive [`Debug`].
#[derive(Clone)]
pub struct Redacted<T>(pub T);

impl<T> std::fmt::Debug for Redacted<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("*****")
    }
}
