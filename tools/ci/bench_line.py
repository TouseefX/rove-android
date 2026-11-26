#!/usr/bin/env python3
"""One line of frame statistics from --bench's JSON, for the CI log.

    python3 tools/ci/bench_line.py out/bench.json
"""

import json
import os
import sys

path = sys.argv[1] if len(sys.argv) > 1 else "out/bench.json"
if not os.path.exists(path):
    print("no bench report at %s" % path)
    sys.exit(1)

with open(path) as f:
    report = json.load(f)

frames = report.get("frame_ms") or {}
work = report.get("work_ms") or {}
gpu = report.get("gpu_frame_ms") or {}
print(
    "frames %s | frame_ms median %s p90 %s p99 %s max %s | work_ms median %s p99 %s"
    % (
        report.get("frames", 0),
        frames.get("median"),
        frames.get("p90"),
        frames.get("p99"),
        frames.get("max"),
        work.get("median"),
        work.get("p99"),
    )
)
if gpu:
    print("gpu_frame_ms median %s p99 %s" % (gpu.get("median"), gpu.get("p99")))
for key in ("gameplay_lua", "physics", "render", "scripts"):
    if key in report:
        print("%s: %s" % (key, json.dumps(report[key])[:600]))
