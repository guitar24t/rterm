//! End-to-end tests: run the real `rterm` binary on a PTY whose other end is
//! an in-memory terminal emulator standing in for the user's terminal.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use alacritty_terminal::Term;
use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::{Config, TermMode};
use alacritty_terminal::vte::ansi::Processor;

const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Default)]
struct Replies(Arc<Mutex<Vec<u8>>>);

impl EventListener for Replies {
    fn send_event(&self, event: Event) {
        if let Event::PtyWrite(s) = event {
            self.0.lock().unwrap().extend_from_slice(s.as_bytes());
        }
    }
}

struct Size(usize, usize);

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.0
    }
    fn screen_lines(&self) -> usize {
        self.0
    }
    fn columns(&self) -> usize {
        self.1
    }
}

/// A fresh socket directory per test so tests can run in parallel.
struct Env {
    dir: PathBuf,
}

impl Env {
    fn new() -> Env {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "rt-e2e-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        Env { dir }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_rterm"));
        cmd.args(args)
            .env("RTERM_SOCKET_DIR", &self.dir)
            .env("SHELL", "/bin/bash")
            .env("TERM", "xterm-256color")
            .env("PS1", "$ ")
            .env("BASH_SILENCE_DEPRECATION_WARNING", "1")
            .env("HISTFILE", "/dev/null")
            .env_remove("RTERM_SESSION")
            .env_remove("RTERM_DETACH_KEY")
            .env_remove("PROMPT_COMMAND");
        cmd
    }

    /// Run a non-interactive rterm command and return its stdout.
    fn run(&self, args: &[&str]) -> String {
        let out = self.command(args).stdin(Stdio::null()).output().unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr)
    }

    /// Start rterm in a new terminal of the given size.
    fn term(&self, rows: u16, cols: u16, args: &[&str]) -> Outer {
        Outer::spawn(self.command(args), rows, cols)
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        // Kill whatever sessions a failed test left behind.
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for e in entries.flatten() {
                if let Some(name) = e.file_name().to_str().and_then(|n| n.strip_suffix(".sock")) {
                    let _ = self.command(&["kill", name]).stdin(Stdio::null()).output();
                }
            }
        }
        thread::sleep(Duration::from_millis(100));
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A terminal window: a PTY running rterm, rendered by an emulator.
struct Outer {
    master: File,
    term: Arc<Mutex<Term<Replies>>>,
    child: Child,
    _reader: thread::JoinHandle<()>,
    raw: Arc<Mutex<Vec<u8>>>,
}

impl Outer {
    fn spawn(mut cmd: Command, rows: u16, cols: u16) -> Outer {
        let ws = nix::pty::Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pty = nix::pty::openpty(Some(&ws), None).unwrap();
        let slave: OwnedFd = pty.slave;
        cmd.stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                libc::ioctl(0, libc::TIOCSCTTY as _, 0);
                Ok(())
            });
        }
        let child = cmd.spawn().unwrap();
        let master = File::from(pty.master);

        let replies = Replies::default();
        let config = Config {
            kitty_keyboard: true,
            scrolling_history: 100_000,
            ..Default::default()
        };
        let term = Arc::new(Mutex::new(Term::new(
            config,
            &Size(rows.into(), cols.into()),
            replies.clone(),
        )));
        let raw = Arc::new(Mutex::new(Vec::new()));
        let reader = {
            let term = term.clone();
            let raw = raw.clone();
            let mut rd = master.try_clone().unwrap();
            let mut wr = master.try_clone().unwrap();
            thread::spawn(move || {
                let mut parser: Processor = Processor::new();
                let mut buf = [0u8; 65536];
                loop {
                    let n = match rd.read(&mut buf) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    raw.lock().unwrap().extend_from_slice(&buf[..n]);
                    parser.advance(&mut *term.lock().unwrap(), &buf[..n]);
                    let out = std::mem::take(&mut *replies.0.lock().unwrap());
                    if !out.is_empty() {
                        let _ = wr.write_all(&out);
                    }
                }
            })
        };
        Outer {
            master,
            term,
            child,
            _reader: reader,
            raw,
        }
    }

    fn send(&mut self, s: &str) {
        self.master.write_all(s.as_bytes()).unwrap();
    }

    fn lines(&self) -> Vec<String> {
        let term = self.term.lock().unwrap();
        let grid = term.grid();
        (0..grid.screen_lines() as i32)
            .map(|l| {
                let row = &grid[Line(l)];
                (0..grid.columns())
                    .map(|c| row[Column(c)].c)
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    fn history(&self) -> Vec<String> {
        let term = self.term.lock().unwrap();
        let grid = term.grid();
        (-(grid.history_size() as i32)..0)
            .map(|l| {
                let row = &grid[Line(l)];
                (0..grid.columns())
                    .map(|c| row[Column(c)].c)
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    fn screen(&self) -> String {
        self.lines().join("\n")
    }

    fn mode(&self) -> TermMode {
        *self.term.lock().unwrap().mode()
    }

    fn cursor(&self) -> (i32, usize) {
        let t = self.term.lock().unwrap();
        let p = t.grid().cursor.point;
        (p.line.0, p.column.0)
    }

    fn wait_for(&self, what: &str, cond: impl Fn(&Outer) -> bool) {
        let start = Instant::now();
        while start.elapsed() < TIMEOUT {
            if cond(self) {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!(
            "timed out waiting for {what}\n--- screen ---\n{}\n--- raw tail ---\n{:?}",
            self.screen(),
            String::from_utf8_lossy(&{
                let raw = self.raw.lock().unwrap();
                raw[raw.len().saturating_sub(600)..].to_vec()
            })
        );
    }

    fn wait_text(&self, text: &str) {
        self.wait_for(&format!("{text:?} on screen"), |o| {
            o.screen().contains(text)
        });
    }

    /// Wait for the prompt to be the last non-empty line.
    fn wait_prompt(&self) {
        self.wait_for("a prompt", |o| {
            let lines = o.lines();
            let last = lines.iter().rev().find(|l| !l.is_empty());
            last.is_some_and(|l| l == "$") && {
                let (line, col) = o.cursor();
                lines.get(line as usize).is_some_and(|l| l == "$") && col == 2
            }
        });
    }

    fn wait_exit(&mut self) -> i32 {
        let start = Instant::now();
        while start.elapsed() < TIMEOUT {
            if let Some(status) = self.child.try_wait().unwrap() {
                // Let the reader catch the final output.
                thread::sleep(Duration::from_millis(100));
                return status.code().unwrap_or(-1);
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("rterm did not exit\n{}", self.screen());
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        self.term
            .lock()
            .unwrap()
            .resize(Size(rows.into(), cols.into()));
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
    }
}

impl Drop for Outer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

const BASH: &[&str] = &["--", "/bin/bash", "--norc", "--noprofile"];

fn args<'a>(head: &[&'a str]) -> Vec<&'a str> {
    head.iter().chain(BASH).copied().collect()
}

const DETACH: &str = "\x1c";

#[test]
fn detach_and_reattach_restores_screen_and_history() {
    let env = Env::new();
    let mut a = env.term(24, 80, &args(&["new", "s"]));
    a.wait_prompt();
    a.send("for i in $(seq 1 60); do echo line-$i; done\r");
    a.wait_text("line-60");
    a.wait_prompt();
    a.send("echo hello-rterm\r");
    a.wait_text("hello-rterm\n$");
    a.send(DETACH);
    assert_eq!(a.wait_exit(), 0);
    assert!(
        a.screen().contains("[rterm: detached from session 's']"),
        "{}",
        a.screen()
    );
    // Back at a normal terminal: cursor visible, no leftover modes.
    assert!(a.mode().contains(TermMode::SHOW_CURSOR));
    assert!(!a.mode().intersects(
        TermMode::MOUSE_MODE | TermMode::ALT_SCREEN | TermMode::KITTY_KEYBOARD_PROTOCOL
    ));

    let ls = env.run(&["ls"]);
    assert!(ls.contains("s ") && ls.contains("detached"), "{ls}");

    // Reattach from a different, bigger terminal.
    let mut b = env.term(30, 100, &["attach", "s"]);
    b.wait_text("hello-rterm");
    b.wait_prompt();
    // The whole session history is in the new terminal's native scrollback.
    let all: Vec<String> = b.history().into_iter().chain(b.lines()).collect();
    for i in 1..=60 {
        assert!(all.contains(&format!("line-{i}")), "missing line-{i}");
    }
    assert!(env.run(&["ls"]).contains("attached"));

    b.send("stty size\r");
    b.wait_text("30 100");
    b.send("exit 3\r");
    assert_eq!(b.wait_exit(), 3);
    assert!(
        b.screen().contains("[rterm: session 's' exited]"),
        "{}",
        b.screen()
    );
    assert!(env.run(&["ls"]).contains("no sessions"));
}

#[test]
fn alternate_screen_survives_reattach() {
    let env = Env::new();
    let mut a = env.term(20, 60, &args(&["new", "alt"]));
    a.wait_prompt();
    a.send("echo before-alt\r");
    a.wait_prompt();
    a.send("printf '\\033[?1049h\\033[H\\033[2JFULLSCREEN-APP\\033[5;3Hrow5'; read x; printf '\\033[?1049l'; echo after-alt\r");
    a.wait_text("FULLSCREEN-APP");
    assert!(a.mode().contains(TermMode::ALT_SCREEN));
    a.send(DETACH);
    a.wait_exit();
    // Detaching left the alternate screen and returned to the shell's view.
    assert!(!a.mode().contains(TermMode::ALT_SCREEN));
    assert!(a.screen().contains("before-alt"));

    let mut b = env.term(20, 60, &["attach", "alt"]);
    b.wait_text("FULLSCREEN-APP");
    assert!(b.mode().contains(TermMode::ALT_SCREEN));
    assert_eq!(b.lines()[4], "  row5");
    // Finish the "app": the primary screen underneath must be the session's.
    b.send("\r");
    b.wait_text("after-alt");
    assert!(!b.mode().contains(TermMode::ALT_SCREEN));
    let screen = b.screen();
    assert!(screen.contains("before-alt"), "{screen}");
    let before = screen.find("before-alt").unwrap();
    let after = screen.find("after-alt").unwrap();
    assert!(before < after, "{screen}");
    b.send("exit\r");
    b.wait_exit();
}

#[test]
fn keyboard_and_mouse_modes_follow_the_session() {
    let env = Env::new();
    let mut a = env.term(20, 60, &args(&["new", "modes"]));
    a.wait_prompt();
    // A program enabling kitty keys, mouse reporting and focus events, then
    // waiting for input (like Codex or vim would).
    a.send("printf '\\033[>1u\\033[?1000h\\033[?1006h\\033[?1004h'; echo MODES-ON; read x; printf '\\033[<1u\\033[?1000l\\033[?1006l\\033[?1004l'; echo MODES-OFF\r");
    a.wait_text("MODES-ON");
    a.wait_for("kitty mode", |o| {
        o.mode().contains(TermMode::DISAMBIGUATE_ESC_CODES)
    });
    assert!(a.mode().contains(TermMode::MOUSE_REPORT_CLICK));
    // With kitty keys on, the terminal sends Ctrl-\ as CSI 92;5u.
    a.send("\x1b[92;5u");
    a.wait_exit();
    let m = a.mode();
    assert!(
        !m.intersects(TermMode::KITTY_KEYBOARD_PROTOCOL),
        "kitty flags left on: {m:?}"
    );
    assert!(
        !m.intersects(TermMode::MOUSE_MODE | TermMode::SGR_MOUSE | TermMode::FOCUS_IN_OUT),
        "{m:?}"
    );

    let mut b = env.term(20, 60, &["attach", "modes"]);
    b.wait_text("MODES-ON");
    b.wait_for("restored modes", |o| {
        let m = o.mode();
        m.contains(TermMode::DISAMBIGUATE_ESC_CODES)
            && m.contains(TermMode::MOUSE_REPORT_CLICK)
            && m.contains(TermMode::SGR_MOUSE)
            && m.contains(TermMode::FOCUS_IN_OUT)
    });
    b.send("\r");
    b.wait_text("MODES-OFF");
    b.wait_for("modes off", |o| {
        !o.mode()
            .intersects(TermMode::KITTY_KEYBOARD_PROTOCOL | TermMode::MOUSE_MODE)
    });
    b.send("exit\r");
    b.wait_exit();
}

#[test]
fn second_attach_takes_over() {
    let env = Env::new();
    let mut a = env.term(24, 80, &args(&["new", "steal"]));
    a.wait_prompt();
    let mut b = env.term(24, 80, &["attach", "steal"]);
    b.wait_prompt();
    assert_eq!(a.wait_exit(), 0);
    assert!(
        a.screen().contains("attached from another terminal"),
        "{}",
        a.screen()
    );
    b.send("echo still-here\r");
    b.wait_text("still-here");
    b.send("exit\r");
    b.wait_exit();
}

#[test]
fn session_survives_terminal_hangup() {
    let env = Env::new();
    let mut a = env.term(24, 80, &args(&["new", "hup"]));
    a.wait_prompt();
    a.send("sleep 1; echo finished-while-away\r");
    // Simulate the ssh connection dropping: the terminal goes away.
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(a.child.id() as i32),
        nix::sys::signal::Signal::SIGHUP,
    );
    a.wait_exit();
    drop(a);
    thread::sleep(Duration::from_millis(1500));
    let mut b = env.term(24, 80, &["attach", "hup"]);
    b.wait_text("finished-while-away");
    b.wait_prompt();
    b.send("exit\r");
    b.wait_exit();
}

#[test]
fn interrupt_and_kill() {
    let env = Env::new();
    let mut a = env.term(24, 80, &args(&["new", "k"]));
    a.wait_prompt();
    a.send("sleep 100\r");
    thread::sleep(Duration::from_millis(300));
    a.send("\x03");
    a.wait_prompt();
    // `rterm detach` and `rterm kill` from another terminal.
    assert_eq!(env.run(&["detach", "k"]).trim(), "");
    a.wait_exit();
    assert!(
        a.screen().contains("detached by `rterm detach`"),
        "{}",
        a.screen()
    );
    let mut b = env.term(24, 80, &["attach", "k"]);
    b.wait_prompt();
    env.run(&["kill", "k"]);
    b.wait_exit();
    assert!(b.screen().contains("exited"), "{}", b.screen());
    assert!(env.run(&["ls"]).contains("no sessions"));
}

#[test]
fn resize_reaches_the_session() {
    let env = Env::new();
    let mut a = env.term(24, 80, &args(&["new", "rs"]));
    a.wait_prompt();
    a.resize(33, 111);
    thread::sleep(Duration::from_millis(200));
    a.send("stty size\r");
    a.wait_text("33 111");
    a.send("exit\r");
    a.wait_exit();
}

#[test]
fn large_output_and_scrollback_replay() {
    let env = Env::new();
    let mut a = env.term(24, 80, &args(&["new", "big", "--scrollback", "5000"]));
    a.wait_prompt();
    a.send("seq 1 200000\r");
    a.wait_text("200000");
    a.wait_prompt();
    a.send(DETACH);
    a.wait_exit();
    let mut b = env.term(24, 80, &["attach", "big"]);
    b.wait_text("200000");
    b.wait_prompt();
    let hist = b.history();
    let replayed: Vec<&String> = hist.iter().filter(|l| l.parse::<u32>().is_ok()).collect();
    assert!(
        replayed.len() >= 4900,
        "only {} lines replayed",
        replayed.len()
    );
    assert!(hist.contains(&"196000".to_owned()));
    assert!(b.lines().contains(&"199990".to_owned()));
    b.send("exit\r");
    b.wait_exit();
}

#[test]
fn attach_existing_only_and_name_validation() {
    let env = Env::new();
    let out = env.run(&["attach", "-x", "nope"]);
    assert!(
        out.contains("rterm needs a terminal") || out.contains("no session"),
        "{out}"
    );
    let mut t = env.term(24, 80, &["attach", "-x", "nope"]);
    assert_eq!(t.wait_exit(), 1);
    assert!(
        t.screen().contains("no session named \"nope\""),
        "{}",
        t.screen()
    );
    let out = env.run(&["attach", "../evil"]);
    assert!(out.contains("invalid session name"), "{out}");
}

#[test]
fn new_detached_then_attach() {
    let env = Env::new();
    let out = env.run(&args(&["new", "-d", "bg"]));
    assert!(out.contains("created session 'bg'"), "{out}");
    let out = env.run(&args(&["new", "-d", "bg"]));
    assert!(out.contains("already exists"), "{out}");
    let mut a = env.term(24, 80, &["bg"]);
    a.wait_prompt();
    a.send("echo $RTERM_SESSION\r");
    a.wait_text("\nbg\n");
    a.send("exit\r");
    a.wait_exit();
    assert!(!Path::new(&env.dir).join("bg.sock").exists());
}

impl Outer {
    /// Wait until the screen stops changing for `quiet`.
    fn settle(&self, quiet: Duration) {
        let start = Instant::now();
        let mut last = (self.screen(), self.cursor());
        let mut since = Instant::now();
        while start.elapsed() < Duration::from_secs(30) {
            thread::sleep(Duration::from_millis(50));
            let now = (self.screen(), self.cursor());
            if now != last {
                last = now;
                since = Instant::now();
            } else if since.elapsed() >= quiet {
                return;
            }
        }
    }

    /// Press Ctrl-\ the way the terminal would currently encode it.
    fn press_detach(&mut self) {
        if self.mode().intersects(TermMode::KITTY_KEYBOARD_PROTOCOL) {
            self.send("\x1b[92;5u");
        } else {
            self.send(DETACH);
        }
    }
}

/// Run a real full-screen or inline TUI in a session, detach, reattach in a
/// new terminal and require an identical screen. Opt-in, e.g.:
///   RTERM_E2E_APP='vim -u NONE' cargo test --test e2e real_app -- --ignored --nocapture
#[test]
#[ignore]
fn real_app_roundtrip() {
    let app = std::env::var("RTERM_E2E_APP").unwrap_or_else(|_| "vim -u NONE".into());
    let keys = std::env::var("RTERM_E2E_KEYS").unwrap_or_else(|_| "ihello from rterm".into());
    let workdir = std::env::var("RTERM_E2E_DIR")
        .unwrap_or_else(|_| std::env::temp_dir().display().to_string());
    let env = Env::new();
    let mut a = env.term(30, 100, &args(&["new", "app"]));
    a.wait_prompt();
    a.send(&format!("cd '{workdir}' && clear && {app}\r"));
    thread::sleep(Duration::from_secs(2));
    a.settle(Duration::from_millis(2500));
    a.send(&keys);
    a.settle(Duration::from_millis(1500));
    let before = a.lines();
    let before_cursor = a.cursor();
    let before_mode = a.mode();
    println!(
        "--- before detach (mode {before_mode:?}) ---\n{}",
        before.join("\n")
    );

    a.press_detach();
    a.wait_exit();
    let m = a.mode();
    println!("--- after detach (mode {m:?}) ---\n{}", a.screen());
    assert!(
        !m.intersects(
            TermMode::KITTY_KEYBOARD_PROTOCOL
                | TermMode::MOUSE_MODE
                | TermMode::ALT_SCREEN
                | TermMode::BRACKETED_PASTE
                | TermMode::APP_CURSOR
                | TermMode::FOCUS_IN_OUT
        ),
        "terminal not reset after detach: {m:?}"
    );
    assert!(m.contains(TermMode::SHOW_CURSOR));

    let b = env.term(30, 100, &["attach", "app"]);
    b.settle(Duration::from_millis(2000));
    let after = b.lines();
    println!(
        "--- after reattach (mode {:?}) ---\n{}",
        b.mode(),
        after.join("\n")
    );
    for (i, (x, y)) in before.iter().zip(&after).enumerate() {
        assert_eq!(x, y, "line {i} differs");
    }
    assert_eq!(before_cursor, b.cursor(), "cursor");
    let relevant = TermMode::KITTY_KEYBOARD_PROTOCOL
        | TermMode::MOUSE_MODE
        | TermMode::SGR_MOUSE
        | TermMode::ALT_SCREEN
        | TermMode::BRACKETED_PASTE
        | TermMode::APP_CURSOR
        | TermMode::APP_KEYPAD
        | TermMode::FOCUS_IN_OUT
        | TermMode::SHOW_CURSOR;
    assert_eq!(before_mode & relevant, b.mode() & relevant, "modes");

    // The reattached session is live, not just a picture.
    let typed = std::env::var("RTERM_E2E_AFTER").unwrap_or_else(|_| "typed-after-reattach".into());
    let mut b = b;
    b.send(&typed);
    b.wait_text(&typed);
    println!("--- after typing ---\n{}", b.screen());
    env.run(&["kill", "app"]);
}

impl Env {
    /// A terminal running a plain bash (prompt "% ") with rterm configured,
    /// i.e. the user's own shell before they run rterm.
    fn shell(&self, rows: u16, cols: u16) -> Outer {
        let mut cmd = self.command(&[]);
        let bin_dir = Path::new(env!("CARGO_BIN_EXE_rterm"))
            .parent()
            .unwrap()
            .to_owned();
        let path = format!(
            "{}:{}",
            bin_dir.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        cmd = {
            let mut c = Command::new("/bin/bash");
            c.args(["--norc", "--noprofile"]);
            for (k, v) in cmd.get_envs() {
                match v {
                    Some(v) => c.env(k, v),
                    None => c.env_remove(k),
                };
            }
            c.env("PS1", "% ").env("PATH", path);
            c
        };
        Outer::spawn(cmd, rows, cols)
    }
}

#[test]
fn detach_swallows_kitty_key_releases() {
    let env = Env::new();
    let mut t = env.shell(24, 80);
    t.wait_text("%");
    t.send("PS1='$ ' rterm new kr -- /bin/bash --norc --noprofile\r");
    t.wait_prompt();
    // An app using kitty keys with release events (flags 1|2), like Codex.
    t.send("printf '\\033[>3u'; echo KITTY-APP; read x\r");
    t.wait_text("KITTY-APP");
    t.wait_for("kitty mode", |o| {
        o.mode().contains(TermMode::REPORT_EVENT_TYPES)
    });
    // Press Ctrl-\, then the key and Ctrl releases arrive a bit later.
    t.send("\x1b[57442;5u\x1b[92;5u");
    thread::sleep(Duration::from_millis(60));
    t.send("\x1b[92;5:3u\x1b[57442;1:3u");
    t.wait_text("[rterm: detached from session 'kr']");
    t.wait_for("outer prompt", |o| o.lines().iter().any(|l| l == "%"));
    thread::sleep(Duration::from_millis(300));
    let screen = t.screen();
    assert!(
        !screen.contains("92;5"),
        "key release leaked into the shell:\n{screen}"
    );
    assert!(!t.mode().intersects(TermMode::KITTY_KEYBOARD_PROTOCOL));
    t.send("echo outer-ok\r");
    t.wait_text("\nouter-ok\n");
    env.run(&["kill", "kr"]);
}

#[test]
fn sigterm_to_client_detaches_cleanly() {
    let env = Env::new();
    let mut a = env.term(24, 80, &args(&["new", "term"]));
    a.wait_prompt();
    a.send("printf '\\033[?1000h\\033[?2004h'; echo MOUSE-ON; read x\r");
    a.wait_text("MOUSE-ON");
    a.wait_for("mouse mode", |o| {
        o.mode().contains(TermMode::MOUSE_REPORT_CLICK)
    });
    let pid = nix::unistd::Pid::from_raw(a.child.id() as i32);
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM).unwrap();
    assert_eq!(a.wait_exit(), 0);
    assert!(
        a.screen().contains("detached from session 'term'"),
        "{}",
        a.screen()
    );
    assert!(
        !a.mode()
            .intersects(TermMode::MOUSE_MODE | TermMode::BRACKETED_PASTE)
    );
    assert!(env.run(&["ls"]).contains("detached"));
}

#[test]
fn daemon_does_not_hold_inherited_fds() {
    let env = Env::new();
    // Like `ssh host rterm new -d x`: the caller waits for EOF on a pipe
    // that rterm inherited. A daemon keeping it open would hang the caller.
    let mut cmd = Command::new("/bin/bash");
    cmd.arg("-c").arg(format!(
        "exec 7>&1; '{}' new -d leak -- /bin/sh",
        env!("CARGO_BIN_EXE_rterm")
    ));
    for (k, v) in env.command(&[]).get_envs() {
        if let Some(v) = v {
            cmd.env(k, v);
        }
    }
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(cmd.stdin(Stdio::null()).output());
    });
    let out = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("caller hung on an fd held by the daemon");
    assert!(String::from_utf8_lossy(&out.unwrap().stdout).contains("created session 'leak'"));
    env.run(&["kill", "leak"]);
}

#[test]
fn daemon_reports_incompatible_protocol() {
    use std::os::unix::net::UnixStream;
    let env = Env::new();
    let out = env.run(&["new", "-d", "proto", "--", "/bin/sh"]);
    assert!(out.contains("created"), "{out}");
    let mut sock = UnixStream::connect(env.dir.join("proto.sock")).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    // An Attach frame from a hypothetical future client (protocol v999).
    let mut payload = 999u32.to_be_bytes().to_vec();
    payload.extend_from_slice(&[0, 24, 0, 80, 0, 0, 0, 0]);
    payload.extend_from_slice(&0u32.to_be_bytes());
    let mut frame = vec![1u8];
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    sock.write_all(&frame).unwrap();
    let mut reply = [0u8; 9];
    sock.read_exact(&mut reply).unwrap();
    assert_eq!(reply[0], 106, "expected an Incompatible reply");
    assert_eq!(u32::from_be_bytes(reply[5..9].try_into().unwrap()), 1);
    env.run(&["kill", "proto"]);
}
