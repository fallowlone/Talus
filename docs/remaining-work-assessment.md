# Remaining-work assessment

_Verified against the working tree on 2026-10-13. Status labels follow
`docs/feature-matrix.md`; this file records why the last items stand where they do._

## Verified current state

| Item | State | Evidence |
|---|---|---|
| Oracle engine | verified | `crates/talus-core/src/engine/oracle.rs`; the shared engine suite plus a headless UI flow run against Oracle Database Free 23 in Docker (`TALUS_ORACLE=1`). |
| PL/SQL debugger | verified | `crates/talus-core/src/ora_debugger.rs` on `DBMS_DEBUG`, driven from the same debugger tab as PL/pgSQL; core tests (`tests/oracle_debugger.rs`) and the Routines UI. Needs `DEBUG CONNECT SESSION` and routines compiled `DEBUG`. `DBMS_DEBUG` cannot list frame variables, so the panes show parameters and declared names. |
| Snowflake engine | implemented, not live-verified | `crates/talus-core/src/engine/snowflake.rs` (account / warehouse / role / schema; password, key-pair, OAuth auth), connection-dialog section, dialect/DDL/sync/users/profile branches, unit tests and the UI test `snowflake_dialog_saves_cloud_settings_without_a_server`. The live suite exists (`snowflake_suite`) but needs a real account: `TALUS_TEST_SNOWFLAKE=account:user:password:database[:warehouse[:role]]`. |
| PHP tunnel | verified | `crates/talus-core/src/php_tunnel.rs` + `tunnel/talus-tunnel.php` (HMAC-signed requests, target allow list, multiplexed streams); `tests/php_tunnel.rs` runs real PHP against PostgreSQL. The database password never passes through the script. |
| Model / ER printing | verified | `crates/talus-core/src/print/` (scene, layout, PDF) + the app print dialog (page setup, preview, multi-page tiling, CUPS job); `tests/print.rs`, `tests/print_dialog.rs`, `tests/print_pdf.rs`. |
| Shared workspaces / cloud collaboration | verified (self-hosted) | `crates/talus-server` + `crates/talus-core/src/workspace.rs` + the app window; decided 2026-10-15: a team hosts its own server (no hosted service), definitions only, never Keychain secrets. Tests: `talus-core/tests/workspace.rs` (two clients, conflict, roles, revocation, lockout, secret refusal) and `talus-app/tests/workspace_dialog.rs`. |

## How cloud collaboration was settled

The blocker was a decision, not code: who hosts it, who holds accounts, what is shared. The
choice: a self-hosted server shipped in this repository, run by each team behind its own TLS proxy;
accounts are created by that server's administrator; only connection definitions are shared, and
each member supplies their own credentials. Hosting, backups and TLS therefore belong to the team
running the server (`crates/talus-server/README.md`). Not built: invitations by e-mail or link,
real-time push (clients pull), and merging of concurrent edits (a stale push is refused with 409).

## Recommended sequence from here

1. Snowflake live verification against a real account (fixes will be small dialect
   or catalog adjustments, driven by `snowflake_suite` failures).
2. Depth items already listed in `docs/progress.md` (ADV-08/09 designers, views sync;
   ADV-04 cross-engine transfer is implemented and verified SQLite→PostgreSQL).
3. Optional growth of shared workspaces: invitation links, live updates, per-field merge.
