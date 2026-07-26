//! Thin owner of one inotify file descriptor.

#![allow(unsafe_code, reason = "Linux inotify is exposed through libc FFI")]

use std::{
    ffi::CString,
    io,
    os::{fd::RawFd, unix::ffi::OsStrExt},
    path::Path,
    time::Duration,
};

const WATCH_MASK: u32 = libc::IN_ATTRIB
    | libc::IN_CLOSE_WRITE
    | libc::IN_CREATE
    | libc::IN_DELETE
    | libc::IN_DELETE_SELF
    | libc::IN_MODIFY
    | libc::IN_MOVE_SELF
    | libc::IN_MOVED_FROM
    | libc::IN_MOVED_TO;

pub struct Inotify {
    fd: RawFd,
}

impl Inotify {
    pub fn open() -> io::Result<Self> {
        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd })
    }

    /// Register one directory, returning its watch descriptor
    pub fn add(&self, dir: &Path) -> io::Result<i32> {
        let path = CString::new(dir.as_os_str().as_bytes())
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let wd = unsafe { libc::inotify_add_watch(self.fd, path.as_ptr(), WATCH_MASK) };
        if wd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(wd)
    }

    pub fn remove(&self, wd: i32) {
        unsafe { libc::inotify_rm_watch(self.fd, wd) };
    }

    /// Block until events are readable or the timeout elapses
    pub fn wait(&self, timeout: Duration) -> io::Result<bool> {
        let mut pollfd = libc::pollfd {
            fd: self.fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout = timeout.as_millis().try_into().unwrap_or(i32::MAX);
        let ready = unsafe { libc::poll(&raw mut pollfd, 1, timeout) };
        match ready.cmp(&0) {
            std::cmp::Ordering::Greater => Ok(pollfd.revents & libc::POLLIN != 0),
            std::cmp::Ordering::Equal => Ok(false),
            std::cmp::Ordering::Less => Err(io::Error::last_os_error()),
        }
    }

    /// Read pending events, or `None` when the queue is drained
    pub fn read(&self, buffer: &mut [u8]) -> io::Result<Option<usize>> {
        let len = unsafe {
            libc::read(
                self.fd,
                buffer.as_mut_ptr().cast::<libc::c_void>(),
                buffer.len(),
            )
        };
        match len.cmp(&0) {
            std::cmp::Ordering::Greater => Ok(Some(usize::try_from(len).unwrap_or_default())),
            std::cmp::Ordering::Equal => Ok(None),
            std::cmp::Ordering::Less => drained_or_error(),
        }
    }
}

fn drained_or_error() -> io::Result<Option<usize>> {
    let err = io::Error::last_os_error();
    if err.kind() == io::ErrorKind::WouldBlock {
        return Ok(None);
    }
    Err(err)
}

impl Drop for Inotify {
    fn drop(&mut self) {
        let _ = unsafe { libc::close(self.fd) };
    }
}

#[cfg(test)]
mod tests {
    use super::Inotify;
    use std::{path::Path, time::Duration};

    #[test]
    fn adding_a_missing_directory_reports_not_found() {
        let inotify = Inotify::open().expect("inotify");

        let err = inotify
            .add(Path::new("/nonexistent/eg-watch-probe"))
            .expect_err("missing dir");

        assert_eq!(std::io::ErrorKind::NotFound, err.kind());
    }

    #[test]
    fn an_idle_descriptor_times_out_and_reads_nothing() {
        let inotify = Inotify::open().expect("inotify");
        let mut buffer = [0u8; 64];

        assert!(!inotify.wait(Duration::from_millis(1)).expect("wait"));
        assert_eq!(None, inotify.read(&mut buffer).expect("read"));
    }
}
