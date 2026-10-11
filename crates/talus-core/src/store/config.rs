//! JSON stores under the application support directory. Writes are atomic
//! (temp file + rename) and files are created with mode 0600.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::model::ConnectionConfig;

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub root: PathBuf,
}

impl AppPaths {
    /// `~/Library/Application Support/io.github.fallowlone.Talus` on macOS.
    pub fn system() -> io::Result<Self> {
        let dirs = directories::ProjectDirs::from("io.github", "fallowlone", "Talus")
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no home directory"))?;
        Self::at(dirs.data_dir())
    }

    pub fn at(root: impl AsRef<Path>) -> io::Result<Self> {
        fs::create_dir_all(root.as_ref())?;
        Ok(Self { root: root.as_ref().to_path_buf() })
    }

    pub fn connections_file(&self) -> PathBuf {
        self.root.join("connections.json")
    }
    pub fn settings_file(&self) -> PathBuf {
        self.root.join("settings.json")
    }
    pub fn session_file(&self) -> PathBuf {
        self.root.join("session.json")
    }
    pub fn history_file(&self) -> PathBuf {
        self.root.join("history.jsonl")
    }
    /// SSH host keys trusted by this application (not `~/.ssh/known_hosts`).
    pub fn known_hosts_file(&self) -> PathBuf {
        self.root.join("known_hosts")
    }
    pub fn jobs_file(&self) -> PathBuf {
        self.root.join("jobs.json")
    }
    pub fn jobs_log_file(&self) -> PathBuf {
        self.root.join("jobs.log")
    }
    pub fn queries_dir(&self) -> PathBuf {
        self.root.join("queries")
    }
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
}

/// Loads JSON; a missing file yields `T::default()`. A corrupt file is an error
/// (we never silently replace user data).
pub fn load_json<T: DeserializeOwned + Default>(path: &Path) -> io::Result<T> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", path.display()))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e),
    }
}

pub fn save_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    write_atomic(path, &bytes)
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ConnectionsFile {
    #[serde(default)]
    pub connections: Vec<ConnectionConfig>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Theme {
    Dark,
    #[default]
    Light,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: Theme,
    pub editor_font_size: f32,
    /// Rows per page in the table data editor.
    pub page_size: u64,
    /// Row cap per result set in the SQL editor.
    pub max_result_rows: usize,
    pub restore_session: bool,
    pub history_limit: usize,
    pub code_completion: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: Theme::Light,
            editor_font_size: 13.0,
            page_size: 1000,
            max_result_rows: 10_000,
            restore_session: true,
            history_limit: 5000,
            code_completion: true,
        }
    }
}

impl Settings {
    pub fn sanitized(mut self) -> Self {
        self.editor_font_size = self.editor_font_size.clamp(8.0, 32.0);
        self.page_size = self.page_size.clamp(10, 100_000);
        self.max_result_rows = self.max_result_rows.clamp(100, 5_000_000);
        self.history_limit = self.history_limit.clamp(100, 1_000_000);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Engine;

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("talus-test-{name}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn connections_roundtrip_atomic_and_private() {
        let paths = AppPaths::at(temp_dir("conn")).unwrap();
        let missing: ConnectionsFile = load_json(&paths.connections_file()).unwrap();
        assert!(missing.connections.is_empty());
        let file = ConnectionsFile { connections: vec![ConnectionConfig::new(Engine::Postgres, "a")] };
        save_json(&paths.connections_file(), &file).unwrap();
        let back: ConnectionsFile = load_json(&paths.connections_file()).unwrap();
        assert_eq!(back.connections, file.connections);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(paths.connections_file()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(!paths.connections_file().with_extension("tmp").exists());
    }

    #[test]
    fn corrupt_file_is_error_not_reset() {
        let paths = AppPaths::at(temp_dir("corrupt")).unwrap();
        fs::write(paths.settings_file(), b"{not json").unwrap();
        assert!(load_json::<Settings>(&paths.settings_file()).is_err());
    }

    #[test]
    fn settings_defaults_and_sanitize() {
        let s: Settings = serde_json::from_str("{\"page_size\": 3}").unwrap();
        assert_eq!(s.editor_font_size, 13.0);
        assert_eq!(s.sanitized().page_size, 10);
    }
}
