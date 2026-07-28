//! ODBC escape-sequence translation (`{fn}`, `{d/t/ts}`, `{oj}`, `{escape}`).
//!
//! A single pure scanner shared by every backend. It rewrites escapes only in
//! real SQL text — string literals, quoted identifiers, and comments are copied
//! verbatim. Backends supply an [`EscapeDialect`] for the parts that differ.

use crate::errors::OdbcError;
use crate::types::SqlState;

/// Per-backend escape-translation rules.
///
/// Start from [`EscapeDialect::ansi_default`] and apply the `with_*` builders.
/// The fields are crate-private and the type is `#[non_exhaustive]`, so a rule
/// added here is a source-compatible change for every driver: there is no
/// struct literal to go stale and no field to be missed. Read a rule back
/// through its accessor.
#[non_exhaustive]
pub struct EscapeDialect {
    pub(crate) identifier_quotes: &'static [(char, char)],
    pub(crate) remap_scalar_fn: fn(&str) -> Option<&'static str>,
    pub(crate) rewrite_scalar_fn: fn(name: &str, args: &str) -> Option<String>,
    pub(crate) render_date: fn(&str) -> String,
    pub(crate) render_time: fn(&str) -> String,
    pub(crate) render_timestamp: fn(&str) -> String,
}

/// Field accessors for [`EscapeDialect`].
///
/// The fields themselves are crate-private: this type is `#[non_exhaustive]`
/// and built through its `with_*` builders, and public fields would have made
/// both of those advisory. Reading goes through these instead.
///
/// Adding an accessor is a source-compatible change, so this set covers every
/// field rather than only the ones a driver happens to need today.
impl EscapeDialect {
    /// Identifier-quote open→close pairs copied verbatim, e.g. `('"','"')`,
    /// `('`','`')`, `('[',']')`.
    #[must_use]
    pub fn identifier_quotes(&self) -> &'static [(char, char)] {
        self.identifier_quotes
    }

    /// Remap an ODBC `{fn NAME(...)}` scalar-function name to the backend's
    /// name; `None` passes the name through unchanged. Input is the raw name.
    ///
    /// The cheap path: it swaps the identifier in front of the parentheses and
    /// never sees the arguments, which is all `UCASE` → `upper` needs. When
    /// the argument syntax differs too, use [`EscapeDialect::rewrite_scalar_fn`].
    #[must_use]
    pub fn remap_scalar_fn(&self) -> fn(&str) -> Option<&'static str> {
        self.remap_scalar_fn
    }

    /// Rewrite a whole `{fn NAME(args)}` escape.
    ///
    /// `args` is the text between the outer parentheses, **already
    /// escape-translated** — a nested `{fn}`, `{ts}` or `{d}` inside the
    /// argument list is resolved before the dialect sees it, so a dialect
    /// never has to re-implement escape parsing. It is handed over with
    /// string literals, quoted identifiers, comments and nested parentheses
    /// intact: splitting it into arguments is the dialect's job, because only
    /// the dialect knows how many arguments each function takes. Core
    /// deliberately does not split on commas, which would corrupt
    /// `{fn LOCATE(',', x)}`.
    ///
    /// Return `None` to fall back to [`EscapeDialect::remap_scalar_fn`] plus
    /// verbatim arguments. Returning `Some` replaces the entire escape, so a
    /// zero-argument call can emit a bare keyword with no trailing `()` —
    /// which is what `{fn CURDATE()}` → `current_date` requires and what
    /// `remap_scalar_fn` alone cannot express.
    ///
    /// Only consulted when the call has a balanced parenthesis pair; a
    /// malformed `{fn ...}` takes the pass-through path unchanged.
    #[must_use]
    pub fn rewrite_scalar_fn(&self) -> fn(name: &str, args: &str) -> Option<String> {
        self.rewrite_scalar_fn
    }

    /// Render `{d <lit>}` given the raw inner literal text (e.g. `"'2020-01-01'"`).
    #[must_use]
    pub fn render_date(&self) -> fn(&str) -> String {
        self.render_date
    }

    /// Render `{t <lit>}`.
    #[must_use]
    pub fn render_time(&self) -> fn(&str) -> String {
        self.render_time
    }

    /// Render `{ts <lit>}`.
    #[must_use]
    pub fn render_timestamp(&self) -> fn(&str) -> String {
        self.render_timestamp
    }
}

impl EscapeDialect {
    /// Replaces the identifier-quote pairs copied verbatim by the scanner.
    #[must_use]
    pub fn with_identifier_quotes(mut self, quotes: &'static [(char, char)]) -> Self {
        self.identifier_quotes = quotes;
        self
    }

    /// Sets the cheap scalar-function rename, which sees only the name.
    #[must_use]
    pub fn with_remap_scalar_fn(mut self, f: fn(&str) -> Option<&'static str>) -> Self {
        self.remap_scalar_fn = f;
        self
    }

    /// Sets the full scalar-function rewrite, which sees the arguments too.
    #[must_use]
    pub fn with_rewrite_scalar_fn(
        mut self,
        f: fn(name: &str, args: &str) -> Option<String>,
    ) -> Self {
        self.rewrite_scalar_fn = f;
        self
    }

    /// Sets the `{d}`, `{t}` and `{ts}` literal renderers.
    #[must_use]
    pub fn with_datetime_renderers(
        mut self,
        date: fn(&str) -> String,
        time: fn(&str) -> String,
        timestamp: fn(&str) -> String,
    ) -> Self {
        self.render_date = date;
        self.render_time = time;
        self.render_timestamp = timestamp;
        self
    }

    /// Neutral ANSI dialect: `"`-quoted identifiers, no scalar remap,
    /// `DATE '…'`/`TIME '…'`/`TIMESTAMP '…'` literals. Used as the `Backend`
    /// default for backends that do not override `escape_dialect`.
    pub const fn ansi_default() -> EscapeDialect {
        EscapeDialect {
            identifier_quotes: &[('"', '"')],
            remap_scalar_fn: |_| None,
            rewrite_scalar_fn: |_, _| None,
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

/// How deeply ODBC escape sequences may nest before translation gives up.
///
/// [`translate_slice`] and [`translate_escape`] are mutually recursive, one
/// level per nested escape, over SQL the application supplies. Without a bound,
/// input like `{oj {oj {oj …}}}` recurses until the stack is exhausted — and a
/// stack overflow is a guard-page abort, not a panic, so
/// [`panic_safe`](crate::panic::panic_safe) cannot contain it and the host
/// application dies. Measured at roughly 330 bytes per level, which is about
/// 25 000 levels on Linux's 8 MiB main stack and only about 3 000 on a 1 MiB
/// Windows thread stack.
///
/// 64 is far above anything real SQL produces (a handful of levels at most) and
/// far below any platform's limit.
const MAX_ESCAPE_DEPTH: usize = 64;

/// Translate ODBC escapes in `sql`. Returns the rewritten SQL, or an error for
/// an unsupported (`{call}` → HYC00) or malformed (unterminated → 42000)
/// escape, or one nested more than 64 levels deep (42000).
pub fn translate_escapes(sql: &str, dialect: &EscapeDialect) -> Result<String, OdbcError> {
    let chars: Vec<char> = sql.chars().collect();
    translate_slice(&chars, dialect, 0)
}

fn translate_slice(
    chars: &[char],
    dialect: &EscapeDialect,
    depth: usize,
) -> Result<String, OdbcError> {
    // Checked here rather than in `translate_escape` because every recursive
    // path — the `{oj}` body, the `{fn}` argument list and the tail after a
    // rewritten call — re-enters through this function.
    if depth > MAX_ESCAPE_DEPTH {
        return Err(OdbcError::general(
            format!("Escape sequences nested deeper than {MAX_ESCAPE_DEPTH} levels"),
            SqlState::syntax_error_or_access_violation(),
        ));
    }
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
            translate_escape(chars, &mut i, dialect, &mut out, depth)?;
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

/// Find the index of the `)` matching the `(` at `open`, skipping strings,
/// quoted identifiers, comments and nested parentheses — the same
/// literal-awareness [`find_matching_brace`] has, so that a `)` inside
/// `'a)b'`, `"a)b"` or `-- a)b` does not close the call.
///
/// Bounded by `limit` (the index of the escape's closing `}`). Returns `None`
/// rather than an error when there is no match, so an unbalanced `{fn ...}`
/// takes the pass-through path instead of failing the statement. Only a
/// malformed *escape* is an error here; malformed SQL inside one is the data
/// source's to reject.
fn find_matching_paren(
    chars: &[char],
    open: usize,
    limit: usize,
    dialect: &EscapeDialect,
) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = open;
    while i < limit {
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
        } else if c == '(' {
            depth += 1;
        } else if c == ')' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Translate one `{...}` escape starting at `*i == '{'`. Advances `*i` past the
/// matching `}`.
fn translate_escape(
    chars: &[char],
    i: &mut usize,
    dialect: &EscapeDialect,
    out: &mut String,
    depth: usize,
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
                match translate_call(chars, &name, n, close, dialect, depth)? {
                    FnCall::Rewritten(rewritten) => out.push_str(&rewritten),
                    FnCall::Declined(rest) => {
                        let mapped = (dialect.remap_scalar_fn)(&name).unwrap_or(&name);
                        out.push_str(mapped);
                        out.push_str(&rest);
                    }
                    FnCall::NotACall => {
                        let mapped = (dialect.remap_scalar_fn)(&name).unwrap_or(&name);
                        out.push_str(mapped);
                        let rest = translate_slice(&chars[n..close], dialect, depth + 1)?;
                        out.push_str(&rest);
                    }
                }
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
            let body = translate_slice(&chars[after_kw..close], dialect, depth + 1)?;
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

/// What [`translate_call`] found when it examined a `{fn NAME(args)}` call.
enum FnCall {
    /// The dialect rewrote the call. The string is the finished replacement.
    Rewritten(String),
    /// The dialect declined. The string is the already-translated remainder of
    /// the escape — the parenthesized argument list and anything between the
    /// closing paren and the `}` — so the caller can assemble the
    /// `remap_scalar_fn` form without translating that span a second time.
    Declined(String),
    /// Not a call: no balanced parenthesis pair follows the name. Nothing has
    /// been translated, so the caller translates the remainder itself.
    NotACall,
}

/// Offer a `{fn NAME(args)}` call to [`EscapeDialect::rewrite_scalar_fn`].
///
/// `name_end` is the index just past the function name, `close` the index of
/// the escape's `}`. Errors propagate from translating the argument text — a
/// `{call ...}` nested inside an argument list must still fail with `HYC00`,
/// not be silently swallowed.
///
/// The argument span must be translated exactly once, whether the dialect
/// accepts the call or not, which is why [`FnCall::Declined`] hands that work
/// back to the caller rather than the caller redoing it. Translating it and
/// then discarding it on the declined path would double the cost at every
/// nesting level, making translation exponential in depth: `MAX_ESCAPE_DEPTH`
/// bounds the recursion but not the work, so it would set the exponent rather
/// than cap it, and a few hundred bytes of nested `{fn}` would never finish.
/// [`EscapeDialect::ansi_default`] declines every call, so that is the path a
/// driver takes unless it implements `rewrite_scalar_fn` — and one that does is
/// still on it for every name it declines.
fn translate_call(
    chars: &[char],
    name: &str,
    name_end: usize,
    close: usize,
    dialect: &EscapeDialect,
    depth: usize,
) -> Result<FnCall, OdbcError> {
    let mut p = name_end;
    while p < close && chars[p].is_whitespace() {
        p += 1;
    }
    if chars.get(p) != Some(&'(') {
        return Ok(FnCall::NotACall);
    }
    let Some(arg_close) = find_matching_paren(chars, p, close, dialect) else {
        return Ok(FnCall::NotACall);
    };

    // Translate the arguments *before* the dialect sees them, so a nested
    // escape is already resolved and the dialect never parses escapes itself.
    let args = translate_slice(&chars[p + 1..arg_close], dialect, depth + 1)?;
    // Anything between the closing paren and `}` is whitespace in a
    // well-formed call, but translate it rather than dropping it so that a
    // trailing nested escape is not silently lost.
    let tail = translate_slice(&chars[arg_close + 1..close], dialect, depth + 1)?;

    match (dialect.rewrite_scalar_fn)(name, args.trim()) {
        Some(rewritten) => Ok(FnCall::Rewritten(rewritten + tail.trim_end())),
        None => {
            // Reassemble exactly what translating `chars[name_end..close]` in
            // one piece would have produced. Only the argument span needed
            // translating, so everything else is copied from the source: the
            // whitespace before the `(` and the `(` itself (`..=p`), then the
            // matching `)` at `arg_close`. `translate_slice` copies whitespace
            // and parentheses verbatim, so splitting the span at them does not
            // change the result.
            let mut rest: String = chars[name_end..=p].iter().collect();
            rest.push_str(&args);
            rest.push(chars[arg_close]);
            rest.push_str(&tail);
            Ok(FnCall::Declined(rest))
        }
    }
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
            // Deliberately the no-op, so every other test in this module
            // covers a dialect that sets only `remap_scalar_fn` and pins what
            // such a dialect does with a `{fn}` call it has no rule for.
            rewrite_scalar_fn: |_, _| None,
            render_date: d,
            render_time: t,
            render_timestamp: ts,
        }
    }
    fn tr(sql: &str) -> String {
        translate_escapes(sql, &dialect()).unwrap()
    }

    // ------------------------------------------------------------------
    // rewrite_scalar_fn — argument-aware scalar function rewriting
    // ------------------------------------------------------------------

    /// A dialect that rewrites the shapes `remap_scalar_fn` cannot express:
    /// an argument reordering, a bare keyword with no parentheses, and a
    /// rewrite that inspects the argument text.
    fn rewrite(name: &str, args: &str) -> Option<String> {
        match name.to_ascii_uppercase().as_str() {
            // Argument syntax differs, not just the name.
            "LOCATE" => {
                let (needle, haystack) = args.split_once(',')?;
                Some(format!(
                    "position({} IN {})",
                    needle.trim(),
                    haystack.trim()
                ))
            }
            // Zero-argument calls that must emit a bare keyword: the target
            // dialect rejects `current_date()`.
            "CURDATE" if args.is_empty() => Some("current_date".to_string()),
            "USERNAME" if args.is_empty() => Some("current_user".to_string()),
            // Proves the hook sees the whole argument text, not just a name.
            "ARGS" => Some(format!("<<{args}>>")),
            _ => None,
        }
    }
    fn rewriting_dialect() -> EscapeDialect {
        EscapeDialect {
            rewrite_scalar_fn: rewrite,
            ..dialect()
        }
    }
    fn rtr(sql: &str) -> String {
        translate_escapes(sql, &rewriting_dialect()).unwrap()
    }

    #[test]
    fn rewrite_scalar_fn_can_reorder_arguments() {
        // `remap_scalar_fn` only swaps the identifier in front of the parens,
        // so this rewrite was impossible to express before.
        assert_eq!(
            rtr("SELECT {fn LOCATE('b','ab')}"),
            "SELECT position('b' IN 'ab')"
        );
    }

    #[test]
    fn rewrite_scalar_fn_can_emit_a_bare_keyword_for_a_zero_argument_call() {
        // No trailing "()" — the whole point: `current_date()` is a syntax
        // error in the dialects that need this.
        assert_eq!(rtr("SELECT {fn CURDATE()}"), "SELECT current_date");
        assert_eq!(rtr("SELECT {fn USERNAME()}"), "SELECT current_user");
    }

    #[test]
    fn rewrite_scalar_fn_sees_arguments_with_nested_escapes_already_translated() {
        // The hook must run *after* the argument text is itself translated,
        // so a dialect never has to re-implement escape parsing.
        assert_eq!(
            rtr("SELECT {fn ARGS({ts '2020-01-01 00:00:00'})}"),
            "SELECT <<TIMESTAMP '2020-01-01 00:00:00'>>"
        );
        // Nested {fn} inside the arguments, including one that is itself
        // rewritten.
        assert_eq!(
            rtr("SELECT {fn ARGS({fn UCASE(a)}, {fn CURDATE()})}"),
            "SELECT <<upper(a), current_date>>"
        );
    }

    #[test]
    fn rewrite_scalar_fn_argument_text_keeps_literals_intact() {
        // Core must not split on commas: this comma is inside a string, and
        // splitting there would corrupt the call. Argument splitting is the
        // dialect's problem, but only if core hands over honest text.
        assert_eq!(rtr("SELECT {fn ARGS(',')}"), "SELECT <<','>>");
        assert_eq!(
            rtr("SELECT {fn ARGS('a,b', \"c,d\", -- x,y\n e)}"),
            "SELECT <<'a,b', \"c,d\", -- x,y\n e>>"
        );
    }

    #[test]
    fn rewrite_scalar_fn_argument_text_keeps_nested_parens_intact() {
        assert_eq!(
            rtr("SELECT {fn ARGS(foo(a,b), c)}"),
            "SELECT <<foo(a,b), c>>"
        );
    }

    #[test]
    fn rewrite_scalar_fn_returning_none_falls_back_to_remap_scalar_fn() {
        // UCASE has no rewrite but does have a remap, so the cheap path still
        // applies and the arguments are passed through verbatim.
        assert_eq!(rtr("SELECT {fn UCASE(name)}"), "SELECT upper(name)");
        // Neither a rewrite nor a remap: name and arguments both untouched.
        assert_eq!(rtr("SELECT {fn WHATEVER(a, b)}"), "SELECT WHATEVER(a, b)");
    }

    #[test]
    fn rewrite_scalar_fn_is_not_consulted_by_a_dialect_that_does_not_set_it() {
        // A dialect that sets only `remap_scalar_fn` passes an unrecognised
        // call straight through, name and arguments untouched. It must not
        // become an error just because the hook exists.
        assert_eq!(
            tr("SELECT {fn LOCATE('b','ab')}"),
            "SELECT LOCATE('b','ab')"
        );
        assert_eq!(tr("SELECT {fn CURDATE()}"), "SELECT CURDATE()");
    }

    #[test]
    fn rewriting_dialect_preserves_the_existing_escape_errors() {
        let d = rewriting_dialect();
        // {call} and {?= call} stay HYC00 ...
        for sql in ["{call foo()}", "{?= call foo()}"] {
            let err = translate_escapes(sql, &d).unwrap_err();
            assert_eq!(
                err.sqlstate().as_str(),
                "HYC00",
                "{sql} must stay optional-feature-not-implemented"
            );
        }
        // ... and an unterminated escape stays 42000.
        let err = translate_escapes("SELECT {fn LOCATE('b','ab')", &d).unwrap_err();
        assert_eq!(err.sqlstate().as_str(), "42000");
    }

    // ------------------------------------------------------------------
    // Nesting depth
    //
    // translate_slice and translate_escape are mutually recursive, one level
    // per nested escape, over SQL the application supplies. A stack overflow
    // is a guard-page abort rather than a panic, so `panic_safe`'s
    // catch_unwind cannot contain it: the host process dies.
    // ------------------------------------------------------------------

    /// `{oj ...}` nests through `translate_escape`'s body recursion.
    fn nested_oj(depth: usize) -> String {
        format!("{}x{}", "{oj ".repeat(depth), "}".repeat(depth))
    }

    /// `{fn ...}` nests through `rewrite_call`'s argument recursion, a
    /// different path into `translate_slice`.
    fn nested_fn(depth: usize) -> String {
        format!("{}x{}", "{fn UCASE(".repeat(depth), ")}".repeat(depth))
    }

    #[test]
    fn nesting_within_the_depth_limit_still_translates() {
        // Real SQL nests escapes a handful deep at most; the limit must not be
        // so tight that ordinary queries hit it.
        let out = translate_escapes(&nested_oj(MAX_ESCAPE_DEPTH - 1), &dialect())
            .expect("nesting inside the limit must translate");
        assert!(out.contains('x'));
    }

    #[test]
    fn nesting_within_the_depth_limit_is_linear_through_the_fn_argument_path() {
        // Exactly `MAX_ESCAPE_DEPTH`, the deepest input the limit accepts, and
        // it has to run to completion: the `+ 1` tests above fail on the first
        // descent, so they never reach the argument recursion. `dialect()`
        // declines every call, so every level here takes the declined path,
        // which is where a superlinear translation cost would show up.
        let out = translate_escapes(&nested_fn(MAX_ESCAPE_DEPTH), &dialect())
            .expect("the deepest accepted nesting must translate");
        assert!(out.contains('x'));
        // Every level is the declined path, so each contributes its remapped
        // name and its parentheses: `upper(` ... `)`.
        assert_eq!(out.matches("upper(").count(), MAX_ESCAPE_DEPTH);
    }

    #[test]
    fn nesting_beyond_the_depth_limit_is_rejected_not_overflowed() {
        let err = translate_escapes(&nested_oj(MAX_ESCAPE_DEPTH + 1), &dialect()).unwrap_err();
        assert_eq!(err.sqlstate().as_str(), "42000");
    }

    #[test]
    fn nesting_beyond_the_depth_limit_is_rejected_through_the_fn_argument_path() {
        let err = translate_escapes(&nested_fn(MAX_ESCAPE_DEPTH + 1), &dialect()).unwrap_err();
        assert_eq!(err.sqlstate().as_str(), "42000");
    }

    // Skipped under Miri: 50 000 levels over a 250 KB input costs more than 16
    // minutes of interpreted execution, against a 30-minute budget for the whole
    // `miri` CI job. Nothing is lost by skipping it — `escape.rs` contains no
    // `unsafe` at all, so Miri has no undefined behaviour to find here, and the
    // three tests above already exercise the depth limit on both recursion paths
    // at `MAX_ESCAPE_DEPTH ± 1`. Same rationale as `--skip proptest`: the check
    // is algorithmic, and it runs on stable.
    #[cfg_attr(
        miri,
        ignore = "50k-deep input is too slow under Miri; no unsafe here to check"
    )]
    #[test]
    fn pathological_nesting_returns_an_error_rather_than_killing_the_process() {
        // The depth that measurably aborted the process before the limit
        // existed. It must now come back as an ordinary error.
        let err = translate_escapes(&nested_oj(50_000), &dialect()).unwrap_err();
        assert_eq!(err.sqlstate().as_str(), "42000");
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
