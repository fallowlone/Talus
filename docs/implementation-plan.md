# Implementation Plan

Each milestone ends with: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test --workspace` (+ PG/MySQL integration when Docker is up), release build, `docs/progress.md` update.

| # | Milestone | Features (matrix IDs) | Exit criteria |
|---|---|---|---|
| M0 | Investigation + design docs | — | feature-matrix, architecture, plan, testing strategy written |
| M1 | Core foundation | DRV-01, SQL-04, CON-04 (store), model | Lexer/splitter/dialect unit tests green; config + Keychain store tested |
| M2 | Engines | CON-02, CON-05, CON-06, SQL-05/06/07/08, NAV-01 (metadata), SCH-01/02 | Integration tests green on SQLite, PostgreSQL 17, MySQL 8.4 (Docker) |
| M3 | **Vertical slice UI** | CON-01/03, NAV-01/02/03, SQL-01/02/03/05/06/07, PRD-05 | Launch → create SQLite/PG connection → connect → tree → SQL tab → run → results → error shown → disconnect; kittest UI tests for the flow |
| M4 | Data editor | DAT-01..07 | Paging/sort/filter/edit/insert/delete with transactional apply verified on 3 engines |
| M5 | Schema management | SCH-03/04/05/06, PRD-07 | Create/alter/drop with SQL preview + confirmation; integration-tested DDL generation |
| M6 | Productivity | SQL-09/10/11/12/13/14, PRD-01/02/03/04/06, CON-07/11 | Completion, history, files, explain, prefs, shortcuts, session restore |
| M7 | Transfer | XFR-01/02, ADV-06 | Streaming export (CSV/TSV/JSON/SQL), CSV import with mapping, SQL dump/execute file |
| M8 | Connectivity | CON-08/09/10 | SSH tunnel (russh), TLS modes, reconnect semantics |
| M9 | Advanced | ADV-01/02/07 | ER diagram, structure sync, server monitor |
| M10 | Packaging + perf | — | `Talus.app` bundle, startup time, 1M-row stress, memory measurements |
| M11+ | Breadth | P2/P3 deferred items (Oracle/SQL Server/MongoDB/Redis, modeling, BI, automation…) | Re-plan with evidence |

Risks:
- egui text editor performance on very large SQL files (>1 MB): mitigate with highlighting cache keyed by text hash; measure in M10.
- `mysql_async` multi-result handling of `DELIMITER` scripts: splitter handles `DELIMITER` client-side.
- Keychain access prompts during tests: tests use `MemoryStore`; a single opt-in test exercises Keychain.
