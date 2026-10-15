//! Shared workspace window: sign in to a self-hosted `talus-server`, pick a workspace, manage its
//! members and share / pull connection definitions. Network calls run on the tokio runtime and
//! report back through a channel, so the UI never blocks.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};

use egui::RichText;
use talus_core::Secret;
use talus_core::model::{ConnectionConfig, Engine};
use talus_core::store::config::{load_json, save_json};
use talus_core::store::secrets::SecretStore;
use talus_core::workspace::{
    RemoteConnection, RemoteWorkspace, Role, TOKEN_SLOT, WorkspaceClient, WorkspaceDetails, WorkspaceError,
    WorkspaceLink, apply_pull,
};
use uuid::Uuid;

use crate::theme;

enum Msg {
    SignedIn(Result<WorkspaceClient, WorkspaceError>),
    Workspaces(Result<Vec<RemoteWorkspace>, WorkspaceError>),
    Details(Result<WorkspaceDetails, WorkspaceError>),
    Remote {
        result: Result<Vec<RemoteConnection>, WorkspaceError>,
        apply: bool,
    },
    Pushed {
        id: Uuid,
        result: Result<u64, WorkspaceError>,
    },
    /// A write that changes the workspace; `reload` refetches everything afterwards.
    Done {
        what: &'static str,
        result: Result<(), WorkspaceError>,
        reload: bool,
    },
    Created(Result<RemoteWorkspace, WorkspaceError>),
}

pub struct WorkspaceDialog {
    pub open: bool,
    rt: tokio::runtime::Handle,
    repaint: Arc<dyn Fn() + Send + Sync>,
    secrets: Arc<dyn SecretStore>,
    link_path: PathBuf,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    link: WorkspaceLink,
    client: Option<Arc<WorkspaceClient>>,
    server: String,
    user: String,
    password: String,
    new_workspace: String,
    member: String,
    member_role: Role,
    workspaces: Vec<RemoteWorkspace>,
    details: Option<WorkspaceDetails>,
    remote: Vec<RemoteConnection>,
    busy: usize,
    message: Option<(String, bool)>,
    restored: bool,
}

fn error_text(e: &WorkspaceError) -> String {
    match e {
        WorkspaceError::Conflict { .. } => {
            "Someone changed this connection on the server. Pull first, then share again.".into()
        }
        other => other.to_string(),
    }
}

fn remote_name(r: &RemoteConnection) -> String {
    r.data.get("name").and_then(|n| n.as_str()).unwrap_or(&r.id).to_string()
}

impl WorkspaceDialog {
    pub fn new(
        rt: tokio::runtime::Handle,
        repaint: Arc<dyn Fn() + Send + Sync>,
        secrets: Arc<dyn SecretStore>,
        link_path: PathBuf,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let link: WorkspaceLink = load_json(&link_path).unwrap_or_default();
        Self {
            open: false,
            rt,
            repaint,
            secrets,
            link_path,
            tx,
            rx,
            server: link.server.clone(),
            user: link.user.clone(),
            link,
            client: None,
            password: String::new(),
            new_workspace: String::new(),
            member: String::new(),
            member_role: Role::Editor,
            workspaces: Vec::new(),
            details: None,
            remote: Vec::new(),
            busy: 0,
            message: None,
            restored: false,
        }
    }

    /// Opens the window; the saved session token is read from the secret store the first time.
    pub fn show(&mut self) {
        self.open = true;
        if self.restored {
            return;
        }
        self.restored = true;
        if self.link.server.is_empty() {
            return;
        }
        match self.secrets.get(&TOKEN_SLOT.0, TOKEN_SLOT.1) {
            Ok(Some(token)) => match WorkspaceClient::new(&self.link.server, Some(token)) {
                Ok(c) => {
                    self.client = Some(Arc::new(c));
                    self.reload();
                }
                Err(e) => self.message = Some((error_text(&e), true)),
            },
            Ok(None) => {}
            Err(e) => self.message = Some((e, true)),
        }
    }

    fn save_link(&mut self) {
        if let Err(e) = save_json(&self.link_path, &self.link) {
            self.message = Some((format!("Could not save the workspace link: {e}"), true));
        }
    }

    fn spawn<Fut>(&mut self, f: impl FnOnce(Arc<WorkspaceClient>, Sender<Msg>) -> Fut)
    where
        Fut: Future<Output = ()> + Send + 'static,
    {
        let Some(client) = self.client.clone() else { return };
        self.busy += 1;
        let fut = f(client, self.tx.clone());
        let repaint = self.repaint.clone();
        self.rt.spawn(async move {
            fut.await;
            repaint();
        });
    }

    /// Fetches the workspace list and, when one is selected, its details and connections.
    fn reload(&mut self) {
        self.spawn(|c, tx| async move {
            let _ = tx.send(Msg::Workspaces(c.workspaces().await));
        });
        self.reload_selected();
    }

    fn reload_selected(&mut self) {
        let ws = self.link.workspace.clone();
        if ws.is_empty() {
            return;
        }
        let ws2 = ws.clone();
        self.spawn(|c, tx| async move {
            let _ = tx.send(Msg::Details(c.details(&ws).await));
        });
        self.fetch_remote(ws2, false);
    }

    fn fetch_remote(&mut self, ws: String, apply: bool) {
        self.spawn(|c, tx| async move {
            let _ = tx.send(Msg::Remote { result: c.connections(&ws).await, apply });
        });
    }

    fn sign_in(&mut self) {
        let (server, user) = (self.server.trim().to_string(), self.user.trim().to_string());
        let password = Secret::new(std::mem::take(&mut self.password));
        self.busy += 1;
        let (tx, repaint) = (self.tx.clone(), self.repaint.clone());
        self.rt.spawn(async move {
            let _ = tx.send(Msg::SignedIn(WorkspaceClient::login(&server, &user, &password).await));
            repaint();
        });
    }

    fn sign_out(&mut self) {
        if let Some(c) = self.client.take() {
            // Best effort: the token is dropped locally either way.
            self.rt.spawn(async move {
                let _ = c.logout().await;
            });
        }
        let _ = self.secrets.delete(&TOKEN_SLOT.0, TOKEN_SLOT.1);
        self.link.workspace.clear();
        self.link.workspace_name.clear();
        self.link.revs.clear();
        self.save_link();
        self.workspaces.clear();
        self.details = None;
        self.remote.clear();
    }

    fn fail(&mut self, e: WorkspaceError) {
        if matches!(e, WorkspaceError::Server { status: 401, .. }) {
            self.client = None;
            let _ = self.secrets.delete(&TOKEN_SLOT.0, TOKEN_SLOT.1);
            self.message = Some(("The session ended. Sign in again.".into(), true));
        } else {
            self.message = Some((error_text(&e), true));
        }
    }

    /// Returns true when the local connection list changed (the caller saves it).
    fn handle(&mut self, msg: Msg, connections: &mut Vec<ConnectionConfig>) -> bool {
        self.busy = self.busy.saturating_sub(1);
        match msg {
            Msg::SignedIn(Ok(c)) => {
                let token = c.token().cloned().unwrap_or_default();
                if let Err(e) = self.secrets.set(&TOKEN_SLOT.0, TOKEN_SLOT.1, &token) {
                    self.message = Some((format!("Signed in, but the token could not be kept: {e}"), true));
                }
                if self.link.server != self.server.trim() || self.link.user != self.user.trim() {
                    self.link = WorkspaceLink {
                        server: self.server.trim().into(),
                        user: self.user.trim().into(),
                        ..Default::default()
                    };
                    self.save_link();
                }
                self.client = Some(Arc::new(c));
                self.reload();
            }
            Msg::SignedIn(Err(e)) => self.message = Some((error_text(&e), true)),
            Msg::Workspaces(Ok(list)) => {
                self.workspaces = list;
                if !self.workspaces.iter().any(|w| w.id == self.link.workspace) {
                    self.link.workspace.clear();
                    self.link.workspace_name.clear();
                    self.details = None;
                    self.remote.clear();
                    self.save_link();
                }
            }
            Msg::Details(Ok(d)) => self.details = Some(d),
            Msg::Remote { result: Ok(list), apply } => {
                if apply {
                    let r = apply_pull(connections, &mut self.link, &list);
                    self.save_link();
                    let mut text = format!("Pulled: {} added, {} updated.", r.added, r.updated);
                    if !r.unshared.is_empty() {
                        text += &format!(" No longer shared (kept locally): {}.", r.unshared.join(", "));
                    }
                    if !r.skipped.is_empty() {
                        text += &format!(" Skipped {}: {}.", r.skipped.len(), r.skipped[0].1);
                    }
                    self.message = Some((text, !r.skipped.is_empty()));
                    self.remote = list;
                    return r.added + r.updated > 0;
                }
                self.remote = list;
            }
            Msg::Pushed { id, result: Ok(rev) } => {
                self.link.revs.insert(id, rev);
                self.save_link();
                self.message = Some(("Shared.".into(), false));
                self.reload_selected();
            }
            Msg::Created(Ok(w)) => {
                self.link.workspace = w.id.clone();
                self.link.workspace_name = w.name.clone();
                self.link.revs.clear();
                self.save_link();
                self.new_workspace.clear();
                self.message = Some((format!("Created “{}”.", w.name), false));
                self.reload();
            }
            Msg::Done { result: Ok(()), reload, what } => {
                self.message = Some((what.into(), false));
                if reload {
                    self.reload();
                }
            }
            Msg::Workspaces(Err(e))
            | Msg::Details(Err(e))
            | Msg::Remote { result: Err(e), .. }
            | Msg::Pushed { result: Err(e), .. }
            | Msg::Created(Err(e))
            | Msg::Done { result: Err(e), .. } => self.fail(e),
        }
        false
    }

    /// Draws the window; true when `connections` changed.
    pub fn ui(&mut self, ctx: &egui::Context, connections: &mut Vec<ConnectionConfig>) -> bool {
        let mut changed = false;
        while let Ok(msg) = self.rx.try_recv() {
            changed |= self.handle(msg, connections);
        }
        if !self.open {
            return changed;
        }
        let mut open = self.open;
        egui::Window::new("Shared Workspace")
            .id(egui::Id::new("workspace-dialog"))
            .open(&mut open)
            .collapsible(false)
            .default_width(560.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, true]).max_height(560.0).show(ui, |ui| {
                    if self.client.is_some() {
                        self.signed_in_ui(ui, connections);
                    } else {
                        self.sign_in_ui(ui);
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if self.busy > 0 {
                            ui.spinner();
                        }
                        if let Some((text, error)) = &self.message {
                            let p = theme::current(ui.ctx());
                            ui.label(RichText::new(text).color(if *error { p.danger } else { p.text_dim }));
                        }
                    });
                });
            });
        self.open = open;
        changed
    }

    fn sign_in_ui(&mut self, ui: &mut egui::Ui) {
        let p = theme::current(ui.ctx());
        ui.label(
            RichText::new(
                "Share connection definitions with a team through a Talus server you host. Passwords, tunnel \
                 secrets and file paths never leave this Mac.",
            )
            .color(p.text_dim),
        );
        ui.add_space(6.0);
        egui::Grid::new("ws-signin").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
            let l = ui.label("Server");
            ui.add(
                crate::ui::widgets::field(&mut self.server).hint_text("https://talus.example.com").desired_width(300.0),
            )
            .labelled_by(l.id);
            ui.end_row();
            let l = ui.label("User");
            ui.add(crate::ui::widgets::field(&mut self.user).desired_width(300.0)).labelled_by(l.id);
            ui.end_row();
            let l = ui.label("Password");
            ui.add(crate::ui::widgets::field(&mut self.password).password(true).desired_width(300.0)).labelled_by(l.id);
            ui.end_row();
        });
        let ready = !self.server.trim().is_empty() && !self.user.trim().is_empty() && !self.password.is_empty();
        if ui.add_enabled(ready && self.busy == 0, egui::Button::new("Sign In")).clicked() {
            self.message = None;
            self.sign_in();
        }
    }

    fn signed_in_ui(&mut self, ui: &mut egui::Ui, connections: &[ConnectionConfig]) {
        let p = theme::current(ui.ctx());
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{} on {}", self.link.user, self.link.server)).strong());
            if ui.button("Refresh").clicked() {
                self.reload();
            }
            if ui.button("Sign Out").clicked() {
                self.sign_out();
            }
        });
        ui.separator();
        ui.horizontal(|ui| {
            let l = ui.label("Workspace");
            let mut picked = None;
            egui::ComboBox::from_id_salt("ws-pick")
                .selected_text(if self.link.workspace_name.is_empty() {
                    "Choose…"
                } else {
                    &self.link.workspace_name
                })
                .show_ui(ui, |ui| {
                    for w in &self.workspaces {
                        if ui
                            .selectable_label(w.id == self.link.workspace, format!("{} ({})", w.name, w.role.label()))
                            .clicked()
                        {
                            picked = Some(w.clone());
                        }
                    }
                })
                .response
                .labelled_by(l.id);
            if let Some(w) = picked
                && w.id != self.link.workspace
            {
                self.link = WorkspaceLink {
                    workspace: w.id,
                    workspace_name: w.name,
                    revs: Default::default(),
                    ..self.link.clone()
                };
                self.details = None;
                self.remote.clear();
                self.save_link();
                self.reload_selected();
            }
        });
        ui.horizontal(|ui| {
            let l = ui.label("New workspace");
            ui.add(crate::ui::widgets::field(&mut self.new_workspace).desired_width(220.0)).labelled_by(l.id);
            if ui
                .add_enabled(!self.new_workspace.trim().is_empty() && self.busy == 0, egui::Button::new("Create"))
                .clicked()
            {
                let name = self.new_workspace.trim().to_string();
                self.spawn(|c, tx| async move {
                    let _ = tx.send(Msg::Created(c.create_workspace(&name).await));
                });
            }
        });
        let Some(details) = self.details.clone().filter(|d| d.id == self.link.workspace) else { return };
        let (role, ws) = (details.role, details.id.clone());
        ui.add_space(8.0);
        ui.label(RichText::new(format!("Members — you are {}", role.label())).strong());
        let me = self.link.user.to_ascii_lowercase();
        for (name, r) in &details.members {
            ui.horizontal(|ui| {
                ui.label(format!("{name}  ·  {}", r.label()));
                let may_remove = (role == Role::Owner && *name != me) || (*name == me && role != Role::Owner);
                if may_remove && ui.small_button(if *name == me { "Leave" } else { "Remove" }).clicked() {
                    let (ws, who) = (ws.clone(), name.clone());
                    let leaving = *name == me;
                    self.spawn(|c, tx| async move {
                        let result = c.remove_member(&ws, &who).await;
                        let what = if leaving { "You left the workspace." } else { "Member removed." };
                        let _ = tx.send(Msg::Done { what, result, reload: true });
                    });
                }
            });
        }
        if role == Role::Owner {
            ui.horizontal(|ui| {
                let l = ui.label("Add member");
                ui.add(crate::ui::widgets::field(&mut self.member).hint_text("user name").desired_width(160.0))
                    .labelled_by(l.id);
                egui::ComboBox::from_id_salt("ws-member-role").selected_text(self.member_role.label()).show_ui(
                    ui,
                    |ui| {
                        for r in [Role::Viewer, Role::Editor, Role::Owner] {
                            ui.selectable_value(&mut self.member_role, r, r.label());
                        }
                    },
                );
                if ui.add_enabled(!self.member.trim().is_empty() && self.busy == 0, egui::Button::new("Add")).clicked()
                {
                    let (ws, who, r) = (ws.clone(), self.member.trim().to_string(), self.member_role);
                    self.member.clear();
                    self.spawn(|c, tx| async move {
                        let result = c.set_member(&ws, &who, r).await;
                        let _ = tx.send(Msg::Done { what: "Member saved.", result, reload: true });
                    });
                }
            });
            ui.label(
                RichText::new("Accounts are created by the server administrator (talus-server set-user).")
                    .small()
                    .color(p.text_dim),
            );
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Shared connections").strong());
            if ui
                .add_enabled(self.busy == 0, egui::Button::new("Pull"))
                .on_hover_text("Add and update connections from the workspace")
                .clicked()
            {
                self.fetch_remote(ws.clone(), true);
            }
        });
        if self.remote.is_empty() {
            ui.label(RichText::new("Nothing shared yet.").color(p.text_dim));
        }
        for r in self.remote.clone() {
            ui.horizontal(|ui| {
                let state = match Uuid::parse_str(&r.id).ok().and_then(|id| self.link.revs.get(&id)) {
                    Some(rev) if *rev == r.rev => "up to date",
                    Some(_) => "changed on the server",
                    None => "not on this Mac",
                };
                ui.label(format!("{}  ·  {}  ·  by {}", remote_name(&r), state, r.updated_by));
                if role >= Role::Editor
                    && ui
                        .small_button("Unshare")
                        .on_hover_text("Remove it from the workspace; local copies stay")
                        .clicked()
                    && let Ok(id) = Uuid::parse_str(&r.id)
                {
                    let (ws, rev) = (ws.clone(), r.rev);
                    self.spawn(|c, tx| async move {
                        let result = c.unshare(&ws, id, rev).await;
                        let _ = tx.send(Msg::Done { what: "Unshared.", result, reload: true });
                    });
                }
            });
        }
        if role >= Role::Editor {
            ui.add_space(8.0);
            ui.label(RichText::new("Your connections").strong());
            for cfg in connections.iter().filter(|c| c.engine != Engine::Sqlite) {
                ui.horizontal(|ui| {
                    let rev = self.link.revs.get(&cfg.id).copied();
                    ui.label(format!("{}  ·  {}", cfg.name, if rev.is_some() { "shared" } else { "local" }));
                    if ui
                        .add_enabled(self.busy == 0, egui::Button::new(if rev.is_some() { "Update" } else { "Share" }))
                        .clicked()
                    {
                        let (ws, cfg) = (ws.clone(), cfg.clone());
                        let id = cfg.id;
                        self.spawn(|c, tx| async move {
                            let result = c.push(&ws, &cfg, rev.unwrap_or(0)).await;
                            let _ = tx.send(Msg::Pushed { id, result });
                        });
                    }
                });
            }
            ui.label(RichText::new("SQLite files stay on this Mac.").small().color(p.text_dim));
        }
    }
}
