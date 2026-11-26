#!/usr/bin/env python3
"""Re-emit compiler diagnostics as GitHub annotations.

Job logs live behind a storage host this project's CI cannot fetch back, but
annotations come through the checks API, so a build failure can be read one
error at a time. Ten lines per invocation (a step is limited to ten error
annotations); call it with 0, 1, 2 ... to page through the rest.

    python3 tools/ci/annotate.py /tmp/build.log 0

Works on `--message-format=short` output ("file:line:col: error[E0308]: ...")
and falls back to plain `error: ...` lines.
"""

import re
import sys

path = sys.argv[1] if len(sys.argv) > 1 else "/tmp/build.log"
page = int(sys.argv[2]) if len(sys.argv) > 2 else 0
PER_PAGE = 10

# A `cargo test` failure prints no file:line:col: line at all — the test's name
# is the diagnostic: "test session::tests::wheel_zoom ... FAILED".
test_failed = re.compile(r"^test (?P<name>\S+) \.\.\. FAILED")
test_ok = re.compile(r"^test \S+ \.\.\. ok")

short = re.compile(r"^(?P<file>[^:\s][^:]*?):(?P<line>\d+):(?P<col>\d+):\s+(?:error|warning)(?:\[\w+\])?:\s*(?P<msg>.*)$")
plain = re.compile(r"^\s*(?:error|warning)(?:\[\w+\])?:\s*(?P<msg>\S.*)$")

try:
    with open(path, errors="replace") as f:
        text = f.read()
except OSError as e:
    print("annotate: cannot read %s: %s" % (path, e))
    sys.exit(0)

# cargo runs with `CARGO_TERM_COLOR: always` in this workflow, so every
# diagnostic is wrapped in SGR escapes: "\x1b[1mcrates/x.rs:1:2: \x1b[0m...".
# Parsing has to start from the plain text or nothing matches at all.
ANSI = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")
lines = [ANSI.sub("", line) for line in text.splitlines()]


def escape(text):
    return text.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


# Where the failure starts: a panic (a failed assert_eq!'s left/right is the
# line after it) or a test's FAILED line. With neither, the failure is whatever
# the compiler said, and the diagnostics below are the account of it.
failure_at = next(
    (i for i, line in enumerate(lines) if "panicked at" in line or "FAILED" in line),
    None,
)

found = []
for i, line in enumerate(lines):
    m = short.match(line)
    if m:
        # The line after a short diagnostic usually names the help or the
        # note; keep it, it is the part that says what to do.
        detail = m.group("msg")
        if i + 1 < len(lines) and lines[i + 1].strip().startswith(("= help", "= note")):
            detail += " — " + lines[i + 1].strip()
        level = "warning" if ": warning" in line else "error"
        found.append((level, m.group("file"), m.group("line"), detail))
        continue
    m = plain.match(line)
    if m and not line.strip().startswith("="):
        level = "warning" if ": warning" in line else "error"
        found.append((level, None, None, m.group("msg")))

# A red run is about its errors. Warnings are real but they are not why the job
# stopped, and a page holds ten lines: the errors get the pages first.
errors = [d for d in found if d[0] == "error"]
warnings = [d for d in found if d[0] == "warning"]
rest = errors + warnings
diag_pages = (len(rest) + PER_PAGE - 1) // PER_PAGE

# The end of the log is the last resort: a test that panics prints no
# `file:line:col:` line at all, and its "test result:" line is at the end.
tail = ([line for line in lines[failure_at:] if line.strip()] if failure_at is not None
        else [line for line in lines if line.strip()][-40:])
tail = tail[-150:][::-1]

if page == 0:
    if failure_at is not None:
        # The failure itself, first: whoever reads one annotation reads this.
        page_items = [("error", None, None, line[:900])
                      for line in tail[::-1]
                      if line.strip() and not test_ok.match(line)][:PER_PAGE]
        print("annotate: %d diagnostic(s); page 0 = where the failure starts" % len(rest))
    else:
        page_items = rest[:PER_PAGE]
        print("annotate: diagnostics page 1 of %d (%d)" % (max(diag_pages, 1), len(rest)))
    rest = rest[len(page_items):] if failure_at is None else rest
elif page - 1 < (len(rest) + PER_PAGE - 1) // PER_PAGE:
    page_items = rest[(page - 1) * PER_PAGE : page * PER_PAGE]
    print("annotate: diagnostics page %d" % (page + 1))
else:
    tail_page = page - 1 - (len(rest) + PER_PAGE - 1) // PER_PAGE
    page_items = [("error", None, None, line[:900]) for line in tail[tail_page * PER_PAGE : (tail_page + 1) * PER_PAGE]]
    print("annotate: end of the log, page %d (%d line(s))" % (tail_page + 1, len(page_items)))

for level, file, line, msg in page_items:
    msg = msg[:900]
    if file and line:
        print("::%s file=%s,line=%s::%s" % (level, file, line, escape(msg)))
    else:
        print("::%s::%s" % (level, escape(msg)))
