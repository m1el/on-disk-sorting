# sortbin

External merge sort for a newline-separated file, bounded by a RAM budget.
Writes the lines in byte (memcmp) order as

```
[u32 little-endian length][that many bytes] ...
```

or, with `--text`, as a newline-separated file.

```
sortbin --memory 8G input.txt output.bin
sortbin --memory 8G --text input.txt sorted.txt
sortbin --memory 8G --unique input.txt distinct.bin
sortbin verify [--text] [--unique] input.txt output.bin
```

`--unique` drops duplicates, giving exactly `LC_ALL=C sort -u`.

Lines are raw bytes (no UTF-8 assumption, NUL bytes are fine). Empty lines are
kept as zero-length records, and a final line without a trailing newline is
kept. The order is exactly `LC_ALL=C sort`'s.

## How it works

**Phase 1 — runs.** The input is mmapped and cut into segments that fit the
budget. Each segment is pulled into the page cache in parallel, indexed into
16-byte keys, sorted, and written out as a sorted run.

A key is an 8-byte big-endian prefix of the line plus a packed
(40-bit offset, 24-bit length) descriptor. The prefix means almost every
comparison is settled in registers without touching the line at all; only ties
fall through to a `memcmp` of the remainder. Zero-padding a short line can only
create a tie, never a wrong order, so the fallback keeps it correct.

The sort is a parallel sample sort: splitters drawn from a random sample cut
the keys into P buckets, every key is counted and scattered into its bucket,
and each bucket is then sorted independently. Concatenating the buckets is the
sorted array — there is no merge step and no shared state in the hot loop.

**Dedup (`--unique`).** Duplicates are adjacent once sorted, so a run drops
them with a local comparison against the predecessor key — which works across
chunk boundaries too, since the whole key array is in hand. Cross-run
duplicates go at merge time: every copy of a value lands in the same range,
because ranges are cut by value, so comparing against the last record the
range emitted is sufficient and needs no coordination between workers.

The catch is that dedup makes a range's output size unpredictable, and ranges
are placed at prefix-summed offsets decided before any of them is written. So
with `--unique` the merge runs twice: once measuring, once writing at exact
offsets. That costs one extra read of the runs (about 8% on the 109 M-line
file, where the runs are already deduped by then) and keeps the merge fully
parallel with no extra memory.

An earlier attempt wrote ranges at their un-deduped offsets and compacted the
gaps afterwards with a parallel in-place left shift. That is wrong, and subtly:
once the cumulative savings exceed one range's reserved size, range *p*'s
destination lands on top of range *q<p*'s not-yet-read source. It passed at one
and two workers and corrupted records at four.

**Phase 2 — merge.** Each run carries a sparse index (one entry per 1024
records or 1 MiB, whichever comes first) holding a byte offset, a record index
and a key prefix. Splitters sampled from those entries cut the key space into
ranges; for each (run, splitter) a binary search over the sparse index — probing
the actual record at each step, so it is exact — plus a short bounded scan gives
the byte offset *and* the record index where that range starts in that run.

That makes the merge fully parallel: range sizes are known before any merging,
so each range's output offset is a prefix sum, and one worker merges its range
out of all runs straight into its own slice of the output file with O_DIRECT.
Nothing large is ever materialized, and there is no single-threaded merge
bottleneck.

Knowing the record indices (not just byte offsets) at the boundaries is what
makes `--text` work too: the output is 3 bytes shorter per record than the run
framing, so the offsets stay computable without a counting pass.

## Memory

Set it with `--memory` (default: half of `MemAvailable`), or set the segment
size directly with `--segment`.

A segment costs its own bytes (they are touched while indexing and gathering)
plus **32 bytes per line** — two 16-byte key arrays, since the sample sort
scatters into a second buffer. The segment size is derived from that:

```
segment = memory * avg_line_len / (avg_line_len + 32) * 0.85
```

The average line length comes from sampling, so the budget is approximate
(±~20%); `--segment` is exact if you need a hard bound. The merge adds one
stream buffer per run per worker, sized from the same budget and capped at
4 MiB.

`--memory` is a guide, not a hard cap. The floor is set by three things the
budget cannot shrink: the minimum segment is 1 MiB; the mmapped segment counts
as resident while it is being indexed and gathered; and each merge worker holds
one 64 KiB stream buffer **per run**, so with many runs the merge falls back to
fewer workers (it says so) and can still exceed a very small budget — a 1 MiB
budget on a 63-run merge really used 77 MiB. A 2-way cascading merge would be
bounded by construction; this one trades that for a single merge pass.

Temporary space: the runs together are the size of the output, so sorting
needs output-size free space in `--tmp` (default: the output's directory) on
top of the output itself.

## Correctness

`sortbin verify` re-reads both files and checks two things: the output is
non-decreasing, and its lines are a permutation of the input's (comparing an
order-independent digest — count, bytes, and the sum and xor of per-line
hashes).

With `--unique` a multiset digest cannot work, so it checks set equality
instead: no output record repeats its predecessor, every input line is present
in the output, and the number of distinct input lines equals the output's
record count. That needs a 128-bit hash set over the output, so it is skipped
with a message when the table would take more than half of MemAvailable —
ordering and uniqueness are still checked.

The test corpus is a 66 MB file mixing heavy duplicates, long shared prefixes,
raw bytes including NULs, empty lines and 30 KB lines. Output is byte-identical
to `LC_ALL=C sort` across 15 combinations of `--segment` (64k … 100M, i.e. 1
to 1030 runs) and `--threads` (1, 4, 64), in both input and output formats,
and to `LC_ALL=C sort -u` across the same grid with `--unique`. On the real
109 M-line file, `--unique` output is byte-identical to `sort -u`'s
(109,017,671 distinct lines).

One bug worth recording: zero-padding the 8-byte prefix of an *empty* line
shifted a `u64` by 64, which in release wraps to a shift by 0 and leaked the
following bytes into the key — so empty lines sorted to arbitrary places. The
nasty-corpus test caught it; `checked_shl(..).unwrap_or(0)` fixes it.

## Timing

32-core Threadripper 3970X, 64 threads, NVMe (Crucial P3 Plus 4 TB).

109 M lines, 1.06 GiB in -> 1.37 GiB out:

| configuration | runs | merge | total |
|---|---|---|---|
| default budget (1 segment, all in RAM) | 1.63 s | — | **2.00 s** |
| `--memory 1G` (6 runs, external) | 1.85 s | 1.10 s | **3.31 s** |
| `--text` output | 1.52 s | — | **1.88 s** |
| `--unique`, in RAM, tmpfs | 1.58 s | — | **1.58 s** |
| `--unique`, `--memory 1G`, tmpfs | 2.16 s | 1.17 s | **3.99 s** |
| same three, writing to tmpfs | | | 1.72 / 2.98 / 1.47 s |
| `LC_ALL=C sort -S 8G --parallel=64` (tmpfs) | | | **9.66 s** |
| `LC_ALL=C sort -u -S 8G --parallel=64` (tmpfs) | | | **12.90 s** |

GNU sort's output is byte-identical to `sortbin --text`'s on this file.

1.97 B lines, 20.25 GiB in -> 25.76 GiB out, `--memory 32G` (3 runs):

| phase | time |
|---|---|
| runs (index + sample sort + write 25.76 GiB) | 39.3 s |
| merge (read 25.76 GiB, write 25.76 GiB) | 34.7 s |
| **total** | **74.3 s** |

With `--unique` the same file takes **131 s** (78.7 s runs + 52.0 s merge) and
yields 1,951,034,078 distinct records — the extra time is the measuring pass
plus the dedup comparisons.

### Versus PathMap PR #159

[PR #159](https://github.com/Adam-Vandervorst/PathMap/pull/159) solves the same
problem (external bytewise sort with a memory budget and disk runs) in 235
single-threaded lines. `../pr159` is a faithful standalone port of it — same
`Chunk` limits, same `sort_unstable_by` on byte slices, same dedup-on-spill,
same binary-carry 2-way merge — reading and writing the same
`[u32 len][bytes]` records sortbin does, with `--no-dedup` added so both do the
same work. Its no-dedup output is byte-identical to sortbin's.

109 M records (1.37 GiB), shuffled, everything on tmpfs, wall time / peak RSS:

| budget | PR #159 | sortbin, 1 thread | sortbin, 64 threads |
|---|---|---|---|
| 16 GiB (no merging at all) | 65.6 s / 2.72 GB | 16.6 s / 3.16 GB | **3.2 s** / 4.99 GB |
| 8 GiB | 65.7 s / 2.22 GB | 20.2 s / 3.15 GB | **2.4 s** / 5.00 GB |
| 1 GiB | 65.2 s / 279 MB | 23.3 s / 574 MB | **6.8 s** / 1.10 GB |
| 256 MiB | 63.9 s / **71 MB** | 24.5 s / 151 MB | **7.7 s** / 318 MB |

So: **~4x single-threaded, 8-21x with 64 threads** — and the PR is 2-4x leaner
in memory at every budget, which is what it is optimizing for.

Where the single-threaded 4x comes from, at a 256 MiB budget: the sort side is
16.1 s here against 69.1 s there, while the merge side is 8.4 s against 8.8 s —
near parity. The difference is almost entirely in the chunk sort. Their
comparator dereferences two pointers into the byte arena for every comparison;
the 8-byte prefix carried inline in the key settles most comparisons without
leaving the key array. They also copy every payload byte into the chunk arena,
which their streaming `Read` input forces; sorting offsets into an mmap avoids
that but needs a seekable file.

Two things that are *not* the explanation: their 16 KiB `BUFFER` (raising it to
1 MiB changed nothing, 65.7 s -> 66.6 s, so they are not I/O-bound), and dedup
(it costs them ~2% and removed 0.4% of this dataset).

Their total is also remarkably flat across budgets — 65.7 / 65.2 / 63.9 s —
because a smaller chunk sorts cheaper exactly as fast as the extra merge passes
cost. Mine gets *worse* as the budget tightens, since the merge grows and the
per-worker buffers shrink.

### Read this before trusting any disk benchmark on this box

The same 1.97 B-line sort measured **514 s** earlier in the day — 7x slower,
same code, same data. The drive was the whole difference, and not for the
reason you would guess:

An SSD can only erase in large blocks, so its translation layer needs a pool
of free blocks to write into. Deleting a file tells the *filesystem* the space
is free; the drive never hears about it, so every LBA ever written still looks
live to the controller. After this session's ~1 TiB of benchmark writes the
free pool was gone, every write waited on garbage collection, and — this drive
being QLC — the pseudo-SLC write cache, which is carved out of free blocks,
had nothing to work with either. Sustained writes fell to ~104 MB/s.

`fstrim /var/data` released 1.2 TiB of stale mappings in 92 s and writes went
straight back to 3.3 GB/s with no idle time at all. Measured again: writing
25.76 GiB takes 10.0 s, immediately writing it again takes 31.0 s (the SLC
cache is spent), and another `fstrim` restores the 10.0 s — so on this drive
the binding constraint is free blocks, not recovery time.

So: `sudo fstrim /var/data` before timing anything, and consider mounting with
`discard=async` for a write-heavy workload. The weekly `fstrim.timer` is far
too coarse for a job that writes a terabyte in an afternoon.

## Options

```
--memory <size>     RAM budget, e.g. 8G (default: half of MemAvailable)
--segment <size>    input bytes per run, overriding the budget
--threads <n>       worker threads (default: all cores)
--write-threads <n> concurrent writes allowed (default min(threads,16))
--tmp <dir>         where run files go (default: the output's directory)
--text              write newline-separated text instead of records
--buffered          write via the page cache instead of O_DIRECT
--keep-runs         don't delete the run files
--no-sync           don't fsync the output before exiting
--quiet             no timings on stderr
```

## Limits

- Input up to 1 TiB (40-bit offset field), lines up to 16 MiB (24-bit length).
- Linux only: `O_DIRECT`, `MADV_POPULATE_READ`, `MADV_HUGEPAGE`, `fallocate`.
- The sort is not stable (equal lines are interchangeable, so it does not matter).
