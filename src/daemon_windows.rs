//! The per-session server process on Windows.
//!
//! It plays the same part as daemon.rs: one process owns one session,
//! outlives the client that started it, and relays bytes between the
//! session and whichever client is attached. The program runs on a ConPTY,
//! and because Windows can't wait on pipes, sockets and processes together,
//! the event loop is a set of threads feeding one channel.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::DaemonArgs;
use crate::ipc::{Listener, Stream};
use crate::paths;
use crate::protocol::{self, FrameReader, Msg, SessionInfo, WinSize};
use crate::screen::{Screen, clamp_size};
use crate::winsys::{self, PseudoConsole};

/// Pause reading the session while this much output waits for the client.
const CLIENT_HIGH_WATER: usize = 4 << 20;
const KILL_GRACE: Duration = Duration::from_secs(3);
/// After the program exits, how long to wait for its last output.
const EXIT_DRAIN: Duration = Duration::from_secs(2);

enum Event {
    Output(Vec<u8>),
    OutputClosed,
    Exited(i32),
    Connected(Stream),
    Message(u64, Msg),
    Disconnected(u64),
    KillTimeout,
    DrainTimeout,
}

enum Outgoing {
    Data(Vec<u8>),
    Close,
}

struct Conn {
    tx: Sender<Outgoing>,
    pending: Arc<AtomicUsize>,
    attached: bool,
}

impl Conn {
    fn send(&self, msg: &Msg) {
        let data = msg.encode();
        self.pending.fetch_add(data.len(), Ordering::SeqCst);
        let _ = self.tx.send(Outgoing::Data(data));
    }

    fn close(&self) {
        let _ = self.tx.send(Outgoing::Close);
    }
}

/// ConPTY asks the hosting terminal for "win32-input-mode" (CSI ? 9001 h).
/// Clients send ordinary VT input, which ConPTY understands too, so that
/// request isn't passed on: a terminal honoring it would encode keys in a
/// form only ConPTY reads, and the detach key could no longer be spotted.
#[derive(Default)]
struct ConptyFilter {
    carry: Vec<u8>,
}

impl ConptyFilter {
    const DROP: [&'static [u8]; 2] = [b"\x1b[?9001h", b"\x1b[?9001l"];

    fn filter(&mut self, data: &[u8]) -> Vec<u8> {
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend_from_slice(data);
        let mut out = Vec::with_capacity(buf.len());
        let mut i = 0;
        while i < buf.len() {
            if buf[i] == 0x1b {
                let rest = &buf[i..];
                if let Some(seq) = Self::DROP.iter().find(|s| rest.starts_with(s)) {
                    i += seq.len();
                    continue;
                }
                if Self::DROP.iter().any(|s| s.starts_with(rest)) {
                    self.carry = rest.to_vec(); // maybe the start of one; wait
                    break;
                }
            }
            out.push(buf[i]);
            i += 1;
        }
        out
    }
}

struct Daemon {
    name: String,
    dir: PathBuf,
    sock_path: PathBuf,
    lock_path: PathBuf,
    lock: Option<File>,

    events: Receiver<Event>,
    events_tx: Sender<Event>,
    console: Option<PseudoConsole>,
    pty_tx: Sender<Vec<u8>>,
    job: std::os::windows::io::OwnedHandle,
    pid: u32,
    exit_code: Option<i32>,
    output_closed: bool,
    killing: bool,
    /// The attached client's backlog, watched by the output reader.
    throttle: Arc<Mutex<Option<Arc<AtomicUsize>>>>,

    screen: Screen,
    filter: ConptyFilter,
    size: WinSize,
    conns: HashMap<u64, Conn>,
    next_id: u64,

    created: u64,
    command: String,
}

/// Entry point of `rterm __daemon`. Reports "ok" (or an error) on stdout
/// once the session is ready, then serves until the session ends.
pub fn run(args: DaemonArgs) -> Result<()> {
    let daemon = match Daemon::start(args) {
        Ok(d) => d,
        Err(e) => {
            println!("error: {e:#}");
            return Ok(());
        }
    };
    println!("ok");
    io::stdout().flush().ok();
    winsys::detach_stdout();
    daemon.serve()
}

impl Daemon {
    fn start(args: DaemonArgs) -> Result<Daemon> {
        let dir = paths::ensure_socket_dir()?;
        let lock_path = paths::lock_path(&dir, &args.name);
        let lock = match winsys::lock_file(&lock_path) {
            Ok(f) => f,
            Err(e) if winsys::is_sharing_violation(&e) => {
                bail!("session {:?} already exists", args.name)
            }
            Err(e) => return Err(e).context("locking session"),
        };
        // We hold the lock, so any socket at this path is stale.
        let sock_path = paths::socket_path(&dir, &args.name);
        let _ = fs::remove_file(&sock_path);
        let listener = Listener::bind(&sock_path)
            .with_context(|| format!("binding {}", sock_path.display()))?;

        let (rows, cols) = clamp_size(args.size.rows, args.size.cols);
        let size = WinSize {
            rows,
            cols,
            ..args.size
        };
        let argv = if args.command.is_empty() {
            winsys::default_shell()
        } else {
            args.command
        };
        let command = display_command(&argv);
        let cwd = std::env::current_dir().unwrap_or_else(|_| dir.clone());
        let env: [(&str, Option<OsString>); 4] = [
            ("RTERM_SESSION", Some(args.name.clone().into())),
            // The session's terminal is rterm; see sys::spawn_on_pty.
            ("TMUX", None),
            ("TMUX_PANE", None),
            ("STY", None),
        ];
        let session = winsys::spawn_session(&argv, size, &cwd, &env)
            .with_context(|| format!("starting {command}"))?;
        // Don't keep the user's directory busy.
        let _ = std::env::set_current_dir(&dir);

        let (events_tx, events) = mpsc::channel();
        let throttle: Arc<Mutex<Option<Arc<AtomicUsize>>>> = Arc::default();

        // Session output.
        let mut output = session.output;
        let tx = events_tx.clone();
        let watch = throttle.clone();
        thread::spawn(move || {
            let mut buf = vec![0u8; 64 << 10];
            loop {
                // Backpressure: don't run ahead of a slow client.
                while watch
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|p| p.load(Ordering::SeqCst) > CLIENT_HIGH_WATER)
                {
                    thread::sleep(Duration::from_millis(10));
                }
                match output.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(Event::Output(buf[..n].to_vec())).is_err() {
                            return;
                        }
                    }
                }
            }
            let _ = tx.send(Event::OutputClosed);
        });

        // Keystrokes into the session (the pipe can block).
        let (pty_tx, pty_rx) = mpsc::channel::<Vec<u8>>();
        let mut input = session.input;
        thread::spawn(move || {
            for data in pty_rx {
                if input.write_all(&data).is_err() {
                    return;
                }
            }
        });

        // The program's exit.
        let process = session.process;
        let tx = events_tx.clone();
        thread::spawn(move || {
            let code = winsys::wait_process(&process);
            let _ = tx.send(Event::Exited(code));
        });

        // New clients.
        let tx = events_tx.clone();
        thread::spawn(move || {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if tx.send(Event::Connected(stream)).is_err() {
                            return;
                        }
                    }
                    Err(_) => thread::sleep(Duration::from_millis(100)),
                }
            }
        });

        Ok(Daemon {
            name: args.name,
            dir,
            sock_path,
            lock_path,
            lock: Some(lock),
            events,
            events_tx,
            console: Some(session.console),
            pty_tx,
            job: session.job,
            pid: session.pid,
            exit_code: None,
            output_closed: false,
            killing: false,
            throttle,
            screen: Screen::new(size.rows, size.cols, args.scrollback),
            filter: ConptyFilter::default(),
            size,
            conns: HashMap::new(),
            next_id: 0,
            created: crate::util::now_unix(),
            command,
        })
    }

    fn serve(mut self) -> Result<()> {
        loop {
            let Ok(event) = self.events.recv() else {
                return Ok(());
            };
            match event {
                Event::Output(data) => self.on_output(&data),
                Event::OutputClosed => {
                    self.output_closed = true;
                    if self.exit_code.is_some() {
                        self.finish();
                    }
                }
                Event::DrainTimeout => self.finish(),
                Event::Exited(code) => {
                    self.exit_code = Some(code);
                    if self.output_closed {
                        self.finish();
                    }
                    // Closing the pseudo console flushes its last output and
                    // then closes the output pipe (OutputClosed).
                    self.close_console();
                    self.after(EXIT_DRAIN, Event::DrainTimeout);
                }
                Event::Connected(stream) => self.add_conn(stream),
                Event::Message(id, msg) => self.process(id, msg),
                Event::Disconnected(id) => self.remove_conn(id),
                Event::KillTimeout => {
                    if self.exit_code.is_none() {
                        winsys::terminate_job(&self.job);
                    }
                }
            }
        }
    }

    fn after(&self, delay: Duration, event: Event) {
        let tx = self.events_tx.clone();
        thread::spawn(move || {
            thread::sleep(delay);
            let _ = tx.send(event);
        });
    }

    fn close_console(&mut self) {
        if let Some(console) = self.console.take() {
            thread::spawn(move || console.close());
        }
    }

    fn attached(&self) -> Option<&Conn> {
        self.conns.values().find(|c| c.attached)
    }

    fn on_output(&mut self, data: &[u8]) {
        let data = self.filter.filter(data);
        if data.is_empty() {
            return;
        }
        self.screen.feed(&data);
        let replies = self.screen.take_replies();
        match self.attached() {
            Some(c) => c.send(&Msg::Output(data)),
            // Nobody attached: answer terminal queries ourselves so programs
            // that wait for a reply don't hang.
            None => {
                if !replies.is_empty() {
                    let _ = self.pty_tx.send(replies);
                }
            }
        }
    }

    fn add_conn(&mut self, stream: Stream) {
        let id = self.next_id;
        self.next_id += 1;
        let (Ok(mut reader), Ok(mut writer)) = (stream.try_clone(), stream.try_clone()) else {
            return;
        };
        let pending = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = mpsc::channel::<Outgoing>();

        let events = self.events_tx.clone();
        let backlog = pending.clone();
        thread::spawn(move || {
            for out in rx {
                match out {
                    Outgoing::Data(data) => {
                        let ok = writer.write_all(&data).is_ok();
                        backlog.fetch_sub(data.len(), Ordering::SeqCst);
                        if !ok {
                            break;
                        }
                    }
                    Outgoing::Close => break,
                }
            }
            let _ = writer.shutdown(Shutdown::Both);
            let _ = events.send(Event::Disconnected(id));
        });

        let events = self.events_tx.clone();
        thread::spawn(move || {
            let mut frames = FrameReader::default();
            let mut buf = vec![0u8; 64 << 10];
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                frames.push(&buf[..n]);
                loop {
                    match frames.next() {
                        Ok(Some(msg)) => {
                            if events.send(Event::Message(id, msg)).is_err() {
                                return;
                            }
                        }
                        Ok(None) => break,
                        Err(_) => {
                            let _ = events.send(Event::Disconnected(id));
                            return;
                        }
                    }
                }
            }
            let _ = events.send(Event::Disconnected(id));
        });

        self.conns.insert(
            id,
            Conn {
                tx,
                pending,
                attached: false,
            },
        );
        drop(stream);
    }

    fn remove_conn(&mut self, id: u64) {
        if let Some(c) = self.conns.remove(&id) {
            c.close();
            if c.attached {
                *self.throttle.lock().unwrap() = None;
            }
        }
    }

    fn reply_and_close(&mut self, id: u64, msg: &Msg) {
        if let Some(c) = self.conns.get(&id) {
            c.send(msg);
            c.close();
        }
    }

    fn process(&mut self, id: u64, msg: Msg) {
        let attached = self.conns.get(&id).is_some_and(|c| c.attached);
        match msg {
            Msg::Attach { version, size, .. } => {
                if version != protocol::VERSION {
                    self.reply_and_close(
                        id,
                        &Msg::Incompatible {
                            version: protocol::VERSION,
                        },
                    );
                } else {
                    self.attach(id, size);
                }
            }
            Msg::Input(data) if attached => {
                let _ = self.pty_tx.send(data);
            }
            Msg::Resize(size) if attached => self.resize(size),
            Msg::Detach if attached => self.detach(id, "detached"),
            Msg::Detach => {
                if let Some(c) = self.conns.get(&id) {
                    c.close();
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
                self.reply_and_close(id, &reply);
            }
            Msg::DetachClient => {
                let attached_id = self.conns.iter().find(|(_, c)| c.attached).map(|(&i, _)| i);
                if let Some(j) = attached_id {
                    self.detach(j, "detached by `rterm detach`");
                }
                self.reply_and_close(id, &Msg::Ok);
            }
            Msg::Kill => {
                self.reply_and_close(id, &Msg::Ok);
                self.kill_session();
            }
            Msg::Input(_) | Msg::Resize(_) => {}
            // Daemon-to-client messages are not valid here.
            _ => self.remove_conn(id),
        }
    }

    fn attach(&mut self, id: u64, size: WinSize) {
        let others: Vec<u64> = self
            .conns
            .iter()
            .filter(|(i, c)| **i != id && c.attached)
            .map(|(i, _)| *i)
            .collect();
        for other in others {
            self.detach(other, "attached from another terminal");
        }
        self.resize(size);
        let snapshot = self.screen.snapshot(usize::MAX);
        // Pending replies were for queries the new terminal will answer.
        self.screen.take_replies();
        if let Some(c) = self.conns.get_mut(&id) {
            c.send(&Msg::Output(snapshot));
            c.attached = true;
            *self.throttle.lock().unwrap() = Some(c.pending.clone());
        }
    }

    fn detach(&mut self, id: u64, reason: &str) {
        let reset = self.screen.reset_sequence();
        if let Some(c) = self.conns.get_mut(&id) {
            c.send(&Msg::Output(reset));
            c.send(&Msg::Detached {
                reason: reason.to_owned(),
                drain_input: false,
            });
            c.close();
            c.attached = false;
            *self.throttle.lock().unwrap() = None;
        }
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
        if let Some(console) = &self.console {
            console.resize(size);
        }
    }

    fn info(&self) -> SessionInfo {
        SessionInfo {
            name: self.name.clone(),
            pid: self.pid,
            created: self.created,
            attached: self.attached().is_some(),
            rows: self.size.rows,
            cols: self.size.cols,
            command: self.command.clone(),
            title: self.screen.title().unwrap_or_default(),
        }
    }

    /// Hang up the session like closing a console window would, then end
    /// whatever is left after a grace period.
    fn kill_session(&mut self) {
        if self.killing || self.exit_code.is_some() {
            return;
        }
        self.killing = true;
        self.close_console();
        self.after(KILL_GRACE, Event::KillTimeout);
    }

    fn finish(&mut self) -> ! {
        let code = self.exit_code.unwrap_or(0);
        let reset = self.screen.reset_sequence();
        for c in self.conns.values() {
            if c.attached {
                c.send(&Msg::Output(reset.clone()));
                c.send(&Msg::Exited { code });
            }
            c.close();
        }
        // Give the writers a moment to deliver the goodbye.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while self
            .conns
            .values()
            .any(|c| c.pending.load(Ordering::SeqCst) > 0)
            && std::time::Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
        self.cleanup();
        std::process::exit(0);
    }

    fn cleanup(&mut self) {
        let _ = fs::remove_file(&self.sock_path);
        drop(self.lock.take());
        let _ = fs::remove_file(&self.lock_path);
        let _ = fs::remove_file(paths::log_path(&self.dir, &self.name));
    }
}

fn display_command(argv: &[OsString]) -> String {
    let mut words = argv.iter().map(|a| a.to_string_lossy().into_owned());
    let program = words
        .next()
        .map(|p| {
            Path::new(&p)
                .file_name()
                .map_or(p.clone(), |f| f.to_string_lossy().into_owned())
        })
        .unwrap_or_default();
    std::iter::once(program)
        .chain(words)
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_win32_input_mode_requests() {
        let mut f = ConptyFilter::default();
        assert_eq!(f.filter(b"a\x1b[?9001h\x1b[?1004hb"), b"a\x1b[?1004hb");
        assert_eq!(f.filter(b"x\x1b[?90"), b"x");
        assert_eq!(f.filter(b"01lmore\x1b[1m"), b"more\x1b[1m");
        assert_eq!(f.filter(b"\x1b[?900"), b"");
        assert_eq!(f.filter(b"0h"), b"\x1b[?9000h");
    }
}
