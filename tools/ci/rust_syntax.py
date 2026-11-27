#!/usr/bin/env python3
"""Check Rust files for syntax errors with tree-sitter.

No cargo or rustc is reachable from this sandbox (static.rust-lang.org is
blocked), so a real compile is not possible here: CI is the only compiler. What
this *can* catch, before a push costs a CI round trip:

- a missing brace, paren or bracket (the tree comes back with an ERROR node)
- a half-applied edit (an `assert` that replaced text and left a fragment)
- unterminated strings and stray characters

It is not a type checker and does not pretend to be: a name typo still has to
wait for CI. But the failures it does catch are the ones a text edit makes.
"""
import sys
from pathlib import Path

import tree_sitter
import tree_sitter_rust

LANGUAGE = tree_sitter.Language(tree_sitter_rust.language())
PARSER = tree_sitter.Parser(LANGUAGE)


def errors(node, source, out, depth=0):
    """Collect ERROR and MISSING nodes, with the line they start on."""
    if node.type == "ERROR" or node.is_missing:
        start = node.start_point
        end = node.end_point
        text = source[node.start_byte : node.end_byte].decode("utf8", "replace")
        first = text.splitlines()[0] if text else ""
        out.append(
            (
                start[0] + 1,
                end[0] + 1,
                node.type,
                node.is_missing,
                first[:120],
            )
        )
    for child in node.children:
        errors(child, source, out, depth + 1)


def check(path: Path) -> int:
    source = path.read_bytes()
    tree = PARSER.parse(source)
    found = []
    errors(tree.root_node, source, found)
    if not found:
        return 0
    print(f"\n{path}:")
    for line, end_line, kind, missing, text in found:
        note = "missing" if missing else kind
        where = f"line {line}" if line == end_line else f"lines {line}-{end_line}"
        print(f"  {where}: {note}: {text!r}")
    return len(found)


def main(argv):
    paths = [Path(p) for p in argv[1:]]
    if not paths:
        print("usage: rust_syntax.py FILE...", file=sys.stderr)
        return 2
    bad = 0
    checked = 0
    for path in paths:
        if not path.is_file():
            print(f"missing: {path}", file=sys.stderr)
            bad += 1
            continue
        checked += 1
        bad += check(path)
    if bad:
        print(f"\n{bad} problem(s) in {checked} file(s)")
        return 1
    print(f"{checked} file(s), no syntax errors")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
