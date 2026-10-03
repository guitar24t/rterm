//! End-to-end tests on Windows: run the real `rterm.exe` on a ConPTY whose
//! other end is an in-memory terminal emulator standing in for the user's
//! terminal (Windows Terminal, say).

#![cfg(windows)]

use std::ffi::{OsStr, c_void};
use std::fs::File;
use std::io::{Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::FromRawHandle;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{mem, ptr, thread};

use alacritty_terminal::Term;
use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::Config;
use alacritty_terminal::vte::ansi::Processor;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, S_OK, WAIT_OBJECT_0};
use windows_sys::Win32::System::Console::{
    COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
    GetExitCodeProcess, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
};

const TIMEOUT: Duration = Duration::from_secs(20);
const DETACH: &str = "\x1c";

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

/// A private session directory per test so tests can run in parallel.
struct Env {
    dir: PathBuf,
}

impl Env {
    fn new() -> Env {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "rt-w-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        Env { dir }
    }

    fn vars(&self) -> Vec<(String, String)> {
        vec![
            ("RTERM_SOCKET_DIR".into(), self.dir.display().to_string()),
            ("RTERM_SHELL".into(), "cmd.exe".into()),
            // cmd.exe shows this as its prompt: "rt>".
            ("PROMPT".into(), "rt$G".into()),
        ]
    }

    /// Run rterm non-interactively. Fails (instead of hanging) if anything
    /// keeps its output pipes open, as a leaked handle in a daemon would.
    fn run(&self, args: &[&str]) -> String {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_rterm"));
        cmd.args(args)
            .envs(self.vars())
            .env_remove("RTERM_SESSION")
            .stdin(Stdio::null());
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(cmd.output());
        });
        let out = rx
            .recv_timeout(Duration::from_secs(30))
            .unwrap_or_else(|_| panic!("rterm {args:?} kept its output open (leaked handle?)"))
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr)
    }

    fn term(&self, rows: u16, cols: u16, args: &[&str]) -> Outer {
        Outer::spawn(env!("CARGO_BIN_EXE_rterm"), args, &self.vars(), rows, cols)
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for e in entries.flatten() {
                if let Some(name) = e.file_name().to_str().and_then(|n| n.strip_suffix(".sock")) {
                    let _ = self.run(&["kill", name]);
                }
            }
        }
        thread::sleep(Duration::from_millis(300));
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

fn quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_owned();
    }
    format!("\"{}\"", arg.replace('"', "\\\""))
}

/// A console window: a ConPTY running a program, rendered by an emulator.
struct Outer {
    console: HPCON,
    input: File,
    process: HANDLE,
    term: Arc<Mutex<Term<Replies>>>,
    raw: Arc<Mutex<Vec<u8>>>,
}

unsafe impl Send for Outer {}

impl Outer {
    fn spawn(
        program: &str,
        args: &[&str],
        vars: &[(String, String)],
        rows: u16,
        cols: u16,
    ) -> Outer {
        unsafe {
            let (mut in_r, mut in_w, mut out_r, mut out_w): (HANDLE, HANDLE, HANDLE, HANDLE) = (
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            );
            assert!(CreatePipe(&mut in_r, &mut in_w, ptr::null(), 0) != 0);
            assert!(CreatePipe(&mut out_r, &mut out_w, ptr::null(), 0) != 0);
            let mut console: HPCON = 0;
            let hr = CreatePseudoConsole(
                COORD {
                    X: cols as i16,
                    Y: rows as i16,
                },
                in_r,
                out_w,
                0,
                &mut console,
            );
            assert_eq!(hr, S_OK);
            CloseHandle(in_r);
            CloseHandle(out_w);

            let mut size = 0usize;
            InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut size);
            let mut buf = vec![0u8; size];
            let attrs = buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
            assert!(InitializeProcThreadAttributeList(attrs, 1, 0, &mut size) != 0);
            assert!(
                UpdateProcThreadAttribute(
                    attrs,
                    0,
                    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                    console as *const c_void,
                    mem::size_of::<HPCON>(),
                    ptr::null_mut(),
                    ptr::null()
                ) != 0
            );
            let mut startup: STARTUPINFOEXW = mem::zeroed();
            startup.StartupInfo.cb = mem::size_of::<STARTUPINFOEXW>() as u32;
            startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            startup.lpAttributeList = attrs;

            let line = std::iter::once(quote(program))
                .chain(args.iter().map(|a| quote(a)))
                .collect::<Vec<_>>()
                .join(" ");
            let mut cmdline = wide(&line);
            // Environment: ours plus the test's variables.
            let mut env: Vec<(String, String)> = std::env::vars()
                .filter(|(k, _)| {
                    !vars.iter().any(|(v, _)| v.eq_ignore_ascii_case(k)) && k != "RTERM_SESSION"
                })
                .collect();
            env.extend(vars.iter().cloned());
            let mut block: Vec<u16> = Vec::new();
            for (k, v) in env {
                block.extend(OsStr::new(&format!("{k}={v}")).encode_wide());
                block.push(0);
            }
            block.push(0);
            let mut info: PROCESS_INFORMATION = mem::zeroed();
            let ok = CreateProcessW(
                ptr::null(),
                cmdline.as_mut_ptr(),
                ptr::null(),
                ptr::null(),
                0,
                EXTENDED_STARTUPINFO_PRESENT
                    | windows_sys::Win32::System::Threading::CREATE_UNICODE_ENVIRONMENT,
                block.as_ptr() as *const c_void,
                ptr::null(),
                &startup.StartupInfo,
                &mut info,
            );
            DeleteProcThreadAttributeList(attrs);
            assert!(
                ok != 0,
                "CreateProcessW: {}",
                std::io::Error::last_os_error()
            );
            CloseHandle(info.hThread);

            let input = File::from_raw_handle(in_w as _);
            let mut output = File::from_raw_handle(out_r as _);
            let replies = Replies::default();
            let term = Arc::new(Mutex::new(Term::new(
                Config {
                    scrolling_history: 100_000,
                    ..Default::default()
                },
                &Size(rows.into(), cols.into()),
                replies.clone(),
            )));
            let raw = Arc::new(Mutex::new(Vec::new()));
            {
                let term = term.clone();
                let raw = raw.clone();
                let mut writer = input.try_clone().unwrap();
                thread::spawn(move || {
                    let mut parser: Processor = Processor::new();
                    let mut buf = [0u8; 65536];
                    loop {
                        let n = match output.read(&mut buf) {
                            Ok(0) | Err(_) => return,
                            Ok(n) => n,
                        };
                        raw.lock().unwrap().extend_from_slice(&buf[..n]);
                        parser.advance(&mut *term.lock().unwrap(), &buf[..n]);
                        let out = std::mem::take(&mut *replies.0.lock().unwrap());
                        if !out.is_empty() {
                            let _ = writer.write_all(&out);
                        }
                    }
                });
            }
            Outer {
                console,
                input,
                process: info.hProcess,
                term,
                raw,
            }
        }
    }

    fn send(&mut self, s: &str) {
        self.input.write_all(s.as_bytes()).unwrap();
    }

    fn lines(&self) -> Vec<String> {
        let term = self.term.lock().unwrap();
        let grid = term.grid();
        (-(grid.history_size() as i32)..grid.screen_lines() as i32)
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

    fn wait_for(&self, what: &str, cond: impl Fn(&Outer) -> bool) {
        let start = Instant::now();
        while start.elapsed() < TIMEOUT {
            if cond(self) {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let raw = self.raw.lock().unwrap();
        panic!(
            "timed out waiting for {what}\n--- screen ---\n{}\n--- raw tail ---\n{:?}",
            self.screen(),
            String::from_utf8_lossy(&raw[raw.len().saturating_sub(800)..])
        );
    }

    fn wait_text(&self, text: &str) {
        self.wait_for(&format!("{text:?} on screen"), |o| {
            o.screen().contains(text)
        });
    }

    /// Wait until some line is exactly `line`.
    fn wait_line(&self, line: &str) {
        self.wait_for(&format!("a line {line:?}"), |o| {
            o.lines().iter().any(|l| l == line)
        });
    }

    fn wait_prompt(&self) {
        self.wait_for("the rt> prompt", |o| {
            o.lines()
                .iter()
                .rev()
                .find(|l| !l.is_empty())
                .is_some_and(|l| l == "rt>")
        });
    }

    fn wait_exit(&self) -> u32 {
        unsafe {
            let r = WaitForSingleObject(self.process, TIMEOUT.as_millis() as u32);
            assert_eq!(r, WAIT_OBJECT_0, "rterm did not exit\n{}", self.screen());
            let mut code = 0u32;
            GetExitCodeProcess(self.process, &mut code);
            thread::sleep(Duration::from_millis(300)); // let the last output arrive
            code
        }
    }

    fn kill_client(&self) {
        unsafe { TerminateProcess(self.process, 1) };
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        self.term
            .lock()
            .unwrap()
            .resize(Size(rows.into(), cols.into()));
        unsafe {
            ResizePseudoConsole(
                self.console,
                COORD {
                    X: cols as i16,
                    Y: rows as i16,
                },
            )
        };
    }
}

impl Drop for Outer {
    fn drop(&mut self) {
        unsafe {
            TerminateProcess(self.process, 1);
            CloseHandle(self.process);
        }
        let console = self.console;
        // ClosePseudoConsole can block until its output is drained.
        thread::spawn(move || unsafe { ClosePseudoConsole(console) });
    }
}

const CMD: &[&str] = &["--", "cmd.exe", "/d"];

fn args<'a>(head: &[&'a str]) -> Vec<&'a str> {
    head.iter().chain(CMD).copied().collect()
}

#[test]
fn detach_and_reattach_restores_the_screen() {
    let env = Env::new();
    let mut a = env.term(24, 100, &args(&["new", "w1"]));
    a.wait_prompt();
    a.send("echo hello-windows\r");
    a.wait_line("hello-windows");
    a.wait_prompt();
    a.send(DETACH);
    assert_eq!(a.wait_exit(), 0);
    assert!(
        a.screen().contains("[rterm: detached from session 'w1']"),
        "{}",
        a.screen()
    );

    let ls = env.run(&["ls"]);
    assert!(ls.contains("w1 ") && ls.contains("detached"), "{ls}");

    let mut b = env.term(30, 110, &["attach", "w1"]);
    b.wait_line("hello-windows");
    b.wait_prompt();
    b.send("echo again\r");
    b.wait_line("again");
    b.send("exit\r");
    assert_eq!(b.wait_exit(), 0);
    assert!(
        b.screen().contains("[rterm: session 'w1' exited]"),
        "{}",
        b.screen()
    );
    assert!(env.run(&["ls"]).contains("no sessions"));
}

#[test]
fn session_survives_its_client_being_killed() {
    let env = Env::new();
    let mut a = env.term(24, 100, &args(&["new", "w2"]));
    a.wait_prompt();
    a.send("echo before-kill\r");
    a.wait_line("before-kill");
    a.kill_client();
    a.wait_exit();
    drop(a);
    thread::sleep(Duration::from_millis(500));
    let b = env.term(24, 100, &["attach", "w2"]);
    b.wait_line("before-kill");
    b.wait_prompt();
    let killed = env.run(&["kill", "w2"]);
    assert!(killed.trim().is_empty(), "{killed}");
    b.wait_exit();
    assert!(env.run(&["ls"]).contains("no sessions"));
}

#[test]
fn background_sessions_list_and_kill() {
    let env = Env::new();
    let out = env.run(&["new", "-d", "bg", "--", "cmd.exe", "/d"]);
    assert!(out.contains("created session 'bg'"), "{out}");
    let json = env.run(&["ls", "--json"]);
    assert!(
        json.starts_with("[{\"name\":\"bg\",\"status\":\"detached\""),
        "{json}"
    );
    assert!(env.run(&["new", "-d", "bg"]).contains("already exists"));
    env.run(&["kill", "bg"]);
    assert_eq!(env.run(&["ls", "--json"]).trim(), "[]");
}

#[test]
fn resize_reaches_the_session() {
    let env = Env::new();
    let mut a = env.term(24, 100, &args(&["new", "w3"]));
    a.wait_prompt();
    a.resize(33, 111);
    thread::sleep(Duration::from_millis(800));
    a.send("mode con\r");
    a.wait_text("111");
    a.wait_text("33");
    a.send("exit\r");
    a.wait_exit();
}

#[test]
fn rterm_connect_name_runs_the_session_picker() {
    let dir = Env::new();
    std::fs::create_dir_all(&dir.dir).unwrap();
    let exe = dir.dir.join("rterm-connect.exe");
    std::fs::copy(env!("CARGO_BIN_EXE_rterm"), &exe).unwrap();
    let out = Command::new(&exe).arg("--help").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("Choose an rterm session"), "{text}");
}
