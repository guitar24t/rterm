//! Small Unix helpers: PTY spawning, poll, terminal size and raw mode.

use std::ffi::OsString;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use nix::fcntl::{FcntlArg, FdFlag, OFlag, fcntl};
use nix::pty::{Winsize, openpty};
use nix::sys::termios::{self, SetArg, Termios};
use nix::unistd::Pid;

use crate::protocol::WinSize;

pub fn set_cloexec(fd: BorrowedFd<'_>) -> nix::Result<()> {
    fcntl(fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).map(drop)
}

pub fn set_nonblocking(fd: BorrowedFd<'_>) -> nix::Result<()> {
    let flags = OFlag::from_bits_retain(fcntl(fd, FcntlArg::F_GETFL)?);
    fcntl(fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).map(drop)
}

fn to_winsize(s: WinSize) -> Winsize {
    Winsize {
        ws_row: s.rows,
        ws_col: s.cols,
        ws_xpixel: s.xpix,
        ws_ypixel: s.ypix,
    }
}

pub fn get_winsize(fd: RawFd) -> Option<WinSize> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let r = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
    (r == 0 && ws.ws_row > 0 && ws.ws_col > 0).then_some(WinSize {
        rows: ws.ws_row,
        cols: ws.ws_col,
        xpix: ws.ws_xpixel,
        ypix: ws.ws_ypixel,
    })
}

pub fn set_winsize(fd: RawFd, size: WinSize) -> io::Result<()> {
    let ws = to_winsize(size);
    if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The user's login shell: `$SHELL`, else the passwd entry, else /bin/sh.
pub fn login_shell() -> OsString {
    if let Some(s) = std::env::var_os("SHELL").filter(|s| !s.is_empty()) {
        return s;
    }
    if let Ok(Some(user)) = nix::unistd::User::from_uid(nix::unistd::getuid())
        && !user.shell.as_os_str().is_empty()
    {
        return user.shell.into_os_string();
    }
    "/bin/sh".into()
}

pub struct Child {
    pub master: OwnedFd,
    pub pid: Pid,
}

/// Start `argv` (or a login shell when empty) on a new PTY as the leader of
/// a new session, the way sshd or a terminal emulator would.
pub fn spawn_on_pty(
    argv: &[OsString],
    size: WinSize,
    cwd: &Path,
    env: &[(&str, OsString)],
) -> Result<Child> {
    let pty = openpty(Some(&to_winsize(size)), None).context("openpty")?;
    set_cloexec(pty.master.as_fd())?;
    set_cloexec(pty.slave.as_fd())?;

    let mut cmd = match argv.split_first() {
        Some((prog, args)) => {
            let mut c = Command::new(prog);
            c.args(args);
            c
        }
        None => {
            let shell = login_shell();
            let base = Path::new(&shell)
                .file_name()
                .map(|b| b.to_os_string())
                .unwrap_or_else(|| "sh".into());
            let mut argv0 = OsString::from("-");
            argv0.push(base);
            let mut c = Command::new(&shell);
            c.arg0(argv0);
            c
        }
    };
    cmd.stdin(Stdio::from(pty.slave.try_clone()?))
        .stdout(Stdio::from(pty.slave.try_clone()?))
        .stderr(Stdio::from(pty.slave))
        .current_dir(cwd);
    for (k, v) in env {
        cmd.env(k, v);
    }
    if std::env::var_os("TERM").is_none_or(|t| t.is_empty()) {
        cmd.env("TERM", "xterm-256color");
    }
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            // The daemon ignores SIGHUP; ignored dispositions survive exec.
            libc::signal(libc::SIGHUP, libc::SIG_DFL);
            Ok(())
        });
    }
    let child = cmd
        .spawn()
        .with_context(|| format!("starting {:?}", cmd.get_program()))?;
    Ok(Child {
        master: pty.master,
        pid: Pid::from_raw(child.id() as i32),
    })
}

/// poll(2) that retries on EINTR. `timeout_ms < 0` waits forever.
pub fn poll(fds: &mut [libc::pollfd], timeout_ms: i32) -> io::Result<usize> {
    loop {
        let r = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
        if r >= 0 {
            return Ok(r as usize);
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

pub fn pollfd(fd: RawFd, read: bool, write: bool) -> libc::pollfd {
    let mut events = 0;
    if read {
        events |= libc::POLLIN;
    }
    if write {
        events |= libc::POLLOUT;
    }
    libc::pollfd {
        fd,
        events,
        revents: 0,
    }
}

pub fn readable(p: &libc::pollfd) -> bool {
    p.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0
}

pub fn writable(p: &libc::pollfd) -> bool {
    p.revents & (libc::POLLOUT | libc::POLLHUP | libc::POLLERR) != 0
}

/// Puts a terminal in raw mode and restores it when dropped.
pub struct RawMode {
    fd: RawFd,
    saved: Termios,
}

impl RawMode {
    pub fn enable(fd: RawFd) -> Result<RawMode> {
        let bfd = unsafe { BorrowedFd::borrow_raw(fd) };
        let saved = termios::tcgetattr(bfd).context("reading terminal attributes")?;
        let mut raw = saved.clone();
        termios::cfmakeraw(&mut raw);
        termios::tcsetattr(bfd, SetArg::TCSADRAIN, &raw).context("entering raw mode")?;
        Ok(RawMode { fd, saved })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let bfd = unsafe { BorrowedFd::borrow_raw(self.fd) };
        let _ = termios::tcsetattr(bfd, SetArg::TCSADRAIN, &self.saved);
    }
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
