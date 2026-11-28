#!/usr/bin/env python3
"""The UI-glyph rule of `tests/ui_glyphs.rs`, without a build.

That test tokenizes every `src/**.rs` and `crates/*/src/**.rs`, keeps the
non-ASCII characters in string literals (not comments, not char literals, not
anything under `#[cfg(test)]`), and asserts each one can be drawn by the fonts
egui ships plus the two bundled icon fonts. It is the rule that keeps a
`"✓"` from reaching a font that has no such glyph and drawing as a box.

It needs `cargo test`, and the suite runs on macOS — the slowest runner, where
a run is a quarter of an hour before it even starts. A glyph mistake is a
property of the source alone, so this does the same scan in a second: no
toolchain, no fonts beyond the ones already in the tree and in egui's crate.
Same answers, a thousandth of the cost.

Usage: tools/ci/ui_glyphs_preflight.py [--verbose]
Exit status 1 lists the characters no bundled font can draw, with the lines.
"""
from __future__ import annotations

import os
import re
import struct
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

# egui 0.36's default families (epaint_default_fonts), in `bundled_fonts()`
# order, plus the Tabler icon fonts appended after them.
EGUI_FONTS = ("Ubuntu-Light", "NotoEmoji-Regular", "emoji-icon-font", "Hack-Regular")
LOCAL_FONTS = {
    "tabler": "assets/tabler/tabler-icons.ttf",
    "tabler-filled": "assets/tabler/tabler-icons-filled.ttf",
}
PROPORTIONAL = ("Ubuntu-Light", "NotoEmoji-Regular", "emoji-icon-font", "tabler", "tabler-filled", "Hack-Regular")
MONOSPACE = ("Hack-Regular", "Ubuntu-Light", "NotoEmoji-Regular", "emoji-icon-font")

# `emoji-icon-font.ttf` maps a handful of emoji to ligature names; `skrifa`'s
# charmap does not see them, and neither does this parser.
LIGATURES = {
    "\U0001F4C4": "page", "\U0001F4C1": "folder", "\U0001F4CE": "paperclip",
    "\U0001F4CC": "pushpin", "\U0001F4CB": "clipboard", "\U0001F4C3": "page",
    "\U0001F4D1": "pages", "\U0001F4CA": "chart", "\U0001F4C8": "chart",
    "\U0001F50D": "zoom", "\U0001F50E": "zoom", "\U0001F512": "lock",
    "\U0001F513": "lock", "\U0001F517": "link", "\U0001F514": "bell",
    "\U0001F516": "bookmark", "\u2714": "check", "\u2716": "close",
    "\u2702": "scissors", "\U0001F4BE": "save", "\U0001F4F7": "camera",
    "\u23F9": "stop", "\u25B6": "play", "\U0001F3C6": "trophy",
}

# The same exemptions `tests/ui_glyphs.rs` makes, by file and character.
SKIP = {
    ("scripts.rs", "\U0001F98A"), ("writer.rs", "\U0001F98A"),
    ("globals_parity.rs", "\u200d"),
    ("gui.rs", "\u05d5"), ("gui.rs", "\u05dc"), ("gui.rs", "\u05dd"),
    ("gui.rs", "\u05e2"), ("gui.rs", "\u05e9"), ("gui.rs", "\u200d"),
    ("gui.rs", "\ufe0f"), ("gui.rs", "\U0001F6D2"),
    ("runtime.rs", "\u2063"), ("ui_shots.rs", "\u2063"),
    ("clipboard.rs", "\u200b"),
}

STRING = re.compile(r'"((?:[^"\\]|\\.)*)"')
CFG_TEST = re.compile(r"#\[\s*cfg\s*\(\s*test\s*\)\s*\]")
ESCAPES = {"n": "\n", "t": "\t", "r": "\r", "0": "\0", "\\": "\\", '"': '"', "'": "'"}


def unescape(literal: str) -> str:
    out, i = [], 0
    while i < len(literal):
        c = literal[i]
        if c == "\\" and i + 1 < len(literal):
            n = literal[i + 1]
            if n == "u" and literal[i + 2 : i + 3] == "{":
                end = literal.find("}", i + 3)
                out.append(chr(int(literal[i + 3 : end], 16)))
                i = end + 1
                continue
            out.append(ESCAPES.get(n, n))
            i += 2
            continue
        out.append(c)
        i += 1
    return "".join(out)



def blank_cfg_test_items(text: str) -> str:
    """`text` with every `#[cfg(test)]` item's body replaced by spaces.

    The Rust test skips tokens under `#[cfg(test)]` (fixtures use private-use
    codepoints as map keys, and no font is asked to draw them), and commits
    those characters' offsets. Line numbers survive here because the blanks keep
    every newline, so a reported line is still the real one.
    """
    masked = list(text)
    for match in CFG_TEST.finditer(text):
        # The item the attribute applies to: skip further attributes, then find
        # where its body starts and where it ends.
        i = match.end()
        while True:
            while i < len(text) and text[i] in " \t\r\n":
                i += 1
            if text.startswith("#[", i):
                depth = 0
                while i < len(text):
                    if text[i] == "[":
                        depth += 1
                    elif text[i] == "]":
                        depth -= 1
                        if depth == 0:
                            i += 1
                            break
                    i += 1
                continue
            break
        # `use a::b;` and `mod x;` end at the semicolon; anything with a body
        # ends at its closing brace.
        depth, j, end = 0, i, None
        while j < len(text):
            c = text[j]
            if c == '"':
                j += 1
                while j < len(text):
                    if text[j] == "\\":
                        j += 1
                    elif text[j] == '"':
                        break
                    j += 1
            elif c == "{":
                depth += 1
            elif c == "}":
                depth -= 1
                if depth == 0:
                    end = j + 1
                    break
            elif c == ";" and depth == 0:
                end = j + 1
                break
            elif text.startswith("//", j):
                newline = text.find("\n", j)
                j = len(text) if newline < 0 else newline
            j += 1
        if end is None:
            end = len(text)
        for k in range(match.start(), end):
            if masked[k] != "\n":
                masked[k] = " "
    return "".join(masked)


def cmap(path: Path) -> set[int]:
    """The codepoints a TrueType/OpenType font maps, from its cmap table."""
    data = path.read_bytes()
    count = struct.unpack(">H", data[4:6])[0]
    offset = None
    for i in range(count):
        entry = 12 + 16 * i
        if data[entry : entry + 4] == b"cmap":
            offset = struct.unpack(">I", data[entry + 8 : entry + 12])[0]
    if offset is None:
        raise SystemExit(f"{path}: no cmap table")
    subtables = struct.unpack(">H", data[offset + 2 : offset + 4])[0]
    best = None
    for i in range(subtables):
        entry = offset + 4 + 8 * i
        _, _, sub = struct.unpack(">HHI", data[entry : entry + 8])
        fmt = struct.unpack(">H", data[offset + sub : offset + sub + 2])[0]
        if fmt in (4, 12) and (best is None or fmt == 12):
            best = (fmt, offset + sub)
    if best is None:
        raise SystemExit(f"{path}: no format 4 or 12 cmap")
    fmt, start = best
    chars: set[int] = set()
    if fmt == 4:
        seg_x2 = struct.unpack(">H", data[start + 6 : start + 8])[0]
        segments = seg_x2 // 2
        ends = [struct.unpack(">H", data[start + 14 + 2 * i : start + 16 + 2 * i])[0] for i in range(segments)]
        starts_at = start + 16 + seg_x2
        starts = [struct.unpack(">H", data[starts_at + 2 * i : starts_at + 2 * i + 2])[0] for i in range(segments)]
        for first, last in zip(starts, ends):
            chars.update(range(first, last + 1))
    else:
        groups = struct.unpack(">I", data[start + 12 : start + 16])[0]
        for i in range(groups):
            at = start + 16 + 12 * i
            first, last, _ = struct.unpack(">III", data[at : at + 12])
            chars.update(range(first, min(last, 0x10FFFF) + 1))
    return chars


def egui_font_dirs() -> list[Path]:
    """Where egui's own TTFs are, if a registry checkout or a sibling copy is here."""
    candidates = []
    env = os.environ.get("EPAINT_DEFAULT_FONTS")
    if env:
        candidates.append(Path(env))
    home = Path.home() / ".cargo" / "registry" / "src"
    if home.is_dir():
        for entry in home.iterdir():
            for name in ("epaint_default_fonts", "egui_extras"):
                found = entry / name
                if found.is_dir():
                    candidates.append(found)
    return candidates


def source_files() -> list[Path]:
    files: list[Path] = []
    roots = [ROOT / "src"]
    crates = ROOT / "crates"
    if crates.is_dir():
        roots += [c / "src" for c in sorted(crates.iterdir()) if (c / "src").is_dir()]
    for root in roots:
        files += sorted(root.rglob("*.rs"))
    return files


def main() -> int:
    verbose = "--verbose" in sys.argv
    fonts: dict[str, set[int]] = {}
    for name in EGUI_FONTS:
        for directory in egui_font_dirs():
            for candidate in (directory / "fonts", directory):
                path = candidate / f"{name}.ttf"
                if path.is_file():
                    fonts[name] = cmap(path)
                    break
            if name in fonts:
                break
        if name not in fonts:
            print(
                f"notice: {name}.ttf not found (set EPAINT_DEFAULT_FONTS to the\n"
                "        epaint_default_fonts crate directory, or run this in a\n"
                "        checkout with a cargo registry). Its coverage is assumed.",
                file=sys.stderr,
            )
            fonts[name] = set(range(0x110000))  # assume; never a false failure
    for name, rel in LOCAL_FONTS.items():
        fonts[name] = cmap(ROOT / rel)

    def draws(family: str, ch: str) -> bool:
        if family == "emoji-icon-font" and ch in LIGATURES:
            return True
        if not fonts.get(family):
            return True
        return ord(ch) in fonts[family]

    def family_draws(members: tuple[str, ...], ch: str) -> bool:
        return any(draws(name, ch) for name in members)

    # A string literal is UI text; a comment is not, and a `//` inside a string
    # is not a comment. Strip line comments only outside quotes.
    found: dict[str, set[str]] = {}
    for path in source_files():
        text = blank_cfg_test_items(path.read_text(encoding="utf-8", errors="replace"))
        in_block = False
        for number, line in enumerate(text.splitlines(), 1):
            stripped, quoted, i = [], False, 0
            while i < len(line):
                if in_block:
                    end = line.find("*/", i)
                    if end < 0:
                        i = len(line)
                    else:
                        in_block, i = False, end + 2
                    continue
                if not quoted and line.startswith("//", i):
                    break
                if not quoted and line.startswith("/*", i):
                    in_block, i = True, i + 2
                    continue
                c = line[i]
                if c == '"' and line[i - 1 : i] != "\\":
                    quoted = not quoted
                stripped.append(c)
                i += 1
            if quoted and len(stripped) < len(line):
                stripped.append(line[len(stripped) :])  # keep a string running on
            for match in STRING.finditer("".join(stripped)):
                for ch in unescape(match.group(1)):
                    if ord(ch) < 128:
                        continue
                    if (path.name, ch) in SKIP:
                        continue
                    found.setdefault(ch, set()).add(f"{path.relative_to(ROOT)}:{number}")

    missing = {
        ch: where
        for ch, where in found.items()
        if not (family_draws(PROPORTIONAL, ch) and family_draws(MONOSPACE, ch))
    }
    if verbose:
        print(f"scanned {len(source_files())} files, {len(found)} non-ASCII characters in UI text")
    if not missing:
        print(f"ui glyphs: {len(found)} character(s) in UI text, all drawable")
        return 0
    print("Missing bundled UI glyphs:")
    for ch in sorted(missing):
        where = ", ".join(sorted(missing[ch])[:4])
        print(f"  {ch!r} (U+{ord(ch):04X}) {where}")
    print("\nEach needs a bundled font that maps it, a Tabler icon instead, or an")
    print("entry in SKIP (tests/ui_glyphs.rs) with a reason it is never drawn.")
    return 1


if __name__ == "__main__":
    sys.exit(main())
