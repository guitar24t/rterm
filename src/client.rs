//! The `rterm` front end: attach the current terminal to a session, plus the
//! small management commands. Terminal handling and daemon launching are
//! platform specific (client_unix.rs, client_windows.rs).

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use crate::ipc::Stream;
use crate::keys::DetachKey;
use crate::paths;
use crate::protocol::{self, FrameReader, Msg, SessionInfo, WinSize};
use crate::util::now_unix;

#[cfg(unix)]
#[path = "client_unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "client_windows.rs"]
mod platform;

pub struct AttachOptions {
    pub create: bool,
    pub command: Vec<OsString>,
    pub detach_key: Option<DetachKey>,
    pub scrollback: usize,
}

pub enum Connect {
    Connected(Stream),
    Missing,
}

/// Connect to a session's socket, cleaning up after a dead daemon.
pub fn connect(dir: &Path, name: &str) -> Result<Connect> {
    let path = paths::socket_path(dir, name);
    match Stream::connect(&path) {
        Ok(s) => Ok(Connect::Connected(s)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Connect::Missing),
        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
            // Nobody listening. If nobody holds the lock either, the daemon
            // is gone and the socket is stale.
            let lock = paths::lock_path(dir, name);
            if platform::lock_is_free(&lock) {
                let _ = fs::remove_file(&path);
                let _ = fs::remove_file(&lock);
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
    let log = File::create(paths::log_path(dir, name))?;
    let mut args: Vec<OsString> = vec![
        "__daemon".into(),
        "--name".into(),
        name.into(),
        "--rows".into(),
        size.rows.to_string().into(),
        "--cols".into(),
        size.cols.to_string().into(),
        "--xpix".into(),
        size.xpix.to_string().into(),
        "--ypix".into(),
        size.ypix.to_string().into(),
        "--scrollback".into(),
        scrollback.to_string().into(),
    ];
    if !command.is_empty() {
        args.push("--".into());
        args.extend(command.iter().cloned());
    }
    let mut daemon = platform::spawn_daemon(&args, log)?;
    let mut status = String::new();
    daemon.status.read_to_string(&mut status)?;
    let status = status.trim();
    match status {
        "ok" => Ok(()),
        s if s.ends_with("already exists") => Ok(()), // lost a creation race
        "" => {
            (daemon.reap)();
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

/// A session daemon that has been started.
pub struct Spawned {
    /// It writes "ok" (or an error) here once the session is ready.
    pub status: Box<dyn Read>,
    /// Collect the process after it has died.
    pub reap: Box<dyn FnOnce()>,
}

pub fn session_exists(dir: &Path, name: &str) -> Result<bool> {
    Ok(matches!(connect(dir, name)?, Connect::Connected(_)))
}

fn terminal_size() -> WinSize {
    platform::terminal_size().unwrap_or(WinSize {
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
    if !platform::is_terminal() {
        bail!("rterm needs a terminal (stdin and stdout must be a terminal)");
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
    let daemon_pid = platform::peer_pid(&sock);
    let hello = Msg::Attach {
        version: protocol::VERSION,
        size,
        ssh_auth_sock,
    };
    let outcome = platform::run(sock, opts.detach_key, &hello);
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
            Some(pid) => platform::run_daemon_binary(pid, version),
            None => bail!("session {name:?} was started by an rterm speaking protocol v{version}"),
        },
    }
}

pub enum Outcome {
    Detached(String),
    Exited(i32),
    /// The daemon went away without saying goodbye.
    Lost,
    /// Our own terminal went away.
    Hangup,
    Error(String),
    /// The session was started by a different rterm version.
    Incompatible(u32),
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
        println!("{}", sessions_json(&sessions, now_unix()));
        return Ok(());
    }
    if sessions.is_empty() {
        println!("no sessions");
        return Ok(());
    }
    let now = now_unix();
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
