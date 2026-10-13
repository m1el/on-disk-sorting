#!/bin/bash
# Same input (shuffled [u32 len][bytes]), same output format, same budget.
# Everything on tmpfs so the QLC drive is out of the picture.
IN=/var/data/bigsort/100m.bin
P=/var/data/bigsort/target/release/pr159
S=/var/data/bigsort/target/release/sortbin
T=/dev/shm/b159; rm -rf $T; mkdir -p $T
cat $IN > /dev/null   # warm the input identically for both
for M in 8G 1G 256M; do
  echo "######## budget $M"
  rm -f $T/*
  printf "PR159 (16 KiB buffers, no-dedup): "
  /usr/bin/time -f "wall %e s, peak RSS %M kB" $P --memory $M --no-dedup --tmp $T $IN $T/pr.bin 2>&1 | tr '\n' ' '; echo
  rm -f $T/pathmap-sort-*
  printf "PR159 (1 MiB buffers, no-dedup):  "
  /usr/bin/time -f "wall %e s, peak RSS %M kB" $P --memory $M --buffer 1M --no-dedup --tmp $T $IN $T/pr2.bin 2>&1 | tr '\n' ' '; echo
  rm -f $T/pathmap-sort-*
  printf "PR159 (16 KiB buffers, dedup):    "
  /usr/bin/time -f "wall %e s, peak RSS %M kB" $P --memory $M --tmp $T $IN $T/prd.bin 2>&1 | tr '\n' ' '; echo
  rm -f $T/pathmap-sort-* $T/prd.bin
  printf "sortbin (64 threads):             "
  /usr/bin/time -f "wall %e s, peak RSS %M kB" $S --in bin --memory $M --tmp $T --buffered --no-sync $IN $T/my.bin 2>&1 | grep -vE "^  run" | tr '\n' ' '; echo
  printf "sortbin (1 thread):               "
  /usr/bin/time -f "wall %e s, peak RSS %M kB" $S --in bin --memory $M --threads 1 --tmp $T --buffered --no-sync $IN $T/my1.bin 2>&1 | grep -vE "^  run" | tr '\n' ' '; echo
  cmp -s $T/pr.bin $T/my.bin && echo "  outputs identical" || echo "  OUTPUTS DIFFER"
  cmp -s $T/pr.bin $T/pr2.bin || echo "  (buffer size changed PR output?!)"
  rm -f $T/*
done
rm -rf $T
