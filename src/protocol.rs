//! Wire protocol between `rterm` clients and session daemons.
//!
//! Every message is a frame: `tag: u8`, `len: u32 (BE)`, `payload: [u8; len]`.
//! Structured payloads use a tiny hand-rolled encoding (big-endian integers,
//! length-prefixed strings) so old daemons and new clients can detect a
//! version mismatch instead of misbehaving.

use std::io::{self, Read, Write};

use anyhow::{Result, anyhow, bail};

/// Bump whenever the meaning of an existing message changes.
///
/// Compatibility rules, so that upgrading the package never strands running
/// sessions: the frame header, the leading `version` field of `Attach` and
/// `Query`, and the `Incompatible` reply must never change. A client that
/// gets `Incompatible` from an older daemon re-runs that daemon's own binary
/// (see `client::run_daemon_binary`).
pub const VERSION: u32 = 1;

/// Frames larger than this are treated as protocol corruption.
const MAX_FRAME: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WinSize {
    pub rows: u16,
    pub cols: u16,
    pub xpix: u16,
    pub ypix: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub name: String,
    pub pid: u32,
    pub created: u64,
    pub attached: bool,
    pub rows: u16,
    pub cols: u16,
    pub command: String,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    // client -> daemon
    Attach {
        version: u32,
        size: WinSize,
        ssh_auth_sock: Option<String>,
    },
    Input(Vec<u8>),
    Resize(WinSize),
    Detach,
    Query {
        version: u32,
    },
    Kill,
    DetachClient,

    // File transfers (see transfer.rs). The daemon routes these between a
    // `rterm get`/`rterm put` process in the session and the transfer
    // endpoint on the user's computer, without looking inside `payload`.
    /// Register this connection as the session's transfer endpoint.
    AgentHello {
        version: u32,
    },
    /// Start transfer `id` (session side); forwarded to the endpoint.
    TransferBegin {
        id: u64,
        payload: Vec<u8>,
    },
    /// A message within transfer `id`, in either direction.
    Transfer {
        id: u64,
        payload: Vec<u8>,
    },
    /// Transfer `id` is over (or one side went away).
    TransferEnd {
        id: u64,
    },

    // daemon -> client
    Output(Vec<u8>),
    /// `drain_input`: the terminal was reporting key releases (kitty
    /// protocol), so trailing key events should be swallowed, not passed to
    /// the user's shell.
    Detached {
        reason: String,
        drain_input: bool,
    },
    Exited {
        code: i32,
    },
    Info(SessionInfo),
    Error(String),
    Ok,
    /// The daemon speaks protocol `version` and can't serve this client.
    Incompatible {
        version: u32,
    },
}

mod tag {
    pub const ATTACH: u8 = 1;
    pub const INPUT: u8 = 2;
    pub const RESIZE: u8 = 3;
    pub const DETACH: u8 = 4;
    pub const QUERY: u8 = 5;
    pub const KILL: u8 = 6;
    pub const DETACH_CLIENT: u8 = 7;
    pub const AGENT_HELLO: u8 = 8;
    pub const TRANSFER_BEGIN: u8 = 9;
    pub const TRANSFER: u8 = 10;
    pub const TRANSFER_END: u8 = 11;
    pub const OUTPUT: u8 = 100;
    pub const DETACHED: u8 = 101;
    pub const EXITED: u8 = 102;
    pub const INFO: u8 = 103;
    pub const ERROR: u8 = 104;
    pub const OK: u8 = 105;
    pub const INCOMPATIBLE: u8 = 106;
}

pub(crate) struct Enc(pub(crate) Vec<u8>);

impl Enc {
    pub(crate) fn u16(&mut self, v: u16) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub(crate) fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub(crate) fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub(crate) fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.u32(v.len() as u32);
        self.0.extend_from_slice(v);
        self
    }
    pub(crate) fn str(&mut self, v: &str) -> &mut Self {
        self.bytes(v.as_bytes())
    }
    fn size(&mut self, s: WinSize) -> &mut Self {
        self.u16(s.rows).u16(s.cols).u16(s.xpix).u16(s.ypix)
    }
}

pub(crate) struct Dec<'a>(pub(crate) &'a [u8]);

impl<'a> Dec<'a> {
    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.0.len() < n {
            bail!("truncated message");
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Ok(head)
    }
    pub(crate) fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into()?))
    }
    pub(crate) fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into()?))
    }
    pub(crate) fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into()?))
    }
    pub(crate) fn bytes(&mut self) -> Result<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }
    pub(crate) fn str(&mut self) -> Result<String> {
        Ok(String::from_utf8_lossy(self.bytes()?).into_owned())
    }
    fn size(&mut self) -> Result<WinSize> {
        Ok(WinSize {
            rows: self.u16()?,
            cols: self.u16()?,
            xpix: self.u16()?,
            ypix: self.u16()?,
        })
    }
}

impl Msg {
    /// Serialize into a complete frame, appending to `out`.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        let mut e = Enc(Vec::new());
        let t = match self {
            Msg::Attach {
                version,
                size,
                ssh_auth_sock,
            } => {
                e.u32(*version)
                    .size(*size)
                    .str(ssh_auth_sock.as_deref().unwrap_or(""));
                tag::ATTACH
            }
            Msg::Input(b) => {
                e.0.extend_from_slice(b);
                tag::INPUT
            }
            Msg::Resize(s) => {
                e.size(*s);
                tag::RESIZE
            }
            Msg::Detach => tag::DETACH,
            Msg::Query { version } => {
                e.u32(*version);
                tag::QUERY
            }
            Msg::Kill => tag::KILL,
            Msg::DetachClient => tag::DETACH_CLIENT,
            Msg::AgentHello { version } => {
                e.u32(*version);
                tag::AGENT_HELLO
            }
            Msg::TransferBegin { id, payload } => {
                e.u64(*id).bytes(payload);
                tag::TRANSFER_BEGIN
            }
            Msg::Transfer { id, payload } => {
                e.u64(*id).bytes(payload);
                tag::TRANSFER
            }
            Msg::TransferEnd { id } => {
                e.u64(*id);
                tag::TRANSFER_END
            }
            Msg::Output(b) => {
                e.0.extend_from_slice(b);
                tag::OUTPUT
            }
            Msg::Detached {
                reason,
                drain_input,
            } => {
                e.str(reason).u16(*drain_input as u16);
                tag::DETACHED
            }
            Msg::Exited { code } => {
                e.u32(*code as u32);
                tag::EXITED
            }
            Msg::Info(i) => {
                e.str(&i.name)
                    .u32(i.pid)
                    .u64(i.created)
                    .u16(i.attached as u16)
                    .u16(i.rows)
                    .u16(i.cols)
                    .str(&i.command)
                    .str(&i.title);
                tag::INFO
            }
            Msg::Error(s) => {
                e.str(s);
                tag::ERROR
            }
            Msg::Ok => tag::OK,
            Msg::Incompatible { version } => {
                e.u32(*version);
                tag::INCOMPATIBLE
            }
        };
        out.push(t);
        out.extend_from_slice(&(e.0.len() as u32).to_be_bytes());
        out.extend_from_slice(&e.0);
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::new();
        self.encode_into(&mut v);
        v
    }

    fn decode(t: u8, p: &[u8]) -> Result<Msg> {
        let mut d = Dec(p);
        Ok(match t {
            tag::ATTACH => {
                let version = d.u32()?;
                let size = d.size()?;
                let sock = d.str()?;
                Msg::Attach {
                    version,
                    size,
                    ssh_auth_sock: (!sock.is_empty()).then_some(sock),
                }
            }
            tag::INPUT => Msg::Input(p.to_vec()),
            tag::RESIZE => Msg::Resize(d.size()?),
            tag::DETACH => Msg::Detach,
            tag::QUERY => Msg::Query { version: d.u32()? },
            tag::KILL => Msg::Kill,
            tag::DETACH_CLIENT => Msg::DetachClient,
            tag::AGENT_HELLO => Msg::AgentHello { version: d.u32()? },
            tag::TRANSFER_BEGIN => Msg::TransferBegin {
                id: d.u64()?,
                payload: d.bytes()?.to_vec(),
            },
            tag::TRANSFER => Msg::Transfer {
                id: d.u64()?,
                payload: d.bytes()?.to_vec(),
            },
            tag::TRANSFER_END => Msg::TransferEnd { id: d.u64()? },
            tag::OUTPUT => Msg::Output(p.to_vec()),
            tag::DETACHED => Msg::Detached {
                reason: d.str()?,
                drain_input: d.u16()? != 0,
            },
            tag::EXITED => Msg::Exited {
                code: d.u32()? as i32,
            },
            tag::INFO => Msg::Info(SessionInfo {
                name: d.str()?,
                pid: d.u32()?,
                created: d.u64()?,
                attached: d.u16()? != 0,
                rows: d.u16()?,
                cols: d.u16()?,
                command: d.str()?,
                title: d.str()?,
            }),
            tag::ERROR => Msg::Error(d.str()?),
            tag::OK => Msg::Ok,
            tag::INCOMPATIBLE => Msg::Incompatible { version: d.u32()? },
            other => return Err(anyhow!("unknown message type {other}")),
        })
    }
}

/// Accumulates bytes from a stream and splits them into messages.
#[derive(Default)]
pub struct FrameReader {
    buf: Vec<u8>,
}

impl FrameReader {
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Returns the next complete message, if one is buffered.
    pub fn next(&mut self) -> Result<Option<Msg>> {
        if self.buf.len() < 5 {
            return Ok(None);
        }
        let len = u32::from_be_bytes(self.buf[1..5].try_into().unwrap()) as usize;
        if len > MAX_FRAME {
            bail!("oversized frame ({len} bytes)");
        }
        if self.buf.len() < 5 + len {
            return Ok(None);
        }
        let msg = Msg::decode(self.buf[0], &self.buf[5..5 + len]);
        self.buf.drain(..5 + len);
        msg.map(Some)
    }
}

/// Blocking helpers for short request/response exchanges (ls, kill, ...).
pub fn send(w: &mut impl Write, msg: &Msg) -> io::Result<()> {
    w.write_all(&msg.encode())
}

pub fn recv(r: &mut impl Read, reader: &mut FrameReader) -> Result<Msg> {
    let mut buf = [0u8; 8192];
    loop {
        if let Some(m) = reader.next()? {
            return Ok(m);
        }
        let n = r.read(&mut buf)?;
        if n == 0 {
            bail!("connection closed");
        }
        reader.push(&buf[..n]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let msgs = vec![
            Msg::Attach {
                version: VERSION,
                size: WinSize {
                    rows: 40,
                    cols: 120,
                    xpix: 1,
                    ypix: 2,
                },
                ssh_auth_sock: Some("/tmp/agent".into()),
            },
            Msg::Attach {
                version: VERSION,
                size: WinSize::default(),
                ssh_auth_sock: None,
            },
            Msg::Input(b"hello\x1b[A".to_vec()),
            Msg::Resize(WinSize {
                rows: 1,
                cols: 2,
                xpix: 3,
                ypix: 4,
            }),
            Msg::Detach,
            Msg::Query { version: VERSION },
            Msg::Kill,
            Msg::DetachClient,
            Msg::AgentHello { version: VERSION },
            Msg::TransferBegin {
                id: 7,
                payload: vec![1, 2, 3],
            },
            Msg::Transfer {
                id: u64::MAX,
                payload: vec![],
            },
            Msg::TransferEnd { id: 9 },
            Msg::Output(vec![0, 1, 2, 255]),
            Msg::Detached {
                reason: "bye".into(),
                drain_input: true,
            },
            Msg::Exited { code: -3 },
            Msg::Info(SessionInfo {
                name: "main".into(),
                pid: 42,
                created: 1_700_000_000,
                attached: true,
                rows: 24,
                cols: 80,
                command: "-zsh".into(),
                title: "t".into(),
            }),
            Msg::Error("nope".into()),
            Msg::Incompatible { version: 7 },
            Msg::Ok,
        ];
        let mut wire = Vec::new();
        for m in &msgs {
            m.encode_into(&mut wire);
        }
        let mut r = FrameReader::default();
        let mut out = Vec::new();
        // Feed one byte at a time to exercise partial frames.
        for b in wire {
            r.push(&[b]);
            while let Some(m) = r.next().unwrap() {
                out.push(m);
            }
        }
        assert_eq!(out, msgs);
    }
}
