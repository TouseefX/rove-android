#!/usr/bin/env python3
"""Re-emit a failing build log as GitHub annotations.

Job logs live behind a storage host this project's CI cannot fetch back, but
annotations come through the checks API, so a build failure can be read from
them. Two things shaped this script:

* A step is limited to **ten** error annotations, and anything past the tenth is
  silently dropped. So paging through a log from several `annotate.py` calls in
  one step does not work: only the first call's output survives. One call, ten
  annotations, and the one that matters most is first.
* Annotation *messages* can be long (tens of kilobytes) and may span lines.
  So the failure itself is emitted as a single multi-line annotation rather
  than as ten separate lines of it.

Hence: annotation 1 is the failure region verbatim — a panic with its left and
right, a `FAILED` test with its output, or a cross-check's `DISAGREE` block —
and 2..10 are compiler diagnostics, which is what a compile failure is made of.

    python3 tools/ci/annotate.py out/diagnostics.log

`page` (optional, default 0) offsets the *diagnostic* list, for the rare log
with more than nine errors worth reading one build at a time.
"""

import re
import sys

path = sys.argv[1] if len(sys.argv) > 1 else "/tmp/build.log"
page = int(sys.argv[2]) if len(sys.argv) > 2 else 0
PER_PAGE = 9
REGION_CHARS = 4000

short = re.compile(
    r"^(?P<file>[^:\s][^:]*?):(?P<line>\d+):(?P<col>\d+):\s+"
    r"(?:error|warning)(?:\[\w+\])?:\s*(?P<msg>.*)$"
)
plain = re.compile(r"^\s*(?:error|warning)(?:\[\w+\])?:\s*(?P<msg>\S.*)$")

ANSI = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")


def is_warning(line):
    """`warning: ...` at the start, or `: warning` mid-line (cargo's short
    format). The trailing `warning: \\`crate\\` (lib) generated 2 warnings` summary
    is the one that used to be misreported as an error."""
    stripped = line.strip()
    return stripped.startswith("warning: ") or ": warning" in line


# Where a failure starts, most specific first. A cross-check that exits after
# printing `DISAGREE` has no panic and no compiler error, and used to fall
# through to "here are the dependency warnings", which says nothing.
ANCHORS = (
    "panicked at",
    "--- FAILED",
    "FAILED",
    "DISAGREE",
    "thread 'main'",
    "error[E",
    "Error:",
)


def anchor_of(lines):
    """The line the failure starts at: the earliest of the strongest anchor."""
    best = None
    for index, line in enumerate(lines):
        stripped = line.strip()
        for order, needle in enumerate(ANCHORS):
            if needle in stripped:
                if best is None or order < best[0]:
                    best = (order, index)
                break
    return None if best is None else best[1]


try:
    with open(path, errors="replace") as f:
        text = f.read()
except OSError as e:
    print("annotate: cannot read %s: %s" % (path, e))
    sys.exit(0)

lines = [ANSI.sub("", line).rstrip() for line in text.splitlines()]

# Diagnostics, errors first: warnings are real but they are not why the job
# stopped.
diagnostics = []
for i, line in enumerate(lines):
    m = short.match(line)
    if m:
        detail = m.group("msg")
        # The line after a short diagnostic usually names the help or the note;
        # keep it, it is the part that says what to do.
        if i + 1 < len(lines) and lines[i + 1].strip().startswith(("= help", "= note")):
            detail += " — " + lines[i + 1].strip()
        level = "warning" if is_warning(line) else "error"
        diagnostics.append((level, m.group("file"), m.group("line"), detail))
        continue
    m = plain.match(line)
    if m and not line.strip().startswith("="):
        level = "warning" if is_warning(line) else "error"
        diagnostics.append((level, None, None, m.group("msg")))

errors = [d for d in diagnostics if d[0] == "error"]


def failure_region():
    """The lines that say what went red, as one block."""
    at = anchor_of(lines)
    if at is not None:
        block = [line for line in lines[at : at + 24]]
        # A panic's left/right and a DISAGREE's two packets follow the anchor;
        # stop at the blank line that ends the test's output.
        kept, blanks = [], 0
        for line in block:
            # cargo's own bookkeeping marks the end of this step's account.
            if "process didn't exit successfully" in line or line.startswith("Process completed"):
                break
            if not line.strip():
                blanks += 1
                if blanks >= 2:
                    break
                continue
            blanks = 0
            kept.append(line)
        return kept
    # No anchor: the last real thing the step printed, warnings excluded.
    tail = [line for line in lines if line.strip() and not is_warning(line)]
    return tail[-12:]


region = failure_region()
text_region = "\n".join(region)[:REGION_CHARS]
if not text_region:
    text_region = "(the log ended without a diagnostic line)"
print("annotate: %d error(s), %d warning(s) in %s" % (
    len(errors), len(diagnostics) - len(errors), path))


def escape(text):
    return text.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


# 1. The failure itself, whole.
print("::error title=the failure::%s" % escape(text_region))

# 2..10. Compiler diagnostics, nine of them.
shown = 0
for level, file, line, msg in errors[page * PER_PAGE : (page + 1) * PER_PAGE]:
    msg = msg[:600]
    if file and line:
        print("::%s file=%s,line=%s::%s" % (level, file, line, escape(msg)))
    else:
        print("::%s::%s" % (level, escape(msg)))
    shown += 1

remaining = len(errors) - (page + 1) * PER_PAGE
if remaining > 0:
    print("::error title=%d more error(s)::re-run with page %d" % (remaining, page + 1))
elif not errors and diagnostics:
    print("::notice::the only diagnostics were %d warning(s)" % len(diagnostics))
