# Project Rules

Read and follow @AGENTS.md. It holds the architecture, the patterns and the
procedures.

## Non-Negotiable Rules

- **ODBC spec compliance is mandatory.** Read the spec page for every function
  you implement, modify or audit. Never claim a SQLSTATE is missing or wrong
  without checking the actual spec diagnostics table first. Pay attention to
  **(DM)** annotations: those SQLSTATEs are returned by the Driver Manager
  rather than the driver, so do not add driver-side checks for them. Every FFI
  function's doc comment must list all SQLSTATEs from the spec diagnostics
  table, and for each one note whether the driver returns it or why it does not,
  for example "(driver-manager-handled; not returned here)". When you touch a
  function, verify its doc comment is complete and accurate against the spec.
  See AGENTS.md "Adding a new ODBC function" for the full checklist.
- **When the spec is silent or ambiguous, check a mature driver before
  deciding.** Some pages state a behaviour without a diagnostics table, describe
  a Driver-Manager fallback without saying what the driver should do, or leave a
  `(DM)` marker off exactly one clause of a row. Read what the established
  drivers do. psqlODBC, MySQL Connector/ODBC, FreeTDS and unixODBC's own Driver
  Manager are open source, and the commercial ones (Amazon Redshift, Simba,
  DataDirect) publish supported-function tables. Prefer their source over their
  documentation, and prefer both over inference from the spec's silence. Record
  what you found and cite it in the doc comment, so the next reader inherits the
  evidence rather than the conclusion. **If the drivers disagree with each
  other, or with the spec, ask rather than picking one.**
- **Use `odbc-sys` types.** Never redefine an enum, struct or constant it
  already provides.
- **Convert raw integers to typed enums at the FFI boundary**, using the
  `xxx_from_raw()` functions. Never `transmute`.
- **Run `pre-commit run --all-files`** before every commit. It is the single
  source of truth for what must pass.

## Scope

- Do not modify files outside the scope of the current task.
- Do not add features, refactoring or improvements beyond what was asked.
- If unsure whether something is in scope, ask.

## Data Retrieval

Never read entire files by default. Survey, locate, then extract.

1. **Survey first.** Check file size before reading (`stat -c%s file`). Files
   over 50 KB must be sliced rather than read whole.
2. **Navigate definitions with ctags.** Run `ctags -R .` once to build a tags
   index, then `grep "^SymbolName" tags` for the exact file and line of any
   function, struct or trait, with no file reading at all.
3. **Locate with Grep.** Find patterns, keywords or usages before reading. Use
   `-C` for context lines.
4. **Extract with Read (offset + limit).** Once you know the line range, read
   only that slice.
5. **Structured data.** Use `jq` for JSON and `yq` for YAML. Never read raw
   markup whole.
6. **Filesystem survey.** Use `tree -L 2 -I '.git|target|node_modules'` rather
   than a recursive `ls`.
7. **Verify edits with diff.** After editing, run `git diff -u` to confirm the
   change instead of re-reading the file.
