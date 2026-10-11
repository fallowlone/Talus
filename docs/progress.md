# Progress

_Last updated: 2026-10-10 (session 2: UI redesign, model comparison)._

## Overall status

M0–M10 done. Remaining work is breadth (deferred engines and P2/P3 features) and depth items listed below.
The app launches, connects to real SQLite / PostgreSQL / MySQL (directly or through an SSH
tunnel), browses metadata, runs SQL with completion/format/explain/cancel, edits data
transactionally, designs tables, imports CSV, exports tables, and searches databases.

| Milestone | State |
|---|---|
| M0 Investigation + design docs | done |
| M1 Core foundation | done |
| M2 Engines SQLite / PostgreSQL / MySQL | done |
| M3 Vertical slice UI | done |
| M4 Data editor | done (cell inspector, copy as INSERT included) |
| M5 Schema management | done (incl. create/drop database) |
| M6 Productivity | done (SQL-14 manual transactions: Begin/Commit/Rollback) |
| M7 Transfer | done (streaming export CSV/TSV/JSON/SQL, CSV import wizard) |
| M8 Connectivity | done: SSH tunnel, TLS verify-full (CA file), reconnect test |
| M9 Advanced | ER diagram, structure sync (tables/columns), server monitor done |
| M10 Packaging + performance | done (`scripts/bundle-macos.sh`, perf probes) |

## UI redesign (session 2)

- Layout: full-height light sidebar (`app/chrome.rs`), top bar with ⌘K search,
  tab strip only when tabs are open, footer status line, white Information pane set off by an edge shadow.
- Home tab (`app/home.rs`): greeting, Connections, Query Activity (7-day area chart from history), Recent Queries,
  Workspace, Automation cards; with a database/schema selected it shows the objects list in a card.
- Theme (`theme.rs`): Light is the default; SF Pro semibold via the variable font's `wght` axis. White everywhere
  except the sidebar; content has no outlines and no tinted surfaces (user feedback): cards are lifted by a
  two-layer shadow (`theme::card_shadows`, `widgets::card`), groups are separated by space and type. In controls
  all neutrals are translucent ink, inputs are white with an
  ink-18 % edge, buttons/combos have a faint ink fill and edge; focus is a black ring, primary buttons are black,
  destructive ones red; the grid has no rules, stripes or header tint.
- The in-window menu bar and the big-icon toolbar were removed (native macOS menu + sidebar cover them).
- Model designer: "Compare with Selection" diffs the model against the selected schema (`DesignModel::diff`).
- Model levels (`ModelKind`): conceptual / logical / physical, converted losslessly via "Model type"; only a physical
  model generates SQL or compares with a database (enforced in core, gated in the UI).
- HTTP tunnel (`tunnel.rs`): connections can go through an HTTP proxy (`CONNECT`, optional Basic auth) instead of
  SSH; one shared `tunnel::Tunnel` type serves both. A PHP tunnel script for shared hosting is not implemented.
- PL/pgSQL debugger (`debugger.rs`, `ui/debug_tab.rs`): pldebugger-based; control + target sessions, steps raced
  against the target's completion, Stop cancels a waiting step through the target's cancel handle. The PostgreSQL
  test container is now built from `docker/postgres-debugger.Dockerfile` (postgres:17 + pldebugger, preloaded).
- Routines of PostgreSQL extensions (e.g. `pldbg_*`) are no longer listed among the user's routines.
- Security: column types and defaults (also from a *source* database in structure sync) are checked on
  dialect tokens — no top-level comma (PostgreSQL would take `SET DEFAULT 0, OWNER TO x` as a second ALTER
  action), no `USING` in types, balanced parentheses, no comments (incl. MySQL `#`), `;` or parameters. Sync
  skips such tables and any view whose definition is not exactly one statement.
- Debugger attaches in-context (`pldbg_oid_debug` → `PLDBGBREAK` notice → `pldbg_attach_to_port`): the global
  breakpoint mode leaks pldebugger's 50 shared slots (verified in its source); start retries when pldebugger's
  proxy reports an interrupted `select()`. PostgreSQL sessions forward server notices (bounded channel).

## Architecture decisions (see `architecture.md`)

- 100% Rust: `egui/eframe` 0.36, `tokio`, `rusqlite`, `tokio-postgres`, `mysql_async`,
  `russh` (ring backend) for SSH, `security-framework` for Keychain.
- `talus-core` (no GUI) / `talus-app` (egui). Export, import, DDL plans and find live in core.
- One session per editor/data tab + metadata session; one SSH tunnel per connection shared by
  its sessions, reopened after failure.
- Text cells; writes bind text parameters with server-side casts; edits apply in one
  transaction and must touch exactly one row each.
- Designer DDL is diffed; destructive steps require confirmation; MySQL `MODIFY` repeats
  `auto_increment` / `on update` / comment and re-quotes literal defaults so nothing is lost.
- SSH host keys are checked against the app's own `known_hosts`; unknown keys are refused
  unless the connection opts into trust-on-first-use; changed keys are always refused.
- No trial, licensing or edition gating anywhere.

## Verification executed

| Gate | Command | Result |
|---|---|---|
| Format | `cargo fmt --all --check` | clean |
| Lint | `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| Core unit | `cargo test -p talus-core --lib` | 46 passed (+1 Keychain test run separately: passed) |
| Engine integration | `eval "$(scripts/test-env.sh)"; cargo test -p talus-core --test engines` | 5 passed — SQLite 3.53, PostgreSQL 17, MySQL 8.4 |
| SSH integration | `cargo test -p talus-core --test ssh` | 1 passed (alpine sshd container → PostgreSQL) |
| App unit | `cargo test -p talus-app --lib` | 15 passed |
| UI (headless, real SQLite / PostgreSQL / SSH) | `cargo test -p talus-app --test ui_flow --test ui_more` | 16 passed |
| Performance (release) | `cargo test --release -p talus-core --test perf -- --ignored --nocapture` | 2 passed (numbers below) |
| Bundle | `scripts/bundle-macos.sh` | `target/release/Talus.app`, 17 MB, ad-hoc signature verifies |
| Startup | `TALUS_STARTUP_PROBE=1 target/release/talus` | first frame 540 ms cold, ~118 ms warm; RSS ≈ 83 MB; idle CPU 0 % |

Performance (1M-row SQLite table, Apple Silicon, release): editor `SELECT *` capped at 10k rows
3 ms; data page at offset 900k 13 ms; filtered + sorted page 73 ms; streaming CSV export of 1M
rows 1.9 s (57.5 MB); splitting a 5 MB script 17 ms (was 28 s before the fix); formatting it 77 ms.

### Covered by tests

- Engines (each of the three): multi-statement scripts, error stop/continue, error line + PG
  position, row cap + session reuse, full metadata (dbs/schemas/objects/columns/indexes/FKs/DDL),
  paging/sort/filters (incl. injection attempt as data), apply update/insert/delete, rollback on
  missing row and constraint violation, cancellation, find in database (structure + data),
  designer create/rename/modify/drop + rollback of failing DDL batch, CSV import 1 200 rows in
  batches with NULLs + full rollback on a duplicate key; failures (wrong password, refused port,
  missing/invalid SQLite file, initial SQL).
- UI: connection dialog → test → save → connect → tree → DDL → query (2 result sets) → error
  line → history → disconnect; data edit/apply/sort/read-only view; Stop cancels; session
  restore; drop table confirm + cancel; delete connection; export job; completion (tables,
  alias columns with lazy metadata), ⌘/ comment, Format; table designer create/rename/drop with
  confirmation; CSV import wizard; explain + inspector + copy as INSERT; find in database;
  SSH tunnel test connection from the dialog; PostgreSQL password prompt, create/drop database,
  server monitor; ER diagram layout + open data; structure sync between two databases.
- Engines also cover: process list + cancelling another session's query, create/drop database,
  ER schema model.

## Bugs found and fixed by the tests

- "Saved" message erased by the immediate page reload.
- Context menu stayed open on top of a modal and disabled its buttons.
- Splitter and formatter were O(n²) on large scripts.
- MySQL `KILL QUERY` on `BENCHMARK`/`SLEEP` returned normally → cancel flag added.
- MySQL inline `REFERENCES` silently ignored (test used wrong DDL; documented).
- PG `COUNT(*)` via parameterised path not text → cast.
- Keyword list for binary search was unsorted.
- Unlabelled "Initial Database" field (accessibility) and unlabelled SSL combo (replaced by radios).
- Drop-database success reported as "Database created.".
- Combo-box popups stayed open after choosing an item (next click only closed the popup).
- Structure-sync success message erased by the automatic re-compare.
- Icon glyphs in monospace text (grid row markers, ER diagram) rendered as boxes.

## Known limitations

- Native macOS menu bar (muda; no key equivalents, no Edit menu so egui keeps ⌘C/⌘V); the in-window bar is only used elsewhere. App icon: `scripts/make-icon.py` → `assets/Talus.icns`, copied by `scripts/bundle-macos.sh` (bundle build not re-run after adding it).
- Visual style: monochrome (theme.rs); colour only for ok/warn/danger and row state.
- PostgreSQL editor results have no column type names (simple-query protocol).
- Rows beyond the editor row cap are drained, not cancelled.
- SSL `prefer`/`require` do not verify certificates; `verify-full` does but fails through an SSH tunnel (host name is 127.0.0.1) and has no client cert/key.
- SQLite designer cannot change type/nullability/default of existing columns (no table rebuild).
- Designer: PK edits only on SQLite (rebuild); PG/MySQL need a query. SQLite rebuild drops triggers on the table and does not run foreign_key_check.
- Export of key-less tables orders by all columns; unorderable types (PG `json`, `xml`) fail.
- Very large scripts (multi-MB) in the editor are slow to lay out (egui TextEdit).
- SQL Server: connection (TLS modes incl. verify-full), metadata, data editor, designer, indexes/FKs, sync, dump, users, routines,
  query builder, monitor are covered by the shared integration suite. Cancel uses `KILL` from a side connection (needs ALTER ANY
  CONNECTION, i.e. sysadmin/processadmin) and the session reconnects, losing an open transaction. Other servers' `CANCEL` is not
  possible. The test container is x86-only (Docker Desktop with Rosetta).
- Redis and MongoDB use their own tabs (key browser, document browser, console) and `nosql::NoSqlSession`; SQL-only tools
  (designer, sync, dump, builder, users, routines) are disabled for them. Redis TLS and MongoDB TLS are not covered by tests.
- Model designer (Tools → Data Modeling): conceptual, logical and physical models; a physical model can be compared
  with the live schema selected in the navigator (Compare with Selection, no drops). Star schema and Data Vault roles
  with templates (Templates menu) and role rules in Check. Common types map to a portable logical type; anything else is kept as a native
  type string and emitted verbatim, so forward engineering to another engine may need manual edits.
- Not implemented (deferred, see feature matrix): Oracle and Snowflake engines, the Oracle PL/SQL debugger, a PHP-script HTTP tunnel (the proxy CONNECT tunnel exists),
  cloud collaboration and the AI assistant (left out by the user's decision).

## Incidents

- A coordinate-based click during manual verification may have landed in another app that came
  to the foreground. Coordinate clicking was stopped; visual checks use off-screen rendering
  (`TALUS_SCREENSHOT_DIR=… cargo test -p talus-app --test ui_flow`).

## Next concrete actions

1. Views sync; cross-engine type mapping for data transfer (ADV-04).
2. Visual grant editor and routine/trigger/view designers (ADV-08/09).
3. Oracle and Snowflake engines need drivers/accounts that are not available here.
