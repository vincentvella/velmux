//! A pty master and child rebuilt from a raw fd and pid, for panes adopted
//! across a live restart (the old server execs the new binary with the pty
//! masters left open; the shells and agents on the other end never notice).

use std::fs::File;
use std::io::{Error as IoError, Result as IoResult};
use std::os::fd::{FromRawFd, RawFd};

use anyhow::Error;
use portable_pty::{Child, ChildKiller, ExitStatus, MasterPty, PtySize};

pub struct RawMaster {
    fd: RawFd,
}

impl RawMaster {
    /// Takes ownership of `fd` (closed on drop).
    pub fn new(fd: RawFd) -> RawMaster {
        // Back to close-on-exec, now that the hand-off is done.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        RawMaster { fd }
    }

    fn dup(&self) -> Result<File, Error> {
        let fd = unsafe { libc::fcntl(self.fd, libc::F_DUPFD_CLOEXEC, 0) };
        if fd < 0 {
            return Err(IoError::last_os_error().into());
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

impl Drop for RawMaster {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

impl MasterPty for RawMaster {
    fn resize(&self, size: PtySize) -> Result<(), Error> {
        let ws = libc::winsize { ws_row: size.rows, ws_col: size.cols, ws_xpixel: size.pixel_width, ws_ypixel: size.pixel_height };
        if unsafe { libc::ioctl(self.fd, libc::TIOCSWINSZ, &ws) } != 0 {
            return Err(IoError::last_os_error().into());
        }
        Ok(())
    }

    fn get_size(&self) -> Result<PtySize, Error> {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(self.fd, libc::TIOCGWINSZ, &mut ws) } != 0 {
            return Err(IoError::last_os_error().into());
        }
        Ok(PtySize { rows: ws.ws_row, cols: ws.ws_col, pixel_width: ws.ws_xpixel, pixel_height: ws.ws_ypixel })
    }

    fn try_clone_reader(&self) -> Result<Box<dyn std::io::Read + Send>, Error> {
        Ok(Box::new(self.dup()?))
    }

    fn take_writer(&self) -> Result<Box<dyn std::io::Write + Send>, Error> {
        Ok(Box::new(self.dup()?))
    }

    fn process_group_leader(&self) -> Option<libc::pid_t> {
        let pgid = unsafe { libc::tcgetpgrp(self.fd) };
        (pgid > 0).then_some(pgid)
    }

    fn as_raw_fd(&self) -> Option<RawFd> {
        Some(self.fd)
    }

    fn tty_name(&self) -> Option<std::path::PathBuf> {
        None
    }
}

/// A child we didn't spawn in this process image, but whose parent we still are
/// (exec keeps the pid), so `waitpid` works.
#[derive(Debug)]
pub struct RawChild {
    pid: libc::pid_t,
}

impl RawChild {
    pub fn new(pid: u32) -> RawChild {
        RawChild { pid: pid as libc::pid_t }
    }

    fn reap(&self, flags: libc::c_int) -> IoResult<Option<ExitStatus>> {
        let mut status = 0;
        let r = unsafe { libc::waitpid(self.pid, &mut status, flags) };
        if r < 0 {
            return Err(IoError::last_os_error());
        }
        if r == 0 {
            return Ok(None);
        }
        let code = if libc::WIFEXITED(status) { libc::WEXITSTATUS(status) as u32 } else { 1 };
        Ok(Some(ExitStatus::with_exit_code(code)))
    }
}

impl ChildKiller for RawChild {
    fn kill(&mut self) -> IoResult<()> {
        if unsafe { libc::kill(self.pid, libc::SIGHUP) } != 0 {
            return Err(IoError::last_os_error());
        }
        Ok(())
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(RawChild { pid: self.pid })
    }
}

impl Child for RawChild {
    fn try_wait(&mut self) -> IoResult<Option<ExitStatus>> {
        self.reap(libc::WNOHANG)
    }

    fn wait(&mut self) -> IoResult<ExitStatus> {
        loop {
            if let Some(s) = self.reap(0)? {
                return Ok(s);
            }
        }
    }

    fn process_id(&self) -> Option<u32> {
        Some(self.pid as u32)
    }
}

/// Keep `fd` open across `exec` (clear close-on-exec).
pub fn keep_across_exec(fd: RawFd) -> IoResult<()> {
    if unsafe { libc::fcntl(fd, libc::F_SETFD, 0) } != 0 {
        return Err(IoError::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn adopts_a_live_pty_by_fd() {
        // Open a real pty, run `cat` on it, then drive it through RawMaster.
        let pair = portable_pty::native_pty_system().openpty(PtySize { rows: 10, cols: 40, pixel_width: 0, pixel_height: 0 }).unwrap();
        let child = pair.slave.spawn_command(portable_pty::CommandBuilder::new("cat")).unwrap();
        drop(pair.slave);
        let fd = unsafe { libc::dup(pair.master.as_raw_fd().unwrap()) };
        let master = RawMaster::new(fd);
        master.resize(PtySize { rows: 20, cols: 50, pixel_width: 0, pixel_height: 0 }).unwrap();
        assert_eq!(master.get_size().unwrap().cols, 50);
        let mut w = master.take_writer().unwrap();
        let mut r = master.try_clone_reader().unwrap();
        w.write_all(b"ping\n").unwrap();
        let mut got = Vec::new();
        let mut buf = [0u8; 64];
        while !String::from_utf8_lossy(&got).matches("ping").count().ge(&2) {
            let n = r.read(&mut buf).unwrap();
            got.extend_from_slice(&buf[..n]);
        }
        let mut raw = RawChild::new(child.process_id().unwrap());
        assert_eq!(raw.try_wait().unwrap().map(|s| s.success()), None, "still running");
        raw.kill().unwrap();
        assert!(raw.wait().is_ok());
    }
}
