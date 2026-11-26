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


# The tail starts at the first "panicked at": the assertion's own message (the
# left/right of an assert_eq!) is the line after it, and it is what a reader
# needs. Everything after it is short.
first_panic = next(
    (i for i, line in enumerate(lines) if "panicked at" in line or "FAILED" in line),
    max(0, len(lines) - 120),
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
        found.append(("error", m.group("file"), m.group("line"), detail))
        continue
    m = plain.match(line)
    if m and not line.strip().startswith("="):
        found.append(("error", None, None, m.group("msg")))


# Page 0 is the one that is certainly read, so it carries what makes a failure
# readable: the panic block (a failed assert_eq!'s left/right) or, if there is
# none, the first compiler diagnostics. Later pages are the rest of the
# diagnostics, then the end of the log (a test that panicked prints no
# `file:line:col:` line at all; its FAILED list and "test result:" line are at
# the end of the output).
panic_block = []
if first_panic < len(lines):
    panic_block = [("error", None, None, line[:900])
                   for line in lines[first_panic:]
                   if line.strip() and not test_ok.match(line)][:6]
    found = [d for d in found if "panicked at" not in d[3]]

rest = found
diag_pages = (len(rest) + PER_PAGE - 1) // PER_PAGE
tail = [line for line in lines[first_panic:] if line.strip()][-150:][::-1]

if page == 0:
    page_items = panic_block if panic_block else rest[:PER_PAGE]
    printed = len(page_items) if panic_block else min(len(rest), PER_PAGE)
    if panic_block:
        print("annotate: found %d diagnostic(s); page 0 = where the failure starts" % len(rest))
    else:
        print("annotate: diagnostics page 1 of %d (%d diagnostic(s))" % (max(diag_pages, 1), len(rest)))
    rest = rest[0:] if panic_block else rest[PER_PAGE:]
elif page - 1 < (len(rest) + PER_PAGE - 1) // PER_PAGE:
    page_items = rest[(page - 1) * PER_PAGE : page * PER_PAGE]
    print("annotate: diagnostics page %d" % (page + 1))
else:
    tail_page = page - 1 - (len(rest) + PER_PAGE - 1) // PER_PAGE
    page_items = [
        ("error", None, None, line[:900])
        for line in tail[tail_page * PER_PAGE : (tail_page + 1) * PER_PAGE]
    ]
    print("annotate: end of the log, page %d (%d line(s))" % (tail_page + 1, len(page_items)))

for level, file, line, msg in page_items:
    msg = msg[:900]
    if file and line:
        print("::%s file=%s,line=%s::%s" % (level, file, line, escape(msg)))
    else:
        print("::%s::%s" % (level, escape(msg)))
