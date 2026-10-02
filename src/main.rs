//! rterm: persistent terminal sessions without a terminal multiplexer.
//!
//! A session keeps running in the background when you disconnect, and you
//! can reattach later, but while attached your terminal talks to the
//! programs directly: native scrollback, selection, copy/paste, mouse and
//! keyboard protocols all behave as in a plain ssh session.

mod client;
mod daemon;
mod keys;
mod paths;
mod protocol;
mod screen;
mod sys;

use std::ffi::OsString;
use std::process::ExitCode;

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand};

use crate::client::AttachOptions;
use crate::keys::{DEFAULT_DETACH_KEY, DetachKey};
use crate::protocol::WinSize;

const DEFAULT_SCROLLBACK: usize = 10_000;

#[derive(Parser)]
#[command(
    name = "rterm",
    version,
    about = "Persistent terminal sessions that behave like a plain terminal",
    long_about = "Persistent terminal sessions that behave like a plain terminal.\n\n\
        `rterm` with no arguments attaches to the session named \"main\", creating it if \
        needed. Detach with Ctrl-\\ (configurable with -e) or just close the terminal or \
        ssh connection; the session keeps running. Run `rterm` again to reattach.",
    args_conflicts_with_subcommands = true,
    subcommand_precedence_over_arg = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,

    /// Session to attach to, created if it doesn't exist [default: main]
    name: Option<String>,

    #[command(flatten)]
    opts: AttachFlags,
}

#[derive(Args, Clone)]
struct AttachFlags {
    /// Key that detaches, e.g. '^\' (Ctrl-\, the default), '^a', or 'none'
    #[arg(short = 'e', long, env = "RTERM_DETACH_KEY", value_name = "KEY")]
    detach_key: Option<String>,

    /// Scrollback lines a new session keeps for replay on reattach
    #[arg(long, env = "RTERM_SCROLLBACK", value_name = "LINES", default_value_t = DEFAULT_SCROLLBACK)]
    scrollback: usize,
}

#[derive(Subcommand)]
enum Cmd {
    /// Attach to a session, creating it if it doesn't exist
    #[command(visible_alias = "a")]
    Attach {
        /// Session name [default: main]
        name: Option<String>,
        /// Fail instead of creating the session when it doesn't exist
        #[arg(short = 'x', long)]
        existing: bool,
        #[command(flatten)]
        opts: AttachFlags,
        /// Program to run instead of your login shell (when creating)
        #[arg(last = true, value_name = "COMMAND")]
        command: Vec<OsString>,
    },
    /// Create a new session (and attach to it unless -d is given)
    #[command(visible_alias = "n")]
    New {
        /// Session name [default: main, or the first free number]
        name: Option<String>,
        /// Start the session in the background without attaching
        #[arg(short, long)]
        detached: bool,
        #[command(flatten)]
        opts: AttachFlags,
        /// Program to run instead of your login shell
        #[arg(last = true, value_name = "COMMAND")]
        command: Vec<OsString>,
    },
    /// List sessions
    #[command(visible_alias = "ls")]
    List {
        /// Print the list as JSON (for scripts)
        #[arg(long)]
        json: bool,
    },
    /// Detach whoever is attached to a session [default: the current one]
    #[command(visible_alias = "d")]
    Detach { name: Option<String> },
    /// End a session by hanging up its terminal
    #[command(visible_alias = "k")]
    Kill { name: Option<String> },
    #[command(name = "__daemon", hide = true)]
    Daemon {
        #[arg(long)]
        name: String,
        #[arg(long)]
        rows: u16,
        #[arg(long)]
        cols: u16,
        #[arg(long, default_value_t = 0)]
        xpix: u16,
        #[arg(long, default_value_t = 0)]
        ypix: u16,
        #[arg(long)]
        scrollback: usize,
        #[arg(last = true)]
        command: Vec<OsString>,
    },
}

fn attach_options(
    flags: &AttachFlags,
    create: bool,
    command: Vec<OsString>,
) -> Result<AttachOptions> {
    let spec = flags.detach_key.as_deref().unwrap_or(DEFAULT_DETACH_KEY);
    Ok(AttachOptions {
        create,
        command,
        detach_key: DetachKey::parse(spec)?,
        scrollback: flags.scrollback,
    })
}

fn session_name(name: Option<String>) -> Result<String> {
    let name = name.unwrap_or_else(|| paths::DEFAULT_SESSION.to_owned());
    paths::validate_name(&name)?;
    Ok(name)
}

/// The session named on the command line, else the one we're running in.
fn current_or_named(name: Option<String>, verb: &str) -> Result<String> {
    match name.or_else(|| {
        std::env::var("RTERM_SESSION")
            .ok()
            .filter(|s| !s.is_empty())
    }) {
        Some(n) => {
            paths::validate_name(&n)?;
            Ok(n)
        }
        None => bail!("not inside an rterm session; say which session to {verb}"),
    }
}

fn first_free_name() -> Result<String> {
    let dir = paths::ensure_socket_dir()?;
    if !client::session_exists(&dir, paths::DEFAULT_SESSION)? {
        return Ok(paths::DEFAULT_SESSION.to_owned());
    }
    for i in 1.. {
        let n = i.to_string();
        if !client::session_exists(&dir, &n)? {
            return Ok(n);
        }
    }
    unreachable!()
}

fn run(cli: Cli) -> Result<i32> {
    match cli.command {
        None => {
            let name = session_name(cli.name)?;
            client::attach(&name, &attach_options(&cli.opts, true, Vec::new())?)
        }
        Some(Cmd::Attach {
            name,
            existing,
            opts,
            command,
        }) => {
            let name = session_name(name)?;
            client::attach(&name, &attach_options(&opts, !existing, command)?)
        }
        Some(Cmd::New {
            name,
            detached,
            opts,
            command,
        }) => {
            let name = match name {
                Some(n) => session_name(Some(n))?,
                None => first_free_name()?,
            };
            let options = attach_options(&opts, true, command)?;
            if detached {
                client::new_detached(&name, &options)?;
                println!("created session '{name}'");
                return Ok(0);
            }
            let dir = paths::ensure_socket_dir()?;
            if client::session_exists(&dir, &name)? {
                bail!("session {name:?} already exists (use `rterm attach {name}`)");
            }
            client::attach(&name, &options)
        }
        Some(Cmd::List { json }) => client::ls(json).map(|_| 0),
        Some(Cmd::Detach { name }) => client::detach(&current_or_named(name, "detach")?).map(|_| 0),
        Some(Cmd::Kill { name }) => client::kill(&current_or_named(name, "kill")?).map(|_| 0),
        Some(Cmd::Daemon {
            name,
            rows,
            cols,
            xpix,
            ypix,
            scrollback,
            command,
        }) => {
            paths::validate_name(&name)?;
            daemon::run(daemon::DaemonArgs {
                name,
                size: WinSize {
                    rows,
                    cols,
                    xpix,
                    ypix,
                },
                scrollback,
                command,
            })
            .map(|_| 0)
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("rterm: {e:#}");
            ExitCode::from(1)
        }
    }
}
