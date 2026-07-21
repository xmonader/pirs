//! Single-instance lock (Telegram getUpdates is exclusive per bot token).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Held lock; releasing the process or dropping unlocks (flock).
#[derive(Debug)]
pub struct InstanceLock {
    _file: File,
    path: PathBuf,
    /// Sidecar with pid= lines (readable while the lock fd is held open).
    meta_path: PathBuf,
}

impl InstanceLock {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.meta_path);
    }
}

/// Try to acquire an exclusive non-blocking lock under `state_dir/locks/{name}.lock`.
pub fn try_acquire(state_dir: &Path, name: &str) -> anyhow::Result<InstanceLock> {
    let dir = state_dir.join("locks");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{name}.lock"));
    let meta_path = dir.join(format!("{name}.meta"));
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(true)
        .open(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let fd = file.as_raw_fd();
        let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            anyhow::bail!(
                "another pirs-claw instance already holds the {name:?} lock ({})\n\
                 Telegram getUpdates allows only one long-poll per bot token.\n\
                 Stop the other process, or remove the stale lock if nothing is running.",
                path.display()
            );
        }
    }
    let pid = std::process::id();
    // Sidecar is always readable by status; the lock file itself may appear empty
    // to other readers while this fd holds an exclusive write handle.
    std::fs::write(&meta_path, format!("pid={pid}\nname={name}\n"))?;
    let _ = writeln!(file, "pid={pid}\nname={name}");
    let _ = file.flush();
    Ok(InstanceLock {
        _file: file,
        path,
        meta_path,
    })
}

/// Human-readable lock status for `status` (held / stale / absent). Never panics.
pub fn lock_status(state_dir: &Path, name: &str) -> String {
    let dir = state_dir.join("locks");
    let path = dir.join(format!("{name}.lock"));
    let meta_path = dir.join(format!("{name}.meta"));
    if !path.exists() && !meta_path.exists() {
        return "absent".into();
    }
    let content = std::fs::read_to_string(&meta_path)
        .or_else(|_| std::fs::read_to_string(&path))
        .unwrap_or_default();
    let pid = content
        .lines()
        .find_map(|l| l.strip_prefix("pid="))
        .and_then(|s| s.trim().parse::<u32>().ok());
    match pid {
        Some(pid) if process_alive(pid) => {
            format!("held by pid {pid} ({})", path.display())
        }
        Some(pid) => format!("stale (pid {pid} not running) ({})", path.display()),
        None if path.exists() => format!("present (pid unknown) ({})", path.display()),
        None => "absent".into(),
    }
}

fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // signal 0 = existence check
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true // best-effort: assume held if file exists
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_lock_fails() {
        let dir = tempfile::tempdir().unwrap();
        let a = try_acquire(dir.path(), "telegram").unwrap();
        let err = try_acquire(dir.path(), "telegram").unwrap_err().to_string();
        assert!(err.contains("already holds") || err.contains("lock"), "{err}");
        let status = lock_status(dir.path(), "telegram");
        assert!(status.contains("held"), "{status}");
        drop(a);
        // After drop, can re-acquire
        let _b = try_acquire(dir.path(), "telegram").unwrap();
    }
}
