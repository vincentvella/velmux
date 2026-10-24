//! The socket clients and the server talk over: a Unix domain socket, which
//! Windows 10 (1803) and later have too (std only exposes it on Unix).

#[cfg(unix)]
pub use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(windows)]
pub use uds_windows::{UnixListener, UnixStream};
