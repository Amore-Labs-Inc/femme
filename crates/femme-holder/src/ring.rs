//! Fixed-size tail buffer, used for the agent's stderr.

use std::collections::VecDeque;

/// Keeps the last `cap` bytes pushed into it.
pub struct Ring {
    buf: VecDeque<u8>,
    cap: usize,
    wrapped: bool,
}

impl Ring {
    pub fn new(cap: usize) -> Self {
        Ring {
            buf: VecDeque::new(),
            cap,
            wrapped: false,
        }
    }

    pub fn push(&mut self, data: &[u8]) {
        if data.len() >= self.cap {
            self.wrapped |= data.len() > self.cap;
            self.buf.clear();
            self.buf.extend(&data[data.len() - self.cap..]);
            return;
        }
        let over = (self.buf.len() + data.len()).saturating_sub(self.cap);
        if over > 0 {
            self.buf.drain(..over);
            self.wrapped = true;
        }
        self.buf.extend(data);
    }

    /// The kept bytes as text, oldest first. Invalid UTF-8 becomes U+FFFD; when the front was cut
    /// off, a character split by the cut is dropped rather than shown as garbage.
    pub fn tail_string(&self) -> String {
        let (a, b) = self.buf.as_slices();
        let mut v = Vec::with_capacity(self.buf.len());
        v.extend_from_slice(a);
        v.extend_from_slice(b);
        let start = if self.wrapped {
            v.iter().position(|b| b & 0xC0 != 0x80).unwrap_or(v.len())
        } else {
            0
        };
        String::from_utf8_lossy(&v[start..]).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_everything_until_full() {
        let mut r = Ring::new(8);
        r.push(b"abc");
        r.push(b"de");
        assert_eq!(r.tail_string(), "abcde");
    }

    #[test]
    fn keeps_only_the_tail() {
        let mut r = Ring::new(4);
        r.push(b"abc");
        r.push(b"defg");
        assert_eq!(r.tail_string(), "defg");
        r.push(b"h");
        assert_eq!(r.tail_string(), "efgh");
        r.push(b"0123456789");
        assert_eq!(r.tail_string(), "6789");
    }

    #[test]
    fn a_cut_character_is_dropped_not_garbled() {
        let mut r = Ring::new(4);
        r.push("aé€".as_bytes()); // 61 | c3 a9 | e2 82 ac
        assert_eq!(r.tail_string(), "€");
        let mut r = Ring::new(16);
        r.push("aé€".as_bytes());
        assert_eq!(r.tail_string(), "aé€");
    }

    #[test]
    fn exact_fit_is_not_a_wrap() {
        let mut r = Ring::new(3);
        r.push("€".as_bytes());
        assert_eq!(r.tail_string(), "€");
    }
}
