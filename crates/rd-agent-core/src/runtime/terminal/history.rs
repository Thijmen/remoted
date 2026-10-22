//! A shell's output history: the last [`HISTORY_BYTES`] bytes, addressed by absolute offsets
//! (bytes since the shell started), so a channel can continue where its viewer is.
//!
//! [`HISTORY_BYTES`]: rd_proto::terminal::HISTORY_BYTES

use std::collections::VecDeque;

#[derive(Debug)]
pub(crate) struct History {
    buf: VecDeque<u8>,
    cap: usize,
    /// Offset of `buf[0]`.
    start: u64,
}

impl History {
    pub(crate) fn new(cap: usize) -> Self {
        History {
            buf: VecDeque::new(),
            cap: cap.max(1),
            start: 0,
        }
    }

    /// Offset of the oldest byte kept.
    #[cfg(test)]
    pub(crate) fn start(&self) -> u64 {
        self.start
    }

    /// Offset after the newest byte: everything the shell wrote so far.
    pub(crate) fn end(&self) -> u64 {
        self.start + self.buf.len() as u64
    }

    /// Appends output, dropping the oldest bytes beyond the capacity.
    pub(crate) fn push(&mut self, data: &[u8]) {
        let data = if data.len() > self.cap {
            let skip = data.len() - self.cap;
            self.start += (self.buf.len() + skip) as u64;
            self.buf.clear();
            &data[skip..]
        } else {
            data
        };
        let over = (self.buf.len() + data.len()).saturating_sub(self.cap);
        if over > 0 {
            self.buf.drain(..over);
            self.start += over as u64;
        }
        self.buf.extend(data);
    }

    /// Where output continues for a channel at `cursor`: `cursor` itself, or the oldest byte
    /// kept when the history moved past it.
    pub(crate) fn clamp(&self, cursor: u64) -> u64 {
        cursor.clamp(self.start, self.end())
    }

    /// Up to `max` bytes from `cursor` (see [`History::clamp`]): (their offset, the bytes).
    pub(crate) fn read(&self, cursor: u64, max: usize) -> (u64, Vec<u8>) {
        let from = self.clamp(cursor);
        let skip = (from - self.start) as usize;
        let n = (self.buf.len() - skip).min(max);
        let (a, b) = self.buf.as_slices();
        let mut out = Vec::with_capacity(n);
        if skip < a.len() {
            let take = n.min(a.len() - skip);
            out.extend_from_slice(&a[skip..skip + take]);
            out.extend_from_slice(&b[..n - take]);
        } else {
            let skip = skip - a.len();
            out.extend_from_slice(&b[skip..skip + n]);
        }
        (from, out)
    }

    /// Where the replay of an `attach` from the viewer's `offset` starts, and whether the
    /// viewer must clear its screen first (it cannot continue from `offset`: output after it
    /// was dropped, or it claims more than the shell wrote).
    pub(crate) fn resume(&self, offset: u64) -> (u64, bool) {
        if (self.start..=self.end()).contains(&offset) {
            (offset, false)
        } else {
            (self.start, true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_newest_bytes() {
        let mut h = History::new(8);
        assert_eq!((h.start(), h.end()), (0, 0));
        assert_eq!(h.read(0, 100), (0, vec![]));
        h.push(b"hello");
        assert_eq!(h.read(0, 100), (0, b"hello".to_vec()));
        assert_eq!(h.read(1, 3), (1, b"ell".to_vec()));
        h.push(b" world");
        // 11 bytes written, 8 kept.
        assert_eq!((h.start(), h.end()), (3, 11));
        assert_eq!(h.read(0, 100), (3, b"lo world".to_vec()));
        assert_eq!(h.read(6, 100), (6, b"world".to_vec()));
        assert_eq!(h.read(11, 100), (11, vec![]));
        // A push larger than the capacity.
        h.push(b"0123456789abc");
        assert_eq!((h.start(), h.end()), (16, 24));
        assert_eq!(h.read(0, 100), (16, b"56789abc".to_vec()));
    }

    #[test]
    fn reads_across_the_wrap() {
        let mut h = History::new(6);
        h.push(b"abcd");
        h.push(b"efgh");
        // The deque wrapped: its two slices are joined in order.
        for at in 2..=8 {
            let (from, bytes) = h.read(at, 100);
            assert_eq!(from, at);
            assert_eq!(bytes, &b"abcdefgh"[at as usize..]);
        }
        assert_eq!(h.read(3, 2), (3, b"de".to_vec()));
        assert_eq!(h.read(5, 2), (5, b"fg".to_vec()));
    }

    #[test]
    fn resume_after_a_reconnect() {
        let mut h = History::new(10);
        h.push(b"0123456789");
        // Everything after the viewer's offset is still there.
        assert_eq!(h.resume(0), (0, false));
        assert_eq!(h.resume(4), (4, false));
        assert_eq!(h.resume(10), (10, false));
        h.push(b"abcde");
        assert_eq!(h.resume(5), (5, false));
        // Output after the viewer's offset was dropped: start over from the oldest byte.
        assert_eq!(h.resume(4), (5, true));
        assert_eq!(h.resume(0), (5, true));
        // A viewer that claims more than the shell wrote.
        assert_eq!(h.resume(99), (5, true));
        // A channel whose cursor the history overtook skips ahead.
        assert_eq!(h.clamp(2), 5);
        assert_eq!(h.clamp(12), 12);
    }
}
