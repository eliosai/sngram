//! Outcome of registering a single directory with inotify.

use std::io;

pub enum Registered {
    Added,
    Skipped,
    Exhausted,
}

impl Registered {
    pub const fn is_spent(&self) -> bool {
        matches!(self, Self::Exhausted)
    }
}

/// A vanished or unreadable directory is skipped; a spent kernel resource
/// exhausts the tree instead of failing the daemon
pub fn tolerate_unwatchable(err: io::Error) -> anyhow::Result<Registered> {
    if matches!(
        err.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
    ) {
        return Ok(Registered::Skipped);
    }
    if is_resource_exhausted(&err) {
        return Ok(Registered::Exhausted);
    }
    Err(err.into())
}

fn is_resource_exhausted(err: &io::Error) -> bool {
    matches!(
        err.raw_os_error(),
        Some(libc::ENOSPC | libc::EMFILE | libc::ENFILE | libc::ENOMEM)
    )
}

#[cfg(test)]
mod tests {
    use super::{Registered, tolerate_unwatchable};
    use std::io;

    #[test]
    fn a_spent_kernel_resource_is_not_a_daemon_error() {
        for errno in [libc::ENOSPC, libc::EMFILE, libc::ENFILE, libc::ENOMEM] {
            let outcome = tolerate_unwatchable(io::Error::from_raw_os_error(errno))
                .expect("resource limits must not fail the daemon");
            assert!(matches!(outcome, Registered::Exhausted), "errno {errno}");
        }
    }

    #[test]
    fn vanished_and_unreadable_directories_are_skipped() {
        for errno in [libc::ENOENT, libc::EACCES] {
            let outcome =
                tolerate_unwatchable(io::Error::from_raw_os_error(errno)).expect("tolerated");
            assert!(matches!(outcome, Registered::Skipped), "errno {errno}");
        }
    }

    #[test]
    fn an_unexpected_errno_still_fails() {
        assert!(tolerate_unwatchable(io::Error::from_raw_os_error(libc::EIO)).is_err());
    }
}
