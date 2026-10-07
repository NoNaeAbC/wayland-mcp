#!/usr/bin/env python3
"""Sample CPU time for named observer processes; never reads frame memory."""
import json
from pathlib import Path
import sys
import time
import os

pids = [int(value) for value in sys.argv[1:]]
if not pids:
    raise SystemExit("supply observer/console process IDs")

def sample(pid):
    fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    # fields start with stat field 3 (state); CPU ticks are fields 14 and 15.
    return int(fields[11]) + int(fields[12])

before = {pid: sample(pid) for pid in pids}
start = time.monotonic()
time.sleep(10)
wall = time.monotonic() - start
ticks_per_second = os.sysconf("SC_CLK_TCK")
results = []
for pid in pids:
    seconds = (sample(pid) - before[pid]) / ticks_per_second
    results.append({"pid": pid, "cpuSeconds": seconds, "oneCorePercent": 100 * seconds / wall})
print(json.dumps({"intervalSeconds": wall, "processes": results}))
