# shufbin

Shuffles the lines of a newline-separated file and writes them as a binary
record stream:

```
[u32 little-endian length][that many bytes] [u32 length][bytes] ...
```

Lines are treated as raw bytes (no UTF-8 assumption). Empty lines are kept as
zero-length records, and a final line without a trailing newline is kept too.
The newline itself is never stored.

```
cargo build --release
./target/release/shufbin input.txt output.bin
./target/release/shufbin verify input.txt output.bin
```

## How it works

Four phases, all multi-threaded over a dynamic work queue (no rayon; just
`std::thread::scope` and an atomic task counter).

1. **load** — `mmap` the input, then pull it into the page cache with parallel
   `MADV_POPULATE_READ`. Leaving this to demand paging is ~2.5x slower on a cold
   file: 64 threads each faulting their own region look nothing like a
   sequential stream to the kernel's readahead heuristic, and the scan
   degenerates into synchronous single-page reads.

2. **scatter** — one SIMD (`memchr`) newline scan. Each line becomes a packed
   `u64` descriptor (40-bit offset | 24-bit length) appended to one of *B*
   randomly chosen buckets. Threads buffer 32 descriptors per bucket locally and
   flush them as one 256-byte burst, so the scatter stays cache-local instead of
   writing single words all over a 16 GiB arena. Each bucket's output byte total
   is accumulated during the same pass, so the output layout is known without a
   second pass over the index.

3. **shuffle** — Fisher-Yates over each bucket's descriptors. Buckets are sized
   (262144 lines ≈ one 2 MiB hugepage of descriptors) to stay cache resident.

4. **write** — gather each bucket's records into a block-aligned buffer and
   `pwrite` it at the bucket's prefix-summed offset with `O_DIRECT`. Buffered
   writes reach about half the device's bandwidth and evict the input from the
   page cache while doing it.

Phases 3 and 4 are fused into one pass over the index.

### Why the result is uniformly random

Assign each line to a bucket independently and uniformly, shuffle each bucket
fully, then concatenate the buckets in index order. Conditioned on the bucket
sizes, every arrangement of the lines is equally likely, and that holds for
every size vector — so the concatenation is a uniformly random permutation.
Checked empirically too: 4000 shuffles of 64 items give χ² = 3997 on 3969
degrees of freedom (z = +0.3) over the full item × position table.

### Things that mattered more than expected

- **Hugepage alignment of the index arena.** A bucket whose region straddled a
  2 MiB transparent hugepage it did not fill made the kernel allocate and zero
  *two* hugepages per bucket. Fixing the alignment (and rounding the per-bucket
  stride to whole hugepages) cut the scatter phase from 6.9 s to 2.9 s on 2 B
  lines. This is also why `--lines-per-bucket` is clamped at 262144: smaller
  buckets waste most of the hugepage the kernel zeroes for them, and 65536
  lines/bucket measured 3x slower overall.
- **`O_DIRECT` for the output**: 36.5 s → 15.5 s on the 2 B-line file.
- **Counting the lines first is unnecessary.** Bucket capacity is reserved for
  the worst case (one line per input byte) with `MAP_NORESERVE`; untouched
  reservation costs address space and nothing else. A sampled estimate picks
  only the bucket count, where being wrong costs a little locality.
- **The gather is the floor.** A global shuffle means one random read per
  record; that phase is memory-latency bound and the `O_DIRECT` writes hide
  completely behind it.

## Timing

32-core Threadripper 3970X, NVMe, 64 threads.

109 M lines, 1.06 GiB in → 1.37 GiB out:

| phase   | cold   | warm   |
|---------|--------|--------|
| load    | 0.58 s | 0.05 s |
| scatter | 0.11 s | 0.11 s |
| write   | 0.40 s | 0.40 s |
| total   | **1.09 s** | **0.56 s** |

1.97 B lines, 20.25 GiB in → 25.76 GiB out:

| phase   | cold   | warm   |
|---------|--------|--------|
| load    | 10.0 s | 0.3 s  |
| scatter | 3.0 s  | 3.2 s  |
| write   | 10.2 s | 10.0 s |
| total   | **23.1 s** | **13.5 s** |

The write phase is gather-bound, not device-bound: `--no-write` takes the same
time, so the `O_DIRECT` writes hide entirely behind the random reads.

Caveat on the big-file numbers: take them after `fstrim`. Untrimmed, this
drive's sustained write rate collapses from 3.4 GB/s to ~104 MB/s and the same
run takes 3x longer — see the "read this before trusting any disk benchmark"
section in [sortbin's README](../sortbin/README.md) for the mechanism.
`cold.sh` here does the `fstrim` + drop-caches dance before a timed run.

## Memory

8 bytes per line for the index, plus page cache for the input. 2 B lines needs
~16 GiB of RAM. The arena's *reservation* is much larger (worst-case sizing) but
only the touched pages are ever committed.

## Options

```
--seed <n>              rng seed (default: random per run)
--threads <n>           worker threads (default: all cores)
--lines-per-bucket <n>  shuffle granularity (default and minimum 262144)
--local <n>             descriptors buffered per bucket per thread (32)
--write-threads <n>     concurrent writes allowed (default min(threads,16));
                        this device sustained ~5 GiB/s with 2-32 concurrent
                        O_DIRECT writers but only ~1 GiB/s with 64
--buffered              write via the page cache instead of O_DIRECT
--no-prefault           don't pre-populate the page cache
--no-sync               don't fsync the output before exiting
--no-write              gather but discard the output (benchmarking)
--quiet                 no timings on stderr
```

`verify` re-reads both files and compares an order-independent digest
(count, bytes, sum and xor of per-line hashes) of the two line multisets.

## Limits

- Input up to 1 TiB (40-bit offset field), lines up to 16 MiB (24-bit length).
- Linux only: `O_DIRECT`, `MADV_POPULATE_READ`, `MADV_HUGEPAGE`, `fallocate`.
