#!/usr/bin/env python3
"""Pick an rterm session on a remote host and attach to it.

    rterm-connect.py myserver
    rterm-connect.py user@host -p 2222 -i ~/.ssh/work_key

Lists the sessions running on HOST, lets you pick one or start a new one,
then attaches over ssh. Arguments after HOST are passed to ssh. Needs only
Python 3.8+ and the OpenSSH client; rterm must be installed on the host.

Clipboard: programs on the host (Codex's copy-on-select, vim, tmux-style
tools) copy by sending an OSC 52 escape sequence to the terminal, which many
terminals ignore (GNOME Terminal and other VTE terminals, macOS Terminal).
While attached, this script watches for those sequences and puts the text on
this machine's clipboard itself. Everything else passes through untouched.
"""

import argparse
import base64
import json
import os
import re
import select
import shlex
import shutil
import signal
import subprocess
import sys
import threading
from dataclasses import dataclass
from typing import Callable, List, NoReturn, Optional, Tuple

INSTALL_HINT = "curl -fsSL https://guitar24t.github.io/rterm/install.sh | sudo sh"
NAME_RE = re.compile(r"^(?![.-])[A-Za-z0-9._@+-]{1,64}\Z")
DEFAULT_NAME = "main"


@dataclass
class Session:
    name: str
    status: str  # "attached", "detached" or "unknown"
    age: str = ""
    size: str = ""
    title: str = ""


class Style:
    """ANSI styling, only when writing to a terminal (and NO_COLOR is unset)."""

    def __init__(self, enabled: bool):
        self.enabled = enabled

    def _wrap(self, code: str, text: str) -> str:
        return f"\033[{code}m{text}\033[0m" if self.enabled else text

    def bold(self, text: str) -> str:
        return self._wrap("1", text)

    def dim(self, text: str) -> str:
        return self._wrap("2", text)

    def green(self, text: str) -> str:
        return self._wrap("32", text)

    def yellow(self, text: str) -> str:
        return self._wrap("33", text)


def fail(message: str) -> NoReturn:
    print(f"rterm-connect: {message}", file=sys.stderr)
    sys.exit(1)


# --- talking to the host ---------------------------------------------------


def mux_options(args: argparse.Namespace) -> List[str]:
    """Share one ssh connection between listing and attaching, so password or
    2FA logins are only asked for once. Skipped if the user's ssh config
    already multiplexes this host, or where it can't work."""
    if args.no_mux or os.name == "nt":
        return []
    try:
        effective = subprocess.run(
            ["ssh", "-G", *args.ssh_args, args.host],
            capture_output=True, text=True, timeout=10,
        ).stdout
    except (OSError, subprocess.SubprocessError):
        return []
    for line in effective.splitlines():
        key, _, value = line.partition(" ")
        if key == "controlmaster" and value not in ("false", "no"):
            return []
    # The socket lives in ~/.ssh, which only the user can access (a socket
    # in a shared directory could be impersonated). ssh briefly binds the
    # path plus a 17-character suffix, so keep it under the socket limit.
    ssh_dir = os.path.expanduser("~/.ssh")
    path = os.path.join(ssh_dir, "rterm-%C")
    if not os.path.isdir(ssh_dir) or len(path) - 2 + 40 + 17 > 100:
        return []
    return [
        "-o", "ControlMaster=auto",
        "-o", f"ControlPath={path}",
        "-o", "ControlPersist=120",
    ]


def ssh_command(args: argparse.Namespace, remote: str, tty: bool) -> List[str]:
    cmd = ["ssh", *args.mux, *args.ssh_args]
    if tty:
        cmd.append("-t")
    cmd += [args.host, remote]
    return cmd


def rterm(args: argparse.Namespace, *argv: str) -> str:
    """A shell-quoted rterm command line to run on the host."""
    return " ".join(shlex.quote(a) for a in (args.rterm, *argv))


def list_sessions(args: argparse.Namespace) -> List[Session]:
    # Hosts running rterm 0.1.0 don't know --json; fall back to the table.
    remote = f"{rterm(args, 'ls', '--json')} 2>/dev/null || {rterm(args, 'ls')}"
    try:
        result = subprocess.run(
            ssh_command(args, remote, tty=False),
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, text=True,
        )
    except OSError as e:
        fail(f"can't run ssh: {e}")
    if result.returncode == 127:
        fail(f"rterm is not installed on {args.host} (or not on its PATH; see --rterm).\n"
             f"Install it there with:\n  {INSTALL_HINT}")
    if result.returncode == 255:
        fail(f"ssh to {args.host} failed")
    if result.returncode != 0:
        fail(f"listing sessions on {args.host} failed (exit {result.returncode})")
    return parse_listing(result.stdout)


def parse_listing(text: str) -> List[Session]:
    text = text.strip()
    if text.startswith("["):
        sessions = []
        for item in json.loads(text):
            title = item.get("title") or item.get("command") or item.get("error") or ""
            size = f"{item['cols']}x{item['rows']}" if "cols" in item else ""
            sessions.append(Session(item["name"], item.get("status", "unknown"),
                                    item.get("age_text", ""), size, title))
        return sessions
    # Human-readable table: NAME STATUS AGE SIZE TITLE (title may hold spaces).
    sessions = []
    for line in text.splitlines():
        if not line.strip() or line.startswith("NAME ") or line == "no sessions":
            continue
        parts = line.split(None, 4)
        if len(parts) >= 2 and parts[1] == "?":
            sessions.append(Session(parts[0], "unknown",
                                    title=line.split(None, 2)[2] if len(parts) > 2 else ""))
        elif len(parts) >= 4:
            sessions.append(Session(parts[0], parts[1], parts[2], parts[3],
                                    parts[4] if len(parts) > 4 else ""))
    return sessions


# --- clipboard bridge --------------------------------------------------------


class Osc52Scanner:
    """Finds OSC 52 clipboard writes (ESC ] 52 ; Pc ; base64 BEL|ST) in a byte
    stream. Sequences may be split across reads; the stream itself is never
    modified."""

    START = b"\x1b]52;"
    MAX_LEN = 16 << 20

    def __init__(self) -> None:
        self.carry = b""
        # Within a carried, unterminated sequence: where to resume looking
        # for its terminator, so long copies aren't rescanned on every read.
        self.resume = 0

    def feed(self, data: bytes) -> List[Tuple[str, str]]:
        """Returns (text, selection) for every complete copy, where selection
        is "clipboard" or "primary"."""
        buf = self.carry + data
        resume = self.resume
        self.carry, self.resume = b"", 0
        found = []
        pos = 0
        while True:
            start = buf.find(self.START, pos)
            if start < 0:
                # Keep a tail that might be the beginning of the next sequence.
                for keep in range(min(len(self.START) - 1, len(buf)), 0, -1):
                    if self.START.startswith(buf[-keep:]):
                        self.carry = buf[-keep:]
                        break
                return found
            body = start + len(self.START)
            end = max(body, resume if start == 0 else 0)
            resume = 0
            bel, esc = buf.find(b"\x07", end), buf.find(b"\x1b", end)
            end = min(i for i in (bel, esc, len(buf)) if i >= 0)
            if end == len(buf) or (buf[end] == 0x1B and end + 1 == len(buf)):
                # Unterminated so far: wait for more (unless it's absurdly long).
                if len(buf) - start <= self.MAX_LEN:
                    self.carry = buf[start:]
                    self.resume = end - start
                return found
            if buf[end] == 0x1B and buf[end + 1] != ord("\\"):
                pos = end  # Another escape sequence cancelled this one.
                continue
            copy = self._parse(buf[body:end])
            if copy:
                found.append(copy)
            pos = end + (1 if buf[end] == 0x07 else 2)

    @staticmethod
    def _parse(payload: bytes) -> Optional[Tuple[str, str]]:
        targets, sep, data = payload.partition(b";")
        if not sep or not data or data == b"?":  # "?" asks to read the clipboard
            return None
        try:
            text = base64.b64decode(data, validate=False).decode("utf-8", "replace")
        except ValueError:
            return None
        if not text:
            return None
        primary = targets and b"c" not in targets and (b"p" in targets or b"s" in targets)
        return text, "primary" if primary else "clipboard"


def is_wsl() -> bool:
    try:
        with open("/proc/sys/kernel/osrelease") as f:
            return "microsoft" in f.read().lower()
    except OSError:
        return False


def x11_available() -> bool:
    import ctypes.util

    return bool(os.environ.get("DISPLAY")) and ctypes.util.find_library("X11") is not None


def clipboard_command(selection: str) -> Optional[List[str]]:
    """A command that puts its stdin on this machine's clipboard."""
    custom = os.environ.get("RTERM_CONNECT_CLIPBOARD_CMD")
    if custom:
        return ["sh", "-c", custom]
    primary = selection == "primary"
    if sys.platform == "darwin":
        return None if primary else (["pbcopy"] if shutil.which("pbcopy") else None)
    if os.name == "nt" or is_wsl():
        return None if primary else (["clip.exe"] if shutil.which("clip.exe") else None)
    if os.environ.get("WAYLAND_DISPLAY") and shutil.which("wl-copy"):
        return ["wl-copy"] + (["--primary"] if primary else [])
    if os.environ.get("DISPLAY"):
        if shutil.which("xclip"):
            return ["xclip", "-selection", selection, "-in"]
        if shutil.which("xsel"):
            return ["xsel", "--primary" if primary else "--clipboard", "--input"]
    if x11_available():
        # Nothing installed: serve the selection ourselves (X11, or XWayland
        # on GNOME/KDE Wayland sessions, which bridge it to Wayland apps).
        return [sys.executable, os.path.abspath(__file__), "--x11-selection-owner", selection]
    return None


def debug(message: str) -> None:
    """Append to the file named by RTERM_CONNECT_DEBUG, if set."""
    path = os.environ.get("RTERM_CONNECT_DEBUG")
    if path:
        with open(path, "a") as f:
            f.write(message + "\n")


def set_clipboard(text: str, selection: str) -> None:
    cmd = clipboard_command(selection)
    debug(f"copy: {len(text)} chars to {selection} via {cmd}")
    if not cmd:
        return
    data = text.encode("utf-16-le") if cmd[0] == "clip.exe" else text.encode()
    if cmd[0] == "clip.exe":
        data = b"\xff\xfe" + data
    try:
        # These tools fork a background process that serves the clipboard;
        # the foreground one returns quickly.
        # Errors go to the debug log (a pipe would be held open by the
        # background clipboard server, and we'd wait for it forever).
        log = os.environ.get("RTERM_CONNECT_DEBUG")
        with open(log or os.devnull, "ab") as errors:
            result = subprocess.run(
                cmd, input=data, stdout=subprocess.DEVNULL, stderr=errors,
                start_new_session=True, timeout=10,
                env={**os.environ, "RTERM_SELECTION": selection},
            )
        debug(f"  exit {result.returncode}")
    except (OSError, subprocess.SubprocessError) as e:
        debug(f"  failed: {e}")


def relay(cmd: List[str], on_copy: Callable[[str, str], None]) -> int:
    """Run cmd (ssh) on a pseudo-terminal between it and our terminal, relaying
    bytes unchanged in both directions and reporting OSC 52 copies."""
    import pty
    import termios
    import tty
    import fcntl

    stdin_fd, stdout_fd = sys.stdin.fileno(), sys.stdout.fileno()
    pid, master = pty.fork()
    if pid == 0:
        try:
            os.execvp(cmd[0], cmd)
        except OSError as e:
            os.write(2, f"rterm-connect: can't run {cmd[0]}: {e}\n".encode())
        os._exit(127)

    def sync_size(*_: object) -> None:
        try:
            size = fcntl.ioctl(stdout_fd, termios.TIOCGWINSZ, b"\0" * 8)
            fcntl.ioctl(master, termios.TIOCSWINSZ, size)
        except OSError:
            pass

    def forward(signum: int, _frame: object) -> None:
        try:
            os.kill(pid, signum)
        except OSError:
            pass

    sync_size()
    previous = {sig: signal.signal(sig, handler) for sig, handler in (
        (signal.SIGWINCH, sync_size), (signal.SIGTERM, forward), (signal.SIGHUP, forward))}
    saved = termios.tcgetattr(stdin_fd)
    tty.setraw(stdin_fd)
    scanner = Osc52Scanner()
    sources = [master, stdin_fd]
    try:
        while True:
            ready, _, _ = select.select(sources, [], [])
            if master in ready:
                try:
                    data = os.read(master, 65536)
                except OSError:  # EIO: ssh exited and closed the terminal
                    data = b""
                if not data:
                    break
                for text, selection in scanner.feed(data):
                    on_copy(text, selection)
                write_all(stdout_fd, data)
            if stdin_fd in ready:
                try:
                    data = os.read(stdin_fd, 65536)
                except OSError:
                    data = b""
                if data:
                    write_all(master, data)
                else:
                    sources.remove(stdin_fd)
    finally:
        termios.tcsetattr(stdin_fd, termios.TCSADRAIN, saved)
        for sig, handler in previous.items():
            signal.signal(sig, handler)
        os.close(master)
    _, status = os.waitpid(pid, 0)
    if os.WIFSIGNALED(status):
        return 128 + os.WTERMSIG(status)
    return os.WEXITSTATUS(status)


def write_all(fd: int, data: bytes) -> None:
    view = memoryview(data)
    while view:
        try:
            view = view[os.write(fd, view):]
        except InterruptedError:
            continue
        except BlockingIOError:
            select.select([], [fd], [])


def x11_selection_owner(selection: str) -> None:
    """`--x11-selection-owner`: take the X11 CLIPBOARD/PRIMARY selection with
    the text on stdin and serve it from a background process until another
    program takes the selection over. Uses only libX11, via ctypes."""
    import ctypes
    import ctypes.util
    from ctypes import POINTER, Structure, Union, byref, c_char_p, c_int, c_long, c_ulong, c_void_p

    data = sys.stdin.buffer.read()
    xlib = ctypes.CDLL(ctypes.util.find_library("X11") or "libX11.so.6")

    class SelectionRequest(Structure):
        _fields_ = [("type", c_int), ("serial", c_ulong), ("send_event", c_int),
                    ("display", c_void_p), ("owner", c_ulong), ("requestor", c_ulong),
                    ("selection", c_ulong), ("target", c_ulong), ("property", c_ulong),
                    ("time", c_ulong)]

    class SelectionNotify(Structure):
        _fields_ = [("type", c_int), ("serial", c_ulong), ("send_event", c_int),
                    ("display", c_void_p), ("requestor", c_ulong), ("selection", c_ulong),
                    ("target", c_ulong), ("property", c_ulong), ("time", c_ulong)]

    class XEvent(Union):
        _fields_ = [("type", c_int), ("request", SelectionRequest), ("pad", c_long * 24)]

    xlib.XOpenDisplay.restype = c_void_p
    xlib.XOpenDisplay.argtypes = [c_char_p]
    xlib.XDefaultRootWindow.restype = c_ulong
    xlib.XDefaultRootWindow.argtypes = [c_void_p]
    xlib.XCreateSimpleWindow.restype = c_ulong
    xlib.XCreateSimpleWindow.argtypes = [c_void_p, c_ulong] + [c_int] * 4 + [c_ulong] * 3
    xlib.XInternAtom.restype = c_ulong
    xlib.XInternAtom.argtypes = [c_void_p, c_char_p, c_int]
    xlib.XSetSelectionOwner.argtypes = [c_void_p, c_ulong, c_ulong, c_ulong]
    xlib.XGetSelectionOwner.restype = c_ulong
    xlib.XGetSelectionOwner.argtypes = [c_void_p, c_ulong]
    xlib.XNextEvent.argtypes = [c_void_p, POINTER(XEvent)]
    xlib.XChangeProperty.argtypes = [c_void_p, c_ulong, c_ulong, c_ulong, c_int, c_int, c_void_p, c_int]
    xlib.XSendEvent.argtypes = [c_void_p, c_ulong, c_int, c_long, c_void_p]
    xlib.XSync.argtypes = [c_void_p, c_int]
    xlib.XFlush.argtypes = [c_void_p]
    xlib.XExtendedMaxRequestSize.restype = c_long
    xlib.XExtendedMaxRequestSize.argtypes = [c_void_p]
    xlib.XMaxRequestSize.restype = c_long
    xlib.XMaxRequestSize.argtypes = [c_void_p]

    dpy = xlib.XOpenDisplay(None)
    if not dpy:
        sys.exit(1)
    max_bytes = (xlib.XExtendedMaxRequestSize(dpy) or xlib.XMaxRequestSize(dpy)) * 4 - 1024
    if len(data) > max_bytes:
        sys.exit(1)  # Would need the INCR protocol; far beyond any OSC 52 copy.
    atom = lambda name: xlib.XInternAtom(dpy, name, 0)  # noqa: E731
    sel = atom(b"PRIMARY" if selection == "primary" else b"CLIPBOARD")
    targets, utf8, text_atom = atom(b"TARGETS"), atom(b"UTF8_STRING"), atom(b"TEXT")
    plain = atom(b"text/plain;charset=utf-8")
    xa_string, xa_atom = 31, 4
    win = xlib.XCreateSimpleWindow(dpy, xlib.XDefaultRootWindow(dpy), 0, 0, 1, 1, 0, 0, 0)
    xlib.XSetSelectionOwner(dpy, sel, win, 0)
    xlib.XSync(dpy, 0)
    if xlib.XGetSelectionOwner(dpy, sel) != win:
        sys.exit(1)
    # Ownership is in place; let the caller go and keep serving in the background.
    if os.fork():
        os._exit(0)
    os.setsid()

    offered = (c_ulong * 5)(targets, utf8, xa_string, text_atom, plain)
    event = XEvent()
    while True:
        xlib.XNextEvent(dpy, byref(event))
        if event.type == 29:  # SelectionClear: someone else copied
            os._exit(0)
        if event.type != 30:  # SelectionRequest
            continue
        req = event.request
        prop = req.property or req.target
        if req.target == targets:
            xlib.XChangeProperty(dpy, req.requestor, prop, xa_atom, 32, 0, offered, len(offered))
        elif req.target in (utf8, xa_string, text_atom, plain):
            kind = utf8 if req.target == text_atom else req.target
            xlib.XChangeProperty(dpy, req.requestor, prop, kind, 8, 0, data, len(data))
        else:
            prop = 0  # Unsupported format.
        reply = SelectionNotify(31, 0, 1, req.display, req.requestor, req.selection,
                                req.target, prop, req.time)
        xlib.XSendEvent(dpy, req.requestor, 0, 0, byref(reply))
        xlib.XFlush(dpy)


# --- choosing ----------------------------------------------------------------


def suggest_name(sessions: List[Session]) -> str:
    taken = {s.name for s in sessions}
    if DEFAULT_NAME not in taken:
        return DEFAULT_NAME
    n = 1
    while str(n) in taken:
        n += 1
    return str(n)


def print_sessions(sessions: List[Session], host: str, style: Style, numbered: bool) -> None:
    if not sessions:
        print(f"No rterm sessions on {style.bold(host)}.")
        return
    print(f"rterm sessions on {style.bold(host)}:\n")
    width = max(len(s.name) for s in sessions)
    size_w = max(len(s.size) for s in sessions)
    for i, s in enumerate(sessions, 1):
        status = {
            "attached": style.yellow("attached"),
            "detached": style.green("detached"),
        }.get(s.status, style.dim(s.status))
        index = f"{i:>3}  " if numbered else "  "
        print(f"{index}{style.bold(s.name.ljust(width))}  {status}  "
              f"{style.dim(s.age.rjust(6))}  {s.size.ljust(size_w)}  {s.title}".rstrip())


def ask(prompt: str) -> str:
    try:
        return input(prompt).strip()
    except (EOFError, KeyboardInterrupt):
        print()
        sys.exit(130)


def ask_new_name(sessions: List[Session]) -> str:
    default = suggest_name(sessions)
    while True:
        name = ask(f"Name for the new session [{default}]: ") or default
        if NAME_RE.match(name):
            return name
        print("  Use letters, digits and -_.@+ (up to 64 characters).")


def choose(sessions: List[Session], host: str, style: Style) -> str:
    """Returns the name of the session to attach (existing or new)."""
    if not sessions:
        print_sessions(sessions, host, style, numbered=True)
        return ask_new_name(sessions)

    print_sessions(sessions, host, style, numbered=True)
    print(f"\n{'n':>3}  new session")
    print(f"{'q':>3}  quit\n")
    detached = [i for i, s in enumerate(sessions, 1) if s.status == "detached"]
    default = str(detached[0]) if detached else "1"
    by_name = {s.name: s for s in sessions}

    while True:
        answer = ask(f"Attach to [{default}]: ") or default
        if answer.lower() in ("q", "quit", "exit"):
            sys.exit(0)
        if answer.lower() in ("n", "new"):
            return ask_new_name(sessions)
        if answer.isdigit() and 1 <= int(answer) <= len(sessions):
            chosen = sessions[int(answer) - 1]
        elif answer in by_name:
            chosen = by_name[answer]
        elif NAME_RE.match(answer):
            if ask(f"No session named '{answer}'. Start it? [Y/n] ").lower() in ("", "y", "yes"):
                return answer
            continue
        else:
            print(f"  Enter 1-{len(sessions)}, a session name, n or q.")
            continue
        if chosen.status == "attached":
            print(style.dim("  (attached elsewhere; attaching here takes it over)"))
        return chosen.name


# --- main --------------------------------------------------------------------


def parse_args(argv: List[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="rterm-connect.py",
        description="Choose an rterm session on a remote host and attach to it.",
        epilog="Options for this script go before HOST; anything after HOST is "
               "passed to ssh, e.g. 'rterm-connect.py me@host -p 2222'. "
               "Detach with Ctrl-\\ as usual.",
    )
    parser.add_argument("--list", action="store_true",
                        help="only list the sessions, don't attach")
    parser.add_argument("--session", metavar="NAME",
                        help="attach to (or start) NAME without asking")
    parser.add_argument("-e", "--detach-key", metavar="KEY",
                        help="detach key to use, e.g. '^a' (default: the host's, Ctrl-\\)")
    parser.add_argument("--rterm", default="rterm", metavar="PATH",
                        help="rterm command on the host (default: rterm)")
    parser.add_argument("--no-mux", action="store_true",
                        help="don't share one ssh connection between listing and attaching")
    parser.add_argument("--no-clipboard", action="store_true",
                        help="don't copy OSC 52 clipboard writes to this machine's clipboard")
    parser.add_argument("host", help="ssh destination, e.g. myserver or user@host")
    parser.add_argument("ssh_args", nargs=argparse.REMAINDER,
                        help="extra ssh options, e.g. -p 2222")
    args = parser.parse_args(argv)
    if args.session is not None and not NAME_RE.match(args.session):
        parser.error(f"invalid session name {args.session!r}")
    return args


def main(argv: Optional[List[str]] = None) -> None:
    argv = sys.argv[1:] if argv is None else argv
    if argv[:1] == ["--x11-selection-owner"]:
        x11_selection_owner(argv[1] if len(argv) > 1 else "clipboard")
        return
    args = parse_args(argv)
    style = Style(sys.stdout.isatty() and "NO_COLOR" not in os.environ)
    args.mux = mux_options(args)

    if args.session:
        name = args.session
    else:
        sessions = list_sessions(args)
        if args.list:
            print_sessions(sessions, args.host, style, numbered=False)
            return
        name = choose(sessions, args.host, style)

    remote = ["attach", name]
    if args.detach_key:
        remote += ["-e", args.detach_key]
    cmd = ssh_command(args, rterm(args, *remote), tty=True)
    sys.stdout.flush()
    if os.name == "nt":
        sys.exit(subprocess.call(cmd))
    bridge = (
        not args.no_clipboard
        and sys.stdin.isatty() and sys.stdout.isatty()
        and clipboard_command("clipboard") is not None
    )
    if not bridge:
        os.execvp(cmd[0], cmd)  # ssh takes over the terminal from here

    def on_copy(text: str, selection: str) -> None:
        # Off the relay loop, so a slow clipboard tool never stalls output.
        threading.Thread(target=set_clipboard, args=(text, selection), daemon=True).start()

    sys.exit(relay(cmd, on_copy))


if __name__ == "__main__":
    main()
