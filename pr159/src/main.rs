//! Standalone port of PathMap PR #159's `sort_paths`, for benchmarking against
//! `sortbin` on identical input.
//!
//!   https://github.com/Adam-Vandervorst/PathMap/pull/159
//!   src/paths_serialization_sort.rs
//!
//! The algorithm is copied as literally as it can be: the same `read_path` /
//! `write_path`, the same `Chunk` with its `memory/3` byte arena and
//! `memory/6/size_of::<(usize,usize)>()` entry limit, the same
//! `sort_unstable_by` on byte slices, the same dedup-on-spill, the same 2-way
//! `merge`, the same binary-carry `add_run` over 64 levels, the same final
//! cascade and the same temp-file cleanup.
//!
//! What changed, and only this:
//!
//!   * Input is read with the PR's own `read_path` instead of
//!     `for_each_deserialized_path`, and output is written with its own
//!     `write_path` instead of `serialize_paths_from_funcs`. Both ends are
//!     therefore the PR's own run format — `[u32 le len][bytes]` — which is
//!     also what sortbin reads and writes, so neither side pays for a codec
//!     the other does not have.
//!   * `--no-dedup` makes the spill and merge keep duplicates, so the two
//!     implementations can be compared doing the same amount of work
//!     (sortbin produces a permutation; the PR produces a set).
//!   * `--buffer` exposes the PR's `BUFFER` constant (16 KiB) so the effect of
//!     buffer size can be separated from the effect of the algorithm.
//!   * Timing/stat output on stderr.

use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

fn new_run(temp_dir: &Path, next_run: &mut u64) -> io::Result<(PathBuf, File)> {
    let path = temp_dir.join(format!("pathmap-sort-{next_run}"));
    let file = File::create_new(&path)?;
    *next_run += 1;
    Ok((path, file))
}

// Runs are uncompressed length-prefixed paths, avoiding repeated compression
// during merges. These functions share no codec state or in-memory path index.
fn read_path(source: &mut impl Read, path: &mut Vec<u8>, limit: usize) -> io::Result<bool> {
    let mut length = [0; 4];
    loop {
        match source.read(&mut length[..1]) {
            Ok(0) => return Ok(false),
            Ok(_) => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    source.read_exact(&mut length[1..])?;
    let length = u32::from_le_bytes(length) as usize;
    if length > limit {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "path exceeds sort memory budget"));
    }
    path.resize(length, 0);
    source.read_exact(path)?;
    Ok(true)
}

fn write_path(target: &mut impl Write, path: &[u8]) -> io::Result<()> {
    let length = u32::try_from(path.len())
        .map_err(|_| io::Error::other("path exceeds .paths length limit"))?;
    target.write_all(&length.to_le_bytes())?;
    target.write_all(path)
}

struct Chunk {
    bytes: Vec<u8>,
    entries: Vec<(usize, usize)>,
    byte_limit: usize,
    entry_limit: usize,
    dedup: bool,
    buffer: usize,
}

impl Chunk {
    fn new(memory: usize, dedup: bool, buffer: usize) -> Self {
        let byte_limit = memory / 3;
        let entry_limit = memory / 6 / std::mem::size_of::<(usize, usize)>();
        Self {
            bytes: Vec::with_capacity(byte_limit),
            entries: Vec::with_capacity(entry_limit),
            byte_limit,
            entry_limit,
            dedup,
            buffer,
        }
    }
    fn fits(&self, path: &[u8]) -> bool {
        self.bytes.len() + path.len() <= self.byte_limit && self.entries.len() < self.entry_limit
    }
    fn push(&mut self, path: &[u8]) {
        self.entries.push((self.bytes.len(), path.len()));
        self.bytes.extend_from_slice(path);
    }
    fn spill(&mut self, temp_dir: &Path, next_run: &mut u64) -> io::Result<PathBuf> {
        let bytes = &self.bytes;
        self.entries
            .sort_unstable_by(|&(a, al), &(b, bl)| bytes[a..a + al].cmp(&bytes[b..b + bl]));
        let (path, file) = new_run(temp_dir, next_run)?;
        let mut writer = BufWriter::with_capacity(self.buffer, file);
        let mut previous = None;
        for &(start, len) in &self.entries {
            let path = &bytes[start..start + len];
            if !self.dedup || previous != Some(path) {
                write_path(&mut writer, path)?;
            }
            previous = Some(path);
        }
        writer.flush()?;
        drop(writer);
        self.entries.clear();
        self.bytes.clear();
        Ok(path)
    }
}

fn merge(
    left: PathBuf,
    right: PathBuf,
    temp_dir: &Path,
    max_path: usize,
    next_run: &mut u64,
    dedup: bool,
    buffer: usize,
) -> io::Result<PathBuf> {
    let mut a = BufReader::with_capacity(buffer, File::open(&left)?);
    let mut b = BufReader::with_capacity(buffer, File::open(&right)?);
    let mut ap = Vec::new();
    let mut bp = Vec::new();
    let (path, output) = new_run(temp_dir, next_run)?;
    let mut writer = BufWriter::with_capacity(buffer, output);
    let mut has_a = read_path(&mut a, &mut ap, max_path)?;
    let mut has_b = read_path(&mut b, &mut bp, max_path)?;
    while has_a || has_b {
        let order = match (has_a, has_b) {
            (true, true) => ap.cmp(&bp),
            (true, false) => std::cmp::Ordering::Less,
            _ => std::cmp::Ordering::Greater,
        };
        if dedup {
            write_path(&mut writer, if order.is_le() { &ap } else { &bp })?;
            if order.is_le() {
                has_a = read_path(&mut a, &mut ap, max_path)?;
            }
            if order.is_ge() {
                has_b = read_path(&mut b, &mut bp, max_path)?;
            }
        } else if order.is_le() {
            write_path(&mut writer, &ap)?;
            has_a = read_path(&mut a, &mut ap, max_path)?;
        } else {
            write_path(&mut writer, &bp)?;
            has_b = read_path(&mut b, &mut bp, max_path)?;
        }
    }
    writer.flush()?;
    drop(writer);
    drop((a, b));
    fs::remove_file(left)?;
    fs::remove_file(right)?;
    Ok(path)
}

// Binary carry merging: at most one run per level and no unbounded run list.
#[allow(clippy::too_many_arguments)]
fn add_run(
    mut run: PathBuf,
    levels: &mut [Option<PathBuf>; 64],
    temp_dir: &Path,
    max_path: usize,
    next_run: &mut u64,
    dedup: bool,
    buffer: usize,
) -> io::Result<()> {
    for slot in levels {
        match slot.take() {
            None => {
                *slot = Some(run);
                return Ok(());
            }
            Some(other) => {
                run = merge(other, run, temp_dir, max_path, next_run, dedup, buffer)?
            }
        }
    }
    Err(io::Error::other("too many sort runs"))
}

/// Sort (and optionally deduplicate) a `[u32 le len][bytes]` stream.
fn sort_paths<R: Read, W: Write>(
    mut source: R,
    target: &mut W,
    memory_bytes: usize,
    temp_dir: &Path,
    dedup: bool,
    buffer: usize,
) -> io::Result<u64> {
    if memory_bytes < 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "sort memory budget must be at least 1 MiB",
        ));
    }
    let mut next_run = 0;
    let result = (|| {
        let max_path = memory_bytes / 16;
        let mut chunk = Chunk::new(memory_bytes, dedup, buffer);
        let mut levels = std::array::from_fn(|_| None);
        let mut path = Vec::new();
        let t_in = Instant::now();
        while read_path(&mut source, &mut path, max_path)? {
            if !chunk.fits(&path) {
                add_run(
                    chunk.spill(temp_dir, &mut next_run)?,
                    &mut levels,
                    temp_dir,
                    max_path,
                    &mut next_run,
                    dedup,
                    buffer,
                )?;
            }
            chunk.push(&path);
        }
        if !chunk.entries.is_empty() {
            add_run(
                chunk.spill(temp_dir, &mut next_run)?,
                &mut levels,
                temp_dir,
                max_path,
                &mut next_run,
                dedup,
                buffer,
            )?;
        }
        drop(chunk);
        eprintln!("ingest+runs {:>8.3}s ({next_run} run files so far)", t_in.elapsed().as_secs_f64());

        let t_m = Instant::now();
        let mut final_run = None;
        for run in levels.into_iter().flatten() {
            final_run = Some(match final_run {
                None => run,
                Some(other) => {
                    merge(other, run, temp_dir, max_path, &mut next_run, dedup, buffer)?
                }
            });
        }
        eprintln!("final merge {:>8.3}s", t_m.elapsed().as_secs_f64());

        let t_w = Instant::now();
        let mut source: Box<dyn Read> = match &final_run {
            Some(path) => Box::new(BufReader::with_capacity(buffer, File::open(path)?)),
            None => Box::new(io::empty()),
        };
        let mut count = 0u64;
        let mut path = Vec::new();
        while read_path(&mut source, &mut path, max_path)? {
            write_path(target, &path)?;
            count += 1;
        }
        target.flush()?;
        eprintln!("copy out    {:>8.3}s", t_w.elapsed().as_secs_f64());
        Ok(count)
    })();
    let mut cleanup = Ok(());
    for index in 0..next_run {
        if let Err(error) = fs::remove_file(temp_dir.join(format!("pathmap-sort-{index}"))) {
            if error.kind() != io::ErrorKind::NotFound {
                cleanup = Err(error);
            }
        }
    }
    result.and_then(|stats| cleanup.map(|()| stats))
}

fn parse_size(s: &str) -> Option<usize> {
    let s = s.trim();
    let (num, mult) = match s.chars().last()? {
        'k' | 'K' => (&s[..s.len() - 1], 1u64 << 10),
        'm' | 'M' => (&s[..s.len() - 1], 1u64 << 20),
        'g' | 'G' => (&s[..s.len() - 1], 1u64 << 30),
        _ => (s, 1),
    };
    Some((num.trim().parse::<f64>().ok()? * mult as f64) as usize)
}

fn usage() -> ! {
    eprintln!(
        "usage: pr159 [--memory <size>] [--tmp <dir>] [--no-dedup] [--buffer <size>] \
         <input.bin> <output.bin>\n\n\
         Standalone port of PathMap PR #159's sort_paths. Reads and writes\n\
         [u32 le length][bytes] records."
    );
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (mut memory, mut tmp, mut dedup, mut buffer) = (1usize << 30, String::new(), true, 16 * 1024);
    let mut pos = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].clone();
        let mut val = || {
            i += 1;
            args.get(i).cloned().unwrap_or_else(|| usage())
        };
        match a.as_str() {
            "--memory" => memory = parse_size(&val()).unwrap_or_else(|| usage()),
            "--buffer" => buffer = parse_size(&val()).unwrap_or_else(|| usage()),
            "--tmp" => tmp = val(),
            "--no-dedup" => dedup = false,
            _ if a.starts_with('-') => usage(),
            _ => pos.push(a),
        }
        i += 1;
    }
    if pos.len() != 2 {
        usage();
    }
    if tmp.is_empty() {
        tmp = Path::new(&pos[1]).parent().map_or(".".into(), |p| p.to_string_lossy().into_owned());
    }
    let t0 = Instant::now();
    let input = File::open(&pos[0]).unwrap_or_else(|e| {
        eprintln!("{}: {e}", pos[0]);
        std::process::exit(1)
    });
    let in_len = input.metadata().unwrap().len();
    let output = File::create(&pos[1]).unwrap_or_else(|e| {
        eprintln!("{}: {e}", pos[1]);
        std::process::exit(1)
    });
    let mut w = BufWriter::with_capacity(buffer, output);
    let n = sort_paths(
        BufReader::with_capacity(buffer, input),
        &mut w,
        memory,
        Path::new(&tmp),
        dedup,
        buffer,
    )
    .unwrap_or_else(|e| {
        eprintln!("sort failed: {e}");
        std::process::exit(1)
    });
    w.flush().unwrap();
    let secs = t0.elapsed().as_secs_f64();
    eprintln!(
        "total       {secs:>8.3}s  {n} records out, {:.2} GiB in, {:.2} GiB/s",
        in_len as f64 / (1u64 << 30) as f64,
        in_len as f64 / (1u64 << 30) as f64 / secs
    );
}
