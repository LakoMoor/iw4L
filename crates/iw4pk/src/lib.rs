//! IW4PK: a pack of the game files a browser session needs.
//!
//! One pack file is a *shard* (`common.iw4pk`, `mp_rust.iw4pk`, ...). Layout:
//!
//! ```text
//! "IW4PK\0\0\1"                         8 bytes
//! chunk data ...                        each chunk zstd or raw, independently decodable
//! index (zstd)                          entry table, see `Index::encode`
//! footer                                index_offset u64, index_len u64, "IW4PKEND"
//! ```
//!
//! The footer is fixed-size so a browser can fetch it with one suffix Range
//! request, then the index, then only the chunks a map load touches.
//! Entries are cut into chunks of at most [`CHUNK`] raw bytes so a 170 MB
//! fastfile can be streamed and a texture costs one small request.

use std::collections::HashMap;
use std::io::Read;

pub const MAGIC: &[u8; 8] = b"IW4PK\0\0\x01";
pub const END_MAGIC: &[u8; 8] = b"IW4PKEND";
pub const FOOTER_LEN: usize = 24;
/// Maximum raw bytes per chunk.
pub const CHUNK: usize = 4 << 20;

#[derive(Debug)]
pub enum Error {
    Truncated,
    BadMagic,
    BadUtf8,
    Codec(String),
    Missing(String),
    Io(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated => write!(f, "iw4pk: truncated"),
            Self::BadMagic => write!(f, "iw4pk: bad magic"),
            Self::BadUtf8 => write!(f, "iw4pk: bad utf-8 in index"),
            Self::Codec(e) => write!(f, "iw4pk: codec: {e}"),
            Self::Missing(p) => write!(f, "iw4pk: no entry {p}"),
            Self::Io(e) => write!(f, "iw4pk: io: {e}"),
        }
    }
}

impl std::error::Error for Error {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    Raw = 0,
    Zstd = 1,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    /// Absolute offset of the stored bytes in the shard.
    pub offset: u64,
    pub stored_len: u32,
    pub raw_len: u32,
    pub codec: Codec,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Virtual path, `/`-separated, relative to the game root, e.g.
    /// `main/iw_00.iwd/images/foo.iwi` or `zone/russian/mp_rust.ff`.
    pub path: String,
    pub raw_len: u64,
    pub chunks: Vec<Chunk>,
}

#[derive(Clone, Debug, Default)]
pub struct Index {
    pub entries: Vec<Entry>,
    by_path: HashMap<String, usize>,
}

struct Cursor<'a>(&'a [u8]);

impl Cursor<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], Error> {
        if self.0.len() < n {
            return Err(Error::Truncated);
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, Error> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, Error> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
}

impl Index {
    pub fn new(entries: Vec<Entry>) -> Self {
        let by_path = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.path.clone(), i))
            .collect();
        Self { entries, by_path }
    }

    pub fn get(&self, path: &str) -> Option<&Entry> {
        self.by_path.get(path).map(|&i| &self.entries[i])
    }

    /// Plain (uncompressed) index bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        for e in &self.entries {
            out.extend_from_slice(&(e.path.len() as u16).to_le_bytes());
            out.extend_from_slice(e.path.as_bytes());
            out.extend_from_slice(&e.raw_len.to_le_bytes());
            out.extend_from_slice(&(e.chunks.len() as u32).to_le_bytes());
            for c in &e.chunks {
                out.extend_from_slice(&c.offset.to_le_bytes());
                out.extend_from_slice(&c.stored_len.to_le_bytes());
                out.extend_from_slice(&c.raw_len.to_le_bytes());
                out.push(c.codec as u8);
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut c = Cursor(bytes);
        let n = c.u32()? as usize;
        let mut entries = Vec::with_capacity(n.min(1 << 20));
        for _ in 0..n {
            let plen = c.u16()? as usize;
            let path = String::from_utf8(c.take(plen)?.to_vec()).map_err(|_| Error::BadUtf8)?;
            let raw_len = c.u64()?;
            let nc = c.u32()? as usize;
            let mut chunks = Vec::with_capacity(nc.min(1 << 16));
            for _ in 0..nc {
                let offset = c.u64()?;
                let stored_len = c.u32()?;
                let raw_len = c.u32()?;
                let codec = match c.u8()? {
                    0 => Codec::Raw,
                    1 => Codec::Zstd,
                    other => return Err(Error::Codec(format!("unknown codec {other}"))),
                };
                chunks.push(Chunk {
                    offset,
                    stored_len,
                    raw_len,
                    codec,
                });
            }
            entries.push(Entry {
                path,
                raw_len,
                chunks,
            });
        }
        Ok(Self::new(entries))
    }
}

/// Footer: where the compressed index sits.
#[derive(Clone, Copy, Debug)]
pub struct Footer {
    pub index_offset: u64,
    pub index_len: u64,
}

impl Footer {
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() < FOOTER_LEN {
            return Err(Error::Truncated);
        }
        let b = &bytes[bytes.len() - FOOTER_LEN..];
        if &b[16..24] != END_MAGIC {
            return Err(Error::BadMagic);
        }
        Ok(Self {
            index_offset: u64::from_le_bytes(b[0..8].try_into().unwrap()),
            index_len: u64::from_le_bytes(b[8..16].try_into().unwrap()),
        })
    }
}

pub fn decompress(codec: Codec, stored: &[u8], raw_len: usize) -> Result<Vec<u8>, Error> {
    match codec {
        Codec::Raw => Ok(stored.to_vec()),
        Codec::Zstd => {
            let mut dec = ruzstd::decoding::StreamingDecoder::new(stored)
                .map_err(|e| Error::Codec(e.to_string()))?;
            let mut out = Vec::with_capacity(raw_len);
            dec.read_to_end(&mut out)
                .map_err(|e| Error::Codec(e.to_string()))?;
            if out.len() != raw_len {
                return Err(Error::Codec(format!(
                    "chunk length {} != {raw_len}",
                    out.len()
                )));
            }
            Ok(out)
        }
    }
}

/// Random access to a shard: a file, an HTTP Range fetcher, an OPFS handle.
pub trait ReadAt {
    fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>, Error>;
    fn len(&self) -> Result<u64, Error>;
}

impl ReadAt for Vec<u8> {
    fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>, Error> {
        let start = usize::try_from(offset).map_err(|_| Error::Truncated)?;
        self.get(start..start + len)
            .map(<[u8]>::to_vec)
            .ok_or(Error::Truncated)
    }
    fn len(&self) -> Result<u64, Error> {
        Ok(<[u8]>::len(self) as u64)
    }
}

impl ReadAt for std::sync::Arc<Vec<u8>> {
    fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>, Error> {
        self.as_ref().read_at(offset, len)
    }
    fn len(&self) -> Result<u64, Error> {
        Ok(self.as_slice().len() as u64)
    }
}

pub struct Pack<R: ReadAt> {
    pub index: Index,
    source: R,
}

impl<R: ReadAt> Pack<R> {
    pub fn open(source: R) -> Result<Self, Error> {
        let total = source.len()?;
        if total < (MAGIC.len() + FOOTER_LEN) as u64 {
            return Err(Error::Truncated);
        }
        if source.read_at(0, MAGIC.len())? != MAGIC {
            return Err(Error::BadMagic);
        }
        let footer = Footer::parse(&source.read_at(total - FOOTER_LEN as u64, FOOTER_LEN)?)?;
        let packed = source.read_at(footer.index_offset, footer.index_len as usize)?;
        let mut plain = Vec::new();
        ruzstd::decoding::StreamingDecoder::new(packed.as_slice())
            .map_err(|e| Error::Codec(e.to_string()))?
            .read_to_end(&mut plain)
            .map_err(|e| Error::Codec(e.to_string()))?;
        Ok(Self {
            index: Index::decode(&plain)?,
            source,
        })
    }

    pub fn read_chunk(&self, entry: &Entry, chunk: usize) -> Result<Vec<u8>, Error> {
        let c = &entry.chunks[chunk];
        let stored = self.source.read_at(c.offset, c.stored_len as usize)?;
        decompress(c.codec, &stored, c.raw_len as usize)
    }

    /// Whole entry.
    pub fn read(&self, path: &str) -> Result<Vec<u8>, Error> {
        let entry = self
            .index
            .get(path)
            .ok_or_else(|| Error::Missing(path.to_owned()))?;
        let mut out = Vec::with_capacity(entry.raw_len as usize);
        for i in 0..entry.chunks.len() {
            out.extend_from_slice(&self.read_chunk(entry, i)?);
        }
        Ok(out)
    }

    /// Only the first `limit` bytes (headers): decodes just the chunks needed.
    pub fn read_prefix(&self, path: &str, limit: usize) -> Result<Vec<u8>, Error> {
        let entry = self
            .index
            .get(path)
            .ok_or_else(|| Error::Missing(path.to_owned()))?;
        let mut out = Vec::new();
        for i in 0..entry.chunks.len() {
            if out.len() >= limit {
                break;
            }
            out.extend_from_slice(&self.read_chunk(entry, i)?);
        }
        out.truncate(limit);
        Ok(out)
    }
}

#[cfg(feature = "write")]
pub mod write {
    use super::*;
    use std::io::Write;

    pub struct Writer<W: Write> {
        out: W,
        pos: u64,
        entries: Vec<Entry>,
        level: i32,
    }

    impl<W: Write> Writer<W> {
        pub fn new(mut out: W, level: i32) -> Result<Self, Error> {
            out.write_all(MAGIC).map_err(|e| Error::Io(e.to_string()))?;
            Ok(Self {
                out,
                pos: MAGIC.len() as u64,
                entries: Vec::new(),
                level,
            })
        }

        /// Bytes written so far (compressed size of the shard).
        pub fn position(&self) -> u64 {
            self.pos
        }

        /// `store_raw`: already-compressed payloads (mp3, opus, deflate) skip zstd.
        pub fn add(&mut self, path: &str, data: &[u8], store_raw: bool) -> Result<(), Error> {
            let mut chunks = Vec::new();
            for part in data.chunks(CHUNK) {
                let (codec, stored) = if store_raw {
                    (Codec::Raw, part.to_vec())
                } else {
                    let z = zstd::bulk::compress(part, self.level)
                        .map_err(|e| Error::Codec(e.to_string()))?;
                    if z.len() < part.len() {
                        (Codec::Zstd, z)
                    } else {
                        (Codec::Raw, part.to_vec())
                    }
                };
                self.out
                    .write_all(&stored)
                    .map_err(|e| Error::Io(e.to_string()))?;
                chunks.push(Chunk {
                    offset: self.pos,
                    stored_len: stored.len() as u32,
                    raw_len: part.len() as u32,
                    codec,
                });
                self.pos += stored.len() as u64;
            }
            self.entries.push(Entry {
                path: path.to_owned(),
                raw_len: data.len() as u64,
                chunks,
            });
            Ok(())
        }

        pub fn finish(mut self) -> Result<(W, Index), Error> {
            let index = Index::new(std::mem::take(&mut self.entries));
            let packed = zstd::bulk::compress(&index.encode(), 19)
                .map_err(|e| Error::Codec(e.to_string()))?;
            let io = |e: std::io::Error| Error::Io(e.to_string());
            self.out.write_all(&packed).map_err(io)?;
            let mut footer = Vec::with_capacity(FOOTER_LEN);
            footer.extend_from_slice(&self.pos.to_le_bytes());
            footer.extend_from_slice(&(packed.len() as u64).to_le_bytes());
            footer.extend_from_slice(END_MAGIC);
            self.out.write_all(&footer).map_err(io)?;
            self.out.flush().map_err(io)?;
            Ok((self.out, index))
        }
    }
}

#[cfg(all(test, feature = "write"))]
mod tests {
    use super::*;

    #[test]
    fn round_trip_multi_chunk() {
        let big: Vec<u8> = (0..CHUNK * 2 + 123).map(|i| (i / 7 % 251) as u8).collect();
        let small = b"hello iw4pk".to_vec();
        let mut w = write::Writer::new(Vec::new(), 3).unwrap();
        w.add("zone/x.ff", &big, false).unwrap();
        w.add("main/a.iwd/s.mp3", &small, true).unwrap();
        let (bytes, _) = w.finish().unwrap();
        let pack = Pack::open(bytes).unwrap();
        assert_eq!(pack.read("zone/x.ff").unwrap(), big);
        assert_eq!(pack.read("main/a.iwd/s.mp3").unwrap(), small);
        assert_eq!(pack.read_prefix("zone/x.ff", 10).unwrap(), &big[..10]);
        assert!(pack.read("nope").is_err());
    }
}
