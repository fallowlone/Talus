//! Query history log (JSON lines). Appends are O(1); the file is rotated once it
//! exceeds `MAX_BYTES` so it never grows without bound.

use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const MAX_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub ts_ms: u64,
    pub connection: String,
    #[serde(default)]
    pub database: String,
    pub sql: String,
    pub elapsed_ms: u64,
    pub ok: bool,
    #[serde(default)]
    pub message: String,
}

impl HistoryEntry {
    pub fn now_ms() -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
    }
}

#[derive(Debug, Clone)]
pub struct History {
    path: PathBuf,
}

impl History {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, entry: &HistoryEntry) -> io::Result<()> {
        if fs::metadata(&self.path).map(|m| m.len() > MAX_BYTES).unwrap_or(false) {
            fs::rename(&self.path, self.path.with_extension("jsonl.1"))?;
        }
        let mut opts = OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&self.path)?;
        let mut line = serde_json::to_vec(entry).map_err(io::Error::other)?;
        line.push(b'\n');
        f.write_all(&line)
    }

    /// Most recent `limit` entries, newest first. Corrupt lines are skipped.
    pub fn recent(&self, limit: usize) -> io::Result<Vec<HistoryEntry>> {
        let f = match fs::File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut all: Vec<HistoryEntry> =
            BufReader::new(f).lines().map_while(Result::ok).filter_map(|l| serde_json::from_str(&l).ok()).collect();
        all.reverse();
        all.truncate(limit);
        Ok(all)
    }

    pub fn clear(&self) -> io::Result<()> {
        match fs::remove_file(&self.path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_recent_clear() {
        let dir = std::env::temp_dir().join(format!("talus-hist-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let h = History::new(dir.join("h.jsonl"));
        assert!(h.recent(10).unwrap().is_empty());
        for i in 0..5 {
            h.append(&HistoryEntry {
                ts_ms: i,
                connection: "c".into(),
                database: String::new(),
                sql: format!("select {i}"),
                elapsed_ms: 1,
                ok: true,
                message: String::new(),
            })
            .unwrap();
        }
        fs::OpenOptions::new().append(true).open(h.path()).unwrap().write_all(b"garbage\n").unwrap();
        let r = h.recent(3).unwrap();
        assert_eq!(r.iter().map(|e| e.sql.as_str()).collect::<Vec<_>>(), vec!["select 4", "select 3", "select 2"]);
        h.clear().unwrap();
        assert!(h.recent(10).unwrap().is_empty());
    }
}
