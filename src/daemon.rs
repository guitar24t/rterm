//! The per-session server process.
//!
//! One daemon owns one PTY and the program running on it. It outlives the
//! client (and the ssh connection the client ran in), listens on a Unix
//! socket for clients, and pipes bytes between the PTY and whichever client
//! is attached.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use nix::sys::signal::{Signal, kill, killpg};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;

use crate::DaemonArgs;
use crate::ipc::Listener;
use crate::paths;
use crate::protocol::{self, FrameReader, Msg, SessionInfo, WinSize};
use crate::screen::{Screen, clamp_size};
use crate::sys;

/// Stop reading the PTY while this much output is queued for the client
/// (backpressure, like a real terminal that can't keep up).
const CLIENT_HIGH_WATER: usize = 4 << 20;
/// Stop reading client input while this much is waiting to enter the PTY.
const PTY_HIGH_WATER: usize = 1 << 20;
/// Max PTY bytes consumed per loop iteration, to stay responsive to input.
const READ_BUDGET: usize = 512 << 10;
const HEALTH_INTERVAL: Duration = Duration::from_secs(60);
const TOUCH_INTERVAL: Duration = Duration::from_secs(3600);
const KILL_GRACE: Duration = Duration::from_secs(3);

struct Conn {
    sock: UnixStream,
    reader: FrameReader,
    out: Vec<u8>,
    attached: bool,
    /// Close once `out` has been flushed.
    closing: bool,
    dead: bool,
}

impl Conn {
    fn new(sock: UnixStream) -> Conn {
        Conn {
            sock,
            reader: FrameReader::default(),
            out: Vec::new(),
            attached: false,
            closing: false,
            dead: false,
        }
    }

    fn queue(&mut self, msg: &Msg) {
        msg.encode_into(&mut self.out);
    }

    fn flush(&mut self) {
        while !self.out.is_empty() {
            match self.sock.write(&self.out) {
                Ok(0) => {
                    self.dead = true;
                    return;
                }
                Ok(n) => {
                    self.out.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    self.dead = true;
                    return;
                }
            }
        }
        if self.closing {
            self.dead = true;
        }
    }

    /// Read whatever is available into the frame reader.
    fn fill(&mut self) {
        let mut buf = [0u8; 64 << 10];
        loop {
            match self.sock.read(&mut buf) {
                Ok(0) => {
                    self.dead = true;
                    return;
                }
                Ok(n) => self.reader.push(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    self.dead = true;
                    return;
                }
            }
        }
    }
}

struct Daemon {
    name: String,
    dir: PathBuf,
    sock_path: PathBuf,
    sock_ino: u64,
    listener: Listener,
    lock_path: PathBuf,
    _lock: Flock<File>,
    agent_link: Option<PathBuf>,

    master: OwnedFd,
    master_eof: bool,
    child: Pid,
    exit_code: Option<i32>,
    kill_deadline: Option<Instant>,

    screen: Screen,
    size: WinSize,
    pty_out: Vec<u8>,
    conns: Vec<Conn>,

    sig_read: UnixStream,
    term_requested: Arc<AtomicBool>,
    created: u64,
    command: String,
    last_health: Instant,
    last_touch: Instant,
}

/// Entry point of `rterm __daemon`. Reports "ok" (or an error) on stdout
/// once the session is ready, then serves until the session ends.
pub fn run(args: DaemonArgs) -> Result<()> {
    close_inherited_fds();
    let mut daemon = match Daemon::start(args) {
        Ok(d) => d,
        Err(e) => {
            println!("error: {e:#}");
            return Ok(());
        }
    };
    println!("ok");
    io::stdout().flush().ok();
    // Detach stdout from the client that started us.
    if let Ok(null) = File::options().write(true).open("/dev/null") {
        unsafe { libc::dup2(null.as_raw_fd(), 1) };
    }
    daemon.serve()
}

impl Daemon {
    fn start(args: DaemonArgs) -> Result<Daemon> {
        let dir = paths::ensure_socket_dir()?;
        let lock_path = paths::lock_path(&dir, &args.name);
        let lock_file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC)
            .open(&lock_path)
            .with_context(|| format!("opening {}", lock_path.display()))?;
        let lock = match Flock::lock(lock_file, FlockArg::LockExclusiveNonblock) {
            Ok(l) => l,
            Err((_, Errno::EWOULDBLOCK)) => bail!("session {:?} already exists", args.name),
            Err((_, e)) => return Err(e).context("locking session"),
        };

        // We hold the lock, so any socket at this path is stale.
        let sock_path = paths::socket_path(&dir, &args.name);
        let _ = fs::remove_file(&sock_path);
        let listener = bind(&sock_path)?;
        let sock_ino = fs::symlink_metadata(&sock_path)?.ino();

        let mut env: Vec<(&str, OsString)> = vec![("RTERM_SESSION", args.name.clone().into())];
        let mut agent_link = None;
        if let Some(agent) = std::env::var_os("SSH_AUTH_SOCK").filter(|a| !a.is_empty()) {
            let link = paths::agent_link_path(&dir, &args.name);
            if point_link(&link, Path::new(&agent)).is_ok() {
                env.push(("SSH_AUTH_SOCK", link.clone().into()));
                agent_link = Some(link);
            }
        }

        let (rows, cols) = clamp_size(args.size.rows, args.size.cols);
        let size = WinSize {
            rows,
            cols,
            ..args.size
        };
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let child = sys::spawn_on_pty(&args.command, size, &cwd, &env)?;
        sys::set_nonblocking(child.master.as_fd())?;

        let command = if args.command.is_empty() {
            let shell = sys::login_shell();
            Path::new(&shell)
                .file_name()
                .unwrap_or(shell.as_ref())
                .to_string_lossy()
                .into_owned()
        } else {
            args.command
                .iter()
                .map(|a| a.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ")
        };

        let (sig_read, sig_write) = UnixStream::pair()?;
        sig_read.set_nonblocking(true)?;
        sig_write.set_nonblocking(true)?;
        let term_requested = Arc::new(AtomicBool::new(false));
        for sig in [libc::SIGTERM, libc::SIGINT] {
            signal_hook::flag::register(sig, term_requested.clone())?;
        }
        for sig in [libc::SIGCHLD, libc::SIGTERM, libc::SIGINT] {
            signal_hook::low_level::pipe::register(sig, sig_write.try_clone()?)?;
        }
        unsafe { libc::signal(libc::SIGHUP, libc::SIG_IGN) };
        let _ = std::env::set_current_dir("/");

        Ok(Daemon {
            name: args.name,
            dir,
            sock_path,
            sock_ino,
            listener,
            lock_path,
            _lock: lock,
            agent_link,
            master: child.master,
            master_eof: false,
            child: child.pid,
            exit_code: None,
            kill_deadline: None,
            screen: Screen::new(size.rows, size.cols, args.scrollback),
            size,
            pty_out: Vec::new(),
            conns: Vec::new(),
            sig_read,
            term_requested,
            created: crate::util::now_unix(),
            command,
            last_health: Instant::now(),
            last_touch: Instant::now(),
        })
    }

    fn attached(&mut self) -> Option<&mut Conn> {
        self.conns.iter_mut().find(|c| c.attached && !c.closing)
    }

    fn serve(&mut self) -> Result<()> {
        let mut fds: Vec<libc::pollfd> = Vec::new();
        loop {
            if self.exit_code.is_some() {
                self.drain_master();
                return self.finish();
            }

            let client_backlog = self.attached().map_or(0, |c| c.out.len());
            fds.clear();
            fds.push(sys::pollfd(self.sig_read.as_raw_fd(), true, false));
            fds.push(sys::pollfd(self.listener.as_raw_fd(), true, false));
            fds.push(sys::pollfd(
                self.master.as_raw_fd(),
                !self.master_eof && client_backlog < CLIENT_HIGH_WATER,
                !self.pty_out.is_empty() && !self.master_eof,
            ));
            for c in &self.conns {
                let pty_full = c.attached && self.pty_out.len() >= PTY_HIGH_WATER;
                let read = !c.closing && !pty_full;
                fds.push(sys::pollfd(c.sock.as_raw_fd(), read, !c.out.is_empty()));
            }

            let timeout = match self.kill_deadline {
                Some(d) => d.saturating_duration_since(Instant::now()).as_millis() as i32 + 1,
                None => HEALTH_INTERVAL.as_millis() as i32,
            };
            sys::poll(&mut fds, timeout)?;

            if sys::readable(&fds[0]) {
                self.handle_signals();
            }
            if sys::readable(&fds[1]) {
                self.accept();
            }
            if sys::writable(&fds[2]) && !self.pty_out.is_empty() {
                self.write_pty();
            }
            if sys::readable(&fds[2]) {
                self.read_pty();
            }
            for i in 0..fds.len() - 3 {
                let pfd = fds[3 + i];
                if i >= self.conns.len() {
                    break;
                }
                if sys::readable(&pfd) {
                    self.conns[i].fill();
                    self.process_conn(i);
                }
            }
            for c in &mut self.conns {
                c.flush();
            }
            self.conns.retain(|c| !c.dead);

            if let Some(d) = self.kill_deadline
                && Instant::now() >= d
            {
                let _ = kill(self.child, Signal::SIGKILL);
                self.kill_deadline = None;
            }
            self.health_check();
        }
    }

    fn handle_signals(&mut self) {
        let mut buf = [0u8; 64];
        while matches!(self.sig_read.read(&mut buf), Ok(n) if n > 0) {}
        loop {
            match waitpid(self.child, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(_, code)) => {
                    self.exit_code = Some(code);
                    break;
                }
                Ok(WaitStatus::Signaled(_, sig, _)) => {
                    self.exit_code = Some(128 + sig as i32);
                    break;
                }
                Ok(WaitStatus::StillAlive) | Err(_) => break,
                Ok(_) => continue,
            }
        }
        if self.term_requested.swap(false, Ordering::SeqCst) {
            self.kill_session();
        }
    }

    fn accept(&mut self) {
        loop {
            match self.listener.accept() {
                Ok((sock, _)) => {
                    if sock.set_nonblocking(true).is_ok() {
                        self.conns.push(Conn::new(sock));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return,
            }
        }
    }

    fn read_pty(&mut self) {
        let mut buf = vec![0u8; 64 << 10];
        let mut total = 0;
        while total < READ_BUDGET {
            match nix::unistd::read(&self.master, &mut buf) {
                Ok(0) => {
                    self.master_eof = true;
                    break;
                }
                Ok(n) => {
                    total += n;
                    self.on_output(&buf[..n]);
                }
                Err(Errno::EAGAIN) => break,
                Err(Errno::EINTR) => {}
                // EIO: every process closed the terminal (normally the
                // shell exited; SIGCHLD follows).
                Err(_) => {
                    self.master_eof = true;
                    break;
                }
            }
        }
    }

    fn drain_master(&mut self) {
        if !self.master_eof {
            self.read_pty();
        }
    }

    fn on_output(&mut self, data: &[u8]) {
        self.screen.feed(data);
        let replies = self.screen.take_replies();
        match self.attached() {
            Some(c) => c.queue(&Msg::Output(data.to_vec())),
            // Nobody attached: answer terminal queries ourselves so programs
            // that wait for a reply don't hang.
            None => self.pty_out.extend_from_slice(&replies),
        }
        if !self.pty_out.is_empty() {
            self.write_pty();
        }
    }

    fn write_pty(&mut self) {
        while !self.pty_out.is_empty() {
            match nix::unistd::write(&self.master, &self.pty_out) {
                Ok(n) => {
                    self.pty_out.drain(..n);
                }
                Err(Errno::EINTR) => {}
                Err(Errno::EAGAIN) => return,
                Err(_) => {
                    self.pty_out.clear();
                    return;
                }
            }
        }
    }

    fn process_conn(&mut self, i: usize) {
        loop {
            let msg = match self.conns[i].reader.next() {
                Ok(Some(m)) => m,
                Ok(None) => return,
                Err(_) => {
                    self.conns[i].dead = true;
                    return;
                }
            };
            match msg {
                Msg::Attach {
                    version,
                    size,
                    ssh_auth_sock,
                } => {
                    if version != protocol::VERSION {
                        let c = &mut self.conns[i];
                        c.queue(&Msg::Incompatible {
                            version: protocol::VERSION,
                        });
                        c.closing = true;
                        return;
                    }
                    self.attach(i, size, ssh_auth_sock);
                }
                Msg::Input(data) => {
                    if self.conns[i].attached {
                        self.pty_out.extend_from_slice(&data);
                        self.write_pty();
                    }
                }
                Msg::Resize(size) => {
                    if self.conns[i].attached {
                        self.resize(size);
                    }
                }
                Msg::Detach => {
                    if self.conns[i].attached {
                        self.detach(i, "detached");
                    } else {
                        self.conns[i].closing = true;
                    }
                }
                Msg::Query { version } => {
                    let reply = if version == protocol::VERSION {
                        Msg::Info(self.info())
                    } else {
                        Msg::Incompatible {
                            version: protocol::VERSION,
                        }
                    };
                    self.conns[i].queue(&reply);
                    self.conns[i].closing = true;
                }
                Msg::DetachClient => {
                    if let Some(j) = self.conns.iter().position(|c| c.attached) {
                        self.detach(j, "detached by `rterm detach`");
                    }
                    self.conns[i].queue(&Msg::Ok);
                    self.conns[i].closing = true;
                }
                Msg::Kill => {
                    self.conns[i].queue(&Msg::Ok);
                    self.conns[i].closing = true;
                    self.kill_session();
                }
                // Daemon-to-client messages are not valid here.
                _ => {
                    self.conns[i].dead = true;
                    return;
                }
            }
        }
    }

    fn attach(&mut self, i: usize, size: WinSize, ssh_auth_sock: Option<String>) {
        for j in 0..self.conns.len() {
            if j != i && self.conns[j].attached {
                self.detach(j, "attached from another terminal");
            }
        }
        if let (Some(link), Some(agent)) = (&self.agent_link, ssh_auth_sock) {
            let _ = point_link(link, Path::new(&agent));
        }
        self.resize(size);
        let snapshot = self.screen.snapshot(usize::MAX);
        // Any pending replies were for queries the new terminal will answer.
        self.screen.take_replies();
        let c = &mut self.conns[i];
        c.queue(&Msg::Output(snapshot));
        c.attached = true;
    }

    fn detach(&mut self, i: usize, reason: &str) {
        // Flag 2 = "report event types": key releases are on their way.
        let drain_input = self.screen.kitty_flags() & 2 != 0;
        let reset = self.screen.reset_sequence();
        let c = &mut self.conns[i];
        c.queue(&Msg::Output(reset));
        c.queue(&Msg::Detached {
            reason: reason.to_owned(),
            drain_input,
        });
        c.attached = false;
        c.closing = true;
    }

    fn resize(&mut self, size: WinSize) {
        if size.rows == 0 || size.cols == 0 {
            return;
        }
        let (rows, cols) = clamp_size(size.rows, size.cols);
        let size = WinSize { rows, cols, ..size };
        if size == self.size {
            return;
        }
        self.size = size;
        self.screen.resize(rows, cols);
        let _ = sys::set_winsize(self.master.as_raw_fd(), size);
    }

    fn info(&self) -> SessionInfo {
        SessionInfo {
            name: self.name.clone(),
            pid: self.child.as_raw() as u32,
            created: self.created,
            attached: self.conns.iter().any(|c| c.attached),
            rows: self.size.rows,
            cols: self.size.cols,
            command: self.command.clone(),
            title: self.screen.title().unwrap_or_default(),
        }
    }

    /// Hang up the session like closing a terminal window would.
    fn kill_session(&mut self) {
        if self.kill_deadline.is_some() || self.exit_code.is_some() {
            return;
        }
        let _ = killpg(self.child, Signal::SIGHUP);
        let _ = kill(self.child, Signal::SIGHUP);
        let _ = killpg(self.child, Signal::SIGCONT);
        self.kill_deadline = Some(Instant::now() + KILL_GRACE);
    }

    /// Recreate the socket if something (e.g. a /tmp cleaner) removed it,
    /// and keep its timestamps fresh so age-based cleaners leave it alone.
    fn health_check(&mut self) {
        if self.last_health.elapsed() < HEALTH_INTERVAL {
            return;
        }
        self.last_health = Instant::now();
        match fs::symlink_metadata(&self.sock_path) {
            Ok(m) if m.ino() == self.sock_ino => {}
            Ok(_) => {} // Someone else owns the path now; leave it alone.
            Err(_) => {
                if paths::ensure_socket_dir().is_ok()
                    && let Ok(l) = bind(&self.sock_path)
                    && let Ok(m) = fs::symlink_metadata(&self.sock_path)
                {
                    self.listener = l;
                    self.sock_ino = m.ino();
                }
            }
        }
        if self.last_touch.elapsed() >= TOUCH_INTERVAL {
            self.last_touch = Instant::now();
            for p in [
                self.dir.as_path(),
                self.sock_path.as_path(),
                self.lock_path.as_path(),
            ] {
                touch(p);
            }
        }
    }

    fn finish(&mut self) -> Result<()> {
        let code = self.exit_code.unwrap_or(0);
        let reset = self.screen.reset_sequence();
        for c in &mut self.conns {
            if c.attached {
                c.queue(&Msg::Output(reset.clone()));
                c.queue(&Msg::Exited { code });
            }
            let _ = c.sock.set_nonblocking(false);
            let _ = c.sock.set_write_timeout(Some(Duration::from_secs(2)));
            let _ = c.sock.write_all(&c.out);
        }
        self.cleanup();
        Ok(())
    }

    fn cleanup(&self) {
        if fs::symlink_metadata(&self.sock_path).is_ok_and(|m| m.ino() == self.sock_ino) {
            let _ = fs::remove_file(&self.sock_path);
        }
        if let Some(link) = &self.agent_link {
            let _ = fs::remove_file(link);
        }
        let _ = fs::remove_file(paths::log_path(&self.dir, &self.name));
        let _ = fs::remove_file(&self.lock_path);
    }
}

/// Close descriptors (other than stdio) inherited from whatever ran
/// `rterm`; a long-lived daemon holding, say, the write end of a pipe would
/// keep its reader waiting forever.
fn close_inherited_fds() {
    let fds: Vec<i32> = match fs::read_dir("/dev/fd") {
        Ok(dir) => dir
            .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
            .collect(),
        Err(_) => return,
    };
    for fd in fds.into_iter().filter(|&fd| fd > 2) {
        unsafe { libc::close(fd) };
    }
}

fn bind(path: &Path) -> Result<Listener> {
    let listener = Listener::bind(path).with_context(|| format!("binding {}", path.display()))?;
    listener.set_nonblocking(true)?;
    sys::set_cloexec(listener.as_fd())?;
    Ok(listener)
}

/// Atomically point `link` at `target`.
fn point_link(link: &Path, target: &Path) -> io::Result<()> {
    let tmp = link.with_extension(format!("agent.{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    std::os::unix::fs::symlink(target, &tmp)?;
    fs::rename(&tmp, link)
}

fn touch(path: &Path) {
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
        return;
    };
    unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            c.as_ptr(),
            std::ptr::null(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
}
