#!/usr/bin/env python3
"""Catch `self.<field>` accesses to fields `Editor` does not have.

Twice now the same class of mistake has cost a CI round: a field belonging to
`JoinWindow` written as `self.live_applied` inside `impl Editor`, because the
live-join state reads as if it were the editor's. A syntax checker cannot see
it and cargo is not reachable from this sandbox, so this is the narrow, cheap
version of what rustc would say.

How it stays honest: the source is walked once with a brace counter, and each
`impl` block's text is extracted by matching braces from its own opening brace.
Fields and `fn` names are collected *per block*, so `impl Game`'s methods are
never credited to `impl Editor`, and a `self.x` is checked against the impl it
is actually inside.
"""
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
# Every file that might hold an `impl Editor`: the editor is spread across a
# dozen modules, and a name declared in one is reachable from all of them.
FILES = sorted(
    str(p.relative_to(ROOT)) for p in (ROOT / "src").glob("**/*.rs")
) + [
    "crates/rustudio-player/src/play.rs",
    "crates/rustudio-player/src/game.rs",
]
TARGET = "Editor"


def strip_code(line: str) -> str:
    """The line without its trailing comment (string-aware enough for source)."""
    out = []
    in_str = False
    escape = False
    for i, ch in enumerate(line):
        if in_str:
            if escape:
                escape = False
            elif ch == "\\":
                escape = True
            elif ch == '"':
                in_str = False
            out.append(ch)
        else:
            if ch == '"':
                in_str = True
            elif ch == "/" and line[i + 1 : i + 2] == "/":
                break
            out.append(ch)
    return "".join(out)


def strip_strings_and_comments(text: str) -> str:
    """Same length as `text`, with string bodies and comments blanked.

    Blanked rather than removed, so byte offsets and brace counting still line
    up with the original.
    """
    out = list(text)
    i = 0
    n = len(text)
    while i < n:
        ch = text[i]
        if ch == "/" and text[i : i + 2] == "//":
            while i < n and text[i] != "\n":
                out[i] = " "
                i += 1
        elif ch == '"':
            out[i] = " "
            i += 1
            while i < n:
                if text[i] == "\\":
                    out[i] = " "
                    if i + 1 < n:
                        out[i + 1] = " "
                    i += 2
                    continue
                if text[i] == '"':
                    out[i] = " "
                    i += 1
                    break
                if text[i] == "\n":
                    break
                out[i] = " "
                i += 1
        else:
            i += 1
    return "".join(out)


def block_after(text: str, brace: int) -> int:
    """Index of the brace closing the one at `brace`."""
    depth = 0
    for i in range(brace, len(text)):
        if text[i] == "{":
            depth += 1
        elif text[i] == "}":
            depth -= 1
            if depth == 0:
                return i
    return len(text)


def collect():
    fields = {}
    methods = {}
    impls = []  # (type, sanitized block text)
    for rel in FILES:
        path = ROOT / rel
        if not path.is_file():
            continue
        raw = path.read_text()
        clean = strip_strings_and_comments(raw)
        for m in re.finditer(r"\bstruct\s+(\w+)\s*\{", clean):
            end = block_after(clean, clean.index("{", m.start()))
            body = clean[clean.index("{", m.start()) : end]
            got = fields.setdefault(m.group(1), set())
            depth = 0
            for line in body.splitlines():
                if depth <= 1:
                    f = re.match(r"\s*(?:pub(?:\([^)]*\))?\s+)?(\w+)\s*:", line)
                    if f:
                        got.add(f.group(1))
                depth += line.count("{") + line.count("(") - line.count("}") - line.count(")")
        # `impl` blocks only where the keyword starts a line's item, which is
        # what keeps `impl Into<String>` in a parameter list out.
        for m in re.finditer(r"(?m)^\s*impl\b([^{\n]*)\{", clean):
            header = m.group(1)
            named = re.search(r"(?:^|\s)(\w+)\s*$", header.strip()) or re.search(
                r"for\s+(\w+)", header
            )
            if not named:
                continue
            name = named.group(1)
            open_brace = clean.index("{", m.start())
            end = block_after(clean, open_brace)
            body = clean[open_brace:end]
            impls.append((name, body))
            methods.setdefault(name, set()).update(re.findall(r"\bfn\s+(\w+)", body))
    return fields, methods, impls


def main():
    fields, methods, impls = collect()
    if TARGET not in fields:
        sys.exit(f"no fields found for {TARGET}")
    known = fields[TARGET] | methods.get(TARGET, set())

    problems = {}
    for name, body in impls:
        if name != TARGET:
            continue
        for m in re.finditer(r"\bself\.(\w+)", body):
            got = m.group(1)
            if got not in known:
                problems.setdefault(got, 0)
                problems[got] += 1

    if not problems:
        print(
            f"{TARGET}: {len(fields[TARGET])} field(s), {len(methods.get(TARGET, ()))} "
            f"method(s); every self.<name> accounted for"
        )
        return 0
    for name, count in sorted(problems.items()):
        print(f"self.{name}: not a field or method of {TARGET} ({count} use(s))")
    print(f"\n{len(problems)} suspect name(s) in impl {TARGET}")
    return 1


if __name__ == "__main__":
    sys.exit(main())
