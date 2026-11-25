//! Log size limits. At the limit a log is either cleared (default) or rolled over to
//! `<name>.log.1`, replacing any earlier one.
//!
//! The daemon log is launchd's stderr redirect, so the daemon takes over fd 2 (rename or unlink,
//! reopen, dup2). Child stderr fds belong to the children, so their files are copied (rollover
//! only) and truncated in place; they're opened O_APPEND, so writes land at the new end.

use crate::config::{Config, parse_size};
use std::{
    fs::{File, OpenOptions},
    os::unix::{fs::MetadataExt, io::AsRawFd},
    path::Path,
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Policy {
    pub max_bytes: u64,
    pub rollover: bool,
}

impl Policy {
    pub fn new(cfg: &Config) -> anyhow::Result<Self> {
        Ok(Policy {
            max_bytes: parse_size(&cfg.log_max_size)?,
            rollover: cfg.log_rollover,
        })
    }

    fn over(&self, path: &Path) -> bool {
        std::fs::metadata(path).is_ok_and(|m| m.len() > self.max_bytes)
    }
}

/// Whether fd 2 is `path` (i.e. launchd redirected stderr there). Foreground runs aren't.
pub fn stderr_is(path: &Path) -> bool {
    let Ok(m) = std::fs::metadata(path) else {
        return false;
    };
    // SAFETY: fstat on fd 2 into a zeroed stat we own.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(2, &mut st) } != 0 {
        return false;
    }
    st.st_dev as u64 == m.dev() && st.st_ino as u64 == m.ino()
}

/// A log nobody holds open (a server's, just before it spawns): roll over or delete.
pub fn limit_closed(path: &Path, p: Policy) -> std::io::Result<bool> {
    if !p.over(path) {
        return Ok(false);
    }
    if p.rollover {
        std::fs::rename(path, path.with_extension("log.1"))?;
    } else {
        std::fs::remove_file(path)?;
    }
    Ok(true)
}

/// The daemon log, then point fd 2 at a fresh file.
pub fn limit_stderr(path: &Path, p: Policy) -> std::io::Result<bool> {
    if !limit_closed(path, p)? {
        return Ok(false);
    }
    let f = OpenOptions::new().create(true).append(true).open(path)?;
    // SAFETY: dup2 onto stderr; `f` stays valid for the call and fd 2 keeps its own reference.
    if unsafe { libc::dup2(f.as_raw_fd(), 2) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(true)
}

/// A log a running child holds open (O_APPEND): copy to `.1` if rolling over, then truncate.
/// Lines written between the copy and the truncate are lost.
pub fn limit_open(path: &Path, p: Policy) -> std::io::Result<bool> {
    if !p.over(path) {
        return Ok(false);
    }
    if p.rollover {
        std::fs::copy(path, path.with_extension("log.1"))?;
    }
    File::options().write(true).open(path)?.set_len(0)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default deletes old lines; rollover keeps exactly one `.log.1`.
    #[test]
    fn delete_or_roll_over() {
        let dir = std::env::temp_dir().join(format!("mcpmux-logs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.log");
        let one = dir.join("s.log.1");
        let delete = Policy {
            max_bytes: 4,
            rollover: false,
        };
        let roll = Policy {
            max_bytes: 4,
            rollover: true,
        };

        std::fs::write(&p, "1234").unwrap();
        assert!(
            !limit_open(&p, delete).unwrap(),
            "at the limit is not over it"
        );
        std::fs::write(&p, "12345").unwrap();
        assert!(limit_open(&p, delete).unwrap());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "");
        assert!(!one.exists());

        std::fs::write(&p, "12345").unwrap();
        assert!(limit_open(&p, roll).unwrap());
        assert_eq!(std::fs::read_to_string(&one).unwrap(), "12345");

        std::fs::write(&p, "abcdef").unwrap();
        assert!(limit_closed(&p, delete).unwrap());
        assert!(!p.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
