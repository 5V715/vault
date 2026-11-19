// SPDX-License-Identifier: MIT
//! Chunking of application messages into Noise-sized frames.
//!
//! Each frame plaintext is `postcard{msg_id:u64, index:u32, last:bool}` followed
//! by the payload; the whole plaintext is at most [`MAX_PLAINTEXT`] bytes.

use crate::ProtoError;
extern crate alloc;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

/// Maximum plaintext per Noise transport message (60 KiB).
pub const MAX_PLAINTEXT: usize = 60 * 1024;
/// Upper bound of the postcard header: u64 (10) + u32 (5) + bool (1).
pub const MAX_HEADER: usize = 16;
/// Payload bytes per chunk.
pub const MAX_PAYLOAD: usize = MAX_PLAINTEXT - MAX_HEADER;
/// Maximum reassembled application message (32 MiB).
pub const MAX_MESSAGE: usize = 32 * 1024 * 1024;
/// Maximum chunks per message.
pub const MAX_CHUNKS: u32 = 600;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Header {
    msg_id: u64,
    index: u32,
    last: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub msg_id: u64,
    pub index: u32,
    pub last: bool,
    pub payload: Vec<u8>,
}

impl Chunk {
    /// Header followed by payload: the plaintext of one Noise transport message.
    pub fn encode(&self) -> Vec<u8> {
        let h = Header {
            msg_id: self.msg_id,
            index: self.index,
            last: self.last,
        };
        let mut out = postcard::to_allocvec(&h).expect("postcard encoding to Vec cannot fail");
        out.extend_from_slice(&self.payload);
        out
    }

    pub fn decode(frame: &[u8]) -> Result<Self, ProtoError> {
        if frame.len() > MAX_PLAINTEXT {
            return Err(ProtoError::TooLarge);
        }
        let (h, rest): (Header, &[u8]) =
            postcard::take_from_bytes(frame).map_err(|_| ProtoError::Malformed)?;
        Ok(Chunk {
            msg_id: h.msg_id,
            index: h.index,
            last: h.last,
            payload: rest.to_vec(),
        })
    }
}

/// Splits messages into chunks, numbering them with a per-direction counter.
#[derive(Debug, Default)]
pub struct Chunker {
    next_id: u64,
}

impl Chunker {
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty message yields one empty last chunk.
    pub fn split(&mut self, msg: &[u8]) -> Result<Vec<Chunk>, ProtoError> {
        if msg.len() > MAX_MESSAGE {
            return Err(ProtoError::TooLarge);
        }
        let msg_id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        if msg.is_empty() {
            return Ok(alloc::vec![Chunk {
                msg_id,
                index: 0,
                last: true,
                payload: Vec::new()
            }]);
        }
        let n = msg.len().div_ceil(MAX_PAYLOAD);
        Ok(msg
            .chunks(MAX_PAYLOAD)
            .enumerate()
            .map(|(i, p)| Chunk {
                msg_id,
                index: i as u32,
                last: i + 1 == n,
                payload: p.to_vec(),
            })
            .collect())
    }
}

/// Reassembles one message at a time; `msg_id` must be strictly increasing per
/// direction. Any violation is an error and resets the in-flight message
/// (callers should then close the session).
#[derive(Debug, Default)]
pub struct Reassembler {
    current: Option<(u64, u32, Vec<u8>)>, // msg_id, next index, buffer
    last_id: Option<u64>,                 // id of the most recently started message
}

impl Reassembler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, c: Chunk) -> Result<Option<Vec<u8>>, ProtoError> {
        let r = self.push_inner(c);
        if r.is_err() {
            self.current = None;
        }
        r
    }

    fn push_inner(&mut self, c: Chunk) -> Result<Option<Vec<u8>>, ProtoError> {
        if c.payload.len() > MAX_PAYLOAD {
            return Err(ProtoError::TooLarge);
        }
        let (id, next, mut buf) = match self.current.take() {
            None => {
                if c.index != 0 || self.last_id.is_some_and(|l| c.msg_id <= l) {
                    return Err(ProtoError::OutOfSequence);
                }
                self.last_id = Some(c.msg_id);
                (c.msg_id, 0, Vec::new())
            }
            Some((id, next, buf)) => {
                if c.msg_id != id {
                    return Err(ProtoError::Interleaved);
                }
                if c.index != next {
                    return Err(ProtoError::OutOfSequence);
                }
                (id, next, buf)
            }
        };
        if next >= MAX_CHUNKS {
            return Err(ProtoError::TooManyChunks);
        }
        if buf.len() + c.payload.len() > MAX_MESSAGE {
            return Err(ProtoError::TooLarge);
        }
        buf.extend_from_slice(&c.payload);
        if c.last {
            Ok(Some(buf))
        } else {
            self.current = Some((id, next + 1, buf));
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i % 251) as u8).collect()
    }

    fn feed(r: &mut Reassembler, chunks: Vec<Chunk>) -> Option<Vec<u8>> {
        let mut out = None;
        for c in chunks {
            // through the wire encoding
            let wire = c.encode();
            assert!(wire.len() <= MAX_PLAINTEXT);
            out = r.push(Chunk::decode(&wire).unwrap()).unwrap();
        }
        out
    }

    #[test]
    fn chunking_roundtrip_and_cap() {
        let mut ck = Chunker::new();
        let mut r = Reassembler::new();
        for n in [
            0,
            1,
            MAX_PAYLOAD - 1,
            MAX_PAYLOAD,
            MAX_PAYLOAD + 1,
            1_000_000,
        ] {
            let m = data(n);
            let chunks = ck.split(&m).unwrap();
            assert_eq!(chunks.len(), n.div_ceil(MAX_PAYLOAD).max(1));
            assert!(chunks.last().unwrap().last);
            assert_eq!(feed(&mut r, chunks).unwrap(), m);
        }
        // exactly the cap works and stays under the chunk limit
        let big = data(MAX_MESSAGE);
        let chunks = ck.split(&big).unwrap();
        assert!(chunks.len() as u32 <= MAX_CHUNKS);
        assert_eq!(feed(&mut r, chunks).unwrap().len(), MAX_MESSAGE);
        // one byte over is refused by the sender...
        assert_eq!(ck.split(&data(MAX_MESSAGE + 1)), Err(ProtoError::TooLarge));
        // ...and by a reassembler fed oversized input directly
        let mut r = Reassembler::new();
        let mut res = Ok(None);
        for i in 0..=(MAX_MESSAGE / MAX_PAYLOAD) as u32 + 1 {
            res = r.push(Chunk {
                msg_id: 1,
                index: i,
                last: false,
                payload: data(MAX_PAYLOAD),
            });
            if res.is_err() {
                break;
            }
        }
        assert_eq!(res, Err(ProtoError::TooLarge));
        // oversized frame rejected on decode
        assert_eq!(
            Chunk::decode(&data(MAX_PLAINTEXT + 1)),
            Err(ProtoError::TooLarge)
        );
    }

    #[test]
    fn reassembler_rejects_interleaved_or_gapped() {
        let mut ck = Chunker::new();
        let a = ck.split(&data(MAX_PAYLOAD * 3)).unwrap();
        let b = ck.split(&data(MAX_PAYLOAD * 2)).unwrap();

        let mut r = Reassembler::new();
        assert_eq!(r.push(a[0].clone()), Ok(None));
        assert_eq!(r.push(b[0].clone()), Err(ProtoError::Interleaved));

        let mut r = Reassembler::new();
        assert_eq!(r.push(a[0].clone()), Ok(None));
        assert_eq!(r.push(a[2].clone()), Err(ProtoError::OutOfSequence));

        let mut r = Reassembler::new();
        assert_eq!(r.push(a[1].clone()), Err(ProtoError::OutOfSequence));

        let mut r = Reassembler::new();
        assert_eq!(r.push(a[0].clone()), Ok(None));
        assert_eq!(r.push(a[0].clone()), Err(ProtoError::OutOfSequence));

        // chunk-count cap
        let mut r = Reassembler::new();
        let mut res = Ok(None);
        for i in 0..=MAX_CHUNKS {
            res = r.push(Chunk {
                msg_id: 9,
                index: i,
                last: false,
                payload: Vec::new(),
            });
            if res.is_err() {
                break;
            }
        }
        assert_eq!(res, Err(ProtoError::TooManyChunks));
    }

    #[test]
    fn reassembler_requires_increasing_msg_id() {
        let mut ck = Chunker::new();
        let a = ck.split(b"one").unwrap();
        let b = ck.split(b"two").unwrap();
        let mut r = Reassembler::new();
        assert_eq!(r.push(b[0].clone()), Ok(Some(b"two".to_vec())));
        // replay of the same message and of an older one
        assert!(r.push(b[0].clone()).is_err());
        assert!(r.push(a[0].clone()).is_err());
        // a newer id is accepted again
        let c = ck.split(b"three").unwrap();
        assert_eq!(r.push(c[0].clone()), Ok(Some(b"three".to_vec())));
    }
}
