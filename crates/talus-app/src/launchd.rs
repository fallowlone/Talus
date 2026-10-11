//! Installing a job as a launchd agent, so it runs while Talus is closed. Only ever called from
//! an explicit, confirmed click.

use std::path::PathBuf;
use std::process::Command;

use talus_core::jobs::{Job, launchd_label, launchd_plist};

fn agents_dir() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(PathBuf::from(home).join("Library/LaunchAgents"))
}

pub fn plist_path(job: &Job) -> Result<PathBuf, String> {
    Ok(agents_dir()?.join(format!("{}.plist", launchd_label(job))))
}

pub fn is_installed(job: &Job) -> bool {
    plist_path(job).is_ok_and(|p| p.exists())
}

fn uid() -> Result<String, String> {
    let out = Command::new("id").arg("-u").output().map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn launchctl(args: &[&str]) -> Result<(), String> {
    let out = Command::new("launchctl").args(args).output().map_err(|e| format!("cannot run launchctl: {e}"))?;
    if out.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&out.stderr).trim().to_string()) }
}

/// Writes the agent's plist and loads it. Returns the plist path.
pub fn install(job: &Job) -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let plist = launchd_plist(job, &exe.to_string_lossy()).ok_or("a manual job has no schedule to install")?;
    let path = plist_path(job)?;
    std::fs::create_dir_all(path.parent().ok_or("no LaunchAgents directory")?).map_err(|e| e.to_string())?;
    std::fs::write(&path, plist).map_err(|e| e.to_string())?;
    // Re-installing replaces a loaded agent.
    let domain = format!("gui/{}", uid()?);
    let _ = launchctl(&["bootout", &format!("{domain}/{}", launchd_label(job))]);
    launchctl(&["bootstrap", &domain, &path.to_string_lossy()])?;
    Ok(path)
}

pub fn remove(job: &Job) -> Result<(), String> {
    let domain = format!("gui/{}", uid()?);
    let _ = launchctl(&["bootout", &format!("{domain}/{}", launchd_label(job))]);
    let path = plist_path(job)?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}
