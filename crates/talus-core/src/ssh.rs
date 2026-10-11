//! SSH tunnel (local port forward) for network engines. Host keys are verified against the
//! application's own known_hosts file: unknown keys are rejected unless the connection opts in
//! to trust-on-first-use; a changed key is always rejected.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::client;
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKey, load_secret_key};
use tokio::net::TcpListener;

use crate::error::{DbError, DbResult};
use crate::model::{SshAuth, SshConfig};
use crate::secret::Secret;
use crate::tunnel::Tunnel;

struct HostCheck {
    host: String,
    port: u16,
    known_hosts: PathBuf,
    trust_new: bool,
    problem: Arc<Mutex<Option<String>>>,
}

impl HostCheck {
    fn reject(&self, why: String) -> bool {
        if let Ok(mut p) = self.problem.lock() {
            *p = Some(why);
        }
        false
    }

    fn verify(&self, key: &PublicKey) -> bool {
        let fp = key.fingerprint(HashAlg::Sha256);
        match russh::keys::check_known_hosts_path(&self.host, self.port, key, &self.known_hosts) {
            Ok(true) => true,
            Ok(false) if self.trust_new => match russh::keys::known_hosts::learn_known_hosts_path(&self.host, self.port, key, &self.known_hosts) {
                Ok(()) => true,
                Err(e) => self.reject(format!("could not record host key {fp}: {e}")),
            },
            Ok(false) => self.reject(format!(
                "unknown SSH host key for {}:{} ({fp}). Enable \"Trust host key on first connect\" if you trust this server.",
                self.host, self.port
            )),
            Err(e) => self.reject(format!("SSH HOST KEY CHANGED for {}:{} ({fp}) — possible man-in-the-middle attack. Connection refused. ({e})", self.host, self.port)),
        }
    }
}

impl client::Handler for HostCheck {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &russh::keys::PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        Ok(match key {
            russh::keys::PublicKeyOrCertificate::PublicKey { key, .. } => self.verify(key),
            russh::keys::PublicKeyOrCertificate::Certificate(_) => {
                self.reject("SSH host certificates are not supported; the server must present a plain host key".into())
            }
        })
    }
}

/// Opens an SSH session and forwards `127.0.0.1:<local_port>` to `target_host:target_port`
/// as seen from the SSH server.
pub async fn open_tunnel(
    cfg: &SshConfig,
    secret: Option<&Secret>,
    target_host: &str,
    target_port: u16,
    known_hosts: &Path,
    timeout: Duration,
) -> DbResult<Tunnel> {
    let problem = Arc::new(Mutex::new(None));
    let handler = HostCheck {
        host: cfg.host.clone(),
        port: cfg.port,
        known_hosts: known_hosts.to_path_buf(),
        trust_new: cfg.trust_new_host_key,
        problem: problem.clone(),
    };
    let config = Arc::new(client::Config {
        inactivity_timeout: None,
        keepalive_interval: Some(Duration::from_secs(30)),
        ..Default::default()
    });
    let connect = client::connect(config, (cfg.host.as_str(), cfg.port), handler);
    let mut handle = match tokio::time::timeout(timeout, connect).await {
        Err(_) => return Err(DbError::Connect(format!("SSH connect to {}:{} timed out", cfg.host, cfg.port))),
        Ok(Err(e)) => {
            let why = problem.lock().ok().and_then(|p| p.clone());
            return Err(DbError::Connect(why.unwrap_or_else(|| format!("SSH connection failed: {e}"))));
        }
        Ok(Ok(h)) => h,
    };
    let auth = match &cfg.auth {
        SshAuth::Password => {
            let pw = secret.map(|s| s.expose().to_string()).unwrap_or_default();
            handle.authenticate_password(cfg.user.clone(), pw).await
        }
        SshAuth::PrivateKey { path } => {
            let passphrase = secret.filter(|s| !s.is_empty()).map(|s| s.expose().to_string());
            let key = load_secret_key(path, passphrase.as_deref())
                .map_err(|e| DbError::Connect(format!("cannot load SSH key {path}: {e}")))?;
            let hash = handle.best_supported_rsa_hash().await.map_err(|e| DbError::Connect(e.to_string()))?.flatten();
            handle.authenticate_publickey(cfg.user.clone(), PrivateKeyWithHashAlg::new(Arc::new(key), hash)).await
        }
    }
    .map_err(|e| DbError::Connect(format!("SSH authentication error: {e}")))?;
    if !auth.success() {
        return Err(DbError::Connect(format!("SSH authentication failed for {}@{}", cfg.user, cfg.host)));
    }
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let local_port = listener.local_addr()?.port();
    let handle = Arc::new(handle);
    let (target_host, target_port) = (target_host.to_string(), u32::from(target_port));
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut sock, peer)) = listener.accept().await else { break };
            let (h, th) = (handle.clone(), target_host.clone());
            tokio::spawn(async move {
                match h.channel_open_direct_tcpip(th, target_port, "127.0.0.1", u32::from(peer.port())).await {
                    Ok(ch) => {
                        let mut stream = ch.into_stream();
                        if let Err(e) = tokio::io::copy_bidirectional(&mut sock, &mut stream).await {
                            tracing::debug!(error = %e, "tunnel stream ended");
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "SSH direct-tcpip channel failed"),
                }
            });
        }
    });
    Ok(Tunnel::new(local_port, task))
}
