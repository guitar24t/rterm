//! Local sockets between clients and session daemons: Unix domain sockets
//! everywhere (Windows has supported AF_UNIX since Windows 10 1803).

#[cfg(unix)]
pub use std::os::unix::net::{UnixListener as Listener, UnixStream as Stream};
#[cfg(windows)]
pub use uds_windows::{UnixListener as Listener, UnixStream as Stream};
