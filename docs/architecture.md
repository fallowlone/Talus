# Architecture — Talus

Status: v1 design, 2026-10-10. Supersedes the initially considered Electron/React stack
(rejected by product owner requirement: **the application is written entirely in Rust and optimised**).

## 1. Constraints that drove the design

| Constraint | Evidence | Consequence |
|---|---|---|
| 100% Rust, optimised | product owner | Rust GUI toolkit; no webview, no JS |
| No Xcode, only Command Line Tools | `xcode-select -p` → CLT | No Metal shader precompile (rules out gpui default build), no xcodebuild; bundle `.app` by script |
| Real DB connectivity, testable | prompt | Engines with mature pure-Rust drivers and local test servers: SQLite (file), PostgreSQL + MySQL (Docker, verified running) |
| Credentials in Keychain | prompt | `security-framework` generic passwords |
| UI must be verified, not only compiled | prompt | Toolkit with headless UI test harness → `egui_kittest` |
| No trial/edition gating | product owner | No licensing code at all |

## 2. Stack

| Concern | Choice | Why / alternatives rejected |
|---|---|---|
| GUI | `eframe`/`egui` 0.36 (wgpu → Metal) | Immediate-mode, renders only visible rows → cheap virtualised grids; AccessKit; `egui_kittest` headless tests. Rejected: gpui (needs Xcode `metal` or unstable runtime-shader path, API churn), iced (no headless test harness of equal maturity, weaker table story), Slint (DSL + licensing), Tauri (UI not Rust) |
| Async runtime | `tokio` multi-thread, owned by app | Drivers are tokio-based |
| SQLite | `rusqlite` (`bundled`) on blocking threads | `InterruptHandle` gives real cancellation |
| PostgreSQL | `tokio-postgres` + `postgres-native-tls` | `CancelToken` = server-side cancel; `simple_query` returns multiple result sets as text |
| MySQL/MariaDB | `mysql_async` | Text protocol multi-result; cancel via `KILL QUERY <id>` on a side connection |
| TLS | `native-tls` (Security.framework) | System trust store, no OpenSSL |
| Secrets | `security-framework` (Keychain generic password, service `io.github.fallowlone.Talus`) | Prompt requirement |
| Config | `serde_json` file in `~/Library/Application Support/io.github.fallowlone.Talus/` (via `directories`) | Human-readable; never contains passwords |
| SQL highlighting | own tokenizer (shared with statement splitter) | One lexer for splitting + highlighting + completion context; avoids syntect's large regex sets |
| File dialogs | `rfd` | Native NSOpenPanel |
| CSV | `csv` crate | Streaming read/write |
| Logging | `tracing` + file appender in app support dir; secret redaction at the type level (`Secret` newtype has redacted `Debug`) | |

## 3. Workspace layout

```
Cargo.toml                 workspace, release profile (lto="fat", codegen-units=1, panic=abort off for GUI safety)
crates/talus-core/        no GUI deps; everything testable without a window
  src/model.rs             ConnectionConfig, Engine, Capabilities, QueryResult, Cell, ColumnMeta, ObjectRef…
  src/secret.rs            Secret newtype (zeroize-on-drop, redacted Debug/Display)
  src/sql/lexer.rs         dialect-aware tokenizer (strings, quoted idents, comments, $tag$, DELIMITER)
  src/sql/split.rs         statement splitter (built on lexer) with byte ranges + line numbers
  src/sql/dialect.rs       Dialect: identifier quoting, literal rendering, LIMIT/OFFSET, type names
  src/sql/dml.rs           SELECT page / UPDATE / INSERT / DELETE builders from (table, key, values) with params
  src/sql/ddl.rs           CREATE/ALTER TABLE generation from designer model diff
  src/engine/mod.rs        Session enum (Sqlite | Postgres | MySql) + CancelHandle; uniform async API
  src/engine/sqlite.rs
  src/engine/postgres.rs
  src/engine/mysql.rs
  src/store/config.rs      connections.json, settings.json (atomic write: temp + rename)
  src/store/keychain.rs    SecretStore trait {get,set,delete}; KeychainStore (macOS) + MemoryStore (tests)
  src/store/history.rs     JSONL history log, size-capped rotation
  src/export.rs            streaming CSV/TSV/JSON/SQL-INSERT writers + chunked table export
  src/import.rs            CSV preview + batched transactional insert
  src/ssh.rs               russh tunnel (local port forward), known_hosts checks
  src/sql/ddl.rs           designer CREATE/ALTER plans with destructive-step reporting
  src/sql/format.rs        token-based beautifier, comment toggle
  tests/                   integration tests (SQLite always; PG/MySQL when TALUS_TEST_PG / TALUS_TEST_MYSQL set)
crates/talus-app/         binary `talus`
  src/main.rs              eframe bootstrap, tokio runtime
  src/app.rs               App state root, layout (top bar, nav pane, tab area, info pane, status bar)
  src/worker.rs            UI ↔ background bridge: Job submission, JobId, results via channel + ctx.request_repaint()
  src/ui/nav_tree.rs       connection/object tree with lazy loading + filter
  src/ui/conn_dialog.rs    new/edit connection form, Test button
  src/ui/sql_tab.rs        editor + toolbar + result tabs
  src/ui/editor.rs         SQL text editor (TextEdit + layouter highlighting + completion popup)
  src/ui/grid.rs           virtualised result/data grid (egui_extras::TableBuilder rows())
  src/ui/data_tab.rs       table data editor: paging, sort, filter, pending changes
  src/ui/design_tab.rs     table designer + SQL preview
  src/ui/info_pane.rs, history.rs, prefs.rs, export_dialog.rs, import_dialog.rs, confirm.rs
  tests/                   egui_kittest UI tests
scripts/bundle-macos.sh    builds release, assembles Talus.app (Info.plist, icon), ad-hoc codesign
docker-compose.test.yml    postgres:17 + mysql:8.4 + alpine sshd for integration tests (ephemeral)
```

## 4. Component boundaries

```
 ┌──────────── talus-app (UI thread, egui) ────────────┐
 │ App state ── tabs ── widgets                          │
 │      │ submit(Job)            ▲ JobEvent (via mpsc)  │
 └──────┼────────────────────────┼───────────────────────┘
        ▼                        │
 ┌──────────── worker (tokio runtime) ───────────────────┐
 │ SessionPool: per (connection, purpose) Session         │
 │   purpose = Meta (tree/info) | Tab(tab_id)              │
 │ CancelHandles registry keyed by JobId                   │
 └──────┼─────────────────────────────────────────────────┘
        ▼
 ┌──────────── talus-core ──────────────────────────────┐
 │ engine::Session (enum dispatch)  sql::*  store::*      │
 └────────────────────────────────────────────────────────┘
```

Rules:
- The UI thread **never** awaits I/O. Every DB/file operation is a `Job` with an id; the UI shows a
  loading state until a `JobEvent::Done/Failed/Cancelled` arrives. Stale results (tab closed,
  newer request) are ignored by id.
- Engine-specific SQL lives only in `engine/<engine>.rs` and `sql/dialect.rs`. UI code never
  branches on engine except via `Capabilities` flags.
- Each SQL editor tab gets its **own** session (like the reference: independent query windows),
  so long queries in one tab do not block metadata or other tabs and can be cancelled
  individually. Metadata uses a shared `Meta` session per connection.

## 5. Key interfaces (core)

```rust
pub enum Engine { Sqlite, Postgres, MySql }
pub struct Capabilities { schemas: bool, databases: bool, server_cancel: bool,
                          transactional_ddl: bool, returning: bool, explain_format: ExplainKind, … }

pub enum Session { Sqlite(SqliteSession), Postgres(PgSession), MySql(MySession) }
impl Session {
    pub async fn connect(cfg: &ConnectionConfig, password: Option<&Secret>) -> Result<Self>;
    pub fn capabilities(&self) -> &Capabilities;
    pub fn cancel_handle(&self) -> CancelHandle;          // Send + Clone, usable while a query runs
    pub async fn server_version(&mut self) -> Result<String>;
    pub async fn execute(&mut self, stmt: &str, limits: &FetchLimits) -> Result<Vec<StatementResult>>;
    pub async fn list_databases(&mut self) -> Result<Vec<String>>;
    pub async fn list_schemas(&mut self, db: &str) -> Result<Vec<String>>;
    pub async fn list_objects(&mut self, scope: &Scope) -> Result<Vec<ObjectInfo>>;   // tables, views…
    pub async fn table_structure(&mut self, t: &ObjectRef) -> Result<TableStructure>; // cols, pk, idx, fks
    pub async fn ddl(&mut self, o: &ObjectRef) -> Result<String>;
    pub async fn fetch_page(&mut self, q: &PageQuery) -> Result<QueryResult>;          // sort/filter/offset
    pub async fn apply_changes(&mut self, t: &ObjectRef, cs: &ChangeSet) -> Result<ApplyReport>; // one txn
    pub async fn close(self) -> Result<()>;
}
```

`Cell` = `Option<String>` text representation (NULL distinct from empty string); binary values are
rendered as `0x…` hex with a flag in `ColumnMeta`. Writing back uses parameters typed as text with
server-side casts (`$1::text::<type>` in PG; implicit conversion in MySQL/SQLite), so no value is
ever spliced into SQL text.

## 6. Security architecture

- **Credentials**: only in Keychain (service `io.github.fallowlone.Talus`, account = connection UUID + kind
  `db`/`ssh`). Config JSON stores `save_password: bool`. If not saved, the user is prompted per
  connect and the secret lives only in memory (`Secret`, zeroized on drop).
- **Redaction**: `Secret` implements `Debug`/`Display` as `***`; config structs derive Debug
  without secret fields; tests assert logs/config never contain test passwords.
- **SQL injection**: identifiers through `Dialect::quote_ident` (doubling quote chars, rejecting NUL);
  values always bound as parameters. User-typed SQL in the editor is executed as-is by design.
- **Destructive operations**: DROP/TRUNCATE/DELETE-from-grid/ALTER that drops columns show a
  confirmation dialog with the exact SQL; nothing executes without explicit confirmation.
  Data-grid edits are batched and applied in one transaction; on error the transaction is rolled
  back and pending edits are kept for correction.
- **No silent retries**: on connection loss the failing statement is reported, the session is
  marked broken, and reconnect is explicit (user action or next job). Statements are never
  re-sent automatically.
- **Files**: export refuses to overwrite without the native save-panel confirmation; import reads
  through a bounded preview and validates column count/types before inserting.
- **Logs**: history log records SQL text (needed for the feature) but never connection passwords;
  file permissions 0600 for config/history.

## 7. Performance design

- Virtualised grid: only visible rows are laid out (`TableBuilder::body().rows(h, n, …)`).
- Result row cap (`FetchLimits.max_rows`, default 10 000 for editor results; data tab pages of
  1 000) with `truncated` flag; exports stream rows from the server instead of using the cap.
- egui repaints only on input or job events (`request_repaint` from worker) → idle CPU ≈ 0.
- Release profile: `opt-level=3`, `lto="fat"`, `codegen-units=1`, `strip=true`.
- Metadata cached per connection and invalidated on refresh/DDL execution.

## 8. Trade-offs

- egui looks less "native AppKit" than the reference; we compensate with a deliberate visual
  design (dense layout, custom theme). Native menus are not provided by egui; we use an in-window
  menu bar + keyboard shortcuts.
- Text representation of cells simplifies the grid and editing at the cost of typed client-side
  sorting (sorting is server-side anyway).
- Separate session per editor tab costs one server connection per tab (same as reference).
