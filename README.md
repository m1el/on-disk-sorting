# bigsort

Tools for turning very large newline-separated files into length-prefixed
record streams — `[u32 little-endian length][bytes]...` — as fast as the
hardware allows.

| crate | what it does |
|-------|--------------|
| [`shufbin`](shufbin/README.md) | shuffles the lines uniformly at random |
| [`sortbin`](sortbin/README.md) | sorts the lines in byte order, with a RAM budget |
| `bigcore` | shared infrastructure both are built on |
| `pr159` | standalone port of [PathMap PR #159](https://github.com/Adam-Vandervorst/PathMap/pull/159)'s external sort, for benchmarking against `sortbin` |

```
cargo build --release

./target/release/shufbin input.txt shuffled.bin
./target/release/sortbin --memory 8G input.txt sorted.bin
./target/release/sortbin --memory 8G --text input.txt sorted.txt

./target/release/shufbin verify input.txt shuffled.bin   # permutation?
./target/release/sortbin verify input.txt sorted.bin     # permutation and sorted?
```

Both read raw bytes (no UTF-8 assumption), keep empty lines as zero-length
records, keep a final line that has no newline, and never store the newline
itself. Both are Linux-only: they lean on `O_DIRECT`, `MADV_POPULATE_READ`,
`MADV_HUGEPAGE` and `fallocate`.

## What `bigcore` carries

- a dynamic-queue thread pool (`parallel_for`, `parallel_for_with`) — no rayon
- mmap plus parallel page-cache population; demand paging a cold file from
  many threads defeats the kernel's readahead heuristic and costs ~2.5x
- `DirectWriter`, a streaming O_DIRECT appender that keeps file offsets and
  buffer addresses in the same alignment, and hands back the sub-block head
  and tail of each range for one buffered pass at the end
- a counting semaphore for write concurrency: this NVMe sustains ~5 GiB/s with
  2–32 concurrent O_DIRECT writers and ~1 GiB/s with 64
- hugepage-aligned anonymous arenas — a region straddling a transparent
  hugepage it does not fill makes the kernel zero two 2 MiB pages instead of
  one, which more than doubled one phase's cost before it was fixed
- the SIMD line scanner and an order-independent multiset digest, so both
  tools' `verify` can prove the output holds exactly the input's lines

## Measurements, honestly

Numbers in the per-tool READMEs come from a 32-core Threadripper 3970X with a
Crucial P3 Plus 4 TB NVMe, after `fstrim`.

That last part matters more than anything else here. Without TRIM, this drive's
sustained write rate had collapsed from 3.4 GB/s to ~104 MB/s by the middle of
the session, which made a 74-second sort take 514 seconds. The cause is not
heat (the controller sat at 57 °C) and not wear: deleting a file tells the
filesystem the space is free but never tells the drive, so after ~1 TiB of
benchmark writes the controller believed every block it had ever written was
still live, leaving it no free blocks to write into and no room for the
pseudo-SLC cache that gives a QLC drive its burst speed. `fstrim` released
1.2 TiB of stale mappings and restored full speed instantly.

`shufbin/cold.sh` does the `fstrim` + drop-caches dance before a timed run.
