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

short = re.compile(r"^(?P<file>[^:\s][^:]*?):(?P<line>\d+):(?P<col>\d+):\s+(?:error|warning)(?:\[\w+\])?:\s*(?P<msg>.*)$")
plain = re.compile(r"^\s*(?:error|warning)(?:\[\w+\])?:\s*(?P<msg>\S.*)$")

try:
    with open(path, errors="replace") as f:
        lines = f.read().splitlines()
except OSError as e:
    print("annotate: cannot read %s: %s" % (path, e))
    sys.exit(0)


def escape(text):
    return text.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


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

print("annotate: %d diagnostic(s) in %s (page %d)" % (len(found), path, page))

page_items = found[page * PER_PAGE : (page + 1) * PER_PAGE]
if not page_items:
    # Nothing that looks like a diagnostic: the failure was not one line of
    # rustc output (a cargo manifest error, a linker, a runner). Emit the tail
    # of the log as annotations instead, page by page, so it can be read back.
    tail = [l for l in lines if l.strip()][-40:]
    page_items = [("error", None, None, line[:900]) for line in tail[page * PER_PAGE : (page + 1) * PER_PAGE]]
    print("annotate: no rustc diagnostics; tail page %d of %d line(s)" % (page, len(tail)))
for level, file, line, msg in page_items:
    msg = msg[:900]
    if file and line:
        print("::%s file=%s,line=%s::%s" % (level, file, line, escape(msg)))
    else:
        print("::%s::%s" % (level, escape(msg)))
