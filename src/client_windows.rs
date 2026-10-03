//! Windows side of the client: the console in raw VT mode, a thread-based
//! relay (Windows can't poll a console, a socket and timers together), and
//! launching session daemons outside the caller's job object.

use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Write};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use windows_sys::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS,
};

use super::Outcome;
use crate::ipc::Stream;
use crate::keys::{DetachKey, KeyScanner};
use crate::protocol::{FrameReader, Msg, WinSize};
use crate::winsys;

/// How often to check whether the console window was resized (Windows has
/// no SIGWINCH).
const RESIZE_POLL: Duration = Duration::from_millis(200);

pub fn is_terminal() -> bool {
    winsys::is_console()
}

pub fn terminal_size() -> Option<WinSize> {
    winsys::console_size()
}

/// True if no daemon holds this session's lock (so its socket is stale).
pub fn lock_is_free(lock: &Path) -> bool {
    match winsys::lock_file(lock) {
        Ok(_) => true,
        Err(e) => !winsys::is_sharing_violation(&e),
    }
}

pub fn peer_pid(_sock: &Stream) -> Option<i32> {
    None
}

pub fn run_daemon_binary(_pid: i32, version: u32) -> Result<i32> {
    bail!("session was started by an rterm speaking protocol v{version}; use that rterm to attach")
}

/// Start `rterm __daemon ...` with no console and outside the caller's job:
/// Windows OpenSSH puts each connection in a job that is killed when the
/// connection closes, but allows processes to break away from it.
pub fn spawn_daemon(args: &[OsString], log: File) -> Result<Child> {
    let dir = crate::paths::ensure_socket_dir()?;
    let exe = winsys::session_exe(&dir).context("preparing the session executable")?;
    let spawn = |flags: u32| -> std::io::Result<Child> {
        Command::new(&exe)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(log.try_clone()?)
            .creation_flags(flags)
            .spawn()
    };
    let detached = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
    match spawn(detached | CREATE_BREAKAWAY_FROM_JOB) {
        Ok(child) => Ok(child),
        // ERROR_ACCESS_DENIED: our job doesn't allow breaking away. The
        // session then lives only as long as that job does.
        Err(e) if e.raw_os_error() == Some(5) => spawn(detached).context("starting session daemon"),
        Err(e) => Err(e).context("starting session daemon"),
    }
}

enum Event {
    Input(Vec<u8>),
    InputClosed,
    Server(Msg),
    ServerClosed,
    Resize(WinSize),
}

/// Relay between this console and an attached session until it ends.
pub fn run(sock: Stream, detach_key: Option<DetachKey>, hello: &Msg) -> Result<Outcome> {
    let mut console = winsys::Console::enter_raw().context("setting up the console")?;
    winsys::ignore_interrupts();
    let (tx, rx) = mpsc::channel();

    let mut keyboard = console.reader();
    let keys_tx = tx.clone();
    thread::spawn(move || {
        loop {
            match keyboard.read() {
                Ok(bytes) if bytes.is_empty() => {}
                Ok(bytes) => {
                    if keys_tx.send(Event::Input(bytes)).is_err() {
                        return;
                    }
                }
                Err(_) => {
                    let _ = keys_tx.send(Event::InputClosed);
                    return;
                }
            }
        }
    });

    let mut incoming = sock.try_clone().context("cloning the session socket")?;
    let server_tx = tx.clone();
    thread::spawn(move || {
        let mut frames = FrameReader::default();
        let mut buf = vec![0u8; 64 << 10];
        loop {
            let n = match incoming.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            frames.push(&buf[..n]);
            loop {
                match frames.next() {
                    Ok(Some(msg)) => {
                        if server_tx.send(Event::Server(msg)).is_err() {
                            return;
                        }
                    }
                    Ok(None) => break,
                    Err(_) => {
                        let _ = server_tx.send(Event::ServerClosed);
                        return;
                    }
                }
            }
        }
        let _ = server_tx.send(Event::ServerClosed);
    });

    let size_tx = tx;
    thread::spawn(move || {
        let mut last = winsys::console_size();
        loop {
            thread::sleep(RESIZE_POLL);
            let now = winsys::console_size();
            if now != last {
                if let Some(size) = now
                    && size_tx.send(Event::Resize(size)).is_err()
                {
                    return;
                }
                last = now;
            }
        }
    });

    let mut sock = sock;
    let mut send = |msg: &Msg| sock.write_all(&msg.encode()).is_ok();
    if !send(hello) {
        return Ok(Outcome::Lost);
    }
    let mut keys = detach_key.map(KeyScanner::new);
    let mut detaching = false;
    loop {
        let event = if detaching {
            // Once a detach was requested, don't wait forever for the daemon.
            match rx.recv_timeout(Duration::from_secs(2)) {
                Ok(e) => e,
                Err(_) => return Ok(Outcome::Detached("detached".into())),
            }
        } else {
            match rx.recv() {
                Ok(e) => e,
                Err(_) => return Ok(Outcome::Lost),
            }
        };
        match event {
            Event::Input(data) if !detaching => {
                let sent = match keys.as_mut().and_then(|k| k.scan(&data)) {
                    Some(start) => {
                        detaching = true;
                        (start == 0 || send(&Msg::Input(data[..start].to_vec())))
                            && send(&Msg::Detach)
                    }
                    None => send(&Msg::Input(data)),
                };
                if !sent {
                    return Ok(Outcome::Lost);
                }
            }
            Event::Input(_) => {}
            Event::InputClosed => return Ok(Outcome::Hangup),
            Event::Server(msg) => match msg {
                Msg::Output(data) => console.write(&data)?,
                Msg::Detached { reason, .. } => return Ok(Outcome::Detached(reason)),
                Msg::Exited { code } => return Ok(Outcome::Exited(code)),
                Msg::Error(e) => return Ok(Outcome::Error(e)),
                Msg::Incompatible { version } => return Ok(Outcome::Incompatible(version)),
                _ => {}
            },
            Event::ServerClosed => return Ok(Outcome::Lost),
            Event::Resize(size) => {
                if !send(&Msg::Resize(size)) {
                    return Ok(Outcome::Lost);
                }
            }
        }
    }
}
