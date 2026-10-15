//! New/Edit Connection window.

use egui::RichText;
use talus_core::Secret;
use talus_core::model::{
    CLOUD_PRESETS, CloudPreset, ConnectionConfig, Engine, HttpProxyConfig, PhpTunnelConfig, SnowflakeAuth, SshAuth,
    SshConfig, SslMode,
};

use crate::theme;
use crate::worker::{ConnInfo, JobId};

#[derive(Debug, Clone)]
pub enum TestState {
    Idle,
    Running(JobId),
    Ok(String),
    Failed(String),
}

pub struct ConnDialog {
    pub cfg: ConnectionConfig,
    pub password: String,
    /// SSH password / key passphrase, or the HTTP proxy password.
    pub tunnel_secret: String,
    use_ssh: bool,
    ssh: SshConfig,
    ssh_port_text: String,
    use_http: bool,
    http: HttpProxyConfig,
    use_php: bool,
    php: PhpTunnelConfig,
    http_port_text: String,
    pub is_new: bool,
    pub test: TestState,
    pub error: Option<String>,
    port_text: String,
    group_text: String,
    preset: Option<CloudPreset>,
}

pub enum DialogAction {
    Test(ConnInfo),
    Save(ConnInfo),
    Cancel,
}

impl ConnDialog {
    pub fn new_connection() -> Self {
        Self::edit(ConnectionConfig::new(Engine::Postgres, ""), String::new(), String::new(), true)
    }

    pub fn edit(cfg: ConnectionConfig, password: String, tunnel_secret: String, is_new: bool) -> Self {
        let port_text = cfg.port.to_string();
        let group_text = cfg.group.clone().unwrap_or_default();
        let use_ssh = cfg.ssh.is_some();
        let ssh = cfg.ssh.clone().unwrap_or_default();
        let ssh_port_text = ssh.port.to_string();
        let use_http = cfg.http_proxy.is_some();
        let http = cfg.http_proxy.clone().unwrap_or_default();
        let http_port_text = http.port.to_string();
        let use_php = cfg.php_tunnel.is_some();
        let php = cfg.php_tunnel.clone().unwrap_or_default();
        Self {
            cfg,
            password,
            tunnel_secret,
            use_ssh,
            ssh,
            ssh_port_text,
            use_http,
            http,
            use_php,
            php,
            http_port_text,
            is_new,
            test: TestState::Idle,
            error: None,
            port_text,
            group_text,
            preset: None,
        }
    }

    fn collect(&mut self) -> Result<ConnInfo, String> {
        let mut cfg = self.cfg.clone();
        // Snowflake connects directly over HTTPS (its tunnel UI is hidden); the dialog keeps the
        // settings so switching back to another engine restores them.
        let tunnels = cfg.engine.capabilities().network && cfg.engine != Engine::Snowflake;
        cfg.ssh = if self.use_ssh && tunnels {
            let mut ssh = self.ssh.clone();
            ssh.port = self
                .ssh_port_text
                .trim()
                .parse::<u16>()
                .map_err(|_| "SSH port must be between 1 and 65535".to_string())?;
            ssh.host = ssh.host.trim().to_string();
            ssh.user = ssh.user.trim().to_string();
            Some(ssh)
        } else {
            None
        };
        cfg.http_proxy = if self.use_http && tunnels {
            let mut http = self.http.clone();
            http.port = self
                .http_port_text
                .trim()
                .parse::<u16>()
                .map_err(|_| "HTTP proxy port must be between 1 and 65535".to_string())?;
            http.host = http.host.trim().to_string();
            http.user = http.user.trim().to_string();
            Some(http)
        } else {
            None
        };
        cfg.php_tunnel = (self.use_php && tunnels)
            .then(|| PhpTunnelConfig { url: self.php.url.trim().to_string(), save_secret: self.php.save_secret });
        if cfg.engine.capabilities().network {
            cfg.port =
                self.port_text.trim().parse::<u16>().map_err(|_| "Port must be between 1 and 65535".to_string())?;
        }
        cfg.group = Some(self.group_text.trim().to_string()).filter(|g| !g.is_empty());
        cfg.name = cfg.name.trim().to_string();
        cfg.host = cfg.host.trim().to_string();
        if let Some(sc) = cfg.snowflake.as_mut() {
            sc.warehouse = sc.warehouse.trim().to_string();
            sc.role = sc.role.trim().to_string();
            sc.schema = sc.schema.trim().to_string();
            sc.endpoint = sc.endpoint.trim().to_string();
            if let SnowflakeAuth::KeyPair { key_file } = &mut sc.auth {
                *key_file = key_file.trim().to_string();
            }
        }
        if cfg.name.is_empty() {
            cfg.name = match cfg.engine {
                Engine::Sqlite => std::path::Path::new(&cfg.database)
                    .file_stem()
                    .map_or_else(String::new, |s| s.to_string_lossy().into_owned()),
                _ => format!("{}@{}", cfg.user, cfg.host),
            };
        }
        cfg.validate()?;
        let password = cfg.needs_password().then(|| Secret::new(self.password.clone()));
        let tunnel_secret = cfg.has_tunnel().then(|| Secret::new(self.tunnel_secret.clone()));
        Ok(ConnInfo { cfg, password, tunnel_secret })
    }

    fn ssh_ui(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("SSH Tunnel").id_salt("conn-ssh").default_open(self.use_ssh).show(ui, |ui| {
            if ui.checkbox(&mut self.use_ssh, "Use SSH tunnel").changed() && self.use_ssh {
                self.use_http = false;
                self.use_php = false;
            }
            ui.add_enabled_ui(self.use_ssh, |ui| {
                egui::Grid::new("conn-ssh-grid").num_columns(2).spacing([12.0, 8.0]).min_col_width(130.0).show(
                    ui,
                    |ui| {
                        let l = ui.label("SSH Host");
                        ui.add(egui::TextEdit::singleline(&mut self.ssh.host).desired_width(300.0)).labelled_by(l.id);
                        ui.end_row();
                        let l = ui.label("SSH Port");
                        ui.add(egui::TextEdit::singleline(&mut self.ssh_port_text).desired_width(80.0))
                            .labelled_by(l.id);
                        ui.end_row();
                        let l = ui.label("SSH User Name");
                        ui.add(egui::TextEdit::singleline(&mut self.ssh.user).desired_width(300.0)).labelled_by(l.id);
                        ui.end_row();
                        ui.label("Authentication");
                        ui.horizontal(|ui| {
                            let is_pw = matches!(self.ssh.auth, SshAuth::Password);
                            if ui.radio(is_pw, "Password").clicked() {
                                self.ssh.auth = SshAuth::Password;
                            }
                            if ui.radio(!is_pw, "Private key").clicked() && is_pw {
                                self.ssh.auth = SshAuth::PrivateKey { path: String::new() };
                            }
                        });
                        ui.end_row();
                        if let SshAuth::PrivateKey { path } = &mut self.ssh.auth {
                            let l = ui.label("Private Key File");
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::TextEdit::singleline(path)
                                        .desired_width(220.0)
                                        .hint_text("~/.ssh/id_ed25519"),
                                )
                                .labelled_by(l.id);
                                if ui.button("Browse…").clicked()
                                    && let Some(f) = rfd::FileDialog::new().pick_file()
                                {
                                    *path = f.to_string_lossy().into_owned();
                                }
                            });
                            ui.end_row();
                        }
                        let label =
                            if matches!(self.ssh.auth, SshAuth::Password) { "SSH Password" } else { "Key Passphrase" };
                        let l = ui.label(label);
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.tunnel_secret).password(true).desired_width(200.0),
                            )
                            .labelled_by(l.id);
                            ui.checkbox(&mut self.ssh.save_secret, "Save in Keychain");
                        });
                        ui.end_row();
                        ui.label("");
                        ui.checkbox(&mut self.ssh.trust_new_host_key, "Trust host key on first connect").on_hover_text(
                            "Unknown host keys are rejected unless this is checked; changed keys are always rejected.",
                        );
                        ui.end_row();
                    },
                );
                ui.label(RichText::new("Host and port above are resolved on the SSH server.").small());
            });
        });
    }

    /// HTTP tunnel: the connection goes through an HTTP proxy's `CONNECT` (corporate proxies, Squid, …).
    fn http_ui(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("HTTP Tunnel").id_salt("conn-http").default_open(self.use_http).show(ui, |ui| {
            if ui.checkbox(&mut self.use_http, "Use HTTP tunnel").changed() && self.use_http {
                self.use_ssh = false;
                self.use_php = false;
            }
            ui.add_enabled_ui(self.use_http, |ui| {
                egui::Grid::new("conn-http-grid").num_columns(2).spacing([12.0, 8.0]).min_col_width(130.0).show(
                    ui,
                    |ui| {
                        let l = ui.label("Proxy Host");
                        ui.add(egui::TextEdit::singleline(&mut self.http.host).desired_width(300.0)).labelled_by(l.id);
                        ui.end_row();
                        let l = ui.label("Proxy Port");
                        ui.add(egui::TextEdit::singleline(&mut self.http_port_text).desired_width(80.0))
                            .labelled_by(l.id);
                        ui.end_row();
                        let l = ui.label("Proxy User Name");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.http.user)
                                .hint_text("empty: no authentication")
                                .desired_width(300.0),
                        )
                        .labelled_by(l.id);
                        ui.end_row();
                        let l = ui.label("Proxy Password");
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.tunnel_secret).password(true).desired_width(200.0),
                            )
                            .labelled_by(l.id);
                            ui.checkbox(&mut self.http.save_secret, "Save in Keychain");
                        });
                        ui.end_row();
                    },
                );
                ui.label(RichText::new("Host and port above are resolved by the proxy (HTTP CONNECT).").small());
            });
        });
    }

    /// PHP tunnel: a script on a web host that can reach the database (shared hosting, no SSH).
    fn php_ui(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("PHP Tunnel").id_salt("conn-php").default_open(self.use_php).show(ui, |ui| {
            if ui.checkbox(&mut self.use_php, "Use PHP tunnel script").changed() && self.use_php {
                self.use_ssh = false;
                self.use_http = false;
            }
            ui.add_enabled_ui(self.use_php, |ui| {
                egui::Grid::new("conn-php-grid").num_columns(2).spacing([12.0, 8.0]).min_col_width(130.0).show(
                    ui,
                    |ui| {
                        let l = ui.label("Script Address");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.php.url)
                                .hint_text("https://example.com/talus-tunnel.php")
                                .desired_width(300.0),
                        )
                        .labelled_by(l.id);
                        ui.end_row();
                        let l = ui.label("Tunnel Key");
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.tunnel_secret).password(true).desired_width(200.0),
                            )
                            .labelled_by(l.id);
                            ui.checkbox(&mut self.php.save_secret, "Save in Keychain");
                        });
                        ui.end_row();
                    },
                );
                ui.label(
                    RichText::new(
                        "Host and port above are resolved by the web host and must be on the script's allow list. \
                         The database password does not pass through the script.",
                    )
                    .small(),
                );
            });
        });
    }

    fn set_engine(&mut self, e: Engine) {
        if self.cfg.engine == e {
            return;
        }
        let keep_name = self.cfg.name.clone();
        let id = self.cfg.id;
        self.cfg = ConnectionConfig { id, name: keep_name, ..ConnectionConfig::new(e, "") };
        self.port_text = self.cfg.port.to_string();
        self.test = TestState::Idle;
        self.error = None;
        self.preset = None;
    }

    fn set_preset(&mut self, p: CloudPreset) {
        self.cfg = p.apply(&self.cfg);
        self.port_text = self.cfg.port.to_string();
        self.test = TestState::Idle;
        self.error = None;
        self.preset = Some(p);
    }

    pub fn ui(&mut self, ctx: &egui::Context) -> Option<DialogAction> {
        let p = theme::current(ctx);
        let mut action = None;
        let mut open = true;
        let title = if self.is_new { "New Connection" } else { "Edit Connection" };
        egui::Window::new(title)
            .id(egui::Id::new("conn-dialog"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(540.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(RichText::new("Connection type").color(p.text_dim));
                ui.horizontal_wrapped(|ui| {
                    for e in Engine::ALL {
                        let selected = self.cfg.engine == e;
                        let text = RichText::new(format!("{}  {}", theme::engine_icon(e), e.label()))
                            .color(if selected { p.accent } else { p.text_dim });
                        let r = ui.add_enabled(
                            self.is_new || selected,
                            egui::Button::selectable(selected, text).min_size(egui::vec2(132.0, 34.0)),
                        );
                        if r.clicked() {
                            self.set_engine(e);
                        }
                    }
                });
                if self.is_new {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Cloud preset").color(p.text_dim));
                        let current = self.preset.map_or_else(|| "None".to_string(), |p| p.label());
                        let mut picked = None;
                        super::widgets::combo("conn-preset").selected_text(current).width(300.0).show_ui(ui, |ui| {
                            for preset in CLOUD_PRESETS {
                                if ui.selectable_label(self.preset == Some(*preset), preset.label()).clicked() {
                                    picked = Some(*preset);
                                }
                            }
                        });
                        if let Some(pr) = picked {
                            self.set_preset(pr);
                        }
                    });
                }
                ui.add_space(6.0);
                egui::Grid::new("conn-form").num_columns(2).spacing([12.0, 8.0]).min_col_width(130.0).show(ui, |ui| {
                    let l = ui.label("Connection Name");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.cfg.name)
                            .hint_text("e.g. Production analytics")
                            .desired_width(300.0),
                    )
                    .labelled_by(l.id);
                    ui.end_row();
                    if self.cfg.engine.capabilities().network {
                        let snowflake = self.cfg.engine == Engine::Snowflake;
                        let l = ui.label(if snowflake { "Account" } else { "Host" });
                        let hint =
                            if snowflake { "e.g. xy12345.eu-west-1" } else { self.preset.map_or("localhost", |p| {
                                p.host_hint
                            }) };
                        ui.add(egui::TextEdit::singleline(&mut self.cfg.host).hint_text(hint).desired_width(300.0))
                            .labelled_by(l.id);
                        ui.end_row();
                        if !snowflake {
                            let l = ui.label("Port");
                            ui.add(egui::TextEdit::singleline(&mut self.port_text).desired_width(80.0))
                                .labelled_by(l.id);
                            ui.end_row();
                        }
                        let l = ui.label("User Name");
                        ui.add(egui::TextEdit::singleline(&mut self.cfg.user).desired_width(300.0)).labelled_by(l.id);
                        ui.end_row();
                        if snowflake {
                            let key_file = match &self.cfg.snowflake_config().auth {
                                SnowflakeAuth::KeyPair { key_file } => key_file.clone(),
                                _ => String::new(),
                            };
                            let mut method = match self.cfg.snowflake_config().auth {
                                SnowflakeAuth::Password => 0,
                                SnowflakeAuth::KeyPair { .. } => 1,
                                SnowflakeAuth::OAuth => 2,
                            };
                            ui.label("Authentication");
                            ui.horizontal(|ui| {
                                ui.radio_value(&mut method, 0, "Password");
                                ui.radio_value(&mut method, 1, "Key pair");
                                ui.radio_value(&mut method, 2, "OAuth token");
                            });
                            ui.end_row();
                            self.cfg.snowflake.get_or_insert_default().auth = match method {
                                1 => SnowflakeAuth::KeyPair { key_file },
                                2 => SnowflakeAuth::OAuth,
                                _ => SnowflakeAuth::Password,
                            };
                        }
                        let sf_auth = snowflake.then(|| self.cfg.snowflake_config().auth);
                        if matches!(sf_auth, Some(SnowflakeAuth::KeyPair { .. })) {
                            let l = ui.label("Private Key File");
                            if let SnowflakeAuth::KeyPair { key_file } =
                                &mut self.cfg.snowflake.get_or_insert_default().auth
                            {
                                ui.add(
                                    egui::TextEdit::singleline(key_file)
                                        .hint_text("/path/to/rsa_key.p8 (unencrypted PEM)")
                                        .desired_width(300.0),
                                )
                                .labelled_by(l.id);
                            }
                            ui.end_row();
                        } else {
                            let l = ui.label(if matches!(sf_auth, Some(SnowflakeAuth::OAuth)) {
                                "OAuth Token"
                            } else {
                                "Password"
                            });
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.password).password(true).desired_width(200.0),
                                )
                                .labelled_by(l.id);
                                ui.checkbox(&mut self.cfg.save_password, "Save in Keychain");
                            });
                            ui.end_row();
                        }
                        let label = match self.cfg.engine {
                            Engine::Redis => "Database Index",
                            Engine::MongoDb => "Auth Database",
                            Engine::Oracle => "Service Name",
                            _ => "Initial Database",
                        };
                        let l = ui.label(label);
                        let hint = match self.cfg.engine {
                            Engine::Oracle => "e.g. FREEPDB1 or ORCLPDB1",
                            Engine::Snowflake => "e.g. ANALYTICS",
                            e => e.default_database().unwrap_or("(none)"),
                        };
                        ui.add(egui::TextEdit::singleline(&mut self.cfg.database).hint_text(hint).desired_width(300.0))
                            .labelled_by(l.id);
                        ui.end_row();
                        if snowflake {
                            // Warehouse/role/schema fall back to the user's server-side defaults when empty.
                            for (label, hint) in [
                                ("Warehouse", "e.g. COMPUTE_WH — every query bills on it"),
                                ("Role", "e.g. SYSADMIN"),
                                ("Schema", "PUBLIC"),
                                ("Custom Endpoint", "PrivateLink URL (empty = default)"),
                            ] {
                                let l = ui.label(label);
                                let field = match label {
                                    "Warehouse" => &mut self.cfg.snowflake.get_or_insert_default().warehouse,
                                    "Role" => &mut self.cfg.snowflake.get_or_insert_default().role,
                                    "Schema" => &mut self.cfg.snowflake.get_or_insert_default().schema,
                                    _ => &mut self.cfg.snowflake.get_or_insert_default().endpoint,
                                };
                                ui.add(egui::TextEdit::singleline(field).hint_text(hint).desired_width(300.0))
                                    .labelled_by(l.id);
                                ui.end_row();
                            }
                            ui.label("TLS");
                            ui.label(
                                RichText::new("Always on: Snowflake speaks verified HTTPS.").color(p.text_dim).small(),
                            );
                            ui.end_row();
                        } else {
                            ui.label("SSL");
                            ui.horizontal(|ui| {
                                for m in SslMode::ALL {
                                    ui.radio_value(&mut self.cfg.ssl_mode, m, m.label());
                                }
                            });
                            ui.end_row();
                            if self.cfg.engine == Engine::Oracle && self.cfg.ssl_mode != SslMode::Disable {
                                // `prefer` cannot be negotiated on Oracle's listener: only require / verify-full use TCPS.
                                let l = ui.label("Wallet Directory");
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.cfg.ssl_ca_file)
                                        .hint_text("folder with ewallet.pem (empty = system roots); TCPS needs require / verify-full")
                                        .desired_width(300.0),
                                )
                                .labelled_by(l.id);
                                ui.end_row();
                            } else if self.cfg.ssl_mode == SslMode::VerifyFull {
                                let l = ui.label("CA Certificate");
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.cfg.ssl_ca_file)
                                        .hint_text("PEM file (empty = system roots)")
                                        .desired_width(300.0),
                                )
                                .labelled_by(l.id);
                                ui.end_row();
                            }
                        }
                    } else {
                        let l = ui.label("Database File");
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.cfg.database)
                                    .hint_text("/path/to/database.sqlite")
                                    .desired_width(250.0),
                            )
                            .labelled_by(l.id);
                            if ui.button("Browse…").clicked()
                                && let Some(f) = rfd::FileDialog::new()
                                    .add_filter("SQLite database", &["sqlite", "sqlite3", "db", "db3"])
                                    .add_filter("All files", &["*"])
                                    .pick_file()
                            {
                                self.cfg.database = f.to_string_lossy().into_owned();
                            }
                            if ui.button("New File…").on_hover_text("Create an empty SQLite database").clicked() {
                                if self.cfg.encrypted && self.password.is_empty() {
                                    self.error = Some("Enter the password to encrypt the new file with.".into());
                                } else if let Some(f) =
                                    rfd::FileDialog::new().set_file_name("database.sqlite").save_file()
                                {
                                    let key = self.cfg.encrypted.then(|| Secret::new(self.password.clone()));
                                    match talus_core::engine::create_database_file_with_key(&f, key.as_ref()) {
                                        Ok(()) => self.cfg.database = f.to_string_lossy().into_owned(),
                                        Err(e) => self.error = Some(e.to_string()),
                                    }
                                }
                            }
                        });
                        ui.end_row();
                        ui.label("Encryption");
                        ui.checkbox(&mut self.cfg.encrypted, "Encrypted file (SQLCipher)");
                        ui.end_row();
                        if self.cfg.encrypted {
                            let l = ui.label("Password");
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.password).password(true).desired_width(200.0),
                                )
                                .labelled_by(l.id);
                                ui.checkbox(&mut self.cfg.save_password, "Save in Keychain");
                            });
                            ui.end_row();
                        }
                    }
                    let l = ui.label("Group");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.group_text)
                            .hint_text("optional folder in the navigator")
                            .desired_width(300.0),
                    )
                    .labelled_by(l.id);
                    ui.end_row();
                });
                if self.cfg.engine.capabilities().network && self.cfg.engine != Engine::Snowflake {
                    self.ssh_ui(ui);
                    self.http_ui(ui);
                    self.php_ui(ui);
                }
                egui::CollapsingHeader::new("Advanced").id_salt("conn-adv").show(ui, |ui| {
                    egui::Grid::new("conn-adv-grid").num_columns(2).spacing([12.0, 8.0]).min_col_width(130.0).show(
                        ui,
                        |ui| {
                            let l = ui.label("Connect timeout (s)");
                            ui.add(egui::DragValue::new(&mut self.cfg.connect_timeout_secs).range(1..=300))
                                .labelled_by(l.id);
                            ui.end_row();
                            let l = ui.label("Initial SQL");
                            ui.add(
                                egui::TextEdit::multiline(&mut self.cfg.init_sql)
                                    .code_editor()
                                    .desired_rows(3)
                                    .desired_width(300.0)
                                    .hint_text("Executed after every connect"),
                            )
                            .labelled_by(l.id);
                            ui.end_row();
                        },
                    );
                });
                ui.add_space(6.0);
                match &self.test {
                    TestState::Idle => {}
                    TestState::Running(_) => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Testing connection…");
                        });
                    }
                    TestState::Ok(v) => {
                        ui.label(
                            RichText::new(format!("{} Connection successful — {v}", egui_phosphor::regular::CHECK))
                                .color(p.ok),
                        );
                    }
                    TestState::Failed(e) => {
                        ui.label(
                            RichText::new(format!("{} {e}", egui_phosphor::regular::WARNING_CIRCLE)).color(p.danger),
                        );
                    }
                }
                if let Some(e) = &self.error {
                    ui.label(RichText::new(e).color(p.danger));
                }
                ui.separator();
                ui.horizontal(|ui| {
                    let testing = matches!(self.test, TestState::Running(_));
                    if ui.add_enabled(!testing, egui::Button::new("Test Connection")).clicked() {
                        match self.collect() {
                            Ok(info) => {
                                self.error = None;
                                action = Some(DialogAction::Test(info));
                            }
                            Err(e) => self.error = Some(e),
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add(theme::primary_button(p, "Save")).clicked() {
                            match self.collect() {
                                Ok(info) => action = Some(DialogAction::Save(info)),
                                Err(e) => self.error = Some(e),
                            }
                        }
                        if ui.button("Cancel").clicked() {
                            action = Some(DialogAction::Cancel);
                        }
                    });
                });
            });
        if !open {
            action = Some(DialogAction::Cancel);
        }
        action
    }
}
