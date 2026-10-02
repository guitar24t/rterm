#!/usr/bin/env python3
"""Pick an rterm session on a remote host and attach to it.

    rterm-connect.py myserver
    rterm-connect.py user@host -p 2222 -i ~/.ssh/work_key

Lists the sessions running on HOST, lets you pick one or start a new one,
then attaches over ssh. Arguments after HOST are passed to ssh. Needs only
Python 3.8+ and the OpenSSH client; rterm must be installed on the host.
"""

import argparse
import json
import os
import re
import shlex
import subprocess
import sys
from dataclasses import dataclass
from typing import List, NoReturn, Optional

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
    parser.add_argument("host", help="ssh destination, e.g. myserver or user@host")
    parser.add_argument("ssh_args", nargs=argparse.REMAINDER,
                        help="extra ssh options, e.g. -p 2222")
    args = parser.parse_args(argv)
    if args.session is not None and not NAME_RE.match(args.session):
        parser.error(f"invalid session name {args.session!r}")
    return args


def main(argv: Optional[List[str]] = None) -> None:
    args = parse_args(sys.argv[1:] if argv is None else argv)
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
    os.execvp(cmd[0], cmd)  # ssh takes over the terminal from here


if __name__ == "__main__":
    main()
