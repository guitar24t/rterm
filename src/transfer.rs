//! File transfer between a session and the user's computer.
//!
//! `rterm get` (session → computer) and `rterm put` (computer → session) run
//! inside a session. The other end is the transfer endpoint that
//! rterm-connect starts on the user's computer: it reaches the session's
//! daemon over a second channel of the same ssh connection (`rterm
//! __bridge`) and registers with `AgentHello`. The daemon then routes
//! `Transfer` frames between the two without looking inside them, so file
//! data never travels through the terminal.
//!
//! Safety: the session side is driven by the server, so the computer side
//! only writes into the download folder unless the user confirms, and it
//! never reads a local file for `put` without the user confirming in a
//! native dialog. Received names are sanitized, existing files are never
//! overwritten without `-f`, and data lands in `.part` files that are only
//! renamed into place once complete.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use crate::ipc::Stream;
use crate::paths;
use crate::protocol::{self, Dec, Enc, FrameReader, Msg};

const CHUNK: usize = 256 << 10;
/// Unacknowledged bytes the sender allows in flight.
const WINDOW: u64 = 8 << 20;
/// How long the session side waits for the user to answer a dialog.
const DECISION_TIMEOUT: Duration = Duration::from_secs(150);
const STALL_TIMEOUT: Duration = Duration::from_secs(120);

/// Messages within one transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tx {
    /// Session → computer: the session will send these items.
    GetRequest {
        items: Vec<String>,
        dest: Option<String>,
        force: bool,
    },
    /// Session → computer: send these local files into `dest` on the server.
    PutRequest {
        sources: Vec<String>,
        home: String,
        dest: String,
        force: bool,
    },
    Accepted {
        dest: String,
    },
    Refused {
        reason: String,
    },
    Total {
        files: u64,
        bytes: u64,
    },
    /// A top-level file or directory follows.
    Item {
        name: String,
    },
    /// A directory inside the current item ("" is the item itself).
    Dir {
        rel: String,
    },
    File {
        rel: String,
        size: u64,
        executable: bool,
    },
    Data(Vec<u8>),
    FileEnd,
    ItemEnd,
    /// Receiver: total bytes written so far.
    Ack {
        bytes: u64,
    },
    /// Receiver: where the last item ended up.
    Saved {
        path: String,
        files: u64,
        bytes: u64,
    },
    /// Sender: no more items.
    Finish,
    /// Receiver: everything is saved.
    Done,
    Failed {
        message: String,
    },
}

mod tag {
    pub const GET_REQUEST: u8 = 1;
    pub const PUT_REQUEST: u8 = 2;
    pub const ACCEPTED: u8 = 3;
    pub const REFUSED: u8 = 4;
    pub const TOTAL: u8 = 5;
    pub const ITEM: u8 = 6;
    pub const DIR: u8 = 7;
    pub const FILE: u8 = 8;
    pub const DATA: u8 = 9;
    pub const FILE_END: u8 = 10;
    pub const ITEM_END: u8 = 11;
    pub const ACK: u8 = 12;
    pub const SAVED: u8 = 13;
    pub const FINISH: u8 = 14;
    pub const DONE: u8 = 15;
    pub const FAILED: u8 = 16;
}

impl Tx {
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc(Vec::new());
        let list = |e: &mut Enc, items: &[String]| {
            e.u32(items.len() as u32);
            for i in items {
                e.str(i);
            }
        };
        let t = match self {
            Tx::GetRequest { items, dest, force } => {
                list(&mut e, items);
                e.str(dest.as_deref().unwrap_or("")).u16(*force as u16);
                tag::GET_REQUEST
            }
            Tx::PutRequest {
                sources,
                home,
                dest,
                force,
            } => {
                list(&mut e, sources);
                e.str(home).str(dest).u16(*force as u16);
                tag::PUT_REQUEST
            }
            Tx::Accepted { dest } => {
                e.str(dest);
                tag::ACCEPTED
            }
            Tx::Refused { reason } => {
                e.str(reason);
                tag::REFUSED
            }
            Tx::Total { files, bytes } => {
                e.u64(*files).u64(*bytes);
                tag::TOTAL
            }
            Tx::Item { name } => {
                e.str(name);
                tag::ITEM
            }
            Tx::Dir { rel } => {
                e.str(rel);
                tag::DIR
            }
            Tx::File {
                rel,
                size,
                executable,
            } => {
                e.str(rel).u64(*size).u16(*executable as u16);
                tag::FILE
            }
            Tx::Data(bytes) => {
                e.0.extend_from_slice(bytes);
                tag::DATA
            }
            Tx::FileEnd => tag::FILE_END,
            Tx::ItemEnd => tag::ITEM_END,
            Tx::Ack { bytes } => {
                e.u64(*bytes);
                tag::ACK
            }
            Tx::Saved { path, files, bytes } => {
                e.str(path).u64(*files).u64(*bytes);
                tag::SAVED
            }
            Tx::Finish => tag::FINISH,
            Tx::Done => tag::DONE,
            Tx::Failed { message } => {
                e.str(message);
                tag::FAILED
            }
        };
        let mut out = vec![t];
        out.extend_from_slice(&e.0);
        out
    }

    pub fn decode(data: &[u8]) -> Result<Tx> {
        let (&t, rest) = data
            .split_first()
            .ok_or_else(|| anyhow!("empty transfer message"))?;
        let mut d = Dec(rest);
        let list = |d: &mut Dec| -> Result<Vec<String>> {
            let n = d.u32()?;
            (0..n).map(|_| d.str()).collect()
        };
        Ok(match t {
            tag::GET_REQUEST => {
                let items = list(&mut d)?;
                let dest = d.str()?;
                Tx::GetRequest {
                    items,
                    dest: (!dest.is_empty()).then_some(dest),
                    force: d.u16()? != 0,
                }
            }
            tag::PUT_REQUEST => Tx::PutRequest {
                sources: list(&mut d)?,
                home: d.str()?,
                dest: d.str()?,
                force: d.u16()? != 0,
            },
            tag::ACCEPTED => Tx::Accepted { dest: d.str()? },
            tag::REFUSED => Tx::Refused { reason: d.str()? },
            tag::TOTAL => Tx::Total {
                files: d.u64()?,
                bytes: d.u64()?,
            },
            tag::ITEM => Tx::Item { name: d.str()? },
            tag::DIR => Tx::Dir { rel: d.str()? },
            tag::FILE => Tx::File {
                rel: d.str()?,
                size: d.u64()?,
                executable: d.u16()? != 0,
            },
            tag::DATA => Tx::Data(rest.to_vec()),
            tag::FILE_END => Tx::FileEnd,
            tag::ITEM_END => Tx::ItemEnd,
            tag::ACK => Tx::Ack { bytes: d.u64()? },
            tag::SAVED => Tx::Saved {
                path: d.str()?,
                files: d.u64()?,
                bytes: d.u64()?,
            },
            tag::FINISH => Tx::Finish,
            tag::DONE => Tx::Done,
            tag::FAILED => Tx::Failed { message: d.str()? },
            other => bail!("unknown transfer message {other}"),
        })
    }
}

/// One end's view of a transfer.
pub trait Link {
    fn send(&mut self, tx: &Tx) -> Result<()>;
    /// The next message; Ok(None) if `timeout` passes first.
    fn recv(&mut self, timeout: Duration) -> Result<Option<Tx>>;

    fn expect(&mut self, timeout: Duration, waiting_for: &str) -> Result<Tx> {
        self.recv(timeout)?
            .ok_or_else(|| anyhow!("timed out waiting for {waiting_for}"))
    }
}

// ---------------------------------------------------------------------------
// Routing inside the session daemon.

/// What the daemon should do with its connections, named by the ids it
/// assigns them.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Send(u64, Msg),
    /// Close the connection once what's queued for it has been sent.
    Close(u64),
}

const NO_ENDPOINT: &str = "file transfer needs your computer's side of the connection: \
    reconnect to this session with rterm-connect, with Rob Terminal installed on your computer";

/// Pairs each transfer's session-side connection (`rterm get`/`put`) with
/// the endpoint on the user's computer. The daemon feeds it transfer
/// messages and disconnects and carries out the actions it returns.
#[derive(Default)]
pub struct Router {
    agent: Option<u64>,
    /// Transfer id → the session-side connection.
    transfers: HashMap<u64, u64>,
}

impl Router {
    pub fn message(&mut self, from: u64, msg: Msg) -> Vec<Action> {
        let mut actions = Vec::new();
        match msg {
            Msg::AgentHello { version } => {
                if version != protocol::VERSION {
                    actions.push(Action::Send(
                        from,
                        Msg::Incompatible {
                            version: protocol::VERSION,
                        },
                    ));
                    actions.push(Action::Close(from));
                    return actions;
                }
                // The newest endpoint wins; an older one is likely a dead
                // connection that hasn't timed out yet.
                if let Some(old) = self.agent.replace(from)
                    && old != from
                {
                    for (id, session) in self.transfers.drain() {
                        actions.push(Action::Send(session, Msg::TransferEnd { id }));
                    }
                    actions.push(Action::Close(old));
                }
                actions.push(Action::Send(from, Msg::Ok));
            }
            Msg::TransferBegin { id, payload } => match self.agent {
                Some(agent) if agent != from && !self.transfers.contains_key(&id) => {
                    self.transfers.insert(id, from);
                    actions.push(Action::Send(agent, Msg::TransferBegin { id, payload }));
                }
                Some(_) => actions.push(Action::Send(from, Msg::Error("transfer refused".into()))),
                None => {
                    actions.push(Action::Send(from, Msg::Error(NO_ENDPOINT.into())));
                    actions.push(Action::Close(from));
                }
            },
            Msg::Transfer { id, payload } => {
                if let Some(to) = self.peer(from, id) {
                    actions.push(Action::Send(to, Msg::Transfer { id, payload }));
                }
            }
            Msg::TransferEnd { id } => {
                if let Some(to) = self.peer(from, id) {
                    self.transfers.remove(&id);
                    actions.push(Action::Send(to, Msg::TransferEnd { id }));
                }
            }
            _ => {}
        }
        actions
    }

    /// The other end of transfer `id`, if `from` is one end.
    fn peer(&self, from: u64, id: u64) -> Option<u64> {
        let session = *self.transfers.get(&id)?;
        if Some(from) == self.agent {
            Some(session)
        } else if from == session {
            self.agent
        } else {
            None
        }
    }

    pub fn disconnected(&mut self, conn: u64) -> Vec<Action> {
        let mut actions = Vec::new();
        if self.agent == Some(conn) {
            self.agent = None;
            for (id, session) in self.transfers.drain() {
                actions.push(Action::Send(session, Msg::TransferEnd { id }));
            }
        } else {
            let ended: Vec<u64> = self
                .transfers
                .iter()
                .filter(|&(_, &s)| s == conn)
                .map(|(&id, _)| id)
                .collect();
            for id in ended {
                self.transfers.remove(&id);
                if let Some(agent) = self.agent {
                    actions.push(Action::Send(agent, Msg::TransferEnd { id }));
                }
            }
        }
        actions
    }
}

// ---------------------------------------------------------------------------
// Sending and receiving files (used by both ends).

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    pub path: String,
    pub files: u64,
    pub bytes: u64,
}

/// What a sender will send: top-level paths with the names they get.
pub struct Sources {
    items: Vec<(PathBuf, String)>,
    pub files: u64,
    pub bytes: u64,
}

impl Sources {
    pub fn scan(paths: &[PathBuf]) -> Result<Sources> {
        let mut items = Vec::new();
        let (mut files, mut bytes) = (0, 0);
        for path in paths {
            let meta = fs::metadata(path).with_context(|| format!("{}", path.display()))?;
            let name = path
                .canonicalize()
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .or_else(|| path.file_name().map(|n| n.to_string_lossy().into_owned()))
                .ok_or_else(|| anyhow!("{} has no name to copy it under", path.display()))?;
            if meta.is_dir() {
                walk(path, &mut |_, m| {
                    if m.is_file() {
                        files += 1;
                        bytes += m.len();
                    }
                })?;
            } else {
                files += 1;
                bytes += meta.len();
            }
            items.push((path.clone(), name));
        }
        Ok(Sources {
            items,
            files,
            bytes,
        })
    }

    pub fn names(&self) -> Vec<String> {
        self.items.iter().map(|(_, n)| n.clone()).collect()
    }
}

/// Visit everything under `dir` (not following symlinks), parents first,
/// with paths relative to `dir` using '/'.
fn walk(dir: &Path, visit: &mut dyn FnMut(&str, &fs::Metadata)) -> Result<()> {
    fn go(base: &Path, rel: &str, visit: &mut dyn FnMut(&str, &fs::Metadata)) -> Result<()> {
        let dir = if rel.is_empty() {
            base.to_path_buf()
        } else {
            base.join(rel)
        };
        let mut entries: Vec<_> = fs::read_dir(&dir)
            .with_context(|| format!("reading {}", dir.display()))?
            .filter_map(|e| e.ok())
            .collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let meta = entry.metadata()?; // does not follow symlinks
            if meta.file_type().is_symlink() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let child = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            visit(&child, &meta);
            if meta.is_dir() {
                go(base, &child, visit)?;
            }
        }
        Ok(())
    }
    go(dir, "", visit)
}

/// Shows transfer progress on stderr when it's a terminal.
pub struct Progress {
    enabled: bool,
    arrow: &'static str,
    total: u64,
    done: u64,
    name: String,
    started: Instant,
    last_draw: Option<Instant>,
}

impl Progress {
    pub fn new(enabled: bool, arrow: &'static str) -> Progress {
        Progress {
            enabled,
            arrow,
            total: 0,
            done: 0,
            name: String::new(),
            started: Instant::now(),
            last_draw: None,
        }
    }

    pub fn silent() -> Progress {
        Progress::new(false, "")
    }

    fn advance(&mut self, bytes: u64) {
        self.done += bytes;
        self.draw();
    }

    fn draw(&mut self) {
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        // Quick transfers finish without a progress line flashing by.
        if now - self.started < Duration::from_millis(300) {
            return;
        }
        if self
            .last_draw
            .is_some_and(|t| now - t < Duration::from_millis(100))
        {
            return;
        }
        self.last_draw = Some(now);
        let secs = (now - self.started).as_secs_f64().max(0.001);
        let pct = (self.done * 100).checked_div(self.total).unwrap_or(100);
        eprint!(
            "\r\x1b[K{} {}  {:>3}%  {}  {}/s",
            self.arrow,
            self.name,
            pct.min(100),
            human_size(self.done),
            human_size((self.done as f64 / secs) as u64)
        );
    }

    fn clear(&mut self) {
        if self.enabled && self.last_draw.is_some() {
            eprint!("\r\x1b[K");
            self.last_draw = None;
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        self.clear();
    }
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1000.0 && unit < UNITS.len() - 1 {
        v /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

/// Stream `sources` over `link`; returns where the receiver saved them.
pub fn send(link: &mut dyn Link, sources: &Sources, progress: &mut Progress) -> Result<Vec<Saved>> {
    let mut acked = 0u64;
    let mut sent = 0u64;
    let mut saved = Vec::new();
    progress.total = sources.bytes;
    link.send(&Tx::Total {
        files: sources.files,
        bytes: sources.bytes,
    })?;

    // Handle whatever the receiver has sent; blocks while too much is
    // unacknowledged.
    let pump = |link: &mut dyn Link,
                sent: u64,
                acked: &mut u64,
                saved: &mut Vec<Saved>,
                block: bool|
     -> Result<()> {
        loop {
            let waiting = block && sent - *acked > WINDOW;
            let timeout = if waiting {
                STALL_TIMEOUT
            } else {
                Duration::ZERO
            };
            match link.recv(timeout)? {
                Some(Tx::Ack { bytes }) => *acked = bytes,
                Some(Tx::Saved { path, files, bytes }) => saved.push(Saved { path, files, bytes }),
                Some(Tx::Failed { message }) => bail!("{message}"),
                Some(other) => bail!("unexpected {other:?} during transfer"),
                None if waiting => bail!("the other side stopped responding"),
                None => return Ok(()),
            }
        }
    };

    for (path, name) in &sources.items {
        link.send(&Tx::Item { name: name.clone() })?;
        progress.name = name.clone();
        let meta = fs::metadata(path)?;
        let mut send_file =
            |link: &mut dyn Link, file: &Path, rel: &str, meta: &fs::Metadata| -> Result<()> {
                link.send(&Tx::File {
                    rel: rel.into(),
                    size: meta.len(),
                    executable: is_executable(meta),
                })?;
                let mut f =
                    File::open(file).with_context(|| format!("opening {}", file.display()))?;
                let mut buf = vec![0u8; CHUNK];
                loop {
                    let n = f
                        .read(&mut buf)
                        .with_context(|| format!("reading {}", file.display()))?;
                    if n == 0 {
                        break;
                    }
                    link.send(&Tx::Data(buf[..n].to_vec()))?;
                    sent += n as u64;
                    progress.advance(n as u64);
                    pump(link, sent, &mut acked, &mut saved, true)?;
                }
                link.send(&Tx::FileEnd)
            };
        if meta.is_dir() {
            link.send(&Tx::Dir { rel: String::new() })?;
            let mut entries = Vec::new();
            walk(path, &mut |rel, m| {
                entries.push((rel.to_owned(), m.is_dir(), m.is_file()))
            })?;
            for (rel, is_dir, is_file) in entries {
                if is_dir {
                    link.send(&Tx::Dir { rel })?;
                } else if is_file {
                    let file = path.join(&rel);
                    let meta = fs::metadata(&file)?;
                    send_file(link, &file, &rel, &meta)?;
                }
            }
        } else {
            send_file(link, path, "", &meta)?;
        }
        link.send(&Tx::ItemEnd)?;
        pump(link, sent, &mut acked, &mut saved, false)?;
    }
    link.send(&Tx::Finish)?;
    progress.clear();
    loop {
        match link.expect(STALL_TIMEOUT, "the other side to finish")? {
            Tx::Ack { bytes } => acked = bytes,
            Tx::Saved { path, files, bytes } => saved.push(Saved { path, files, bytes }),
            Tx::Done => return Ok(saved),
            Tx::Failed { message } => bail!("{message}"),
            other => bail!("unexpected {other:?} at the end of the transfer"),
        }
        let _ = acked;
    }
}

#[cfg(unix)]
fn is_executable(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn set_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = fs::metadata(path) {
        let mode = meta.permissions().mode();
        let _ = fs::set_permissions(
            path,
            fs::Permissions::from_mode(mode | ((mode & 0o444) >> 2)),
        );
    }
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) {}

/// Make a received name safe to create on this system: one plain component.
fn safe_component(name: &str) -> Result<String> {
    let mut out: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | '\0' => '_',
            c if cfg!(windows) && (c < ' ' || "<>:\"|?*".contains(c)) => '_',
            c => c,
        })
        .collect();
    if cfg!(windows) {
        out = out.trim_end_matches([' ', '.']).to_owned();
        let stem = out.split('.').next().unwrap_or("").to_ascii_uppercase();
        let reserved = ["CON", "PRN", "AUX", "NUL"].contains(&stem.as_str())
            || ((stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.len() == 4
                && stem.as_bytes()[3].is_ascii_digit());
        if reserved {
            out.insert(0, '_');
        }
    }
    if out.is_empty() || out == "." || out == ".." {
        bail!("refusing the name {name:?}");
    }
    Ok(out)
}

/// A received relative path ("a/b/c") as safe components under a root.
fn safe_relative(rel: &str) -> Result<PathBuf> {
    let mut path = PathBuf::new();
    for part in rel.split('/') {
        path.push(safe_component(part)?);
    }
    Ok(path)
}

/// `dir/name`, or `dir/name (1)` etc. if that exists.
fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if fs::symlink_metadata(&first).is_err() {
        return first;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| fs::symlink_metadata(p).is_err())
        .unwrap()
}

/// Receive items into `dest`. `display` turns saved paths into what the
/// user is shown.
pub fn receive(
    link: &mut dyn Link,
    dest: &Path,
    force: bool,
    display: &dyn Fn(&Path) -> String,
    progress: &mut Progress,
) -> Result<Vec<Saved>> {
    let mut state = Receiving {
        parts: Vec::new(),
        created_roots: Vec::new(),
    };
    let result = state.run(link, dest, force, display, progress);
    if let Err(e) = &result {
        state.clean_up();
        let _ = link.send(&Tx::Failed {
            message: format!("{e:#}"),
        });
    }
    progress.clear();
    result
}

struct Receiving {
    parts: Vec<PathBuf>,
    created_roots: Vec<PathBuf>,
}

impl Receiving {
    fn clean_up(&mut self) {
        for p in self.parts.drain(..) {
            let _ = fs::remove_file(p);
        }
        for d in self.created_roots.drain(..) {
            let _ = fs::remove_dir_all(d);
        }
    }

    fn run(
        &mut self,
        link: &mut dyn Link,
        dest: &Path,
        force: bool,
        display: &dyn Fn(&Path) -> String,
        progress: &mut Progress,
    ) -> Result<Vec<Saved>> {
        let mut saved = Vec::new();
        let mut received = 0u64;
        let mut item: Option<(String, Option<PathBuf>, u64, u64)> = None; // name, root, files, bytes
        let mut file: Option<(File, PathBuf, PathBuf, bool)> = None; // handle, part, final, executable
        loop {
            match link.expect(STALL_TIMEOUT, "data")? {
                Tx::Total { bytes, .. } => progress.total = bytes,
                Tx::Item { name } => {
                    let name = safe_component(&name)?;
                    progress.name = name.clone();
                    item = Some((name, None, 0, 0));
                }
                Tx::Dir { rel } => {
                    let (name, root, _, _) = item
                        .as_mut()
                        .ok_or_else(|| anyhow!("directory outside an item"))?;
                    if rel.is_empty() {
                        let dir = if force {
                            dest.join(&*name)
                        } else {
                            unique_path(dest, name)
                        };
                        if !dir.is_dir() {
                            fs::create_dir(&dir)
                                .with_context(|| format!("creating {}", dir.display()))?;
                            self.created_roots.push(dir.clone());
                        }
                        *root = Some(dir);
                    } else {
                        let root = root
                            .as_ref()
                            .ok_or_else(|| anyhow!("directory entry outside a directory"))?;
                        let dir = root.join(safe_relative(&rel)?);
                        fs::create_dir_all(&dir)
                            .with_context(|| format!("creating {}", dir.display()))?;
                    }
                }
                Tx::File {
                    rel, executable, ..
                } => {
                    let (name, root, _, _) = item
                        .as_mut()
                        .ok_or_else(|| anyhow!("file outside an item"))?;
                    let target = if rel.is_empty() {
                        if force {
                            dest.join(&*name)
                        } else {
                            unique_path(dest, name)
                        }
                    } else {
                        let root = root
                            .as_ref()
                            .ok_or_else(|| anyhow!("file entry outside a directory"))?;
                        let path = root.join(safe_relative(&rel)?);
                        if !force && fs::symlink_metadata(&path).is_ok() {
                            bail!("{} already exists", path.display());
                        }
                        path
                    };
                    let mut part_name = target.file_name().unwrap_or_default().to_os_string();
                    part_name.push(".part");
                    let part = target.with_file_name(part_name);
                    let handle = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&part)
                        .with_context(|| format!("creating {}", part.display()))?;
                    self.parts.push(part.clone());
                    file = Some((handle, part, target, executable));
                }
                Tx::Data(bytes) => {
                    let (handle, part, ..) = file
                        .as_mut()
                        .ok_or_else(|| anyhow!("data outside a file"))?;
                    handle
                        .write_all(&bytes)
                        .with_context(|| format!("writing {}", part.display()))?;
                    received += bytes.len() as u64;
                    progress.advance(bytes.len() as u64);
                    if let Some((_, _, _, b)) = item.as_mut() {
                        *b += bytes.len() as u64;
                    }
                    link.send(&Tx::Ack { bytes: received })?;
                }
                Tx::FileEnd => {
                    let (handle, part, target, executable) =
                        file.take().ok_or_else(|| anyhow!("end of no file"))?;
                    handle.sync_all().ok();
                    drop(handle);
                    if force && target.is_file() {
                        let _ = fs::remove_file(&target); // Windows can't rename over a file
                    }
                    fs::rename(&part, &target)
                        .with_context(|| format!("saving {}", target.display()))?;
                    self.parts.retain(|p| p != &part);
                    if executable {
                        set_executable(&target);
                    }
                    if let Some((_, root, files, _)) = item.as_mut() {
                        *files += 1;
                        if root.is_none() {
                            *root = Some(target);
                        }
                    }
                }
                Tx::ItemEnd => {
                    let (_, root, files, bytes) =
                        item.take().ok_or_else(|| anyhow!("end of no item"))?;
                    let root = root.ok_or_else(|| anyhow!("empty item"))?;
                    self.created_roots.retain(|d| d != &root);
                    let entry = Saved {
                        path: display(&root),
                        files,
                        bytes,
                    };
                    link.send(&Tx::Saved {
                        path: entry.path.clone(),
                        files,
                        bytes,
                    })?;
                    saved.push(entry);
                }
                Tx::Finish => {
                    link.send(&Tx::Done)?;
                    return Ok(saved);
                }
                Tx::Failed { message } => bail!("{message}"),
                other => bail!("unexpected {other:?} while receiving"),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The session side: `rterm get` and `rterm put`.

/// The session-side end of a transfer: frames to and from the daemon.
struct DaemonLink {
    sock: Stream,
    reader: FrameReader,
    id: u64,
    buf: Vec<u8>,
    /// Set by Ctrl-C (and hangups), so a cancelled transfer cleans up.
    interrupted: Arc<AtomicBool>,
}

impl DaemonLink {
    fn send_msg(&mut self, msg: &Msg) -> Result<()> {
        self.sock
            .write_all(&msg.encode())
            .context("talking to the session")
    }
}

impl Link for DaemonLink {
    fn send(&mut self, tx: &Tx) -> Result<()> {
        let msg = Msg::Transfer {
            id: self.id,
            payload: tx.encode(),
        };
        self.send_msg(&msg)
    }

    fn recv(&mut self, timeout: Duration) -> Result<Option<Tx>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(msg) = self.reader.next()? {
                match msg {
                    Msg::Transfer { id, payload } if id == self.id => {
                        return Tx::decode(&payload).map(Some);
                    }
                    Msg::TransferEnd { .. } => bail!("your computer went away during the transfer"),
                    Msg::Error(e) => bail!("{e}"),
                    _ => continue,
                }
            }
            if self.interrupted.load(Ordering::SeqCst) {
                bail!("interrupted");
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                // Poll once without waiting.
                self.sock.set_nonblocking(true)?;
                let r = self.sock.read(&mut self.buf);
                self.sock.set_nonblocking(false)?;
                match r {
                    Ok(0) => bail!(SESSION_GONE),
                    Ok(n) => {
                        self.reader.push(&self.buf[..n]);
                        continue;
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                    Err(e) => return Err(e.into()),
                }
            }
            // Wake up now and then to notice an interrupt.
            let slice = left.min(Duration::from_millis(200));
            self.sock.set_read_timeout(Some(slice))?;
            match self.sock.read(&mut self.buf) {
                Ok(0) => bail!(SESSION_GONE),
                Ok(n) => self.reader.push(&self.buf[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
}

const SESSION_GONE: &str = "the session went away";

fn open_session_link() -> Result<DaemonLink> {
    let name = std::env::var("RTERM_SESSION")
        .ok()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("run this inside an rterm session"))?;
    let dir = paths::socket_dir();
    let sock = Stream::connect(paths::socket_path(&dir, &name))
        .with_context(|| format!("connecting to session {name:?}"))?;
    // Unique among the session's transfers (the daemon pairs ids with
    // connections, so guessing one gets nothing).
    let id = {
        use std::hash::{BuildHasher, Hasher};
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u32(std::process::id());
        h.finish()
    };
    let interrupted = Arc::new(AtomicBool::new(false));
    #[cfg(unix)]
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        let _ = signal_hook::flag::register(sig, interrupted.clone());
    }
    #[cfg(windows)]
    crate::winsys::flag_interrupts(interrupted.clone());
    Ok(DaemonLink {
        sock,
        reader: FrameReader::default(),
        id,
        buf: vec![0u8; 512 << 10],
        interrupted,
    })
}

fn begin(link: &mut DaemonLink, request: &Tx, timeout: Duration) -> Result<String> {
    let msg = Msg::TransferBegin {
        id: link.id,
        payload: request.encode(),
    };
    link.send_msg(&msg)?;
    match link.recv(timeout) {
        Ok(Some(Tx::Accepted { dest })) => Ok(dest),
        Ok(Some(Tx::Refused { reason })) => bail!("{reason}"),
        Ok(Some(other)) => bail!("unexpected answer {other:?}"),
        Ok(None) => bail!("no answer from your computer"),
        // A daemon from before file transfer drops the connection when it
        // sees a message it doesn't know.
        Err(e) if e.to_string() == SESSION_GONE || e.downcast_ref::<io::Error>().is_some() => {
            bail!("this session's rterm is too old for file transfer; start a new session")
        }
        Err(e) => Err(e),
    }
}

fn stderr_is_terminal() -> bool {
    use std::io::IsTerminal;
    io::stderr().is_terminal()
}

/// `rterm get`: copy files from the session to the user's computer.
pub fn get(paths: &[PathBuf], to: Option<String>, force: bool) -> Result<i32> {
    let mut link = open_session_link()?;
    let sources = Sources::scan(paths)?;
    let request = Tx::GetRequest {
        items: sources.names(),
        dest: to,
        force,
    };
    let dest = begin(&mut link, &request, DECISION_TIMEOUT)?;
    let mut progress = Progress::new(stderr_is_terminal(), "⇣");
    let saved = send(&mut link, &sources, &mut progress)?;
    let _ = link.send_msg(&Msg::TransferEnd { id: link.id });
    let width = sources
        .names()
        .iter()
        .map(|n| n.chars().count())
        .max()
        .unwrap_or(0);
    for (name, s) in sources.names().iter().zip(&saved) {
        println!("{:width$}  → {}  ({})", name, s.path, describe(s));
    }
    if saved.is_empty() {
        println!("nothing to copy into {dest}");
    }
    Ok(0)
}

/// `rterm put`: copy files from the user's computer into the session.
pub fn put(paths: &[String], to: Option<PathBuf>, force: bool) -> Result<i32> {
    let cwd = std::env::current_dir().context("finding the current directory")?;
    let dest = match to {
        Some(d) if d.is_absolute() => d,
        Some(d) => cwd.join(d),
        None => cwd.clone(),
    };
    if !dest.is_dir() {
        bail!("{} is not a directory", dest.display());
    }
    let home = home_dir()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut link = open_session_link()?;
    let request = Tx::PutRequest {
        sources: paths.to_vec(),
        home: home.clone(),
        dest: dest.to_string_lossy().into_owned(),
        force,
    };
    if stderr_is_terminal() {
        eprint!("Waiting for your OK on your computer…");
    }
    let accepted = begin(&mut link, &request, DECISION_TIMEOUT);
    if stderr_is_terminal() {
        eprint!("\r\x1b[K");
    }
    accepted?;
    let mut progress = Progress::new(stderr_is_terminal(), "⇡");
    let display = |p: &Path| tilde(p, home_dir().as_deref());
    let saved = receive(&mut link, &dest, force, &display, &mut progress)?;
    let _ = link.send_msg(&Msg::TransferEnd { id: link.id });
    let names: Vec<String> = saved
        .iter()
        .map(|s| {
            Path::new(&s.path)
                .file_name()
                .map_or(s.path.clone(), |n| n.to_string_lossy().into_owned())
        })
        .collect();
    let width = names.iter().map(|n| n.chars().count()).max().unwrap_or(0);
    for (name, s) in names.iter().zip(&saved) {
        println!("{:width$}  → {}  ({})", name, s.path, describe(s));
    }
    Ok(0)
}

fn describe(s: &Saved) -> String {
    if s.files == 1 {
        human_size(s.bytes)
    } else {
        format!("{} files, {}", s.files, human_size(s.bytes))
    }
}

pub fn home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// Show `path` with the home directory as `~` (Unix-style systems).
fn tilde(path: &Path, home: Option<&Path>) -> String {
    if cfg!(not(windows))
        && let Some(home) = home
        && let Ok(rest) = path.strip_prefix(home)
    {
        return if rest.as_os_str().is_empty() {
            "~".into()
        } else {
            format!("~/{}", rest.display())
        };
    }
    path.display().to_string()
}

// ---------------------------------------------------------------------------
// The computer side: the endpoint rterm-connect starts.

pub struct EndpointArgs {
    pub session: String,
    pub host: String,
    pub ssh_args: Vec<String>,
    pub rterm: String,
    pub cwd: PathBuf,
    pub download_dir: Option<PathBuf>,
    pub parent: Option<u32>,
    /// Connect to a local session directly instead of over ssh (tests).
    pub direct: bool,
}

type Writer = Arc<Mutex<Box<dyn Write + Send>>>;

/// The computer-side end of a transfer.
struct EndpointLink {
    id: u64,
    rx: Receiver<Tx>,
    writer: Writer,
}

impl Link for EndpointLink {
    fn send(&mut self, tx: &Tx) -> Result<()> {
        let msg = Msg::Transfer {
            id: self.id,
            payload: tx.encode(),
        };
        self.writer
            .lock()
            .unwrap()
            .write_all(&msg.encode())
            .context("sending to the session")
    }

    fn recv(&mut self, timeout: Duration) -> Result<Option<Tx>> {
        match self.rx.recv_timeout(timeout) {
            Ok(tx) => Ok(Some(tx)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => bail!("the transfer was cancelled"),
        }
    }
}

/// Run the endpoint until the parent (rterm-connect) exits.
pub fn endpoint(args: EndpointArgs) -> Result<()> {
    if let Some(pid) = args.parent {
        watch_parent(pid);
    }
    let args = Arc::new(args);
    // The session may not exist yet (rterm-connect starts us alongside
    // creating it), so retry: quickly at first, then every few seconds.
    let mut failures = 0;
    loop {
        match serve_once(&args) {
            Ok(()) => failures = 0, // registered, then the connection dropped
            Err(_) => failures += 1,
        }
        if failures > 80 {
            bail!("could not reach session {:?}", args.session);
        }
        let delay = if failures < 8 { 250 } else { 2000 };
        std::thread::sleep(Duration::from_millis(delay));
    }
}

/// One connection to the session: register, then handle transfers until
/// the connection drops.
fn serve_once(args: &Arc<EndpointArgs>) -> Result<()> {
    let (mut reader, writer): (Box<dyn Read + Send>, Writer) = if args.direct {
        let dir = paths::socket_dir();
        let sock = Stream::connect(paths::socket_path(&dir, &args.session))?;
        (
            Box::new(sock.try_clone()?),
            Arc::new(Mutex::new(Box::new(sock))),
        )
    } else {
        let mut cmd = std::process::Command::new("ssh");
        cmd.args(["-T", "-o", "BatchMode=yes", "-o", "ControlMaster=no"])
            .args(&args.ssh_args)
            .arg(&args.host)
            .arg(format!("{} __bridge {}", args.rterm, args.session))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = cmd.spawn().context("starting ssh")?;
        let stdout = child.stdout.take().unwrap();
        let stdin = child.stdin.take().unwrap();
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        (Box::new(stdout), Arc::new(Mutex::new(Box::new(stdin))))
    };
    writer.lock().unwrap().write_all(
        &Msg::AgentHello {
            version: protocol::VERSION,
        }
        .encode(),
    )?;

    let mut frames = FrameReader::default();
    let mut buf = vec![0u8; 512 << 10];
    let mut handlers: HashMap<u64, Sender<Tx>> = HashMap::new();
    let mut registered = false;
    loop {
        while let Some(msg) = frames.next()? {
            match msg {
                Msg::Ok => registered = true,
                Msg::TransferBegin { id, payload } => {
                    let (tx, rx) = mpsc::channel();
                    handlers.insert(id, tx);
                    let link = EndpointLink {
                        id,
                        rx,
                        writer: writer.clone(),
                    };
                    let args = args.clone();
                    let request = Tx::decode(&payload);
                    std::thread::spawn(move || handle(link, request, &args));
                }
                Msg::Transfer { id, payload } => {
                    if let (Some(h), Ok(tx)) = (handlers.get(&id), Tx::decode(&payload)) {
                        let _ = h.send(tx);
                    }
                }
                Msg::TransferEnd { id } => {
                    handlers.remove(&id);
                }
                Msg::Error(e) => bail!("{e}"),
                _ => {}
            }
        }
        let n = reader.read(&mut buf)?;
        if n == 0 {
            if registered {
                return Ok(());
            }
            bail!("connection closed before registering");
        }
        frames.push(&buf[..n]);
    }
}

fn handle(mut link: EndpointLink, request: Result<Tx>, args: &EndpointArgs) {
    let result = match request {
        Ok(Tx::GetRequest { items, dest, force }) => {
            handle_get(&mut link, &items, dest, force, args)
        }
        Ok(Tx::PutRequest {
            sources,
            home,
            dest,
            force,
        }) => handle_put(&mut link, &sources, &home, &dest, force, args),
        Ok(other) => Err(anyhow!("unexpected request {other:?}")),
        Err(e) => Err(e),
    };
    if let Err(e) = result {
        let _ = link.send(&Tx::Refused {
            reason: format!("{e:#}"),
        });
    }
}

fn handle_get(
    link: &mut EndpointLink,
    items: &[String],
    dest: Option<String>,
    force: bool,
    args: &EndpointArgs,
) -> Result<()> {
    let downloads = args.download_dir.clone().unwrap_or_else(download_dir);
    let target = match &dest {
        None => downloads.clone(),
        Some(d) => expand_local(d, &args.cwd),
    };
    if dest.is_none() {
        fs::create_dir_all(&target).with_context(|| format!("creating {}", target.display()))?;
    }
    if !target.is_dir() {
        bail!("{} is not a folder on your computer", target.display());
    }
    // Writing anywhere but the download folder needs the user's OK.
    let inside_downloads = match (target.canonicalize(), downloads.canonicalize()) {
        (Ok(t), Ok(d)) => t.starts_with(d),
        _ => false,
    };
    if !inside_downloads {
        let list = items
            .iter()
            .map(|i| format!("  {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let message = format!(
            "{} wants to save these into {} on this computer:\n\n{list}\n\nAllow?",
            args.host,
            target.display()
        );
        if !confirm(&message)? {
            bail!("declined on your computer");
        }
    }
    let home = home_dir();
    link.send(&Tx::Accepted {
        dest: tilde(&target, home.as_deref()),
    })?;
    let display = |p: &Path| tilde(p, home.as_deref());
    receive(link, &target, force, &display, &mut Progress::silent())?;
    Ok(())
}

fn handle_put(
    link: &mut EndpointLink,
    requested: &[String],
    remote_home: &str,
    dest: &str,
    force: bool,
    args: &EndpointArgs,
) -> Result<()> {
    let _ = force; // the session side applies it when writing
    let home = home_dir();
    let mut paths = Vec::new();
    let mut missing = Vec::new();
    for r in requested {
        match resolve_local(r, remote_home, &args.cwd, home.as_deref()) {
            Some(p) => paths.push(p),
            None => missing.push(r.clone()),
        }
    }
    if !missing.is_empty() {
        bail!(
            "not found on your computer: {} (relative paths start in {})",
            missing.join(", "),
            args.cwd.display()
        );
    }
    let sources = Sources::scan(&paths)?;
    let list = paths
        .iter()
        .map(|p| format!("  {}", p.display()))
        .collect::<Vec<_>>()
        .join("\n");
    let message = format!(
        "{} wants to copy {} ({}) from this computer into {}:\n\n{list}\n\nAllow?",
        args.host,
        if sources.files == 1 {
            "1 file".into()
        } else {
            format!("{} files", sources.files)
        },
        human_size(sources.bytes),
        dest
    );
    if !confirm(&message)? {
        bail!("declined on your computer");
    }
    link.send(&Tx::Accepted { dest: dest.into() })?;
    send(link, &sources, &mut Progress::silent())?;
    Ok(())
}

/// Expand `~` and make relative paths relative to `cwd`.
fn expand_local(path: &str, cwd: &Path) -> PathBuf {
    if let Some(rest) = path.strip_prefix('~')
        && (rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\'))
        && let Some(home) = home_dir()
    {
        return home.join(rest.trim_start_matches(['/', '\\']));
    }
    cwd.join(path)
}

/// Find the local file the user meant by `requested`, as typed in the
/// session: `~` is this computer's home; a path under the server's home
/// directory (what an unquoted `~` turned into) maps onto this computer's
/// home; relative paths start where rterm-connect was run.
fn resolve_local(
    requested: &str,
    remote_home: &str,
    cwd: &Path,
    home: Option<&Path>,
) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(rest) = requested.strip_prefix('~')
        && (rest.is_empty() || rest.starts_with('/'))
        && let Some(home) = home
    {
        candidates.push(home.join(rest.trim_start_matches('/')));
    }
    if !remote_home.is_empty()
        && let Some(home) = home
        && let Some(rest) = requested.strip_prefix(remote_home)
        && (rest.is_empty() || rest.starts_with('/'))
    {
        candidates.push(home.join(rest.trim_start_matches('/')));
    }
    let as_given = Path::new(requested);
    if as_given.is_absolute() {
        candidates.push(as_given.to_path_buf());
    } else {
        candidates.push(cwd.join(requested));
    }
    candidates.into_iter().find(|p| p.exists())
}

/// The user's download folder.
pub fn download_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("RTERM_DOWNLOAD_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    let home = home_dir().unwrap_or_else(|| PathBuf::from("."));
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // The (possibly localized) XDG download folder.
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        if let Ok(text) = fs::read_to_string(config.join("user-dirs.dirs")) {
            for line in text.lines() {
                if let Some(value) = line.trim().strip_prefix("XDG_DOWNLOAD_DIR=") {
                    let value = value
                        .trim_matches('"')
                        .replace("$HOME", &home.to_string_lossy());
                    if !value.is_empty() {
                        return PathBuf::from(value);
                    }
                }
            }
        }
    }
    home.join("Downloads")
}

/// Ask the user, in a native dialog on this computer, to allow a transfer.
/// `RTERM_TRANSFER_CONFIRM` changes that: `off` allows without asking (for
/// computers with no desktop to show a dialog on), `deny` refuses.
fn confirm(message: &str) -> Result<bool> {
    match std::env::var("RTERM_TRANSFER_CONFIRM").as_deref() {
        Ok("off") => Ok(true),
        Ok("deny") => Ok(false),
        _ => dialog::ask("Rob Terminal", message),
    }
}

mod dialog {
    use anyhow::Result;

    #[cfg(target_os = "macos")]
    pub fn ask(title: &str, message: &str) -> Result<bool> {
        let quote = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
        let script = format!(
            "display dialog {} with title {} buttons {{\"Don't Allow\", \"Allow\"}} \
             default button \"Don't Allow\" cancel button \"Don't Allow\" with icon caution \
             giving up after 120",
            quote(message),
            quote(title)
        );
        let out = std::process::Command::new("osascript")
            .args(["-e", "activate", "-e", &script])
            .output()?;
        let errors = String::from_utf8_lossy(&out.stderr);
        // Error -128 is the cancel button ("Don't Allow").
        if !out.status.success() && !errors.contains("-128") {
            anyhow::bail!("couldn't ask for your OK: {}", errors.trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).contains("button returned:Allow"))
    }

    #[cfg(windows)]
    pub fn ask(title: &str, message: &str) -> Result<bool> {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            IDYES, MB_DEFBUTTON2, MB_ICONWARNING, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO,
            MessageBoxW,
        };
        let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
        let (title, message) = (wide(title), wide(message));
        let answer = unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                message.as_ptr(),
                title.as_ptr(),
                MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2 | MB_TOPMOST | MB_SETFOREGROUND,
            )
        };
        if answer == 0 {
            anyhow::bail!(
                "couldn't ask for your OK: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(answer == IDYES)
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub fn ask(title: &str, message: &str) -> Result<bool> {
        use std::process::Command;
        let has_display =
            std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some();
        let have = |p: &str| {
            std::env::var_os("PATH")
                .is_some_and(|path| std::env::split_paths(&path).any(|d| d.join(p).is_file()))
        };
        if has_display && have("zenity") {
            let status = Command::new("zenity")
                .args([
                    "--question",
                    "--title",
                    title,
                    "--text",
                    message,
                    "--ok-label=Allow",
                    "--cancel-label=Don't Allow",
                    "--default-cancel",
                    "--timeout=120",
                    "--no-markup",
                ])
                .status()?;
            // 1 is "Don't Allow", 5 the timeout; anything else went wrong.
            return match status.code() {
                Some(0) => Ok(true),
                Some(1 | 5) => Ok(false),
                _ => anyhow::bail!("couldn't ask for your OK (zenity failed: {status})"),
            };
        }
        if has_display && have("kdialog") {
            let status = Command::new("kdialog")
                .args([
                    "--title",
                    title,
                    "--yes-label",
                    "Allow",
                    "--no-label",
                    "Don't Allow",
                ])
                .args(["--warningyesno", message])
                .status()?;
            return match status.code() {
                Some(0) => Ok(true),
                Some(1 | 2) => Ok(false),
                _ => anyhow::bail!("couldn't ask for your OK (kdialog failed: {status})"),
            };
        }
        anyhow::bail!(
            "can't ask for your OK on this computer (no desktop dialog: install zenity, or set \
             RTERM_TRANSFER_CONFIRM=off to allow transfers without asking)"
        )
    }
}

/// Exit when the process that started us (rterm-connect) is gone.
fn watch_parent(pid: u32) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(1));
            if !process_alive(pid) {
                std::process::exit(0);
            }
        }
    });
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    let r = unsafe { libc::kill(pid as i32, 0) };
    r == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };
    unsafe {
        let h = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if h.is_null() {
            return false;
        }
        let alive = WaitForSingleObject(h, 0) == WAIT_TIMEOUT;
        CloseHandle(h);
        alive
    }
}

// ---------------------------------------------------------------------------
// `rterm __bridge NAME`: the far end of the endpoint's ssh channel.

/// Join stdin/stdout to session `name`'s daemon socket.
pub fn bridge(name: &str) -> Result<()> {
    let dir = paths::socket_dir();
    let sock = Stream::connect(paths::socket_path(&dir, name))
        .with_context(|| format!("no session named {name:?}"))?;
    let mut to_daemon = sock.try_clone()?;
    std::thread::spawn(move || {
        let _ = io::copy(&mut io::stdin().lock(), &mut to_daemon);
        let _ = to_daemon.shutdown(std::net::Shutdown::Write);
    });
    let mut from_daemon = sock;
    let mut out = io::stdout().lock();
    let mut buf = vec![0u8; 512 << 10];
    loop {
        let n = from_daemon.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        out.write_all(&buf[..n])?;
        out.flush()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two in-memory ends of a transfer.
    struct Pipe {
        tx: Sender<Tx>,
        rx: Receiver<Tx>,
    }

    impl Link for Pipe {
        fn send(&mut self, tx: &Tx) -> Result<()> {
            // Round-trip through the wire format.
            let decoded = Tx::decode(&tx.encode())?;
            assert_eq!(&decoded, tx);
            self.tx.send(decoded).map_err(|_| anyhow!("closed"))
        }
        fn recv(&mut self, timeout: Duration) -> Result<Option<Tx>> {
            match self.rx.recv_timeout(timeout) {
                Ok(t) => Ok(Some(t)),
                Err(RecvTimeoutError::Timeout) => Ok(None),
                Err(RecvTimeoutError::Disconnected) => bail!("closed"),
            }
        }
    }

    fn pipes() -> (Pipe, Pipe) {
        let (a_tx, b_rx) = mpsc::channel();
        let (b_tx, a_rx) = mpsc::channel();
        (Pipe { tx: a_tx, rx: a_rx }, Pipe { tx: b_tx, rx: b_rx })
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rterm-transfer-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn transfer(paths: &[PathBuf], dest: &Path, force: bool) -> Result<Vec<Saved>> {
        let (mut a, mut b) = pipes();
        let sources = Sources::scan(paths)?;
        let dest = dest.to_path_buf();
        let receiver = std::thread::spawn(move || {
            let shown = |p: &Path| p.display().to_string();
            receive(&mut b, &dest, force, &shown, &mut Progress::silent())
        });
        let sent = send(&mut a, &sources, &mut Progress::silent());
        let received = receiver.join().unwrap();
        let sent = sent?;
        assert_eq!(sent, received?);
        Ok(sent)
    }

    #[test]
    fn copies_files_and_directories() {
        let src = temp("src1");
        let dst = temp("dst1");
        fs::write(src.join("a.txt"), "hello").unwrap();
        let big: Vec<u8> = (0..3_000_000u32).map(|i| (i * 7) as u8).collect();
        fs::write(src.join("big.bin"), &big).unwrap();
        fs::create_dir_all(src.join("tree/sub/empty")).unwrap();
        fs::write(src.join("tree/one"), "1").unwrap();
        fs::write(src.join("tree/sub/two"), "22").unwrap();

        let saved = transfer(
            &[src.join("a.txt"), src.join("big.bin"), src.join("tree")],
            &dst,
            false,
        )
        .unwrap();
        assert_eq!(saved.len(), 3);
        assert_eq!(fs::read(dst.join("a.txt")).unwrap(), b"hello");
        assert_eq!(fs::read(dst.join("big.bin")).unwrap(), big);
        assert_eq!(fs::read(dst.join("tree/sub/two")).unwrap(), b"22");
        assert!(dst.join("tree/sub/empty").is_dir());
        assert_eq!((saved[2].files, saved[2].bytes), (2, 3));

        // Never clobber: the second copy gets a new name.
        let again = transfer(&[src.join("a.txt"), src.join("tree")], &dst, false).unwrap();
        assert!(again[0].path.ends_with("a (1).txt"), "{:?}", again);
        assert!(again[1].path.ends_with("tree (1)"), "{:?}", again);
        assert_eq!(fs::read(dst.join("a (1).txt")).unwrap(), b"hello");

        // -f replaces.
        fs::write(src.join("a.txt"), "changed").unwrap();
        transfer(&[src.join("a.txt")], &dst, true).unwrap();
        assert_eq!(fs::read(dst.join("a.txt")).unwrap(), b"changed");
        assert!(
            !fs::read_dir(&dst).unwrap().any(|e| e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".part"))
        );
    }

    #[test]
    fn hostile_names_stay_inside() {
        let dst = temp("dst2");
        let (mut a, mut b) = pipes();
        let d = dst.clone();
        let receiver = std::thread::spawn(move || {
            let shown = |p: &Path| p.display().to_string();
            receive(&mut b, &d, false, &shown, &mut Progress::silent())
        });
        a.send(&Tx::Total { files: 1, bytes: 1 }).unwrap();
        a.send(&Tx::Item { name: "..".into() }).unwrap();
        let result = receiver.join().unwrap();
        assert!(result.is_err());

        assert!(safe_relative("ok/../../etc").is_err());
        assert_eq!(safe_component("a/b").unwrap(), "a_b");
        assert!(safe_component("").is_err());
        assert!(fs::read_dir(&dst).unwrap().next().is_none());
    }

    #[test]
    fn local_paths_resolve_like_the_user_means() {
        let home = temp("home");
        let cwd = temp("cwd");
        fs::create_dir_all(home.join("Desktop")).unwrap();
        fs::write(home.join("Desktop/x.csv"), "x").unwrap();
        fs::write(cwd.join("rel.txt"), "r").unwrap();
        let h = Some(home.as_path());
        assert_eq!(
            resolve_local("~/Desktop/x.csv", "/home/rob", &cwd, h),
            Some(home.join("Desktop/x.csv"))
        );
        // An unquoted ~ was expanded by the server's shell.
        assert_eq!(
            resolve_local("/home/rob/Desktop/x.csv", "/home/rob", &cwd, h),
            Some(home.join("Desktop/x.csv"))
        );
        assert_eq!(
            resolve_local("rel.txt", "/home/rob", &cwd, h),
            Some(cwd.join("rel.txt"))
        );
        assert_eq!(resolve_local("missing", "/home/rob", &cwd, h), None);
        assert_eq!(resolve_local("/home/robert/x", "/home/rob", &cwd, h), None);
    }

    #[test]
    fn router_pairs_transfers_with_the_endpoint() {
        let mut r = Router::default();
        let begin = |id| Msg::TransferBegin {
            id,
            payload: vec![1],
        };
        // No endpoint yet.
        let a = r.message(5, begin(1));
        assert!(matches!(&a[0], Action::Send(5, Msg::Error(_))));
        assert_eq!(a[1], Action::Close(5));

        assert_eq!(
            r.message(
                9,
                Msg::AgentHello {
                    version: protocol::VERSION
                }
            ),
            vec![Action::Send(9, Msg::Ok)]
        );
        assert_eq!(r.message(5, begin(1)), vec![Action::Send(9, begin(1))]);
        let data = |id| Msg::Transfer {
            id,
            payload: vec![2],
        };
        assert_eq!(r.message(9, data(1)), vec![Action::Send(5, data(1))]);
        assert_eq!(r.message(5, data(1)), vec![Action::Send(9, data(1))]);
        // A stranger can't inject into someone else's transfer.
        assert_eq!(r.message(6, data(1)), vec![]);
        assert_eq!(r.message(9, data(2)), vec![]);

        // The session side going away ends the transfer at the endpoint.
        assert_eq!(r.message(6, begin(2)), vec![Action::Send(9, begin(2))]);
        assert_eq!(
            r.disconnected(6),
            vec![Action::Send(9, Msg::TransferEnd { id: 2 })]
        );
        assert_eq!(r.message(9, data(2)), vec![]);

        // A newer endpoint replaces the old one, ending its transfers.
        let a = r.message(
            10,
            Msg::AgentHello {
                version: protocol::VERSION,
            },
        );
        assert_eq!(
            a,
            vec![
                Action::Send(5, Msg::TransferEnd { id: 1 }),
                Action::Close(9),
                Action::Send(10, Msg::Ok)
            ]
        );
        assert_eq!(r.disconnected(9), vec![]);
        assert_eq!(r.message(5, begin(3)), vec![Action::Send(10, begin(3))]);
        assert_eq!(
            r.disconnected(10),
            vec![Action::Send(5, Msg::TransferEnd { id: 3 })]
        );
    }

    #[test]
    fn names_and_sizes() {
        let d = temp("unique");
        fs::write(d.join("r.tar.gz"), "").unwrap();
        assert_eq!(unique_path(&d, "r.tar.gz"), d.join("r.tar (1).gz"));
        fs::write(d.join("noext"), "").unwrap();
        assert_eq!(unique_path(&d, "noext"), d.join("noext (1)"));
        assert_eq!(human_size(999), "999 B");
        assert_eq!(human_size(1_234_567), "1.2 MB");
    }
}
