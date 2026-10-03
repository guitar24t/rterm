//! Native `rterm-connect`: list the sessions on a remote host over ssh, pick
//! one or start a new one, and attach.
//!
//! Linux and macOS ship contrib/rterm-connect.py, which also shares one ssh
//! connection and bridges OSC 52 copies to the local clipboard. This is the
//! same tool for Windows, which has no Python by default, whose OpenSSH has
//! no connection sharing, and whose Windows Terminal handles OSC 52 itself.
//! It runs when rterm is invoked as `rterm-connect`.

use std::ffi::OsString;
use std::io::{self, BufRead, IsTerminal, Write};
use std::process::{Command, Stdio};

use clap::Parser;

const INSTALL_HINT: &str = "curl -fsSL https://guitar24t.github.io/rterm/install.sh | sudo sh";
const DEFAULT_NAME: &str = "main";

#[derive(Parser)]
#[command(
    name = "rterm-connect",
    version,
    about = "Choose an rterm session on a remote host and attach to it.",
    after_help = "Options for rterm-connect go before HOST; anything after HOST is passed to \
        ssh, e.g. 'rterm-connect me@host -p 2222'. Detach with Ctrl-\\ as usual."
)]
struct Args {
    /// Only list the sessions, don't attach
    #[arg(long)]
    list: bool,
    /// Attach to (or start) NAME without asking
    #[arg(long, value_name = "NAME")]
    session: Option<String>,
    /// Detach key to use, e.g. '^a' (default: the host's, Ctrl-\)
    #[arg(short = 'e', long, value_name = "KEY")]
    detach_key: Option<String>,
    /// rterm command on the host
    #[arg(long, value_name = "PATH", default_value = "rterm")]
    rterm: String,
    /// Accepted for compatibility with rterm-connect.py; no effect here
    #[arg(long, hide = true)]
    no_mux: bool,
    /// Accepted for compatibility with rterm-connect.py; no effect here
    #[arg(long, hide = true)]
    no_clipboard: bool,
    /// ssh destination, e.g. myserver or user@host
    host: String,
    /// Extra ssh options, e.g. -p 2222
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    ssh_args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub name: String,
    pub status: String,
    pub age: String,
    pub size: String,
    pub title: String,
}

struct Style(bool);

impl Style {
    fn wrap(&self, code: &str, text: &str) -> String {
        if self.0 {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }
    fn bold(&self, t: &str) -> String {
        self.wrap("1", t)
    }
    fn dim(&self, t: &str) -> String {
        self.wrap("2", t)
    }
    fn status(&self, s: &str) -> String {
        match s {
            "attached" => self.wrap("33", s),
            "detached" => self.wrap("32", s),
            _ => self.dim(s),
        }
    }
}

pub fn main(argv: Vec<OsString>) -> i32 {
    let args =
        match Args::try_parse_from(std::iter::once(OsString::from("rterm-connect")).chain(argv)) {
            Ok(a) => a,
            Err(e) => {
                let _ = e.print();
                return if e.use_stderr() { 2 } else { 0 };
            }
        };
    match run(args) {
        Ok(code) => code,
        Err(msg) => {
            eprintln!("rterm-connect: {msg}");
            1
        }
    }
}

fn run(args: Args) -> Result<i32, String> {
    #[cfg(windows)]
    crate::winsys::enable_vt_output();
    let style = Style(io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none());

    let name = match &args.session {
        Some(n) => {
            crate::paths::validate_name(n).map_err(|e| e.to_string())?;
            n.clone()
        }
        None => {
            let sessions = list_sessions(&args)?;
            if args.list {
                print_sessions(&sessions, &args.host, &style, false);
                return Ok(0);
            }
            choose(&sessions, &args.host, &style)?
        }
    };

    let mut remote = vec![args.rterm.clone(), "attach".into(), name];
    if let Some(key) = &args.detach_key {
        remote.extend(["-e".into(), key.clone()]);
    }
    let mut cmd = Command::new("ssh");
    cmd.args(&args.ssh_args)
        .arg("-t")
        .arg(&args.host)
        .arg(shell_join(&remote));
    let _ignore = IgnoreInterrupts::new();
    let status = cmd.status().map_err(|e| format!("can't run ssh: {e}"))?;
    Ok(status.code().unwrap_or(1))
}

/// While ssh runs in the foreground, Ctrl-C belongs to it (and the remote).
struct IgnoreInterrupts;

impl IgnoreInterrupts {
    fn new() -> IgnoreInterrupts {
        #[cfg(windows)]
        crate::winsys::ignore_ctrl_c(true);
        #[cfg(unix)]
        unsafe {
            libc::signal(libc::SIGINT, libc::SIG_IGN);
        }
        IgnoreInterrupts
    }
}

impl Drop for IgnoreInterrupts {
    fn drop(&mut self) {
        #[cfg(windows)]
        crate::winsys::ignore_ctrl_c(false);
        #[cfg(unix)]
        unsafe {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
        }
    }
}

/// Quote words for the remote (POSIX) shell, leaving plain ones bare so the
/// command also works when the host's shell is cmd.exe or PowerShell.
fn shell_join(words: &[String]) -> String {
    words
        .iter()
        .map(|w| {
            let plain = !w.is_empty()
                && w.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./:=@+,".contains(c));
            if plain {
                w.clone()
            } else {
                format!("'{}'", w.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn ssh_output(args: &Args, remote: &[String]) -> Result<(i32, String), String> {
    let out = Command::new("ssh")
        .args(&args.ssh_args)
        .arg(&args.host)
        .arg(shell_join(remote))
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("can't run ssh: {e}"))?;
    Ok((
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    ))
}

fn list_sessions(args: &Args) -> Result<Vec<Session>, String> {
    let rterm = args.rterm.clone();
    let (mut code, mut out) = ssh_output(args, &[rterm.clone(), "ls".into(), "--json".into()])?;
    if code != 0 && code != 127 && code != 255 {
        // rterm 0.1.0 doesn't know --json; read its table instead.
        (code, out) = ssh_output(args, &[rterm, "ls".into()])?;
    }
    match code {
        0 => Ok(parse_listing(&out)),
        127 => Err(format!(
            "rterm is not installed on {} (or not on its PATH; see --rterm).\n\
             Install it there with:\n  {INSTALL_HINT}",
            args.host
        )),
        255 => Err(format!("ssh to {} failed", args.host)),
        c => Err(format!(
            "listing sessions on {} failed (exit {c})",
            args.host
        )),
    }
}

pub fn parse_listing(text: &str) -> Vec<Session> {
    let text = text.trim();
    if text.starts_with('[') {
        let items: Vec<serde_json::Value> = serde_json::from_str(text).unwrap_or_default();
        return items
            .iter()
            .filter_map(|item| {
                let get = |k: &str| {
                    item.get(k)
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_owned()
                };
                let name = get("name");
                if name.is_empty() {
                    return None;
                }
                let title = [get("title"), get("command"), get("error")]
                    .into_iter()
                    .find(|t| !t.is_empty())
                    .unwrap_or_default();
                let size = match (item.get("cols"), item.get("rows")) {
                    (Some(c), Some(r)) => format!("{c}x{r}"),
                    _ => String::new(),
                };
                let status = item
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                Some(Session {
                    name,
                    status: status.into(),
                    age: get("age_text"),
                    size,
                    title,
                })
            })
            .collect();
    }
    // The human-readable table: NAME STATUS AGE SIZE TITLE (title may hold spaces).
    text.lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with("NAME ") && *l != "no sessions")
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let session = |status: &str, age: &str, size: &str, title: String| Session {
                name: fields[0].into(),
                status: status.into(),
                age: age.into(),
                size: size.into(),
                title,
            };
            match fields.as_slice() {
                [_, "?", ..] => Some(session("unknown", "", "", after_fields(line, 2))),
                [_, status, age, size, ..] => {
                    Some(session(status, age, size, after_fields(line, 4)))
                }
                _ => None,
            }
        })
        .collect()
}

/// The rest of `line` after its first `n` whitespace-separated fields.
fn after_fields(line: &str, n: usize) -> String {
    let mut rest = line;
    for _ in 0..n {
        rest = rest.trim_start();
        rest = &rest[rest.find(char::is_whitespace).unwrap_or(rest.len())..];
    }
    rest.trim().to_owned()
}

pub fn suggest_name(sessions: &[Session]) -> String {
    let taken = |n: &str| sessions.iter().any(|s| s.name == n);
    if !taken(DEFAULT_NAME) {
        return DEFAULT_NAME.into();
    }
    (1..)
        .map(|n: u32| n.to_string())
        .find(|n| !taken(n))
        .unwrap()
}

fn print_sessions(sessions: &[Session], host: &str, style: &Style, numbered: bool) {
    if sessions.is_empty() {
        println!("No rterm sessions on {}.", style.bold(host));
        return;
    }
    println!("rterm sessions on {}:\n", style.bold(host));
    let width = sessions.iter().map(|s| s.name.len()).max().unwrap_or(0);
    let size_w = sessions.iter().map(|s| s.size.len()).max().unwrap_or(0);
    for (i, s) in sessions.iter().enumerate() {
        let index = if numbered {
            format!("{:>3}  ", i + 1)
        } else {
            "  ".into()
        };
        let line = format!(
            "{index}{}  {}  {}  {:size_w$}  {}",
            style.bold(&format!("{:width$}", s.name)),
            style.status(&s.status),
            style.dim(&format!("{:>6}", s.age)),
            s.size,
            s.title
        );
        println!("{}", line.trim_end());
    }
}

fn ask(prompt: &str) -> Result<String, String> {
    print!("{prompt}");
    io::stdout().flush().ok();
    let mut line = String::new();
    match io::stdin().lock().read_line(&mut line) {
        Ok(0) | Err(_) => Err("cancelled".into()),
        Ok(_) => Ok(line.trim().to_owned()),
    }
}

fn ask_new_name(sessions: &[Session]) -> Result<String, String> {
    let default = suggest_name(sessions);
    loop {
        let answer = ask(&format!("Name for the new session [{default}]: "))?;
        let name = if answer.is_empty() {
            default.clone()
        } else {
            answer
        };
        if crate::paths::validate_name(&name).is_ok() {
            return Ok(name);
        }
        println!("  Use letters, digits and -_.@+ (up to 64 characters).");
    }
}

fn choose(sessions: &[Session], host: &str, style: &Style) -> Result<String, String> {
    print_sessions(sessions, host, style, true);
    if sessions.is_empty() {
        return ask_new_name(sessions);
    }
    println!("\n{:>3}  new session", "n");
    println!("{:>3}  quit\n", "q");
    let default = sessions
        .iter()
        .position(|s| s.status == "detached")
        .map_or(1, |i| i + 1)
        .to_string();
    loop {
        let answer = ask(&format!("Attach to [{default}]: "))?;
        let answer = if answer.is_empty() {
            default.clone()
        } else {
            answer
        };
        match answer.to_lowercase().as_str() {
            "q" | "quit" | "exit" => std::process::exit(0),
            "n" | "new" => return ask_new_name(sessions),
            _ => {}
        }
        let chosen = match answer.parse::<usize>() {
            Ok(i) if (1..=sessions.len()).contains(&i) => Some(&sessions[i - 1]),
            _ => sessions.iter().find(|s| s.name == answer),
        };
        match chosen {
            Some(s) => {
                if s.status == "attached" {
                    println!(
                        "{}",
                        style.dim("  (attached elsewhere; attaching here takes it over)")
                    );
                }
                return Ok(s.name.clone());
            }
            None if crate::paths::validate_name(&answer).is_ok() => {
                let yes = ask(&format!("No session named '{answer}'. Start it? [Y/n] "))?;
                if matches!(yes.to_lowercase().as_str(), "" | "y" | "yes") {
                    return Ok(answer);
                }
            }
            None => println!("  Enter 1-{}, a session name, n or q.", sessions.len()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(name: &str, status: &str, age: &str, size: &str, title: &str) -> Session {
        Session {
            name: name.into(),
            status: status.into(),
            age: age.into(),
            size: size.into(),
            title: title.into(),
        }
    }

    #[test]
    fn parses_json() {
        let text = r#"[{"name":"main","status":"attached","created":1,"age":5,"age_text":"5s",
            "cols":80,"rows":24,"pid":9,"command":"-zsh","title":"vim  a b"},
            {"name":"bare","status":"detached","age_text":"5s","cols":80,"rows":24,"command":"-zsh","title":""},
            {"name":"old","status":"unknown","error":"started by a different rterm version"}]"#;
        assert_eq!(
            parse_listing(text),
            vec![
                s("main", "attached", "5s", "80x24", "vim  a b"),
                s("bare", "detached", "5s", "80x24", "-zsh"),
                s(
                    "old",
                    "unknown",
                    "",
                    "",
                    "started by a different rterm version"
                ),
            ]
        );
    }

    #[test]
    fn parses_table() {
        let text = "NAME     STATUS     AGE  SIZE    TITLE\n\
                    main     attached  3h05m  120x40  vim  README.md\n\
                    work     detached     9s  80x24   -zsh\n\
                    broken   ?                        Connection refused (os error 61)\n";
        assert_eq!(
            parse_listing(text),
            vec![
                s("main", "attached", "3h05m", "120x40", "vim  README.md"),
                s("work", "detached", "9s", "80x24", "-zsh"),
                s(
                    "broken",
                    "unknown",
                    "",
                    "",
                    "Connection refused (os error 61)"
                ),
            ]
        );
        assert!(parse_listing("no sessions\n").is_empty());
        assert!(parse_listing("[]").is_empty());
    }

    #[test]
    fn names_and_quoting() {
        assert_eq!(suggest_name(&[]), "main");
        assert_eq!(
            suggest_name(&[s("main", "", "", "", ""), s("1", "", "", "", "")]),
            "2"
        );
        assert_eq!(
            shell_join(&[
                "rterm".into(),
                "attach".into(),
                "main".into(),
                "-e".into(),
                "^\\".into()
            ]),
            "rterm attach main -e '^\\'"
        );
        assert_eq!(shell_join(&["it's".into()]), "'it'\\''s'");
    }
}
