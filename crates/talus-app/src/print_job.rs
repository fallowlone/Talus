//! Sending a PDF to a printer through CUPS (`lp`, `lpstat`, `cancel`, present on every macOS). The commands
//! can be replaced by environment variables (`TALUS_LP`, `TALUS_LPSTAT`, `TALUS_CANCEL`) so tests never
//! reach a real printer.

use std::path::Path;
use std::process::Command;

use talus_core::print::Paper;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Printer {
    pub name: String,
    pub is_default: bool,
}

/// A job CUPS accepted, e.g. `Brother_MFC-12`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobId(pub String);

fn tool(var: &str, default: &str) -> String {
    std::env::var(var).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| default.to_string())
}

/// Printer names are passed to `lp -d` as one argument; refuse anything that could be read as an option.
fn valid_destination(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':' | '@' | '/' | '+') || c as u32 > 127
        })
}

/// The queues that accept jobs, the system default marked.
pub fn printers() -> Result<Vec<Printer>, String> {
    let list = Command::new(tool("TALUS_LPSTAT", "lpstat"))
        .arg("-e")
        .output()
        .map_err(|e| format!("lpstat is not available: {e}"))?;
    if !list.status.success() {
        return Err(String::from_utf8_lossy(&list.stderr).trim().to_string());
    }
    let default = Command::new(tool("TALUS_LPSTAT", "lpstat"))
        .arg("-d")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .and_then(|t| t.split(':').nth(1).map(|d| d.trim().to_string()))
        .unwrap_or_default();
    Ok(String::from_utf8_lossy(&list.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|name| Printer { name: name.to_string(), is_default: name == default })
        .collect())
}

/// `request id is Brother-12 (1 file(s))` → `Brother-12`.
fn parse_job(out: &str) -> Option<JobId> {
    let rest = out.split("request id is ").nth(1)?;
    Some(JobId(rest.split_whitespace().next()?.to_string()))
}

/// Queues `pdf` on `printer` (`copies` of it, on `paper`). The error is what `lp` said.
pub fn submit(pdf: &Path, printer: &str, copies: u32, paper: Paper, title: &str) -> Result<JobId, String> {
    if !valid_destination(printer) {
        return Err(format!("{printer:?} is not a valid printer name"));
    }
    let out = Command::new(tool("TALUS_LP", "lp"))
        .args(["-d", printer, "-n", &copies.clamp(1, 99).to_string(), "-t"])
        .arg(title.chars().take(100).collect::<String>())
        .args(["-o", &format!("media={}", paper.cups_media())])
        .arg("--")
        .arg(pdf)
        .output()
        .map_err(|e| format!("lp is not available: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if err.is_empty() { format!("lp failed ({})", out.status) } else { err });
    }
    parse_job(&text).ok_or_else(|| format!("lp accepted the job but did not name it: {}", text.trim()))
}

/// Whether the job is still queued or printing.
pub fn is_pending(job: &JobId) -> bool {
    Command::new(tool("TALUS_LPSTAT", "lpstat"))
        .args(["-W", "not-completed", "-o"])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout).lines().any(|l| l.split_whitespace().next() == Some(job.0.as_str()))
        })
        .unwrap_or(false)
}

/// Removes a queued job (`cancel`).
pub fn cancel(job: &JobId) -> Result<(), String> {
    let out = Command::new(tool("TALUS_CANCEL", "cancel"))
        .arg("--")
        .arg(&job.0)
        .output()
        .map_err(|e| format!("cancel is not available: {e}"))?;
    if out.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&out.stderr).trim().to_string()) }
}

/// Shows the file in the system's PDF viewer (Preview), whose own print dialog offers every native option.
pub fn open_in_viewer(pdf: &Path) -> Result<(), String> {
    let out =
        Command::new("open").arg("--").arg(pdf).output().map_err(|e| format!("cannot open the PDF viewer: {e}"))?;
    if out.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&out.stderr).trim().to_string()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn job_ids_are_read_from_lp_output_and_names_are_checked() {
        assert_eq!(parse_job("request id is Brother_MFC-12 (1 file(s))\n"), Some(JobId("Brother_MFC-12".into())));
        assert_eq!(parse_job("something else"), None);
        assert!(
            valid_destination("Brother_MFC_L2860DW") && valid_destination("HP LaserJet".replace(' ', "_").as_str())
        );
        for bad in ["", "-d", "a b", "a;b", "$(x)", "a\nb"] {
            assert!(!valid_destination(bad), "{bad:?}");
        }
    }

    /// A fake CUPS: records its arguments in a file and answers like the real tools.
    fn fake(dir: &Path, name: &str, body: &str) -> String {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().into_owned()
    }

    // The commands come from the environment: this test owns those variables for its duration.
    #[test]
    fn submit_list_poll_and_cancel_drive_the_cups_commands() {
        let dir = std::env::temp_dir().join(format!("talus-fake-cups-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("log");
        let lp =
            fake(&dir, "lp", &format!("echo \"$@\" >> {0}; echo 'request id is Fake-7 (1 file(s))'", log.display()));
        let cancel_cmd = fake(&dir, "cancel", &format!("echo \"cancel $@\" >> {0}", log.display()));
        let lpstat = fake(
            &dir,
            "lpstat",
            "case \"$1\" in -e) printf 'Office\\nHome\\n';; -d) echo 'system default destination: Home';; -W) echo 'Fake-7 me 1024 Mon';; esac",
        );
        // SAFETY-free: plain process environment, read by `tool` at each call.
        unsafe {
            std::env::set_var("TALUS_LP", &lp);
            std::env::set_var("TALUS_CANCEL", &cancel_cmd);
            std::env::set_var("TALUS_LPSTAT", &lpstat);
        }
        let ps = printers().unwrap();
        assert_eq!(
            ps,
            vec![
                Printer { name: "Office".into(), is_default: false },
                Printer { name: "Home".into(), is_default: true }
            ]
        );
        let job = submit(Path::new("/tmp/x.pdf"), "Home", 2, Paper::A4, "Shop model").unwrap();
        assert_eq!(job, JobId("Fake-7".into()));
        let logged = std::fs::read_to_string(&log).unwrap();
        assert!(logged.contains("-d Home -n 2 -t Shop model -o media=A4 -- /tmp/x.pdf"), "{logged}");
        assert!(is_pending(&job));
        cancel(&job).unwrap();
        assert!(std::fs::read_to_string(&log).unwrap().contains("cancel -- Fake-7"));
        // A refusing printer reports what lp said.
        let failing = fake(&dir, "lp-fail", "echo 'lp: The printer or class does not exist.' >&2; exit 1");
        unsafe { std::env::set_var("TALUS_LP", &failing) };
        assert_eq!(
            submit(Path::new("/tmp/x.pdf"), "Nowhere", 1, Paper::A4, "t").unwrap_err(),
            "lp: The printer or class does not exist."
        );
        assert!(submit(Path::new("/tmp/x.pdf"), "-evil", 1, Paper::A4, "t").is_err(), "option-like names are refused");
        unsafe {
            std::env::remove_var("TALUS_LP");
            std::env::remove_var("TALUS_CANCEL");
            std::env::remove_var("TALUS_LPSTAT");
        }
    }
}
