//! Where session sockets live and how sessions are named.

use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

pub const DEFAULT_SESSION: &str = "main";

/// The per-user runtime directory holding session sockets.
///
/// `$XDG_RUNTIME_DIR` is deliberately avoided: systemd-logind deletes it when
/// the user's last login session ends, which would orphan every detached
/// session. Like tmux, we use a private directory under /tmp instead.
pub fn socket_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("RTERM_SOCKET_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    PathBuf::from(format!("/tmp/rterm-{}", nix::unistd::getuid()))
}

/// Create the socket directory if needed and verify nobody else controls it.
pub fn ensure_socket_dir() -> Result<PathBuf> {
    let dir = socket_dir();
    match fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).with_context(|| format!("creating {}", dir.display())),
    }
    let meta = fs::symlink_metadata(&dir).with_context(|| format!("checking {}", dir.display()))?;
    if !meta.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    if meta.uid() != nix::unistd::getuid().as_raw() {
        bail!(
            "{} is owned by another user; refusing to use it",
            dir.display()
        );
    }
    if meta.mode() & 0o077 != 0 {
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restricting permissions on {}", dir.display()))?;
    }
    Ok(dir)
}

pub fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@' | '+'));
    if !ok {
        bail!("invalid session name {name:?} (use letters, digits and -_.@+, up to 64 characters)");
    }
    Ok(())
}

pub fn socket_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.sock"))
}

pub fn lock_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.lock"))
}

pub fn log_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.log"))
}

/// Stable path handed to the session as `SSH_AUTH_SOCK`; it is re-pointed at
/// the agent socket of whichever ssh connection attached most recently.
pub fn agent_link_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.agent"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        for ok in ["main", "a", "work-1", "x_y.z", "me@host", "a+b"] {
            validate_name(ok).unwrap();
        }
        for bad in ["", ".hidden", "-x", "a/b", "a b", "ü", &"x".repeat(65)] {
            assert!(validate_name(bad).is_err(), "{bad:?} should be rejected");
        }
    }
}
