//! Native `rterm-connect`: list the sessions on a remote host over ssh, pick
//! one or start a new one, and attach.
//!
//! Linux and macOS ship contrib/rterm-connect.py, which also shares one ssh
//! connection and bridges OSC 52 copies to the local clipboard. This is the
//! same tool for Windows, which has no Python by default, whose OpenSSH has
//! no connection sharing, and whose Windows Terminal handles OSC 52 itself.
//! It runs when rterm is invoked as `rterm-connect`. Like the script, it
//! starts the transfer endpoint that serves `rterm get` and `rterm put` from
//! the session (see transfer.rs).

use std::ffi::OsString;
use std::io::{self, BufRead, IsTerminal, Write};
use std::process::{Command, Stdio};

use clap::Parser;

const INSTALL_HINT: &str = "curl -fsSL https://guitar24t.github.io/rterm/install.sh | sudo sh";
const WINDOWS_INSTALL: &str = "https://github.com/guitar24t/rterm/releases/latest";
/// rterm where the MSI installs it, in PowerShell's syntax and in cmd.exe's,
/// for Windows hosts whose ssh logins don't get the user's own PATH (as with
/// Windows OpenSSH 10.0, which gives them the service's). When attaching,
/// with a terminal, Windows OpenSSH starts the shell through conhost, which
/// splits the command into words and joins them again, quoting only words
/// with spaces; PowerShell then drops those quotes. So PowerShell's form has
/// no double quotes, and cmd.exe's keeps its path, spaces and all, in one.
const MSI_RTERM: [&str; 2] = [
    r"& (Join-Path $env:LOCALAPPDATA 'Programs\Rob Terminal\rterm.exe')",
    r#""%LOCALAPPDATA%\Programs\Rob Terminal\rterm.exe""#,
];
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
    /// Don't serve `rterm get` and `rterm put` for this connection
    #[arg(long)]
    no_transfer: bool,
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

    // The command that runs rterm on the host (see list_sessions).
    let mut rterm = shell_join(std::slice::from_ref(&args.rterm));
    let name = match &args.session {
        Some(n) => {
            crate::paths::validate_name(n).map_err(|e| e.to_string())?;
            n.clone()
        }
        None => {
            let sessions = list_sessions(&args, &mut rterm)?;
            if args.list {
                print_sessions(&sessions, &args.host, &style, false);
                return Ok(0);
            }
            choose(&sessions, &args.host, &style)?
        }
    };

    let endpoint = (!args.no_transfer)
        .then(|| start_transfer_endpoint(&args, &rterm, &name))
        .flatten();
    let mut remote = vec!["attach".into(), name];
    if let Some(key) = &args.detach_key {
        remote.extend(["-e".into(), key.clone()]);
    }
    let mut cmd = Command::new("ssh");
    cmd.args(&args.ssh_args)
        .arg("-t")
        .arg(&args.host)
        .arg(format!("{rterm} {}", shell_join(&remote)));
    let _ignore = IgnoreInterrupts::new();
    let status = cmd.status().map_err(|e| format!("can't run ssh: {e}"));
    if let Some(mut child) = endpoint {
        let _ = child.kill();
        let _ = child.wait();
    }
    Ok(status?.code().unwrap_or(1))
}

/// Start the background process that serves `rterm get` and `rterm put`
/// from the session (see transfer.rs). It reaches the session over its own
/// ssh connection, so it needs a login that doesn't prompt.
fn start_transfer_endpoint(args: &Args, rterm: &str, name: &str) -> Option<std::process::Child> {
    let exe = std::env::current_exe().ok()?;
    let mut cmd = Command::new(exe);
    cmd.arg("__transfer-endpoint")
        .args(["--session", name, "--host", &args.host, "--rterm", rterm])
        .arg("--parent")
        .arg(std::process::id().to_string());
    if let Ok(cwd) = std::env::current_dir() {
        cmd.arg("--cwd").arg(cwd);
    }
    cmd.arg("--")
        .args(&args.ssh_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0); // Ctrl-C at the terminal is for the session
    }
    cmd.spawn().ok()
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

/// Quote words for the host's shell. Plain words stay bare and most others
/// go in double quotes, which POSIX shells, cmd.exe and PowerShell all read
/// the same way; only words with characters special inside double quotes
/// to one of them fall back to POSIX single quotes.
fn shell_join(words: &[String]) -> String {
    words
        .iter()
        .map(|w| {
            let plain = !w.is_empty()
                && w.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./:=@+,".contains(c));
            if plain {
                w.clone()
            } else if !w.chars().any(|c| "\"\\$`%!".contains(c)) {
                format!("\"{w}\"")
            } else {
                format!("'{}'", w.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn ssh_output(args: &Args, command: &str) -> Result<(i32, String, String), String> {
    // Capturing stderr hides no prompts: ssh asks for passwords on the
    // console (Windows) or terminal (Unix) directly.
    let out = Command::new("ssh")
        .args(&args.ssh_args)
        .arg(&args.host)
        .arg(command)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("can't run ssh: {e}"))?;
    Ok((
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

/// Whether cmd.exe or PowerShell said the command doesn't exist.
fn windows_not_found(stderr: &str) -> bool {
    stderr.contains("is not recognized as") || stderr.contains("CommandNotFoundException")
}

/// Separate ssh commands rather than `a || b`, which the host's shell may
/// not understand: Windows OpenSSH runs cmd.exe or PowerShell. On a Windows
/// host where `rterm` isn't found, also try where the MSI installs it, and
/// if that works, set `rterm` to run it from there.
fn list_sessions(args: &Args, rterm: &mut String) -> Result<Vec<Session>, String> {
    let ls = |rterm: &str, json: bool| {
        ssh_output(
            args,
            &format!("{rterm} ls{}", if json { " --json" } else { "" }),
        )
    };
    let (mut code, mut out, mut err) = ls(rterm, true)?;
    // The host's shell answered, and it is cmd.exe or PowerShell.
    let windows =
        ![0, 127, 255].contains(&code) && (windows_not_found(&err) || err.contains("\r\n"));
    if windows && args.rterm == "rterm" {
        let mut tries = MSI_RTERM;
        if err.contains("internal or external command") {
            tries.reverse(); // cmd.exe's message
        }
        for msi in tries {
            let (c, o, _) = ls(msi, true)?;
            if c == 0 {
                (code, out) = (0, o);
                *rterm = msi.to_owned();
                break;
            }
        }
    } else if ![0, 127, 255].contains(&code) && !windows {
        // rterm 0.1.0 doesn't know --json; read its table instead.
        (code, out, err) = ls(rterm, false)?;
    }
    let host = &args.host;
    if code == 0 {
        return Ok(parse_listing(&out));
    }
    if code == 127 {
        return Err(format!(
            "rterm is not installed on {host} (or not on its PATH; see --rterm).\n\
             Install it there with:\n  {INSTALL_HINT}"
        ));
    }
    if windows_not_found(&err) {
        return Err(format!(
            "rterm is not installed on {host} for the account you log in as (or not on \
             its PATH; see --rterm).\n\
             {host} runs Windows: install Rob Terminal there from the MSI at\n  \
             {WINDOWS_INSTALL}\n\
             signed in to Windows as that account (the installer is per-user)."
        ));
    }
    eprint!("{err}");
    match code {
        255 => Err(format!("ssh to {host} failed")),
        c => Err(format!("listing sessions on {host} failed (exit {c})")),
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
        assert_eq!(shell_join(&["^a".into(), "^]".into()]), "\"^a\" \"^]\"");
        assert_eq!(shell_join(&["it's".into()]), "\"it's\"");
    }

    #[test]
    fn msi_forms_survive_attaching() {
        // See MSI_RTERM.
        assert!(!MSI_RTERM[0].contains('"'));
        // contrib/rterm-connect.py runs rterm the same ways.
        let python = include_str!("../contrib/rterm-connect.py");
        for form in MSI_RTERM {
            assert!(python.contains(&form.replace('\\', r"\\")), "{form}");
        }
    }

    #[test]
    fn recognizes_missing_commands_on_windows_hosts() {
        let cmd = "'rterm' is not recognized as an internal or external command,\r\n\
                   operable program or batch file.\r\n";
        let powershell = "rterm : The term 'rterm' is not recognized as the name of a cmdlet, \
                          ...\r\n    + CategoryInfo          : ObjectNotFound: (rterm:String) [], \
                          CommandNotFoundException\r\n";
        assert!(windows_not_found(cmd));
        assert!(windows_not_found(powershell));
        assert!(!windows_not_found(
            "bash: line 1: rterm: command not found\n"
        ));
        assert_eq!(shell_join(&["$it's".into()]), "'$it'\\''s'");
    }
}
