//! The ODBC spec's per-function Diagnostics table, transcribed as code.
//!
//! Every function page under
//! <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/> carries a
//! "Diagnostics" table: one row per SQLSTATE, with some rows (or some
//! *clauses* of a row) annotated `(DM)` to mean the Driver Manager returns
//! it, not the driver. `CLAUDE.md` makes reproducing that table in each FFI
//! function's doc comment non-negotiable. This module is the machine-checkable
//! half of that rule, in the same spirit as [`crate::types::info_type_shape`]:
//! the spec table transcribed once, and a test that compares it against what
//! the source actually says.
//!
//! # Why this exists
//!
//! A `(DM)` annotation is easy to get wrong in both directions, and prose
//! cannot be trusted to stay right across dozens of hand edits.
//!
//! - **A row with no `(DM)` marker, written off as driver-manager business.**
//!   The phrase "(driver-manager-handled; not returned here)" then declines to
//!   answer a real question about this driver.
//! - **A `(DM)`-marked row presented as an ordinary driver row.** A reader
//!   cannot then tell a spec-required check from a defensive one.
//! - **A row marked on one clause and not the next.** `HY090` in the catalog
//!   functions is the case to watch, since reading it as wholly the Driver
//!   Manager's gives the wrong reason for a correct conclusion.
//!
//! The `(DM)` attribution and the row set are therefore pinned by
//! [`every_doc_comment_matches_the_spec_diagnostics_table`] rather than by
//! review.
//!
//! # What the guard does and does not prove
//!
//! It proves three things, and only these three:
//!
//! 1. Every row in the spec's table appears in the doc comment.
//! 2. Every SQLSTATE the doc comment names is in the spec's table, unless the
//!    bullet says `absent from this function's diagnostics table`. That is the
//!    house phrase, used for `08S01` in `sql_describe_col_w` and `3D000` in
//!    `sql_connect_w`.
//! 3. A bullet attributing a row to the Driver Manager names a row the spec
//!    really did mark `(DM)`, and a `(DM)`-marked row is never presented as an
//!    ordinary driver row.
//!
//! It does **not** prove the *reason* prose is true. "not applicable: core
//! supports no connection timeout" is checked by a human, and the reasons this
//! sweep wrote are recorded in the doc comments rather than here so that the
//! next reader inherits the evidence.
//!
//! # The blind spot those three properties leave
//!
//! All three start from the doc comment, so a SQLSTATE the code returns and the
//! doc comment never mentions satisfies every one of them, because there is no
//! bullet to check. `SQLBindParameter` is the shape of the gap: answering
//! `HY024` for an unrecognised `InputOutputType` while its own page lists
//! `HY105` for exactly that condition and no `HY024` at all.
//!
//! [`every_sqlstate_a_function_body_returns_is_in_its_table_or_declared_off_table`]
//! closes it from the other end: it scans each function's *body* for
//! `SqlState::` factory calls and requires each state to be in the transcribed
//! table or declared with the off-table phrase. It is an under-approximation by
//! construction (see its own docs for what it cannot see), which is why the
//! prose reasons still matter.
//!
//! # The four verdict phrasings
//!
//! The corrections use a closed vocabulary, so the guard can recognise a
//! Driver-Manager attribution without reading English:
//!
//! | Situation | Phrasing |
//! |---|---|
//! | Row is `(DM)`, core adds no check | `(driver-manager-handled; not returned here)` |
//! | Row is `(DM)`, core checks it anyway | `The spec annotates this (DM); it is guarded defensively here` |
//! | Row is not `(DM)`, core does not return it | a plain reason, with **no** `(DM)` and no "driver-manager" anywhere in the bullet |
//! | Row is not in the table at all | `**absent from this function's diagnostics table**`, then why it is returned anyway |
//!
//! A `(DM)` row core checks anyway is not a spec violation. `SQLAllocHandle`'s
//! `HY009` and `HY092` are guarded that way, because core is also linked
//! directly (by its own tests, and by an embedder with no Driver Manager in
//! front of it) and because several of those checks are load-bearing for memory
//! safety rather than for the spec.
//!
//! # A third row shape the guard does not model
//!
//! [`DmMarking`] records `(DM)` *markers*, and some rows divide the work in
//! prose without printing one. The `24000` rows of `SQLExecDirect`,
//! `SQLExecute` and `SQLGetTypeInfo` all read "This error is returned by the
//! Driver Manager if `SQLFetch` or `SQLFetchScroll` has not returned
//! SQL_NO_DATA, and is returned by the driver if `SQLFetch` or
//! `SQLFetchScroll` has returned SQL_NO_DATA." That splits one condition by
//! outcome, so both sides own it at different moments, and the row still
//! carries no marker anywhere. `SQLPrepare`'s `24000` is the contrast: same
//! subject, an actual `(DM)` on its first sentence, hence [`DmMarking::Split`].
//!
//! Those three are transcribed [`DmMarking::None`], which is what the page
//! prints, and their doc comments say in prose where the boundary falls. The
//! guard cannot check that half, because the sentence it would have to read is
//! English rather than a marker. Do not "fix" such a row to `Split` to make the
//! prose match: `Split` names a `(DM)`-marked clause, and there is none to
//! name.

/// One row of a spec Diagnostics table.
struct DiagnosticsRow {
    /// The five-character SQLSTATE, exactly as the spec's first column spells
    /// it.
    sqlstate: &'static str,
    /// Which of the row's clauses the spec marks `(DM)`.
    dm: DmMarking,
}

/// How a Diagnostics row is annotated for the Driver Manager.
///
/// Three variants rather than a `bool`, because the split case is real and is
/// where the subtlest errors live. `SQLTables`' `HY090` reads
/// "(DM) The value of one of the name length arguments was less than 0 but not
/// equal to SQL_NTS. The value of one of the name length arguments exceeded the
/// maximum length value for the corresponding name.": one marker, two
/// sentences, and only the first is covered.
enum DmMarking {
    /// No clause of this row carries `(DM)`. A doc comment calling it the
    /// Driver Manager's is wrong.
    None,
    /// Every clause carries `(DM)`. A doc comment presenting it as an ordinary
    /// driver row is wrong, even when core does implement the check. Say it is
    /// guarded defensively instead.
    All,
    /// Some clauses carry `(DM)` and some do not. The payload is a distinctive
    /// phrase from an **unmarked** clause; a doc comment for this row has to
    /// quote it, so that it says *which* half it is talking about rather than
    /// generalising the row away.
    Split(&'static str),
}

/// One function's transcribed table, and where to find its doc comment.
struct FunctionDiagnostics {
    /// The generic implementation's Rust name, e.g. `"sql_bind_col"`.
    func: &'static str,
    /// The ODBC name, for failure messages.
    odbc_name: &'static str,
    /// The module path, for failure messages.
    module: &'static str,
    /// That module's source, via `include_str!`.
    source: &'static str,
    /// The spec's Diagnostics table, in the order the page prints it.
    rows: &'static [DiagnosticsRow],
}

const BIND_RS: &str = include_str!("../ffi/bind.rs");
const CONNECT_RS: &str = include_str!("../ffi/connect.rs");
const CONNECT_ATTR_RS: &str = include_str!("../ffi/connect_attr.rs");
const CURSOR_RS: &str = include_str!("../ffi/cursor.rs");
const DESC_RS: &str = include_str!("../ffi/desc.rs");
const DIAG_RS: &str = include_str!("../ffi/diag.rs");
const ENV_RS: &str = include_str!("../ffi/env.rs");
const EXECUTE_RS: &str = include_str!("../ffi/execute.rs");
const FETCH_RS: &str = include_str!("../ffi/fetch.rs");
const HANDLE_RS: &str = include_str!("../ffi/handle.rs");
const INFO_RS: &str = include_str!("../ffi/info.rs");
const METADATA_RS: &str = include_str!("../ffi/metadata.rs");
const PARAMS_RS: &str = include_str!("../ffi/params.rs");
const SETUP_RS: &str = include_str!("../ffi/setup.rs");
const STMT_ATTR_RS: &str = include_str!("../ffi/stmt_attr.rs");
const TRAN_RS: &str = include_str!("../ffi/tran.rs");

/// Every module holding `pub unsafe fn` FFI entry points, so
/// [`every_exported_ffi_function_has_a_transcribed_diagnostics_table`] can
/// derive the function list from the source rather than from a second
/// hand-maintained roster. A hand-maintained list of "functions someone
/// thought to transcribe" is the failure mode this whole module exists to
/// close.
const MODULES: &[(&str, &str)] = &[
    ("src/ffi/bind.rs", BIND_RS),
    ("src/ffi/connect.rs", CONNECT_RS),
    ("src/ffi/connect_attr.rs", CONNECT_ATTR_RS),
    ("src/ffi/cursor.rs", CURSOR_RS),
    ("src/ffi/desc.rs", DESC_RS),
    ("src/ffi/diag.rs", DIAG_RS),
    ("src/ffi/env.rs", ENV_RS),
    ("src/ffi/execute.rs", EXECUTE_RS),
    ("src/ffi/fetch.rs", FETCH_RS),
    ("src/ffi/handle.rs", HANDLE_RS),
    ("src/ffi/info.rs", INFO_RS),
    ("src/ffi/metadata.rs", METADATA_RS),
    ("src/ffi/params.rs", PARAMS_RS),
    ("src/ffi/setup.rs", SETUP_RS),
    ("src/ffi/stmt_attr.rs", STMT_ATTR_RS),
    ("src/ffi/tran.rs", TRAN_RS),
];

/// Functions whose spec page defines no Diagnostics table at all. A permanent
/// exception, not a backlog.
const NO_SPEC_DIAGNOSTICS_TABLE: &[&str] = &[
    // ConfigDSN is an ODBC *installer* entry point, not an ODBC function, and
    // it has no handle to post a diagnostic record through. Its page does have
    // a Diagnostics table, but the table's rows are installer error codes
    // (ODBC_ERROR_INVALID_HWND and friends), not SQLSTATEs, so there is nothing
    // here for this guard to compare against. It reports via
    // SQLPostInstallerError instead, which
    // `every_false_return_from_config_dsn_w_posts_an_installer_error` in
    // `ffi/setup.rs` guards, and the codes themselves are modelled by
    // `crate::setup::InstallerError`.
    "config_dsn_w",
    // The two diagnostic-retrieval functions have a "Diagnostics" heading with
    // no SQLSTATE table under it. Both pages open that section with the same
    // sentence, "does not post diagnostic records for itself", and then list
    // return codes instead, because a function that reads the diagnostic queue
    // cannot report through it.
    "sql_get_diag_rec_w",
    "sql_get_diag_field_w",
];

/// The `///` lines of `func`'s doc comment, in source order, with the leading
/// `/// ` stripped.
///
/// Walks backwards from the `pub unsafe fn` line over the contiguous run of
/// doc lines, skipping the `#[allow(...)]` and `// SAFETY:` lines that sit
/// between a doc comment and its function.
fn doc_lines<'a>(source: &'a str, func: &str) -> Vec<&'a str> {
    let needle = format!("\npub unsafe fn {func}");
    let at = match source.find(&needle) {
        Some(at) => at,
        None => panic!("{func} is not defined in the module declared for it"),
    };
    let mut lines: Vec<&str> = Vec::new();
    for line in source[..at].lines().rev() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("///") {
            lines.push(rest.strip_prefix(' ').unwrap_or(rest));
        } else if trimmed.starts_with("#[") || trimmed.starts_with("//") {
            continue;
        } else {
            break;
        }
    }
    lines.reverse();
    lines
}

/// One `- SQLSTATE …` bullet: the states it names, and its prose with
/// continuation lines joined.
struct Bullet {
    states: Vec<String>,
    text: String,
}

/// The SQLSTATE bullets of a doc comment's `# Spec compliance` section.
///
/// A bullet that does not open with a SQLSTATE is skipped along with its
/// continuation lines, so its prose cannot be misread as part of a neighbouring
/// row's verdict. `sql_alloc_handle`'s "Handle-specific rules" and
/// `sql_free_handle`'s `SQL_INVALID_HANDLE` note are the two of that shape.
fn spec_compliance_bullets(doc: &[&str]) -> Vec<Bullet> {
    let Some(start) = doc.iter().position(|l| l.trim() == "# Spec compliance") else {
        return Vec::new();
    };
    let body = &doc[start + 1..];
    let end = body
        .iter()
        .position(|l| l.starts_with("# "))
        .unwrap_or(body.len());

    let mut out: Vec<Bullet> = Vec::new();
    let mut in_state_bullet = false;
    for line in &body[..end] {
        if let Some(rest) = line.strip_prefix("- ") {
            match leading_states(rest) {
                Some(states) => {
                    out.push(Bullet {
                        states,
                        text: rest.to_string(),
                    });
                    in_state_bullet = true;
                }
                None => in_state_bullet = false,
            }
            continue;
        }
        if line.trim().is_empty() {
            // A blank line does not end a bullet: `sql_column_privileges_w`'s
            // `HY009` sets its two unmarked clauses out as a numbered sub-list,
            // which needs one. What ends a bullet is the next unindented line.
            continue;
        }
        if in_state_bullet && line.starts_with(' ') {
            if let Some(last) = out.last_mut() {
                last.text.push(' ');
                last.text.push_str(line.trim());
            }
        } else {
            in_state_bullet = false;
        }
    }
    out
}

/// The SQLSTATE(s) a bullet opens with, if any.
///
/// Several bullet shapes are in use across `ffi/`, all accepted rather than
/// normalised, because rewriting every doc comment into one shape buys nothing
/// the parser needs:
///
/// ```text
/// - 01000: General warning …
/// - `01000` General warning …
/// - 01000 (general warning): …
/// - **01000** — General warning …
/// - IM001–IM018: All Driver Manager internal codes …
/// ```
///
/// The fourth shape is why `—` is one of the terminators below. Replace one in
/// that position with a colon rather than with nothing, so the state stays the
/// first whitespace-delimited word of the head.
fn leading_states(bullet: &str) -> Option<Vec<String>> {
    let head: String = bullet
        .chars()
        .take_while(|c| !matches!(c, ':' | '(' | '—' | ',' | ';'))
        .filter(|c| !matches!(c, '`' | '*'))
        .collect();
    let head = head.trim();

    if let Some((lo, hi)) = head.split_once('–') {
        let lo = lo.trim();
        let hi = hi.split_whitespace().next().unwrap_or("");
        if is_sqlstate(lo) && is_sqlstate(hi) {
            return Some(expand_range(lo, hi));
        }
    }
    let first = head.split_whitespace().next()?;
    is_sqlstate(first).then(|| vec![first.to_string()])
}

fn is_sqlstate(t: &str) -> bool {
    t.len() == 5
        && t.chars()
            .all(|c| c.is_ascii_digit() || c.is_ascii_uppercase())
}

/// `IM001–IM018` covers every state in that class with a numeric suffix in the
/// inclusive span. The blanket form is the accepted house shorthand for a
/// Driver-Manager block (`sql_connect_w` uses it), and expanding it here is
/// what lets one bullet satisfy fifteen transcribed rows.
fn expand_range(lo: &str, hi: &str) -> Vec<String> {
    let class = &lo[..2];
    match (lo[2..].parse::<u32>(), hi[2..].parse::<u32>()) {
        (Ok(first), Ok(last)) if class == &hi[..2] && first <= last => {
            (first..=last).map(|n| format!("{class}{n:03}")).collect()
        }
        _ => vec![lo.to_string(), hi.to_string()],
    }
}

/// Every way a doc comment attributes a row to the Driver Manager, lower-cased
/// and backtick-free for a case-insensitive `contains`. A closed set on
/// purpose: a correction that invents a fifth phrasing silently disables
/// property 3 for that bullet, so a new phrasing has to be added here to count.
const DM_ATTRIBUTIONS: &[&str] = &[
    "(dm)",
    "driver-manager-handled",
    "driver-manager handled",
    "handled by the driver manager",
    "the dm checks",
];

/// Phrases that **deny** a `(DM)` marker, stripped before the attribution scan.
///
/// This is not a nicety. A bullet that says "the row carries no `(DM)` marker"
/// in as many words is making the point the guard exists to enforce, and a
/// naive `contains("(dm)")` would read that denial as a claim and fail the
/// bullet for being right.
const DM_DENIALS: &[&str] = &[
    "no (dm) marker",
    "no (dm)-marked clause",
    "not dm-annotated",
];

fn claims_driver_manager(text: &str) -> bool {
    let mut lower = text.to_lowercase().replace(['`', '*'], "");
    for denial in DM_DENIALS {
        lower = lower.replace(denial, "");
    }
    DM_ATTRIBUTIONS.iter().any(|p| lower.contains(p))
}

/// The house phrase for a SQLSTATE core returns although the function's own
/// table omits it. Already in use for `08S01` in `sql_describe_col_w` and
/// `3D000` in `sql_connect_w`.
const OFF_TABLE: &str = "absent from this function's diagnostics table";

/// This module's own source, for the factory-name mapping below.
const SQL_STATE_RS: &str = include_str!("sql_state.rs");

/// Every `SqlState` factory paired with the SQLSTATE it produces, read out of
/// `sql_state.rs` rather than restated here.
///
/// Two passes over that file: `pub const NAME: &str = "XXXXX";` gives the
/// constants, and `pub fn factory() -> Self { Self::new(NAME) }` gives the
/// factory that returns each. Deriving it is the point, because a hand-written
/// second copy of the state names is exactly what this module exists to
/// prevent.
fn sql_state_factories() -> Vec<(String, &'static str)> {
    let mut constants: Vec<(&str, &str)> = Vec::new();
    for line in SQL_STATE_RS.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("pub const ") else {
            continue;
        };
        let Some((name, value)) = rest.split_once(": &str = ") else {
            continue;
        };
        let value = value.trim_end_matches(';').trim_matches('"');
        if is_sqlstate(value) {
            constants.push((name, value));
        }
    }

    let mut factories: Vec<(String, &'static str)> = Vec::new();
    let mut pending: Option<&str> = None;
    for line in SQL_STATE_RS.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("pub fn ") {
            pending = rest.split('(').next();
            continue;
        }
        if let Some(name) = pending
            && let Some(rest) = trimmed.strip_prefix("Self::new(")
        {
            let konst = rest.trim_end_matches(')');
            if let Some(&(_, value)) = constants.iter().find(|(c, _)| *c == konst) {
                factories.push((format!("SqlState::{name}()"), value));
            }
            pending = None;
        }
    }
    factories
}

/// The body text of `func`, from its signature to the closing brace in column
/// zero.
///
/// Textual, like [`doc_lines`], because the alternative is a parser and all
/// this needs is "which `SqlState::` factories appear between these two
/// points". Stopping at a column-zero `}` works because every function here is
/// a top-level item, and it keeps the test module, which names plenty of
/// SQLSTATEs, out of the scan.
fn body_of<'a>(source: &'a str, func: &str) -> &'a str {
    let needle = format!("\npub unsafe fn {func}");
    let at = match source.find(&needle) {
        Some(at) => at + 1,
        None => panic!("{func} is not defined in the module declared for it"),
    };
    let rest = &source[at..];
    match rest.find("\n}\n") {
        Some(end) => &rest[..end],
        None => rest,
    }
}

#[rustfmt::skip]
const DIAGNOSTICS_TABLES: &[FunctionDiagnostics] = &[
FunctionDiagnostics {
    func: "sql_bind_col",
    odbc_name: "SQLBindCol",
    module: "src/ffi/bind.rs",
    source: BIND_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlbindcol-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "07006", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "07009", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY003", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY090", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYC00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
    ],
},
FunctionDiagnostics {
    func: "sql_exec_direct_w",
    odbc_name: "SQLExecDirect",
    module: "src/ffi/execute.rs",
    source: EXECUTE_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlexecdirect-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01003", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01004", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01006", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01007", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01S02", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01S07", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "07002", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "07006", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "07007", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "07S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "21S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "21S02", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22002", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22003", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22007", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22008", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22012", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22015", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22018", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22019", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22025", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "23000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "24000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "34000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "3D000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "3F000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "40001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "40003", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "42000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "42S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "42S02", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "42S11", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "42S12", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "42S21", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "42S22", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "44000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY008", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY009", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        // Only the `TextLength` sentence is marked. The three that follow it
        // describe a parameter bound by `SQLBindParameter`, and are the
        // driver's.
        DiagnosticsRow { sqlstate: "HY090", dm: DmMarking::Split("parameter length value") },
        DiagnosticsRow { sqlstate: "HY105", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY109", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYC00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM017", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM018", dm: DmMarking::None },
    ],
},
FunctionDiagnostics {
    func: "sql_prepare_w",
    odbc_name: "SQLPrepare",
    module: "src/ffi/execute.rs",
    source: EXECUTE_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlprepare-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01S02", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "21S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "21S02", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22018", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22019", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22025", dm: DmMarking::None },
        // "(DM) A cursor was open ... and SQLFetch or SQLFetchScroll had been
        // called. A cursor was open ... but SQLFetch or SQLFetchScroll had not
        // been called." The second sentence is unmarked and is the driver's.
        DiagnosticsRow { sqlstate: "24000", dm: DmMarking::Split("had not been called") },
        DiagnosticsRow { sqlstate: "34000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "3D000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "3F000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "42000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "42S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "42S02", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "42S11", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "42S12", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "42S21", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "42S22", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY008", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY009", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        // Unlike SQLExecDirect's, this row has the `TextLength` clause and
        // nothing else, so the whole row is the Driver Manager's.
        DiagnosticsRow { sqlstate: "HY090", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYC00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM017", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM018", dm: DmMarking::None },
    ],
},
FunctionDiagnostics {
    func: "sql_execute",
    odbc_name: "SQLExecute",
    module: "src/ffi/execute.rs",
    source: EXECUTE_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlexecute-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01003", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01004", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01006", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01007", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01S02", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01S07", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "07002", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "07006", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "07007", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "07S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "21S02", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22002", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22003", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22007", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22008", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22012", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22015", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22018", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22019", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22025", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "23000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "24000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "40001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "40003", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "42000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "44000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY008", dm: DmMarking::None },
        // This row has a fifth clause its two siblings lack, and it carries the
        // marker too: "(DM) The StatementHandle was not prepared."
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        // No `TextLength` clause here, so nothing in this row is marked.
        DiagnosticsRow { sqlstate: "HY090", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY105", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY109", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYC00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM017", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM018", dm: DmMarking::None },
    ],
},
FunctionDiagnostics {
    func: "sql_fetch",
    odbc_name: "SQLFetch",
    module: "src/ffi/fetch.rs",
    source: FETCH_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlfetch-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01004", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01S07", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "07006", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "07009", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22002", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22003", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22007", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22012", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22015", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22018", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "24000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "40001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "40003", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY008", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY090", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY107", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYC00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM017", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM018", dm: DmMarking::None },
    ],
},
FunctionDiagnostics {
    func: "sql_fetch_scroll",
    odbc_name: "SQLFetchScroll",
    module: "src/ffi/fetch.rs",
    source: FETCH_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlfetchscroll-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01004", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01S06", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01S07", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "07006", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "07009", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22002", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22003", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22007", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22012", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22015", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22018", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "24000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "40001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "40003", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY008", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY090", dm: DmMarking::None },
        // The invalid-value and bookmark clauses are marked; the two that name
        // a forward-only or non-scrollable cursor are not, and they are the
        // ones core acts on.
        DiagnosticsRow { sqlstate: "HY106", dm: DmMarking::Split("sql_cursor_forward_only") },
        DiagnosticsRow { sqlstate: "HY107", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY111", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYC00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM017", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM018", dm: DmMarking::None },
    ],
},
FunctionDiagnostics {
    func: "sql_extended_fetch",
    odbc_name: "SQLExtendedFetch",
    module: "src/ffi/fetch.rs",
    source: FETCH_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlextendedfetch-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01004", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01S06", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01S07", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "07006", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "07009", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22002", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22003", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22007", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22012", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22015", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22018", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "24000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY008", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY106", dm: DmMarking::Split("sql_cursor_forward_only") },
        DiagnosticsRow { sqlstate: "HY107", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY111", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYC00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
    ],
},
FunctionDiagnostics {
    func: "sql_get_data",
    odbc_name: "SQLGetData",
    module: "src/ffi/fetch.rs",
    source: FETCH_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlgetdata-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01004", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01S07", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "07006", dm: DmMarking::None },
        // The first three clauses are unmarked and are the driver's; the five
        // that follow describe the SQL_GETDATA_EXTENSIONS restrictions and are
        // all marked.
        DiagnosticsRow { sqlstate: "07009", dm: DmMarking::Split("greater than the number of columns") },
        DiagnosticsRow { sqlstate: "08S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22002", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22003", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22007", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22012", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22015", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22018", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "24000", dm: DmMarking::Split("before the start of the result set") },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY003", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY008", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY009", dm: DmMarking::All },
        // Five of the six clauses are `(DM)`-marked; the last is not. That one
        // is a SQL_PARAM_DATA_AVAILABLE result read with SQLGetData instead of
        // SQLParamData.
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::Split("SQL_PARAM_DATA_AVAILABLE") },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        // Two clauses: the negative-BufferLength one is marked, the ODBC 2.x
        // bookmark one is not.
        DiagnosticsRow { sqlstate: "HY090", dm: DmMarking::Split("less than 4") },
        DiagnosticsRow { sqlstate: "HY109", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYC00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM017", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM018", dm: DmMarking::None },
    ],
},
FunctionDiagnostics {
    func: "sql_bind_parameter",
    odbc_name: "SQLBindParameter",
    module: "src/ffi/params.rs",
    source: PARAMS_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlbindparameter-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "07006", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "07009", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY003", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY004", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY009", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY021", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY090", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY104", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY105", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYC00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
    ],
},
FunctionDiagnostics {
    func: "sql_num_params",
    odbc_name: "SQLNumParams",
    module: "src/ffi/params.rs",
    source: PARAMS_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlnumparams-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY008", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM017", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM018", dm: DmMarking::None },
    ],
},
FunctionDiagnostics {
    func: "sql_describe_param",
    odbc_name: "SQLDescribeParam",
    module: "src/ffi/params.rs",
    source: PARAMS_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqldescribeparam-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        // Only the "less than 1" sentence is marked. The three that follow it
        // are the driver's.
        DiagnosticsRow { sqlstate: "07009", dm: DmMarking::Split("greater than the number of parameters") },
        DiagnosticsRow { sqlstate: "08S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "21S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY008", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM017", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM018", dm: DmMarking::None },
    ],
},
FunctionDiagnostics {
    func: "sql_put_data",
    odbc_name: "SQLPutData",
    module: "src/ffi/params.rs",
    source: PARAMS_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlputdata-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01004", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "07006", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "07S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22003", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22007", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22008", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22012", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22015", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22018", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY008", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY009", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY019", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY020", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY090", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM017", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM018", dm: DmMarking::None },
    ],
},
FunctionDiagnostics {
    func: "sql_param_data",
    odbc_name: "SQLParamData",
    module: "src/ffi/params.rs",
    source: PARAMS_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlparamdata-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "07006", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22026", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "40001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "40003", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY008", dm: DmMarking::None },
        // Three of the five clauses are marked; the "previous call was
        // SQLParamData" and cancelled-data-at-execution ones are not.
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::Split("sqlcancel was called before data was sent") },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM017", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM018", dm: DmMarking::None },
    ],
},
FunctionDiagnostics {
    func: "sql_tables_w",
    odbc_name: "SQLTables",
    module: "src/ffi/metadata.rs",
    source: METADATA_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqltables-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "24000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "40001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "40003", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY008", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY009", dm: DmMarking::Split("sql_catalog_name") },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY090", dm: DmMarking::Split("maximum length") },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYC00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM017", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM018", dm: DmMarking::None },
    ],
},
FunctionDiagnostics {
    func: "sql_columns_w",
    odbc_name: "SQLColumns",
    module: "src/ffi/metadata.rs",
    source: METADATA_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlcolumns-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "24000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "40001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "40003", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY008", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY009", dm: DmMarking::Split("sql_catalog_name") },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY090", dm: DmMarking::Split("maximum length") },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYC00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM017", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM018", dm: DmMarking::None },
    ],
},
FunctionDiagnostics {
    func: "sql_primary_keys_w",
    odbc_name: "SQLPrimaryKeys",
    module: "src/ffi/metadata.rs",
    source: METADATA_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlprimarykeys-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "24000", dm: DmMarking::Split("had not been called") },
        DiagnosticsRow { sqlstate: "40001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "40003", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY008", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY009", dm: DmMarking::Split("sql_catalog_name") },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY090", dm: DmMarking::Split("maximum length") },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYC00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM017", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM018", dm: DmMarking::None },
    ],
},
FunctionDiagnostics {
    func: "sql_foreign_keys_w",
    odbc_name: "SQLForeignKeys",
    module: "src/ffi/metadata.rs",
    source: METADATA_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlforeignkeys-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "24000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "40001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "40003", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY008", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY009", dm: DmMarking::Split("sql_catalog_name") },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY090", dm: DmMarking::Split("maximum length") },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYC00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM017", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM018", dm: DmMarking::None },
    ],
},
FunctionDiagnostics {
    func: "sql_statistics_w",
    odbc_name: "SQLStatistics",
    module: "src/ffi/metadata.rs",
    source: METADATA_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlstatistics-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "24000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "40001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "40003", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY008", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY009", dm: DmMarking::Split("sql_catalog_name") },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY090", dm: DmMarking::Split("maximum length") },
        DiagnosticsRow { sqlstate: "HY100", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY101", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYC00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM017", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM018", dm: DmMarking::None },
    ],
},
FunctionDiagnostics {
    func: "sql_special_columns_w",
    odbc_name: "SQLSpecialColumns",
    module: "src/ffi/metadata.rs",
    source: METADATA_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlspecialcolumns-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "24000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "40001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "40003", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY008", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY009", dm: DmMarking::Split("sql_catalog_name") },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY090", dm: DmMarking::Split("maximum length") },
        DiagnosticsRow { sqlstate: "HY097", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY098", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY099", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYC00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM017", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM018", dm: DmMarking::None },
    ],
},
FunctionDiagnostics {
    func: "sql_describe_col_w",
    odbc_name: "SQLDescribeCol",
    module: "src/ffi/metadata.rs",
    source: METADATA_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqldescribecol-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01004", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "07005", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "07009", dm: DmMarking::Split("greater than the number of columns") },
        DiagnosticsRow { sqlstate: "08S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY008", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY090", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM017", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM018", dm: DmMarking::None },
    ],
},
FunctionDiagnostics {
    func: "sql_col_attribute_w",
    odbc_name: "SQLColAttribute",
    module: "src/ffi/metadata.rs",
    source: METADATA_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlcolattribute-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01004", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "07005", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "07009", dm: DmMarking::Split("greater than the number of columns") },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY008", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY090", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY091", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYC00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM017", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM018", dm: DmMarking::None },
    ],
},
FunctionDiagnostics {
    func: "sql_procedures_w",
    odbc_name: "SQLProcedures",
    module: "src/ffi/metadata.rs",
    source: METADATA_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlprocedures-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "24000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "40001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "40003", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY008", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY009", dm: DmMarking::Split("sql_catalog_name") },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY090", dm: DmMarking::Split("maximum length") },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYC00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM017", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM018", dm: DmMarking::None },
    ],
},
FunctionDiagnostics {
    func: "sql_procedure_columns_w",
    odbc_name: "SQLProcedureColumns",
    module: "src/ffi/metadata.rs",
    source: METADATA_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlprocedurecolumns-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "24000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "40001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "40003", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY008", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY009", dm: DmMarking::Split("sql_catalog_name") },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY090", dm: DmMarking::Split("maximum length") },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYC00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM017", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM018", dm: DmMarking::None },
    ],
},
FunctionDiagnostics {
    func: "sql_column_privileges_w",
    odbc_name: "SQLColumnPrivileges",
    module: "src/ffi/metadata.rs",
    source: METADATA_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlcolumnprivileges-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "24000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "40001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "40003", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY008", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY009", dm: DmMarking::Split("sql_catalog_name") },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY090", dm: DmMarking::Split("maximum length") },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYC00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM017", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM018", dm: DmMarking::None },
    ],
},
FunctionDiagnostics {
    func: "sql_table_privileges_w",
    odbc_name: "SQLTablePrivileges",
    module: "src/ffi/metadata.rs",
    source: METADATA_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqltableprivileges-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "24000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "40001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "40003", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY008", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY009", dm: DmMarking::Split("sql_catalog_name") },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY090", dm: DmMarking::Split("maximum length") },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYC00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM017", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM018", dm: DmMarking::None },
    ],
},
FunctionDiagnostics {
    func: "sql_num_result_cols",
    odbc_name: "SQLNumResultCols",
    module: "src/ffi/cursor.rs",
    source: CURSOR_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlnumresultcols-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY008", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM017", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM018", dm: DmMarking::None },
    ],
},
FunctionDiagnostics {
    func: "sql_row_count",
    odbc_name: "SQLRowCount",
    module: "src/ffi/cursor.rs",
    source: CURSOR_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlrowcount-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
    ],
},
FunctionDiagnostics {
    func: "sql_more_results",
    odbc_name: "SQLMoreResults",
    module: "src/ffi/cursor.rs",
    source: CURSOR_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlmoreresults-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01S02", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "40001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "40003", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY008", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM017", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM018", dm: DmMarking::None },
    ],
},
FunctionDiagnostics {
    func: "sql_close_cursor",
    odbc_name: "SQLCloseCursor",
    module: "src/ffi/cursor.rs",
    source: CURSOR_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlclosecursor-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "24000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
    ],
},
FunctionDiagnostics {
    func: "sql_cancel",
    odbc_name: "SQLCancel",
    module: "src/ffi/cursor.rs",
    source: CURSOR_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlcancel-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY018", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
    ],
},
FunctionDiagnostics {
    func: "sql_get_cursor_name_w",
    odbc_name: "SQLGetCursorName",
    module: "src/ffi/cursor.rs",
    source: CURSOR_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlgetcursorname-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01004", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY015", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY090", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
    ],
},
FunctionDiagnostics {
    func: "sql_set_cursor_name_w",
    odbc_name: "SQLSetCursorName",
    module: "src/ffi/cursor.rs",
    source: CURSOR_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlsetcursorname-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01004", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "24000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "34000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "3C000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY009", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY090", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
    ],
},
FunctionDiagnostics {
    func: "sql_bulk_operations",
    odbc_name: "SQLBulkOperations",
    module: "src/ffi/cursor.rs",
    source: CURSOR_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlbulkoperations-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01004", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01S07", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "07006", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "07009", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "21S02", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22003", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22007", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22008", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22015", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22018", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "23000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "24000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "40001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "40003", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "42000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "44000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY008", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY011", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY090", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY092", dm: DmMarking::Split("concur_read_only") },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYC00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM017", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM018", dm: DmMarking::None },
    ],
},
FunctionDiagnostics {
    func: "sql_set_pos",
    odbc_name: "SQLSetPos",
    module: "src/ffi/cursor.rs",
    source: CURSOR_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlsetpos-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01004", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01S07", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "07006", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "07009", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "21S02", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22003", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22007", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22008", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22015", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22018", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "23000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "24000", dm: DmMarking::Split("before the start of the result set") },
        DiagnosticsRow { sqlstate: "40001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "40003", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "42000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "44000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY008", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY011", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY090", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY092", dm: DmMarking::Split("concur_read_only") },
        DiagnosticsRow { sqlstate: "HY107", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY109", dm: DmMarking::Split("forward-only") },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYC00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM017", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM018", dm: DmMarking::None },
    ],
},
FunctionDiagnostics {
    func: "sql_driver_connect_w",
    odbc_name: "SQLDriverConnect",
    module: "src/ffi/connect.rs",
    source: CONNECT_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqldriverconnect-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01004", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01S00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01S02", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01S08", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01S09", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "08001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08002", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "08004", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "28000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::Split("no specific sqlstate") },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY008", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY090", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY092", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY110", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYC00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM002", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM003", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM004", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM005", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM006", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM007", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM008", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM009", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM011", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM012", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM014", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM015", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM017", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM018", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "S1118", dm: DmMarking::None },
    ],
},
FunctionDiagnostics {
    func: "sql_connect_w",
    odbc_name: "SQLConnect",
    module: "src/ffi/connect.rs",
    source: CONNECT_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlconnect-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01S02", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08002", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "08004", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "28000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY008", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY090", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY114", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYT00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM002", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM003", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM004", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM005", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM006", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM009", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM014", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM015", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM017", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM018", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "S1118", dm: DmMarking::None },
    ],
},
FunctionDiagnostics {
    func: "sql_browse_connect_w",
    odbc_name: "SQLBrowseConnect",
    module: "src/ffi/connect.rs",
    source: CONNECT_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlbrowseconnect-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01004", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01S00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01S02", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08002", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "08004", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "28000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY008", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY090", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY114", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYT00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM002", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM003", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM004", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM005", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM006", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM009", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM011", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM012", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM014", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM017", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM018", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "S1118", dm: DmMarking::None },
    ],
},
FunctionDiagnostics {
    func: "sql_disconnect",
    odbc_name: "SQLDisconnect",
    module: "src/ffi/connect.rs",
    source: CONNECT_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqldisconnect-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01002", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08003", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "25000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY008", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM017", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM018", dm: DmMarking::None },
    ],
},
FunctionDiagnostics {
    func: "sql_native_sql_w",
    odbc_name: "SQLNativeSql",
    module: "src/ffi/connect.rs",
    source: CONNECT_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlnativesql-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01004", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08003", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22007", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "24000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY009", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY090", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY109", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
    ],
},
FunctionDiagnostics {
    func: "sql_alloc_handle",
    odbc_name: "SQLAllocHandle",
    module: "src/ffi/handle.rs",
    source: HANDLE_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlallochandle-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08003", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::Split("the driver was unable to allocate memory") },
        DiagnosticsRow { sqlstate: "HY009", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY014", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY092", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYC00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
    ],
},
FunctionDiagnostics {
    func: "sql_free_handle",
    odbc_name: "SQLFreeHandle",
    module: "src/ffi/handle.rs",
    source: HANDLE_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlfreehandle-function>
    rows: &[
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY017", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
    ],
},
FunctionDiagnostics {
    func: "sql_free_stmt",
    odbc_name: "SQLFreeStmt",
    module: "src/ffi/handle.rs",
    source: HANDLE_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlfreestmt-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY092", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
    ],
},
FunctionDiagnostics {
    func: "sql_get_info_w",
    odbc_name: "SQLGetInfo",
    module: "src/ffi/info.rs",
    source: INFO_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlgetinfo-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01004", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08003", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "08S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY024", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY090", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY096", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYC00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
    ],
},
FunctionDiagnostics {
    func: "sql_get_type_info",
    odbc_name: "SQLGetTypeInfo",
    module: "src/ffi/info.rs",
    source: INFO_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlgettypeinfo-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01S02", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "24000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "40001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "40003", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY004", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY008", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYC00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM017", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM018", dm: DmMarking::None },
    ],
},
FunctionDiagnostics {
    func: "sql_get_functions",
    odbc_name: "SQLGetFunctions",
    module: "src/ffi/info.rs",
    source: INFO_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlgetfunctions-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY095", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
    ],
},
FunctionDiagnostics {
    func: "sql_set_env_attr",
    odbc_name: "SQLSetEnvAttr",
    module: "src/ffi/env.rs",
    source: ENV_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlsetenvattr-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01S02", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY009", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY024", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY090", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY092", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYC00", dm: DmMarking::Split("valid odbc environment attribute") },
    ],
},
FunctionDiagnostics {
    func: "sql_get_env_attr",
    odbc_name: "SQLGetEnvAttr",
    module: "src/ffi/env.rs",
    source: ENV_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlgetenvattr-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01004", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY092", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYC00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
    ],
},
FunctionDiagnostics {
    func: "sql_end_tran",
    odbc_name: "SQLEndTran",
    module: "src/ffi/tran.rs",
    source: TRAN_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlendtran-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08003", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "08007", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "25S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "25S02", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "25S03", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "40001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "40002", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY008", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY012", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY092", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY115", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYC00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM017", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM018", dm: DmMarking::None },
    ],
},
FunctionDiagnostics {
    func: "sql_get_desc_field_w",
    odbc_name: "SQLGetDescField",
    module: "src/ffi/desc.rs",
    source: DESC_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlgetdescfield-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01004", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "07009", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "08S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY007", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY021", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY090", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY091", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
    ],
},
FunctionDiagnostics {
    func: "sql_set_desc_field_w",
    odbc_name: "SQLSetDescField",
    module: "src/ffi/desc.rs",
    source: DESC_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlsetdescfield-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01S02", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "07009", dm: DmMarking::Split("referred to an ard or an apd") },
        DiagnosticsRow { sqlstate: "08S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "22001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY016", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY021", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY090", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY091", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY092", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY105", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
    ],
},
FunctionDiagnostics {
    func: "sql_get_desc_rec_w",
    odbc_name: "SQLGetDescRec",
    module: "src/ffi/desc.rs",
    source: DESC_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlgetdescrec-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01004", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "07009", dm: DmMarking::Split("record field") },
        DiagnosticsRow { sqlstate: "08S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY007", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
    ],
},
FunctionDiagnostics {
    func: "sql_set_desc_rec",
    odbc_name: "SQLSetDescRec",
    module: "src/ffi/desc.rs",
    source: DESC_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlsetdescrec-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "07009", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY016", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY021", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY090", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
    ],
},
FunctionDiagnostics {
    func: "sql_copy_desc",
    odbc_name: "SQLCopyDesc",
    module: "src/ffi/desc.rs",
    source: DESC_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlcopydesc-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY007", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY016", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY021", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY092", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
    ],
},
FunctionDiagnostics {
    func: "sql_set_connect_attr_w",
    odbc_name: "SQLSetConnectAttr",
    module: "src/ffi/connect_attr.rs",
    source: CONNECT_ATTR_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlsetconnectattr-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01S02", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08002", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08003", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "08S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "24000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "25000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "3D000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY008", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY009", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY011", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY024", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY090", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY092", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY114", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY121", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYC00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "IM009", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM017", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM018", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "S1118", dm: DmMarking::None },
    ],
},
FunctionDiagnostics {
    func: "sql_get_connect_attr_w",
    odbc_name: "SQLGetConnectAttr",
    module: "src/ffi/connect_attr.rs",
    source: CONNECT_ATTR_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlgetconnectattr-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01004", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08003", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "08S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY090", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY092", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY114", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYC00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
    ],
},
FunctionDiagnostics {
    func: "sql_set_stmt_attr_w",
    odbc_name: "SQLSetStmtAttr",
    module: "src/ffi/stmt_attr.rs",
    source: STMT_ATTR_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlsetstmtattr-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01S02", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "08S01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "24000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY009", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY011", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY017", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY024", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY090", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY092", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYC00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "S1118", dm: DmMarking::None },
    ],
},
FunctionDiagnostics {
    func: "sql_get_stmt_attr_w",
    odbc_name: "SQLGetStmtAttr",
    module: "src/ffi/stmt_attr.rs",
    source: STMT_ATTR_RS,
    // <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlgetstmtattr-function>
    rows: &[
        DiagnosticsRow { sqlstate: "01000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "01004", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "24000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY000", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY001", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY010", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY013", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY090", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HY092", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY109", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HY117", dm: DmMarking::All },
        DiagnosticsRow { sqlstate: "HYC00", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "HYT01", dm: DmMarking::None },
        DiagnosticsRow { sqlstate: "IM001", dm: DmMarking::All },
    ],
},
];

#[test]
fn the_transcription_is_well_formed() {
    let mut problems: Vec<String> = Vec::new();
    let mut seen: Vec<&str> = Vec::new();

    for entry in DIAGNOSTICS_TABLES {
        if seen.contains(&entry.func) {
            problems.push(format!("{} is transcribed twice", entry.func));
        }
        seen.push(entry.func);

        for (i, row) in entry.rows.iter().enumerate() {
            if !is_sqlstate(row.sqlstate) {
                problems.push(format!(
                    "{}: {:?} is not a five-character SQLSTATE",
                    entry.func, row.sqlstate
                ));
            }
            if i > 0 && entry.rows[i - 1].sqlstate >= row.sqlstate {
                problems.push(format!(
                    "{}: rows must be sorted and unique; {:?} follows {:?}",
                    entry.func,
                    row.sqlstate,
                    entry.rows[i - 1].sqlstate
                ));
            }
        }
    }

    assert!(
        problems.is_empty(),
        "the transcribed tables are malformed:\n{}",
        problems.join("\n")
    );
}

/// The guard. Compares each FFI function's `# Spec compliance` list against the
/// spec's own Diagnostics table, transcribed above. See the module docs for the
/// three properties it proves and the one it does not.
///
/// Skipped under Miri, on the same grounds as
/// `escape::tests::pathological_nesting_returns_an_error_rather_than_killing_the_process`:
/// the cost is algorithmic rather than memory-safety-related. This module holds
/// no `unsafe` at all, and it scans the `include_str!`'d FFI source above as
/// `&'static str`, so Miri has nothing here to check. Interpreting that scan
/// byte by byte dominates a Miri run instead.
#[cfg_attr(
    miri,
    ignore = "1.86 MB string scan; no unsafe in this module for Miri to check"
)]
#[test]
fn every_doc_comment_matches_the_spec_diagnostics_table() {
    let mut problems: Vec<String> = Vec::new();

    for entry in DIAGNOSTICS_TABLES {
        let doc = doc_lines(entry.source, entry.func);
        let bullets = spec_compliance_bullets(&doc);
        let where_ = format!("{} ({}, {})", entry.func, entry.odbc_name, entry.module);

        if bullets.is_empty() {
            problems.push(format!("{where_}: no `# Spec compliance` bullets found"));
            continue;
        }

        // Property 1: every spec row appears.
        for row in entry.rows {
            let covered = bullets
                .iter()
                .any(|b| b.states.iter().any(|s| s.as_str() == row.sqlstate));
            if !covered {
                problems.push(format!(
                    "{where_}: the spec's table has {} and the doc comment does not \
                     mention it",
                    row.sqlstate
                ));
            }
        }

        for bullet in &bullets {
            for state in &bullet.states {
                let Some(row) = entry.rows.iter().find(|r| r.sqlstate == *state) else {
                    // Property 2: an off-table claim has to say so.
                    if !bullet.text.to_lowercase().contains(OFF_TABLE) {
                        problems.push(format!(
                            "{where_}: the doc comment lists {state}, which is not in \
                             this function's diagnostics table. If the driver really \
                             returns it, say `**{OFF_TABLE}**` and why."
                        ));
                    }
                    continue;
                };

                let claims_dm = claims_driver_manager(&bullet.text);
                match row.dm {
                    // Property 3a: no (DM) marker, so no (DM) claim.
                    DmMarking::None if claims_dm => problems.push(format!(
                        "{where_}: {state} is attributed to the Driver Manager, but its \
                         row carries no `(DM)` marker. Give the driver-side reason it \
                         is not returned instead."
                    )),
                    // Property 3b: fully (DM), so say so, even when core checks
                    // it anyway, which is what "guarded defensively" is for.
                    DmMarking::All if !claims_dm => problems.push(format!(
                        "{where_}: every clause of {state} carries `(DM)`, and the doc \
                         comment presents it as an ordinary driver row. If core checks \
                         it anyway, say the spec annotates it `(DM)` and it is guarded \
                         defensively here."
                    )),
                    // Property 3c: a split row has to name which half.
                    DmMarking::Split(unmarked) => {
                        if !claims_dm {
                            problems.push(format!(
                                "{where_}: {state} has a `(DM)`-marked clause the doc \
                                 comment does not acknowledge."
                            ));
                        }
                        // Both sides lowercased. A raw needle against a
                        // lowercased haystack silently never matches when the
                        // needle carries a capital, as `SQL_PARAM_DATA_AVAILABLE`
                        // does, and the guard would then report a defect no edit
                        // could clear.
                        if !bullet
                            .text
                            .to_lowercase()
                            .contains(&unmarked.to_lowercase())
                        {
                            problems.push(format!(
                                "{where_}: {state} is marked `(DM)` on only some of its \
                                 clauses. The doc comment must name the unmarked one, \
                                 which the spec words around {unmarked:?}."
                            ));
                        }
                    }
                    DmMarking::None | DmMarking::All => {}
                }
            }
        }
    }

    assert!(
        problems.is_empty(),
        "{} doc-comment defect(s) against the spec's diagnostics tables:\n{}",
        problems.len(),
        problems.join("\n")
    );
}

/// Skipped under Miri for the same reason as the guard above: it scans the same
/// `include_str!`'d source for `pub unsafe fn` lines, which is expensive to
/// interpret, and there is no `unsafe` in this module for Miri to check.
#[cfg_attr(
    miri,
    ignore = "1.86 MB string scan; no unsafe in this module for Miri to check"
)]
#[test]
fn every_exported_ffi_function_has_a_transcribed_diagnostics_table() {
    let mut missing: Vec<String> = Vec::new();

    for (module, source) in MODULES {
        for line in source.lines() {
            let Some(rest) = line.strip_prefix("pub unsafe fn ") else {
                continue;
            };
            let func: &str = rest.split(['<', '(']).next().unwrap_or(rest);
            if NO_SPEC_DIAGNOSTICS_TABLE.contains(&func)
                || DIAGNOSTICS_TABLES.iter().any(|e| e.func == func)
            {
                continue;
            }
            missing.push(format!("{module}: {func}"));
        }
    }

    assert!(
        missing.is_empty(),
        "these FFI entry points have no transcribed diagnostics table, so nothing \
         checks their doc comment:\n{}",
        missing.join("\n")
    );
}

/// The sibling guard, in the other direction: a SQLSTATE the function's **body**
/// returns must be either in its transcribed table or declared off-table in the
/// doc comment.
///
/// [`every_doc_comment_matches_the_spec_diagnostics_table`] reads the doc comment
/// and asks whether the spec agrees. That leaves a blind spot it cannot see: a
/// state the code returns and the doc comment never mentions passes both of its
/// first two properties, because there is no bullet to check. The shape to
/// catch is `SQLBindParameter` answering `HY024` for an unrecognised
/// `InputOutputType` while its own table lists `HY105` and no `HY024` at all,
/// undocumented either way.
///
/// # What it can and cannot see
///
/// It scans for literal `SqlState::factory()` calls between a function's
/// signature and its closing brace, so it is an **under**-approximation by
/// design. It misses a state produced inside a helper the function calls, one
/// carried by an [`crate::errors::OdbcError`] variant such as
/// `FractionalTruncation`, and one propagated from a backend. Those are why the
/// doc comments carry prose reasons a test cannot check. What it does catch is
/// a factory called at the entry point itself, naming a state nobody reconciled
/// against the page.
///
/// Skipped under Miri for the same reason as its two neighbours: it scans the
/// same `include_str!`'d source and this module holds no `unsafe`.
#[cfg_attr(
    miri,
    ignore = "1.86 MB string scan; no unsafe in this module for Miri to check"
)]
#[test]
fn every_sqlstate_a_function_body_returns_is_in_its_table_or_declared_off_table() {
    let factories = sql_state_factories();
    assert!(
        factories.len() > 20,
        "the factory scan found only {} entries, so it has stopped matching \
         sql_state.rs's shape and this guard is passing vacuously",
        factories.len()
    );

    let mut problems: Vec<String> = Vec::new();
    let mut observed = 0usize;

    for entry in DIAGNOSTICS_TABLES {
        let body = body_of(entry.source, entry.func);
        let bullets = spec_compliance_bullets(&doc_lines(entry.source, entry.func));
        let where_ = format!("{} ({}, {})", entry.func, entry.odbc_name, entry.module);

        let mut reported: Vec<&str> = Vec::new();
        for (call, state) in &factories {
            if !body.contains(call.as_str()) || reported.contains(state) {
                continue;
            }
            reported.push(state);
            observed += 1;

            if entry.rows.iter().any(|r| r.sqlstate == *state) {
                continue;
            }
            let declared = bullets.iter().any(|b| {
                b.states.iter().any(|s| s == state) && b.text.to_lowercase().contains(OFF_TABLE)
            });
            if !declared {
                problems.push(format!(
                    "{where_}: the body calls {call}, so it can return {state}, which is \
                     not in this function's diagnostics table. Either return the state the \
                     table does list, or document it with `**{OFF_TABLE}**` and why."
                ));
            }
        }
    }

    // The other way this guard could pass without proving anything: `body_of`
    // returning nothing useful, because a signature or a closing brace moved.
    // 90-odd (function, state) pairs are visible today; the floor is well under
    // that and still far from zero.
    assert!(
        observed > 60,
        "the body scan observed only {observed} (function, SQLSTATE) pairs, so \
         `body_of` has stopped finding function bodies and this guard is passing \
         vacuously"
    );

    assert!(
        problems.is_empty(),
        "these functions return a SQLSTATE their own spec table does not list, \
         without saying so:\n{}",
        problems.join("\n")
    );
}
