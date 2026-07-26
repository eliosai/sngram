//! Raw inotify event records read from the watch descriptor.

#![allow(
    unsafe_code,
    reason = "inotify event records arrive as packed C structs"
)]

use std::mem;

pub struct ParsedEvent {
    wd: i32,
    mask: u32,
    name: Vec<u8>,
}

impl ParsedEvent {
    /// Split one event off the front of a read buffer
    pub fn take(bytes: &[u8]) -> anyhow::Result<Option<(Self, &[u8])>> {
        let header_len = mem::size_of::<libc::inotify_event>();
        if bytes.len() < header_len {
            return Ok(None);
        }
        let event = unsafe {
            bytes
                .as_ptr()
                .cast::<libc::inotify_event>()
                .read_unaligned()
        };
        let total_len = header_len.saturating_add(usize::try_from(event.len)?);
        if bytes.len() < total_len {
            return Ok(None);
        }
        let parsed = Self {
            wd: event.wd,
            mask: event.mask,
            name: bytes[header_len..total_len].to_vec(),
        };
        Ok(Some((parsed, &bytes[total_len..])))
    }

    pub const fn wd(&self) -> i32 {
        self.wd
    }

    pub fn name(&self) -> &[u8] {
        &self.name
    }

    pub const fn created_dir(&self) -> bool {
        self.mask & libc::IN_ISDIR != 0 && self.mask & (libc::IN_CREATE | libc::IN_MOVED_TO) != 0
    }

    /// True when the change reaches further than the single path it names
    pub const fn is_coarse(&self) -> bool {
        self.mask & libc::IN_ISDIR != 0
            || self.mask & (libc::IN_DELETE_SELF | libc::IN_MOVE_SELF | libc::IN_UNMOUNT) != 0
    }

    /// True when the kernel dropped events, so the queue is no longer complete
    pub const fn overflowed(&self) -> bool {
        self.mask & libc::IN_Q_OVERFLOW != 0
    }
}

#[cfg(test)]
mod tests {
    use super::ParsedEvent;
    use std::mem;

    fn encoded(wd: i32, mask: u32, name: &[u8]) -> Vec<u8> {
        let mut padded = name.to_vec();
        padded.push(0);
        while !padded.len().is_multiple_of(4) {
            padded.push(0);
        }
        let header = libc::inotify_event {
            wd,
            mask,
            cookie: 0,
            len: u32::try_from(padded.len()).expect("name length"),
        };
        let mut bytes = vec![0u8; mem::size_of::<libc::inotify_event>()];
        unsafe {
            bytes
                .as_mut_ptr()
                .cast::<libc::inotify_event>()
                .write_unaligned(header);
        }
        bytes.extend_from_slice(&padded);
        bytes
    }

    #[test]
    fn short_buffer_yields_no_event() {
        assert!(ParsedEvent::take(&[0u8; 3]).expect("take").is_none());
    }

    #[test]
    fn events_are_taken_in_order() {
        let mut bytes = encoded(7, libc::IN_MODIFY, b"first.txt");
        bytes.extend(encoded(9, libc::IN_CREATE | libc::IN_ISDIR, b"sub"));

        let (first, rest) = ParsedEvent::take(&bytes).expect("take").expect("first");
        assert_eq!(7, first.wd());
        assert!(first.name().starts_with(b"first.txt"));
        assert!(!first.created_dir());

        let (second, tail) = ParsedEvent::take(rest).expect("take").expect("second");
        assert_eq!(9, second.wd());
        assert!(second.created_dir());
        assert!(tail.is_empty());
    }

    #[test]
    fn truncated_payload_yields_no_event() {
        let bytes = encoded(3, libc::IN_MODIFY, b"partial");
        let clipped = &bytes[..bytes.len() - 2];

        assert!(ParsedEvent::take(clipped).expect("take").is_none());
    }
}
