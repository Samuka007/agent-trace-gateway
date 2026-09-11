#!/bin/sh
# memory.current sampler for the cargo test phase (ticket 02)
i=0
while [ $i -lt 120 ]; do
  echo "$(date +%s) $(cat /sys/fs/cgroup/memory.current)" >> /tmp/memsample.txt
  sleep 2
  i=$((i+1))
done
