//! sortbin — external merge sort for a newline-separated file, bounded by a
//! RAM budget, writing `[u32 len][bytes]...` records (or text) in byte order.
//!
//!   phase 1 (runs)   the input is cut into segments that fit the budget. Each
//!                    segment is indexed in parallel into 16-byte keys — an
//!                    8-byte big-endian prefix of the line plus a packed
//!                    (offset, length) — sorted with a parallel sample sort,
//!                    and written out as a sorted run. Most comparisons are
//!                    settled by the inline prefix, in registers, without
//!                    touching the line itself.
//!   phase 2 (merge)  splitters sampled from the runs' sparse indexes cut the
//!                    key space into ranges. Each range is merged out of all
//!                    runs independently by one thread, straight into its own
//!                    prefix-summed slice of the output file, so the merge is
//!                    parallel and never materializes anything large.
//!
//! A segment costs `segment_bytes + 32 bytes per line` of RAM (two key arrays
//! for the sample sort); the merge costs one stream buffer per run per worker.
//! Both are derived from --memory, or set directly with --segment.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::Instant;

use bigcore::*;

// --------------------------------------------------------------------- keys

/// 8-byte key prefix plus a packed (offset, length) descriptor.
///
/// The prefix is the line's first 8 bytes, big-endian so that integer order is
/// byte order, zero-padded when the line is shorter. Padding can only create
/// ties, never a wrong order, and ties fall back to a full comparison.
#[derive(Clone, Copy)]
#[repr(C)]
struct Key {
    pre: u64,
    desc: u64,
}

#[inline(always)]
fn prefix8(data: &[u8], off: usize, len: usize) -> u64 {
    let n = len.min(8);
    if off + 8 <= data.len() {
        let v = u64::from_be_bytes(data[off..off + 8].try_into().unwrap());
        // keep the top n bytes; checked_shl covers n == 0, where a plain
        // shift by 64 wraps to a shift by 0 and leaks the following bytes
        v & (!0u64).checked_shl(8 * (8 - n) as u32).unwrap_or(0)
    } else {
        let mut t = [0u8; 8];
        t[..n].copy_from_slice(&data[off..off + n]);
        u64::from_be_bytes(t)
    }
}

#[inline(always)]
fn cmp_key(x: &Key, y: &Key, data: &[u8]) -> std::cmp::Ordering {
    if x.pre != y.pre {
        return x.pre.cmp(&y.pre);
    }
    let (xo, xl) = (desc_off(x.desc), desc_len(x.desc));
    let (yo, yl) = (desc_off(y.desc), desc_len(y.desc));
    if xl >= 8 && yl >= 8 {
        // first 8 bytes already known equal
        data[xo + 8..xo + xl].cmp(&data[yo + 8..yo + yl])
    } else {
        // equal prefix and at least one line shorter than 8 bytes: that line
        // is a prefix of the other, so the shorter one sorts first
        xl.cmp(&yl)
    }
}

/// Hugepage-backed array of keys.
struct KeyBuf {
    raw: *mut libc::c_void,
    raw_bytes: usize,
    ptr: *mut Key,
    n: usize,
}
unsafe impl Send for KeyBuf {}
unsafe impl Sync for KeyBuf {}
impl KeyBuf {
    fn new(n: usize) -> KeyBuf {
        let raw_bytes = n * std::mem::size_of::<Key>() + HUGE;
        let raw = anon_map(raw_bytes, raw_bytes >= (64 << 20));
        let ptr = align_up(raw as usize, HUGE) as *mut Key;
        KeyBuf { raw, raw_bytes, ptr, n }
    }
    fn s(&self) -> &[Key] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.n) }
    }
    #[allow(clippy::mut_from_ref)]
    fn sm(&self) -> &mut [Key] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.n) }
    }
}
impl Drop for KeyBuf {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.raw, self.raw_bytes) };
    }
}

// ------------------------------------------------------------------ indexing

/// Build the key array for `data[b0..e0]` (which must start at a line start
/// and end at a line end), in parallel.
fn index_segment(data: &[u8], b0: usize, e0: usize, threads: usize) -> KeyBuf {
    let seg = &data[b0..e0];
    let nchunks = seg.len().div_ceil(SCAN_CHUNK).max(1);
    let counts: Vec<AtomicUsize> = (0..nchunks).map(|_| AtomicUsize::new(0)).collect();
    parallel_for(nchunks, threads, |c| {
        let b = c * SCAN_CHUNK;
        let e = (b + SCAN_CHUNK).min(seg.len());
        let mut n = memchr::memchr_iter(b'\n', &seg[b..e]).count();
        if e == seg.len() && seg.last() != Some(&b'\n') {
            n += 1; // unterminated final line
        }
        counts[c].store(n, Relaxed);
    });
    let mut base = Vec::with_capacity(nchunks + 1);
    let mut acc = 0usize;
    for c in &counts {
        base.push(acc);
        acc += c.load(Relaxed);
    }
    base.push(acc);

    let keys = KeyBuf::new(acc);
    let kp = Shared(keys.ptr);
    parallel_for(nchunks, threads, |c| {
        let b = c * SCAN_CHUNK;
        let e = (b + SCAN_CHUNK).min(seg.len());
        let mut i = base[c];
        for_each_line(seg, b, e, |off, len| {
            if len as u64 > MAX_LEN {
                die("line longer than 16 MiB");
            }
            unsafe {
                *kp.ptr().add(i) = Key { pre: prefix8(seg, off, len), desc: desc(b0 + off, len) };
            }
            i += 1;
        });
        debug_assert_eq!(i, base[c + 1]);
    });
    keys
}

/// Index one segment of a `[u32 len][bytes]` stream, starting at `b0` and
/// taking records whose header starts before `b0 + limit`. Returns the segment
/// end and its keys.
///
/// Record boundaries are a chain — you cannot find the n-th record without
/// walking the first n-1 — so pass A walks the length headers sequentially
/// (forward-only, so the hardware prefetcher covers it) and drops a checkpoint
/// every CHECK records; pass B then builds the keys between checkpoints in
/// parallel.
fn index_segment_bin(data: &[u8], b0: usize, limit: usize, threads: usize) -> (usize, KeyBuf) {
    const CHECK: usize = 1 << 16;
    let hard = (b0 + limit).min(data.len());
    let mut ck: Vec<(usize, usize)> = Vec::new();
    let (mut off, mut n) = (b0, 0usize);
    while off < hard {
        if n % CHECK == 0 {
            ck.push((off, n));
        }
        if off + 4 > data.len() {
            die("truncated record header");
        }
        let len = u32::from_le_bytes(data[off..off + 4].try_into().unwrap()) as usize;
        if len as u64 > MAX_LEN {
            die("record longer than 16 MiB");
        }
        if off + 4 + len > data.len() {
            die("truncated record body");
        }
        off += 4 + len;
        n += 1;
    }
    let end = off;

    let keys = KeyBuf::new(n);
    let kp = Shared(keys.ptr);
    parallel_for(ck.len(), threads, |c| {
        let (mut o, mut i) = ck[c];
        let stop = if c + 1 < ck.len() { ck[c + 1].0 } else { end };
        while o < stop {
            let len = u32::from_le_bytes(data[o..o + 4].try_into().unwrap()) as usize;
            unsafe {
                *kp.ptr().add(i) = Key { pre: prefix8(data, o + 4, len), desc: desc(o + 4, len) };
            }
            o += 4 + len;
            i += 1;
        }
    });
    (end, keys)
}

// ---------------------------------------------------------------- sort phase

/// Parallel sample sort. Returns true when the sorted result is in `b`.
///
/// Splitters sampled from the data cut the keys into P buckets; every key is
/// counted, scattered into its bucket, and then each bucket is sorted
/// independently. Concatenating the buckets is the sorted array, so there is
/// no merge step.
fn sort_keys(a: &mut [Key], b: &mut [Key], data: &[u8], threads: usize, seed: u64) -> bool {
    let n = a.len();
    if n <= (1 << 15) || threads == 1 {
        a.sort_unstable_by(|x, y| cmp_key(x, y, data));
        return false;
    }
    let p = (threads * 4).clamp(2, 1024).next_power_of_two();

    let mut rng = seed | 1;
    let ssize = (p * 32).min(n);
    let mut sample: Vec<Key> = (0..ssize).map(|_| a[bounded(&mut rng, n as u64) as usize]).collect();
    sample.sort_unstable_by(|x, y| cmp_key(x, y, data));
    let splitters: Vec<Key> = (1..p).map(|i| sample[i * ssize / p]).collect();

    #[inline(always)]
    fn bucket_of(splitters: &[Key], k: &Key, data: &[u8]) -> usize {
        // number of splitters <= k, which is monotone in k
        let (mut lo, mut hi) = (0usize, splitters.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if cmp_key(&splitters[mid], k, data) != std::cmp::Ordering::Greater {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    let chunks = threads;
    let csz = n.div_ceil(chunks);
    let mut counts = vec![0usize; chunks * p];
    let cp = Shared(counts.as_mut_ptr());
    parallel_for(chunks, threads, |c| {
        let (lo, hi) = (c * csz, ((c + 1) * csz).min(n));
        let row = unsafe { std::slice::from_raw_parts_mut(cp.ptr().add(c * p), p) };
        for k in &a[lo..hi] {
            row[bucket_of(&splitters, k, data)] += 1;
        }
    });

    // bucket-major prefix sum: bucket 0's chunks, then bucket 1's, ...
    let mut starts = vec![0usize; chunks * p + 1];
    let mut acc = 0usize;
    for bk in 0..p {
        for c in 0..chunks {
            starts[c * p + bk] = acc;
            acc += counts[c * p + bk];
        }
    }
    starts[chunks * p] = acc;
    debug_assert_eq!(acc, n);
    let mut bstart = vec![0usize; p + 1];
    {
        let mut acc = 0usize;
        for bk in 0..p {
            bstart[bk] = acc;
            for c in 0..chunks {
                acc += counts[c * p + bk];
            }
        }
        bstart[p] = acc;
    }

    let bp = Shared(b.as_mut_ptr());
    parallel_for(chunks, threads, |c| {
        let (lo, hi) = (c * csz, ((c + 1) * csz).min(n));
        let mut cur: Vec<usize> = (0..p).map(|bk| starts[c * p + bk]).collect();
        for k in &a[lo..hi] {
            let bk = bucket_of(&splitters, k, data);
            unsafe { *bp.ptr().add(cur[bk]) = *k };
            cur[bk] += 1;
        }
    });

    parallel_for(p, threads, |bk| {
        let (lo, hi) = (bstart[bk], bstart[bk + 1]);
        let s = unsafe { std::slice::from_raw_parts_mut(bp.ptr().add(lo), hi - lo) };
        s.sort_unstable_by(|x, y| cmp_key(x, y, data));
    });
    true
}

// ----------------------------------------------------------------- run files

/// One entry per SPARSE_RECS records or SPARSE_BYTES bytes, whichever comes
/// first, so that the gap between entries is bounded in both records and bytes.
const SPARSE_RECS: u64 = 1024;
const SPARSE_BYTES: u64 = 1 << 20;

#[derive(Clone, Copy)]
struct Sparse {
    off: u64,
    rec: u64,
    pre: u64,
}

struct Run {
    path: String,
    file: File,
    bytes: u64,
    nrecs: u64,
    sparse: Vec<Sparse>,
}

struct Dumped {
    bytes: u64,
    nrecs: u64,
    sparse: Vec<Sparse>,
}

/// Write sorted keys out as records, in parallel: each chunk of the key array
/// knows its own byte offset from a prefix sum, so chunks write concurrently.
#[allow(clippy::too_many_arguments)]
fn dump(
    keys: &[Key],
    data: &[u8],
    path: &str,
    fmt: Fmt,
    threads: usize,
    direct: bool,
    sem: &Semaphore,
    wbuf: usize,
    unique: bool,
) -> Dumped {
    let n = keys.len();
    let nchunks = (threads * 8).min(n.div_ceil(4096)).max(1);
    let csz = n.div_ceil(nchunks);
    let nchunks = n.div_ceil(csz.max(1)).max(1);

    // duplicates are adjacent in a sorted array, so "keep" is a local test
    // against the predecessor — including across chunk boundaries, since the
    // whole key array is in hand
    let keep = |i: usize| -> bool {
        !unique || i == 0 || cmp_key(&keys[i - 1], &keys[i], data) != std::cmp::Ordering::Equal
    };
    let sizes: Vec<AtomicUsize> = (0..nchunks).map(|_| AtomicUsize::new(0)).collect();
    let counts: Vec<AtomicUsize> = (0..nchunks).map(|_| AtomicUsize::new(0)).collect();
    parallel_for(nchunks, threads, |c| {
        let (lo, hi) = (c * csz, ((c + 1) * csz).min(n));
        let (mut s, mut cnt) = (0usize, 0usize);
        for i in lo..hi {
            if keep(i) {
                s += fmt.rec_len(desc_len(keys[i].desc));
                cnt += 1;
            }
        }
        sizes[c].store(s, Relaxed);
        counts[c].store(cnt, Relaxed);
    });
    let mut offs = Vec::with_capacity(nchunks + 1);
    let mut nkept_before = Vec::with_capacity(nchunks + 1);
    let (mut acc, mut kacc) = (0u64, 0u64);
    for c in 0..nchunks {
        offs.push(acc);
        nkept_before.push(kacc);
        acc += sizes[c].load(Relaxed) as u64;
        kacc += counts[c].load(Relaxed) as u64;
    }
    offs.push(acc);
    nkept_before.push(kacc);
    let total = acc;
    let nkept = kacc;

    let fout = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(if direct { libc::O_DIRECT } else { 0 })
        .open(path)
        .unwrap_or_else(|e| die(&format!("{path}: {e}")));
    unsafe { libc::fallocate(fout.as_raw_fd(), 0, 0, total as libc::off_t) };

    let out: Vec<Mutex<(Vec<Sparse>, Vec<(u64, Vec<u8>)>)>> =
        (0..nchunks).map(|_| Mutex::new((Vec::new(), Vec::new()))).collect();
    parallel_for(nchunks, threads, |c| {
        let (lo, hi) = (c * csz, ((c + 1) * csz).min(n));
        let mut w = DirectWriter::new(&fout, offs[c], wbuf, direct, fmt, Some(sem));
        let mut sparse = Vec::new();
        let (mut since_rec, mut since_byte) = (u64::MAX, u64::MAX); // force one at the start
        let mut emitted = nkept_before[c];
        for (i, k) in keys[lo..hi].iter().enumerate() {
            if !keep(lo + i) {
                continue;
            }
            let (koff, klen) = (desc_off(k.desc), desc_len(k.desc));
            if since_rec >= SPARSE_RECS || since_byte >= SPARSE_BYTES {
                sparse.push(Sparse { off: w.pos(), rec: emitted, pre: k.pre });
                since_rec = 0;
                since_byte = 0;
            }
            emitted += 1;
            w.push(&data[koff..koff + klen]);
            since_rec += 1;
            since_byte += fmt.rec_len(klen) as u64;
        }
        debug_assert_eq!(w.pos(), offs[c + 1]);
        *out[c].lock().unwrap() = (sparse, w.finish());
    });

    let mut sparse = Vec::new();
    let mut edges = Vec::new();
    for c in out {
        let (s, e) = c.into_inner().unwrap();
        sparse.extend(s);
        edges.extend(e);
    }
    write_edges(path, &edges);
    fout.set_len(total).unwrap();
    fout.sync_data().unwrap_or_else(|e| die(&format!("fsync {path}: {e}")));
    Dumped { bytes: total, nrecs: nkept, sparse }
}

// --------------------------------------------------------------- merge phase

/// Read one record's header plus up to `max` payload bytes.
fn record_at(f: &File, off: u64, max: usize) -> (usize, Vec<u8>) {
    let mut hdr = [0u8; 4];
    f.read_exact_at(&mut hdr, off).unwrap_or_else(|e| die(&format!("read run: {e}")));
    let len = u32::from_le_bytes(hdr) as usize;
    let take = len.min(max);
    let mut v = vec![0u8; take];
    if take > 0 {
        f.read_exact_at(&mut v, off + 4).unwrap_or_else(|e| die(&format!("read run: {e}")));
    }
    (len, v)
}

/// Compare a record (of which only a prefix may be in hand) against a splitter.
fn cmp_rec(rec_prefix: &[u8], rec_len: usize, s: &[u8]) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    let m = rec_prefix.len().min(s.len());
    match rec_prefix[..m].cmp(&s[..m]) {
        Equal => {}
        o => return o,
    }
    if rec_prefix.len() >= s.len() {
        rec_len.cmp(&s.len())
    } else {
        Less // the record ran out first, so it is a strict prefix of s
    }
}

/// Byte offset and record index of the first record in `run` that is >= `s`.
fn lower_bound(run: &Run, s: &[u8], probe: usize) -> (u64, u64) {
    let (mut lo, mut hi) = (0usize, run.sparse.len());
    while lo < hi {
        let mid = (lo + hi) / 2;
        let e = run.sparse[mid];
        let (len, pfx) = record_at(&run.file, e.off, probe);
        if cmp_rec(&pfx, len, s) == std::cmp::Ordering::Less {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    // everything before sparse[lo] (and after sparse[lo-1]) needs a linear scan
    let (mut off, mut rec) = if lo == 0 { (0, 0) } else { (run.sparse[lo - 1].off, run.sparse[lo - 1].rec) };
    let end = if lo < run.sparse.len() { run.sparse[lo].off } else { run.bytes };
    while off < end {
        let (len, pfx) = record_at(&run.file, off, probe);
        if cmp_rec(&pfx, len, s) != std::cmp::Ordering::Less {
            break;
        }
        off += 4 + len as u64;
        rec += 1;
    }
    (off, rec)
}

/// Buffered forward reader over one run's slice.
struct Stream {
    f: File,
    pos: u64,  // next file offset to read into the buffer
    end: u64,  // end of this slice
    at: u64,   // file offset of the current record
    buf: Vec<u8>,
    head: usize, // buffer offset of the current record
    fill: usize,
    cur: usize, // payload offset in buf
    len: usize,
}

impl Stream {
    fn new(path: &str, start: u64, end: u64, bufsize: usize) -> Option<Stream> {
        if start >= end {
            return None;
        }
        let f = File::open(path).unwrap_or_else(|e| die(&format!("{path}: {e}")));
        fadvise_sequential(&f);
        let mut s = Stream {
            f,
            pos: start,
            end,
            at: start,
            buf: vec![0u8; bufsize.max(1 << 16)],
            head: 0,
            fill: 0,
            cur: 0,
            len: 0,
        };
        if s.load() {
            Some(s)
        } else {
            None
        }
    }

    fn refill(&mut self, need: usize) {
        let left = self.fill - self.head;
        if self.buf.len() < need + 64 {
            self.buf.resize((need + 64).next_power_of_two(), 0);
        }
        self.buf.copy_within(self.head..self.fill, 0);
        self.head = 0;
        self.fill = left;
        let want = (self.buf.len() - self.fill).min((self.end - self.pos) as usize);
        if want > 0 {
            let at = self.pos;
            let dst = &mut self.buf[self.fill..self.fill + want];
            self.f.read_exact_at(dst, at).unwrap_or_else(|e| die(&format!("read run: {e}")));
            self.pos += want as u64;
            self.fill += want;
        }
    }

    /// Parse the record at `head`; false when the slice is exhausted.
    fn load(&mut self) -> bool {
        if self.at >= self.end {
            return false;
        }
        if self.fill - self.head < 4 {
            self.refill(4);
            if self.fill - self.head < 4 {
                return false;
            }
        }
        let len =
            u32::from_le_bytes(self.buf[self.head..self.head + 4].try_into().unwrap()) as usize;
        if self.fill - self.head < 4 + len {
            self.refill(4 + len);
            if self.fill - self.head < 4 + len {
                die("truncated record in run file");
            }
        }
        self.cur = self.head + 4;
        self.len = len;
        true
    }

    #[inline(always)]
    fn rec(&self) -> &[u8] {
        &self.buf[self.cur..self.cur + self.len]
    }

    #[inline(always)]
    fn pre(&self) -> u64 {
        let r = self.rec();
        let n = r.len().min(8);
        let mut t = [0u8; 8];
        t[..n].copy_from_slice(&r[..n]);
        u64::from_be_bytes(t)
    }

    fn advance(&mut self) -> bool {
        self.head = self.cur + self.len;
        self.at += 4 + self.len as u64;
        self.load()
    }
}

/// Min-heap of stream indices, ordered by their current records.
fn heap_less(s: &[Stream], h: &[(u64, usize)], i: usize, j: usize) -> bool {
    if h[i].0 != h[j].0 {
        return h[i].0 < h[j].0;
    }
    s[h[i].1].rec() < s[h[j].1].rec()
}

fn sift_down(s: &[Stream], h: &mut Vec<(u64, usize)>, mut i: usize) {
    loop {
        let (l, r) = (2 * i + 1, 2 * i + 2);
        let mut m = i;
        if l < h.len() && heap_less(s, h, l, m) {
            m = l;
        }
        if r < h.len() && heap_less(s, h, r, m) {
            m = r;
        }
        if m == i {
            return;
        }
        h.swap(i, m);
        i = m;
    }
}

// ----------------------------------------------------------------------- cli

struct Opts {
    input: String,
    output: String,
    tmp: String,
    memory: usize,
    segment: usize,
    threads: usize,
    write_threads: usize,
    in_fmt: Fmt,
    fmt: Fmt,
    unique: bool,
    direct: bool,
    sync: bool,
    keep_runs: bool,
    quiet: bool,
}

fn parse_fmt(s: &str) -> Fmt {
    match s {
        "bin" | "binary" | "records" => Fmt::Bin,
        "text" | "lines" => Fmt::Text,
        _ => usage(),
    }
}

fn usage() -> ! {
    eprintln!(
        "usage: sortbin [options] <input.txt> <output.bin>\n\
         \x20      sortbin verify [--text] <input.txt> <output>\n\n\
         Sorts the lines of <input.txt> into byte (memcmp) order using an external\n\
         merge sort, and writes them as records: a 4-byte little-endian length followed\n\
         by that many bytes (or as newline-separated text with --text). Lines are raw\n\
         bytes; empty lines and a final line without a newline are kept.\n\n\
         options:\n\
         \x20 --memory <size>     RAM budget, e.g. 8G (default: half of MemAvailable)\n\
         \x20 --segment <size>    input bytes per run, overriding the budget\n\
         \x20 --threads <n>       worker threads (default: all cores)\n\
         \x20 --write-threads <n> concurrent writes allowed (default min(threads,16))\n\
         \x20 --tmp <dir>         where run files go (default: the output's directory)\n\
         \x20 --unique, -u        drop duplicate lines (keep one of each)\n\
         \x20 --in <bin|text>     input format (default text: newline-separated)\n\
         \x20 --out <bin|text>    output format (default bin: length-prefixed records)\n\
         \x20 --text              shorthand for --out text\n\
         \x20 --buffered          write via the page cache instead of O_DIRECT\n\
         \x20 --keep-runs         don't delete the run files\n\
         \x20 --no-sync           don't fsync the output before exiting\n\
         \x20 --quiet             no timings on stderr\n\n\
         verify checks that the output is sorted and is a permutation of the input;\n\
         it takes the same --in/--out/--unique flags (with --unique it checks set\n\
         equality and that no record repeats)."
    );
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("verify") {
        let mut uniq = false;
        let mut inf = Fmt::Text;
        let mut outf = if args.iter().any(|a| a == "--text") { Fmt::Text } else { Fmt::Bin };
        let mut pos: Vec<String> = Vec::new();
        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "--in" => {
                    i += 1;
                    inf = parse_fmt(args.get(i).unwrap_or_else(|| usage()));
                }
                "--out" => {
                    i += 1;
                    outf = parse_fmt(args.get(i).unwrap_or_else(|| usage()));
                }
                "--text" => {}
                "--unique" | "-u" => uniq = true,
                a if a.starts_with('-') => usage(),
                a => pos.push(a.to_string()),
            }
            i += 1;
        }
        if pos.len() != 2 {
            usage();
        }
        verify(&pos[0], &pos[1], inf, outf, uniq);
        return;
    }

    let mut o = Opts {
        input: String::new(),
        output: String::new(),
        tmp: String::new(),
        memory: 0,
        segment: 0,
        threads: default_threads(),
        write_threads: 0,
        in_fmt: Fmt::Text,
        fmt: Fmt::Bin,
        unique: false,
        direct: true,
        sync: true,
        keep_runs: false,
        quiet: false,
    };
    let mut pos = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].clone();
        let mut val = || {
            i += 1;
            args.get(i).cloned().unwrap_or_else(|| usage())
        };
        match a.as_str() {
            "--memory" => o.memory = parse_size(&val()).unwrap_or_else(|| usage()),
            "--segment" => o.segment = parse_size(&val()).unwrap_or_else(|| usage()),
            "--threads" => o.threads = val().parse().unwrap_or_else(|_| usage()),
            "--write-threads" => o.write_threads = val().parse().unwrap_or_else(|_| usage()),
            "--tmp" => o.tmp = val(),
            "--text" => o.fmt = Fmt::Text,
            "--unique" | "-u" => o.unique = true,
            "--in" => o.in_fmt = parse_fmt(&val()),
            "--out" => o.fmt = parse_fmt(&val()),
            "--buffered" => o.direct = false,
            "--keep-runs" => o.keep_runs = true,
            "--no-sync" => o.sync = false,
            "--quiet" | "-q" => o.quiet = true,
            "-h" | "--help" => usage(),
            _ if a.starts_with('-') => usage(),
            _ => pos.push(a),
        }
        i += 1;
    }
    if pos.len() != 2 {
        usage();
    }
    o.input = pos[0].clone();
    o.output = pos[1].clone();
    o.threads = o.threads.max(1);
    if o.write_threads == 0 {
        o.write_threads = o.threads.min(16);
    }
    o.write_threads = o.write_threads.clamp(1, o.threads);
    if o.memory == 0 {
        o.memory = mem_available() / 2;
    }
    if o.tmp.is_empty() {
        o.tmp = std::path::Path::new(&o.output)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| ".".into());
    }
    run(&o);
}

macro_rules! say {
    ($o:expr, $($arg:tt)*) => { if !$o.quiet { eprintln!($($arg)*); } };
}

fn run(o: &Opts) {
    let t0 = Instant::now();
    let fin = File::open(&o.input).unwrap_or_else(|e| die(&format!("{}: {e}", o.input)));
    let flen = fin.metadata().unwrap().len() as usize;
    if flen as u64 > OFF_MASK {
        die("input larger than 1 TiB (the descriptor offset field is 40 bits)");
    }
    if flen == 0 {
        File::create(&o.output).unwrap_or_else(|e| die(&format!("{}: {e}", o.output)));
        say!(o, "0 lines");
        return;
    }
    let input = map_input(&fin, flen);
    let data = input.as_slice();

    let est = match o.in_fmt {
        Fmt::Text => estimate_lines(data).max(1),
        // sample the record chain from the front to estimate the average size
        Fmt::Bin => {
            let (mut off, mut n) = (0usize, 0usize);
            while off + 4 <= data.len() && n < 1 << 20 {
                off += 4 + u32::from_le_bytes(data[off..off + 4].try_into().unwrap()) as usize;
                n += 1;
            }
            ((n.max(1) as u128 * flen as u128) / off.max(1) as u128).max(1) as usize
        }
    };
    let avg = (flen / est).max(1);
    // a segment costs its own bytes (touched while indexing and gathering)
    // plus two 16-byte keys per line for the sample sort
    let seg_bytes = if o.segment > 0 {
        o.segment
    } else {
        // 0.85 leaves room for the sampled average line length being an
        // over-estimate, which would otherwise make the key arrays overshoot
        ((o.memory as u128 * avg as u128 * 85) / ((avg + 32) as u128 * 100)) as usize
    }
    .clamp((1 << 20).min(flen), flen.max(1));

    // text segments cut at the first newline at or after the budget; binary
    // ones have to be walked, so they are discovered as we go
    let mut segs: Vec<(usize, usize)> = Vec::new();
    if o.in_fmt == Fmt::Text {
        let mut at = 0usize;
        while at < flen {
            let want = (at + seg_bytes).min(flen);
            let end = if want >= flen {
                flen
            } else {
                match memchr::memchr(b'\n', &data[want..]) {
                    Some(p) => want + p + 1,
                    None => flen,
                }
            };
            segs.push((at, end));
            at = end;
        }
    }
    let nsegs = if o.in_fmt == Fmt::Text { segs.len() } else { flen.div_ceil(seg_bytes) };

    // gather buffers are per worker, so they come out of the budget too
    let wbuf = (o.memory / o.threads.max(1) / 4).clamp(256 << 10, 4 << 20);
    let wsem = Semaphore::new(o.write_threads);
    say!(
        o,
        "{} in, ~{} lines, {} threads, {} budget -> {} runs of {}",
        fmt_size(flen),
        est,
        o.threads,
        fmt_size(o.memory),
        nsegs,
        fmt_size(seg_bytes)
    );

    // ---- phase 1: sorted runs ---------------------------------------------
    let t1 = Instant::now();
    let single = nsegs == 1;
    let mut runs: Vec<Run> = Vec::new();
    let mut nlines = 0u64;
    let mut out_bytes = 0u64;
    let mut at = 0usize;
    for i in 0..nsegs {
        let ts = Instant::now();
        let (b, e, a) = if o.in_fmt == Fmt::Text {
            let (b, e) = segs[i];
            prefault(&input, b, e - b, o.threads);
            (b, e, index_segment(data, b, e, o.threads))
        } else {
            let b = at;
            prefault(&input, b, seg_bytes.min(flen - b), o.threads);
            let (e, k) = index_segment_bin(data, b, seg_bytes, o.threads);
            (b, e, k)
        };
        at = e;
        let n = a.s().len();
        let scratch = KeyBuf::new(n);
        let in_b = sort_keys(a.sm(), scratch.sm(), data, o.threads, splitmix(0x5e9 + i as u64));
        let sorted = if in_b { scratch.s() } else { a.s() };

        let path =
            if single { o.output.clone() } else { format!("{}/sortbin-{}-run{i}.tmp", o.tmp, std::process::id()) };
        let fmt = if single { o.fmt } else { Fmt::Bin };
        let d = dump(sorted, data, &path, fmt, o.threads, o.direct, &wsem, wbuf, o.unique);
        nlines += d.nrecs;
        out_bytes = d.bytes;
        say!(
            o,
            "  run {:<3} {:>12} lines  {:>10}  {:>6.2}s",
            i,
            d.nrecs,
            fmt_size(d.bytes as usize),
            ts.elapsed().as_secs_f64()
        );
        if !single {
            let f = File::open(&path).unwrap_or_else(|e| die(&format!("{path}: {e}")));
            runs.push(Run { path, file: f, bytes: d.bytes, nrecs: d.nrecs, sparse: d.sparse });
        }
        // the segment is done with: drop both its page-cache pages and our
        // page-table entries for it, so RSS tracks one segment, not the file
        drop(a);
        madvise_dontneed(&input, b, e - b);
        fadvise_dontneed(&fin, b as u64, (e - b) as u64);
        if e >= flen {
            break;
        }
    }
    say!(o, "runs      {:>8.3}s", t1.elapsed().as_secs_f64());

    // ---- phase 2: merge ----------------------------------------------------
    if !single {
        let t2 = Instant::now();
        let (b, r) = merge(o, &runs, &wsem, wbuf);
        out_bytes = b;
        nlines = r;
        say!(o, "merge     {:>8.3}s", t2.elapsed().as_secs_f64());
        if !o.keep_runs {
            for r in &runs {
                let _ = std::fs::remove_file(&r.path);
            }
        }
    }

    let secs = t0.elapsed().as_secs_f64();
    say!(
        o,
        "total     {:>8.3}s  {} lines, {} in -> {} out, {:.2} GiB/s",
        secs,
        nlines,
        fmt_size(flen),
        fmt_size(out_bytes as usize),
        (flen as f64 + out_bytes as f64) / (1u64 << 30) as f64 / secs
    );
}

/// Parallel k-way merge: splitters cut the key space into ranges, each range is
/// merged from every run by one worker into its own slice of the output.
fn merge(o: &Opts, runs: &[Run], wsem: &Semaphore, wbuf: usize) -> (u64, u64) {
    const PROBE: usize = 4096;
    let nranges = (o.threads * 4).clamp(1, 4096);

    // candidate splitters: every sparse entry of every run
    let mut cand: Vec<(u64, usize, u64)> = Vec::new(); // (pre, run, off)
    for (ri, r) in runs.iter().enumerate() {
        for s in &r.sparse {
            cand.push((s.pre, ri, s.off));
        }
    }
    cand.sort_unstable();
    let nsplit = (nranges - 1).min(cand.len());
    let mut splitters: Vec<Vec<u8>> = Vec::new();
    for i in 1..=nsplit {
        let (_, ri, off) = cand[i * cand.len() / (nsplit + 1)];
        let (_, bytes) = record_at(&runs[ri].file, off, PROBE);
        splitters.push(bytes);
    }
    splitters.sort();
    splitters.dedup();

    // boundaries[r][p] = (byte offset, record index) of the first record >= splitter p
    let nb = splitters.len() + 2;
    let mut bounds = vec![(0u64, 0u64); runs.len() * nb];
    let bp = Shared(bounds.as_mut_ptr());
    parallel_for(runs.len() * splitters.len(), o.threads, |i| {
        let (r, p) = (i / splitters.len(), i % splitters.len());
        let v = lower_bound(&runs[r], &splitters[p], PROBE);
        unsafe { *bp.ptr().add(r * nb + p + 1) = v };
    });
    for (r, run) in runs.iter().enumerate() {
        bounds[r * nb] = (0, 0);
        bounds[r * nb + nb - 1] = (run.bytes, run.nrecs);
    }

    let nranges = nb - 1;
    let mut offs = Vec::with_capacity(nranges + 1);
    let mut acc = 0u64;
    for p in 0..nranges {
        offs.push(acc);
        for r in 0..runs.len() {
            let (b0, r0) = bounds[r * nb + p];
            let (b1, r1) = bounds[r * nb + p + 1];
            acc += (b1 - b0) - if o.fmt == Fmt::Bin { 0 } else { 3 * (r1 - r0) };
        }
    }
    offs.push(acc);

    // Each worker holds one stream per run, so both buffer memory and open
    // file descriptors scale with workers x runs. With many runs that would
    // blow the budget (and RLIMIT_NOFILE), so cap the worker count instead of
    // letting the buffers shrink below something useful.
    const MIN_BUF: usize = 64 << 10;
    let nofile = {
        let mut rl = libc::rlimit { rlim_cur: 1024, rlim_max: 1024 };
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) };
        rl.rlim_cur as usize
    };
    let by_mem = o.memory / (runs.len() * MIN_BUF).max(1);
    let by_fd = (nofile.saturating_sub(16) * 3 / 4) / runs.len().max(1);
    let mthreads = o.threads.min(by_mem.max(1)).min(by_fd.max(1));
    let bufsize =
        (o.memory / (mthreads * runs.len()).max(1) / 2).clamp(MIN_BUF, 4 << 20).next_power_of_two();
    if mthreads < o.threads {
        eprintln!(
            "  merge: {mthreads} workers (not {}) to keep {} runs x {} buffers inside the budget",
            o.threads,
            runs.len(),
            fmt_size(bufsize)
        );
    }

    let edges: Mutex<Vec<(u64, Vec<u8>)>> = Mutex::new(Vec::new());

    // Merge one range out of every run. With `out` it writes at that offset;
    // without, it only measures, which is how --unique learns each range's
    // size before anything is placed (dedup makes the byte count impossible to
    // predict from the boundaries alone).
    let run_range = |p: usize, out: Option<(&File, u64)>| -> (u64, u64) {
        let mut streams: Vec<Stream> = Vec::with_capacity(runs.len());
        for (r, run) in runs.iter().enumerate() {
            let (b0, _) = bounds[r * nb + p];
            let (b1, _) = bounds[r * nb + p + 1];
            if let Some(s) = Stream::new(&run.path, b0, b1, bufsize) {
                streams.push(s);
            }
        }
        if streams.is_empty() {
            return (0, 0);
        }
        let mut heap: Vec<(u64, usize)> = (0..streams.len()).map(|i| (streams[i].pre(), i)).collect();
        for i in (0..heap.len() / 2).rev() {
            sift_down(&streams, &mut heap, i);
        }
        let mut w = out.map(|(f, off)| DirectWriter::new(f, off, wbuf, o.direct, o.fmt, Some(wsem)));
        // every copy of a value lands in the same range (ranges are cut by
        // value), so comparing against the previous record emitted here is
        // enough -- no cross-range coordination needed
        let mut prev: Vec<u8> = Vec::new();
        let mut first = true;
        let (mut bytes, mut kept) = (0u64, 0u64);
        while !heap.is_empty() {
            let i = heap[0].1;
            let dup = o.unique && !first && prev.as_slice() == streams[i].rec();
            if !dup {
                if let Some(w) = w.as_mut() {
                    w.push(streams[i].rec());
                }
                bytes += o.fmt.rec_len(streams[i].len) as u64;
                kept += 1;
                if o.unique {
                    prev.clear();
                    prev.extend_from_slice(streams[i].rec());
                    first = false;
                }
            }
            if streams[i].advance() {
                heap[0].0 = streams[i].pre();
                sift_down(&streams, &mut heap, 0);
            } else {
                let last = heap.pop().unwrap();
                if !heap.is_empty() {
                    heap[0] = last;
                    sift_down(&streams, &mut heap, 0);
                }
            }
        }
        if let Some(w) = w {
            let e = w.finish();
            if !e.is_empty() {
                edges.lock().unwrap().extend(e);
            }
        }
        (bytes, kept)
    };

    // sizes: known from the boundaries unless dedup is dropping records, in
    // which case a measuring pass over the runs settles them exactly
    let (sizes, counts): (Vec<u64>, Vec<u64>) = if o.unique {
        let bytes: Vec<AtomicUsize> = (0..nranges).map(|_| AtomicUsize::new(0)).collect();
        let recs: Vec<AtomicUsize> = (0..nranges).map(|_| AtomicUsize::new(0)).collect();
        parallel_for(nranges, mthreads, |p| {
            let (b, r) = run_range(p, None);
            bytes[p].store(b as usize, Relaxed);
            recs[p].store(r as usize, Relaxed);
        });
        (
            bytes.iter().map(|b| b.load(Relaxed) as u64).collect(),
            recs.iter().map(|r| r.load(Relaxed) as u64).collect(),
        )
    } else {
        let mut sz = Vec::with_capacity(nranges);
        let mut ct = Vec::with_capacity(nranges);
        for p in 0..nranges {
            sz.push(offs[p + 1] - offs[p]);
            let mut c = 0u64;
            for r in 0..runs.len() {
                c += bounds[r * nb + p + 1].1 - bounds[r * nb + p].1;
            }
            ct.push(c);
        }
        (sz, ct)
    };

    let mut final_offs = Vec::with_capacity(nranges + 1);
    let (mut acc, mut nrecs) = (0u64, 0u64);
    for p in 0..nranges {
        final_offs.push(acc);
        acc += sizes[p];
        nrecs += counts[p];
    }
    final_offs.push(acc);
    let total = acc;

    let fout = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(if o.direct { libc::O_DIRECT } else { 0 })
        .open(&o.output)
        .unwrap_or_else(|e| die(&format!("{}: {e}", o.output)));
    unsafe { libc::fallocate(fout.as_raw_fd(), 0, 0, total as libc::off_t) };

    parallel_for(nranges, mthreads, |p| {
        let (b, _) = run_range(p, Some((&fout, final_offs[p])));
        debug_assert_eq!(b, sizes[p]);
    });

    write_edges(&o.output, &edges.into_inner().unwrap());
    fout.set_len(total).unwrap();
    if o.sync {
        fout.sync_data().unwrap_or_else(|e| die(&format!("fsync: {e}")));
    }
    (total, nrecs)
}

/// Set of 128-bit record hashes, with a "seen" mark per slot. Used to check
/// set equality for --unique, where a multiset digest cannot work.
struct HashSet128 {
    slots: Vec<(u64, u64)>,
    seen: Vec<u64>,
    mask: usize,
}
impl HashSet128 {
    fn new(n: usize) -> HashSet128 {
        let slots = (n.max(1) * 2).next_power_of_two();
        HashSet128 { slots: vec![(0, 0); slots], seen: vec![0u64; slots / 64 + 1], mask: slots - 1 }
    }
    fn bytes(n: usize) -> usize {
        (n.max(1) * 2).next_power_of_two() * 17
    }
    fn key(b: &[u8]) -> (u64, u64) {
        let h1 = hash_bytes(b);
        (if h1 == 0 { 1 } else { h1 }, hash_bytes_seeded(b, 0x51ed_2701_1cb3_e7a9))
    }
    fn insert(&mut self, b: &[u8]) {
        let k = Self::key(b);
        let mut i = k.0 as usize & self.mask;
        loop {
            if self.slots[i].0 == 0 {
                self.slots[i] = k;
                return;
            }
            if self.slots[i] == k {
                return;
            }
            i = (i + 1) & self.mask;
        }
    }
    /// Returns (present, newly marked).
    fn mark(&mut self, b: &[u8]) -> (bool, bool) {
        let k = Self::key(b);
        let mut i = k.0 as usize & self.mask;
        loop {
            if self.slots[i].0 == 0 {
                return (false, false);
            }
            if self.slots[i] == k {
                let (w, bit) = (i / 64, 1u64 << (i % 64));
                let fresh = self.seen[w] & bit == 0;
                self.seen[w] |= bit;
                return (true, fresh);
            }
            i = (i + 1) & self.mask;
        }
    }
}

/// Check that the output is sorted and holds exactly the input's lines.
fn verify(input: &str, output: &str, in_fmt: Fmt, fmt: Fmt, unique: bool) {
    let threads = default_threads();
    let fin = File::open(input).unwrap_or_else(|e| die(&format!("{input}: {e}")));
    let flen = fin.metadata().unwrap().len() as usize;
    let inp = map_input(&fin, flen);
    let a = match in_fmt {
        Fmt::Text => digest_text(inp.as_slice(), threads),
        Fmt::Bin => {
            let mut d = Digest::default();
            for_each_record(inp.as_slice(), |r| d.add(r)).unwrap_or_else(|e| die(&e));
            d
        }
    };

    let fo = File::open(output).unwrap_or_else(|e| die(&format!("{output}: {e}")));
    let olen = fo.metadata().unwrap().len() as usize;
    let om = map_input(&fo, olen);
    let mut b = Digest::default();
    let mut bad: Option<u64> = None;
    let mut dups = 0u64;
    let mut prev: Vec<u8> = Vec::new();
    let mut check = |rec: &[u8], d: &mut Digest| {
        d.add(rec);
        if bad.is_none() && prev.as_slice() > rec {
            bad = Some(d.lines);
        }
        if d.lines > 1 && prev.as_slice() == rec {
            dups += 1;
        }
        prev.clear();
        prev.extend_from_slice(rec);
    };
    match fmt {
        Fmt::Bin => for_each_record(om.as_slice(), |rec| check(rec, &mut b)).unwrap_or_else(|e| die(&e)),
        Fmt::Text => {
            let d = om.as_slice();
            for_each_line(d, 0, d.len(), |off, len| check(&d[off..off + len], &mut b))
        }
    }

    a.show("input ");
    b.show("output");
    match bad {
        Some(i) => println!("NOT SORTED: record {i} is smaller than its predecessor"),
        None => println!("sorted: yes"),
    }

    let mut full_check = true;
    let ok = if unique {
        // a multiset digest cannot work once duplicates are dropped, so check
        // set equality directly: every output record is distinct (strictly
        // increasing, below), every input line is in the output, and the
        // output has no record the input lacks
        if dups > 0 {
            println!("NOT UNIQUE: {dups} output records repeat their predecessor");
        }
        let need = HashSet128::bytes(b.lines as usize);
        if need > mem_available() / 2 {
            println!(
                "set equality not checked: it needs {} of hash table, over half of MemAvailable",
                fmt_size(need)
            );
            full_check = false;
            dups == 0 && bad.is_none()
        } else {
            let mut set = HashSet128::new(b.lines as usize);
            match fmt {
                Fmt::Bin => for_each_record(om.as_slice(), |r| set.insert(r)).unwrap(),
                Fmt::Text => {
                    let d = om.as_slice();
                    for_each_line(d, 0, d.len(), |off, len| set.insert(&d[off..off + len]))
                }
            }
            let (mut missing, mut distinct) = (0u64, 0u64);
            let mut check = |rec: &[u8]| {
                let (present, fresh) = set.mark(rec);
                if !present {
                    missing += 1;
                } else if fresh {
                    distinct += 1;
                }
            };
            match in_fmt {
                Fmt::Bin => for_each_record(inp.as_slice(), |r| check(r)).unwrap(),
                Fmt::Text => {
                    let d = inp.as_slice();
                    for_each_line(d, 0, d.len(), |off, len| check(&d[off..off + len]))
                }
            }
            println!(
                "distinct input lines: {distinct}, output records: {}, input lines not in output: {missing}",
                b.lines
            );
            missing == 0 && distinct == b.lines && dups == 0 && bad.is_none()
        }
    } else {
        if a != b {
            println!("MISMATCH: output is not a permutation of the input");
        }
        a == b && bad.is_none()
    };

    if ok {
        if unique && !full_check {
            println!("OK as far as checked: output is sorted with no duplicates");
        } else if unique {
            println!("OK: output is the input's distinct lines in byte order");
        } else {
            println!("OK: output is the input's lines in byte order");
        }
    } else {
        std::process::exit(1);
    }
}
