# Testing Strategy

| Layer | Tool | Scope | Runs |
|---|---|---|---|
| Unit | `cargo test -p talus-core` | lexer, splitter, quoting, DML/DDL builders, config store, history, export writers, redaction | always |
| Integration | `cargo test -p talus-core --test engines_*` | real drivers: connect, version, execute multi-result, errors with position, cancel, metadata, DDL, paging, apply_changes txn/rollback | SQLite always (temp files); PG/MySQL when `TALUS_TEST_PG_URL` / `TALUS_TEST_MYSQL_URL` set |
| UI | `egui_kittest` in `crates/talus-app/tests` | vertical slice against temp SQLite: create connection, connect, expand tree, open SQL tab, run, see results, error banner, disconnect; data tab edit+apply | always (headless, no window) |
| E2E manual | release `Talus.app` | smoke checklist in `docs/progress.md` | per milestone |
| Performance | `cargo test --release -- --ignored perf_` | 1M-row SQLite fetch with cap, export 1M rows streaming, splitter on 5 MB script; record timings | M10 |
| Security | unit/integration | config/history/log files contain no test password; quoting fuzz-style cases; destructive ops require confirmation token | always |

Test databases:
- `docker compose -f docker-compose.test.yml up -d` → `postgres:17` on 55432, `mysql:8.4` on 53306, tmpfs storage,
  throwaway credentials defined only in that file and env for tests (never real servers).
- Each integration test creates its own uniquely named schema/database/table and drops it afterwards.

Rules: never report a test as passing without running it; failing tests are fixed at the root cause, not skipped.
