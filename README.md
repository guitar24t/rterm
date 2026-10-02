# rterm

Persistent terminal sessions that behave like a plain terminal.

`rterm` gives you the one thing people use tmux/screen for on remote machines —
**processes keep running when you disconnect, and you can reattach later** —
without the terminal multiplexer. While you're attached, your terminal talks
to the programs directly, exactly like a normal ssh session:

- native scrollback (scroll wheel, scrollbar, search in your terminal)
- native selection and copy/paste, no "mouse mode", no copy mode
- no prefix key, no status bar, no `TERM=tmux-256color`
- programs get your real terminal's features: truecolor, hyperlinks, the
  kitty keyboard protocol, mouse reporting, focus events, bracketed paste,
  OSC 52, inline images, …
- TUIs like Codex, Claude Code, vim and less work as they do outside a multiplexer

## Install

On Ubuntu 24.04+ / Debian or RHEL 9 / Rocky / Alma (x86_64 or arm64):

```sh
curl -fsSL https://guitar24t.github.io/rterm/install.sh | sudo sh
```

This adds rterm's signed apt/dnf repository and installs the package, so
`apt upgrade` / `dnf upgrade` (and, on Ubuntu, unattended-upgrades) keep it
current. Manual repository setup and the signing key fingerprint are on
<https://guitar24t.github.io/rterm/>. Each
[GitHub release](https://github.com/guitar24t/rterm/releases) also carries the
.deb/.rpm packages and static binaries.

Install it on the machine where the sessions should live (usually the remote
server). To build from source instead (Linux or macOS): `cargo install --path .`

## Use

```sh
rterm                 # attach to session "main", creating it if needed
rterm work            # attach to (or create) session "work"
rterm new build -- make -j8   # new session running a command instead of a shell
rterm new -d logs -- tail -f /var/log/syslog   # start in the background
rterm ls              # list sessions
rterm detach [NAME]   # detach whoever is attached (inside a session: this one)
rterm kill NAME       # hang up a session
```

Detach with **Ctrl-\\**, or just close the terminal / drop the ssh connection;
the session keeps running. Run `rterm` again (from any terminal) to get it
back. Attaching from a second terminal takes the session over and detaches
the first one, so a dead connection never locks you out.

Change the detach key with `-e '^a'` or `RTERM_DETACH_KEY`; `-e none`
disables it.

### Make every ssh login persistent

```sshconfig
# ~/.ssh/config
Host myserver
    RequestTTY yes
    RemoteCommand rterm
```

or ad hoc: `ssh -t myserver rterm`. Use `ssh -t myserver rterm other` for a
second session.

### Picking a session from your laptop

`contrib/rterm-connect.py` runs on your own machine: it lists the sessions on
a host, lets you pick one (or start a new one), and attaches over ssh. It
needs Python 3.8+ and the OpenSSH client, nothing else. Download it from
<https://guitar24t.github.io/rterm/rterm-connect.py> or copy it from this repo.

```console
$ rterm-connect.py myserver
rterm sessions on myserver:

  1  main   detached  3h05m  120x40  vim README.md
  2  build  attached    12m  200x50  make -j8

  n  new session
  q  quit

Attach to [1]:
```

Press Enter for the suggested session, type a number or a session name, or
`n` for a new one. Options go before the host and ssh options after it:
`rterm-connect.py --list me@host -p 2222`, `--session NAME` to skip the menu,
`-e '^a'` for a different detach key, `--rterm PATH` if rterm isn't on the
host's `PATH`. Listing and attaching share one ssh connection, so you
authenticate once.

**Clipboard.** Programs on the host copy text by sending your terminal an
OSC 52 escape sequence (Codex's copy-on-select, vim, many TUIs). Many
terminals ignore it, including GNOME Terminal and the other VTE-based
terminals on Ubuntu, and macOS Terminal. While attached, `rterm-connect.py`
watches for these sequences and puts the text on your local clipboard itself,
so copying works in any terminal; everything else passes through unchanged.
It uses `wl-copy`, `xclip`, `xsel`, `pbcopy` or `clip.exe` if available, and
otherwise owns the X11 selection itself (on GNOME/KDE Wayland sessions this
goes through XWayland and is shared with Wayland apps), so nothing needs to be
installed. `--no-clipboard` turns it off; `RTERM_CONNECT_CLIPBOARD_CMD` sets
your own command (it receives the text on stdin); `RTERM_CONNECT_DEBUG=FILE`
logs what was copied and how.

To use your terminal's own selection while a program has captured the mouse
(as Codex does), hold **Shift** while dragging (Option in iTerm2, Fn in macOS
Terminal), then copy as usual.

## How it works

Each session is a small background daemon that owns a pseudo-terminal and
the program running on it, and listens on a Unix socket
(`/tmp/rterm-$UID/NAME.sock`, override with `RTERM_SOCKET_DIR`). The `rterm`
client puts your terminal in raw mode and relays bytes both ways **unchanged**
— there is no terminal emulator in the live path, which is why everything
behaves natively.

The daemon also feeds the program's output into an invisible terminal
emulator (alacritty's). That shadow copy is used only at two moments:

- **Reattach**: rterm redraws the session into your terminal — the scrollback
  (replayed so it lands in your terminal's own scrollback), the screen, the
  cursor, colors, and the modes programs had turned on (alternate screen,
  mouse reporting, bracketed paste, kitty keyboard flags, scroll region, …).
  If your terminal is a different size, the session is resized and reflowed.
- **Detach**: rterm undoes those modes, so your own shell isn't left with
  mouse reporting, an alternate screen or kitty-encoded keys.

While nobody is attached, the shadow terminal also answers programs' queries
(cursor position, device attributes) so they don't hang waiting.

### Small things that make it feel like ssh

- The session runs your login shell (`$SHELL`, as a login shell) in the
  directory you started it from, with your environment.
- `SSH_AUTH_SOCK` inside the session points at a stable symlink that is
  re-pointed at the agent of whichever ssh connection attached last, so agent
  forwarding keeps working after you reconnect.
- `RTERM_SESSION` is set inside the session; `rterm detach` with no name
  detaches the session you're in.
- The session's exit code becomes `rterm`'s exit code.

## Limits and caveats

- **systemd `KillUserProcesses=yes`** (off by default on most distros) kills
  everything you started when you log out, rterm included. Either enable
  lingering (`loginctl enable-linger $USER`) or start rterm outside the login
  scope: `systemd-run --user --scope rterm`.
- One terminal is attached at a time (attaching elsewhere takes over).
- Reattaching in the same terminal window you detached from replays the
  scrollback again, so it appears twice in that window's history.
- The session inherits `TERM` from the terminal that created it, as with ssh.
- Images drawn with sixel/kitty graphics aren't redrawn on reattach;
  programs redraw them when they next repaint.
- `--scrollback` (default 10 000 lines) bounds how much history is kept for
  replay.

## Development

```sh
cargo test                       # unit + end-to-end tests
RTERM_E2E_APP='vim -u NONE' cargo test --test e2e real_app -- --ignored --nocapture
```

The end-to-end tests run the real binary on a PTY whose other side is an
in-memory terminal, and check that detaching and reattaching reproduces the
screen exactly. `real_app_roundtrip` does the same for any program you name
(vim, less, codex, claude, …).

### Releasing

Bump `version` in `Cargo.toml`, commit, then push a matching tag:

```sh
git tag v0.2.0 && git push origin v0.2.0
```

The Release workflow runs the tests, builds static x86_64 and arm64 binaries,
packages and signs them, installs them from the freshly built repository on
Ubuntu 24.04, Ubuntu 26.04 and RHEL 9 (both architectures), and only then
publishes the GitHub release and updates the apt/dnf repository on GitHub
Pages. Every push to `main` runs the same packaging and install tests with a
throwaway key.

Packages are signed with the key in `packaging/rterm-signing-key.asc`; its
private half lives in the `GPG_PRIVATE_KEY` / `GPG_PASSPHRASE` repository
secrets. If the protocol between client and daemon changes, bump
`protocol::VERSION` and keep the compatibility rules described there, so that
upgrades never strand running sessions.
