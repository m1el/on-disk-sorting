#!/bin/bash
sync
sudo -n fstrim /var/data > /dev/null
sudo -n tee /proc/sys/vm/drop_caches <<< 3 > /dev/null
