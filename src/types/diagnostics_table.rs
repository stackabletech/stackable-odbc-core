//! The ODBC spec's per-function Diagnostics table, transcribed as code.
//!
//! Every function page under
//! <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/> carries a
//! "Diagnostics" table: one row per SQLSTATE, with some rows — or some
//! *clauses* of a row — annotated `(DM)` to mean the Driver Manager returns
//! it, not the driver. `CLAUDE.md` makes reproducing that table in each FFI
//! function's doc comment non-negotiable. This module is the machine-checkable
//! half of that rule, in the same spirit as [`crate::types::info_type_shape`]:
//! the spec table transcribed once, and a test that compares it against what
//! the source actually says.
//!
//! # Why this exists
//!
//! An audit of all sixty exported functions found roughly forty doc-comment
//! defects sharing one root cause: the `(DM)` annotations were never checked
//! against the spec's own table. They ran in both directions.
//!
//! - Rows with **no** `(DM)` marker were written off as
//!   "(driver-manager-handled; not returned here)" — `01000`, `HY001`,
//!   `HY013` and `HYT01` across the binding and parameter functions, `IM017`
//!   and `IM018` across five metadata functions. Each is a real question about
//!   this driver that the annotation declined to answer.
//! - Rows the spec **does** mark `(DM)` were presented as ordinary driver
//!   rows, so a reader could not tell a spec-required check from a defensive
//!   one — `SQLFreeHandle`'s `HY010`, `SQLNumResultCols`' `HY010`,
//!   `SQLEndTran`'s `08003`, and six more.
//! - One row, `HY090` in the twelve catalog functions, carries `(DM)` on its
//!   first sentence and not on its second. Twelve doc comments read the row as
//!   wholly the Driver Manager's and gave the wrong reason for a correct
//!   conclusion.
//!
//! Prose cannot be trusted to stay right through forty hand edits, so the
//! `(DM)` attribution and the row set are pinned by
//! [`every_doc_comment_matches_the_spec_diagnostics_table`] instead.
//!
//! # What the guard does and does not prove
//!
//! It proves three things, and only these three:
//!
//! 1. Every row in the spec's table appears in the doc comment.
//! 2. Every SQLSTATE the doc comment names is in the spec's table, unless the
//!    bullet says `absent from this function's diagnostics table` — the house
//!    phrase, already used for `08S01` in `sql_describe_col_w` and `3D000` in
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
//! A `(DM)` row core checks anyway is not a spec violation: `SQLAllocHandle`'s
//! `HY009` and `HY092` have been guarded that way since the beginning, because
//! core is also linked directly — by its own tests, and by an embedder with no
//! Driver Manager in front of it — and because several of those checks are
//! load-bearing for memory safety rather than for the spec.

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
/// where the audit found the subtlest errors: `SQLTables`' `HY090` reads
/// "(DM) The value of one of the name length arguments was less than 0 but not
/// equal to SQL_NTS. The value of one of the name length arguments exceeded the
/// maximum length value for the corresponding name." — one marker, two
/// sentences, and only the first is covered.
enum DmMarking {
    /// No clause of this row carries `(DM)`. A doc comment calling it the
    /// Driver Manager's is wrong.
    None,
    /// Every clause carries `(DM)`. A doc comment presenting it as an ordinary
    /// driver row is wrong, even when core does implement the check — say it is
    /// guarded defensively instead.
    All,
    /// Some clauses carry `(DM)` and some do not. The payload is a distinctive
    /// phrase from an **unmarked** clause; a doc comment for this row has to
    /// quote it, so that it says *which* half it is talking about rather than
    /// generalising the row away.
    ///
    /// The first table to carry one is the catalog functions' `HY090`. Until
    /// that one is transcribed the variant is read but never built, and the
    /// `expect` below turns the day it is built into a prompt to delete the
    /// line rather than a warning to explain away.
    #[expect(dead_code, reason = "no transcribed table has a split row yet")]
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
    // ConfigDSN is an ODBC *installer* entry point, not an ODBC function. Its
    // page defines no Diagnostics table and it has no handle to post one
    // through; it reports via SQLPostInstallerError instead, which
    // `every_false_return_from_config_dsn_w_posts_an_installer_error` in
    // `ffi/setup.rs` already guards.
    "config_dsn_w",
];

/// Functions this sweep has not reached yet. **This list only ever shrinks**,
/// and the change that empties it deletes it and the assertion that reads it.
/// It exists so the sweep can land file by file, each commit reviewable on its
/// own, without leaving the guard switched off in between.
const NOT_YET_TRANSCRIBED: &[&str] = &[
    "sql_alloc_handle",
    "sql_bind_parameter",
    "sql_browse_connect_w",
    "sql_bulk_operations",
    "sql_cancel",
    "sql_close_cursor",
    "sql_col_attribute_w",
    "sql_column_privileges_w",
    "sql_columns_w",
    "sql_connect_w",
    "sql_copy_desc",
    "sql_describe_col_w",
    "sql_describe_param",
    "sql_disconnect",
    "sql_driver_connect_w",
    "sql_end_tran",
    "sql_exec_direct_w",
    "sql_execute",
    "sql_extended_fetch",
    "sql_fetch",
    "sql_fetch_scroll",
    "sql_foreign_keys_w",
    "sql_free_handle",
    "sql_free_stmt",
    "sql_get_connect_attr_w",
    "sql_get_cursor_name_w",
    "sql_get_data",
    "sql_get_desc_field_w",
    "sql_get_desc_rec_w",
    "sql_get_diag_field_w",
    "sql_get_diag_rec_w",
    "sql_get_env_attr",
    "sql_get_functions",
    "sql_get_info_w",
    "sql_get_stmt_attr_w",
    "sql_get_type_info",
    "sql_more_results",
    "sql_native_sql_w",
    "sql_num_params",
    "sql_num_result_cols",
    "sql_param_data",
    "sql_prepare_w",
    "sql_primary_keys_w",
    "sql_procedure_columns_w",
    "sql_procedures_w",
    "sql_put_data",
    "sql_row_count",
    "sql_set_connect_attr_w",
    "sql_set_cursor_name_w",
    "sql_set_desc_field_w",
    "sql_set_desc_rec",
    "sql_set_env_attr",
    "sql_set_pos",
    "sql_set_stmt_attr_w",
    "sql_special_columns_w",
    "sql_statistics_w",
    "sql_table_privileges_w",
    "sql_tables_w",
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
/// Bullets that do not open with a SQLSTATE — `sql_alloc_handle`'s
/// "Handle-specific rules", `sql_free_handle`'s `SQL_INVALID_HANDLE` note —
/// are skipped along with their continuation lines, so their prose cannot be
/// misread as part of a neighbouring row's verdict.
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
        if in_state_bullet && !line.trim().is_empty() {
            if let Some(last) = out.last_mut() {
                last.text.push(' ');
                last.text.push_str(line.trim());
            }
        } else if line.trim().is_empty() {
            in_state_bullet = false;
        }
    }
    out
}

/// The SQLSTATE(s) a bullet opens with, if any.
///
/// Four bullet shapes are in use across `ffi/`, all of them accepted rather
/// than normalised — rewriting sixty doc comments into one shape is churn this
/// sweep does not need:
///
/// ```text
/// - 01000: General warning …
/// - `01000` General warning — …
/// - 01000 (general warning): …
/// - **01000** — General warning …
/// - IM001–IM018: All Driver Manager internal codes — …
/// ```
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
/// This is not a nicety. The corrections this guard exists to enforce say "the
/// row carries no `(DM)` marker" in as many words — that sentence is the whole
/// point of the sweep — and a naive `contains("(dm)")` would read the denial as
/// a claim and fail every bullet the sweep just got right.
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

#[rustfmt::skip]
const DIAGNOSTICS_TABLES: &[FunctionDiagnostics] = &[FunctionDiagnostics {
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
}];

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
/// three properties it proves and the one it deliberately does not.
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
                    // Property 3b: fully (DM), so say so — even when core checks
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
                        if !bullet.text.to_lowercase().contains(unmarked) {
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
                || NOT_YET_TRANSCRIBED.contains(&func)
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
