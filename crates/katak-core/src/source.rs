//! File access: a memory map where that is safe, buffered positional reads
//! otherwise.
//!
//! On Windows a page fault on a mapping whose backing store disappears
//! (network share dropped, USB stick pulled) is an access violation, while a
//! failed `read` is an error we can report. Network and removable drives are
//! therefore read through a small block cache instead of being mapped.

use std::borrow::Cow;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use memmap2::Mmap;
use parking_lot::Mutex;

/// How to access the file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AccessMode {
    /// Map local fixed drives, read everything else in chunks.
    #[default]
    Auto,
    /// Always try to map (falls back to buffered reads if mapping fails).
    Mapped,
    /// Never map.
    Buffered,
}

const BLOCK: usize = 1 << 20;
const CACHE_BLOCKS: usize = 32;

/// An open, immutable view of a file's bytes as they were at open time.
pub struct Source {
    path: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
    file: File,
    map: Option<Mmap>,
    buffered_reason: Option<String>,
    cache: Mutex<BlockCache>,
}

#[derive(Default)]
struct BlockCache {
    blocks: Vec<(u64, Arc<[u8]>, u64)>,
    tick: u64,
}

impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Source")
            .field("path", &self.path)
            .field("len", &self.len)
            .field("mapped", &self.map.is_some())
            .finish()
    }
}

impl Source {
    /// Opens `path` for reading.
    pub fn open(path: &Path, mode: AccessMode) -> io::Result<Source> {
        let file = File::open(path)?;
        let meta = file.metadata()?;
        if meta.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "this is a folder, not a file",
            ));
        }
        let len = meta.len();
        let mut buffered_reason = None;
        let map = if len == 0 {
            None
        } else if mode == AccessMode::Buffered {
            buffered_reason = Some("buffered access requested".to_string());
            None
        } else if mode == AccessMode::Auto && !mapping_is_safe(path) {
            buffered_reason = Some("network or removable drive".to_string());
            None
        } else {
            // SAFETY: the map is read-only. Another process may still change
            // the file underneath us; on Windows it cannot truncate a mapped
            // file, and appended bytes beyond `len` are never looked at.
            match unsafe { Mmap::map(&file) } {
                Ok(map) => Some(map),
                Err(e) => {
                    buffered_reason = Some(format!("memory mapping failed: {e}"));
                    None
                }
            }
        };
        Ok(Source {
            path: path.to_path_buf(),
            len,
            modified: meta.modified().ok(),
            file,
            map,
            buffered_reason,
            cache: Mutex::new(BlockCache::default()),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// File length at open time.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Modification time at open time.
    pub fn modified(&self) -> Option<SystemTime> {
        self.modified
    }

    pub fn is_mapped(&self) -> bool {
        self.map.is_some()
    }

    /// Why the file is read in chunks instead of mapped.
    pub fn buffered_reason(&self) -> Option<&str> {
        self.buffered_reason.as_deref()
    }

    /// The whole file, if it is memory-mapped.
    pub fn mapped(&self) -> Option<&[u8]> {
        self.map.as_deref()
    }

    /// Reads into `buf` from `off` until `buf` is full or the file ends.
    /// Returns the number of bytes read. Bypasses the block cache.
    pub fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<usize> {
        if off >= self.len {
            return Ok(0);
        }
        let n = buf.len().min((self.len - off) as usize);
        if let Some(map) = &self.map {
            buf[..n].copy_from_slice(&map[off as usize..off as usize + n]);
            return Ok(n);
        }
        let got = read_full_at(&self.file, &mut buf[..n], off)?;
        if got < n {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the file became shorter while it was open",
            ));
        }
        Ok(got)
    }

    /// Bytes `[off, off + len)`, clipped to the end of the file. Borrowed from
    /// the map, or assembled from cached blocks.
    pub fn bytes(&self, off: u64, len: usize) -> io::Result<Cow<'_, [u8]>> {
        let off = off.min(self.len);
        let end = off.saturating_add(len as u64).min(self.len);
        if let Some(map) = &self.map {
            return Ok(Cow::Borrowed(&map[off as usize..end as usize]));
        }
        let mut out = Vec::with_capacity((end - off) as usize);
        let mut pos = off;
        while pos < end {
            let bi = pos / BLOCK as u64;
            let block = self.block(bi)?;
            let in_block = (pos - bi * BLOCK as u64) as usize;
            let take = ((end - pos) as usize).min(block.len() - in_block);
            if take == 0 {
                break;
            }
            out.extend_from_slice(&block[in_block..in_block + take]);
            pos += take as u64;
        }
        Ok(Cow::Owned(out))
    }

    fn block(&self, bi: u64) -> io::Result<Arc<[u8]>> {
        {
            let mut c = self.cache.lock();
            c.tick += 1;
            let tick = c.tick;
            if let Some(e) = c.blocks.iter_mut().find(|e| e.0 == bi) {
                e.2 = tick;
                return Ok(e.1.clone());
            }
        }
        let start = bi * BLOCK as u64;
        let n = (BLOCK as u64).min(self.len - start) as usize;
        let mut data = vec![0u8; n];
        self.read_at(start, &mut data)?;
        let data: Arc<[u8]> = data.into();
        let mut c = self.cache.lock();
        let tick = c.tick;
        if c.blocks.len() >= CACHE_BLOCKS
            && let Some(oldest) = c
                .blocks
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.2)
                .map(|(i, _)| i)
        {
            c.blocks.swap_remove(oldest);
        }
        c.blocks.push((bi, data.clone(), tick));
        Ok(data)
    }
}

fn read_full_at(file: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match pread(file, &mut buf[n..], off + n as u64) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

#[cfg(unix)]
fn pread(file: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.read_at(buf, off)
}

#[cfg(windows)]
fn pread(file: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_read(buf, off)
}

#[cfg(windows)]
fn mapping_is_safe(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetVolumePathNameW};
    const DRIVE_FIXED: u32 = 3;
    const DRIVE_RAMDISK: u32 = 6;
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let wide: Vec<u16> = abs.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut root = vec![0u16; wide.len() + 8];
    // SAFETY: both buffers are valid, NUL-terminated / sized as declared.
    let ok = unsafe { GetVolumePathNameW(wide.as_ptr(), root.as_mut_ptr(), root.len() as u32) };
    if ok == 0 {
        return false;
    }
    // SAFETY: `root` is NUL-terminated by GetVolumePathNameW.
    let kind = unsafe { GetDriveTypeW(root.as_ptr()) };
    kind == DRIVE_FIXED || kind == DRIVE_RAMDISK
}

#[cfg(not(windows))]
fn mapping_is_safe(_path: &Path) -> bool {
    true
}

/// A window of the source handed to a state machine. Bytes before the
/// requested position are history (look-behind).
#[derive(Clone, Copy, Debug)]
pub struct Window<'a> {
    pub buf: &'a [u8],
    /// Absolute offset of `buf[0]`.
    pub base: u64,
    /// The input ends at the end of `buf`.
    pub eof: bool,
}

impl Window<'_> {
    pub fn end(&self) -> u64 {
        self.base + self.buf.len() as u64
    }
}

/// Sequential reader in `fill`/`consume` style: each call returns a window
/// starting a little before `at` (history) and extending at least `chunk`
/// bytes past it (unless the end is reached). Zero-copy over a map; buffered
/// sources keep the unconsumed tail and refill a reusable buffer.
pub struct ChunkReader<'a> {
    src: &'a Source,
    end: u64,
    chunk: usize,
    buf: Vec<u8>,
    buf_base: u64,
    cached: bool,
}

impl<'a> ChunkReader<'a> {
    /// Look-behind kept before the requested position.
    pub const HISTORY: usize = 4096;

    /// Reads up to `end` (exclusive, clipped to the file length).
    pub fn new(src: &'a Source, end: u64, chunk: usize) -> Self {
        ChunkReader {
            src,
            end: end.min(src.len()),
            chunk: chunk.max(64),
            buf: Vec::new(),
            buf_base: 0,
            cached: false,
        }
    }

    /// Like [`ChunkReader::new`], but buffered sources read through the
    /// shared block cache (for small, repeated UI-side reads).
    pub fn cached(src: &'a Source, end: u64, chunk: usize) -> Self {
        ChunkReader {
            cached: true,
            ..Self::new(src, end, chunk)
        }
    }

    pub fn end(&self) -> u64 {
        self.end
    }

    /// A window containing `at` (and up to `HISTORY` bytes before it).
    pub fn window(&mut self, at: u64) -> io::Result<Window<'_>> {
        let at = at.min(self.end);
        let lo = at.saturating_sub(Self::HISTORY as u64);
        let hi = at.saturating_add(self.chunk as u64).min(self.end);
        if let Some(map) = self.src.mapped() {
            return Ok(Window {
                buf: &map[lo as usize..hi as usize],
                base: lo,
                eof: hi == self.end,
            });
        }
        let have_hi = self.buf_base + self.buf.len() as u64;
        let enough = lo >= self.buf_base && at <= have_hi && (have_hi >= hi || have_hi == self.end);
        if !enough {
            if lo >= self.buf_base && lo <= have_hi {
                self.buf.drain(..(lo - self.buf_base) as usize);
            } else {
                self.buf.clear();
            }
            self.buf_base = lo;
            let have = self.buf_base + self.buf.len() as u64;
            let want = (hi - have) as usize;
            if self.cached {
                let bytes = self.src.bytes(have, want)?;
                self.buf.extend_from_slice(&bytes);
            } else {
                let old = self.buf.len();
                self.buf.resize(old + want, 0);
                let got = self.src.read_at(have, &mut self.buf[old..])?;
                self.buf.truncate(old + got);
            }
        }
        let buf_end = self.buf_base + self.buf.len() as u64;
        Ok(Window {
            buf: &self.buf[(lo - self.buf_base) as usize..],
            base: lo,
            eof: buf_end == self.end,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_with(bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("data.csv");
        std::fs::write(&p, bytes).unwrap();
        (dir, p)
    }

    #[test]
    fn mapped_and_buffered_read_the_same() {
        let data: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        let (_d, p) = temp_with(&data);
        let m = Source::open(&p, AccessMode::Mapped).unwrap();
        let b = Source::open(&p, AccessMode::Buffered).unwrap();
        assert!(m.is_mapped());
        assert!(!b.is_mapped());
        for (off, len) in [(0, 10), (1_048_570, 20), (2_999_990, 100), (5_000_000, 5)] {
            assert_eq!(m.bytes(off, len).unwrap(), b.bytes(off, len).unwrap());
        }
    }

    #[test]
    fn chunk_reader_windows_are_consistent() {
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 253) as u8).collect();
        let (_d, p) = temp_with(&data);
        for mode in [AccessMode::Mapped, AccessMode::Buffered] {
            let src = Source::open(&p, mode).unwrap();
            let mut rd = ChunkReader::new(&src, src.len(), 10_000);
            let mut at = 0u64;
            let mut seen = 0u64;
            loop {
                let w = rd.window(at).unwrap();
                assert!(w.base <= at && at <= w.end());
                assert_eq!(w.buf, &data[w.base as usize..w.end() as usize]);
                assert!(at - w.base <= ChunkReader::HISTORY as u64);
                if w.eof && at == w.end() {
                    break;
                }
                // Consume a ragged amount.
                at = (at + 7_777).min(w.end());
                seen += 1;
            }
            assert!(seen > 20);
        }
    }

    #[test]
    fn empty_file() {
        let (_d, p) = temp_with(b"");
        let src = Source::open(&p, AccessMode::Auto).unwrap();
        assert!(src.is_empty());
        assert_eq!(src.bytes(0, 10).unwrap().len(), 0);
        let mut rd = ChunkReader::new(&src, 0, 1024);
        let w = rd.window(0).unwrap();
        assert!(w.eof && w.buf.is_empty());
    }
}
