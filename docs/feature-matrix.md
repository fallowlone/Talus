# Feature Matrix


Engines: `L` SQLite, `P` PostgreSQL, `M` MySQL/MariaDB. Complexity S/M/L. Risk: data-loss / security notes.
Verification: `U` unit test, `I` integration test vs real DB (SQLite file / Docker PG+MySQL), `E` headless UI test with `egui_kittest` (AccessKit queries) against real SQLite, `R` manual run of the release `.app`.

Product rule: **no trial, licensing, registration or edition gating** — every implemented feature is available
unconditionally (commercial clients often split features across editions; Talus does not).

Status values: `todo`, `wip`, `impl` (code exists, not fully verified), `verified` (acceptance met and
verification executed), `deferred`. **Status is updated in `docs/progress.md`; this table mirrors it.**

## P0 — usable database client

| ID | Feature | Expected behaviour | Eng | Deps | Cx | Risk | Acceptance | Ver | Status |
|---|---|---|---|---|---|---|---|---|---|
| CON-01 | Create connection | Pick type, fill General fields, save; appears in tree | LPM | — | M | secrets | Saved config persists across restart; password not in plain JSON | U,E | verified |
| CON-02 | Test connection | "Test" button opens & closes a session, shows server version or error | LPM | DRV | S | — | Success shows version; wrong password shows driver error, no crash | I,E | verified |
| CON-03 | Edit / duplicate / delete connection | Context menu actions; delete asks confirmation and removes stored secret | LPM | CON-01 | S | secret leak | Deleted secret no longer retrievable | U | impl (delete verified; edit/duplicate untested) |
| CON-04 | Secure credential storage | Passwords encrypted via OS keychain-backed key; "save password" optional, else prompt | LPM | — | M | high | Config file contains no plaintext secret; prompt when unsaved | U,E | verified |
| CON-05 | Connect / disconnect | Double-click connects; Close Connection disconnects and clears tree | LPM | DRV | M | — | Sessions closed in driver after disconnect | I,E | verified |
| CON-06 | Connection failure handling | Timeout, auth, network errors shown inline with driver message; UI stays usable | LPM | CON-05 | S | — | Unreachable host errors within timeout | I | verified |
| DRV-01 | Provider abstraction + capabilities | One interface; engine-specific SQL in provider modules only | LPM | — | M | injection | Identifier quoting tested per engine | U | verified |
| NAV-01 | Object tree | connection → database → schema → Tables/Views → columns/indexes/FKs | LPM | DRV | M | — | Tree matches catalog of test DB | I,E | verified |
| NAV-02 | Refresh metadata | Reloads children of selected node | LPM | NAV-01 | S | — | New table visible after refresh | E | impl |
| NAV-03 | Tree filter | Search box filters loaded nodes, expands matches | — | NAV-01 | S | — | Typing filters nodes | U | impl |
| SQL-01 | SQL editor tabs | Multiple tabs, each bound to connection + database/schema | LPM | — | M | — | Two tabs keep independent text/results | E | verified |
| SQL-02 | Syntax highlighting per dialect | Keywords per engine dialect | LPM | SQL-01 | S | — | Dialect switches with connection | R | verified |
| SQL-03 | Execute all / selection / current statement | ⌘R run (selection if any), ⇧⌘R current statement | LPM | SPL | M | — | Correct statements sent | U,E | verified |
| SQL-04 | Statement splitter | Splits on `;` respecting strings, comments, `$$`, `DELIMITER` | LPM | — | M | wrong exec | Unit-tested edge cases | U | verified |
| SQL-05 | Result grid (multi result sets) | One tab per result set + message summary (rows affected, time) | LPM | SQL-03 | M | — | Multiple SELECTs → multiple tabs | I,E | verified |
| SQL-06 | Error reporting with location | Error message, statement index, line/position when driver provides it | LPM | SQL-03 | S | — | PG syntax error shows line | I | verified |
| SQL-07 | Cancel running query | Stop button cancels server-side (PG cancel, MySQL KILL QUERY, SQLite worker interrupt) | LPM | SQL-03 | M | — | `pg_sleep(30)` stopped < 2s | I | verified |
| SQL-08 | Large result safety | Row cap per result (configurable), truncated flag shown | LPM | SQL-05 | S | memory | 1M-row SELECT does not freeze UI | I | verified |
| DAT-01 | Open table data (paged) | Double-click table → data tab with pagination (default 1000) | LPM | NAV-01 | M | — | Page n shows correct rows with stable order | I,E | verified |
| DAT-02 | Sort by column | Click header asc/desc/none; server-side ORDER BY | LPM | DAT-01 | S | injection | Quoted identifiers | U,I | verified |
| DAT-03 | Filter rows | WHERE builder (column, op, value) with params | LPM | DAT-01 | M | injection | Values bound as parameters | U,I | verified (core; UI bar impl) |
| DAT-04 | Edit cell, insert, delete rows | Pending changes highlighted; Apply (⌘↩) runs in a transaction; Discard (⌘⎋) | LPM | DAT-01, PK | L | data loss | Requires PK/rowid; failure rolls back and keeps pending edits | I,E | verified |
| DAT-05 | Set NULL / empty | ⌃0 sets NULL | LPM | DAT-04 | S | — | NULL persisted | I | verified (core; ⌃0 impl) |
| DAT-06 | Cell inspector | Side panel showing full value (text/JSON/hex size) | — | DAT-01 | S | — | Long text viewable | E | verified |
| DAT-07 | Copy as TSV / INSERT | Context menu copy | LPM | DAT-01 | S | — | Clipboard content correct | U | verified |

## P1 — broad practical usefulness

| ID | Feature | Expected behaviour | Eng | Deps | Cx | Risk | Acceptance | Ver | Status |
|---|---|---|---|---|---|---|---|---|---|
| CON-07 | Connection groups | Group folders in tree | — | CON-01 | S | — | Persisted grouping | U | impl |
| CON-08 | SSH tunnel | Password or key auth; local port forward | PM | CON-05 | M | secrets | Connect through sshd container | I | verified |
| CON-09 | SSL/TLS options | PG sslmode + certs; MySQL CA/cert/key | PM | CON-05 | M | MITM | verify-full rejects bad host | I | impl (verify-full + CA file; client cert/key not yet; not usable through SSH tunnel: host is 127.0.0.1) |
| CON-10 | Reconnect after loss | Detect broken session, offer reconnect; never auto-retry non-idempotent statements | PM | CON-05 | M | duplicate writes | Killed backend → next query reports & reconnect works | I | verified |
| CON-11 | Advanced: timeouts, initial queries | Connect timeout; statements run after connect | LPM | CON-05 | S | — | Initial query effect visible | I | verified |
| SQL-09 | Code completion (keywords, schema objects, columns after `.`) | Context-aware suggestions from cached metadata | LPM | NAV-01 | M | — | `alias.` suggests columns | U,R | verified |
| SQL-10 | Format SQL / toggle comment | Beautify, ⌘/ | — | SQL-01 | S | — | Formatter output valid | U | verified |
| SQL-11 | Query history | Every executed statement logged with time, duration, connection, status; searchable | LPM | SQL-03 | S | secrets in SQL | Persisted, size-capped, viewable | U,E | verified |
| SQL-12 | Save / open .sql files | Save query to file, open external | — | SQL-01 | S | — | Round-trip file | E | impl |
| SQL-13 | Explain plan | EXPLAIN result as table/tree | LPM | SQL-03 | M | — | Plan rows shown | I | verified |
| SQL-14 | Transaction mode in editor | Auto-commit toggle; commit/rollback buttons | PM L | SQL-03 | M | data loss | Uncommitted warning on close | I | verified |
| SCH-01 | DDL view | Show CREATE statement of table/view/index | LPM | NAV-01 | M | — | Matches engine output | I | verified |
| SCH-02 | Table structure view | Columns, types, nullability, defaults, PK, indexes, FKs | LPM | DRV | M | — | Matches catalog | I | verified |
| SCH-03 | Create table designer | Grid of columns + PK; SQL preview; execute | LPM | SCH-02 | L | — | Table created with preview SQL | I,E | verified |
| SCH-04 | Alter table (add/drop/rename column, indexes) | Diff → ALTER statements; preview; destructive confirmation | LPM | SCH-03 | L | data loss | DROP COLUMN requires explicit confirm | U,I | verified (columns, indexes, foreign keys; SQLite: type/null/default/PK/FK edits via table rebuild, triggers on the table are lost) |
| SCH-05 | Drop / truncate / rename objects | Confirmation dialog showing exact SQL | LPM | NAV-01 | S | data loss | Cancel performs nothing | I,E | verified |
| SCH-06 | Create / drop database/schema | Dialog with SQL preview | PM | NAV-01 | S | data loss | Confirm required | I | verified (databases; PG schemas via query) |
| XFR-01 | Export result/table to CSV, JSON, SQL INSERT, TSV | Streams rows to file, progress, cancel | LPM | DAT-01 | M | file overwrite | 100k rows exported without full in-memory load | I | verified |
| XFR-02 | Import CSV into table | Preview, column mapping, batch insert in transaction, error report | LPM | SCH-02 | L | partial import | Bad row → rollback or skip per option, reported | I | verified |
| PRD-01 | Keyboard shortcuts | Core set (⌘N/⌘Y/⌘R/⇧⌘R/⌘./⌘W/⌘{ }/⌘1/⌘2/⌘↩/⌘⎋) | — | — | S | — | Shortcuts work in E2E | E | impl (⌘R/⌘//⇧⌘F verified) |
| PRD-02 | Resizable panes, tab management | Navigation/center/info panes resizable; tabs close/reorder | — | — | M | — | Sizes persisted | E | impl |
| PRD-03 | Session restore | Reopen tabs (text) after restart | — | SQL-01 | M | — | Unsaved SQL restored | E | verified |
| PRD-04 | Preferences | Theme, editor font size, page size, row cap, confirm settings | — | — | S | — | Persisted, applied | U,E | impl |
| PRD-05 | Status bar & activity | Connection state, running op, row counts, timing | — | — | S | — | Visible state transitions | E | impl |
| PRD-06 | Find in database | Search object names (structure) and data (contains) | LPM | NAV-01 | M | heavy queries | Results clickable; cancellable | I | verified |
| PRD-07 | Info pane | General (row count, size, comment), DDL | LPM | SCH-01 | S | — | Shows selected object | E | verified |

## P2 — advanced

| ID | Feature | Notes | Status |
|---|---|---|---|
| ADV-01 | ER diagram view (read-only, auto-layout) | Render tables + FK lines from metadata | verified (read-only canvas, drag/zoom; SVG export instead of PNG) |
| ADV-02 | Structure synchronization (compare two schemas, generate script) | Same-engine only first | verified (tables, columns, indexes, foreign keys and views — across differently named schemas/databases the view definitions' qualifiers are rewritten to the target namespace, token-aware so literals and comments are untouched; verified on PostgreSQL with two schemas in one database) |
| ADV-03 | Data synchronization | Compare by PK, dry run + apply, optional delete of extra rows | verified (same-engine exact; text comparison, target table held in memory ≤ 1M rows) |
| ADV-04 | Data transfer between connections | Table copy with type mapping | implemented (the sync tab compares across engines: missing tables are created on the target with types mapped through the portable logical types, indexes carried where the target can express them, then data synchronization copies the rows; defaults, identity markers and foreign keys are not carried and are reported in the plan; verified SQLite→SQLite and SQLite→PostgreSQL in core and through the UI) |
| ADV-05 | Visual query builder | | verified (tables, FK-suggested joins, columns, AND conditions, order, limit; generated SQL opens in a query tab) |
| ADV-06 | Backup/restore (SQL dump) | Dump/execute SQL file | verified (dump structure+data in FK order, views; execute file all-or-nothing on transactional-DDL engines) |
| ADV-07 | Server monitor (processes, variables) | PG `pg_stat_activity`, MySQL processlist, kill | verified (process list, cancel/terminate, auto-refresh) |
| ADV-08 | Users/roles/privileges | | verified (PG roles / MySQL users / SQL Server logins: list, grants, create, drop; privilege editor for database / schema / table targets builds GRANT / REVOKE from a whitelist and shows the SQL before running) |
| ADV-09 | Views/routines/triggers designers | Read-only DDL first (in SCH-01) | implemented (Routines tab: list with definitions, creation skeletons for function / procedure / trigger / view in each engine's own spelling, edit by opening the definition in a query tab, confirmed drop; no form-based designer — SQL stays the editing surface) |
| ADV-10 | Excel/XML/HTML export, JSON/XML import | | verified (HTML, XML, Excel .xlsx export; JSON and XML import) |

## P3 — specialized / optional

| ID | Feature | Status |
|---|---|---|
| OPT-01 | Oracle, SQL Server engines (+ Redis, MongoDB) | SQL Server verified (tiberius; the whole engine suite runs against SQL Server 2022 in Docker, x86 emulation); Redis verified (key browser with typed value editing, TTL, rename, SCAN, console; core + UI tests against redis:7); MongoDB verified (document browser with filter/sort/paging, JSON edit by _id, insert/delete, indexes, commands; core + UI tests against mongo:7); Oracle verified (oracledb thin driver; the whole engine suite plus a UI flow run against Oracle Database Free 23 in Docker, `TALUS_ORACLE=1`); Snowflake implemented (snowflake-connector-rs; unit tests + connection-dialog UI test; the live suite `snowflake_suite` runs only with a real account in `TALUS_TEST_SNOWFLAKE`, so end-to-end verification is pending) |
| OPT-02 | MongoDB, Redis (NoSQL UIs) | verified (see OPT-01) |
| OPT-03 | Snowflake & cloud vendor presets | Snowflake engine implemented (account / warehouse / role / schema, password, key-pair and OAuth auth; see OPT-01 for its verification status); cloud vendor presets cover the hosted PostgreSQL / MySQL / SQL Server / Redis services (CON-01 cloud presets) |
| OPT-04 | Modeling (conceptual/logical/physical), forward/reverse engineering | verified (conceptual / logical / physical models with lossless conversion — conceptual: entities, attribute names, keys, relationships; logical: + portable types, nullability, indexes; physical: + engine types, defaults, auto-increment; canvas with draggable entities and FK lines, consistency check, JSON save/open, reverse from a live schema, forward DDL for SQLite/PostgreSQL/MySQL/SQL Server, compare with the selected live schema; forward script executes and reverse/compare round-trip on all four engines in the shared suite; star schema (fact / dimension) and Data Vault (hub / link / satellite) table roles with templates and role rules in Check; printing of the whole model and of ER diagrams with page setup, preview, multi-page tiling, PDF export and CUPS printing) |
| OPT-05 | BI dashboards & charts | partial (bar / line / scatter / pie chart of any query result with column picking; no dashboards or saved charts) |
| OPT-06 | Automation / scheduled batch jobs | verified (jobs: run SQL, export table, dump database; schedules manual / every N minutes / daily / weekly; in-app scheduler with catch-up, `talus --run-job` for launchd, launchd agent install/remove from the UI (the install itself was not exercised in tests); SQL connections only, SSH-tunnelled jobs run only inside the app) |
| OPT-07 | Debugger (PL/pgSQL, PL/SQL) | verified for PL/pgSQL (server needs pldebugger: `shared_preload_libraries = 'plugin_debugger'`, `CREATE EXTENSION pldbgapi`): Routines → Debug, input parameters with NULL, Start/Continue (F9), Step Over (F10), Step Into (F11), Stop (aborts and rolls back), breakpoints by clicking line numbers, call stack, variables with Set Value (double-click), log with the result; Step Out (⇧F11, emulated by stepping over until the stack is shallower — pldebugger has none), watch list; tested in core and through the UI against PostgreSQL 17. PL/SQL verified on `DBMS_DEBUG` (same debugger UI; needs `DEBUG CONNECT SESSION` and routines compiled `DEBUG`; variables are the routine's parameters and declared names, because `DBMS_DEBUG` cannot list frame variables; tested against Oracle Database Free 23 in core and through the Routines UI) |
| OPT-08 | Cloud collaboration, AI assistant | Shared workspaces verified: self-hosted [`talus-server`](../crates/talus-server/README.md) (accounts created by the administrator, PBKDF2 passwords, hashed 30-day session tokens, owner / editor / viewer roles enforced on every endpoint, members added and revoked by user name, revision-checked writes that return 409 instead of overwriting) and the app window Tools → Shared Workspace… (sign in, create workspace, members, share / update / unshare, pull); only connection definitions are shared — never passwords, tunnel secrets, local paths or `init_sql`; tested with two clients against the real server in `talus-core/tests/workspace.rs` and through the UI in `talus-app/tests/workspace_dialog.rs`. The server speaks HTTP and needs a TLS reverse proxy; the hosting is the team's own. AI assistant: left out by the user's decision |
| OPT-09 | Data generation, data dictionary, data profiling | verified (generation by type/name with FK parents, unique keys, length limits, repeatable runs; dictionary as HTML/Markdown; profile tab with counts, NULL %, distinct, min/max/avg, top values — all on SQLite, PostgreSQL, MySQL and SQL Server) |
| OPT-10 | HTTP tunnel (PHP), encrypted SQLite | verified (HTTP tunnel through an HTTP proxy's CONNECT method with optional Basic auth — works for every network engine and needs no script on a web server; probe on open reports proxy/auth errors; tested with an in-test proxy and PostgreSQL, also from the connection dialog. PHP tunnel script ([`tunnel/talus-tunnel.php`](../tunnel/talus-tunnel.php)) for shared hosting: HMAC-signed requests, per-script target allow list, multiplexed streams over HTTPS; the database password never passes through the script; integration-tested with real PHP + PostgreSQL (transactions, cancel, large results, signature/allow-list rejection). Encrypted SQLite via SQLCipher: open, create, wrong-key refusal; files from SQLite3 Multiple Ciphers use a different default cipher) |
