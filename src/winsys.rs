//! Windows plumbing: the console in raw VT mode, ConPTY sessions, job
//! objects, and a few process and file helpers.

use std::ffi::{OsStr, OsString, c_void};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::{mem, ptr};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION, ERROR_SUCCESS, HANDLE,
    HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, S_OK, SetHandleInformation,
};
use windows_sys::Win32::System::Console::{
    CONSOLE_MODE, CONSOLE_SCREEN_BUFFER_INFO, COORD, CTRL_BREAK_EVENT, CTRL_C_EVENT,
    ClosePseudoConsole, CreatePseudoConsole, DISABLE_NEWLINE_AUTO_RETURN, ENABLE_ECHO_INPUT,
    ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT, ENABLE_PROCESSED_OUTPUT,
    ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode,
    GetConsoleScreenBufferInfo, GetStdHandle, HPCON, ReadConsoleW, ResizePseudoConsole, STD_HANDLE,
    STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleCtrlHandler, SetConsoleMode, SetStdHandle,
    WriteConsoleW,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
use windows_sys::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED,
    CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DETACHED_PROCESS, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES,
    STARTUPINFOEXW, UpdateProcThreadAttribute, WaitForSingleObject,
};

use crate::protocol::WinSize;

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(Some(0)).collect()
}

fn std_console_handle(which: STD_HANDLE) -> Option<HANDLE> {
    let h = unsafe { GetStdHandle(which) };
    if h.is_null() || h == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut mode: CONSOLE_MODE = 0;
    (unsafe { GetConsoleMode(h, &mut mode) } != 0).then_some(h)
}

pub fn is_console() -> bool {
    std_console_handle(STD_INPUT_HANDLE).is_some()
        && std_console_handle(STD_OUTPUT_HANDLE).is_some()
}

/// The visible size of the console window.
pub fn console_size() -> Option<WinSize> {
    let h = std_console_handle(STD_OUTPUT_HANDLE)?;
    let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { mem::zeroed() };
    if unsafe { GetConsoleScreenBufferInfo(h, &mut info) } == 0 {
        return None;
    }
    let w = &info.srWindow;
    Some(WinSize {
        rows: (w.Bottom - w.Top + 1).max(1) as u16,
        cols: (w.Right - w.Left + 1).max(1) as u16,
        xpix: 0,
        ypix: 0,
    })
}

/// Let VT escape sequences (colors) through to the console.
pub fn enable_vt_output() {
    if let Some(h) = std_console_handle(STD_OUTPUT_HANDLE) {
        let mut mode: CONSOLE_MODE = 0;
        unsafe {
            GetConsoleMode(h, &mut mode);
            SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        }
    }
}

/// Ignore (or stop ignoring) Ctrl-C in this process, leaving it to a child.
pub fn ignore_ctrl_c(ignore: bool) {
    unsafe { SetConsoleCtrlHandler(None, ignore as i32) };
}

unsafe extern "system" fn swallow_interrupts(ctrl: u32) -> i32 {
    (ctrl == CTRL_C_EVENT || ctrl == CTRL_BREAK_EVENT) as i32
}

/// While attached, Ctrl-C arrives as input for the session; make sure
/// Ctrl-Break can't kill the client either.
pub fn ignore_interrupts() {
    unsafe { SetConsoleCtrlHandler(Some(swallow_interrupts), 1) };
}

/// The console in raw VT mode: keys arrive as the bytes a Unix terminal
/// would send, and escape sequences we write are interpreted. Restores the
/// previous modes when dropped.
pub struct Console {
    input: HANDLE,
    output: HANDLE,
    saved_input: CONSOLE_MODE,
    saved_output: CONSOLE_MODE,
    utf8_carry: Vec<u8>,
}

// The handles are process-wide console handles.
unsafe impl Send for Console {}

impl Console {
    pub fn enter_raw() -> io::Result<Console> {
        let not_console = || io::Error::other("not a console");
        let input = std_console_handle(STD_INPUT_HANDLE).ok_or_else(not_console)?;
        let output = std_console_handle(STD_OUTPUT_HANDLE).ok_or_else(not_console)?;
        let (mut saved_input, mut saved_output): (CONSOLE_MODE, CONSOLE_MODE) = (0, 0);
        unsafe {
            GetConsoleMode(input, &mut saved_input);
            GetConsoleMode(output, &mut saved_output);
        }
        let raw_input = (saved_input
            & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT))
            | ENABLE_VIRTUAL_TERMINAL_INPUT;
        let raw_output = saved_output
            | ENABLE_PROCESSED_OUTPUT
            | ENABLE_VIRTUAL_TERMINAL_PROCESSING
            | DISABLE_NEWLINE_AUTO_RETURN;
        if unsafe { SetConsoleMode(input, raw_input) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { SetConsoleMode(output, raw_output) } == 0 {
            let e = io::Error::last_os_error();
            unsafe { SetConsoleMode(input, saved_input) };
            return Err(e);
        }
        Ok(Console {
            input,
            output,
            saved_input,
            saved_output,
            utf8_carry: Vec::new(),
        })
    }

    pub fn reader(&self) -> ConsoleReader {
        ConsoleReader {
            input: self.input,
            pending: None,
        }
    }

    /// Write UTF-8 output, which may end in the middle of a character.
    pub fn write(&mut self, data: &[u8]) -> io::Result<()> {
        self.utf8_carry.extend_from_slice(data);
        let mut text = String::new();
        let mut rest: &[u8] = &self.utf8_carry;
        loop {
            match std::str::from_utf8(rest) {
                Ok(s) => {
                    text.push_str(s);
                    rest = &[];
                    break;
                }
                Err(e) => {
                    let (valid, after) = rest.split_at(e.valid_up_to());
                    text.push_str(std::str::from_utf8(valid).unwrap());
                    match e.error_len() {
                        Some(n) => {
                            text.push('\u{FFFD}');
                            rest = &after[n..];
                        }
                        None => {
                            rest = after; // incomplete character: wait for the rest
                            break;
                        }
                    }
                }
            }
        }
        self.utf8_carry = rest.to_vec();
        let units: Vec<u16> = text.encode_utf16().collect();
        let mut done = 0;
        while done < units.len() {
            let chunk = &units[done..units.len().min(done + 16 * 1024)];
            let mut written = 0u32;
            if unsafe {
                WriteConsoleW(
                    self.output,
                    chunk.as_ptr(),
                    chunk.len() as u32,
                    &mut written,
                    ptr::null(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            done += written.max(1) as usize;
        }
        Ok(())
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        unsafe {
            SetConsoleMode(self.input, self.saved_input);
            SetConsoleMode(self.output, self.saved_output);
        }
    }
}

/// Blocking keyboard reader for a raw-mode console.
pub struct ConsoleReader {
    input: HANDLE,
    pending: Option<u16>,
}

unsafe impl Send for ConsoleReader {}

impl ConsoleReader {
    /// Wait for input; returns it as UTF-8 (special keys as VT sequences).
    pub fn read(&mut self) -> io::Result<Vec<u8>> {
        let mut buf = [0u16; 4096];
        let mut n = 0u32;
        if unsafe {
            ReadConsoleW(
                self.input,
                buf.as_mut_ptr().cast(),
                buf.len() as u32,
                &mut n,
                ptr::null(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let mut units: Vec<u16> = self.pending.take().into_iter().collect();
        units.extend_from_slice(&buf[..n as usize]);
        if units.last().is_some_and(|u| (0xD800..0xDC00).contains(u)) {
            self.pending = units.pop(); // high surrogate; its pair comes next
        }
        let text: String = char::decode_utf16(units)
            .map(|r| r.unwrap_or('\u{FFFD}'))
            .collect();
        Ok(text.into_bytes())
    }
}

/// After the daemon has reported that it is ready, stop holding the pipe the
/// client is reading from.
pub fn detach_stdout() {
    unsafe {
        let old = GetStdHandle(STD_OUTPUT_HANDLE);
        if let Ok(nul) = OpenOptions::new().write(true).open("NUL") {
            SetStdHandle(STD_OUTPUT_HANDLE, nul.into_raw_handle() as HANDLE);
        }
        if !old.is_null() && old != INVALID_HANDLE_VALUE {
            CloseHandle(old);
        }
    }
}

/// Open `path` exclusively; it stays locked while the file is open.
pub fn lock_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(path)
}

pub fn is_sharing_violation(e: &io::Error) -> bool {
    e.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32)
}

/// A copy of this executable for session daemons to run from. A running
/// .exe can't be replaced, so daemons never run the installed file: upgrades
/// then never collide with a session that's still running.
pub fn session_exe(dir: &Path) -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let meta = fs::metadata(&exe)?;
    let stamp = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    let bin = dir.join("bin");
    fs::create_dir_all(&bin)?;
    let name = format!(
        "rterm-{}-{:x}-{:x}.exe",
        env!("CARGO_PKG_VERSION"),
        meta.len(),
        stamp
    );
    let target = bin.join(&name);
    if !target.exists() {
        let tmp = bin.join(format!("{name}.{}.tmp", std::process::id()));
        fs::copy(&exe, &tmp)?;
        if fs::rename(&tmp, &target).is_err() {
            let _ = fs::remove_file(&tmp);
        }
        // Drop copies no session is using any more (in-use ones can't be
        // deleted, which is exactly what we want).
        for entry in fs::read_dir(&bin)?.flatten() {
            if entry.file_name() != OsStr::new(&name) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    Ok(target)
}

fn registry_string(subkey: &str, value: &str) -> Option<OsString> {
    let (subkey, value) = (wide(OsStr::new(subkey)), wide(OsStr::new(value)));
    let mut size = 0u32;
    let query = |data: *mut c_void, size: &mut u32| unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            ptr::null_mut(),
            data,
            size,
        )
    };
    if query(ptr::null_mut(), &mut size) != ERROR_SUCCESS || size == 0 {
        return None;
    }
    let mut buf = vec![0u16; size as usize / 2 + 1];
    if query(buf.as_mut_ptr().cast(), &mut size) != ERROR_SUCCESS {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(OsString::from_wide(&buf[..len]))
}

fn find_in_path(program: &str) -> Option<OsString> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|p| p.is_file())
        .map(PathBuf::into_os_string)
}

/// The shell a new session starts: `RTERM_SHELL`, else OpenSSH's configured
/// default shell (as an ssh login would get), else PowerShell, else cmd.
pub fn default_shell() -> Vec<OsString> {
    if let Some(shell) = std::env::var_os("RTERM_SHELL").filter(|s| !s.is_empty()) {
        return vec![shell];
    }
    if let Some(shell) =
        registry_string("SOFTWARE\\OpenSSH", "DefaultShell").filter(|s| Path::new(s).is_file())
    {
        return vec![shell];
    }
    for program in ["pwsh.exe", "powershell.exe"] {
        if let Some(p) = find_in_path(program) {
            return vec![p];
        }
    }
    vec![std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into())]
}

/// Quote one argument the way the Microsoft C runtime parses command lines.
fn push_quoted(arg: &OsStr, out: &mut Vec<u16>) {
    let units: Vec<u16> = arg.encode_wide().collect();
    let special = |c: &u16| *c == b' ' as u16 || *c == b'\t' as u16 || *c == b'"' as u16;
    if !units.is_empty() && !units.iter().any(special) {
        out.extend(units);
        return;
    }
    out.push(b'"' as u16);
    let mut backslashes = 0;
    for c in units {
        if c == b'\\' as u16 {
            backslashes += 1;
            continue;
        }
        let n = if c == b'"' as u16 {
            backslashes * 2 + 1
        } else {
            backslashes
        };
        out.extend(std::iter::repeat_n(b'\\' as u16, n));
        out.push(c);
        backslashes = 0;
    }
    out.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2));
    out.push(b'"' as u16);
}

pub fn command_line(argv: &[OsString]) -> Vec<u16> {
    let mut out = Vec::new();
    for (i, arg) in argv.iter().enumerate() {
        if i > 0 {
            out.push(b' ' as u16);
        }
        push_quoted(arg, &mut out);
    }
    out.push(0);
    out
}

/// This process's environment with `changes` applied (None removes), as a
/// CreateProcessW environment block. Names compare case-insensitively.
fn environment_block(changes: &[(&str, Option<OsString>)]) -> Vec<u16> {
    let mut vars: Vec<(OsString, OsString)> = std::env::vars_os()
        .filter(|(k, _)| !changes.iter().any(|(c, _)| k.eq_ignore_ascii_case(c)))
        .collect();
    for (k, v) in changes {
        if let Some(v) = v {
            vars.push((k.into(), v.clone()));
        }
    }
    vars.sort_by_key(|(k, _)| k.to_ascii_uppercase());
    let mut block = Vec::new();
    for (k, v) in vars {
        block.extend(k.encode_wide());
        block.push(b'=' as u16);
        block.extend(v.encode_wide());
        block.push(0);
    }
    block.push(0);
    block
}

/// A pseudo console. Closing it hangs up the programs attached to it.
pub struct PseudoConsole(HPCON);

unsafe impl Send for PseudoConsole {}

impl PseudoConsole {
    pub fn resize(&self, size: WinSize) {
        let coord = COORD {
            X: size.cols as i16,
            Y: size.rows as i16,
        };
        unsafe { ResizePseudoConsole(self.0, coord) };
    }

    /// May block until the output pipe has been drained, so call it from
    /// a thread while the output is still being read.
    pub fn close(self) {
        unsafe { ClosePseudoConsole(self.0) };
    }
}

/// A program running on a pseudo console.
pub struct Session {
    pub console: PseudoConsole,
    /// Keystrokes for the program.
    pub input: File,
    /// The screen output, as VT sequences.
    pub output: File,
    pub process: OwnedHandle,
    /// Holds the program and everything it starts; closing it ends them.
    pub job: OwnedHandle,
    pub pid: u32,
}

fn check(ok: i32) -> io::Result<()> {
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub fn spawn_session(
    argv: &[OsString],
    size: WinSize,
    cwd: &Path,
    env: &[(&str, Option<OsString>)],
) -> io::Result<Session> {
    unsafe {
        let (mut in_read, mut in_write): (HANDLE, HANDLE) = (ptr::null_mut(), ptr::null_mut());
        let (mut out_read, mut out_write): (HANDLE, HANDLE) = (ptr::null_mut(), ptr::null_mut());
        check(CreatePipe(&mut in_read, &mut in_write, ptr::null(), 0))?;
        check(CreatePipe(&mut out_read, &mut out_write, ptr::null(), 0))?;
        let input = File::from_raw_handle(in_write as _);
        let output = File::from_raw_handle(out_read as _);

        let mut hpc: HPCON = 0;
        let coord = COORD {
            X: size.cols as i16,
            Y: size.rows as i16,
        };
        let hr = CreatePseudoConsole(coord, in_read, out_write, 0, &mut hpc);
        // The pseudo console holds its own copies of its ends of the pipes.
        CloseHandle(in_read);
        CloseHandle(out_write);
        if hr != S_OK {
            return Err(io::Error::other(format!(
                "CreatePseudoConsole failed (0x{hr:08x})"
            )));
        }
        let console = PseudoConsole(hpc);

        let mut attrs = Attributes::new()?;
        // For this attribute the value is the HPCON itself, not a pointer to it.
        attrs.set(
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
            hpc as *const c_void,
            mem::size_of::<HPCON>(),
        )?;

        let mut startup: STARTUPINFOEXW = mem::zeroed();
        startup.StartupInfo.cb = mem::size_of::<STARTUPINFOEXW>() as u32;
        // No standard handles: the program talks to the pseudo console only.
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.lpAttributeList = attrs.ptr();

        let mut cmdline = command_line(argv);
        let env_block = environment_block(env);
        let cwd = wide(cwd.as_os_str());
        let mut info: PROCESS_INFORMATION = mem::zeroed();
        let created = check(CreateProcessW(
            ptr::null(),
            cmdline.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            0,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED,
            env_block.as_ptr() as *const c_void,
            cwd.as_ptr(),
            &startup.StartupInfo,
            &mut info,
        ));
        drop(attrs);
        created?;

        // Put the program (and anything it starts) in a job that dies with
        // us, as a Unix session dies when its terminal goes away.
        let job = CreateJobObjectW(ptr::null(), ptr::null());
        if !job.is_null() {
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const c_void,
                mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            AssignProcessToJobObject(job, info.hProcess);
        }
        ResumeThread(info.hThread);
        CloseHandle(info.hThread);

        Ok(Session {
            console,
            input,
            output,
            process: OwnedHandle::from_raw_handle(info.hProcess as _),
            job: OwnedHandle::from_raw_handle(job as _),
            pid: info.dwProcessId,
        })
    }
}

/// A process-thread attribute list holding one attribute.
struct Attributes {
    buf: Vec<u8>,
}

impl Attributes {
    fn new() -> io::Result<Attributes> {
        let mut size = 0usize;
        unsafe { InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut size) };
        let mut buf = vec![0u8; size];
        let list = buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        check(unsafe { InitializeProcThreadAttributeList(list, 1, 0, &mut size) })?;
        Ok(Attributes { buf })
    }

    fn ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST
    }

    /// `value` must stay alive and unmoved until the process is created.
    fn set<T>(&mut self, attribute: usize, value: *const T, size: usize) -> io::Result<()> {
        let list = self.ptr();
        check(unsafe {
            UpdateProcThreadAttribute(
                list,
                0,
                attribute,
                value.cast(),
                size,
                ptr::null_mut(),
                ptr::null(),
            )
        })
    }
}

impl Drop for Attributes {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.ptr()) };
    }
}

/// A session daemon starting in the background.
pub struct Daemon {
    /// Where it reports "ok" (or an error) once it is ready.
    pub status: File,
    pub process: OwnedHandle,
}

/// Start `exe args...` with no console, in its own process group, outside
/// the caller's job object if that's allowed (Windows OpenSSH puts each
/// connection in a job that is killed when the connection closes, but lets
/// processes break away), and inheriting exactly three handles: NUL for
/// input, a pipe for its status line, and `log` for errors. Plain
/// CreateProcess would hand it every inheritable handle we have, such as the
/// pipes of whoever is waiting for *our* output, keeping them open for the
/// lifetime of the session.
pub fn spawn_daemon(exe: &Path, args: &[OsString], log: &File) -> io::Result<Daemon> {
    let nul = OpenOptions::new().read(true).open("NUL")?;
    let log = log.try_clone()?;
    let (mut status_read, mut status_write): (HANDLE, HANDLE) = (ptr::null_mut(), ptr::null_mut());
    check(unsafe { CreatePipe(&mut status_read, &mut status_write, ptr::null(), 0) })?;
    let status = unsafe { File::from_raw_handle(status_read as _) };
    let status_write = unsafe { OwnedHandle::from_raw_handle(status_write as _) };
    let handles: [HANDLE; 3] = [
        nul.as_raw_handle() as HANDLE,
        status_write.as_raw_handle() as HANDLE,
        log.as_raw_handle() as HANDLE,
    ];
    for h in handles {
        check(unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) })?;
    }

    let mut argv = vec![exe.as_os_str().to_owned()];
    argv.extend(args.iter().cloned());
    let base = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | EXTENDED_STARTUPINFO_PRESENT;
    let mut result = Err(io::Error::other("no attempt made"));
    for flags in [base | CREATE_BREAKAWAY_FROM_JOB, base] {
        let mut attrs = Attributes::new()?;
        attrs.set(
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            handles.as_ptr(),
            mem::size_of_val(&handles),
        )?;
        let mut startup: STARTUPINFOEXW = unsafe { mem::zeroed() };
        startup.StartupInfo.cb = mem::size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = handles[0];
        startup.StartupInfo.hStdOutput = handles[1];
        startup.StartupInfo.hStdError = handles[2];
        startup.lpAttributeList = attrs.ptr();
        let mut cmdline = command_line(&argv);
        let mut info: PROCESS_INFORMATION = unsafe { mem::zeroed() };
        let created = check(unsafe {
            CreateProcessW(
                ptr::null(),
                cmdline.as_mut_ptr(),
                ptr::null(),
                ptr::null(),
                1,
                flags,
                ptr::null(),
                ptr::null(),
                &startup.StartupInfo,
                &mut info,
            )
        });
        match created {
            Ok(()) => {
                unsafe { CloseHandle(info.hThread) };
                result = Ok(unsafe { OwnedHandle::from_raw_handle(info.hProcess as _) });
                break;
            }
            // Our job forbids breaking away; the session then lives only as
            // long as the job does.
            Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => result = Err(e),
            Err(e) => return Err(e),
        }
    }
    let process = result?;
    // Only the daemon may hold the write end, so we see EOF once it reports.
    drop(status_write);
    Ok(Daemon { status, process })
}

/// Wait for a process to exit and return its exit code.
pub fn wait_process(process: &OwnedHandle) -> i32 {
    unsafe {
        WaitForSingleObject(process.as_raw_handle() as HANDLE, INFINITE);
        let mut code = 0u32;
        GetExitCodeProcess(process.as_raw_handle() as HANDLE, &mut code);
        code as i32
    }
}

/// End every process in a job.
pub fn terminate_job(job: &OwnedHandle) {
    unsafe { TerminateJobObject(job.as_raw_handle() as HANDLE, 1) };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(args: &[&str]) -> String {
        let argv: Vec<OsString> = args.iter().map(OsString::from).collect();
        let mut w = command_line(&argv);
        w.pop();
        String::from_utf16(&w).unwrap()
    }

    #[test]
    fn quoting() {
        assert_eq!(line(&["cmd.exe"]), "cmd.exe");
        assert_eq!(
            line(&["C:\\Program Files\\x.exe", "a b", ""]),
            "\"C:\\Program Files\\x.exe\" \"a b\" \"\""
        );
        assert_eq!(line(&["say", "\"hi\""]), "say \"\\\"hi\\\"\"");
        assert_eq!(line(&["dir\\", "trailing \\"]), "dir\\ \"trailing \\\\\"");
    }
}
