//! Application root: owns state, routes worker events, executes actions and lays out the
//! window (toolbar, navigator, tabs, information pane, status bar).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::RichText;
use egui_phosphor::regular as icons;
use serde::{Deserialize, Serialize};
use talus_core::Secret;
use talus_core::engine::ScriptOptions;
use talus_core::export::ExportFormat;
use talus_core::model::{ConnectionConfig, Engine, FetchLimits, ObjectKind, ObjectRef, Outcome};
use talus_core::sql::Dialect;
use talus_core::store::config::{AppPaths, ConnectionsFile, Settings, load_json, save_json};
use talus_core::store::history::{History, HistoryEntry};
use talus_core::store::secrets::{SecretKind, SecretStore};
use uuid::Uuid;

mod chrome;
mod home;

use chrome::TOP_BAR_H;

use crate::actions::Action;
use crate::state::{ConnState, ConnStatus, DbNode, Loadable, NavSel, ObjNode, SchemaNode};
use crate::theme;
use crate::ui::builder_tab::{BuilderAction, BuilderTab};
use crate::ui::conn_dialog::{ConnDialog, DialogAction, TestState};
use crate::ui::console_tab::ConsoleTab;
use crate::ui::data_tab::{DataAction, DataTab};
use crate::ui::debug_tab::{DebugAction, DebugTab};
use crate::ui::design_tab::{DesignAction, DesignTab};
use crate::ui::diagram_tab::{DiagramAction, DiagramTab};
use crate::ui::dialogs::{self, Confirm, ConfirmResult, Confirmed, PasswordPrompt, PromptResult};
use crate::ui::docs_tab::DocsTab;
use crate::ui::find_tab::{FindAction, FindTab};
use crate::ui::import_dialog::{ImportAction, ImportDialog};
use crate::ui::jobs_tab::{ConnChoice, JobsAction, JobsTab};
use crate::ui::keys_tab::KeysTab;
use crate::ui::model_tab::{ModelAction, ModelTab};
use crate::ui::monitor_tab::{MonitorAction, MonitorTab};
use crate::ui::nosql_common::NoSqlCmd;
use crate::ui::panels::{self, HistoryAction, HistoryTab, StructureTab};
use crate::ui::profile_tab::{ProfileAction, ProfileTab};
use crate::ui::prog_tab::{ProgAction, ProgTab};
use crate::ui::sql_tab::{RunMode, SqlAction, SqlCtx, SqlTab, TxAction};
use crate::ui::sync_tab::{SyncAction, SyncTab};
use crate::ui::users_tab::{UsersAction, UsersTab};
use crate::ui::widgets;
use crate::worker::{ConnInfo, DebugOp, JobEvent, JobId, Purpose, Request, Response, Worker};

pub struct AppEnv {
    pub paths: AppPaths,
    pub secrets: Arc<dyn SecretStore>,
    pub rt: tokio::runtime::Handle,
}

pub enum TabKind {
    Objects,
    Sql(Box<SqlTab>),
    Data(Box<DataTab>),
    Structure(StructureTab),
    Design(Box<DesignTab>),
    Find(Box<FindTab>),
    Monitor(Box<MonitorTab>),
    Diagram(Box<DiagramTab>),
    Sync(Box<SyncTab>),
    Routines(Box<ProgTab>),
    Debug(Box<DebugTab>),
    Users(Box<UsersTab>),
    Builder(Box<BuilderTab>),
    Model(Box<ModelTab>),
    Keys(Box<KeysTab>),
    Docs(Box<DocsTab>),
    Console(Box<ConsoleTab>),
    Jobs(Box<JobsTab>),
    Profile(Box<ProfileTab>),
    History(HistoryTab),
}

pub struct Tab {
    pub id: u64,
    pub kind: TabKind,
}

impl Tab {
    fn title(&self) -> String {
        match &self.kind {
            TabKind::Objects => "Home".into(),
            TabKind::Sql(t) => t.title(),
            TabKind::Data(t) => t.title(),
            TabKind::Structure(t) => format!("{} (structure)", t.obj.name),
            TabKind::Design(t) => format!("{}{}", t.title(), if t.is_dirty() { " •" } else { "" }),
            TabKind::Find(_) => "Find in Database".into(),
            TabKind::Monitor(m) => format!("Monitor — {}", m.conn_name),
            TabKind::Diagram(d) => d.title(),
            TabKind::Sync(_) => "Structure Sync".into(),
            TabKind::Routines(r) => r.title(),
            TabKind::Debug(d) => d.title(),
            TabKind::Builder(b) => b.title(),
            TabKind::Model(m) => m.title(),
            TabKind::Keys(k) => k.title(),
            TabKind::Docs(d) => d.title(),
            TabKind::Console(c) => c.title(),
            TabKind::Jobs(_) => "Automation".into(),
            TabKind::Profile(p) => p.title(),
            TabKind::Users(u) => format!("Users — {}", u.conn_name),
            TabKind::History(_) => "History".into(),
        }
    }
    fn icon(&self) -> &'static str {
        match &self.kind {
            TabKind::Objects => icons::SQUARES_FOUR,
            TabKind::Sql(_) => icons::FILE_SQL,
            TabKind::Data(_) => icons::TABLE,
            TabKind::Structure(_) => icons::COLUMNS,
            TabKind::Design(_) => icons::PENCIL_RULER,
            TabKind::Find(_) => icons::MAGNIFYING_GLASS,
            TabKind::Monitor(_) => icons::PULSE,
            TabKind::Diagram(_) => icons::GRAPH,
            TabKind::Sync(_) => icons::ARROWS_LEFT_RIGHT,
            TabKind::Routines(_) => icons::FUNCTION,
            TabKind::Debug(_) => icons::BUG,
            TabKind::Builder(_) => icons::BLUEPRINT,
            TabKind::Model(_) => icons::TREE_STRUCTURE,
            TabKind::Keys(_) => icons::KEY,
            TabKind::Docs(_) => icons::FILE_TEXT,
            TabKind::Console(_) => icons::TERMINAL_WINDOW,
            TabKind::Jobs(_) => icons::CLOCK_COUNTDOWN,
            TabKind::Profile(_) => icons::CHART_BAR_HORIZONTAL,
            TabKind::Users(_) => icons::USERS,
            TabKind::History(_) => icons::CLOCK_COUNTER_CLOCKWISE,
        }
    }
    fn is_dirty(&self) -> bool {
        match &self.kind {
            TabKind::Sql(t) => t.dirty || t.tx_open,
            TabKind::Data(t) => t.is_dirty(),
            TabKind::Design(t) => t.is_dirty(),
            _ => false,
        }
    }
}

enum Pending {
    Test,
    Connect(Uuid),
    Schemas {
        conn: Uuid,
        db: String,
    },
    Objects {
        conn: Uuid,
        db: String,
        schema: Option<String>,
    },
    TreeStructure {
        conn: Uuid,
        obj: ObjectRef,
    },
    InfoDdl {
        conn: Uuid,
        obj: ObjectRef,
    },
    Script {
        tab: u64,
    },
    DataStructure {
        tab: u64,
    },
    Page {
        tab: u64,
        started: Instant,
    },
    Apply {
        tab: u64,
    },
    StructureTab {
        tab: u64,
    },
    DesignStructure {
        tab: u64,
        obj: ObjectRef,
    },
    DesignSave {
        tab: u64,
        obj: ObjectRef,
    },
    Destructive {
        conn: Uuid,
        db: String,
        schema: Option<String>,
    },
    Export,
    Dump,
    Routines {
        tab: u64,
    },
    DebugDescribe {
        tab: u64,
    },
    Debug {
        tab: u64,
    },
    Builder {
        tab: u64,
    },
    ModelReverse {
        tab: u64,
        name: String,
        /// Set for "Compare with Selection": diff instead of replacing the model.
        compare: Option<(Uuid, Engine, String, Option<String>)>,
    },
    NoSql {
        tab: u64,
        tag: String,
    },
    Job {
        id: Uuid,
        at: String,
        started: Instant,
    },
    Profile {
        tab: u64,
    },
    Dictionary,
    Fill {
        obj: ObjectRef,
    },
    Users {
        tab: u64,
    },
    Grants {
        tab: u64,
    },
    UserAdmin {
        tab: u64,
    },
    ProgrammableDrop {
        tab: u64,
    },
    RunFile {
        conn: Uuid,
    },
    Find {
        tab: u64,
    },
    Monitor {
        tab: u64,
    },
    Diagram {
        tab: u64,
    },
    SyncModel {
        tab: u64,
        target: bool,
    },
    SyncViews {
        tab: u64,
        target: bool,
    },
    SyncApply {
        tab: u64,
        target_conn: Uuid,
    },
    SyncData {
        tab: u64,
        table: String,
    },
    Kill {
        tab: u64,
    },
    Databases {
        conn: Uuid,
    },
    /// CREATE/DROP DATABASE finished; `done` is the success message.
    DatabaseChanged {
        conn: Uuid,
        done: &'static str,
    },
    ImportStructure,
    Import {
        conn: Uuid,
        obj: ObjectRef,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SavedTab {
    conn: Option<Uuid>,
    database: Option<String>,
    text: String,
    file: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SavedSession {
    tabs: Vec<SavedTab>,
}

struct Notice {
    text: String,
    error: bool,
    at: Instant,
}

/// Asks how many rows of test data to generate into a table.
struct FillDialog {
    conn: Uuid,
    obj: ObjectRef,
    rows: String,
}

struct ExportDialog {
    conn: Uuid,
    obj: ObjectRef,
    format: ExportFormat,
}

pub struct App {
    paths: AppPaths,
    secrets: Arc<dyn SecretStore>,
    worker: Worker,
    pub settings: Settings,
    pub connections: Vec<ConnectionConfig>,
    pub states: HashMap<Uuid, ConnState>,
    passwords: HashMap<Uuid, Secret>,
    tunnel_secrets: HashMap<Uuid, Secret>,
    pending: HashMap<JobId, Pending>,
    pub tabs: Vec<Tab>,
    pub active: usize,
    next_tab_id: u64,
    next_query_no: u64,
    pub selection: Option<NavSel>,
    nav_filter: String,
    objects_filter: String,
    ddl: HashMap<(Uuid, ObjectRef), Loadable<String>>,
    conn_dialog: Option<ConnDialog>,
    confirm: Option<Confirm>,
    prompt: Option<(PasswordPrompt, Action)>,
    export: Option<ExportDialog>,
    /// "New Database" name prompt: (connection, name).
    new_database: Option<(Uuid, String)>,
    import: Option<ImportDialog>,
    settings_open: bool,
    settings_draft: Settings,
    about_open: bool,
    show_nav: bool,
    show_info: bool,
    /// Sidebar sections.
    conns_open: bool,
    tools_open: bool,
    /// ⌘K: focus the top-bar search field next frame.
    focus_search: bool,
    /// Recent history for the Home dashboard, re-read every few seconds while Home is visible.
    home_recent: Option<(Instant, Vec<HistoryEntry>)>,
    notice: Option<Notice>,
    history: History,
    fill: Option<FillDialog>,
    workspace: crate::ui::workspace_dialog::WorkspaceDialog,
    jobs: Vec<talus_core::jobs::Job>,
    jobs_running: std::collections::HashSet<Uuid>,
    jobs_checked: Instant,
    actions: Vec<Action>,
    last_autosave: Instant,
    applied_theme: Option<(talus_core::store::config::Theme, f32)>,
    /// Set by `TALUS_STARTUP_PROBE`: report time to first frame, then close.
    pub startup_probe: Option<Instant>,
    /// Ids of clicked native menu items (macOS).
    native_menu: Option<std::sync::mpsc::Receiver<String>>,
}

impl App {
    pub fn new(ctx: &egui::Context, env: AppEnv) -> Self {
        theme::install_fonts(ctx);
        let repaint_ctx = ctx.clone();
        let worker =
            Worker::new(env.rt.clone(), env.paths.known_hosts_file(), Arc::new(move || repaint_ctx.request_repaint()));
        let repaint_ws = ctx.clone();
        let workspace = crate::ui::workspace_dialog::WorkspaceDialog::new(
            env.rt,
            Arc::new(move || repaint_ws.request_repaint()),
            env.secrets.clone(),
            env.paths.workspace_file(),
        );
        let mut notice = None;
        let settings = load_json::<Settings>(&env.paths.settings_file())
            .unwrap_or_else(|e| {
                notice = Some(Notice {
                    text: format!("Settings could not be read ({e}); using defaults"),
                    error: true,
                    at: Instant::now(),
                });
                Settings::default()
            })
            .sanitized();
        let connections = match load_json::<ConnectionsFile>(&env.paths.connections_file()) {
            Ok(f) => f.connections,
            Err(e) => {
                notice = Some(Notice {
                    text: format!("connections.json is unreadable and was left untouched: {e}"),
                    error: true,
                    at: Instant::now(),
                });
                Vec::new()
            }
        };
        let history = History::new(env.paths.history_file());
        let jobs_path = env.paths.jobs_file();
        let mut app = Self {
            paths: env.paths,
            secrets: env.secrets,
            worker,
            settings_draft: settings.clone(),
            settings,
            connections,
            states: HashMap::new(),
            passwords: HashMap::new(),
            tunnel_secrets: HashMap::new(),
            pending: HashMap::new(),
            tabs: vec![Tab { id: 0, kind: TabKind::Objects }],
            active: 0,
            next_tab_id: 1,
            next_query_no: 1,
            selection: None,
            nav_filter: String::new(),
            objects_filter: String::new(),
            ddl: HashMap::new(),
            conn_dialog: None,
            confirm: None,
            prompt: None,
            export: None,
            new_database: None,
            import: None,
            settings_open: false,
            about_open: false,
            show_nav: true,
            show_info: true,
            conns_open: true,
            tools_open: false,
            focus_search: false,
            home_recent: None,
            notice,
            history,
            fill: None,
            workspace,
            jobs: load_json::<talus_core::jobs::JobsFile>(&jobs_path).map(|f| f.jobs).unwrap_or_default(),
            jobs_running: Default::default(),
            jobs_checked: Instant::now(),
            actions: Vec::new(),
            last_autosave: Instant::now(),
            applied_theme: None,
            startup_probe: None,
            native_menu: None,
        };
        if app.settings.restore_session {
            app.restore_session();
        }
        app
    }

    // ---------- helpers ----------

    fn cfg(&self, id: Uuid) -> Option<&ConnectionConfig> {
        self.connections.iter().find(|c| c.id == id)
    }

    fn notify(&mut self, text: impl Into<String>, error: bool) {
        self.notice = Some(Notice { text: text.into(), error, at: Instant::now() });
    }

    fn save_connections(&mut self) {
        let file = ConnectionsFile { connections: self.connections.clone() };
        if let Err(e) = save_json(&self.paths.connections_file(), &file) {
            self.notify(format!("Could not save connections: {e}"), true);
        }
    }

    /// Connection info with password. `None` means a password prompt was opened and `then`
    /// will be re-dispatched after the user answers.
    fn conn_info(&mut self, id: Uuid, then: Action) -> Option<ConnInfo> {
        let cfg = self.cfg(id)?.clone();
        let password = if cfg.needs_password() {
            Some(self.resolve_secret(&cfg, SecretKind::DbPassword, then.clone())?)
        } else {
            None
        };
        let tunnel_secret =
            if cfg.has_tunnel() { Some(self.resolve_secret(&cfg, SecretKind::TunnelSecret, then)?) } else { None };
        Some(ConnInfo { cfg, password, tunnel_secret })
    }

    fn cache(&mut self, kind: SecretKind) -> &mut HashMap<Uuid, Secret> {
        match kind {
            SecretKind::DbPassword => &mut self.passwords,
            SecretKind::TunnelSecret => &mut self.tunnel_secrets,
        }
    }

    /// Cached → Keychain → prompt. Key-file SSH auth without a saved passphrase uses none.
    fn resolve_secret(&mut self, cfg: &ConnectionConfig, kind: SecretKind, then: Action) -> Option<Secret> {
        if let Some(p) = self.cache(kind).get(&cfg.id) {
            return Some(p.clone());
        }
        let (saved, ask) = match kind {
            SecretKind::DbPassword => (cfg.save_password, true),
            SecretKind::TunnelSecret => cfg.tunnel_secret_policy().unwrap_or((false, false)),
        };
        if saved {
            match self.secrets.get(&cfg.id, kind) {
                Ok(Some(p)) => {
                    self.cache(kind).insert(cfg.id, p.clone());
                    return Some(p);
                }
                Ok(None) => return Some(Secret::new("")),
                Err(e) => self.notify(e, true),
            }
        }
        if !ask {
            return Some(Secret::new(""));
        }
        self.prompt = Some((PasswordPrompt::new(cfg.id, cfg.name.clone(), kind), then));
        None
    }

    fn submit(
        &mut self,
        info: &ConnInfo,
        purpose: Purpose,
        database: Option<String>,
        req: Request,
        pending: Pending,
    ) -> JobId {
        let id = self.worker.submit(info, purpose, database, req);
        self.pending.insert(id, pending);
        id
    }

    fn new_tab_id(&mut self) -> u64 {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        id
    }

    fn tab_mut(&mut self, id: u64) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }

    fn push_tab(&mut self, kind: TabKind) -> u64 {
        let id = self.new_tab_id();
        self.tabs.push(Tab { id, kind });
        self.active = self.tabs.len() - 1;
        id
    }

    fn open_sql_tab(&mut self, conn: Option<Uuid>, database: Option<String>, text: String) -> u64 {
        let mut t = SqlTab::new(self.next_query_no, conn, database);
        self.next_query_no += 1;
        t.text = text;
        self.push_tab(TabKind::Sql(Box::new(t)))
    }

    // ---------- session persistence ----------

    fn restore_session(&mut self) {
        let Ok(s) = load_json::<SavedSession>(&self.paths.session_file()) else { return };
        for t in s.tabs {
            let conn = t.conn.filter(|c| self.cfg(*c).is_some());
            let id = self.open_sql_tab(conn, t.database, t.text);
            if let Some(Tab { kind: TabKind::Sql(st), .. }) = self.tab_mut(id) {
                st.file = t.file;
            }
        }
        self.active = 0;
    }

    pub fn save_session(&self) {
        if !self.settings.restore_session {
            return;
        }
        let tabs = self
            .tabs
            .iter()
            .filter_map(|t| match &t.kind {
                TabKind::Sql(s) => Some(SavedTab {
                    conn: s.conn,
                    database: s.database.clone(),
                    text: s.text.clone(),
                    file: s.file.clone(),
                }),
                _ => None,
            })
            .collect();
        if let Err(e) = save_json(&self.paths.session_file(), &SavedSession { tabs }) {
            tracing::warn!(error = %e, "session save failed");
        }
    }

    // ---------- worker events ----------

    /// Hands the app the receiver of the native menu bar; the in-window menu then stays hidden.
    pub fn set_native_menu(&mut self, rx: std::sync::mpsc::Receiver<String>) {
        self.native_menu = Some(rx);
    }

    fn native_menu_events(&mut self) {
        let ids: Vec<String> = self.native_menu.as_ref().map(|rx| rx.try_iter().collect()).unwrap_or_default();
        for id in ids {
            match id.as_str() {
                "about" => self.actions.push(Action::OpenAbout),
                "settings" => self.actions.push(Action::OpenSettings),
                "new-connection" => self.actions.push(Action::NewConnection),
                "new-query" => self.actions.push(Action::NewQuery { conn: None, database: None, text: String::new() }),
                "open-file" => self.actions.push(Action::OpenFile),
                "close-tab" => {
                    if let Some(t) = self.tabs.get(self.active) {
                        self.actions.push(Action::CloseTab(t.id));
                    }
                }
                "toggle-nav" => self.show_nav = !self.show_nav,
                "toggle-info" => self.show_info = !self.show_info,
                "sync" => self.actions.push(Action::OpenSync),
                "builder" => self.actions.push(Action::OpenBuilder),
                "model" => self.actions.push(Action::OpenModel),
                "users" => match &self.selection {
                    Some(sel) => self.actions.push(Action::OpenUsers(sel.conn())),
                    None => self.notify("Select a connection in the navigator first.", true),
                },
                "routines" => self.actions.push(Action::OpenRoutines),
                "dump-data" => self.actions.push(Action::DumpSql { include_data: true }),
                "dump-structure" => self.actions.push(Action::DumpSql { include_data: false }),
                "run-file" => self.actions.push(Action::RunSqlFile),
                "jobs" => self.actions.push(Action::OpenJobs),
                "workspace" => self.actions.push(Action::OpenWorkspace),
                "dict-html" => self.actions.push(Action::Dictionary { html: true }),
                "dict-md" => self.actions.push(Action::Dictionary { html: false }),
                "find" => self.actions.push(Action::OpenFind),
                "history" => self.actions.push(Action::OpenHistory),
                other => tracing::debug!(id = other, "unmapped native menu item"),
            }
        }
    }

    fn process_events(&mut self) {
        self.native_menu_events();
        let events: Vec<JobEvent> = self.worker.poll();
        for ev in events {
            let Some(p) = self.pending.remove(&ev.id) else { continue };
            self.route(ev, p);
        }
    }

    fn route(&mut self, ev: JobEvent, pending: Pending) {
        let conn = ev.conn;
        match (pending, ev.result) {
            (Pending::Test, r) => {
                if let Some(d) = self.conn_dialog.as_mut()
                    && matches!(d.test, TestState::Running(id) if id == ev.id)
                {
                    d.test = match r {
                        Ok(Response::Tested(v)) => TestState::Ok(v),
                        Ok(_) => TestState::Failed("unexpected response".into()),
                        Err(e) => TestState::Failed(e.to_string()),
                    };
                }
            }
            (Pending::Connect(c), r) => self.on_connected(c, r),
            (Pending::Schemas { conn, db }, r) => {
                let default_schema = self.cfg(conn).and_then(ConnectionConfig::default_schema);
                let st = self.states.entry(conn).or_default();
                if let Some(d) = st.db_mut(&db) {
                    d.schemas = match r {
                        Ok(Response::Schemas(list)) => Loadable::Loaded(
                            list.into_iter()
                                .map(|name| SchemaNode { name, expanded: false, objects: Loadable::NotLoaded })
                                .collect(),
                        ),
                        Ok(_) => Loadable::Failed("unexpected response".into()),
                        Err(e) => Loadable::Failed(e.to_string()),
                    };
                    // Auto-expand the default schema (`public`, `dbo`) like most clients do.
                    if let Some(list) = d.schemas.loaded_mut()
                        && let Some(def) = default_schema
                        && let Some(s) = list.iter_mut().find(|s| s.name == def)
                        && !s.expanded
                    {
                        s.expanded = true;
                        self.actions.push(Action::LoadObjects { conn, db: db.clone(), schema: Some(def) });
                    }
                }
            }
            (Pending::Objects { conn, db, schema }, r) => {
                let st = self.states.entry(conn).or_default();
                if let Some(slot) = st.objects_mut(&db, schema.as_deref()) {
                    *slot = match r {
                        Ok(Response::Objects(list)) => Loadable::Loaded(list.into_iter().map(ObjNode::new).collect()),
                        Ok(_) => Loadable::Failed("unexpected response".into()),
                        Err(e) => Loadable::Failed(e.to_string()),
                    };
                }
            }
            (Pending::TreeStructure { conn, obj }, r) => {
                if let Some(n) = self.states.entry(conn).or_default().obj_node_mut(&obj) {
                    n.structure = match r {
                        Ok(Response::Structure(s)) => Loadable::Loaded(s),
                        Ok(_) => Loadable::Failed("unexpected response".into()),
                        Err(e) => Loadable::Failed(e.to_string()),
                    };
                }
            }
            (Pending::InfoDdl { conn, obj }, r) => {
                let v = match r {
                    Ok(Response::Ddl(s)) => Loadable::Loaded(s),
                    Ok(_) => Loadable::Failed("unexpected response".into()),
                    Err(e) => Loadable::Failed(e.to_string()),
                };
                self.ddl.insert((conn, obj), v);
            }
            (Pending::Script { tab }, r) => self.on_script(tab, conn, r),
            (Pending::DataStructure { tab }, r) => {
                let mut load = false;
                if let Some(Tab { kind: TabKind::Data(d), .. }) = self.tab_mut(tab) {
                    d.loading = None;
                    match r {
                        Ok(Response::Structure(s)) => {
                            d.structure = Loadable::Loaded(s);
                            load = true;
                        }
                        Ok(_) => d.structure = Loadable::Failed("unexpected response".into()),
                        Err(e) => {
                            d.error = Some(e.to_string());
                            d.structure = Loadable::Failed(e.to_string());
                        }
                    }
                }
                if load {
                    self.load_page(tab);
                }
            }
            (Pending::Page { tab, started }, r) => {
                if let Some(Tab { kind: TabKind::Data(d), .. }) = self.tab_mut(tab)
                    && d.loading == Some(ev.id)
                {
                    d.loading = None;
                    d.elapsed = Some(started.elapsed());
                    match r {
                        Ok(Response::Page(p)) => {
                            d.page = Some(p);
                            d.grid.reset();
                            d.error = None;
                        }
                        Ok(_) => d.error = Some("unexpected response".into()),
                        Err(e) => d.error = Some(e.to_string()),
                    }
                }
            }
            (Pending::Apply { tab }, r) => {
                let mut reload = false;
                if let Some(Tab { kind: TabKind::Data(d), .. }) = self.tab_mut(tab) {
                    d.applying = None;
                    match r {
                        Ok(Response::Applied(rep)) => {
                            d.discard();
                            d.info = Some(format!(
                                "Saved: {} updated, {} inserted, {} deleted",
                                rep.updated, rep.inserted, rep.deleted
                            ));
                            d.error = None;
                            reload = true;
                        }
                        Ok(_) => d.error = Some("unexpected response".into()),
                        // Pending edits are kept so the user can fix and retry.
                        Err(e) => d.error = Some(format!("Changes were not saved: {e}")),
                    }
                }
                if reload {
                    self.load_page(tab);
                }
            }
            (Pending::StructureTab { tab }, r) => {
                if let Some(Tab { kind: TabKind::Structure(s), .. }) = self.tab_mut(tab) {
                    s.structure = match r {
                        Ok(Response::Structure(st)) => Loadable::Loaded(st),
                        Ok(_) => Loadable::Failed("unexpected response".into()),
                        Err(e) => Loadable::Failed(e.to_string()),
                    };
                }
            }
            (Pending::DesignStructure { tab, obj }, r) => {
                if let Some(Tab { kind: TabKind::Design(d), .. }) = self.tab_mut(tab) {
                    d.loading = None;
                    match r {
                        Ok(Response::Structure(st)) => d.set_structure(obj, &st),
                        Ok(_) => d.error = Some("unexpected response".into()),
                        Err(e) => d.error = Some(e.to_string()),
                    }
                }
            }
            (Pending::DesignSave { tab, obj }, r) => {
                let ok = matches!(r, Ok(Response::BatchDone(_)));
                if let Some(Tab { kind: TabKind::Design(d), .. }) = self.tab_mut(tab) {
                    d.saving = None;
                    match r {
                        Ok(Response::BatchDone(n)) => {
                            d.error = None;
                            d.info = Some(format!("Saved ({n} statement{}).", if n == 1 { "" } else { "s" }));
                        }
                        Ok(_) => d.error = Some("unexpected response".into()),
                        Err(e) => d.error = Some(e.to_string()),
                    }
                }
                if ok {
                    // Reload the designer from the database and refresh the navigator.
                    self.invalidate_objects(conn);
                    self.ddl.remove(&(conn, obj.clone()));
                    if let Some(info) = self.conn_info(conn, Action::ActivateTab(tab)) {
                        let job = self.submit(
                            &info,
                            Purpose::Meta,
                            Some(obj.database.clone()),
                            Request::Structure(obj.clone()),
                            Pending::DesignStructure { tab, obj },
                        );
                        if let Some(Tab { kind: TabKind::Design(d), .. }) = self.tab_mut(tab) {
                            d.loading = Some(job);
                        }
                    }
                }
            }
            (Pending::Destructive { conn, db, schema }, r) => {
                match r {
                    Ok(Response::Script(o)) if o.errors.is_empty() => self.notify("Done.", false),
                    Ok(Response::Script(o)) => {
                        let msg = o.errors.first().map(|e| e.error.to_string()).unwrap_or_default();
                        self.notify(format!("Failed: {msg}"), true);
                    }
                    Ok(_) => {}
                    Err(e) => self.notify(format!("Failed: {e}"), true),
                }
                self.actions.push(Action::LoadObjects { conn, db, schema });
            }
            (Pending::SyncModel { tab, target }, r) => {
                let engines: HashMap<Uuid, Engine> = self.connections.iter().map(|c| (c.id, c.engine)).collect();
                if let Some(Tab { kind: TabKind::Sync(t), .. }) = self.tab_mut(tab) {
                    let side = if target { &mut t.target } else { &mut t.source };
                    side.model = match r {
                        Ok(Response::Model(m)) => Loadable::Loaded(m),
                        Ok(_) => Loadable::Failed("unexpected response".into()),
                        Err(e) => Loadable::Failed(e.to_string()),
                    };
                    t.recompute(|c| engines.get(&c).copied());
                }
            }
            (Pending::SyncViews { tab, target }, r) => {
                let engines: HashMap<Uuid, Engine> = self.connections.iter().map(|c| (c.id, c.engine)).collect();
                if let Some(Tab { kind: TabKind::Sync(t), .. }) = self.tab_mut(tab) {
                    let side = if target { &mut t.target } else { &mut t.source };
                    side.views = match r {
                        Ok(Response::Views(v)) => Loadable::Loaded(v),
                        // Views are an extra: a failure leaves the table plan alone.
                        Ok(_) | Err(_) => Loadable::NotLoaded,
                    };
                    t.recompute(|c| engines.get(&c).copied());
                }
            }
            (Pending::SyncApply { tab, target_conn }, r) => {
                let result = match r {
                    Ok(Response::BatchDone(n)) => Ok(n),
                    Ok(_) => Err("unexpected response".to_string()),
                    Err(e) => Err(e.to_string()),
                };
                if result.is_ok() {
                    self.invalidate_objects(target_conn);
                    // Re-compare so the plan reflects the new target state.
                    self.compare_sync(tab);
                }
                if let Some(Tab { kind: TabKind::Sync(t), .. }) = self.tab_mut(tab) {
                    t.running = None;
                    match result {
                        Ok(n) => {
                            t.info = Some(format!("Applied {n} statement(s) to the target."));
                            t.error = None;
                        }
                        Err(e) => t.error = Some(e),
                    }
                }
            }
            (Pending::SyncData { tab, table }, r) => {
                let result = match r {
                    Ok(Response::DataSync(rep)) => Ok(rep),
                    Ok(_) => Err("unexpected response".to_string()),
                    Err(e) => Err(e.to_string()),
                };
                if let Some(Tab { kind: TabKind::Sync(t), .. }) = self.tab_mut(tab) {
                    t.data_queue.retain(|n| *n != table);
                    t.data_results.push((table, result));
                    t.running = None;
                }
                self.next_data_sync(tab);
            }
            (Pending::Diagram { tab }, r) => {
                if let Some(Tab { kind: TabKind::Diagram(d), .. }) = self.tab_mut(tab) {
                    d.loading = None;
                    match r {
                        Ok(Response::Model(m)) => d.set_model(m),
                        Ok(_) => d.model = Loadable::Failed("unexpected response".into()),
                        Err(e) => d.model = Loadable::Failed(e.to_string()),
                    }
                }
            }
            (Pending::Monitor { tab }, r) => {
                if let Some(Tab { kind: TabKind::Monitor(m), .. }) = self.tab_mut(tab) {
                    m.running = None;
                    m.last_refresh = Some(Instant::now());
                    match r {
                        Ok(Response::Script(o)) => match (o.results.into_iter().next(), o.errors.into_iter().next()) {
                            (_, Some(e)) => m.error = Some(e.error.to_string()),
                            (Some(res), None) => {
                                if let Outcome::Rows(q) = res.outcome {
                                    m.result = Some(q);
                                    m.error = None;
                                }
                            }
                            (None, None) => {}
                        },
                        Ok(_) => m.error = Some("unexpected response".into()),
                        Err(e) => m.error = Some(e.to_string()),
                    }
                }
            }
            (Pending::Kill { tab }, r) => {
                let msg = match r {
                    Ok(Response::Script(o)) if o.errors.is_empty() => None,
                    Ok(Response::Script(o)) => o.errors.first().map(|e| e.error.to_string()),
                    Ok(_) => None,
                    Err(e) => Some(e.to_string()),
                };
                match msg {
                    Some(m) => self.notify(format!("Kill failed: {m}"), true),
                    None => self.notify("Signal sent.", false),
                }
                self.refresh_monitor(tab);
            }
            (Pending::Databases { conn }, r) => match r {
                Ok(Response::Databases(names)) => {
                    let st = self.states.entry(conn).or_default();
                    let mut old = std::mem::take(&mut st.databases);
                    st.databases = names
                        .into_iter()
                        .map(|n| match old.iter().position(|d| d.name == n) {
                            Some(i) => old.swap_remove(i),
                            None => DbNode::new(n),
                        })
                        .collect();
                }
                Ok(_) => {}
                Err(e) => self.notify(format!("Could not list databases: {e}"), true),
            },
            (Pending::DatabaseChanged { conn, done }, r) => {
                match r {
                    Ok(Response::BatchDone(_)) => self.notify(done, false),
                    Ok(_) => {}
                    Err(e) => self.notify(format!("Failed: {e}"), true),
                }
                self.reload_databases(conn);
            }
            (Pending::Find { tab }, r) => {
                if let Some(Tab { kind: TabKind::Find(f), .. }) = self.tab_mut(tab) {
                    f.running = None;
                    match r {
                        Ok(Response::Found(h)) => {
                            f.hits = Some(h);
                            f.error = None;
                        }
                        Ok(_) => f.error = Some("unexpected response".into()),
                        Err(e) => f.error = Some(e.to_string()),
                    }
                }
            }
            (Pending::ImportStructure, r) => {
                if let Some(d) = self.import.as_mut() {
                    match r {
                        Ok(Response::Structure(st)) => d.set_structure(st),
                        Ok(_) => d.error = Some("unexpected response".into()),
                        Err(e) => d.error = Some(e.to_string()),
                    }
                }
            }
            (Pending::Import { conn, obj }, r) => {
                if let Some(d) = self.import.as_mut() {
                    d.running = None;
                }
                match r {
                    Ok(Response::Imported(n)) => {
                        self.import = None;
                        self.notify(format!("Imported {n} rows into {}", obj.display_name()), false);
                        let tabs: Vec<u64> = self.tabs.iter().filter(|t| matches!(&t.kind, TabKind::Data(d) if d.conn == conn && d.obj == obj && !d.is_dirty())).map(|t| t.id).collect();
                        for t in tabs {
                            self.load_page(t);
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        if let Some(d) = self.import.as_mut() {
                            d.error = Some(e.to_string());
                        }
                    }
                }
            }
            (Pending::Users { tab }, r) => {
                if let Some(Tab { kind: TabKind::Users(t), .. }) = self.tab_mut(tab) {
                    t.users = match r {
                        Ok(Response::Users(u)) => Loadable::Loaded(u),
                        Ok(_) => Loadable::Failed("unexpected response".into()),
                        Err(e) => Loadable::Failed(e.to_string()),
                    };
                    t.selected = None;
                    t.grants = Loadable::NotLoaded;
                }
            }
            (Pending::Grants { tab }, r) => {
                if let Some(Tab { kind: TabKind::Users(t), .. }) = self.tab_mut(tab) {
                    t.grants = match r {
                        Ok(Response::Grants(g)) => Loadable::Loaded(g),
                        Ok(_) => Loadable::Failed("unexpected response".into()),
                        Err(e) => Loadable::Failed(e.to_string()),
                    };
                }
            }
            (Pending::UserAdmin { tab }, r) => {
                let ok = r.is_ok();
                if let Some(Tab { kind: TabKind::Users(t), .. }) = self.tab_mut(tab) {
                    match r {
                        Ok(_) => {
                            t.error = None;
                            t.form = None;
                        }
                        Err(e) => t.error = Some(e.to_string()),
                    }
                }
                if ok {
                    self.load_users(tab);
                }
            }
            (Pending::ProgrammableDrop { tab }, r) => match r {
                Ok(_) => {
                    self.notify("Dropped.", false);
                    self.load_routines(tab);
                }
                Err(e) => self.notify(format!("Failed: {e}"), true),
            },
            (Pending::Profile { tab }, r) => {
                if let Some(Tab { kind: TabKind::Profile(p), .. }) = self.tab_mut(tab) {
                    p.columns = match r {
                        Ok(Response::Profile(c)) => Loadable::Loaded(c),
                        Ok(_) => Loadable::Failed("unexpected response".into()),
                        Err(e) => Loadable::Failed(e.to_string()),
                    };
                }
            }
            (Pending::Dictionary, r) => match r {
                Ok(Response::DictionaryWritten { tables, path }) => {
                    self.notify(format!("Documented {tables} table(s) in {}", path.display()), false)
                }
                Ok(_) => {}
                Err(e) => self.notify(format!("Data dictionary failed: {e}"), true),
            },
            (Pending::Fill { obj }, r) => match r {
                Ok(Response::Filled(n)) => {
                    self.notify(format!("Inserted {n} generated row(s) into {}", obj.display_name()), false)
                }
                Ok(_) => {}
                Err(e) => self.notify(format!("Generating data failed: {e}"), true),
            },
            (Pending::Job { id, at, started }, r) => {
                let (ok, message) = match r {
                    Ok(Response::JobDone(m)) => (true, m),
                    Ok(_) => (false, "unexpected response".to_string()),
                    Err(e) => (false, e.to_string()),
                };
                self.finish_job(
                    id,
                    talus_core::jobs::JobRun { at, ok, message, duration_ms: started.elapsed().as_millis() as u64 },
                );
            }
            (Pending::NoSql { tab, tag }, r) => {
                let r = match r {
                    Ok(Response::NoSql(o)) => Ok(o),
                    Ok(_) => Err("unexpected response".to_string()),
                    Err(e) => Err(e.to_string()),
                };
                let (cmds, dropped) = match self.tab_mut(tab).map(|t| &mut t.kind) {
                    Some(TabKind::Keys(k)) => (k.on_result(&tag, r), false),
                    Some(TabKind::Docs(d)) => {
                        let cmds = d.on_result(&tag, r);
                        (cmds, d.dropped)
                    }
                    Some(TabKind::Console(c)) => {
                        c.on_result(r);
                        (Vec::new(), false)
                    }
                    _ => (Vec::new(), false),
                };
                if dropped {
                    self.invalidate_objects(conn);
                    self.actions.push(Action::CloseTab(tab));
                    self.notify("Collection dropped.", false);
                }
                self.run_nosql_cmds(tab, cmds);
            }
            (Pending::Builder { tab }, r) => {
                if let Some(Tab { kind: TabKind::Builder(b), .. }) = self.tab_mut(tab) {
                    match r {
                        Ok(Response::Model(m)) => b.set_model(m),
                        Ok(_) => b.model = Loadable::Failed("unexpected response".into()),
                        Err(e) => b.model = Loadable::Failed(e.to_string()),
                    }
                }
            }
            (Pending::ModelReverse { tab, name, compare }, r) => {
                if let Some(Tab { kind: TabKind::Model(m), .. }) = self.tab_mut(tab) {
                    match (r, compare) {
                        (Ok(Response::Model(tables)), Some((conn, engine, db, schema))) => {
                            m.compare_with(conn, engine, &db, schema.as_deref(), &tables)
                        }
                        (Ok(Response::Model(tables)), None) => {
                            m.set_model(talus_core::model_design::DesignModel::reverse(&name, &tables))
                        }
                        (Ok(_), _) => m.fail("unexpected response".into()),
                        (Err(e), _) => m.fail(e.to_string()),
                    }
                }
            }
            (Pending::Routines { tab }, r) => {
                if let Some(Tab { kind: TabKind::Routines(t), .. }) = self.tab_mut(tab) {
                    match r {
                        Ok(Response::Programmables(items)) => t.set_items(items),
                        Ok(_) => t.items = Loadable::Failed("unexpected response".into()),
                        Err(e) => t.items = Loadable::Failed(e.to_string()),
                    }
                }
            }
            (Pending::DebugDescribe { tab }, r) => {
                if let Some(Tab { kind: TabKind::Debug(t), .. }) = self.tab_mut(tab) {
                    match r {
                        Ok(Response::DebugTarget(target)) => t.set_target(*target),
                        Ok(_) => t.fail("unexpected response".into()),
                        Err(e) => t.fail(e.to_string()),
                    }
                }
            }
            (Pending::Debug { tab }, r) => {
                if let Some(Tab { kind: TabKind::Debug(t), .. }) = self.tab_mut(tab) {
                    match r {
                        Ok(Response::Debug(ev)) => t.on_event(ev),
                        Ok(_) => t.fail("unexpected response".into()),
                        Err(e) => t.fail(e.to_string()),
                    }
                }
            }
            (Pending::Dump, r) => match r {
                Ok(Response::Dumped(rep, path)) => self.notify(
                    format!(
                        "Dumped {} table(s), {} view(s), {} row(s) to {}",
                        rep.tables,
                        rep.views,
                        rep.rows,
                        path.display()
                    ),
                    false,
                ),
                Ok(_) => {}
                Err(e) => self.notify(format!("Dump failed: {e}"), true),
            },
            (Pending::RunFile { conn }, r) => match r {
                Ok(Response::BatchDone(n)) => {
                    self.invalidate_objects(conn);
                    self.notify(format!("Executed {n} statement(s) from the file."), false);
                }
                Ok(_) => {}
                Err(e) => {
                    self.invalidate_objects(conn);
                    self.notify(format!("SQL file failed: {e}"), true);
                }
            },
            (Pending::Export, r) => match r {
                Ok(Response::Exported { rows, path }) => {
                    self.notify(format!("Exported {rows} rows to {}", path.display()), false)
                }
                Ok(_) => {}
                Err(e) => self.notify(format!("Export failed: {e}"), true),
            },
        }
    }

    fn on_connected(&mut self, conn: Uuid, r: Result<Response, talus_core::DbError>) {
        let engine = self.cfg(conn).map(|c| c.engine);
        let initial_db = self.cfg(conn).map(|c| c.database.clone()).unwrap_or_default();
        let st = self.states.entry(conn).or_default();
        match r {
            Ok(Response::Connected { version, databases }) => {
                st.status = ConnStatus::Open { version };
                st.expanded = true;
                st.databases = databases.into_iter().map(DbNode::new).collect();
                // Expand the obvious database: the configured one, or the only one.
                let target = match engine {
                    Some(Engine::Sqlite) => Some("main".to_string()),
                    // Redis databases are listed as db0, db1, …; the dialog stores the bare index.
                    Some(Engine::Redis) => {
                        Some(format!("db{}", initial_db.trim_start_matches("db").trim().parse::<u32>().unwrap_or(0)))
                    }
                    Some(e @ (Engine::Postgres | Engine::SqlServer)) => Some(if initial_db.is_empty() {
                        e.default_database().unwrap_or_default().to_string()
                    } else {
                        initial_db
                    }),
                    _ => Some(initial_db)
                        .filter(|d| !d.is_empty())
                        .or_else(|| (st.databases.len() == 1).then(|| st.databases[0].name.clone())),
                };
                if let Some(t) = target
                    && let Some(d) = st.db_mut(&t)
                {
                    d.expanded = true;
                    let action = if engine.is_some_and(|e| e.capabilities().schemas) {
                        Action::LoadSchemas { conn, db: t.clone() }
                    } else {
                        Action::LoadObjects { conn, db: t.clone(), schema: None }
                    };
                    self.actions.push(action);
                }
                if self.selection.is_none() {
                    self.selection = Some(NavSel::Conn(conn));
                }
            }
            Ok(_) => st.status = ConnStatus::Failed("unexpected response".into()),
            Err(e) => {
                st.status = ConnStatus::Failed(e.to_string());
                // A prompted password that failed must be asked again next time.
                if self.cfg(conn).is_some_and(|c| !c.save_password) {
                    self.passwords.remove(&conn);
                }
                if self.cfg(conn).and_then(ConnectionConfig::tunnel_secret_policy).is_some_and(|(saved, _)| !saved) {
                    self.tunnel_secrets.remove(&conn);
                }
            }
        }
    }

    fn on_script(&mut self, tab: u64, conn: Uuid, r: Result<Response, talus_core::DbError>) {
        let conn_name = self.cfg(conn).map(|c| c.name.clone()).unwrap_or_default();
        let outcome = match r {
            Ok(Response::Script(o)) => Ok(o),
            Ok(_) => Err(talus_core::DbError::Invalid("unexpected response".into())),
            Err(e) => Err(e),
        };
        let mut ddl_ran = false;
        // History changes below; Home re-reads it on its next frame.
        self.home_recent = None;
        if let Ok(o) = &outcome {
            let database = match self.tabs.iter().find(|t| t.id == tab).map(|t| &t.kind) {
                Some(TabKind::Sql(s)) => s.database.clone().unwrap_or_default(),
                _ => String::new(),
            };
            let now = HistoryEntry::now_ms();
            for res in &o.results {
                let (rows_msg, _) = match &res.outcome {
                    Outcome::Rows(q) => (format!("{} rows", q.rows.len()), true),
                    Outcome::Affected(n) => (format!("{n} affected"), false),
                };
                let head = res.sql.split_whitespace().next().unwrap_or("").to_ascii_uppercase();
                ddl_ran |= matches!(head.as_str(), "CREATE" | "DROP" | "ALTER" | "RENAME" | "TRUNCATE");
                let _ = self.history.append(&HistoryEntry {
                    ts_ms: now,
                    connection: conn_name.clone(),
                    database: database.clone(),
                    sql: res.sql.clone(),
                    elapsed_ms: res.elapsed.as_millis() as u64,
                    ok: true,
                    message: rows_msg,
                });
            }
            for e in &o.errors {
                let _ = self.history.append(&HistoryEntry {
                    ts_ms: now,
                    connection: conn_name.clone(),
                    database: database.clone(),
                    sql: e.sql.clone(),
                    elapsed_ms: 0,
                    ok: false,
                    message: e.error.to_string(),
                });
            }
        }
        if let Some(Tab { kind: TabKind::Sql(s), .. }) = self.tab_mut(tab) {
            s.finish(outcome);
        }
        if ddl_ran {
            self.invalidate_objects(conn);
        }
    }

    /// Marks loaded object lists of a connection stale and reloads the expanded ones.
    fn invalidate_objects(&mut self, conn: Uuid) {
        let Some(st) = self.states.get_mut(&conn) else { return };
        let mut reload = Vec::new();
        for d in &mut st.databases {
            if !matches!(d.objects, Loadable::NotLoaded) {
                d.objects = Loadable::NotLoaded;
                if d.expanded {
                    reload.push((d.name.clone(), None));
                }
            }
            if let Some(list) = d.schemas.loaded_mut() {
                for s in list {
                    if !matches!(s.objects, Loadable::NotLoaded) {
                        s.objects = Loadable::NotLoaded;
                        if s.expanded {
                            reload.push((d.name.clone(), Some(s.name.clone())));
                        }
                    }
                }
            }
        }
        for (db, schema) in reload {
            self.actions.push(Action::LoadObjects { conn, db, schema });
        }
    }

    fn load_page(&mut self, tab: u64) {
        let Some(Tab { kind: TabKind::Data(d), .. }) = self.tabs.iter().find(|t| t.id == tab) else { return };
        let (conn, db) = (d.conn, d.obj.database.clone());
        let Some(structure) = d.structure.loaded().cloned() else { return };
        let query = d.page_query();
        let Some(info) = self.conn_info(conn, Action::ActivateTab(tab)) else { return };
        let job = self.submit(
            &info,
            Purpose::Tab(tab),
            Some(db),
            Request::FetchPage { query, structure },
            Pending::Page { tab, started: Instant::now() },
        );
        if let Some(Tab { kind: TabKind::Data(d), .. }) = self.tab_mut(tab) {
            d.loading = Some(job);
        }
    }

    // ---------- actions ----------

    fn run_actions(&mut self) {
        let mut guard = 0;
        while !self.actions.is_empty() && guard < 64 {
            guard += 1;
            let batch = std::mem::take(&mut self.actions);
            for a in batch {
                self.handle(a);
            }
        }
    }

    pub fn dispatch(&mut self, a: Action) {
        self.actions.push(a);
        self.run_actions();
    }

    fn handle(&mut self, a: Action) {
        match a {
            Action::NewConnection => self.conn_dialog = Some(ConnDialog::new_connection()),
            Action::EditConnection(id) => {
                if let Some(cfg) = self.cfg(id).cloned() {
                    let pw = self
                        .passwords
                        .get(&id)
                        .map(|p| p.expose().to_string())
                        .or_else(|| {
                            cfg.save_password
                                .then(|| {
                                    self.secrets
                                        .get(&id, SecretKind::DbPassword)
                                        .ok()
                                        .flatten()
                                        .map(|s| s.expose().to_string())
                                })
                                .flatten()
                        })
                        .unwrap_or_default();
                    let ssh_pw = self
                        .tunnel_secrets
                        .get(&id)
                        .map(|p| p.expose().to_string())
                        .or_else(|| {
                            cfg.tunnel_secret_policy().filter(|(saved, _)| *saved).and_then(|_| {
                                self.secrets
                                    .get(&id, SecretKind::TunnelSecret)
                                    .ok()
                                    .flatten()
                                    .map(|s| s.expose().to_string())
                            })
                        })
                        .unwrap_or_default();
                    self.conn_dialog = Some(ConnDialog::edit(cfg, pw, ssh_pw, false));
                }
            }
            Action::DuplicateConnection(id) => {
                if let Some(cfg) = self.cfg(id).cloned() {
                    let pw = self.passwords.get(&id).map(|p| p.expose().to_string()).unwrap_or_default();
                    let copy = ConnectionConfig { id: Uuid::new_v4(), name: format!("{} copy", cfg.name), ..cfg };
                    let ssh_pw = self.tunnel_secrets.get(&id).map(|p| p.expose().to_string()).unwrap_or_default();
                    self.conn_dialog = Some(ConnDialog::edit(copy, pw, ssh_pw, true));
                }
            }
            Action::AskDeleteConnection(id) => {
                if let Some(cfg) = self.cfg(id) {
                    self.confirm = Some(Confirm {
                        title: "Delete Connection".into(),
                        message: format!(
                            "Delete the connection “{}”? Its saved password is removed from the Keychain. Databases on the server are not affected.",
                            cfg.name
                        ),
                        sql: None,
                        confirm_label: "Delete".into(),
                        on_confirm: Confirmed::DeleteConnection(id),
                    });
                }
            }
            Action::Open(id) => {
                let Some(info) = self.conn_info(id, Action::Open(id)) else { return };
                let job = self.submit(&info, Purpose::Meta, None, Request::Connect, Pending::Connect(id));
                self.states.entry(id).or_default().status = ConnStatus::Connecting(job);
            }
            Action::Close(id) => {
                self.worker.disconnect(id);
                self.states.insert(id, ConnState::default());
                self.ddl.retain(|(c, _), _| *c != id);
                if self.selection.as_ref().is_some_and(|s| s.conn() == id) {
                    self.selection = Some(NavSel::Conn(id));
                }
                for t in &mut self.tabs {
                    if let TabKind::Sql(s) = &mut t.kind
                        && s.conn == Some(id)
                    {
                        if let Some(job) = s.running.take() {
                            self.worker.cancel(job);
                        }
                        s.started = None;
                    }
                }
            }
            Action::Refresh(sel) => self.refresh(sel),
            Action::LoadSchemas { conn, db } => {
                let Some(info) = self.conn_info(conn, Action::LoadSchemas { conn, db: db.clone() }) else { return };
                let job = self.submit(
                    &info,
                    Purpose::Meta,
                    Some(db.clone()),
                    Request::ListSchemas,
                    Pending::Schemas { conn, db: db.clone() },
                );
                if let Some(d) = self.states.entry(conn).or_default().db_mut(&db) {
                    d.schemas = Loadable::Loading(job);
                }
            }
            Action::LoadObjects { conn, db, schema } => {
                let slot_ok = self
                    .states
                    .get_mut(&conn)
                    .and_then(|s| s.objects_mut(&db, schema.as_deref()))
                    .is_some_and(|s| !s.is_loading());
                if !slot_ok {
                    return;
                }
                let Some(info) =
                    self.conn_info(conn, Action::LoadObjects { conn, db: db.clone(), schema: schema.clone() })
                else {
                    return;
                };
                let job = self.submit(
                    &info,
                    Purpose::Meta,
                    Some(db.clone()),
                    Request::ListObjects { database: db.clone(), schema: schema.clone() },
                    Pending::Objects { conn, db: db.clone(), schema: schema.clone() },
                );
                if let Some(s) = self.states.entry(conn).or_default().objects_mut(&db, schema.as_deref()) {
                    *s = Loadable::Loading(job);
                }
            }
            Action::LoadStructure { conn, obj } => {
                let Some(info) = self.conn_info(conn, Action::LoadStructure { conn, obj: obj.clone() }) else { return };
                let job = self.submit(
                    &info,
                    Purpose::Meta,
                    Some(obj.database.clone()),
                    Request::Structure(obj.clone()),
                    Pending::TreeStructure { conn, obj: obj.clone() },
                );
                if let Some(n) = self.states.entry(conn).or_default().obj_node_mut(&obj) {
                    n.structure = Loadable::Loading(job);
                }
            }
            Action::Select(sel) => {
                if let NavSel::Obj(conn, obj) = &sel {
                    let key = (*conn, obj.clone());
                    // Redis and MongoDB have no DDL to show.
                    let has_ddl = self.cfg(*conn).is_some_and(|c| c.engine.is_sql());
                    if has_ddl
                        && matches!(self.ddl.get(&key), None | Some(Loadable::NotLoaded))
                        && let Some(info) = self.conn_info(*conn, Action::Select(sel.clone()))
                    {
                        let job = self.submit(
                            &info,
                            Purpose::Meta,
                            Some(obj.database.clone()),
                            Request::Ddl(obj.clone()),
                            Pending::InfoDdl { conn: *conn, obj: obj.clone() },
                        );
                        self.ddl.insert(key, Loadable::Loading(job));
                    }
                }
                self.selection = Some(sel);
            }
            Action::OpenData { conn, obj } if self.cfg(conn).is_some_and(|c| !c.engine.is_sql()) => {
                self.open_nosql_object(conn, obj);
            }
            Action::OpenData { conn, obj } => {
                // Reuse an existing tab for the same object.
                if let Some(i) =
                    self.tabs.iter().position(|t| matches!(&t.kind, TabKind::Data(d) if d.conn == conn && d.obj == obj))
                {
                    self.active = i;
                    return;
                }
                let Some(engine) = self.cfg(conn).map(|c| c.engine) else { return };
                let Some(info) = self.conn_info(conn, Action::OpenData { conn, obj: obj.clone() }) else { return };
                let tab = self.push_tab(TabKind::Data(Box::new(DataTab::new(
                    conn,
                    engine,
                    obj.clone(),
                    self.settings.page_size,
                ))));
                let job = self.submit(
                    &info,
                    Purpose::Tab(tab),
                    Some(obj.database.clone()),
                    Request::Structure(obj),
                    Pending::DataStructure { tab },
                );
                if let Some(Tab { kind: TabKind::Data(d), .. }) = self.tab_mut(tab) {
                    d.structure = Loadable::Loading(job);
                    d.loading = Some(job);
                }
            }
            Action::OpenStructure { conn, obj } => {
                let Some(info) = self.conn_info(conn, Action::OpenStructure { conn, obj: obj.clone() }) else { return };
                let tab = self.push_tab(TabKind::Structure(StructureTab {
                    conn,
                    obj: obj.clone(),
                    structure: Loadable::NotLoaded,
                }));
                let job = self.submit(
                    &info,
                    Purpose::Meta,
                    Some(obj.database.clone()),
                    Request::Structure(obj),
                    Pending::StructureTab { tab },
                );
                if let Some(Tab { kind: TabKind::Structure(s), .. }) = self.tab_mut(tab) {
                    s.structure = Loadable::Loading(job);
                }
            }
            Action::DesignTable { conn, obj } => {
                if let Some(i) = self.tabs.iter().position(|t| matches!(&t.kind, TabKind::Design(d) if d.conn == conn && d.existing.as_ref().is_some_and(|(o, _)| *o == obj))) {
                    self.active = i;
                    return;
                }
                let Some(engine) = self.cfg(conn).map(|c| c.engine) else { return };
                let Some(info) = self.conn_info(conn, Action::DesignTable { conn, obj: obj.clone() }) else { return };
                let tab = self.push_tab(TabKind::Design(Box::new(DesignTab::for_table(conn, engine, &obj))));
                let job = self.submit(
                    &info,
                    Purpose::Meta,
                    Some(obj.database.clone()),
                    Request::Structure(obj.clone()),
                    Pending::DesignStructure { tab, obj },
                );
                if let Some(Tab { kind: TabKind::Design(d), .. }) = self.tab_mut(tab) {
                    d.loading = Some(job);
                }
            }
            Action::NewTable { conn, db, schema } => {
                let Some(engine) = self.cfg(conn).map(|c| c.engine) else { return };
                self.push_tab(TabKind::Design(Box::new(DesignTab::new_table(conn, engine, db, schema))));
            }
            Action::NewQuery { conn, database, text } => {
                let conn = conn.or_else(|| self.selection.as_ref().map(NavSel::conn));
                let database =
                    database.or_else(|| self.selection.as_ref().and_then(|s| s.database().map(String::from)));
                match conn.and_then(|c| self.cfg(c)).map(|c| (c.id, c.name.clone(), c.engine)) {
                    Some((id, name, engine)) if !engine.is_sql() => {
                        let db =
                            database.unwrap_or_else(|| {
                                self.cfg(id)
                                    .map(|c| c.database.clone())
                                    .filter(|d| !d.is_empty())
                                    .or_else(|| engine.default_database().map(String::from))
                                    .map(|d| {
                                        if engine == Engine::Redis && !d.starts_with("db") {
                                            format!("db{d}")
                                        } else {
                                            d
                                        }
                                    })
                                    .unwrap_or_default()
                            });
                        self.push_tab(TabKind::Console(Box::new(ConsoleTab::new(id, name, engine, db))));
                    }
                    _ => {
                        self.open_sql_tab(conn, database, text);
                    }
                }
            }
            Action::Import { conn, obj } => {
                let Some(info) = self.conn_info(conn, Action::Import { conn, obj: obj.clone() }) else { return };
                let mut dlg = ImportDialog::new(conn, obj.clone());
                let job = self.submit(
                    &info,
                    Purpose::Meta,
                    Some(obj.database.clone()),
                    Request::Structure(obj),
                    Pending::ImportStructure,
                );
                dlg.structure = Loadable::Loading(job);
                self.import = Some(dlg);
            }
            Action::Export { conn, obj } => self.export = Some(ExportDialog { conn, obj, format: ExportFormat::Csv }),
            Action::AskDrop { conn, obj } => {
                let Some(cfg) = self.cfg(conn) else { return };
                let Ok(q) = Dialect(cfg.engine).qualified(&obj) else { return };
                let kw = if obj.kind == ObjectKind::View { "VIEW" } else { "TABLE" };
                self.confirm = Some(Confirm {
                    title: format!("Drop {}", obj.kind.label()),
                    message: format!(
                        "Permanently delete “{}” and all of its data? This cannot be undone.",
                        obj.display_name()
                    ),
                    sql: Some(format!("DROP {kw} {q}")),
                    confirm_label: "Drop".into(),
                    on_confirm: Confirmed::RunDestructive {
                        conn,
                        database: obj.database.clone(),
                        schema: obj.schema.clone(),
                        sql: format!("DROP {kw} {q}"),
                    },
                });
            }
            Action::AskTruncate { conn, obj } => {
                let Some(cfg) = self.cfg(conn) else { return };
                let Ok(q) = Dialect(cfg.engine).qualified(&obj) else { return };
                let sql = if cfg.engine == Engine::Sqlite {
                    format!("DELETE FROM {q}")
                } else {
                    format!("TRUNCATE TABLE {q}")
                };
                self.confirm = Some(Confirm {
                    title: "Empty Table".into(),
                    message: format!("Delete every row of “{}”? This cannot be undone.", obj.display_name()),
                    sql: Some(sql.clone()),
                    confirm_label: "Empty Table".into(),
                    on_confirm: Confirmed::RunDestructive {
                        conn,
                        database: obj.database.clone(),
                        schema: obj.schema.clone(),
                        sql,
                    },
                });
            }
            Action::CopyText(_) => {}
            Action::OpenHistory => {
                if let Some(i) = self.tabs.iter().position(|t| matches!(t.kind, TabKind::History(_))) {
                    self.active = i;
                } else {
                    self.push_tab(TabKind::History(HistoryTab::default()));
                }
                self.reload_history();
            }
            Action::OpenFind => {
                if let Some(i) = self.tabs.iter().position(|t| matches!(t.kind, TabKind::Find(_))) {
                    self.active = i;
                } else {
                    let conn = self.selection.as_ref().map(NavSel::conn);
                    let (db, schema) = self
                        .selection
                        .as_ref()
                        .and_then(NavSel::object_scope)
                        .map_or((None, None), |(d, s)| (Some(d), s));
                    self.push_tab(TabKind::Find(Box::new(FindTab::new(conn, db, schema))));
                }
            }
            Action::OpenMonitor(conn) => {
                if let Some(i) = self.tabs.iter().position(|t| matches!(&t.kind, TabKind::Monitor(m) if m.conn == conn))
                {
                    self.active = i;
                } else if let Some(name) = self.cfg(conn).map(|c| c.name.clone()) {
                    self.push_tab(TabKind::Monitor(Box::new(MonitorTab::new(conn, name))));
                }
            }
            Action::OpenDiagram { conn, db, schema } => {
                if let Some(i) = self.tabs.iter().position(|t| matches!(&t.kind, TabKind::Diagram(d) if d.conn == conn && d.database == db && d.schema == schema)) {
                    self.active = i;
                } else {
                    self.push_tab(TabKind::Diagram(Box::new(DiagramTab::new(conn, db, schema))));
                }
            }
            Action::OpenSync => {
                if let Some(i) = self.tabs.iter().position(|t| matches!(t.kind, TabKind::Sync(_))) {
                    self.active = i;
                } else {
                    let conn = self.selection.as_ref().map(NavSel::conn);
                    let (db, schema) = self
                        .selection
                        .as_ref()
                        .and_then(NavSel::object_scope)
                        .map_or((None, None), |(d, s)| (Some(d), s));
                    self.push_tab(TabKind::Sync(Box::new(SyncTab::new(conn, db, schema))));
                }
            }
            Action::OpenUsers(conn) => {
                if let Some(i) = self.tabs.iter().position(|t| matches!(&t.kind, TabKind::Users(u) if u.conn == conn)) {
                    self.active = i;
                } else if let Some((name, engine)) = self.cfg(conn).map(|c| (c.name.clone(), c.engine)) {
                    self.push_tab(TabKind::Users(Box::new(UsersTab::new(conn, name, engine))));
                }
            }
            Action::OpenModel => {
                self.push_tab(TabKind::Model(Box::default()));
            }
            Action::OpenBuilder => {
                let Some((conn, db, schema)) = self.selected_scope() else {
                    self.notify("Select a database or schema in the navigator first.", true);
                    return;
                };
                let Some(engine) = self.cfg(conn).map(|c| c.engine) else { return };
                let schema = engine.default_schema().map(|d| schema.unwrap_or_else(|| d.into()));
                self.push_tab(TabKind::Builder(Box::new(BuilderTab::new(conn, engine, db, schema))));
            }
            Action::Profile { conn, obj } => {
                if let Some(i) = self
                    .tabs
                    .iter()
                    .position(|t| matches!(&t.kind, TabKind::Profile(p) if p.conn == conn && p.obj == obj))
                {
                    self.active = i;
                } else {
                    self.push_tab(TabKind::Profile(Box::new(ProfileTab::new(conn, obj))));
                }
            }
            Action::GenerateData { conn, obj } => self.fill = Some(FillDialog { conn, obj, rows: "100".into() }),
            Action::Dictionary { html } => {
                let Some((conn, db, schema)) = self.selected_scope() else {
                    self.notify("Select a database or schema of a SQL connection in the navigator first.", true);
                    return;
                };
                let format = if html {
                    talus_core::dictionary::DictFormat::Html
                } else {
                    talus_core::dictionary::DictFormat::Markdown
                };
                let stem = schema.clone().unwrap_or_else(|| db.clone());
                let Some(path) = rfd::FileDialog::new()
                    .add_filter("Data dictionary", &[format.extension()])
                    .set_file_name(format!("{stem}-dictionary.{}", format.extension()))
                    .save_file()
                else {
                    return;
                };
                let Some(info) = self.conn_info(conn, Action::Dictionary { html }) else { return };
                self.submit(
                    &info,
                    Purpose::Meta,
                    Some(db.clone()),
                    Request::Dictionary { database: db, schema, format, path },
                    Pending::Dictionary,
                );
                self.notify("Writing the data dictionary…", false);
            }
            Action::OpenWorkspace => self.workspace.show(),
            Action::OpenJobs => {
                if let Some(i) = self.tabs.iter().position(|t| matches!(t.kind, TabKind::Jobs(_))) {
                    self.active = i;
                } else {
                    self.push_tab(TabKind::Jobs(Box::default()));
                }
            }
            Action::OpenRoutines => {
                let Some((conn, db, schema)) = self.selected_scope() else {
                    self.notify("Select a database or schema in the navigator first.", true);
                    return;
                };
                let Some(engine) = self.cfg(conn).map(|c| c.engine) else { return };
                let schema = engine.default_schema().map(|d| schema.unwrap_or_else(|| d.into()));
                if let Some(i) = self.tabs.iter().position(|t| {
                    matches!(&t.kind, TabKind::Routines(r) if r.conn == conn && r.database == db && r.schema == schema)
                }) {
                    self.active = i;
                } else {
                    self.push_tab(TabKind::Routines(Box::new(ProgTab::new(conn, engine, db, schema))));
                }
            }
            Action::DumpSql { include_data } => {
                let Some((conn, db, schema)) = self.selected_scope() else {
                    self.notify("Select a database or schema in the navigator first.", true);
                    return;
                };
                let stem = schema.clone().unwrap_or_else(|| db.clone());
                let Some(path) =
                    rfd::FileDialog::new().add_filter("SQL", &["sql"]).set_file_name(format!("{stem}.sql")).save_file()
                else {
                    return;
                };
                let Some(info) = self.conn_info(conn, Action::DumpSql { include_data }) else { return };
                self.submit(
                    &info,
                    Purpose::Meta,
                    Some(db.clone()),
                    Request::DumpSchema { database: db, schema, path, include_data },
                    Pending::Dump,
                );
                self.notify("Dump started…", false);
            }
            Action::RunSqlFile => {
                let Some((conn, db, _)) = self.selected_scope() else {
                    self.notify("Select the target database in the navigator first.", true);
                    return;
                };
                let Some(path) = rfd::FileDialog::new().add_filter("SQL", &["sql", "txt"]).pick_file() else {
                    return;
                };
                let name = self.cfg(conn).map(|c| c.name.clone()).unwrap_or_default();
                self.confirm = Some(Confirm {
                    title: "Execute SQL File".into(),
                    message: format!(
                        "Run every statement of {} on “{name}” / {db}? The statements can change or delete data.",
                        path.display()
                    ),
                    sql: None,
                    confirm_label: "Execute".into(),
                    on_confirm: Confirmed::RunSqlFile { conn, database: db, path },
                });
            }
            Action::NewDatabase(conn) => self.new_database = Some((conn, String::new())),
            Action::AskDropDatabase { conn, db } => {
                let Some(engine) = self.cfg(conn).map(|c| c.engine) else { return };
                match talus_core::sql::admin::drop_database(engine, &db) {
                    Ok(sql) => {
                        self.confirm = Some(Confirm {
                            title: "Drop Database".into(),
                            message: format!(
                                "Permanently delete the database “{db}” with all its objects and data? This cannot be undone."
                            ),
                            sql: Some(sql.clone()),
                            confirm_label: "Drop Database".into(),
                            on_confirm: Confirmed::DropDatabase { conn, db, sql },
                        });
                    }
                    Err(e) => self.notify(e.to_string(), true),
                }
            }
            Action::OpenSettings => {
                self.settings_draft = self.settings.clone();
                self.settings_open = true;
            }
            Action::OpenAbout => self.about_open = true,
            Action::OpenFile => self.open_file(),
            Action::ActivateTab(id) => {
                if let Some(i) = self.tabs.iter().position(|t| t.id == id) {
                    self.active = i;
                }
            }
            Action::CloseTab(id) => {
                if let Some(t) = self.tabs.iter().find(|t| t.id == id) {
                    if t.is_dirty() {
                        self.confirm = Some(Confirm {
                            title: "Unsaved Changes".into(),
                            message: format!(
                                "“{}” has unsaved changes or an open transaction (it will be rolled back). Close it anyway?",
                                t.title()
                            ),
                            sql: None,
                            confirm_label: "Close Without Saving".into(),
                            on_confirm: Confirmed::CloseTab(id),
                        });
                    } else {
                        self.close_tab(id);
                    }
                }
            }
        }
    }

    fn refresh(&mut self, sel: NavSel) {
        match sel {
            NavSel::Conn(c) => {
                if self.states.get(&c).is_some_and(|s| s.status.is_open()) {
                    self.handle(Action::Close(c));
                    self.handle(Action::Open(c));
                }
            }
            NavSel::Db(c, db) => {
                let engine = self.cfg(c).map(|x| x.engine);
                if let Some(d) = self.states.entry(c).or_default().db_mut(&db) {
                    d.objects = Loadable::NotLoaded;
                    d.schemas = Loadable::NotLoaded;
                    d.expanded = true;
                }
                self.ddl.retain(|(cc, o), _| !(*cc == c && o.database == db));
                if engine.is_some_and(|e| e.capabilities().schemas) {
                    self.handle(Action::LoadSchemas { conn: c, db });
                } else {
                    self.handle(Action::LoadObjects { conn: c, db, schema: None });
                }
            }
            NavSel::Schema(c, db, s) => {
                if let Some(slot) = self.states.entry(c).or_default().objects_mut(&db, Some(&s)) {
                    *slot = Loadable::NotLoaded;
                }
                self.handle(Action::LoadObjects { conn: c, db, schema: Some(s) });
            }
            NavSel::Obj(c, o) => {
                self.ddl.remove(&(c, o.clone()));
                if let Some(slot) = self.states.entry(c).or_default().objects_mut(&o.database, o.schema.as_deref()) {
                    *slot = Loadable::NotLoaded;
                }
                self.handle(Action::LoadObjects { conn: c, db: o.database.clone(), schema: o.schema.clone() });
            }
        }
    }

    fn close_tab(&mut self, id: u64) {
        let Some(i) = self.tabs.iter().position(|t| t.id == id) else { return };
        if matches!(self.tabs[i].kind, TabKind::Objects) {
            return;
        }
        if let TabKind::Sql(s) = &self.tabs[i].kind
            && let Some(job) = s.running
        {
            self.worker.cancel(job);
        }
        if let TabKind::Debug(d) = &self.tabs[i].kind {
            // Abort a routine paused in the debugger (its transaction rolls back).
            let (conn, database) = (d.conn, d.database.clone());
            if let Some(info) = self.conn_info(conn, Action::CloseTab(id)) {
                self.submit(
                    &info,
                    Purpose::Tab(id),
                    Some(database),
                    Request::Debug { tab: id, op: DebugOp::Stop },
                    Pending::Debug { tab: id },
                );
            }
        }
        self.worker.close_tab(id);
        self.tabs.remove(i);
        if self.active >= self.tabs.len() || self.active > i {
            self.active = self.active.saturating_sub(1);
        }
    }

    fn reload_history(&mut self) {
        let limit = self.settings.history_limit;
        let entries = self.history.recent(limit);
        let err = entries.as_ref().err().map(|e| e.to_string());
        if let Some(Tab { kind: TabKind::History(h), .. }) =
            self.tabs.iter_mut().find(|t| matches!(t.kind, TabKind::History(_)))
        {
            h.entries = entries.unwrap_or_default();
            h.loaded = true;
        }
        if let Some(e) = err {
            self.notify(format!("History could not be read: {e}"), true);
        }
    }

    fn open_file(&mut self) {
        let Some(path) = rfd::FileDialog::new().add_filter("SQL", &["sql", "txt"]).pick_file() else { return };
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let conn = self.selection.as_ref().map(NavSel::conn);
                let id = self.open_sql_tab(conn, None, text);
                if let Some(Tab { kind: TabKind::Sql(s), .. }) = self.tab_mut(id) {
                    s.file = Some(path);
                }
            }
            Err(e) => self.notify(format!("Could not open {}: {e}", path.display()), true),
        }
    }

    fn save_sql_tab(&mut self, tab: u64, save_as: bool) {
        let Some(Tab { kind: TabKind::Sql(s), .. }) = self.tabs.iter_mut().find(|t| t.id == tab) else { return };
        let path = match (&s.file, save_as) {
            (Some(p), false) => Some(p.clone()),
            _ => rfd::FileDialog::new()
                .add_filter("SQL", &["sql"])
                .set_file_name(format!("{}.sql", s.title().trim_end_matches(" •")))
                .save_file(),
        };
        let Some(path) = path else { return };
        match std::fs::write(&path, s.text.as_bytes()) {
            Ok(()) => {
                s.file = Some(path.clone());
                s.dirty = false;
                self.notify(format!("Saved {}", path.display()), false);
            }
            Err(e) => self.notify(format!("Could not save {}: {e}", path.display()), true),
        }
    }

    fn run_sql(&mut self, tab: u64, mode: RunMode) {
        let Some(Tab { kind: TabKind::Sql(s), .. }) = self.tabs.iter().find(|t| t.id == tab) else { return };
        if s.running.is_some() {
            return;
        }
        let Some(conn) = s.conn else {
            self.notify("Choose a connection for this query first.", true);
            return;
        };
        let Some(engine) = self.cfg(conn).map(|c| c.engine) else { return };
        let Some((script, line)) = s.script_for(mode, engine) else {
            self.notify("Nothing to run.", true);
            return;
        };
        self.submit_sql(tab, script, line, None);
    }

    /// Begin/Commit/Rollback of the tab's own session; `tx_open` flips only if the statement succeeds.
    fn run_tx(&mut self, tab: u64, begin: bool, sql: &str) {
        self.submit_sql(tab, sql.to_string(), 0, Some(begin));
    }

    fn submit_sql(&mut self, tab: u64, script: String, line: usize, tx: Option<bool>) {
        let Some(Tab { kind: TabKind::Sql(s), .. }) = self.tabs.iter().find(|t| t.id == tab) else { return };
        if s.running.is_some() {
            return;
        }
        let Some(conn) = s.conn else { return };
        let database = s.database.clone();
        let opts = ScriptOptions {
            limits: FetchLimits { max_rows: self.settings.max_result_rows },
            // SQL Server's SHOWPLAN mode must be switched off even when the explained statement fails.
            continue_on_error: s.continue_on_error || script.starts_with("SET SHOWPLAN_TEXT ON"),
        };
        let Some(info) = self.conn_info(conn, Action::ActivateTab(tab)) else { return };
        if matches!(
            self.states.get(&conn).map(|s| &s.status),
            None | Some(ConnStatus::Closed) | Some(ConnStatus::Failed(_))
        ) {
            // Open the navigator side as well so database pickers populate.
            self.handle(Action::Open(conn));
        }
        let job = self.submit(
            &info,
            Purpose::Tab(tab),
            database,
            Request::RunScript { script, opts },
            Pending::Script { tab },
        );
        if let Some(Tab { kind: TabKind::Sql(s), .. }) = self.tab_mut(tab) {
            s.begin_run(job, line);
            s.tx_next = tx;
        }
    }

    // ---------- dialogs ----------

    fn dialogs(&mut self, ctx: &egui::Context) {
        self.fill_dialog(ctx);
        if self.confirm.is_some() || self.prompt.is_some() {
            // A context menu that opened the modal would otherwise stay on top and block it.
            egui::Popup::close_all(ctx);
        }
        if let Some(d) = self.conn_dialog.as_mut() {
            match d.ui(ctx) {
                Some(DialogAction::Cancel) => self.conn_dialog = None,
                Some(DialogAction::Test(info)) => {
                    let job = self.worker.submit(&info, Purpose::Meta, None, Request::Test);
                    self.pending.insert(job, Pending::Test);
                    if let Some(d) = self.conn_dialog.as_mut() {
                        d.test = TestState::Running(job);
                    }
                }
                Some(DialogAction::Save(info)) => self.save_connection(info),
                None => {}
            }
        }
        if let Some(c) = &self.confirm {
            match c.ui(ctx) {
                Some(ConfirmResult::Yes(what)) => {
                    self.confirm = None;
                    self.confirmed(what);
                }
                Some(ConfirmResult::No) => self.confirm = None,
                None => {}
            }
        }
        if let Some((p, _)) = self.prompt.as_mut() {
            match p.ui(ctx) {
                Some(PromptResult::Ok { password, save }) => {
                    if let Some((p, then)) = self.prompt.take() {
                        let secret = Secret::new(password);
                        if save {
                            match self.secrets.set(&p.conn, p.kind, &secret) {
                                Ok(()) => {
                                    if let Some(c) = self.connections.iter_mut().find(|c| c.id == p.conn) {
                                        match p.kind {
                                            SecretKind::DbPassword => c.save_password = true,
                                            SecretKind::TunnelSecret => c.set_tunnel_secret_saved(true),
                                        }
                                    }
                                    self.save_connections();
                                }
                                Err(e) => self.notify(e, true),
                            }
                        }
                        self.cache(p.kind).insert(p.conn, secret);
                        self.dispatch(then);
                    }
                }
                Some(PromptResult::Cancel) => self.prompt = None,
                None => {}
            }
        }
        if self.settings_open {
            let mut open = self.settings_open;
            if let Some(s) = dialogs::settings_ui(ctx, &mut self.settings_draft, &mut open) {
                self.settings = s;
                if let Err(e) = save_json(&self.paths.settings_file(), &self.settings) {
                    self.notify(format!("Could not save settings: {e}"), true);
                }
            }
            self.settings_open = open;
        }
        if self.about_open {
            dialogs::about_ui(ctx, &mut self.about_open);
        }
        if self.workspace.ui(ctx, &mut self.connections) {
            self.save_connections();
        }
        if let Some((conn, name)) = self.new_database.as_mut() {
            let (conn, mut go, mut open) = (*conn, false, true);
            egui::Window::new("New Database").open(&mut open).collapsible(false).resizable(false).show(ctx, |ui| {
                let l = ui.label("Database Name");
                let r = ui.add(egui::TextEdit::singleline(name).desired_width(260.0)).labelled_by(l.id);
                go = (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                    || ui.add_enabled(!name.trim().is_empty(), egui::Button::new("Create")).clicked();
            });
            if go {
                let name = name.clone();
                self.new_database = None;
                let engine = self.cfg(conn).map(|c| c.engine);
                match engine.map(|e| talus_core::sql::admin::create_database(e, &name)) {
                    Some(Ok(sql)) => {
                        if let Some(info) = self.conn_info(conn, Action::NewDatabase(conn)) {
                            self.submit(
                                &info,
                                Purpose::Meta,
                                None,
                                Request::ExecuteBatch { statements: vec![sql], transactional: false },
                                Pending::DatabaseChanged { conn, done: "Database created." },
                            );
                        }
                    }
                    Some(Err(e)) => self.notify(e.to_string(), true),
                    None => {}
                }
            } else if !open {
                self.new_database = None;
            }
        }
        if let Some(d) = self.import.as_mut() {
            match d.ui(ctx) {
                Some(ImportAction::Close) => self.import = None,
                Some(action @ (ImportAction::Start(_) | ImportAction::StartJson(_) | ImportAction::StartXml(_))) => {
                    let (conn, obj) = (d.conn, d.obj.clone());
                    d.error = None;
                    if let Some(info) = self.conn_info(conn, Action::Import { conn, obj: obj.clone() }) {
                        let job = self.submit(
                            &info,
                            Purpose::Meta,
                            Some(obj.database.clone()),
                            match action {
                                ImportAction::Start(spec) => Request::ImportCsv { obj: obj.clone(), spec },
                                ImportAction::StartJson(path) => Request::ImportJson { obj: obj.clone(), path },
                                ImportAction::StartXml(path) => Request::ImportXml { obj: obj.clone(), path },
                                ImportAction::Close => return,
                            },
                            Pending::Import { conn, obj },
                        );
                        if let Some(d) = self.import.as_mut() {
                            d.running = Some(job);
                        }
                    }
                }
                None => {}
            }
        }
        if let Some(ex) = self.export.as_mut() {
            let mut open = true;
            let mut go = false;
            egui::Window::new("Export Data").open(&mut open).collapsible(false).resizable(false).show(ctx, |ui| {
                ui.label(format!("Export all rows of “{}” to a file.", ex.obj.display_name()));
                ui.horizontal(|ui| {
                    for f in ExportFormat::ALL {
                        ui.selectable_value(&mut ex.format, f, f.label());
                    }
                });
                ui.label(
                    RichText::new("Rows are streamed in chunks; large tables do not need to fit in memory.").small(),
                );
                if ui.button("Choose File and Export…").clicked() {
                    go = true;
                }
            });
            if go {
                let (conn, obj, format) = (ex.conn, ex.obj.clone(), ex.format);
                self.export = None;
                let file =
                    rfd::FileDialog::new().set_file_name(format!("{}.{}", obj.name, format.extension())).save_file();
                if let (Some(path), Some(info)) =
                    (file, self.conn_info(conn, Action::Export { conn, obj: obj.clone() }))
                {
                    self.submit(
                        &info,
                        Purpose::Meta,
                        Some(obj.database.clone()),
                        Request::Export { obj, format, path },
                        Pending::Export,
                    );
                    self.notify("Export started…", false);
                }
            } else if !open {
                self.export = None;
            }
        }
    }

    /// Connection, database and schema the navigator selection points into.
    fn selected_scope(&self) -> Option<(Uuid, String, Option<String>)> {
        let sel = self.selection.as_ref()?;
        // Dumps, builder, routines, … only exist for SQL connections.
        if !self.cfg(sel.conn())?.engine.is_sql() {
            return None;
        }
        let (db, schema) = sel.object_scope()?;
        Some((sel.conn(), db, schema))
    }

    fn confirmed(&mut self, what: Confirmed) {
        match what {
            Confirmed::RunNoSql { tab, op, tag } => self.submit_nosql(tab, op, tag),
            Confirmed::DeleteJob(id) => {
                self.jobs.retain(|j| j.id != id);
                self.save_jobs();
            }
            Confirmed::InstallLaunchd(id) => {
                #[cfg(target_os = "macos")]
                if let Some(job) = self.jobs.iter().find(|j| j.id == id).cloned() {
                    match crate::launchd::install(&job) {
                        Ok(path) => self.notify(format!("Scheduled with launchd: {}", path.display()), false),
                        Err(e) => self.notify(format!("Could not install the launchd agent: {e}"), true),
                    }
                }
                #[cfg(not(target_os = "macos"))]
                let _ = id;
            }
            Confirmed::RemoveLaunchd(id) => {
                #[cfg(target_os = "macos")]
                if let Some(job) = self.jobs.iter().find(|j| j.id == id).cloned() {
                    match crate::launchd::remove(&job) {
                        Ok(()) => self.notify("The launchd agent was removed.", false),
                        Err(e) => self.notify(format!("Could not remove the launchd agent: {e}"), true),
                    }
                }
                #[cfg(not(target_os = "macos"))]
                let _ = id;
            }
            Confirmed::RunUserSql { tab, sql } => {
                if let Some(job) = self.users_job(
                    tab,
                    Request::ExecuteBatch { statements: vec![sql], transactional: false },
                    Pending::UserAdmin { tab },
                ) && let Some(Tab { kind: TabKind::Users(u), .. }) = self.tab_mut(tab)
                {
                    let _ = job;
                    u.error = None;
                }
            }
            Confirmed::DropProgrammable { tab, sql } => {
                let Some(Tab { kind: TabKind::Routines(r), .. }) = self.tabs.iter().find(|t| t.id == tab) else {
                    return;
                };
                let (conn, database) = (r.conn, r.database.clone());
                let Some(info) = self.conn_info(conn, Action::ActivateTab(tab)) else { return };
                self.submit(
                    &info,
                    Purpose::Tab(tab),
                    Some(database),
                    Request::ExecuteBatch { statements: vec![sql], transactional: false },
                    Pending::ProgrammableDrop { tab },
                );
            }
            Confirmed::RunSqlFile { conn, database, path } => {
                let Some(cfg) = self.cfg(conn).cloned() else { return };
                let Some(info) = self.conn_info(conn, Action::RunSqlFile) else { return };
                // A fixed pseudo-tab keeps its own session on the chosen database (also for MySQL).
                self.submit(
                    &info,
                    Purpose::Tab(u64::MAX),
                    Some(database),
                    Request::ExecuteFile { path, transactional: cfg.engine.capabilities().transactional_ddl },
                    Pending::RunFile { conn },
                );
                self.notify("Executing SQL file…", false);
            }
            Confirmed::DeleteConnection(id) => {
                self.worker.disconnect(id);
                self.connections.retain(|c| c.id != id);
                self.states.remove(&id);
                self.passwords.remove(&id);
                self.tunnel_secrets.remove(&id);
                for kind in [SecretKind::DbPassword, SecretKind::TunnelSecret] {
                    if let Err(e) = self.secrets.delete(&id, kind) {
                        self.notify(e, true);
                    }
                }
                if self.selection.as_ref().is_some_and(|s| s.conn() == id) {
                    self.selection = None;
                }
                for t in &mut self.tabs {
                    if let TabKind::Sql(s) = &mut t.kind
                        && s.conn == Some(id)
                    {
                        s.conn = None;
                        s.database = None;
                    }
                }
                self.tabs.retain(|t| {
                    !matches!(&t.kind, TabKind::Data(d) if d.conn == id)
                        && !matches!(&t.kind, TabKind::Structure(s) if s.conn == id)
                });
                self.active = self.active.min(self.tabs.len() - 1);
                self.save_connections();
            }
            Confirmed::CloseTab(id) => self.close_tab(id),
            Confirmed::RunDesign { tab, statements } => self.run_design(tab, statements),
            Confirmed::RunSync { tab, statements } => self.run_sync(tab, statements),
            Confirmed::RunDataSync { tab, tables } => self.start_data_sync(tab, tables, true),
            Confirmed::DropDatabase { conn, db, sql } => {
                // Sessions connected to that database would block the drop.
                self.worker.close_database(conn, db.clone());
                self.tabs.retain(|t| !matches!(&t.kind, TabKind::Data(d) if d.conn == conn && d.obj.database == db));
                self.active = self.active.min(self.tabs.len() - 1);
                if let Some(info) = self.conn_info(conn, Action::Refresh(NavSel::Conn(conn))) {
                    self.submit(
                        &info,
                        Purpose::Meta,
                        None,
                        Request::ExecuteBatch { statements: vec![sql], transactional: false },
                        Pending::DatabaseChanged { conn, done: "Database dropped." },
                    );
                }
                if self.selection.as_ref().is_some_and(|s| s.conn() == conn && s.database() == Some(db.as_str())) {
                    self.selection = Some(NavSel::Conn(conn));
                }
            }
            Confirmed::Kill { tab, sql } => {
                let Some(Tab { kind: TabKind::Monitor(m), .. }) = self.tabs.iter().find(|t| t.id == tab) else {
                    return;
                };
                let conn = m.conn;
                if let Some(info) = self.conn_info(conn, Action::ActivateTab(tab)) {
                    let opts = ScriptOptions { limits: FetchLimits { max_rows: 10 }, continue_on_error: false };
                    self.submit(
                        &info,
                        Purpose::Meta,
                        None,
                        Request::RunScript { script: sql, opts },
                        Pending::Kill { tab },
                    );
                }
            }
            Confirmed::RunDestructive { conn, database, schema, sql } => {
                let Some(info) = self.conn_info(conn, Action::Refresh(NavSel::Conn(conn))) else { return };
                let opts = ScriptOptions { limits: FetchLimits { max_rows: 0 }, continue_on_error: false };
                self.submit(
                    &info,
                    Purpose::Meta,
                    Some(database.clone()),
                    Request::RunScript { script: sql, opts },
                    Pending::Destructive { conn, db: database, schema },
                );
            }
        }
    }

    fn save_connection(&mut self, info: ConnInfo) {
        let ConnInfo { cfg, password, tunnel_secret } = info;
        let id = cfg.id;
        let tunnel_save = cfg.tunnel_secret_policy().is_some_and(|(saved, _)| saved);
        let r = match (&tunnel_secret, tunnel_save) {
            (Some(p), true) => self.secrets.set(&id, SecretKind::TunnelSecret, p),
            _ => self.secrets.delete(&id, SecretKind::TunnelSecret),
        };
        if let Err(e) = r {
            if let Some(d) = self.conn_dialog.as_mut() {
                d.error = Some(e);
            }
            return;
        }
        match tunnel_secret {
            Some(p) if tunnel_save || !p.is_empty() => {
                self.tunnel_secrets.insert(id, p);
            }
            _ => {
                self.tunnel_secrets.remove(&id);
            }
        }
        if cfg.needs_password() {
            let r = match (&password, cfg.save_password) {
                (Some(p), true) => self.secrets.set(&id, SecretKind::DbPassword, p),
                _ => self.secrets.delete(&id, SecretKind::DbPassword),
            };
            if let Err(e) = r {
                if let Some(d) = self.conn_dialog.as_mut() {
                    d.error = Some(e);
                }
                return;
            }
        }
        match password {
            Some(p) if cfg.save_password || !p.is_empty() => {
                self.passwords.insert(id, p);
            }
            _ => {
                self.passwords.remove(&id);
            }
        }
        let was_open = self.states.get(&id).is_some_and(|s| s.status.is_open());
        match self.connections.iter_mut().find(|c| c.id == id) {
            Some(existing) => *existing = cfg,
            None => self.connections.push(cfg),
        }
        self.save_connections();
        self.conn_dialog = None;
        if was_open {
            // Sessions were opened with the old settings.
            self.handle(Action::Close(id));
            self.handle(Action::Open(id));
        }
        self.selection = Some(NavSel::Conn(id));
    }

    // ---------- shortcuts ----------

    fn shortcuts(&mut self, ctx: &egui::Context) {
        use egui::{Key, KeyboardShortcut, Modifiers};
        let cmd = Modifiers::COMMAND;
        let cmd_shift = Modifiers::COMMAND | Modifiers::SHIFT;
        let hit = |m: Modifiers, k: Key| ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(m, k)));
        if hit(cmd, Key::Y) {
            self.actions.push(Action::NewQuery { conn: None, database: None, text: String::new() });
        }
        if hit(cmd, Key::N) {
            self.actions.push(Action::NewConnection);
        }
        if hit(cmd, Key::O) {
            self.actions.push(Action::OpenFile);
        }
        if hit(cmd, Key::Comma) {
            self.actions.push(Action::OpenSettings);
        }
        if hit(cmd_shift, Key::H) {
            self.actions.push(Action::OpenHistory);
        }
        if hit(cmd_shift, Key::F) {
            self.actions.push(Action::OpenFind);
        }
        if hit(cmd, Key::K) {
            self.focus_search = true;
        }
        if hit(cmd, Key::Num1) {
            self.show_nav = !self.show_nav;
        }
        if hit(cmd, Key::Num2) {
            self.show_info = !self.show_info;
        }
        if hit(cmd_shift, Key::CloseBracket) || hit(Modifiers::CTRL, Key::Tab) {
            self.active = (self.active + 1) % self.tabs.len();
        }
        if hit(cmd_shift, Key::OpenBracket) || hit(Modifiers::CTRL | Modifiers::SHIFT, Key::Tab) {
            self.active = (self.active + self.tabs.len() - 1) % self.tabs.len();
        }
        let active_id = self.tabs[self.active].id;
        if hit(cmd, Key::W) {
            self.actions.push(Action::CloseTab(active_id));
        }
        match &mut self.tabs[self.active].kind {
            TabKind::Sql(s) => {
                if hit(cmd_shift, Key::R) {
                    self.run_sql(active_id, RunMode::Current);
                } else if hit(cmd, Key::R) {
                    self.run_sql(active_id, RunMode::All);
                } else if hit(cmd, Key::Period) {
                    if let Some(j) = s.running {
                        self.worker.cancel(j);
                    }
                } else if hit(Modifiers::COMMAND | Modifiers::ALT, Key::E) {
                    self.run_sql(active_id, RunMode::Explain);
                } else if hit(cmd, Key::Slash) {
                    s.toggle_comment(ctx, active_id);
                } else if hit(Modifiers::COMMAND | Modifiers::ALT, Key::L) {
                    let engine = s
                        .conn
                        .and_then(|c| self.connections.iter().find(|x| x.id == c))
                        .map_or(Engine::Sqlite, |c| c.engine);
                    s.format(ctx, active_id, engine);
                } else if hit(cmd_shift, Key::S) {
                    self.save_sql_tab(active_id, true);
                } else if hit(cmd, Key::S) {
                    self.save_sql_tab(active_id, false);
                }
            }
            TabKind::Data(d) => {
                if hit(cmd, Key::Enter) {
                    match d.change_set() {
                        Ok(cs) if !cs.is_empty() => self.data_action(active_id, DataAction::Apply(cs)),
                        Ok(_) => {}
                        Err(e) => d.error = Some(e),
                    }
                } else if hit(cmd, Key::Escape) {
                    d.discard();
                } else if hit(cmd, Key::R) {
                    if d.is_dirty() {
                        d.error = Some("Apply or discard pending changes before refreshing.".into());
                    } else {
                        self.load_page(active_id);
                    }
                } else if hit(cmd, Key::Plus) || hit(cmd, Key::Equals) {
                    d.add_row();
                } else if hit(cmd, Key::Minus) {
                    d.delete_selected();
                } else if hit(cmd, Key::Period)
                    && let Some(j) = d.loading
                {
                    self.worker.cancel(j);
                }
            }
            TabKind::Design(d) => {
                if hit(cmd, Key::S)
                    && d.saving.is_none()
                    && let Ok(plan) = d.plan()
                    && !plan.statements.is_empty()
                {
                    self.design_action(active_id, DesignAction::Save(plan));
                }
            }
            TabKind::Objects => {
                if hit(cmd, Key::R)
                    && let Some(sel) = self.selection.clone()
                {
                    self.actions.push(Action::Refresh(sel));
                }
            }
            _ => {}
        }
    }

    /// Completion metadata for a query tab's target database, from the navigator cache.
    // ponytail: rebuilt every frame for the active tab; cache by (conn, db, version) if schemas get huge.
    fn meta_view(
        connections: &[ConnectionConfig],
        states: &HashMap<Uuid, ConnState>,
        conn: Option<Uuid>,
        database: Option<&str>,
    ) -> crate::ui::completion::MetaView {
        let mut meta = crate::ui::completion::MetaView::default();
        let Some(cfg) = conn.and_then(|c| connections.iter().find(|x| x.id == c)) else { return meta };
        let Some(st) = states.get(&cfg.id) else { return meta };
        let db = Self::target_db(cfg, database);
        let schema = cfg.default_schema();
        if let Some(d) = st.databases.iter().find(|d| d.name == db)
            && let Some(list) = d.schemas.loaded()
        {
            meta.schemas = list.iter().map(|s| s.name.clone()).collect();
        }
        if let Some(objs) = st.objects(&db, schema.as_deref()).and_then(|o| o.loaded()) {
            for n in objs {
                meta.objects.push((n.obj.name.clone(), n.obj.kind));
                if let Some(s) = n.structure.loaded() {
                    meta.columns.insert(
                        n.obj.name.to_lowercase(),
                        s.columns.iter().map(|c| (c.name.clone(), c.data_type.clone())).collect(),
                    );
                }
            }
        }
        meta
    }

    /// The database a query tab effectively targets.
    fn target_db(cfg: &ConnectionConfig, database: Option<&str>) -> String {
        match (cfg.engine, database) {
            (Engine::Sqlite, _) => "main".into(),
            (_, Some(d)) => d.to_string(),
            (Engine::Postgres | Engine::SqlServer, None) if cfg.database.is_empty() => {
                cfg.engine.default_database().unwrap_or_default().to_string()
            }
            (_, None) => cfg.database.clone(),
        }
    }

    fn load_columns_for(&mut self, tab: u64, tables: Vec<String>) {
        let Some(Tab { kind: TabKind::Sql(s), .. }) = self.tabs.iter().find(|t| t.id == tab) else { return };
        let Some(conn) = s.conn else { return };
        let Some(cfg) = self.cfg(conn).cloned() else { return };
        let db = Self::target_db(&cfg, s.database.as_deref());
        let schema = cfg.default_schema();
        let objs: Vec<ObjectRef> = self
            .states
            .get(&conn)
            .and_then(|st| st.objects(&db, schema.as_deref()))
            .and_then(|o| o.loaded())
            .map(|list| {
                list.iter()
                    .filter(|n| n.structure.needs_load() && tables.iter().any(|t| t.eq_ignore_ascii_case(&n.obj.name)))
                    .map(|n| n.obj.clone())
                    .collect()
            })
            .unwrap_or_default();
        for obj in objs {
            self.actions.push(Action::LoadStructure { conn, obj });
        }
    }

    fn compare_sync(&mut self, tab: u64) {
        let Some(Tab { kind: TabKind::Sync(t), .. }) = self.tabs.iter().find(|t| t.id == tab) else { return };
        let sides = [
            (false, t.source.conn, t.source.database.clone(), t.source.schema.clone()),
            (true, t.target.conn, t.target.database.clone(), t.target.schema.clone()),
        ];
        for (target, conn, db, schema) in sides {
            let (Some(conn), Some(db)) = (conn, db) else { return };
            let default_schema = self.cfg(conn).and_then(|c| c.engine.default_schema());
            let schema = default_schema.map(|d| schema.unwrap_or_else(|| d.into()));
            let Some(info) = self.conn_info(conn, Action::ActivateTab(tab)) else { return };
            let job = self.submit(
                &info,
                Purpose::Meta,
                Some(db.clone()),
                Request::SchemaModel { database: db.clone(), schema: schema.clone() },
                Pending::SyncModel { tab, target },
            );
            self.submit(
                &info,
                Purpose::Meta,
                Some(db.clone()),
                Request::SchemaViews { database: db, schema },
                Pending::SyncViews { tab, target },
            );
            if let Some(Tab { kind: TabKind::Sync(t), .. }) = self.tab_mut(tab) {
                let side = if target { &mut t.target } else { &mut t.source };
                side.model = Loadable::Loading(job);
                side.views = Loadable::NotLoaded;
                t.plan = None;
                t.info = None;
            }
        }
    }

    fn start_data_sync(&mut self, tab: u64, tables: Vec<String>, apply: bool) {
        if let Some(Tab { kind: TabKind::Sync(t), .. }) = self.tab_mut(tab) {
            t.data_queue = tables.into_iter().rev().collect();
            t.data_results.clear();
            t.data_apply = apply;
            t.info = None;
            t.error = None;
        }
        self.next_data_sync(tab);
    }

    /// Submits the next queued table; the result handler calls this again until the queue is empty.
    fn next_data_sync(&mut self, tab: u64) {
        let Some(Tab { kind: TabKind::Sync(t), .. }) = self.tabs.iter().find(|t| t.id == tab) else { return };
        let Some(name) = t.data_queue.last().cloned() else {
            if let Some(Tab { kind: TabKind::Sync(t), .. }) = self.tab_mut(tab) {
                t.info = Some(if t.data_apply { "Data sync finished." } else { "Data comparison finished." }.into());
            }
            return;
        };
        let (Some(sc), Some(sdb), Some(tc), Some(tdb)) =
            (t.source.conn, t.source.database.clone(), t.target.conn, t.target.database.clone())
        else {
            return;
        };
        let Some(dt) = t.data_tables.iter().find(|d| d.name == name) else { return };
        let (src, dst, delete_missing, apply) = (dt.src.clone(), dt.dst.clone(), t.delete_missing, t.data_apply);
        let Some(src_info) = self.conn_info(sc, Action::ActivateTab(tab)) else { return };
        let Some(dst_info) = self.conn_info(tc, Action::ActivateTab(tab)) else { return };
        let job = self.submit(
            &src_info,
            Purpose::Meta,
            Some(sdb),
            Request::SyncData {
                target: Box::new(dst_info),
                target_database: Some(tdb),
                src,
                dst,
                delete_missing,
                apply,
            },
            Pending::SyncData { tab, table: name },
        );
        if let Some(Tab { kind: TabKind::Sync(t), .. }) = self.tab_mut(tab) {
            t.running = Some(job);
        }
    }

    fn run_sync(&mut self, tab: u64, statements: Vec<String>) {
        let Some(Tab { kind: TabKind::Sync(t), .. }) = self.tabs.iter().find(|t| t.id == tab) else { return };
        let (Some(conn), Some(db)) = (t.target.conn, t.target.database.clone()) else { return };
        let Some(cfg) = self.cfg(conn).cloned() else { return };
        let Some(info) = self.conn_info(conn, Action::ActivateTab(tab)) else { return };
        let transactional = cfg.engine.capabilities().transactional_ddl;
        let job = self.submit(
            &info,
            Purpose::Meta,
            Some(db),
            Request::ExecuteBatch { statements, transactional },
            Pending::SyncApply { tab, target_conn: conn },
        );
        if let Some(Tab { kind: TabKind::Sync(t), .. }) = self.tab_mut(tab) {
            t.running = Some(job);
            t.error = None;
        }
    }

    fn users_job(&mut self, tab: u64, req: Request, pending: Pending) -> Option<JobId> {
        let Some(Tab { kind: TabKind::Users(u), .. }) = self.tabs.iter().find(|t| t.id == tab) else { return None };
        let conn = u.conn;
        let info = self.conn_info(conn, Action::ActivateTab(tab))?;
        Some(self.submit(&info, Purpose::Tab(tab), None, req, pending))
    }

    fn load_users(&mut self, tab: u64) {
        if let Some(job) = self.users_job(tab, Request::ListUsers, Pending::Users { tab })
            && let Some(Tab { kind: TabKind::Users(u), .. }) = self.tab_mut(tab)
        {
            u.users = Loadable::Loading(job);
        }
    }

    // ---------- automation ----------

    fn save_jobs(&mut self) {
        let file = talus_core::jobs::JobsFile { jobs: self.jobs.clone() };
        if let Err(e) = save_json(&self.paths.jobs_file(), &file) {
            self.notify(format!("Could not save jobs: {e}"), true);
        }
    }

    /// Connection settings and secrets for an unattended run: never prompts.
    fn conn_info_quiet(&mut self, id: Uuid) -> Result<ConnInfo, String> {
        let cfg = self.cfg(id).cloned().ok_or("the connection of this job no longer exists")?;
        if !cfg.engine.is_sql() {
            return Err(format!("jobs are not available for {} connections", cfg.engine.label()));
        }
        let mut secret = |kind: SecretKind, needed: bool, saved: bool| -> Result<Option<Secret>, String> {
            if !needed {
                return Ok(None);
            }
            if let Some(p) = self.cache(kind).get(&cfg.id) {
                return Ok(Some(p.clone()));
            }
            if saved && let Some(p) = self.secrets.get(&cfg.id, kind)? {
                return Ok(Some(p));
            }
            Err("the password is not saved in the Keychain and not entered in this session".into())
        };
        let password = secret(SecretKind::DbPassword, cfg.needs_password(), cfg.save_password)?;
        let tunnel_secret = match cfg.tunnel_secret_policy() {
            Some((saved, true)) => secret(SecretKind::TunnelSecret, true, saved)?,
            Some((_, false)) => Some(Secret::new("")),
            None => None,
        };
        Ok(ConnInfo { cfg, password, tunnel_secret })
    }

    fn load_profile(&mut self, tab: u64) {
        let Some(Tab { kind: TabKind::Profile(p), .. }) = self.tabs.iter().find(|t| t.id == tab) else { return };
        let (conn, obj) = (p.conn, p.obj.clone());
        let Some(info) = self.conn_info(conn, Action::ActivateTab(tab)) else { return };
        let job = self.submit(
            &info,
            Purpose::Tab(tab),
            Some(obj.database.clone()),
            Request::Profile(obj),
            Pending::Profile { tab },
        );
        if let Some(Tab { kind: TabKind::Profile(p), .. }) = self.tab_mut(tab) {
            p.columns = Loadable::Loading(job);
        }
    }

    fn fill_dialog(&mut self, ctx: &egui::Context) {
        let Some(d) = self.fill.as_mut() else { return };
        let mut go = false;
        let mut open = true;
        egui::Window::new("Generate Test Data").id(egui::Id::new("fill-dialog")).open(&mut open).collapsible(false).resizable(false).show(ctx, |ui| {
            ui.label(format!("Insert generated rows into “{}”.", d.obj.display_name()));
            ui.label(
                egui::RichText::new("Values follow column types and names; foreign keys use existing parent rows. One transaction: any error inserts nothing.")
                    .small(),
            );
            ui.horizontal(|ui| {
                let l = ui.label("Rows");
                ui.add(egui::TextEdit::singleline(&mut d.rows).desired_width(100.0)).labelled_by(l.id);
            });
            let n = d.rows.trim().parse::<usize>().ok().filter(|n| (1..=talus_core::datagen::MAX_ROWS).contains(n));
            ui.horizontal(|ui| {
                if ui.add_enabled(n.is_some(), egui::Button::new("Generate")).clicked() {
                    go = true;
                }
            });
        });
        let n = d.rows.trim().parse::<usize>().ok();
        let (conn, obj) = (d.conn, d.obj.clone());
        if go {
            self.fill = None;
            if let (Some(rows), Some(info)) = (n, self.conn_info(conn, Action::GenerateData { conn, obj: obj.clone() }))
            {
                let seed = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(1, |t| t.as_nanos() as u64);
                self.submit(
                    &info,
                    Purpose::Meta,
                    Some(obj.database.clone()),
                    Request::FillTable { obj: obj.clone(), rows, seed },
                    Pending::Fill { obj },
                );
                self.notify("Generating test data…", false);
            }
        } else if !open {
            self.fill = None;
        }
    }

    fn job_action(&mut self, a: JobsAction) {
        match a {
            JobsAction::Save(job) => {
                let job = *job;
                match self.jobs.iter_mut().find(|j| j.id == job.id) {
                    Some(slot) => *slot = job,
                    None => self.jobs.push(job),
                }
                self.save_jobs();
            }
            JobsAction::SetEnabled(id, on) => {
                if let Some(j) = self.jobs.iter_mut().find(|j| j.id == id) {
                    j.enabled = on;
                }
                self.save_jobs();
            }
            JobsAction::RunNow(id) => self.start_job(id),
            JobsAction::Delete(id) => {
                let name = self.jobs.iter().find(|j| j.id == id).map(|j| j.name.clone()).unwrap_or_default();
                self.confirm = Some(Confirm {
                    title: "Delete Job".into(),
                    message: format!("Delete the job “{name}”? A launchd agent installed for it is not removed."),
                    sql: None,
                    confirm_label: "Delete Job".into(),
                    on_confirm: Confirmed::DeleteJob(id),
                });
            }
            JobsAction::InstallLaunchd(id) => {
                let Some(job) = self.jobs.iter().find(|j| j.id == id) else { return };
                #[cfg(target_os = "macos")]
                let target = crate::launchd::plist_path(job).map(|p| p.display().to_string()).unwrap_or_default();
                #[cfg(not(target_os = "macos"))]
                let target = String::new();
                self.confirm = Some(Confirm {
                    title: "Install launchd Agent".into(),
                    message: format!(
                        "Write {target} and load it, so “{}” runs on schedule ({}) even when Talus is closed? Remove it again from the same button.",
                        job.name,
                        job.schedule.label()
                    ),
                    sql: None,
                    confirm_label: "Install".into(),
                    on_confirm: Confirmed::InstallLaunchd(id),
                });
            }
            JobsAction::RemoveLaunchd(id) => {
                self.confirm = Some(Confirm {
                    title: "Remove launchd Agent".into(),
                    message: "Unload and delete the launchd agent of this job? The job itself stays.".into(),
                    sql: None,
                    confirm_label: "Remove".into(),
                    on_confirm: Confirmed::RemoveLaunchd(id),
                });
            }
        }
    }

    fn start_job(&mut self, id: Uuid) {
        let Some(job) = self.jobs.iter().find(|j| j.id == id).cloned() else { return };
        if self.jobs_running.contains(&id) {
            return;
        }
        let at = chrono::Local::now().naive_local().format(talus_core::jobs::TIME_FORMAT).to_string();
        let info = match self.conn_info_quiet(job.conn) {
            Ok(i) => i,
            Err(e) => {
                self.finish_job(id, talus_core::jobs::JobRun { at, ok: false, message: e, duration_ms: 0 });
                return;
            }
        };
        let database = match &job.action {
            talus_core::jobs::JobAction::RunSql { database, .. } => database.clone(),
            talus_core::jobs::JobAction::ExportTable { database, .. }
            | talus_core::jobs::JobAction::DumpDatabase { database, .. } => Some(database.clone()),
        };
        self.jobs_running.insert(id);
        self.submit(
            &info,
            Purpose::Meta,
            database,
            Request::RunJob(job.action),
            Pending::Job { id, at, started: Instant::now() },
        );
    }

    fn finish_job(&mut self, id: Uuid, run: talus_core::jobs::JobRun) {
        self.jobs_running.remove(&id);
        crate::headless::record_run(&self.paths, id, run.clone());
        if let Ok(f) = load_json::<talus_core::jobs::JobsFile>(&self.paths.jobs_file()) {
            self.jobs = f.jobs;
        }
        let name = self.jobs.iter().find(|j| j.id == id).map(|j| j.name.clone()).unwrap_or_default();
        if run.ok {
            self.notify(format!("Job “{name}” finished: {}", run.message), false);
        } else {
            self.notify(format!("Job “{name}” failed: {}", run.message), true);
        }
    }

    /// Starts every job whose time has come; checked about once a second.
    fn tick_jobs(&mut self, ctx: &egui::Context) {
        if !self.jobs.iter().any(|j| j.enabled && j.schedule != talus_core::jobs::Schedule::Manual) {
            return;
        }
        ctx.request_repaint_after(Duration::from_secs(20));
        if self.jobs_checked.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.jobs_checked = Instant::now();
        let now = chrono::Local::now().naive_local();
        let due: Vec<Uuid> =
            self.jobs.iter().filter(|j| j.is_due(now) && !self.jobs_running.contains(&j.id)).map(|j| j.id).collect();
        for id in due {
            self.start_job(id);
        }
    }

    /// Redis → key browser of that database; MongoDB → document browser of that collection.
    fn open_nosql_object(&mut self, conn: Uuid, obj: ObjectRef) {
        let Some((name, engine)) = self.cfg(conn).map(|c| (c.name.clone(), c.engine)) else { return };
        let existing = self.tabs.iter().position(|t| match &t.kind {
            TabKind::Keys(k) => engine == Engine::Redis && k.conn == conn && k.database == obj.database,
            TabKind::Docs(d) => d.conn == conn && d.database == obj.database && d.collection == obj.name,
            _ => false,
        });
        if let Some(i) = existing {
            self.active = i;
            return;
        }
        let kind = match engine {
            Engine::Redis => TabKind::Keys(Box::new(KeysTab::new(conn, name, obj.database))),
            _ => TabKind::Docs(Box::new(DocsTab::new(conn, name, obj.database, obj.name))),
        };
        self.push_tab(kind);
    }

    /// Sends the operations a Redis / MongoDB tab asked for; confirmations go through the dialog.
    fn run_nosql_cmds(&mut self, tab: u64, cmds: Vec<NoSqlCmd>) {
        for c in cmds {
            match c.confirm {
                Some(spec) => {
                    self.confirm = Some(Confirm {
                        title: spec.title,
                        message: spec.message,
                        sql: spec.detail,
                        confirm_label: spec.button,
                        on_confirm: Confirmed::RunNoSql { tab, op: c.op, tag: c.tag },
                    });
                }
                None => self.submit_nosql(tab, c.op, c.tag),
            }
        }
    }

    fn submit_nosql(&mut self, tab: u64, op: talus_core::nosql::NoSqlOp, tag: String) {
        let (conn, database) = match self.tabs.iter().find(|t| t.id == tab).map(|t| &t.kind) {
            Some(TabKind::Keys(k)) => (k.conn, k.database.clone()),
            Some(TabKind::Docs(d)) => (d.conn, d.database.clone()),
            Some(TabKind::Console(c)) => (c.conn, c.database.clone()),
            _ => return,
        };
        let Some(info) = self.conn_info(conn, Action::ActivateTab(tab)) else { return };
        self.submit(&info, Purpose::Tab(tab), Some(database), Request::NoSql(op), Pending::NoSql { tab, tag });
    }

    fn load_builder(&mut self, tab: u64) {
        let Some(Tab { kind: TabKind::Builder(b), .. }) = self.tabs.iter().find(|t| t.id == tab) else { return };
        let (conn, database, schema) = (b.conn, b.database.clone(), b.schema.clone());
        let Some(info) = self.conn_info(conn, Action::ActivateTab(tab)) else { return };
        let job = self.submit(
            &info,
            Purpose::Tab(tab),
            Some(database.clone()),
            Request::SchemaModel { database, schema },
            Pending::Builder { tab },
        );
        if let Some(Tab { kind: TabKind::Builder(b), .. }) = self.tab_mut(tab) {
            b.model = Loadable::Loading(job);
        }
    }

    /// Loads the selected schema's tables into the model tab: replaces the model, or diffs against it.
    fn reverse_model(&mut self, tab: u64, compare: bool) {
        let fail = |app: &mut Self, msg: &str| {
            if let Some(Tab { kind: TabKind::Model(m), .. }) = app.tab_mut(tab) {
                m.fail(msg.into());
            }
        };
        let Some((conn, db, schema)) = self.selected_scope() else {
            return fail(self, "Select a database or schema in the navigator first.");
        };
        let Some((engine, cname)) = self.cfg(conn).map(|c| (c.engine, c.name.clone())) else { return };
        let schema = engine.default_schema().map(|d| schema.unwrap_or_else(|| d.into()));
        let Some(info) = self.conn_info(conn, Action::ActivateTab(tab)) else {
            return fail(self, "Connection is not available.");
        };
        let name = format!("{cname}/{db}");
        let compare = compare.then(|| (conn, engine, db.clone(), schema.clone()));
        self.submit(
            &info,
            Purpose::Tab(tab),
            Some(db.clone()),
            Request::SchemaModel { database: db, schema },
            Pending::ModelReverse { tab, name, compare },
        );
    }

    fn debug_action(&mut self, tab: u64, conn: Uuid, database: &str, oid: u32, a: DebugAction) {
        let Some(info) = self.conn_info(conn, Action::ActivateTab(tab)) else { return };
        let db = Some(database.to_string());
        match a {
            DebugAction::Describe => {
                let job = self.submit(
                    &info,
                    Purpose::Tab(tab),
                    db,
                    Request::DebugDescribe(oid),
                    Pending::DebugDescribe { tab },
                );
                if let Some(Tab { kind: TabKind::Debug(d), .. }) = self.tab_mut(tab) {
                    d.target = Loadable::Loading(job);
                }
            }
            DebugAction::Op(op) => {
                self.submit(&info, Purpose::Tab(tab), db, Request::Debug { tab, op }, Pending::Debug { tab });
            }
        }
    }

    fn load_routines(&mut self, tab: u64) {
        let Some(Tab { kind: TabKind::Routines(r), .. }) = self.tabs.iter().find(|t| t.id == tab) else { return };
        let (conn, database, schema) = (r.conn, r.database.clone(), r.schema.clone());
        let Some(info) = self.conn_info(conn, Action::ActivateTab(tab)) else { return };
        let job = self.submit(
            &info,
            Purpose::Tab(tab),
            Some(database.clone()),
            Request::ListProgrammables { database, schema },
            Pending::Routines { tab },
        );
        if let Some(Tab { kind: TabKind::Routines(r), .. }) = self.tab_mut(tab) {
            r.items = Loadable::Loading(job);
        }
    }

    fn load_diagram(&mut self, tab: u64) {
        let Some(Tab { kind: TabKind::Diagram(d), .. }) = self.tabs.iter().find(|t| t.id == tab) else { return };
        let (conn, database, schema) = (d.conn, d.database.clone(), d.schema.clone());
        let Some(info) = self.conn_info(conn, Action::ActivateTab(tab)) else { return };
        let job = self.submit(
            &info,
            Purpose::Tab(tab),
            Some(database.clone()),
            Request::SchemaModel { database, schema },
            Pending::Diagram { tab },
        );
        if let Some(Tab { kind: TabKind::Diagram(d), .. }) = self.tab_mut(tab) {
            d.loading = Some(job);
            if matches!(d.model, Loadable::NotLoaded) {
                d.model = Loadable::Loading(job);
            }
        }
    }

    fn reload_databases(&mut self, conn: Uuid) {
        if let Some(info) = self.conn_info(conn, Action::Refresh(NavSel::Conn(conn))) {
            self.submit(&info, Purpose::Meta, None, Request::ListDatabases, Pending::Databases { conn });
        }
    }

    fn refresh_monitor(&mut self, tab: u64) {
        let Some(Tab { kind: TabKind::Monitor(m), .. }) = self.tabs.iter().find(|t| t.id == tab) else { return };
        if m.running.is_some() {
            return;
        }
        let conn = m.conn;
        let Some(engine) = self.cfg(conn).map(|c| c.engine) else { return };
        let Some(sql) = talus_core::sql::admin::process_list(engine) else {
            if let Some(Tab { kind: TabKind::Monitor(m), .. }) = self.tab_mut(tab) {
                m.error = Some("SQLite has no server processes.".into());
            }
            return;
        };
        let Some(info) = self.conn_info(conn, Action::ActivateTab(tab)) else { return };
        let opts = ScriptOptions { limits: FetchLimits { max_rows: 10_000 }, continue_on_error: false };
        let job = self.submit(
            &info,
            Purpose::Tab(tab),
            None,
            Request::RunScript { script: sql.to_string(), opts },
            Pending::Monitor { tab },
        );
        if let Some(Tab { kind: TabKind::Monitor(m), .. }) = self.tab_mut(tab) {
            m.running = Some(job);
        }
    }

    fn run_find(&mut self, tab: u64) {
        let Some(Tab { kind: TabKind::Find(f), .. }) = self.tabs.iter().find(|t| t.id == tab) else { return };
        let (Some(conn), Some(database)) = (f.conn, f.database.clone()) else { return };
        let default_schema = self.cfg(conn).and_then(|c| c.engine.default_schema());
        let schema = default_schema.map(|d| f.schema.clone().unwrap_or_else(|| d.into()));
        let (needle, data) = (f.needle.clone(), f.data);
        let Some(info) = self.conn_info(conn, Action::ActivateTab(tab)) else { return };
        let job = self.submit(
            &info,
            Purpose::Tab(tab),
            Some(database.clone()),
            Request::Find { database, schema, needle, data },
            Pending::Find { tab },
        );
        if let Some(Tab { kind: TabKind::Find(f), .. }) = self.tab_mut(tab) {
            f.running = Some(job);
            f.error = None;
            f.hits = None;
        }
    }

    fn design_action(&mut self, tab: u64, a: DesignAction) {
        let DesignAction::Save(plan) = a;
        if plan.destructive.is_empty() {
            self.run_design(tab, plan.statements);
            return;
        }
        self.confirm = Some(Confirm {
            title: "Confirm Table Changes".into(),
            message: format!("These changes can lose data:\n• {}", plan.destructive.join("\n• ")),
            sql: Some(plan.statements.iter().map(|s| format!("{s};")).collect::<Vec<_>>().join("\n")),
            confirm_label: "Apply Changes".into(),
            on_confirm: Confirmed::RunDesign { tab, statements: plan.statements },
        });
    }

    fn run_design(&mut self, tab: u64, statements: Vec<String>) {
        let Some(Tab { kind: TabKind::Design(d), .. }) = self.tabs.iter().find(|t| t.id == tab) else { return };
        let conn = d.conn;
        let obj = d.existing.as_ref().map_or_else(|| d.object(), |(o, _)| o.clone());
        let Some(cfg) = self.cfg(conn).cloned() else { return };
        let Some(info) = self.conn_info(conn, Action::ActivateTab(tab)) else { return };
        let transactional = cfg.engine.capabilities().transactional_ddl;
        let job = self.submit(
            &info,
            Purpose::Meta,
            Some(obj.database.clone()),
            Request::ExecuteBatch { statements, transactional },
            Pending::DesignSave { tab, obj },
        );
        if let Some(Tab { kind: TabKind::Design(d), .. }) = self.tab_mut(tab) {
            d.saving = Some(job);
            d.info = None;
            d.error = None;
        }
    }

    fn data_action(&mut self, tab: u64, a: DataAction) {
        match a {
            DataAction::Reload => self.load_page(tab),
            DataAction::Apply(cs) => {
                let Some(Tab { kind: TabKind::Data(d), .. }) = self.tabs.iter().find(|t| t.id == tab) else { return };
                let (conn, obj) = (d.conn, d.obj.clone());
                let Some(structure) = d.structure.loaded().cloned() else { return };
                let Some(info) = self.conn_info(conn, Action::ActivateTab(tab)) else { return };
                let job = self.submit(
                    &info,
                    Purpose::Tab(tab),
                    Some(obj.database.clone()),
                    Request::Apply { table: obj, structure, changes: cs },
                    Pending::Apply { tab },
                );
                if let Some(Tab { kind: TabKind::Data(d), .. }) = self.tab_mut(tab) {
                    d.applying = Some(job);
                    d.grid.editing = None;
                }
            }
        }
    }

    // ---------- layout ----------

    fn active_tab_ui(&mut self, ui: &mut egui::Ui) {
        let idx = self.active.min(self.tabs.len() - 1);
        let tab_id = self.tabs[idx].id;
        match &mut self.tabs[idx].kind {
            TabKind::Objects => self.home_ui(ui),
            TabKind::Sql(s) => {
                let meta = Self::meta_view(&self.connections, &self.states, s.conn, s.database.as_deref());
                // Make sure the target database's object list exists for completion.
                if let Some(cfg) = s.conn.and_then(|c| self.connections.iter().find(|x| x.id == c)) {
                    let db = Self::target_db(cfg, s.database.as_deref());
                    if let Some(st) = self.states.get(&cfg.id).filter(|st| st.status.is_open()) {
                        if let Some(def) = cfg.engine.default_schema() {
                            match st.databases.iter().find(|d| d.name == db).map(|d| &d.schemas) {
                                Some(Loadable::NotLoaded) => {
                                    self.actions.push(Action::LoadSchemas { conn: cfg.id, db })
                                }
                                Some(Loadable::Loaded(_))
                                    if st.objects(&db, Some(def)).is_some_and(Loadable::needs_load) =>
                                {
                                    self.actions.push(Action::LoadObjects {
                                        conn: cfg.id,
                                        db,
                                        schema: Some(def.into()),
                                    })
                                }
                                _ => {}
                            }
                        } else if st.objects(&db, None).is_some_and(Loadable::needs_load) {
                            self.actions.push(Action::LoadObjects { conn: cfg.id, db, schema: None });
                        }
                    }
                }
                let ctx = SqlCtx {
                    connections: &self.connections,
                    states: &self.states,
                    meta: &meta,
                    completion: self.settings.code_completion,
                };
                let acts = s.ui(ui, tab_id, &ctx);
                for a in acts {
                    match a {
                        SqlAction::Run(mode) => self.run_sql(tab_id, mode),
                        SqlAction::Stop => {
                            if let Some(Tab { kind: TabKind::Sql(s), .. }) = self.tabs.iter().find(|t| t.id == tab_id)
                                && let Some(j) = s.running
                            {
                                self.worker.cancel(j);
                            }
                        }
                        SqlAction::Tx(kind) => {
                            let engine = self
                                .tabs
                                .iter()
                                .find_map(|t| match &t.kind {
                                    TabKind::Sql(s) if t.id == tab_id => s.conn,
                                    _ => None,
                                })
                                .and_then(|c| self.cfg(c))
                                .map(|c| c.engine);
                            let begin = engine.map_or("BEGIN", Engine::begin_sql);
                            match kind {
                                TxAction::Begin => self.run_tx(tab_id, true, begin),
                                TxAction::Commit => self.run_tx(tab_id, false, "COMMIT"),
                                TxAction::Rollback => self.run_tx(tab_id, false, "ROLLBACK"),
                            }
                        }
                        SqlAction::Save => self.save_sql_tab(tab_id, false),
                        SqlAction::NeedColumns(tables) => self.load_columns_for(tab_id, tables),
                        SqlAction::TargetChanged => {
                            self.worker.close_tab(tab_id);
                            if let Some(Tab { kind: TabKind::Sql(s), .. }) = self.tab_mut(tab_id) {
                                s.tx_open = false; // session dropped: server rolled it back
                            }
                            if let Some(Tab { kind: TabKind::Sql(s), .. }) = self.tabs.iter().find(|t| t.id == tab_id)
                                && let Some(c) = s.conn
                                && !self.states.get(&c).is_some_and(|x| {
                                    x.status.is_open() || matches!(x.status, ConnStatus::Connecting(_))
                                })
                            {
                                self.actions.push(Action::Open(c));
                            }
                        }
                    }
                }
            }
            TabKind::Data(d) => {
                let acts = d.ui(ui, tab_id);
                for a in acts {
                    self.data_action(tab_id, a);
                }
            }
            TabKind::Structure(s) => s.ui(ui),
            TabKind::Sync(t) => {
                let acts = t.ui(ui, &self.connections, &self.states);
                let delete_missing = t.delete_missing;
                for a in acts {
                    match a {
                        SyncAction::Compare => self.compare_sync(tab_id),
                        SyncAction::Data { tables, apply: false } => self.start_data_sync(tab_id, tables, false),
                        SyncAction::Data { tables, apply: true } => {
                            self.confirm = Some(Confirm {
                                title: "Confirm Data Sync".into(),
                                message: format!(
                                    "Rows of {} table(s) on the target will be inserted, updated{} so they match the source, in one transaction per table:\n• {}",
                                    tables.len(),
                                    if delete_missing { " and DELETED" } else { "" },
                                    tables.join("\n• ")
                                ),
                                sql: None,
                                confirm_label: "Sync Data".into(),
                                on_confirm: Confirmed::RunDataSync { tab: tab_id, tables },
                            });
                        }
                        SyncAction::Execute(plan) if plan.destructive.is_empty() => {
                            self.run_sync(tab_id, plan.statements)
                        }
                        SyncAction::Execute(plan) => {
                            self.confirm = Some(Confirm {
                                title: "Confirm Structure Sync".into(),
                                message: format!(
                                    "These changes can lose data on the target:\n• {}",
                                    plan.destructive.join("\n• ")
                                ),
                                sql: Some(
                                    plan.statements.iter().map(|s| format!("{s};")).collect::<Vec<_>>().join("\n"),
                                ),
                                confirm_label: "Execute".into(),
                                on_confirm: Confirmed::RunSync { tab: tab_id, statements: plan.statements },
                            });
                        }
                    }
                }
            }
            TabKind::Diagram(d) => {
                let acts = d.ui(ui, tab_id);
                for a in acts {
                    match a {
                        DiagramAction::Nav(a) => self.actions.push(a),
                        DiagramAction::Reload => self.load_diagram(tab_id),
                        DiagramAction::ExportSvg(svg) => {
                            let Some(path) = rfd::FileDialog::new()
                                .add_filter("SVG", &["svg"])
                                .set_file_name("diagram.svg")
                                .save_file()
                            else {
                                continue;
                            };
                            match std::fs::write(&path, svg) {
                                Ok(()) => self.notify(format!("Saved {}", path.display()), false),
                                Err(e) => self.notify(format!("Cannot save diagram: {e}"), true),
                            }
                        }
                    }
                }
            }
            TabKind::Monitor(m) => {
                let acts = m.ui(ui, tab_id);
                let conn = m.conn;
                for a in acts {
                    match a {
                        MonitorAction::Refresh => self.refresh_monitor(tab_id),
                        MonitorAction::Kill { id, terminate } => {
                            let Some(engine) = self.cfg(conn).map(|c| c.engine) else { continue };
                            match talus_core::sql::admin::kill(engine, &id, terminate) {
                                Ok(sql) => {
                                    self.confirm = Some(Confirm {
                                        title: if terminate { "Kill Session".into() } else { "Cancel Query".into() },
                                        message: if terminate {
                                            format!(
                                                "Disconnect process {id}? Its open transaction will be rolled back."
                                            )
                                        } else {
                                            format!("Cancel the statement running in process {id}?")
                                        },
                                        sql: Some(sql.clone()),
                                        confirm_label: if terminate {
                                            "Kill Session".into()
                                        } else {
                                            "Cancel Query".into()
                                        },
                                        on_confirm: Confirmed::Kill { tab: tab_id, sql },
                                    });
                                }
                                Err(e) => self.notify(e.to_string(), true),
                            }
                        }
                    }
                }
            }
            TabKind::Find(f) => {
                let acts = f.ui(ui, tab_id, &self.connections, &self.states);
                for a in acts {
                    match a {
                        FindAction::Nav(a) => self.actions.push(a),
                        FindAction::Stop => {
                            if let Some(Tab { kind: TabKind::Find(f), .. }) = self.tabs.iter().find(|t| t.id == tab_id)
                                && let Some(j) = f.running
                            {
                                self.worker.cancel(j);
                            }
                        }
                        FindAction::Run => self.run_find(tab_id),
                    }
                }
            }
            TabKind::Design(d) => {
                if let Some(a) = d.ui(ui, tab_id) {
                    self.design_action(tab_id, a);
                }
            }
            TabKind::Keys(k) => {
                let cmds = k.ui(ui, tab_id);
                self.run_nosql_cmds(tab_id, cmds);
            }
            TabKind::Docs(d) => {
                let cmds = d.ui(ui, tab_id);
                self.run_nosql_cmds(tab_id, cmds);
            }
            TabKind::Console(c) => {
                let cmds = c.ui(ui, tab_id);
                self.run_nosql_cmds(tab_id, cmds);
            }
            TabKind::Profile(p) => {
                for a in p.ui(ui, tab_id) {
                    match a {
                        ProfileAction::Reload => self.load_profile(tab_id),
                    }
                }
            }
            TabKind::Jobs(t) => {
                let conns: Vec<ConnChoice> = self
                    .connections
                    .iter()
                    .filter(|c| c.engine.is_sql())
                    .map(|c| ConnChoice { id: c.id, name: c.name.clone() })
                    .collect();
                let running = self.jobs_running.clone();
                #[cfg(target_os = "macos")]
                let installed = |j: &talus_core::jobs::Job| crate::launchd::is_installed(j);
                #[cfg(not(target_os = "macos"))]
                let installed = |_: &talus_core::jobs::Job| false;
                let now = chrono::Local::now().naive_local();
                let acts = t.ui(ui, &self.jobs, &conns, &installed, &|id| running.contains(&id), now);
                for a in acts {
                    self.job_action(a);
                }
            }
            TabKind::Model(m) => {
                let acts = m.ui(ui, tab_id);
                let (conn, database) = m.target.clone().unzip();
                for a in acts {
                    match a {
                        ModelAction::Reverse => self.reverse_model(tab_id, false),
                        ModelAction::Compare => self.reverse_model(tab_id, true),
                        ModelAction::Copy(text) => self.actions.push(Action::CopyText(text)),
                        ModelAction::OpenSql(text) => {
                            self.actions.push(Action::NewQuery { conn, database: database.clone(), text })
                        }
                    }
                }
            }
            TabKind::Builder(b) => {
                let (conn, database) = (b.conn, b.database.clone());
                for a in b.ui(ui, tab_id) {
                    match a {
                        BuilderAction::Reload => self.load_builder(tab_id),
                        BuilderAction::Copy(text) => self.actions.push(Action::CopyText(text)),
                        BuilderAction::OpenInQuery(text) => self.actions.push(Action::NewQuery {
                            conn: Some(conn),
                            database: Some(database.clone()),
                            text,
                        }),
                    }
                }
            }
            TabKind::Users(u) => {
                let (conn, engine) = (u.conn, u.engine);
                for a in u.ui(ui, tab_id) {
                    match a {
                        UsersAction::Reload => self.load_users(tab_id),
                        UsersAction::LoadGrants(user) => {
                            if let Some(job) =
                                self.users_job(tab_id, Request::UserGrants(user), Pending::Grants { tab: tab_id })
                                && let Some(Tab { kind: TabKind::Users(u), .. }) = self.tab_mut(tab_id)
                            {
                                u.grants = Loadable::Loading(job);
                            }
                        }
                        UsersAction::Create { name, host, password } => {
                            match talus_core::sql::users::create_user_sql(engine, &name, &host, &password) {
                                Ok((sql, shown)) => {
                                    self.confirm = Some(Confirm {
                                        title: "Create User".into(),
                                        message: format!("Create the account “{}” on this server?", name.trim()),
                                        sql: Some(shown),
                                        confirm_label: "Create".into(),
                                        on_confirm: Confirmed::RunUserSql { tab: tab_id, sql },
                                    });
                                }
                                Err(e) => self.notify(e.to_string(), true),
                            }
                        }
                        UsersAction::Drop(user) => match talus_core::sql::users::drop_user_sql(engine, &user) {
                            Ok(sql) => {
                                self.confirm = Some(Confirm {
                                    title: "Drop User".into(),
                                    message: format!(
                                        "Permanently delete the account “{}”? Its grants disappear with it.",
                                        user.display()
                                    ),
                                    sql: Some(sql.clone()),
                                    confirm_label: "Drop User".into(),
                                    on_confirm: Confirmed::RunUserSql { tab: tab_id, sql },
                                });
                            }
                            Err(e) => self.notify(e.to_string(), true),
                        },
                        UsersAction::RunGrant { title, sql } => {
                            self.confirm = Some(Confirm {
                                title: title.clone(),
                                message: format!("{title} for this account? The statement below runs on the server."),
                                sql: Some(sql.clone()),
                                confirm_label: title.split_whitespace().next().unwrap_or("Run").to_string(),
                                on_confirm: Confirmed::RunUserSql { tab: tab_id, sql },
                            });
                        }
                        UsersAction::OpenTemplate(text) => {
                            self.actions.push(Action::NewQuery { conn: Some(conn), database: None, text })
                        }
                    }
                }
            }
            TabKind::Routines(r) => {
                let (conn, database, engine, schema) = (r.conn, r.database.clone(), r.engine, r.schema.clone());
                for a in r.ui(ui, tab_id) {
                    match a {
                        ProgAction::Reload => self.load_routines(tab_id),
                        ProgAction::Copy(text) => self.actions.push(Action::CopyText(text)),
                        ProgAction::OpenInQuery(text) => self.actions.push(Action::NewQuery {
                            conn: Some(conn),
                            database: Some(database.clone()),
                            text,
                        }),
                        ProgAction::Drop(item) => {
                            match talus_core::programmability::drop_sql(engine, &database, schema.as_deref(), &item) {
                                Ok(sql) => {
                                    self.confirm = Some(Confirm {
                                        title: format!("Drop {}", item.kind.label()),
                                        message: format!(
                                            "Permanently drop {} “{}”? Anything depending on it may break.",
                                            item.kind.label().to_lowercase(),
                                            item.name
                                        ),
                                        sql: Some(sql.clone()),
                                        confirm_label: "Drop".into(),
                                        on_confirm: Confirmed::DropProgrammable { tab: tab_id, sql },
                                    });
                                }
                                Err(e) => self.notify(e.to_string(), true),
                            }
                        }
                        ProgAction::Debug { oid, name } => {
                            let tab = DebugTab::new(conn, database.clone(), oid, name);
                            self.push_tab(TabKind::Debug(Box::new(tab)));
                        }
                    }
                }
            }
            TabKind::Debug(d) => {
                let (conn, database, oid) = (d.conn, d.database.clone(), d.oid);
                for a in d.ui(ui, tab_id) {
                    self.debug_action(tab_id, conn, &database, oid, a);
                }
            }
            TabKind::History(h) => match h.ui(ui) {
                Some(HistoryAction::Reload) => self.reload_history(),
                Some(HistoryAction::Clear) => {
                    if let Err(e) = self.history.clear() {
                        self.notify(format!("Could not clear history: {e}"), true);
                    }
                    self.reload_history();
                }
                Some(HistoryAction::Open(sql)) => {
                    self.actions.push(Action::NewQuery { conn: None, database: None, text: sql })
                }
                None => {}
            },
        }
    }

    pub fn draw(&mut self, root: &mut egui::Ui) {
        let ctx = &root.ctx().clone();
        let wanted = (self.settings.theme, self.settings.editor_font_size);
        if self.applied_theme != Some(wanted) {
            theme::apply(ctx, wanted.0, wanted.1);
            self.applied_theme = Some(wanted);
        }
        self.process_events();
        self.tick_jobs(ctx);
        if self.confirm.is_none() && self.prompt.is_none() {
            self.shortcuts(ctx);
        }
        let p = theme::current(ctx);
        // Sidebar and information pane span the full window height; the top bar, tabs and footer
        // belong to the centre column.
        if self.show_nav {
            egui::Panel::left("nav")
                .resizable(true)
                .default_size(252.0)
                .size_range(200.0..=520.0)
                .frame(egui::Frame::NONE.fill(p.sidebar).inner_margin(egui::Margin {
                    left: 8,
                    right: 8,
                    top: 8,
                    bottom: 0,
                }))
                .show(root, |ui| self.sidebar(ui));
        }
        if self.show_info {
            egui::Panel::right("info")
                .resizable(true)
                .default_size(300.0)
                .size_range(220.0..=640.0)
                .frame(egui::Frame::NONE.fill(p.bg))
                .show(root, |ui| {
                    egui::Panel::top("info-head")
                        .exact_size(TOP_BAR_H)
                        .frame(egui::Frame::NONE.inner_margin(egui::Margin::symmetric(16, 0)))
                        .show(ui, |ui| {
                            ui.horizontal_centered(|ui| {
                                ui.label(RichText::new("Information").font(theme::semibold(15.0)).color(p.text));
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if widgets::icon_button(ui, icons::X, "Hide information pane (⌘2)", false).clicked()
                                    {
                                        self.show_info = false;
                                    }
                                });
                            });
                        });
                    egui::CentralPanel::default().frame(egui::Frame::NONE.inner_margin(16.0)).show(ui, |ui| {
                        let mut acts = Vec::new();
                        panels::info_pane(ui, &self.connections, &self.states, &self.selection, &self.ddl, &mut acts);
                        self.actions.extend(acts);
                    });
                });
        }
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(p.bg)).show(root, |ui| {
            egui::Panel::top("topbar")
                .exact_size(TOP_BAR_H)
                .frame(egui::Frame::NONE.fill(p.bg).inner_margin(egui::Margin::symmetric(12, 0)))
                .show(ui, |ui| self.top_bar(ui));
            egui::Panel::bottom("statusbar")
                .frame(egui::Frame::NONE.fill(p.bg).inner_margin(egui::Margin::symmetric(14, 4)))
                .show(ui, |ui| self.status_bar(ui));
            let home = matches!(self.tabs[self.active.min(self.tabs.len() - 1)].kind, TabKind::Objects);
            if self.tabs.len() > 1 {
                egui::Panel::top("tabbar")
                    .frame(egui::Frame::NONE.fill(p.bg).inner_margin(egui::Margin::symmetric(10, 6)))
                    .show(ui, |ui| self.tab_bar(ui));
            }
            let margin = if home { egui::Margin::symmetric(36, 8) } else { egui::Margin::symmetric(10, 6) };
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE.inner_margin(margin))
                .show(ui, |ui| self.active_tab_ui(ui));
            // The information pane is told apart by a soft shadow on its edge, not a line or a tint.
            if self.show_info {
                let r = ui.max_rect();
                widgets::edge_shadow(ui.painter(), r, r.right());
            }
        });
        self.run_actions();
        self.dialogs(ctx);
        self.run_actions();
        if self.last_autosave.elapsed() > Duration::from_secs(20) {
            self.save_session();
            self.last_autosave = Instant::now();
        }
        if let Some(t) = self.startup_probe.take() {
            eprintln!("first frame after {} ms", t.elapsed().as_millis());
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        // Keep polling while jobs run (events also trigger repaints from the worker).
        if !self.pending.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.draw(ui);
    }

    fn on_exit(&mut self) {
        self.save_session();
    }
}
