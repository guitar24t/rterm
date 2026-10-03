//! Unix side of the client: raw terminal mode, the poll(2) relay loop,
//! signals, and launching session daemons with setsid.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use nix::fcntl::{Flock, FlockArg};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGWINCH};

use super::Outcome;
use crate::keys::{DetachKey, KeyScanner};
use crate::protocol::{FrameReader, Msg, WinSize};
use crate::sys::{self, RawMode};

/// Stop reading the keyboard while this much input is waiting to be sent.
const SEND_HIGH_WATER: usize = 1 << 20;

pub fn is_terminal() -> bool {
    unsafe { libc::isatty(libc::STDIN_FILENO) == 1 && libc::isatty(libc::STDOUT_FILENO) == 1 }
}

pub fn terminal_size() -> Option<WinSize> {
    sys::get_winsize(libc::STDOUT_FILENO).or_else(|| sys::get_winsize(libc::STDIN_FILENO))
}

/// True if no daemon holds this session's lock (so its socket is stale).
pub fn lock_is_free(lock: &Path) -> bool {
    match File::open(lock) {
        Ok(f) => Flock::lock(f, FlockArg::LockExclusiveNonblock).is_ok(),
        Err(_) => true,
    }
}

/// Start `rterm __daemon ...` detached from this terminal.
pub fn spawn_daemon(args: &[OsString], log: File) -> Result<Child> {
    let exe = std::env::current_exe().context("locating the rterm executable")?;
    let mut cmd = Command::new(exe);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(log);
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            // Leave our session and process group so terminal hangups and
            // job-control signals aimed at this terminal never reach it.
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn().context("starting session daemon")
}

/// Relay between this terminal and an attached session until it ends.
pub fn run(sock: UnixStream, detach_key: Option<DetachKey>, hello: &Msg) -> Result<Outcome> {
    let mut client = Client::new(sock, detach_key)?;
    client.send(hello);
    let _raw = RawMode::enable(libc::STDIN_FILENO)?;
    client.run()
}

struct Client {
    sock: UnixStream,
    reader: FrameReader,
    out: Vec<u8>,
    keys: Option<KeyScanner>,
    sig_read: UnixStream,
    winch: Arc<AtomicBool>,
    hangup: Arc<AtomicBool>,
    terminate: Arc<AtomicBool>,
    detaching: bool,
}

impl Client {
    fn new(sock: UnixStream, detach_key: Option<DetachKey>) -> Result<Client> {
        sock.set_nonblocking(true)?;
        let (sig_read, sig_write) = UnixStream::pair()?;
        sig_read.set_nonblocking(true)?;
        sig_write.set_nonblocking(true)?;
        let winch = Arc::new(AtomicBool::new(false));
        let hangup = Arc::new(AtomicBool::new(false));
        let terminate = Arc::new(AtomicBool::new(false));
        signal_hook::flag::register(SIGWINCH, winch.clone())?;
        signal_hook::flag::register(SIGHUP, hangup.clone())?;
        for sig in [SIGTERM, SIGINT] {
            signal_hook::flag::register(sig, terminate.clone())?;
        }
        for sig in [SIGWINCH, SIGTERM, SIGINT, SIGHUP] {
            signal_hook::low_level::pipe::register(sig, sig_write.try_clone()?)?;
        }
        Ok(Client {
            sock,
            reader: FrameReader::default(),
            out: Vec::new(),
            keys: detach_key.map(KeyScanner::new),
            sig_read,
            winch,
            hangup,
            terminate,
            detaching: false,
        })
    }

    fn send(&mut self, msg: &Msg) {
        msg.encode_into(&mut self.out);
    }

    fn flush(&mut self) -> io::Result<()> {
        while !self.out.is_empty() {
            match self.sock.write(&self.out) {
                Ok(n) => {
                    self.out.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn run(&mut self) -> Result<Outcome> {
        let stdin = libc::STDIN_FILENO;
        let mut stdout = io::stdout().lock();
        let mut buf = vec![0u8; 64 << 10];
        loop {
            if self.flush().is_err() {
                return Ok(Outcome::Lost);
            }
            let mut fds = [
                sys::pollfd(
                    stdin,
                    !self.detaching && self.out.len() < SEND_HIGH_WATER,
                    false,
                ),
                sys::pollfd(self.sock.as_raw_fd(), true, !self.out.is_empty()),
                sys::pollfd(self.sig_read.as_raw_fd(), true, false),
            ];
            // Once a detach was requested, don't wait forever for the daemon.
            let timeout = if self.detaching { 2000 } else { -1 };
            if sys::poll(&mut fds, timeout)? == 0 {
                return Ok(Outcome::Detached("detached".into()));
            }

            if sys::readable(&fds[2]) {
                let mut drain = [0u8; 64];
                while matches!(self.sig_read.read(&mut drain), Ok(n) if n > 0) {}
                if self.hangup.swap(false, Ordering::SeqCst) {
                    return Ok(Outcome::Hangup);
                }
                // Killed while the terminal is still there: detach properly
                // so the terminal gets reset.
                if self.terminate.swap(false, Ordering::SeqCst) && !self.detaching {
                    self.send(&Msg::Detach);
                    self.detaching = true;
                }
                if self.winch.swap(false, Ordering::SeqCst)
                    && let Some(size) = sys::get_winsize(libc::STDOUT_FILENO)
                {
                    self.send(&Msg::Resize(size));
                }
            }

            if sys::readable(&fds[1]) {
                let mut eof = false;
                loop {
                    match self.sock.read(&mut buf) {
                        Ok(0) => {
                            eof = true;
                            break;
                        }
                        Ok(n) => self.reader.push(&buf[..n]),
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(_) => {
                            eof = true;
                            break;
                        }
                    }
                }
                while let Some(msg) = self.reader.next()? {
                    match msg {
                        Msg::Output(data) => {
                            stdout.write_all(&data)?;
                        }
                        Msg::Detached {
                            reason,
                            drain_input,
                        } => {
                            stdout.flush()?;
                            if drain_input && self.detaching {
                                drain_key_events();
                            }
                            return Ok(Outcome::Detached(reason));
                        }
                        Msg::Exited { code } => {
                            stdout.flush()?;
                            return Ok(Outcome::Exited(code));
                        }
                        Msg::Error(e) => return Ok(Outcome::Error(e)),
                        Msg::Incompatible { version } => return Ok(Outcome::Incompatible(version)),
                        _ => {}
                    }
                }
                stdout.flush()?;
                if eof {
                    return Ok(Outcome::Lost);
                }
            }

            if sys::readable(&fds[0]) {
                let n = match nix::unistd::read(io::stdin(), &mut buf) {
                    Ok(n) => n,
                    Err(nix::errno::Errno::EINTR | nix::errno::Errno::EAGAIN) => continue,
                    Err(_) => 0,
                };
                if n == 0 {
                    // Our terminal is gone.
                    return Ok(Outcome::Hangup);
                }
                let input = &buf[..n];
                match self.keys.as_mut().and_then(|k| k.scan(input)) {
                    Some(start) => {
                        if start > 0 {
                            self.send(&Msg::Input(input[..start].to_vec()));
                        }
                        self.send(&Msg::Detach);
                        self.detaching = true;
                    }
                    None => self.send(&Msg::Input(input.to_vec())),
                }
            }
        }
    }
}

/// After detaching with the kitty keyboard protocol's release reporting on,
/// the terminal may still send release events for the detach key (and its
/// modifier) before it processes our reset. Swallow input until it goes
/// quiet so those don't land in the user's shell as garbage.
fn drain_key_events() {
    let deadline = std::time::Instant::now() + Duration::from_millis(1500);
    let mut buf = [0u8; 1024];
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return;
        }
        let mut fds = [sys::pollfd(libc::STDIN_FILENO, true, false)];
        let wait = left.min(Duration::from_millis(250)).as_millis() as i32;
        match sys::poll(&mut fds, wait) {
            Ok(1) if sys::readable(&fds[0]) => {
                if !matches!(nix::unistd::read(io::stdin(), &mut buf), Ok(n) if n > 0) {
                    return;
                }
            }
            _ => return,
        }
    }
}

/// Pid of the process on the other end of a Unix socket.
pub fn peer_pid(sock: &UnixStream) -> Option<i32> {
    #[cfg(target_os = "linux")]
    {
        use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
        getsockopt(sock, PeerCredentials).ok().map(|c| c.pid())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = sock;
        None
    }
}

/// The session's daemon runs an rterm from before an upgrade. Its binary
/// may be gone from disk, but the kernel still has it: hand this command
/// over to it so the session stays reachable until it ends.
pub fn run_daemon_binary(pid: i32, version: u32) -> Result<i32> {
    let exe = format!("/proc/{pid}/exe");
    if !Path::new(&exe).exists() {
        bail!(
            "session was started by an rterm speaking protocol v{version}, which isn't available"
        );
    }
    let err = Command::new(&exe)
        .arg0("rterm")
        .args(std::env::args_os().skip(1))
        .exec();
    Err(err).with_context(|| format!("running the session's rterm ({exe})"))
}
