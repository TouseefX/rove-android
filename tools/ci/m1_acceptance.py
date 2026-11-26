#!/usr/bin/env python3
"""M1's acceptance check: rove's fixture world, bridged and rendered.

Reads what the run wrote (`out/`) and fails loudly when the world is not
there, the picture is a stub, or the frame statistics are not numbers. Every
line it prints goes into the CI job's log, which is the only place these
numbers can be read from (an artifact cannot be downloaded back).

    python3 tools/ci/m1_acceptance.py [out-dir]
"""

import glob
import json
import os
import struct
import sys

out = sys.argv[1] if len(sys.argv) > 1 else "out"
failed = []


def check(ok, what):
    print(("PASS  " if ok else "FAIL  ") + what)
    if not ok:
        failed.append(what)


def png_size(path):
    with open(path, "rb") as f:
        head = f.read(33)
    if head[:8] != b"\x89PNG\r\n\x1a\n":
        return None
    return struct.unpack(">II", head[16:24])


print("=== files ===")
for path in sorted(glob.glob(os.path.join(out, "*"))):
    print("%10d  %s" % (os.path.getsize(path), path))

print()
print("=== picture ===")
pngs = sorted(p for p in glob.glob(os.path.join(out, "*.png")) if not p.endswith(".view.png"))
for path in pngs:
    size = png_size(path)
    print("%s: %d bytes, %s" % (path, os.path.getsize(path), "%dx%d" % size if size else "not a PNG"))

print()
print("=== bridge report (Output lines) ===")
report_lines = []
for log in sorted(glob.glob(os.path.join(out, "*.log"))):
    with open(log, errors="replace") as f:
        for line in f:
            if "rustudio-live" in line:
                report_lines.append(line.rstrip())
for line in report_lines:
    print(line)
if not report_lines:
    print("(no report line found)")

result_path = os.path.join(out, "live-world.json")
print()
print("=== --result ===")
if os.path.exists(result_path):
    with open(result_path) as f:
        result = json.load(f)
    print(json.dumps(result, indent=1)[:6000])
else:
    result = {}
    print("no --result file")

live = (result.get("live_world") or {}) if isinstance(result, dict) else {}

print()
print("=== the acceptance checks ===")

# 1. The bridge built rove's world, not an empty document.
check(live.get("nodes") == 7, "snapshot has 7 nodes (got %r)" % (live.get("nodes"),))
check(live.get("instances") == 8, "document has 8 instances incl. the root (got %r)" % (live.get("instances"),))
check(bool(live.get("local_player")), "Players.LocalPlayer is bound")
check(
    (live.get("refs_bound") or 0) >= 2,
    "LocalPlayer and Player.Character are bound (got %r)" % (live.get("refs_bound"),),
)
check(live.get("dangling_refs") == 0, "no ref into nowhere (got %r)" % (live.get("dangling_refs"),))
check(live.get("refused_values") == 0, "no value kept its default (got %r)" % (live.get("refused_values"),))
check(
    (live.get("root_classes") or [])[:1] == ["Workspace"],
    "Workspace is at the root (got %r)" % (live.get("root_classes"),),
)
check(bool(report_lines), "the bridge's report reached the Output")

# 2. The picture: a drawn scene, not a stub.
check(len(pngs) >= 1, "a screenshot was written (got %r)" % (pngs,))
for path in pngs:
    size = os.path.getsize(path)
    check(size > 20_000, "%s is a drawn scene, not an empty frame (%d bytes)" % (path, size))

# 3. Frames were drawn, and their statistics are numbers.
frames = (result.get("frames") or {}) if isinstance(result, dict) else {}
count = frames.get("count", 0) or 0
mean = frames.get("mean_ms") or 0
check(count > 10, "frames drawn (got %r)" % (count,))
check(0 < mean < 1000, "mean frame time is a number: %.2f ms" % (mean if isinstance(mean, (int, float)) else 0))

# 4. Nothing errored.
check((result.get("errors") or 0) == 0, "no error lines in the Output (got %r)" % (result.get("errors"),))

print()
if failed:
    print("%d check(s) failed" % len(failed))
    sys.exit(1)

# The numbers, again as annotations: this job's own log cannot be fetched back
# in every environment, but check-run annotations can, and these are what M1 is
# accepted on. GitHub allows ten notice annotations per step.
report_line = report_lines[0] if report_lines else "(no bridge report line)"
numbers = [
    "bridge: " + report_line.strip(),
    "nodes %s / instances %s / refs_bound %s / dangling %s / refused %s / roots %s"
    % (
        live.get("nodes"),
        live.get("instances"),
        live.get("refs_bound"),
        live.get("dangling_refs"),
        live.get("refused_values"),
        (live.get("root_classes") or [])[:4],
    ),
    "frames %s, mean %.2f ms; png %s"
    % (
        count,
        mean if isinstance(mean, (int, float)) else 0,
        ", ".join("%s (%d bytes)" % (os.path.basename(p), os.path.getsize(p)) for p in pngs),
    ),
]
bench_path = os.path.join(out, "bench.json")
if os.path.exists(bench_path):
    with open(bench_path) as f:
        bench = json.load(f)
    fm = bench.get("frame_ms") or {}
    numbers.append(
        "bench: frames %s, frame_ms median %s p90 %s p99 %s max %s"
        % (bench.get("frames"), fm.get("median"), fm.get("p90"), fm.get("p99"), fm.get("max"))
    )
for line in numbers:
    print("::notice::" + line.replace("%", "%25").replace("\n", "%0A"))
print("all checks passed")
