# Rob Terminal

**Persistent terminal sessions that behave like a plain terminal.**

Rob Terminal (`rterm`) keeps your shell and everything running in it alive when
you disconnect, and lets you pick it up again later from any terminal. While
you're attached it gets out of the way completely: scrolling, selecting,
copying, the mouse and the keyboard all work exactly as they do in a plain
ssh session, because your terminal is talking straight to your programs.

```sh
ssh myserver
rterm          # a persistent shell: start a build, run Codex, ...
               # close the laptop, lose the wifi, or press Ctrl-\ to leave
ssh myserver
rterm          # everything is exactly where you left it
```

[Why](#why-rob-terminal-exists) ·
[Install](#install) ·
[Quick start](#quick-start) ·
[Using it](#using-rob-terminal) ·
[rterm-connect](#rterm-connect-pick-a-session-from-your-own-computer) ·
[Reference](#reference) ·
[How it works](#how-it-works) ·
[FAQ](#troubleshooting-and-faq)

---

## Why Rob Terminal exists

Remote machines are where long-running work happens: builds, test suites,
data jobs, and increasingly AI coding agents like Codex and Claude Code that
work for an hour while you do something else. Over a plain ssh connection all
of that dies the moment the connection does.

The usual fix is a terminal multiplexer such as tmux or screen. They solve the
disconnect problem, but they do it by becoming a second terminal sitting
between you and your programs. That terminal has its own ideas about
everything: its own scrollback that you scroll with a copy mode instead of
your mouse wheel, a prefix key that swallows a shortcut, a mouse mode that
breaks your terminal's normal selection, its own clipboard, a different
`TERM`, and partial support for the newer terminal features modern programs
rely on (the kitty keyboard protocol, synchronized output, OSC 52 clipboard,
hyperlinks). Programs that draw rich inline interfaces, like today's AI CLIs,
are particularly sensitive to that layer.

Rob Terminal starts from a different observation: **on a remote machine,
persistence is the only multiplexer feature most people actually need.** So it
does exactly that and nothing else, and leaves your terminal in charge of
everything a terminal is good at: scrollback, selection, copy and paste,
fonts, tabs and splits.

### Design principles

1. **Invisible while attached.** Bytes pass between your terminal and your
   programs unchanged. There is no terminal emulator in the live path, so
   there is nothing to configure and nothing to get wrong.
2. **Seamless when you come back.** Reattaching restores the screen, the
   scrollback (into your terminal's own scrollback) and every mode the
   programs had switched on, so it looks as if you never left.
3. **Never in the way.** No prefix key, no status bar, no configuration file.
   One detach key (Ctrl-\\), which you can change or switch off.
4. **Boring to operate.** One static binary, signed packages for Linux and a
   Homebrew formula for macOS, and automatic updates that never interrupt a
   running session.

### What it deliberately isn't

Rob Terminal has no windows, panes, splits or status bar; use your terminal's
own tabs and splits, each running its own session if you like. It isn't a
replacement for mosh's roaming and predictive echo. And it doesn't share one
session between several people at once: one terminal is attached at a time.

### How it compares

| | plain ssh | tmux / screen | Rob Terminal |
|---|---|---|---|
| Programs keep running after a disconnect | no | yes | yes |
| Screen is restored when you come back | n/a | yes | yes |
| Scroll with your terminal's own scrollback | yes | no (copy mode) | yes |
| Select and copy with your terminal | yes | partly (mouse mode, panes) | yes |
| No prefix key or status bar | yes | no | yes |
| Programs see your real terminal and `TERM` | yes | no | yes |
| Windows, panes and splits | no | yes | no (use your terminal's) |

Tools such as dtach and abduco also pass bytes straight through, but rely on
the program to redraw its screen when you reattach and don't bring back your
scrollback or the terminal modes the program had set.
[shpool](https://github.com/shell-pool/shpool) takes a similar approach to
Rob Terminal's and is worth a look too.

---

## Install

Install Rob Terminal on the machine where your sessions should live, usually
a server. Every package contains two commands:

- `rterm`, the session tool itself;
- `rterm-connect`, a session picker you run on your own computer
  ([details below](#rterm-connect-pick-a-session-from-your-own-computer)).
  It needs Python 3.8+, which the Linux packages recommend and macOS provides.

### Ubuntu, Debian, RHEL 9, Rocky, Alma (x86_64 and arm64)

```sh
curl -fsSL https://guitar24t.github.io/rterm/install.sh | sudo sh
```

The installer adds Rob Terminal's signed apt or dnf repository and installs
the package. From then on `apt upgrade` / `dnf upgrade` keep it current. On
Ubuntu it also lets unattended-upgrades install new versions automatically;
set `RTERM_NO_AUTO_UPDATE=1` before running it to skip that. Manual
repository setup and the signing key are on the
[package site](https://guitar24t.github.io/rterm/); the key's fingerprint is
`6BA1 9243 0FF1 E559 0D33  F634 B392 5793 0093 C660`.

### macOS (Apple Silicon and Intel, macOS 11+)

```sh
brew install guitar24t/tap/rterm
```

A universal binary from the [Homebrew tap](https://github.com/guitar24t/homebrew-tap).
`brew upgrade` picks up new releases.

### Other options

Every [GitHub release](https://github.com/guitar24t/rterm/releases) carries the
.deb and .rpm packages, static Linux tarballs, the macOS tarball, and signed
checksums. To build from source with a Rust toolchain:

```sh
cargo install --path .
```

---

## Quick start

1. ssh to the machine and run `rterm`. You are in your normal login shell,
   in a session called `main`.
2. Work as usual. Start something long: a build, a test run, Codex.
3. Leave whenever you like: press **Ctrl-\\**, close the terminal window, or
   simply lose the connection. The session keeps running.
4. Later, from the same computer or a different one, ssh in and run `rterm`
   again. The screen, the scrollback and the running programs are just as you
   left them.
5. When you're done for good, type `exit` in the shell. The session ends and
   `rterm` hands you back the shell's exit code, just like ssh does.

---

## Using Rob Terminal

### Sessions

A session is a shell (or another program) that keeps running whether or not
anyone is looking at it. Sessions have names; the default one is `main`.

```sh
rterm              # attach to "main", creating it if needed
rterm work         # attach to "work", creating it if needed
rterm attach -x db # attach to "db" only if it already exists
rterm ls           # list sessions
```

```console
$ rterm ls
NAME   STATUS     AGE   SIZE    TITLE
build  detached  2h05m  120x40  make -j8
main   attached    12m  200x50  user@server: ~/project
```

`STATUS` says whether a terminal is attached right now, `SIZE` is the
session's current width and height, and `TITLE` is the window title the
program set (or the command, if it set none).

A new session starts your login shell, in the directory you ran `rterm` from,
with the environment of the terminal that created it.

### Leaving and coming back

There are three ways to leave a session, and all of them keep it running:

- press the detach key, **Ctrl-\\** by default;
- close the terminal window, or let the ssh connection drop;
- run `rterm detach` (inside the session, or `rterm detach NAME` from
  anywhere).

To come back, run `rterm` or `rterm NAME` from any terminal. Rob Terminal
redraws everything: the scrollback is replayed into your terminal's own
scrollback, the screen and cursor are restored, and so is every mode the
program had switched on, such as a full-screen app's alternate screen, mouse
reporting, bracketed paste or enhanced keyboard reporting. If your terminal is
a different size than before, the session is resized to fit.

When you leave, Rob Terminal switches those modes off again, so the shell you
return to behaves normally: no stray mouse codes, no stuck alternate screen,
no oddly encoded keys.

### One terminal at a time

Attaching to a session that is attached somewhere else takes it over: the
other terminal is detached and told why. That's deliberate. A connection that
died without anyone noticing can never lock you out of your own session.

### Starting programs directly

A session can run a program instead of a shell; the session ends when the
program does.

```sh
rterm new build -- make -j8                     # create and attach
rterm new -d logs -- tail -f /var/log/syslog    # create in the background
rterm new                                       # "main", or the next free number
```

`rterm new` refuses names that already exist; use `rterm NAME` or
`rterm attach NAME` to get back to them.

### Ending a session

Exit the shell or program as usual, or hang the session up from anywhere:

```sh
rterm kill build
```

`kill` behaves like closing a terminal window: programs receive a hangup
signal, and the session's main program is stopped if it is still running a
few seconds later. As with a closed window, background jobs started with
`nohup` keep running. The command returns once the session is gone.

### The detach key

The detach key is **Ctrl-\\**. It is recognized however your terminal sends
it, including when a program has turned on enhanced keyboard reporting (as
Codex does), and it is ignored inside pasted text, so pasting can never
detach you by accident.

```sh
rterm -e '^a' work        # use Ctrl-A for this attach
export RTERM_DETACH_KEY='^]'
rterm -e none             # no detach key; leave by closing the terminal
```

Accepted forms are `^x`, `C-x` and `ctrl-x` for Ctrl plus a letter or one of
`\ ] ^ _ @`. Ctrl-\[ is Escape and can't be used.

### Inside a session

- `RTERM_SESSION` holds the session's name, so prompts and scripts can show
  it. `rterm detach` and `rterm kill` without a name act on the current
  session.
- If you use ssh agent forwarding, `SSH_AUTH_SOCK` points at a stable link
  that Rob Terminal re-points at the agent of whichever ssh connection
  attached most recently, so `git push` and `ssh` from inside the session keep
  working after you reconnect.
- Variables that claim the session runs inside tmux or screen (`TMUX`,
  `TMUX_PANE`, `STY`) are removed, so programs don't wrap their output for a
  multiplexer that isn't there.
- Running `rterm` inside a session works for other sessions; attaching a
  session to itself is refused.

### Every ssh login lands in Rob Terminal

Add this to `~/.ssh/config` on your own computer:

```sshconfig
Host myserver
    RequestTTY yes
    RemoteCommand rterm
```

Now `ssh myserver` always puts you in your persistent `main` session. For a
one-off, `ssh -t myserver rterm work`.

### Full-screen programs and AI coding agents

Because nothing sits between your terminal and your programs, editors,
pagers and AI CLIs (vim, less, Codex, Claude Code) behave exactly as they
would over plain ssh, including their mouse support, their enhanced keyboard
handling, and inline interfaces leaving their output in your terminal's
scrollback. Their screens survive detaching and reattaching intact.

Two things are worth knowing, and both are the same over plain ssh:

- **Selecting text while a program captures the mouse.** Programs like Codex
  turn on mouse reporting, so a normal drag selects inside the program. Hold
  **Shift** while dragging (Option in iTerm2, Fn in macOS Terminal) to make
  your terminal's own selection instead, then copy as usual.
- **Copies made by the program.** When a program copies text itself (Codex's
  copy-on-select, vim's clipboard over ssh), it asks your terminal to set the
  clipboard with an OSC 52 escape sequence. Some terminals ignore that,
  notably GNOME Terminal and the other VTE-based terminals on Ubuntu, and
  macOS Terminal. Connect with
  [`rterm-connect`](#rterm-connect-pick-a-session-from-your-own-computer) and
  those copies reach your clipboard in any terminal.

---

## rterm-connect: pick a session from your own computer

`rterm-connect` runs on your laptop or desktop. It lists the sessions on a
host, lets you choose one or start a new one, and attaches over ssh.

```console
$ rterm-connect myserver
rterm sessions on myserver:

  1  build  detached   2h05m  120x40  make -j8
  2  main   attached     12m  200x50  user@server: ~/project

  n  new session
  q  quit

Attach to [1]:
```

Press Enter for the suggestion (the first detached session), type a number or
a session name, `n` for a new session, or `q` to quit. Typing a name that
doesn't exist offers to create it.

It comes with every Rob Terminal package. On a machine without one, download
just the script; all it needs is Python 3.8+ and the OpenSSH client:

```sh
curl -fsSLO https://guitar24t.github.io/rterm/rterm-connect.py && chmod +x rterm-connect.py
```

### Options

Options for `rterm-connect` go before the host; anything after the host goes
to ssh:

```sh
rterm-connect --list me@myserver -p 2222 -i ~/.ssh/work_key
```

| Option | Effect |
|---|---|
| `--list` | Show the sessions and exit |
| `--session NAME` | Attach to (or start) `NAME` without the menu |
| `-e KEY`, `--detach-key KEY` | Detach key for this connection, e.g. `'^a'` |
| `--rterm PATH` | Where `rterm` is on the host, if it isn't on the `PATH` |
| `--no-mux` | Use separate ssh connections for listing and attaching |
| `--no-clipboard` | Don't bring program copies to the local clipboard |

Listing and attaching share one ssh connection, so you authenticate once even
with passwords or two-factor prompts. If your ssh configuration already
shares connections for that host, yours is used.

### The clipboard bridge

While you're attached, `rterm-connect` watches the session's output for
OSC 52 clipboard requests and puts the text on your computer's clipboard
itself. Everything else passes through untouched. This is what makes
copy-on-select in Codex, and clipboard copies from vim and other programs
that copy over ssh, work in terminals that don't handle OSC 52 on their own.

It uses `wl-copy`, `xclip`, `xsel`, `pbcopy` or `clip.exe` when available.
With none of those installed, as on a stock Ubuntu desktop, it owns the X11
clipboard itself; on GNOME and KDE Wayland sessions that goes through
XWayland and is shared with Wayland apps. Nothing extra needs installing.

| Variable | Effect |
|---|---|
| `RTERM_CONNECT_CLIPBOARD_CMD` | Your own copy command; it receives the text on stdin |
| `RTERM_CONNECT_DEBUG=FILE` | Log each copy and how it was delivered |

---

## Reference

### Commands

| Command | Alias | What it does |
|---|---|---|
| `rterm [NAME]` | | Attach to `NAME` (default `main`), creating it if needed |
| `rterm attach [NAME] [-- CMD...]` | `a` | Same; `-x` fails instead of creating; `CMD` runs instead of a shell when creating |
| `rterm new [NAME] [-- CMD...]` | `n` | Create a session and attach; `-d` creates it in the background |
| `rterm ls [--json]` | `list` | List sessions; `--json` for scripts |
| `rterm detach [NAME]` | `d` | Detach whoever is attached to `NAME` (default: this session) |
| `rterm kill [NAME]` | `k` | End `NAME` (default: this session) and wait until it's gone |

Options for attaching and creating:

| Option | Default | Effect |
|---|---|---|
| `-e`, `--detach-key KEY` | `^\` | Detach key, or `none` |
| `--scrollback LINES` | `10000` | History a new session keeps for replay on reattach |

### Environment variables

| Variable | Where | Effect |
|---|---|---|
| `RTERM_DETACH_KEY` | your shell | Default for `-e` |
| `RTERM_SCROLLBACK` | your shell | Default for `--scrollback` |
| `RTERM_SOCKET_DIR` | your shell | Where sessions live (default `/tmp/rterm-$UID`) |
| `RTERM_SESSION` | set inside sessions | The session's name |
| `SSH_AUTH_SOCK` | set inside sessions | Follows the most recently attached ssh agent |
| `RTERM_NO_AUTO_UPDATE=1` | installer | Don't enable unattended upgrades on Ubuntu |

### Exit status

`rterm` exits with 0 after detaching, with the session's own exit code when
the session ends, and with 1 on errors or if the connection to the session is
lost.

### `rterm ls --json`

An array with one object per session:

```json
[{"name":"main","status":"detached","created":1790980672,"age":14,"age_text":"14s",
  "cols":100,"rows":30,"pid":4056,"command":"bash","title":"user@server: ~"}]
```

`status` is `attached`, `detached`, or `unknown` (with an `error` field) for a
session that couldn't be queried; `created` is a Unix timestamp; `pid` is the
session's main process.

---

## How it works

```
 your terminal ── ssh ── rterm (client) ══ Unix socket ══ session daemon ── pseudo-terminal ── shell, programs
                         raw byte relay                    │
                                                           └─ shadow terminal (only used on reattach/detach)
```

- **One daemon per session.** Creating a session starts a small background
  process that owns a pseudo-terminal and the shell running on it. It is not
  tied to your ssh connection, so it survives when that connection goes away.
  It listens on a Unix socket in `/tmp/rterm-$UID/`, a directory only you can
  access.
- **A transparent client.** The `rterm` you run puts your terminal in raw mode
  and relays bytes in both directions, unchanged. That is why everything
  behaves natively: there is nothing in the path that interprets output.
- **A shadow terminal.** The daemon also feeds the output into an in-memory
  terminal emulator (the one from the Alacritty project). It's used at
  exactly two moments: when you attach, to redraw the screen, scrollback and
  modes into your terminal; and when you detach, to switch those modes off
  again. While nobody is attached, it also answers the questions programs ask
  their terminal (cursor position, device attributes), so they don't hang.
- **Housekeeping.** If a /tmp cleaner deletes a session's socket, the daemon
  recreates it; it also keeps its files fresh so age-based cleaners leave
  them alone. A daemon that crashes leaves its error output in
  `/tmp/rterm-$UID/NAME.log`.

### Updates and running sessions

Package updates never interrupt running sessions: each session keeps running
the version it started with, and the new `rterm` attaches to it as before. The
protocol between `rterm` and its sessions is versioned. Should it ever change,
a newer `rterm` on Linux hands an older session over to that session's own
binary, so sessions started before an upgrade stay reachable.

---

## Troubleshooting and FAQ

**"rterm needs a terminal".** You ran it through ssh without a terminal; use
`ssh -t host rterm`, or `RequestTTY yes` in your ssh config.

**Sessions disappear when I log out.** Some systems run systemd with
`KillUserProcesses=yes` (not the default on Ubuntu or RHEL), which ends
everything you started when your last login session closes. Run
`loginctl enable-linger $USER` once, or start sessions outside your login
scope with `systemd-run --user --scope rterm`.

**Copying from Codex (or vim over ssh) doesn't reach my clipboard.** Your
terminal ignores OSC 52 clipboard requests. Connect with `rterm-connect`,
which delivers those copies itself, or hold Shift while dragging to use your
terminal's own selection.

**The history appears twice.** You reattached in the same terminal window you
detached from, so the scrollback was replayed below the copy already there.
A fresh window shows it once.

**Ctrl-\\ is a key I need.** Choose another with `-e` or `RTERM_DETACH_KEY`,
or turn it off with `-e none`.

**Colours or keys look wrong after switching terminal apps.** A session keeps
the `TERM` of the terminal that created it, as an ssh login would. Start a new
session from the new terminal.

**My terminal is in a strange state after rterm was killed.** If the `rterm`
client is killed forcibly (`kill -9`), it can't switch the session's modes
off. Run `reset`.

---

## Limitations

- One terminal is attached to a session at a time.
- Images drawn with sixel or kitty graphics aren't restored on reattach; they
  return when the program next redraws them.
- History beyond the scrollback limit (`--scrollback`, 10,000 lines by
  default) isn't kept for replay.
- Sessions don't survive a reboot of the machine they run on.

---

## Development

```sh
cargo test                                            # unit and end-to-end tests
python3 -m unittest discover -s contrib               # rterm-connect tests
RTERM_E2E_APP='vim -u NONE' cargo test --test e2e real_app -- --ignored --nocapture
```

The end-to-end tests run the real binary on a pseudo-terminal whose other side
is an in-memory terminal, and check that detaching and reattaching reproduces
the screen exactly. `real_app_roundtrip` does the same for any program you
name (vim, less, codex, claude, ...).

### Releasing

Bump `version` in `Cargo.toml`, commit, then push a matching tag:

```sh
git tag v0.2.0 && git push origin v0.2.0
```

The Release workflow runs the tests, builds static x86_64 and arm64 Linux
binaries and a universal macOS binary, packages and signs them, installs them
from the freshly built repository on Ubuntu 24.04, Ubuntu 26.04 and RHEL 9
(both architectures) and through Homebrew on macOS, and only then publishes
the GitHub release, updates the apt/dnf repository on GitHub Pages and pushes
the new formula to [guitar24t/homebrew-tap](https://github.com/guitar24t/homebrew-tap)
(through the `HOMEBREW_TAP_DEPLOY_KEY` deploy key). Every push to `main` runs
the same packaging and install tests with a throwaway key.

Packages are signed with the key in `packaging/rterm-signing-key.asc`; its
private half lives in the `GPG_PRIVATE_KEY` / `GPG_PASSPHRASE` repository
secrets. If the protocol between client and daemon changes, bump
`protocol::VERSION` and keep the compatibility rules described there, so that
upgrades never strand running sessions.

## License

MIT. See [LICENSE](LICENSE).
