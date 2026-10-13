//! Shared infrastructure for the bigsort tools: threading, mappings, O_DIRECT
//! writing, line scanning and an order-independent multiset digest.

use std::alloc::{alloc, dealloc, Layout};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;
use std::process::exit;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{Condvar, Mutex};

// --------------------------------------------------------------- constants

/// O_DIRECT block alignment.
pub const ALIGN: usize = 4096;
/// Transparent hugepage size.
pub const HUGE: usize = 2 << 20;
/// Granularity of the parallel line scan.
pub const SCAN_CHUNK: usize = 4 << 20;

/// Descriptor layout: low OFF_BITS bits = byte offset, high bits = length.
pub const OFF_BITS: u32 = 40;
pub const OFF_MASK: u64 = (1u64 << OFF_BITS) - 1;
pub const MAX_LEN: u64 = (1u64 << (64 - OFF_BITS)) - 1;

#[inline(always)]
pub fn desc(off: usize, len: usize) -> u64 {
    (off as u64) | ((len as u64) << OFF_BITS)
}
#[inline(always)]
pub fn desc_off(d: u64) -> usize {
    (d & OFF_MASK) as usize
}
#[inline(always)]
pub fn desc_len(d: u64) -> usize {
    (d >> OFF_BITS) as usize
}

#[inline(always)]
pub fn align_down(x: usize, a: usize) -> usize {
    x & !(a - 1)
}
#[inline(always)]
pub fn align_up(x: usize, a: usize) -> usize {
    align_down(x + a - 1, a)
}

pub fn die(msg: &str) -> ! {
    eprintln!("error: {msg}");
    exit(1);
}

// --------------------------------------------------------------------- rng

#[inline(always)]
pub fn wyrand(s: &mut u64) -> u64 {
    *s = s.wrapping_add(0x2d35_8dcc_aa6c_78a5);
    let t = (*s as u128).wrapping_mul((*s ^ 0x8bb8_4b93_962e_acc9) as u128);
    (t as u64) ^ ((t >> 64) as u64)
}

#[inline(always)]
pub fn splitmix(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Lemire multiply-shift bounded random: bias below 2^-32 for 32-bit bounds.
#[inline(always)]
pub fn bounded(s: &mut u64, bound: u64) -> u64 {
    (((wyrand(s) as u128) * (bound as u128)) >> 64) as u64
}

// ---------------------------------------------------------- raw memory

#[derive(Clone, Copy)]
pub struct Shared<T>(pub *mut T);
unsafe impl<T> Send for Shared<T> {}
unsafe impl<T> Sync for Shared<T> {}
impl<T> Shared<T> {
    #[inline(always)]
    pub fn ptr(&self) -> *mut T {
        self.0
    }
}

pub fn anon_map(bytes: usize, huge: bool) -> *mut libc::c_void {
    let p = unsafe {
        libc::mmap(
            ptr::null_mut(),
            bytes,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        )
    };
    if p == libc::MAP_FAILED {
        die(&format!("mmap({bytes} bytes): {}", std::io::Error::last_os_error()));
    }
    if huge {
        unsafe { libc::madvise(p, bytes, libc::MADV_HUGEPAGE) };
    }
    p
}

/// Anonymous arena of `u64`s, reserved with MAP_NORESERVE so that an
/// over-generous reservation costs address space and nothing else. The base is
/// hugepage-aligned: a region that straddles a transparent hugepage it does not
/// fill makes the kernel allocate and zero two 2 MiB pages instead of one.
pub struct Arena {
    raw: *mut libc::c_void,
    raw_bytes: usize,
    pub ptr: *mut u64,
}
impl Arena {
    pub fn new(elems: usize, huge: bool) -> Arena {
        let raw_bytes = elems * 8 + HUGE;
        let raw = anon_map(raw_bytes, huge);
        let ptr = align_up(raw as usize, HUGE) as *mut u64;
        Arena { raw, raw_bytes, ptr }
    }
}
impl Drop for Arena {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.raw, self.raw_bytes) };
    }
}

/// Block-aligned scratch buffer for O_DIRECT I/O.
pub struct AlignedBuf {
    pub ptr: *mut u8,
    pub cap: usize,
}
impl AlignedBuf {
    pub fn new() -> AlignedBuf {
        AlignedBuf { ptr: ptr::null_mut(), cap: 0 }
    }
    fn layout(cap: usize) -> Layout {
        Layout::from_size_align(cap, ALIGN).unwrap()
    }
    /// Grow to at least `want` bytes, preserving the first `keep` bytes.
    pub fn reserve(&mut self, want: usize, keep: usize) {
        if want <= self.cap {
            return;
        }
        let cap = want.next_power_of_two().max(1 << 16);
        let p = unsafe { alloc(Self::layout(cap)) };
        if p.is_null() {
            die("out of memory for an I/O buffer");
        }
        if !self.ptr.is_null() {
            if keep > 0 {
                unsafe { ptr::copy_nonoverlapping(self.ptr, p, keep.min(self.cap)) };
            }
            unsafe { dealloc(self.ptr, Self::layout(self.cap)) };
        }
        self.ptr = p;
        self.cap = cap;
    }
}
impl Default for AlignedBuf {
    fn default() -> Self {
        Self::new()
    }
}
impl Drop for AlignedBuf {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { dealloc(self.ptr, Self::layout(self.cap)) };
        }
    }
}

// ----------------------------------------------------------------- mappings

/// Read-only mapping of a file.
pub struct Input {
    ptr: *const u8,
    pub len: usize,
}
unsafe impl Send for Input {}
unsafe impl Sync for Input {}

impl Input {
    pub fn as_slice(&self) -> &[u8] {
        if self.len == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
        }
    }
    pub fn ptr(&self) -> *const u8 {
        self.ptr
    }
}
impl Drop for Input {
    fn drop(&mut self) {
        if self.len != 0 {
            unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.len) };
        }
    }
}

pub fn map_input(f: &File, len: usize) -> Input {
    if len == 0 {
        return Input { ptr: ptr::null(), len: 0 };
    }
    let p = unsafe {
        libc::mmap(ptr::null_mut(), len, libc::PROT_READ, libc::MAP_PRIVATE, f.as_raw_fd(), 0)
    };
    if p == libc::MAP_FAILED {
        die(&format!("mmap(input): {}", std::io::Error::last_os_error()));
    }
    Input { ptr: p as *const u8, len }
}

pub const MADV_POPULATE_READ: libc::c_int = 22;

/// Pull `[off, off+len)` of a mapping into the page cache, in parallel.
///
/// Leaving this to demand paging is far slower on a cold file: N threads each
/// faulting their own region look nothing like a sequential stream to the
/// kernel's readahead heuristic, and the scan degenerates into synchronous
/// single-page reads.
pub fn prefault(inp: &Input, off: usize, len: usize, threads: usize) {
    const SLICE: usize = 32 << 20;
    let base = Shared(unsafe { inp.ptr.add(off) } as *mut u8);
    parallel_for(len.div_ceil(SLICE), threads, |i| {
        let o = i * SLICE;
        let n = SLICE.min(len - o);
        unsafe { libc::madvise(base.ptr().add(o) as *mut _, n, MADV_POPULATE_READ) };
    });
}

/// Drop a byte range of a file from the page cache (keeps a bounded footprint
/// when streaming through something much bigger than RAM).
pub fn fadvise_dontneed(f: &File, off: u64, len: u64) {
    unsafe {
        libc::posix_fadvise(f.as_raw_fd(), off as libc::off_t, len as libc::off_t, libc::POSIX_FADV_DONTNEED)
    };
}

/// Drop a finished range of a mapping from this process's page tables. The
/// pages are clean and read-only, so they simply fault back in if touched.
/// Without this, RSS grows to the whole mapped file even though only one
/// segment is live at a time.
pub fn madvise_dontneed(inp: &Input, off: usize, len: usize) {
    // madvise needs a page-aligned start or it fails with EINVAL and silently
    // does nothing; shrink to the whole pages strictly inside the range so we
    // never drop a page the next segment still needs.
    let start = align_up(off, 4096);
    let end = align_down(off + len, 4096);
    if end <= start {
        return;
    }
    unsafe {
        libc::madvise(inp.ptr().add(start) as *mut libc::c_void, end - start, libc::MADV_DONTNEED)
    };
}

pub fn fadvise_sequential(f: &File) {
    unsafe { libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_SEQUENTIAL) };
}

// ------------------------------------------------------------- thread pool

/// Counting semaphore limiting how many threads may be inside a write at once.
///
/// NVMe devices typically sustain their peak with a handful of concurrent
/// O_DIRECT writers and fall off a cliff with dozens, so the compute side can
/// keep every core busy while the writes stay in the device's sweet spot.
pub struct Semaphore {
    n: Mutex<usize>,
    cv: Condvar,
}
impl Semaphore {
    pub fn new(n: usize) -> Semaphore {
        Semaphore { n: Mutex::new(n), cv: Condvar::new() }
    }
    pub fn acquire(&self) {
        let mut g = self.n.lock().unwrap();
        while *g == 0 {
            g = self.cv.wait(g).unwrap();
        }
        *g -= 1;
    }
    pub fn release(&self) {
        *self.n.lock().unwrap() += 1;
        self.cv.notify_one();
    }
}

/// Dynamic work queue over `tasks` indices.
pub fn parallel_for(tasks: usize, threads: usize, f: impl Fn(usize) + Sync) {
    parallel_for_with(tasks, threads, || (), |_, i| f(i));
}

/// Same, with a per-thread state built once per worker and dropped when the
/// worker retires.
pub fn parallel_for_with<T>(
    tasks: usize,
    threads: usize,
    init: impl Fn() -> T + Sync,
    f: impl Fn(&mut T, usize) + Sync,
) {
    if tasks == 0 {
        return;
    }
    let next = AtomicUsize::new(0);
    let threads = threads.min(tasks).max(1);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                let mut state = init();
                loop {
                    let i = next.fetch_add(1, Relaxed);
                    if i >= tasks {
                        break;
                    }
                    f(&mut state, i);
                }
            });
        }
    });
}

// ------------------------------------------------------------------ records

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fmt {
    /// `[u32 little-endian length][bytes]`
    Bin,
    /// `bytes` followed by a newline
    Text,
}

impl Fmt {
    /// Bytes one record of `len` payload bytes occupies.
    #[inline(always)]
    pub fn rec_len(self, len: usize) -> usize {
        len + if self == Fmt::Bin { 4 } else { 1 }
    }
}

// --------------------------------------------------------------- O_DIRECT

/// Streaming writer that appends records at a fixed file offset.
///
/// O_DIRECT can only write whole blocks from block-aligned memory, so the
/// buffer is positioned to share its alignment with the file offset: file
/// position X always lives at `buf[pad + X - start]`. A block-aligned slice of
/// the file is then a block-aligned slice of memory. The sub-block head and
/// tail of the whole range (at most one each) are handed back as `edges` for
/// one buffered pass at the end; they never share a block with a direct write.
pub struct DirectWriter<'a> {
    file: &'a File,
    sem: Option<&'a Semaphore>,
    direct: bool,
    fmt: Fmt,
    buf: AlignedBuf,
    cap: usize,
    start: u64,
    fill: usize,
    pad: usize,
    edges: Vec<(u64, Vec<u8>)>,
}

impl<'a> DirectWriter<'a> {
    pub fn new(
        file: &'a File,
        off: u64,
        cap: usize,
        direct: bool,
        fmt: Fmt,
        sem: Option<&'a Semaphore>,
    ) -> DirectWriter<'a> {
        let cap = align_up(cap.max(1 << 20), ALIGN);
        let pad = if direct { (off % ALIGN as u64) as usize } else { 0 };
        let mut buf = AlignedBuf::new();
        buf.reserve(pad + cap + ALIGN, 0);
        DirectWriter { file, sem, direct, fmt, buf, cap, start: off, fill: 0, pad, edges: Vec::new() }
    }

    #[inline(always)]
    pub fn push(&mut self, bytes: &[u8]) {
        let need = self.fmt.rec_len(bytes.len());
        if self.pad + self.fill + need > self.buf.cap {
            self.spill(need);
        }
        unsafe {
            let dst = self.buf.ptr.add(self.pad + self.fill);
            match self.fmt {
                Fmt::Bin => {
                    ptr::copy_nonoverlapping((bytes.len() as u32).to_le_bytes().as_ptr(), dst, 4);
                    ptr::copy_nonoverlapping(bytes.as_ptr(), dst.add(4), bytes.len());
                }
                Fmt::Text => {
                    ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len());
                    *dst.add(bytes.len()) = b'\n';
                }
            }
        }
        self.fill += need;
    }

    /// Current file offset just past the last pushed record.
    pub fn pos(&self) -> u64 {
        self.start + self.fill as u64
    }

    /// Flush what can be written as whole blocks, then make room for `need`.
    fn spill(&mut self, need: usize) {
        self.flush(false);
        if self.pad + self.fill + need > self.buf.cap {
            let want = self.pad + self.fill + need + ALIGN;
            self.buf.reserve(want, self.pad + self.fill);
            self.cap = self.buf.cap - self.pad - ALIGN;
        }
    }

    fn write_at(&self, bytes: &[u8], at: u64) {
        if let Some(s) = self.sem {
            s.acquire();
        }
        let r = self.file.write_all_at(bytes, at);
        if let Some(s) = self.sem {
            s.release();
        }
        r.unwrap_or_else(|e| die(&format!("write at {at}: {e}")));
    }

    fn flush(&mut self, final_: bool) {
        if self.fill == 0 && !final_ {
            return;
        }
        let end = self.start + self.fill as u64;
        if !self.direct {
            if self.fill > 0 {
                let b = unsafe { std::slice::from_raw_parts(self.buf.ptr.add(self.pad), self.fill) };
                self.write_at(b, self.start);
            }
            self.start = end;
            self.fill = 0;
            return;
        }
        // head [start, a) and tail [z, end) are the sub-block scraps; the
        // body [a, z) is whole blocks. The clamps matter when the whole range
        // lives inside a single block, in which case a == z == end.
        let a = (align_up(self.start as usize, ALIGN) as u64).clamp(self.start, end);
        let z = (align_down(end as usize, ALIGN) as u64).clamp(a, end);
        if !final_ && z == a {
            return; // nothing whole to write yet; the caller will grow
        }
        if a > self.start {
            // only possible on the first flush of an unaligned range
            let n = (a - self.start) as usize;
            let b = unsafe { std::slice::from_raw_parts(self.buf.ptr.add(self.pad), n) };
            self.edges.push((self.start, b.to_vec()));
        }
        if z > a {
            let from = self.pad + (a - self.start) as usize;
            let b = unsafe { std::slice::from_raw_parts(self.buf.ptr.add(from), (z - a) as usize) };
            self.write_at(b, a);
        }
        let kept_from = self.pad + (z - self.start) as usize;
        let kept = (end - z) as usize;
        if final_ {
            if kept > 0 {
                let b = unsafe { std::slice::from_raw_parts(self.buf.ptr.add(kept_from), kept) };
                self.edges.push((z, b.to_vec()));
            }
            self.start = end;
            self.fill = 0;
        } else {
            // carry the partial block; z is block aligned, so pad becomes 0
            unsafe { ptr::copy(self.buf.ptr.add(kept_from), self.buf.ptr, kept) };
            self.start = z;
            self.pad = 0;
            self.fill = kept;
        }
    }

    /// Flush and return the sub-block scraps for a later buffered pass.
    pub fn finish(mut self) -> Vec<(u64, Vec<u8>)> {
        self.flush(true);
        std::mem::take(&mut self.edges)
    }
}

/// Write the scraps collected from `DirectWriter`s through a buffered fd.
pub fn write_edges(path: &str, edges: &[(u64, Vec<u8>)]) {
    if edges.is_empty() {
        return;
    }
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap_or_else(|e| die(&format!("{path}: {e}")));
    for (at, bytes) in edges {
        f.write_all_at(bytes, *at).unwrap_or_else(|e| die(&format!("write(edge): {e}")));
    }
    f.sync_data().ok();
}

// ------------------------------------------------------------------ scanning

/// Walk every line whose terminating newline lies in `[b, e)`, plus the
/// unterminated tail if `e` is the end of `data`. A line belongs to the chunk
/// holding its newline, so chunks partition the lines exactly — nothing is
/// dropped or emitted twice, and a chunk's first line may start before `b`.
#[inline(always)]
pub fn for_each_line(data: &[u8], b: usize, e: usize, mut emit: impl FnMut(usize, usize)) {
    let mut start = if b == 0 { 0 } else { memchr::memrchr(b'\n', &data[..b]).map_or(0, |p| p + 1) };
    let mut pos = b;
    while let Some(rel) = memchr::memchr(b'\n', &data[pos..e]) {
        let nl = pos + rel;
        emit(start, nl - start);
        start = nl + 1;
        pos = nl + 1;
    }
    if e == data.len() && start < data.len() {
        emit(start, data.len() - start);
    }
}

/// Sample a few windows to estimate the line count of `data`.
pub fn estimate_lines(data: &[u8]) -> usize {
    let flen = data.len();
    if flen == 0 {
        return 0;
    }
    const W: usize = 64 << 10;
    if flen <= 16 * W {
        return memchr::memchr_iter(b'\n', data).count().max(1);
    }
    let windows = 256;
    let (mut nl, mut seen) = (0usize, 0usize);
    for i in 0..windows {
        let b = (flen - W) / windows * i;
        nl += memchr::memchr_iter(b'\n', &data[b..b + W]).count();
        seen += W;
    }
    ((nl.max(1) as u128 * flen as u128) / seen as u128) as usize
}

// -------------------------------------------------------------------- digest

/// Order-independent digest of a multiset of byte strings.
#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub struct Digest {
    pub lines: u64,
    pub bytes: u64,
    pub sum: u64,
    pub xor: u64,
}

impl Digest {
    #[inline(always)]
    pub fn add(&mut self, b: &[u8]) {
        let h = hash_bytes(b);
        self.lines += 1;
        self.bytes += b.len() as u64;
        self.sum = self.sum.wrapping_add(h);
        self.xor ^= h;
    }
    pub fn merge(&mut self, o: &Digest) {
        self.lines += o.lines;
        self.bytes += o.bytes;
        self.sum = self.sum.wrapping_add(o.sum);
        self.xor ^= o.xor;
    }
    pub fn show(&self, label: &str) {
        println!(
            "{label}: {} lines, {} bytes, sum {:016x}, xor {:016x}",
            self.lines, self.bytes, self.sum, self.xor
        );
    }
}

#[inline(always)]
pub fn hash_bytes(b: &[u8]) -> u64 {
    hash_bytes_seeded(b, 0xa076_1d64_78bd_642f)
}

#[inline(always)]
pub fn hash_bytes_seeded(b: &[u8], seed: u64) -> u64 {
    let mut h = splitmix(b.len() as u64 ^ seed);
    let mut i = 0;
    while i + 8 <= b.len() {
        h = splitmix(h ^ u64::from_le_bytes(b[i..i + 8].try_into().unwrap()));
        i += 8;
    }
    if i < b.len() {
        let mut t = [0u8; 8];
        t[..b.len() - i].copy_from_slice(&b[i..]);
        h = splitmix(h ^ u64::from_le_bytes(t));
    }
    h
}

/// Digest of every line of a text file, computed in parallel.
pub fn digest_text(data: &[u8], threads: usize) -> Digest {
    let nchunks = data.len().div_ceil(SCAN_CHUNK);
    let parts: Vec<Mutex<Digest>> = (0..nchunks.max(1)).map(|_| Mutex::new(Digest::default())).collect();
    parallel_for(nchunks, threads, |c| {
        let mut d = Digest::default();
        let b = c * SCAN_CHUNK;
        let e = (b + SCAN_CHUNK).min(data.len());
        for_each_line(data, b, e, |off, len| d.add(&data[off..off + len]));
        *parts[c].lock().unwrap() = d;
    });
    let mut all = Digest::default();
    for p in &parts {
        all.merge(&p.lock().unwrap());
    }
    all
}

/// Walk a `[u32 len][bytes]` record stream, calling `emit` for each record.
/// Returns an error message if the stream is truncated.
pub fn for_each_record(data: &[u8], mut emit: impl FnMut(&[u8])) -> Result<(), String> {
    let mut p = 0usize;
    while p < data.len() {
        if p + 4 > data.len() {
            return Err(format!("truncated record header at {p}"));
        }
        let len = u32::from_le_bytes(data[p..p + 4].try_into().unwrap()) as usize;
        p += 4;
        if p + len > data.len() {
            return Err(format!("truncated record body at {p} (len {len})"));
        }
        emit(&data[p..p + len]);
        p += len;
    }
    Ok(())
}

// ----------------------------------------------------------------- misc util

/// Parse sizes like `512`, `64k`, `8G`, `1.5g`.
pub fn parse_size(s: &str) -> Option<usize> {
    let s = s.trim();
    let (num, mult) = match s.chars().last()? {
        'k' | 'K' => (&s[..s.len() - 1], 1u64 << 10),
        'm' | 'M' => (&s[..s.len() - 1], 1u64 << 20),
        'g' | 'G' => (&s[..s.len() - 1], 1u64 << 30),
        't' | 'T' => (&s[..s.len() - 1], 1u64 << 40),
        _ => (s, 1),
    };
    let v: f64 = num.trim().parse().ok()?;
    if v < 0.0 {
        return None;
    }
    Some((v * mult as f64) as usize)
}

pub fn fmt_size(b: usize) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < U.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.2} {}", U[i])
    }
}

/// MemAvailable from /proc/meminfo, in bytes.
pub fn mem_available() -> usize {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines().find(|l| l.starts_with("MemAvailable:")).and_then(|l| {
                l.split_whitespace().nth(1).and_then(|v| v.parse::<usize>().ok()).map(|k| k * 1024)
            })
        })
        .unwrap_or(2 << 30)
}

pub fn default_threads() -> usize {
    std::thread::available_parallelism().map_or(8, |n| n.get())
}
