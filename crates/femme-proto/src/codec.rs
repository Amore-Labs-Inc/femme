//! Framing for every QUIC/SSH stream: `varint len | u8 flags | payload`, where `len` counts the
//! flags byte plus the payload, and flag bit 0 says the payload is zstd-compressed. The other flag
//! bits are reserved: a frame that sets one is refused, not ignored.
//! Payloads under [`MIN_COMPRESS`] bytes are never compressed (CRIME-style leaks, wasted CPU).
//! A zstd frame must record its decompressed size (the encoder here always does), and that size is
//! what the decoder reserves.
//!
//! ponytail: nothing caps the total a peer can make us decompress, only each frame (16 MiB, from a
//! few hundred bytes of zstd). The peers are the user's own devices; budget decompressed bytes per
//! connection if that ever stops being true.

use bytes::{Buf, Bytes, BytesMut};
use serde::de::DeserializeOwned;
use serde::Serialize;

pub const FLAG_ZSTD: u8 = 1;
pub const MIN_COMPRESS: usize = 256;
/// Largest frame a peer accepts (a bulk chunk of 256 rows or a 1 MiB blob fits well under this).
pub const MAX_FRAME: usize = 16 << 20;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("postcard: {0}")]
    Postcard(#[from] postcard::Error),
    #[error("zstd: {0}")]
    Zstd(std::io::Error),
    #[error("frame of {0} bytes exceeds the limit")]
    TooLarge(usize),
    #[error("bad varint")]
    BadVarint,
    #[error("empty frame: the length counts the flags byte, so it is at least 1")]
    EmptyFrame,
    #[error("unknown frame flags {0:#04x}")]
    BadFlags(u8),
    #[error("compressed frame but this build has no zstd")]
    NoZstd,
}

pub fn encode<T: Serialize + ?Sized>(v: &T) -> Result<Vec<u8>, Error> {
    Ok(postcard::to_stdvec(v)?)
}

pub fn decode<T: DeserializeOwned>(b: &[u8]) -> Result<T, Error> {
    Ok(postcard::from_bytes(b)?)
}

pub fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Returns `(value, bytes used)`, or `None` when `b` ends mid-varint. One value has one encoding:
/// zero padding and bits past 64 are refused.
pub fn get_varint(b: &[u8]) -> Result<Option<(u64, usize)>, Error> {
    let mut v = 0u64;
    for (i, &byte) in b.iter().enumerate() {
        // The tenth byte holds bit 63 only.
        if i >= 10 || (i == 9 && byte > 1) {
            return Err(Error::BadVarint);
        }
        v |= u64::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return if i > 0 && byte == 0 {
                Err(Error::BadVarint)
            } else {
                Ok(Some((v, i + 1)))
            };
        }
    }
    Ok(None)
}

/// Turns messages into frames on one stream. One per stream, kept for its life.
pub struct Framer {
    #[cfg(feature = "zstd")]
    z: Option<zstd::bulk::Compressor<'static>>,
    scratch: Vec<u8>,
}

impl Framer {
    /// `compress`: enable zstd for payloads ≥ [`MIN_COMPRESS`] (keystrokes streams say false).
    pub fn new(compress: bool) -> Self {
        #[cfg(feature = "zstd")]
        let z = if compress {
            // ponytail: bulk level-1 (context reused, no cross-frame history). Upgrade to
            // zstd::stream::raw with per-stream history if measured frames stay > ~1 KiB.
            zstd::bulk::Compressor::new(1).ok()
        } else {
            None
        };
        #[cfg(not(feature = "zstd"))]
        let _ = compress;
        Framer {
            #[cfg(feature = "zstd")]
            z,
            scratch: Vec::new(),
        }
    }

    pub fn frame<T: Serialize + ?Sized>(
        &mut self,
        msg: &T,
        out: &mut Vec<u8>,
    ) -> Result<(), Error> {
        self.scratch.clear();
        postcard::to_io(msg, &mut self.scratch)?;
        let payload = std::mem::take(&mut self.scratch);
        let r = self.frame_bytes(&payload, out);
        self.scratch = payload;
        r
    }

    pub fn frame_bytes(&mut self, payload: &[u8], out: &mut Vec<u8>) -> Result<(), Error> {
        // A frame the peer's `Deframer` would refuse (the length counts the flags byte).
        if payload.len() >= MAX_FRAME {
            return Err(Error::TooLarge(payload.len()));
        }
        #[cfg(feature = "zstd")]
        if payload.len() >= MIN_COMPRESS {
            if let Some(z) = self.z.as_mut() {
                let c = z.compress(payload).map_err(Error::Zstd)?;
                if c.len() < payload.len() {
                    put_varint(out, (c.len() + 1) as u64);
                    out.push(FLAG_ZSTD);
                    out.extend_from_slice(&c);
                    return Ok(());
                }
            }
        }
        put_varint(out, (payload.len() + 1) as u64);
        out.push(0);
        out.extend_from_slice(payload);
        Ok(())
    }
}

/// Reassembles frames from stream bytes. One per stream.
pub struct Deframer {
    buf: BytesMut,
    max: usize,
    #[cfg(feature = "zstd")]
    z: Option<zstd::bulk::Decompressor<'static>>,
}

impl Default for Deframer {
    fn default() -> Self {
        Self::new()
    }
}

impl Deframer {
    pub fn new() -> Self {
        Deframer {
            buf: BytesMut::new(),
            max: MAX_FRAME,
            #[cfg(feature = "zstd")]
            z: None,
        }
    }

    /// Refuse frames over `max` bytes, on the wire and once decompressed, from now on. The default
    /// (and the most this accepts) is [`MAX_FRAME`]; a transport lowers it for the first frame of a
    /// stream, which is a few bytes, so a peer cannot make it wait for or buffer more.
    pub fn set_max_frame(&mut self, max: usize) {
        self.max = max.min(MAX_FRAME);
    }

    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// `true` when no partial frame is buffered, so a stream that ends now ended on a frame boundary.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Next complete payload, decompressed, or `None` when more bytes are needed.
    // Not an Iterator: it is fallible and needs more input between calls.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<Bytes>, Error> {
        let Some((len, n)) = get_varint(&self.buf)? else {
            return Ok(None);
        };
        // Compare before narrowing: on a 32-bit target `len as usize` would wrap past the limit.
        if len > self.max as u64 {
            return Err(Error::TooLarge(usize::try_from(len).unwrap_or(usize::MAX)));
        }
        let len = len as usize;
        if len < 1 {
            return Err(Error::EmptyFrame);
        }
        if self.buf.len() < n + len {
            return Ok(None);
        }
        self.buf.advance(n);
        let flags = self.buf[0];
        if flags & !FLAG_ZSTD != 0 {
            return Err(Error::BadFlags(flags));
        }
        let body = self.buf.split_to(len);
        // A payload that lives on would pin the whole read buffer: copy the small ones out.
        // ponytail: bigger payloads stay slices of it; copy those too if a long-lived large frame
        // ever shows up.
        let payload = if len <= 4096 {
            Bytes::copy_from_slice(&body[1..])
        } else {
            body.freeze().slice(1..)
        };
        if flags & FLAG_ZSTD == 0 {
            return Ok(Some(payload));
        }
        #[cfg(feature = "zstd")]
        {
            // The size the frame declares is the capacity reserved for it, and the most it can
            // produce. A frame that declares none would make the decoder reserve `max` up front (and
            // keep it for as long as the payload lives).
            let size = match zstd::zstd_safe::get_frame_content_size(&payload) {
                Ok(Some(n)) if n <= self.max as u64 => n as usize,
                Ok(Some(n)) => {
                    return Err(Error::TooLarge(usize::try_from(n).unwrap_or(usize::MAX)))
                }
                _ => {
                    return Err(Error::Zstd(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "zstd frame without a content size",
                    )))
                }
            };
            let z = match self.z.as_mut() {
                Some(z) => z,
                None => self
                    .z
                    .insert(zstd::bulk::Decompressor::new().map_err(Error::Zstd)?),
            };
            Ok(Some(Bytes::from(
                z.decompress(&payload, size).map_err(Error::Zstd)?,
            )))
        }
        #[cfg(not(feature = "zstd"))]
        Err(Error::NoZstd)
    }

    pub fn next_msg<T: DeserializeOwned>(&mut self) -> Result<Option<T>, Error> {
        match self.next()? {
            Some(b) => Ok(Some(decode(&b)?)),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::{Cursors, C2S};
    use crate::term::{Frame, RowPatch, TermDown};
    use proptest::prelude::*;

    #[test]
    fn roundtrip_small_and_large() {
        let mut f = Framer::new(true);
        let mut d = Deframer::new();
        let mut wire = Vec::new();
        let hello = C2S::Hello {
            proto: 1,
            client_instance: 7,
            cursors: Cursors::default(),
        };
        f.frame(&hello, &mut wire).unwrap();
        let big = TermDown::Frame(Frame {
            row_patches: (0..200)
                .map(|i| RowPatch {
                    abs: i,
                    runs: vec![(0, "x".repeat(80))],
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        });
        f.frame(&big, &mut wire).unwrap();
        assert!(
            wire.len() < 2000,
            "big frame should compress, got {}",
            wire.len()
        );
        // feed byte by byte to exercise partial reads
        let mut got = Vec::new();
        for b in wire {
            d.push(&[b]);
            while let Some(p) = d.next().unwrap() {
                got.push(p);
            }
        }
        assert_eq!(got.len(), 2);
        assert_eq!(decode::<C2S>(&got[0]).unwrap(), hello);
        assert_eq!(decode::<TermDown>(&got[1]).unwrap(), big);
    }

    #[test]
    fn oversized_frames_are_refused_on_both_ends() {
        let mut out = Vec::new();
        assert!(matches!(
            Framer::new(false).frame_bytes(&vec![0u8; MAX_FRAME], &mut out),
            Err(Error::TooLarge(_))
        ));
        assert!(out.is_empty(), "nothing is written for a refused frame");
        // The largest frame the receiver accepts still goes through.
        Framer::new(false)
            .frame_bytes(&vec![0u8; MAX_FRAME - 1], &mut out)
            .unwrap();
        let mut d = Deframer::new();
        d.push(&out);
        assert_eq!(d.next().unwrap().unwrap().len(), MAX_FRAME - 1);
        // A length that only fits in 64 bits must not wrap into range.
        let mut wire = Vec::new();
        put_varint(&mut wire, (1u64 << 32) + 5);
        let mut d = Deframer::new();
        d.push(&wire);
        assert!(matches!(d.next(), Err(Error::TooLarge(_))));
    }

    /// A frame as a peer would put it on the wire: `len | flags | payload`.
    fn wire(flags: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        put_varint(&mut v, payload.len() as u64 + 1);
        v.push(flags);
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn a_frame_with_no_room_for_its_flags_byte_is_not_reported_as_a_bad_varint() {
        let mut d = Deframer::new();
        d.push(&[0]);
        assert!(matches!(d.next(), Err(Error::EmptyFrame)));
    }

    #[test]
    fn unknown_flag_bits_are_refused() {
        for flags in [2u8, 0x80, 0xff] {
            let mut d = Deframer::new();
            d.push(&wire(flags, b"hi"));
            assert!(d.next().is_err(), "flags {flags:#x} were ignored");
        }
    }

    #[test]
    fn a_varint_has_one_encoding_and_fits_64_bits() {
        assert!(get_varint(&[0x80, 0x00]).is_err(), "zero padding");
        assert!(
            get_varint(&[0x81, 0x80, 0x00]).is_err(),
            "a trailing zero group"
        );
        assert!(
            get_varint(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f]).is_err(),
            "bits past 64 were dropped"
        );
        assert_eq!(
            get_varint(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]).unwrap(),
            Some((u64::MAX, 10))
        );
        assert_eq!(
            get_varint(&[0x00]).unwrap(),
            Some((0, 1)),
            "zero itself is one byte"
        );
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn zstd_frames_without_a_content_size_are_refused() {
        // A streaming encoder does not know the length up front, so it records none; femme's own
        // (one-shot) encoder always does.
        let mut enc = zstd::stream::write::Encoder::new(Vec::new(), 1).unwrap();
        std::io::Write::write_all(&mut enc, &[7u8; 4000]).unwrap();
        let sizeless = enc.finish().unwrap();
        let mut d = Deframer::new();
        d.push(&wire(FLAG_ZSTD, &sizeless));
        assert!(matches!(d.next(), Err(Error::Zstd(_))));
        // Control: what `Framer` writes still decodes.
        let mut out = Vec::new();
        Framer::new(true)
            .frame_bytes(&[7u8; 4000], &mut out)
            .unwrap();
        let mut d = Deframer::new();
        d.push(&out);
        assert_eq!(d.next().unwrap().unwrap().len(), 4000);
    }

    #[test]
    fn a_small_payload_does_not_pin_the_read_buffer() {
        let mut d = Deframer::new();
        d.push(&wire(0, b"hello"));
        let start = d.buf.as_ptr() as usize;
        let block = start..start + d.buf.len();
        let p = d.next().unwrap().unwrap();
        assert_eq!(&p[..], b"hello");
        assert!(
            !block.contains(&(p.as_ptr() as usize)),
            "the payload is a slice of the read buffer"
        );
    }

    proptest! {
        #[test]
        fn varint_roundtrip(v in any::<u64>()) {
            let mut b = Vec::new();
            put_varint(&mut b, v);
            prop_assert_eq!(get_varint(&b).unwrap(), Some((v, b.len())));
        }

        #[test]
        fn frames_survive_any_chunking(payloads in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..600), 0..8), cut in 1usize..64) {
            let mut f = Framer::new(true);
            let mut wire = Vec::new();
            for p in &payloads { f.frame_bytes(p, &mut wire).unwrap(); }
            let mut d = Deframer::new();
            let mut got = Vec::new();
            for chunk in wire.chunks(cut) {
                d.push(chunk);
                while let Some(p) = d.next().unwrap() { got.push(p.to_vec()); }
            }
            prop_assert_eq!(got, payloads);
        }
    }
}
