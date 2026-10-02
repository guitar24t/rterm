//! The `rterm` front end: attach the current terminal to a session, plus the
//! small management commands.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use nix::fcntl::{Flock, FlockArg};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGWINCH};

use crate::keys::{DetachKey, KeyScanner};
use crate::paths;
use crate::protocol::{self, FrameReader, Msg, SessionInfo, WinSize};
use crate::sys::{self, RawMode};

/// Stop reading the keyboard while this much input is waiting to be sent.
const SEND_HIGH_WATER: usize = 1 << 20;

pub struct AttachOptions {
    pub create: bool,
    pub command: Vec<OsString>,
    pub detach_key: Option<DetachKey>,
    pub scrollback: usize,
}

pub enum Connect {
    Connected(UnixStream),
    Missing,
}

/// Connect to a session's socket, cleaning up after a dead daemon.
pub fn connect(dir: &Path, name: &str) -> Result<Connect> {
    let path = paths::socket_path(dir, name);
    match UnixStream::connect(&path) {
        Ok(s) => Ok(Connect::Connected(s)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Connect::Missing),
        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
            // Nobody listening. If nobody holds the lock either, the daemon
            // is gone and the socket is stale.
            if let Ok(f) = File::open(paths::lock_path(dir, name)) {
                if Flock::lock(f, FlockArg::LockExclusiveNonblock).is_ok() {
                    let _ = fs::remove_file(&path);
                    let _ = fs::remove_file(paths::lock_path(dir, name));
                }
            } else {
                let _ = fs::remove_file(&path);
            }
            Ok(Connect::Missing)
        }
        Err(e) => Err(e).with_context(|| format!("connecting to {}", path.display())),
    }
}

/// Start a daemon for `name` and wait until it is ready.
pub fn create_session(
    dir: &Path,
    name: &str,
    size: WinSize,
    scrollback: usize,
    command: &[OsString],
) -> Result<()> {
    let exe = std::env::current_exe().context("locating the rterm executable")?;
    let log = File::create(paths::log_path(dir, name))?;
    let mut cmd = Command::new(exe);
    cmd.arg("__daemon")
        .arg("--name")
        .arg(name)
        .arg("--rows")
        .arg(size.rows.to_string())
        .arg("--cols")
        .arg(size.cols.to_string())
        .arg("--xpix")
        .arg(size.xpix.to_string())
        .arg("--ypix")
        .arg(size.ypix.to_string())
        .arg("--scrollback")
        .arg(scrollback.to_string());
    if !command.is_empty() {
        cmd.arg("--").args(command);
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(log);
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
    let mut child = cmd.spawn().context("starting session daemon")?;
    let mut status = String::new();
    child.stdout.take().unwrap().read_to_string(&mut status)?;
    let status = status.trim();
    match status {
        "ok" => Ok(()),
        s if s.ends_with("already exists") => Ok(()), // lost a creation race
        "" => {
            let _ = child.wait();
            let log = fs::read_to_string(paths::log_path(dir, name)).unwrap_or_default();
            bail!(
                "session daemon failed to start{}",
                if log.is_empty() {
                    String::new()
                } else {
                    format!(":\n{log}")
                }
            )
        }
        s => bail!("{}", s.strip_prefix("error: ").unwrap_or(s)),
    }
}

pub fn session_exists(dir: &Path, name: &str) -> Result<bool> {
    Ok(matches!(connect(dir, name)?, Connect::Connected(_)))
}

fn terminal_size() -> WinSize {
    sys::get_winsize(libc::STDOUT_FILENO)
        .or_else(|| sys::get_winsize(libc::STDIN_FILENO))
        .unwrap_or(WinSize {
            rows: 24,
            cols: 80,
            xpix: 0,
            ypix: 0,
        })
}

pub fn new_detached(name: &str, opts: &AttachOptions) -> Result<()> {
    let dir = paths::ensure_socket_dir()?;
    if session_exists(&dir, name)? {
        bail!("session {name:?} already exists");
    }
    create_session(&dir, name, terminal_size(), opts.scrollback, &opts.command)
}

pub fn attach(name: &str, opts: &AttachOptions) -> Result<i32> {
    if std::env::var("RTERM_SESSION").is_ok_and(|s| s == name) {
        bail!("refusing to attach session {name:?} from inside itself");
    }
    let stdin = libc::STDIN_FILENO;
    if unsafe { libc::isatty(stdin) } != 1 || unsafe { libc::isatty(libc::STDOUT_FILENO) } != 1 {
        bail!("rterm needs a terminal (stdin and stdout must be a tty)");
    }
    let dir = paths::ensure_socket_dir()?;
    let size = terminal_size();
    let sock = match connect(&dir, name)? {
        Connect::Connected(s) => {
            if !opts.command.is_empty() {
                eprintln!("rterm: session {name:?} already exists; ignoring the command");
            }
            s
        }
        Connect::Missing if opts.create => {
            create_session(&dir, name, size, opts.scrollback, &opts.command)?;
            match connect(&dir, name)? {
                Connect::Connected(s) => s,
                Connect::Missing => bail!("session {name:?} exited immediately"),
            }
        }
        Connect::Missing => bail!("no session named {name:?}"),
    };

    let ssh_auth_sock = std::env::var("SSH_AUTH_SOCK")
        .ok()
        .filter(|s| !s.is_empty());
    let daemon_pid = peer_pid(&sock);
    let mut client = Client::new(sock, opts.detach_key)?;
    client.send(&Msg::Attach {
        version: protocol::VERSION,
        size,
        ssh_auth_sock,
    });

    let outcome = {
        let _raw = RawMode::enable(stdin)?;
        client.run()
    };
    let mut err = io::stderr();
    match outcome? {
        Outcome::Detached(reason) => {
            let _ = writeln!(err, "[rterm: {reason} from session '{name}']");
            Ok(0)
        }
        Outcome::Exited(code) => {
            let _ = writeln!(err, "[rterm: session '{name}' exited]");
            Ok(code)
        }
        Outcome::Lost => {
            let _ = writeln!(err, "[rterm: lost connection to session '{name}']");
            Ok(1)
        }
        Outcome::Hangup => Ok(1),
        Outcome::Error(msg) => bail!("{msg}"),
        Outcome::Incompatible(version) => match daemon_pid {
            Some(pid) => run_daemon_binary(pid, version),
            None => bail!("session {name:?} was started by an rterm speaking protocol v{version}"),
        },
    }
}

enum Outcome {
    Detached(String),
    Exited(i32),
    /// The daemon went away without saying goodbye.
    Lost,
    /// Our own terminal went away (SIGHUP).
    Hangup,
    Error(String),
    /// The session was started by a different rterm version.
    Incompatible(u32),
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
fn peer_pid(sock: &UnixStream) -> Option<i32> {
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
fn run_daemon_binary(pid: i32, version: u32) -> Result<i32> {
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

/// Send one request to a session and wait for the reply.
fn request(dir: &Path, name: &str, msg: &Msg) -> Result<Option<Msg>> {
    let mut sock = match connect(dir, name)? {
        Connect::Connected(s) => s,
        Connect::Missing => return Ok(None),
    };
    sock.set_read_timeout(Some(Duration::from_secs(5)))?;
    protocol::send(&mut sock, msg)?;
    let reply = protocol::recv(&mut sock, &mut FrameReader::default())?;
    match reply {
        Msg::Error(e) => return Err(anyhow!(e)),
        Msg::Incompatible { version } => {
            bail!("started by a different rterm version (protocol v{version})")
        }
        _ => {}
    }
    Ok(Some(reply))
}

pub fn list_sessions() -> Result<Vec<Result<SessionInfo, (String, String)>>> {
    let dir = paths::ensure_socket_dir()?;
    let mut names: Vec<String> = fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            e.file_name()
                .to_str()?
                .strip_suffix(".sock")
                .map(str::to_owned)
        })
        .collect();
    names.sort();
    let mut out = Vec::new();
    for name in names {
        match request(
            &dir,
            &name,
            &Msg::Query {
                version: protocol::VERSION,
            },
        ) {
            Ok(Some(Msg::Info(info))) => out.push(Ok(info)),
            Ok(_) => {}
            Err(e) => out.push(Err((name, format!("{e:#}")))),
        }
    }
    Ok(out)
}

pub fn ls(json: bool) -> Result<()> {
    let sessions = list_sessions()?;
    if json {
        println!("{}", sessions_json(&sessions, sys::now_unix()));
        return Ok(());
    }
    if sessions.is_empty() {
        println!("no sessions");
        return Ok(());
    }
    let now = sys::now_unix();
    let rows: Vec<[String; 5]> = sessions
        .iter()
        .map(|s| match s {
            Ok(i) => [
                i.name.clone(),
                if i.attached {
                    "attached".into()
                } else {
                    "detached".into()
                },
                format_age(now.saturating_sub(i.created)),
                format!("{}x{}", i.cols, i.rows),
                if i.title.is_empty() {
                    i.command.clone()
                } else {
                    i.title.clone()
                },
            ],
            Err((name, e)) => [
                name.clone(),
                "?".into(),
                String::new(),
                String::new(),
                e.clone(),
            ],
        })
        .collect();
    let header = ["NAME", "STATUS", "AGE", "SIZE", "TITLE"];
    let mut widths = header.map(str::len);
    for r in &rows {
        for (w, c) in widths.iter_mut().zip(r) {
            *w = (*w).max(c.chars().count());
        }
    }
    let print = |r: [&str; 5]| {
        let line = format!(
            "{:w0$}  {:w1$}  {:>w2$}  {:w3$}  {}",
            r[0],
            r[1],
            r[2],
            r[3],
            r[4],
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2],
            w3 = widths[3]
        );
        println!("{}", line.trim_end());
    };
    print(header);
    for r in &rows {
        print([&r[0], &r[1], &r[2], &r[3], &r[4]]);
    }
    Ok(())
}

/// `rterm ls --json`: a stable, machine-readable listing (one array,
/// one object per session) for scripts such as contrib/rterm-connect.py.
fn sessions_json(sessions: &[Result<SessionInfo, (String, String)>], now: u64) -> String {
    let items: Vec<String> = sessions
        .iter()
        .map(|s| match s {
            Ok(i) => {
                let age = now.saturating_sub(i.created);
                format!(
                    "{{\"name\":{},\"status\":\"{}\",\"created\":{},\"age\":{},\"age_text\":{},\
                     \"cols\":{},\"rows\":{},\"pid\":{},\"command\":{},\"title\":{}}}",
                    json_str(&i.name),
                    if i.attached { "attached" } else { "detached" },
                    i.created,
                    age,
                    json_str(&format_age(age)),
                    i.cols,
                    i.rows,
                    i.pid,
                    json_str(&i.command),
                    json_str(&i.title),
                )
            }
            Err((name, e)) => format!(
                "{{\"name\":{},\"status\":\"unknown\",\"error\":{}}}",
                json_str(name),
                json_str(e)
            ),
        })
        .collect();
    format!("[{}]", items.join(","))
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn format_age(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86400 => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
        s => format!("{}d{:02}h", s / 86400, s % 86400 / 3600),
    }
}

pub fn kill(name: &str) -> Result<()> {
    let dir = paths::ensure_socket_dir()?;
    if request(&dir, name, &Msg::Kill)?.is_none() {
        bail!("no session named {name:?}");
    }
    // Return once the session is really gone, so that a following `ls` or
    // `new` with the same name doesn't race the exiting daemon. (It escalates
    // to SIGKILL after a few seconds if the program ignores the hangup.)
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    while matches!(connect(&dir, name)?, Connect::Connected(_)) {
        if std::time::Instant::now() >= deadline {
            bail!("session {name:?} is still shutting down");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

pub fn detach(name: &str) -> Result<()> {
    let dir = paths::ensure_socket_dir()?;
    match request(&dir, name, &Msg::DetachClient)? {
        Some(_) => Ok(()),
        None => bail!("no session named {name:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_listing() {
        let info = SessionInfo {
            name: "main".into(),
            pid: 42,
            created: 1000,
            attached: true,
            rows: 24,
            cols: 80,
            command: "-zsh".into(),
            title: "say \"hi\"\\\t\u{1b}é".into(),
        };
        let sessions = vec![Ok(info), Err(("old".to_owned(), "bad\nthing".to_owned()))];
        assert_eq!(
            sessions_json(&sessions, 4723),
            "[{\"name\":\"main\",\"status\":\"attached\",\"created\":1000,\"age\":3723,\
             \"age_text\":\"1h02m\",\"cols\":80,\"rows\":24,\"pid\":42,\"command\":\"-zsh\",\
             \"title\":\"say \\\"hi\\\"\\\\\\t\\u001bé\"},\
             {\"name\":\"old\",\"status\":\"unknown\",\"error\":\"bad\\nthing\"}]"
        );
        assert_eq!(sessions_json(&[], 0), "[]");
    }
}
