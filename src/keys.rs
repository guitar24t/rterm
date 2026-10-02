//! Detach-key parsing and detection in the client's input stream.
//!
//! The key must be recognized however the outer terminal currently encodes
//! it: as a plain control byte, as a kitty keyboard-protocol `CSI ... u`
//! sequence (enabled by programs such as Codex), or as an xterm
//! modifyOtherKeys `CSI 27 ; mods ; code ~` sequence.

use anyhow::{Result, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DetachKey {
    /// Byte sent in legacy mode, e.g. 0x1c for Ctrl-\.
    byte: u8,
    /// Unicode key code reported by the kitty protocol, e.g. '\\'.
    code: u32,
}

pub const DEFAULT_DETACH_KEY: &str = "^\\";

impl DetachKey {
    /// Parse `^\`, `^a`, `C-a`, `ctrl-a`; `none` disables the key.
    pub fn parse(spec: &str) -> Result<Option<DetachKey>> {
        let s = spec.trim();
        if s.is_empty() || s.eq_ignore_ascii_case("none") || s.eq_ignore_ascii_case("off") {
            return Ok(None);
        }
        let lower = s.to_ascii_lowercase();
        let key = ["^", "c-", "ctrl-", "ctrl+", "control-"]
            .iter()
            .find_map(|p| lower.strip_prefix(p).map(|_| &s[p.len()..]));
        let Some(key) = key else {
            bail!("unrecognized detach key {spec:?}; use something like '^\\' or '^a', or 'none'");
        };
        let mut chars = key.chars();
        let (Some(c), None) = (chars.next(), chars.next()) else {
            bail!("unrecognized detach key {spec:?}; expected a single key after the Ctrl prefix");
        };
        let c = c.to_ascii_lowercase();
        let (byte, code) = match c {
            'a'..='z' => (c as u8 - b'a' + 1, c as u32),
            '@' | ' ' | '2' => (0x00, c as u32),
            '[' => (0x1b, c as u32),
            '\\' => (0x1c, c as u32),
            ']' => (0x1d, c as u32),
            '^' | '6' => (0x1e, c as u32),
            '_' | '/' | '-' => (0x1f, c as u32),
            _ => bail!("unsupported detach key {spec:?}"),
        };
        if byte == 0x1b {
            bail!("Ctrl-[ is Escape and cannot be used as the detach key");
        }
        Ok(Some(DetachKey { byte, code }))
    }

    /// Find the detach key in `input`; returns (start, end) of its encoding.
    #[cfg(test)]
    pub fn find(&self, input: &[u8]) -> Option<(usize, usize)> {
        (0..input.len()).find_map(|i| self.match_at(&input[i..]).map(|len| (i, i + len)))
    }

    /// If `input` starts with the detach key, return the encoding's length.
    pub fn match_at(&self, input: &[u8]) -> Option<usize> {
        match input {
            [b, ..] if *b == self.byte => Some(1),
            [0x1b, b'[', body @ ..] => self.match_csi(body).map(|len| len + 2),
            _ => None,
        }
    }

    /// Match a CSI body (after `ESC [`); returns its length if it encodes
    /// a press of the detach key.
    fn match_csi(&self, body: &[u8]) -> Option<usize> {
        let end = body
            .iter()
            .position(|&b| !(b.is_ascii_digit() || b == b';' || b == b':'))?;
        let params = std::str::from_utf8(&body[..end]).ok()?;
        let fields: Vec<&str> = params.split(';').collect();
        let num = |s: &str| -> Option<u32> {
            if s.is_empty() {
                Some(1)
            } else {
                s.parse().ok()
            }
        };
        let ctrl_only = |mods: u32| (mods.saturating_sub(1) & !(64 | 128)) == 4;
        match body[end] {
            b'u' => {
                // CSI code[:shifted[:base]] ; mods[:event] [; text] u
                let mut key = fields.first()?.split(':');
                let code: u32 = key.next()?.parse().ok()?;
                let base = key.nth(1).and_then(|s| s.parse::<u32>().ok());
                let mut m = fields.get(1).copied().unwrap_or("").split(':');
                let mods = num(m.next().unwrap_or(""))?;
                let event = num(m.next().unwrap_or(""))?;
                let is_key = code == self.code || base == Some(self.code);
                (is_key && ctrl_only(mods) && event != 3).then_some(end + 1)
            }
            b'~' => {
                // modifyOtherKeys: CSI 27 ; mods ; code ~
                if fields.len() == 3 && fields[0] == "27" {
                    let mods = num(fields[1])?;
                    let code: u32 = fields[2].parse().ok()?;
                    let is_key = code == self.code || code == u32::from(self.byte);
                    return (is_key && ctrl_only(mods)).then_some(end + 1);
                }
                None
            }
            _ => None,
        }
    }
}

/// Finds the detach key in a stream of input, ignoring it inside bracketed
/// pastes (pasted text is data, not a key press).
pub struct KeyScanner {
    key: DetachKey,
    in_paste: bool,
}

impl KeyScanner {
    const PASTE_START: &'static [u8] = b"\x1b[200~";
    const PASTE_END: &'static [u8] = b"\x1b[201~";

    pub fn new(key: DetachKey) -> KeyScanner {
        KeyScanner {
            key,
            in_paste: false,
        }
    }

    /// Returns the offset of the detach key in `input`, if pressed.
    pub fn scan(&mut self, input: &[u8]) -> Option<usize> {
        let mut i = 0;
        while i < input.len() {
            let rest = &input[i..];
            if rest.starts_with(Self::PASTE_START) {
                self.in_paste = true;
                i += Self::PASTE_START.len();
            } else if rest.starts_with(Self::PASTE_END) {
                self.in_paste = false;
                i += Self::PASTE_END.len();
            } else if !self.in_paste && self.key.match_at(rest).is_some() {
                return Some(i);
            } else {
                i += 1;
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(s: &str) -> DetachKey {
        DetachKey::parse(s).unwrap().unwrap()
    }

    #[test]
    fn parse() {
        assert_eq!(
            key("^\\"),
            DetachKey {
                byte: 0x1c,
                code: '\\' as u32
            }
        );
        assert_eq!(key("^a"), key("C-a"));
        assert_eq!(key("ctrl-A").byte, 1);
        assert!(DetachKey::parse("none").unwrap().is_none());
        assert!(DetachKey::parse("x").is_err());
        assert!(DetachKey::parse("^[").is_err());
    }

    #[test]
    fn find() {
        let k = key("^\\");
        assert_eq!(k.find(b"abc"), None);
        assert_eq!(k.find(b"ab\x1cc"), Some((2, 3)));
        assert_eq!(k.find(b"x\x1b[92;5u"), Some((1, 8)));
        assert_eq!(k.find(b"\x1b[92;5:1u"), Some((0, 9)));
        assert_eq!(k.find(b"\x1b[92;69u"), Some((0, 8))); // ctrl + caps lock
        assert_eq!(k.find(b"\x1b[92;5:3u"), None); // release
        assert_eq!(k.find(b"\x1b[92;7u"), None); // ctrl+alt
        assert_eq!(k.find(b"\x1b[92u"), None);
        assert_eq!(k.find(b"\x1b[27;5;92~"), Some((0, 10)));
        assert_eq!(k.find(b"\x1b[A\x1b[1;5A"), None);
        let a = key("^a");
        assert_eq!(a.find(b"\x1b[97;5u"), Some((0, 7)));
        assert_eq!(a.find(b"\x1b[1092:1092:97;5u"), Some((0, 17))); // non-latin layout
    }

    #[test]
    fn ignores_key_inside_paste() {
        let mut sc = KeyScanner::new(key("^\\"));
        assert_eq!(sc.scan(b"ab\x1b[200~x\x1cy\x1b[201~z"), None);
        assert_eq!(sc.scan(b"\x1b[200~split"), None);
        assert_eq!(sc.scan(b"still \x1c pasting\x1b[201~"), None);
        assert_eq!(sc.scan(b"typed\x1c"), Some(5));
        assert_eq!(sc.scan(b"\x1b[A\x1b[92;5u"), Some(3));
        assert_eq!(sc.scan(b"\x1b[200~p\x1b[201~\x1c"), Some(13));
    }
}
