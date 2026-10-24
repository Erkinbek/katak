//! File access: memory-mapped views where that is safe, buffered
//! positional reads otherwise.
//!
//! On Windows a page fault on a mapping whose backing store disappears
//! (network share dropped, USB stick pulled) is an access violation, while a
//! failed `read` is an error we can report. Network and removable drives are
//! therefore read through a small block cache instead of being mapped.
//!
//! The file is never mapped as a whole: Windows charges the page-table
//! entries of a mapped view to the process (about 2 MB per GB), so a single
//! 50 GB view costs 100 MB of private memory before a byte is read. Random
//! access maps 16 MB views on demand and keeps a few of them; bulk scans
//! read with `pread`/`ReadFile` into a reusable buffer, which is also faster
//! than faulting pages in.
//!
//! A network share can take seconds to answer. On a thread that asked for
//! non-blocking reads (the UI thread), a block of a buffered source that is
//! not cached yet is fetched by a background thread and the read fails
//! with [`io::ErrorKind::WouldBlock`]; the document's notify callback
//! repaints when it arrives.

use std::cell::Cell;
use std::collections::HashSet;
use std::fs::File;
use std::io;
use std::ops::{Deref, Range};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Sender, channel};
use std::time::{Duration, Instant, SystemTime};

use memmap2::{Mmap, MmapOptions};
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

thread_local! {
    static NONBLOCKING: Cell<bool> = const { Cell::new(false) };
}

/// Makes cache misses of buffered sources on this thread non-blocking
/// (see the module docs). For the UI thread.
pub fn set_nonblocking_reads(on: bool) {
    NONBLOCKING.with(|c| c.set(on));
}

/// Runs `f` with blocking reads on this thread (opening a document needs
/// its first bytes now).
pub fn with_blocking_reads<T>(f: impl FnOnce() -> T) -> T {
    let before = NONBLOCKING.with(|c| c.replace(false));
    let out = f();
    NONBLOCKING.with(|c| c.set(before));
    out
}

/// The error only says "not loaded yet": try again after the next notify.
pub fn is_pending(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::WouldBlock
}

fn pending() -> io::Error {
    io::Error::new(io::ErrorKind::WouldBlock, "still loading from the drive")
}

/// After a failed background read, misses report that error for a while
/// instead of queueing the same read again and again.
const FAILURE_HOLD: Duration = Duration::from_secs(3);

/// Background reader of a buffered source's blocks.
struct Fetcher {
    tx: Sender<u64>,
    queued: Arc<Mutex<HashSet<u64>>>,
    failed: Arc<Mutex<Option<(String, Instant)>>>,
}

const BLOCK: usize = 1 << 20;
const CACHE_BLOCKS: usize = 32;
/// Size of a cached mapped view. Views overlap by `VIEW_OVERLAP`, so any
/// request of up to that many bytes fits in one view.
const VIEW: u64 = 16 << 20;
const VIEW_OVERLAP: u64 = 8 << 20;
const CACHE_VIEWS: usize = 16;

/// Bytes borrowed from a mapped view (kept alive by the handle) or owned.
pub enum Bytes {
    Mapped(Arc<Mmap>, Range<usize>),
    Owned(Vec<u8>),
}

impl Deref for Bytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            Bytes::Mapped(m, r) => &m[r.clone()],
            Bytes::Owned(v) => v,
        }
    }
}

impl std::fmt::Debug for Bytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Bytes({} bytes)", self.len())
    }
}

/// A tiny least-recently-used cache keyed by block/view number.
struct Lru<T> {
    items: Vec<(u64, T, u64)>,
    tick: u64,
    cap: usize,
}

impl<T: Clone> Lru<T> {
    fn new(cap: usize) -> Self {
        Lru {
            items: Vec::new(),
            tick: 0,
            cap,
        }
    }

    fn get(&mut self, key: u64) -> Option<T> {
        self.tick += 1;
        let tick = self.tick;
        self.items.iter_mut().find(|e| e.0 == key).map(|e| {
            e.2 = tick;
            e.1.clone()
        })
    }

    fn put(&mut self, key: u64, value: T) {
        if self.items.len() >= self.cap
            && let Some(oldest) = self
                .items
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.2)
                .map(|(i, _)| i)
        {
            self.items.swap_remove(oldest);
        }
        self.items.push((key, value, self.tick));
    }
}

/// Size and modification time of a file: a change means another program
/// wrote it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileStamp {
    pub len: u64,
    pub modified: Option<SystemTime>,
}

impl FileStamp {
    /// The current stamp of `path` (may block on a network drive).
    pub fn of(path: &Path) -> io::Result<FileStamp> {
        let m = std::fs::metadata(path)?;
        Ok(FileStamp {
            len: m.len(),
            modified: m.modified().ok(),
        })
    }
}

/// An open, immutable view of a file's bytes as they were at open time.
pub struct Source {
    path: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
    file: File,
    mappable: bool,
    buffered_reason: Option<String>,
    blocks: Arc<Mutex<Lru<Arc<[u8]>>>>,
    views: Mutex<Lru<Arc<Mmap>>>,
    fetcher: Mutex<Option<Fetcher>>,
}

impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Source")
            .field("path", &self.path)
            .field("len", &self.len)
            .field("mapped", &self.mappable)
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
        let mappable = if len == 0 {
            false
        } else if mode == AccessMode::Buffered {
            buffered_reason = Some("buffered access requested".to_string());
            false
        } else if mode == AccessMode::Auto && !mapping_is_safe(path) {
            buffered_reason = Some("network or removable drive".to_string());
            false
        } else {
            // Probe once so a file system that cannot map falls back cleanly.
            match map_range(&file, 0, len.min(4096)) {
                Ok(_) => true,
                Err(e) => {
                    buffered_reason = Some(format!("memory mapping failed: {e}"));
                    false
                }
            }
        };
        Ok(Source {
            path: path.to_path_buf(),
            len,
            modified: meta.modified().ok(),
            file,
            mappable,
            buffered_reason,
            blocks: Arc::new(Mutex::new(Lru::new(CACHE_BLOCKS))),
            views: Mutex::new(Lru::new(CACHE_VIEWS)),
            fetcher: Mutex::new(None),
        })
    }

    /// Lets non-blocking threads read this (buffered) source: misses are
    /// fetched by a background thread, which calls `notify` per block.
    /// Mapped sources never block on the network and need none.
    pub fn enable_background_fetch(&self, notify: crate::job::Notify) {
        if self.mappable || self.fetcher.lock().is_some() {
            return;
        }
        let Ok(file) = self.file.try_clone() else {
            return;
        };
        let (tx, rx) = channel::<u64>();
        let queued = Arc::new(Mutex::new(HashSet::new()));
        let failed = Arc::new(Mutex::new(None));
        let (blocks, q, f, len) = (
            self.blocks.clone(),
            queued.clone(),
            failed.clone(),
            self.len,
        );
        // Testing aid: simulate a slow drive.
        let delay = std::env::var("KATAK_TEST_READ_DELAY_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_millis);
        let spawned = std::thread::Builder::new()
            .name("katak-fetch".into())
            .spawn(move || {
                // Ends when the source (and its sender) is dropped.
                while let Ok(bi) = rx.recv() {
                    if let Some(d) = delay {
                        std::thread::sleep(d);
                    }
                    let start = bi * BLOCK as u64;
                    let n = (BLOCK as u64).min(len.saturating_sub(start)) as usize;
                    let mut data = vec![0u8; n];
                    match read_full_at(&file, &mut data, start) {
                        Ok(got) if got == n => blocks.lock().put(bi, data.into()),
                        Ok(_) => {
                            *f.lock() = Some((
                                "the file became shorter while it was open".into(),
                                Instant::now(),
                            ))
                        }
                        Err(e) => *f.lock() = Some((e.to_string(), Instant::now())),
                    }
                    q.lock().remove(&bi);
                    notify();
                }
            });
        if spawned.is_ok() {
            *self.fetcher.lock() = Some(Fetcher { tx, queued, failed });
        }
    }

    /// Puts the first bytes of the file (already read, e.g. the detection
    /// sample) into the block cache.
    pub fn prime(&self, head: &[u8]) {
        if self.mappable {
            return;
        }
        let n = (BLOCK as u64).min(self.len) as usize;
        if head.len() >= n {
            self.blocks.lock().put(0, head[..n].to_vec().into());
        }
    }

    /// On a non-blocking thread: queues the missing blocks among `range`
    /// (plus one read-ahead) and returns the error to report, if any.
    fn queue_missing(&self, range: std::ops::RangeInclusive<u64>) -> Option<io::Error> {
        if !NONBLOCKING.with(Cell::get) {
            return None;
        }
        let guard = self.fetcher.lock();
        let f = guard.as_ref()?;
        let last_block = self.len.saturating_sub(1) / BLOCK as u64;
        let missing: Vec<u64> = {
            let mut cache = self.blocks.lock();
            range.filter(|&bi| cache.get(bi).is_none()).collect()
        };
        if missing.is_empty() {
            return None;
        }
        if let Some((msg, at)) = f.failed.lock().clone()
            && at.elapsed() < FAILURE_HOLD
        {
            return Some(io::Error::other(msg));
        }
        let ahead = missing.last().map(|&b| b + 1).filter(|&b| b <= last_block);
        let mut queued = f.queued.lock();
        for bi in missing.iter().copied().chain(ahead) {
            if queued.insert(bi) {
                let _ = f.tx.send(bi);
            }
        }
        Some(pending())
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

    /// Size and modification time at open time.
    pub fn stamp(&self) -> FileStamp {
        FileStamp {
            len: self.len,
            modified: self.modified,
        }
    }

    /// Random access goes through memory-mapped views.
    pub fn is_mapped(&self) -> bool {
        self.mappable
    }

    /// Why the file is read in chunks instead of mapped.
    pub fn buffered_reason(&self) -> Option<&str> {
        self.buffered_reason.as_deref()
    }

    /// Reads into `buf` from `off` until `buf` is full or the file ends.
    /// Returns the number of bytes read. Bypasses caches and views.
    pub fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<usize> {
        if off >= self.len {
            return Ok(0);
        }
        let n = buf.len().min((self.len - off) as usize);
        let got = read_full_at(&self.file, &mut buf[..n], off)?;
        if got < n {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the file became shorter while it was open",
            ));
        }
        Ok(got)
    }

    /// Bytes `[off, off + len)`, clipped to the end of the file: borrowed
    /// from a mapped view, or assembled from cached blocks.
    pub fn bytes(&self, off: u64, len: usize) -> io::Result<Bytes> {
        let off = off.min(self.len);
        let end = off.saturating_add(len as u64).min(self.len);
        if self.mappable && end > off {
            // A failed mapping (address space, odd file systems) falls back
            // to plain reads.
            if let Ok(b) = self.view_bytes(off, end) {
                return Ok(b);
            }
        }
        if end > off
            && let Some(e) = self.queue_missing(off / BLOCK as u64..=(end - 1) / BLOCK as u64)
        {
            return Err(e);
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
        Ok(Bytes::Owned(out))
    }

    fn view_bytes(&self, off: u64, end: u64) -> io::Result<Bytes> {
        let k = off / VIEW;
        let vstart = k * VIEW;
        let vend = (vstart + VIEW + VIEW_OVERLAP).min(self.len);
        if end <= vend {
            let cached = self.views.lock().get(k);
            let view = match cached {
                Some(v) => v,
                None => {
                    let v = Arc::new(map_range(&self.file, vstart, vend - vstart)?);
                    self.views.lock().put(k, v.clone());
                    v
                }
            };
            return Ok(Bytes::Mapped(
                view,
                (off - vstart) as usize..(end - vstart) as usize,
            ));
        }
        // Larger than the overlap: map exactly this range, uncached.
        let v = Arc::new(map_range(&self.file, off, end - off)?);
        Ok(Bytes::Mapped(v, 0..(end - off) as usize))
    }

    fn block(&self, bi: u64) -> io::Result<Arc<[u8]>> {
        if let Some(b) = self.blocks.lock().get(bi) {
            return Ok(b);
        }
        let start = bi * BLOCK as u64;
        let n = (BLOCK as u64).min(self.len - start) as usize;
        let mut data = vec![0u8; n];
        self.read_at(start, &mut data)?;
        let data: Arc<[u8]> = data.into();
        self.blocks.lock().put(bi, data.clone());
        Ok(data)
    }
}

/// Maps `[off, off + len)` read-only.
fn map_range(file: &File, off: u64, len: u64) -> io::Result<Mmap> {
    // SAFETY: the map is read-only. Another process may still change the
    // file underneath us; on Windows it cannot truncate a file with mapped
    // views, and bytes appended beyond the open-time length are never read.
    unsafe { MmapOptions::new().offset(off).len(len as usize).map(file) }
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
/// bytes past it (unless the end is reached). Bulk readers keep the
/// unconsumed tail and refill a reusable buffer with positional reads;
/// cached readers (small UI-side reads) borrow mapped views or use the
/// block cache.
pub struct ChunkReader<'a> {
    src: &'a Source,
    end: u64,
    chunk: usize,
    /// Reused buffer; only `buf[..valid]` is meaningful (it is never shrunk
    /// or re-zeroed, which would cost a 16 MB memset per refill).
    buf: Vec<u8>,
    valid: usize,
    buf_base: u64,
    cached: bool,
    held: Option<Bytes>,
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
            valid: 0,
            buf_base: 0,
            cached: false,
            held: None,
        }
    }

    /// Like [`ChunkReader::new`], but reading through the source's mapped
    /// views or block cache (for small, repeated UI-side reads).
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
        if self.cached && self.src.is_mapped() {
            let held = self.held.insert(self.src.bytes(lo, (hi - lo) as usize)?);
            return Ok(Window {
                buf: held,
                base: lo,
                eof: hi == self.end,
            });
        }
        let have_hi = self.buf_base + self.valid as u64;
        let enough = lo >= self.buf_base && at <= have_hi && (have_hi >= hi || have_hi == self.end);
        if !enough {
            if lo >= self.buf_base && lo <= have_hi {
                let keep_from = (lo - self.buf_base) as usize;
                self.buf.copy_within(keep_from..self.valid, 0);
                self.valid -= keep_from;
            } else {
                self.valid = 0;
            }
            self.buf_base = lo;
            let have = self.buf_base + self.valid as u64;
            let want = (hi - have) as usize;
            let need = self.valid + want;
            if self.buf.len() < need {
                self.buf.resize(need, 0);
            }
            if self.cached {
                let bytes = self.src.bytes(have, want)?;
                self.buf[self.valid..self.valid + bytes.len()].copy_from_slice(&bytes);
                self.valid += bytes.len();
            } else {
                let got = self.src.read_at(have, &mut self.buf[self.valid..need])?;
                self.valid += got;
            }
        }
        let buf_end = self.buf_base + self.valid as u64;
        Ok(Window {
            buf: &self.buf[(lo - self.buf_base) as usize..self.valid],
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
        for (off, len) in [
            (0, 10),
            (1_048_570, 20),
            (2_999_990, 100),
            (5_000_000, 5),
            (10, 2_999_000),
        ] {
            assert_eq!(&*m.bytes(off, len).unwrap(), &*b.bytes(off, len).unwrap());
        }
    }

    #[test]
    fn views_cover_requests_across_view_boundaries() {
        // Requests straddling the 64 MB view grid, larger than the overlap.
        let len = (VIEW + VIEW_OVERLAP + (3 << 20)) as usize;
        let data: Vec<u8> = (0..len).map(|i| (i % 249) as u8).collect();
        let (_d, p) = temp_with(&data);
        let m = Source::open(&p, AccessMode::Mapped).unwrap();
        for (off, n) in [
            (VIEW - 10, 20usize),
            (VIEW + VIEW_OVERLAP - 5, 10),
            (VIEW - (1 << 20), (20 << 20) as usize),
        ] {
            let got = m.bytes(off, n).unwrap();
            assert_eq!(&*got, &data[off as usize..(off as usize + n).min(len)]);
        }
    }

    #[test]
    fn chunk_reader_windows_are_consistent() {
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 253) as u8).collect();
        let (_d, p) = temp_with(&data);
        for mode in [AccessMode::Mapped, AccessMode::Buffered] {
            let src = Source::open(&p, mode).unwrap();
            for cached in [false, true] {
                let mut rd = if cached {
                    ChunkReader::cached(&src, src.len(), 10_000)
                } else {
                    ChunkReader::new(&src, src.len(), 10_000)
                };
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

    #[test]
    fn nonblocking_reads_load_in_the_background() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let data: Vec<u8> = (0..3_500_000u32).map(|i| (i % 253) as u8).collect();
        let (_d, p) = temp_with(&data);
        let src = Source::open(&p, AccessMode::Buffered).unwrap();
        src.prime(&data[..1 << 20]);
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        src.enable_background_fetch(Arc::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
        }));
        set_nonblocking_reads(true);
        // The primed first block is there; the third is not.
        assert_eq!(&*src.bytes(10, 20).unwrap(), &data[10..30]);
        let err = src.bytes(2_500_000, 64).unwrap_err();
        assert!(is_pending(&err), "{err}");
        let t = Instant::now();
        let got = loop {
            match src.bytes(2_500_000, 64) {
                Ok(b) => break b,
                Err(e) if is_pending(&e) => {
                    assert!(t.elapsed() < Duration::from_secs(10), "never arrived");
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(e) => panic!("{e}"),
            }
        };
        assert_eq!(&*got, &data[2_500_000..2_500_064]);
        assert!(calls.load(Ordering::SeqCst) >= 1);
        // Blocking reads on this thread are unaffected.
        let far = with_blocking_reads(|| src.bytes(3_400_000, 16).unwrap());
        assert_eq!(&*far, &data[3_400_000..3_400_016]);
        set_nonblocking_reads(false);
    }
}
