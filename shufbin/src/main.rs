//! shufbin — shuffle a newline-separated file into a `[u32 len][bytes]...`
//! record stream, as fast as the machine allows.
//!
//! Pipeline (every phase is multi-threaded over a dynamic work queue):
//!
//!   1. load     mmap the input and pull it into the page cache with parallel
//!               MADV_POPULATE_READ (see bigcore::prefault).
//!   2. scatter  one SIMD newline scan; each line becomes a packed u64
//!               descriptor (offset | len<<40) appended to one of B randomly
//!               chosen buckets. Per-thread write-combining buffers keep the
//!               scatter cache-local, and each bucket's output byte total is
//!               accumulated on the way, so the output layout is known for
//!               free and no second pass over the index is needed.
//!   3. write    per bucket: Fisher-Yates over its descriptors (a bucket is
//!               sized to stay cache resident), gather the records and write
//!               them at the bucket's prefix-summed offset with O_DIRECT.
//!
//! Random bucket assignment, a full shuffle inside each bucket, and
//! concatenating the buckets in index order give a uniformly random
//! permutation: conditioned on the bucket sizes, every arrangement of the
//! lines is equally likely.
//!
//! Memory: 8 bytes per line, plus page cache for the input.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::Instant;

use bigcore::*;

/// Below this, hugepage rounding of the arena would waste more than it saves.
const HUGE_ARENA_MIN: usize = 256 << 20;
/// How far ahead the gather pulls record addresses into the cache.
const PREFETCH: usize = 16;
/// Descriptors buffered per bucket per thread before a flush into the arena:
/// 32 x 8 B = one 256 B burst per bucket. The whole table should fit in L2.
const LOCAL_DEFAULT: usize = 32;

struct Opts {
    input: String,
    output: String,
    seed: u64,
    threads: usize,
    lines_per_bucket: usize,
    local: usize,
    write_threads: usize,
    direct: bool,
    prefault: bool,
    sync: bool,
    no_write: bool,
    quiet: bool,
}

fn usage() -> ! {
    eprintln!(
        "usage: shufbin [options] <input.txt> <output.bin>\n\
         \x20      shufbin verify <input.txt> <output.bin>\n\n\
         Shuffles the lines of <input.txt> uniformly at random and writes them as a\n\
         stream of records: a 4-byte little-endian length followed by that many bytes\n\
         of line content (no newline). Lines are raw bytes; empty lines and a final\n\
         line without a newline are both kept. Needs ~8 bytes of RAM per line.\n\n\
         options:\n\
         \x20 --seed <n>              rng seed (default: random per run)\n\
         \x20 --threads <n>           worker threads (default: all cores)\n\
         \x20 --lines-per-bucket <n>  shuffle granularity (default 262144, the minimum)\n\
         \x20 --local <n>             descriptors buffered per bucket per thread (32)\n\
         \x20 --write-threads <n>     concurrent writes allowed (default min(threads,16))\n\
         \x20 --buffered              write via the page cache instead of O_DIRECT\n\
         \x20 --no-prefault           don't pre-populate the page cache\n\
         \x20 --no-sync               don't fsync the output before exiting\n\
         \x20 --no-write              gather but discard the output (benchmarking)\n\
         \x20 --quiet                 no timings on stderr\n\n\
         verify re-reads both files and compares an order-independent digest of the\n\
         two line multisets."
    );
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("verify") {
        if args.len() != 3 {
            usage();
        }
        verify(&args[1], &args[2]);
        return;
    }

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    let mut o = Opts {
        input: String::new(),
        output: String::new(),
        seed: splitmix(nanos) ^ splitmix(std::process::id() as u64),
        threads: default_threads(),
        lines_per_bucket: HUGE / 8,
        local: LOCAL_DEFAULT,
        write_threads: 0,
        direct: true,
        prefault: true,
        sync: true,
        no_write: false,
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
            "--seed" => o.seed = val().parse().unwrap_or_else(|_| usage()),
            "--threads" => o.threads = val().parse().unwrap_or_else(|_| usage()),
            "--lines-per-bucket" => o.lines_per_bucket = val().parse().unwrap_or_else(|_| usage()),
            "--local" => o.local = val().parse().unwrap_or_else(|_| usage()),
            "--write-threads" => o.write_threads = val().parse().unwrap_or_else(|_| usage()),
            "--buffered" => o.direct = false,
            "--no-prefault" => o.prefault = false,
            "--no-sync" => o.sync = false,
            "--no-write" => o.no_write = true,
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
    // A bucket holding less than one hugepage of descriptors wastes most of
    // the hugepage the kernel zeroes for it, which costs far more than the
    // extra locality buys back.
    o.lines_per_bucket = o.lines_per_bucket.max(HUGE / 8).next_power_of_two();
    o.local = o.local.clamp(4, 1024);
    if o.write_threads == 0 {
        o.write_threads = o.threads.min(16);
    }
    o.write_threads = o.write_threads.clamp(1, o.threads);
    run(&o);
}

macro_rules! phase {
    ($o:expr, $t:expr, $name:literal) => {
        if !$o.quiet {
            eprintln!("{:<9} {:>8.3}s", $name, $t.elapsed().as_secs_f64());
        }
        $t = Instant::now();
    };
}

#[allow(unused_assignments)] // the last phase! resets the timer
fn run(o: &Opts) {
    let t0 = Instant::now();
    let mut t = Instant::now();

    let fin = File::open(&o.input).unwrap_or_else(|e| die(&format!("{}: {e}", o.input)));
    let flen = fin.metadata().unwrap().len() as usize;
    if flen as u64 > OFF_MASK {
        die("input larger than 1 TiB (the descriptor offset field is 40 bits)");
    }
    if flen == 0 {
        File::create(&o.output).unwrap_or_else(|e| die(&format!("{}: {e}", o.output)));
        if !o.quiet {
            eprintln!("0 lines");
        }
        return;
    }
    let input = map_input(&fin, flen);
    if o.prefault {
        prefault(&input, 0, flen, o.threads);
    }
    let data = input.as_slice();
    phase!(o, t, "load");

    // --- scan + scatter -----------------------------------------------------
    let est = estimate_lines(data);
    let nbuckets = (est / o.lines_per_bucket)
        .next_power_of_two()
        .clamp((o.threads * 4).next_power_of_two(), 1 << 20);
    let shift = 64 - nbuckets.trailing_zeros();
    // Worst case is a file of empty lines: one line per input byte. Plus 10
    // sigma of multinomial spread, so an unlucky bucket still fits.
    let mean = flen / nbuckets + 1;
    let cap = mean + 10 * (mean as f64).sqrt() as usize + 1024;
    let huge = est * 8 >= HUGE_ARENA_MIN;
    let cap = (cap * 8).next_multiple_of(if huge { HUGE } else { ALIGN }) / 8;
    let arena = Arena::new(nbuckets * cap, huge);
    let base = Shared(arena.ptr);
    let cursors: Vec<AtomicUsize> = (0..nbuckets).map(|_| AtomicUsize::new(0)).collect();
    let obytes: Vec<AtomicU64> = (0..nbuckets).map(|_| AtomicU64::new(0)).collect();
    let seed = o.seed;
    let local = o.local;
    let nchunks = flen.div_ceil(SCAN_CHUNK);

    // Per-thread write-combining buffers, held for the worker's whole lifetime
    // (flushing them per chunk would cost an atomic per bucket per chunk).
    struct Local<'a> {
        buf: Vec<u64>,
        cnt: Vec<u32>,
        bytes: Vec<u64>,
        local: usize,
        cursors: &'a [AtomicUsize],
        obytes: &'a [AtomicU64],
        base: Shared<u64>,
        cap: usize,
    }
    impl Local<'_> {
        #[inline(always)]
        fn flush(&mut self, b: usize) {
            let n = self.cnt[b] as usize;
            if n == 0 {
                return;
            }
            let at = self.cursors[b].fetch_add(n, Relaxed);
            if at + n > self.cap {
                die("bucket overflow; rerun with a different --seed");
            }
            unsafe {
                std::ptr::copy_nonoverlapping(
                    self.buf.as_ptr().add(b * self.local),
                    self.base.ptr().add(b * self.cap + at),
                    n,
                );
            }
            self.cnt[b] = 0;
        }
    }
    impl Drop for Local<'_> {
        fn drop(&mut self) {
            for b in 0..self.cnt.len() {
                self.flush(b);
                let by = self.bytes[b];
                if by != 0 {
                    self.obytes[b].fetch_add(by, Relaxed);
                }
            }
        }
    }

    parallel_for_with(
        nchunks,
        o.threads,
        || Local {
            buf: vec![0u64; nbuckets * local],
            cnt: vec![0u32; nbuckets],
            bytes: vec![0u64; nbuckets],
            local,
            cursors: &cursors,
            obytes: &obytes,
            base,
            cap,
        },
        |l, c| {
            let mut rng = splitmix(seed ^ splitmix(c as u64 + 0x9e37_79b9));
            let b = c * SCAN_CHUNK;
            let e = (b + SCAN_CHUNK).min(flen);
            for_each_line(data, b, e, |off, len| {
                if len as u64 > MAX_LEN {
                    die("line longer than 16 MiB");
                }
                let bk = (wyrand(&mut rng) >> shift) as usize;
                let n = l.cnt[bk] as usize;
                unsafe {
                    *l.buf.get_unchecked_mut(bk * local + n) = desc(off, len);
                    *l.bytes.get_unchecked_mut(bk) += (len + 4) as u64;
                }
                l.cnt[bk] = (n + 1) as u32;
                if n + 1 == local {
                    l.flush(bk);
                }
            });
        },
    );
    let sizes: Vec<usize> = cursors.iter().map(|c| c.load(Relaxed)).collect();
    let nlines: usize = sizes.iter().sum();
    phase!(o, t, "scatter");

    // --- shuffle each bucket, gather its records, write it ------------------
    let mut offs = Vec::with_capacity(nbuckets + 1);
    let mut acc = 0usize;
    for b in 0..nbuckets {
        offs.push(acc);
        acc += obytes[b].load(Relaxed) as usize;
    }
    let total = acc;
    offs.push(total);

    // --no-write must not touch the output path at all, so that benchmarking a
    // gather can't clobber a previous run's result
    let out_path: &str = if o.no_write { "/dev/null" } else { &o.output };
    let fout = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(!o.no_write)
        .custom_flags(if o.direct && !o.no_write { libc::O_DIRECT } else { 0 })
        .open(out_path)
        .unwrap_or_else(|e| die(&format!("{out_path}: {e}")));
    if !o.no_write {
        unsafe { libc::fallocate(fout.as_raw_fd(), 0, 0, total as libc::off_t) };
    }

    let edges: Mutex<Vec<(u64, Vec<u8>)>> = Mutex::new(Vec::new());
    let wsem = Semaphore::new(o.write_threads);

    parallel_for(nbuckets, o.threads, |b| {
        let n = sizes[b];
        let s = unsafe { std::slice::from_raw_parts_mut(base.ptr().add(b * cap), n) };

        let mut rng = splitmix(seed ^ splitmix(((b as u64) << 32) | 0x5bf0_3635));
        for i in (1..n).rev() {
            let j = bounded(&mut rng, (i + 1) as u64) as usize;
            s.swap(i, j);
        }

        if o.no_write {
            // still touch every record, so --no-write measures the same gather
            let mut sink = 0u64;
            for (i, &d) in s.iter().enumerate() {
                if let Some(&nd) = s.get(i + PREFETCH) {
                    unsafe { prefetch(data.as_ptr().add(desc_off(nd))) };
                }
                let (doff, dlen) = (desc_off(d), desc_len(d));
                sink = sink.wrapping_add(data[doff] as u64).wrapping_add(dlen as u64);
            }
            std::hint::black_box(sink);
            return;
        }

        let mut w =
            DirectWriter::new(&fout, offs[b] as u64, 4 << 20, o.direct, Fmt::Bin, Some(&wsem));
        for (i, &d) in s.iter().enumerate() {
            if let Some(&nd) = s.get(i + PREFETCH) {
                unsafe { prefetch(data.as_ptr().add(desc_off(nd))) };
            }
            let (doff, dlen) = (desc_off(d), desc_len(d));
            w.push(&data[doff..doff + dlen]);
        }
        debug_assert_eq!(w.pos(), offs[b + 1] as u64);
        let e = w.finish();
        if !e.is_empty() {
            edges.lock().unwrap().extend(e);
        }
    });

    if !o.no_write {
        write_edges(&o.output, &edges.into_inner().unwrap());
        fout.set_len(total as u64).unwrap();
        if o.sync {
            fout.sync_data().unwrap_or_else(|e| die(&format!("fsync: {e}")));
        }
    }
    phase!(o, t, "write");

    if !o.quiet {
        let secs = t0.elapsed().as_secs_f64();
        let gib = |x: usize| x as f64 / (1u64 << 30) as f64;
        eprintln!(
            "{:<9} {:>8.3}s  {nlines} lines, {nbuckets} buckets, {:.2} GiB in -> {:.2} GiB out, \
             {:.2} GiB/s",
            "total",
            secs,
            gib(flen),
            gib(total),
            gib(flen + total) / secs,
        );
    }
}

#[inline(always)]
unsafe fn prefetch(p: *const u8) {
    #[cfg(target_arch = "x86_64")]
    core::arch::x86_64::_mm_prefetch(p as *const i8, core::arch::x86_64::_MM_HINT_T0);
    #[cfg(not(target_arch = "x86_64"))]
    let _ = p;
}

/// Compare an order-independent digest of the two line multisets.
fn verify(input: &str, output: &str) {
    let threads = default_threads();
    let fin = File::open(input).unwrap_or_else(|e| die(&format!("{input}: {e}")));
    let flen = fin.metadata().unwrap().len() as usize;
    let inp = map_input(&fin, flen);
    let a = digest_text(inp.as_slice(), threads);

    let fo = File::open(output).unwrap_or_else(|e| die(&format!("{output}: {e}")));
    let olen = fo.metadata().unwrap().len() as usize;
    let om = map_input(&fo, olen);
    let mut b = Digest::default();
    let mut sorted = true;
    let mut prev: Vec<u8> = Vec::new();
    for_each_record(om.as_slice(), |rec| {
        b.add(rec);
        if sorted && prev.as_slice() > rec {
            sorted = false;
        }
        prev.clear();
        prev.extend_from_slice(rec);
    })
    .unwrap_or_else(|e| die(&e));

    a.show("input ");
    b.show("output");
    if a == b {
        println!("OK: output is a permutation of the input (multiset digest matches)");
        if b.lines > 1 && sorted {
            println!("warning: output is in sorted order");
        }
    } else {
        println!("MISMATCH");
        std::process::exit(1);
    }
}
