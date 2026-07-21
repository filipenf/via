//! Fixed-capacity byte ring for PTY scrollback replay on Attach.

/// Default scrollback capacity (~4 MiB) from the remote-execution spike.
pub const DEFAULT_SCROLLBACK_CAP: usize = 4 * 1024 * 1024;

#[derive(Debug)]
pub struct ScrollbackRing {
    buf: Vec<u8>,
    cap: usize,
    /// Next write index in `buf` once full (ring mode).
    start: usize,
    len: usize,
}

impl ScrollbackRing {
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            buf: Vec::new(),
            cap: cap.max(1),
            start: 0,
            len: 0,
        }
    }

    pub fn push(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        if data.len() >= self.cap {
            // Keep only the trailing window.
            self.buf.clear();
            self.buf.extend_from_slice(&data[data.len() - self.cap..]);
            self.start = 0;
            self.len = self.cap;
            return;
        }

        if self.buf.capacity() < self.cap {
            self.buf.reserve(self.cap - self.buf.capacity());
        }

        for &b in data {
            if self.len < self.cap {
                self.buf.push(b);
                self.len += 1;
            } else {
                if self.buf.len() < self.cap {
                    self.buf.resize(self.cap, 0);
                }
                self.buf[self.start] = b;
                self.start = (self.start + 1) % self.cap;
            }
        }
    }

    pub fn snapshot(&self) -> Vec<u8> {
        if self.len == 0 {
            return Vec::new();
        }
        if self.len < self.cap {
            return self.buf[..self.len].to_vec();
        }
        let mut out = Vec::with_capacity(self.len);
        out.extend_from_slice(&self.buf[self.start..]);
        out.extend_from_slice(&self.buf[..self.start]);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grows_until_cap_then_drops_oldest() {
        let mut ring = ScrollbackRing::with_capacity(8);
        ring.push(b"abcdef");
        assert_eq!(ring.snapshot(), b"abcdef");
        ring.push(b"ghij");
        assert_eq!(ring.snapshot(), b"cdefghij");
        assert_eq!(ring.snapshot().len(), 8);
    }

    #[test]
    fn oversized_chunk_keeps_tail() {
        let mut ring = ScrollbackRing::with_capacity(4);
        ring.push(b"0123456789");
        assert_eq!(ring.snapshot(), b"6789");
    }
}
