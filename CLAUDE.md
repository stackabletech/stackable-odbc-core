# Project Rules

Read and follow @AGENTS.md — it contains architecture, patterns, and procedures.

## Non-Negotiable Rules

- **ODBC spec compliance is mandatory.** Read the spec page for every function you implement, modify, or audit. Never claim a SQLSTATE is missing or wrong without checking the actual spec diagnostics table first. Pay attention to **(DM)** annotations — those SQLSTATEs are returned by the Driver Manager, not the driver; do not add driver-side checks for DM-only codes. Every FFI function's doc comment must list all SQLSTATEs from the spec diagnostics table — for each one, note whether the driver returns it or why not (e.g., "(driver-manager-handled; not returned here)"). When touching a function, verify its doc comment is complete and accurate against the spec. See AGENTS.md "Adding a new ODBC function" for the full checklist.
- **Use `odbc-sys` types** — never redefine enums, structs, or constants it already provides.
- **Convert raw integers to typed enums at the FFI boundary** — use `xxx_from_raw()` functions, never `transmute`.
- **Run `pre-commit run --all-files`** before every commit. This is the single source of truth for what must pass.

## Scope

- Do not modify files outside the scope of the current task.
- Do not add features, refactoring, or "improvements" beyond what was asked.
- If unsure whether something is in scope, ask.
