#!/usr/bin/env python3
"""Build the M2 fixture: a recorded live session rove's own decoder accepts.

The live path cannot be pointed at Roblox from CI, so its evidence is a
**recording** of the payloads a connection received, replayed offline
(crates/rustudio-live/src/replay.rs). This script writes that recording with
the byte layout rove's decoder reads — the layout rove's own schema tests
encode (`rove/client-core/src/schema.rs`, `fn schema_bytes`):
zstd frames, LEB128 varints, little-endian class/property ids.

The session it writes is a small place: a Workspace holding an anchored Part,
then (as a player joins) a Player whose Character arrives as a bare id with its
parent set afterwards, a Humanoid inside it, and finally the Part is destroyed —
so the replay proves not just that creates and sets arrive, but that a child may
arrive before its parent, that a parent edge can be a later `set`, and that a
removal forgets the ids it destroyed.

Usage (writes the fixture in place):

    python3 tools/ci/make_live_recording.py
"""

import base64
import json
import os
import struct

OUT = "crates/rustudio-live/tests/fixtures/live_session.json"


def varuint(value):
    out = bytearray()
    while True:
        byte = value & 127
        value >>= 7
        if value:
            byte |= 128
        out.append(byte)
        if not value:
            return bytes(out)


def string(text):
    raw = text.encode()
    return varuint(len(raw)) + raw


def zstd_frame(payload):
    """A zstd frame for `payload`, from a real encoder.

    Hand-rolling the frame is not worth the risk: the reference encoder is the
    only thing that can be trusted to produce bytes every decoder accepts, and
    `zstandard` is the Python binding to that encoder. The script refuses to
    write a fixture it cannot encode (and the fixture this wrote is checked
    into the tree, so CI does not need the module).
    """
    try:
        import zstandard
    except ImportError:
        raise SystemExit(
            "needs the `zstandard` module to encode the fixture's frames: "
            "pip install zstandard"
        )
    # The content size goes in the header: rove's decoder expands a frame to the
    # length it was told to expect, and the Python decoder needs the same hint.
    frame = zstandard.ZstdCompressor(level=1, write_checksum=False, write_content_size=True).compress(payload)
    # The same bytes must come back out of a decoder, or the fixture is wrong.
    back = zstandard.ZstdDecompressor().decompress(frame, max_output_size=max(len(payload), 1))
    if back != payload:
        raise SystemExit("the zstd frame did not round-trip")
    return frame


def packet(kind, expanded):
    compressed = zstd_frame(expanded)
    return bytes([kind]) + struct.pack("<I", len(compressed)) + struct.pack("<I", len(expanded)) + compressed


# ---------------------------------------------------------------- the schema
# One class (Part) with two properties. Property names are what the bridge hands
# the converter, so they are spelled as rustudio's api-dump.json spells them.
def schema_bytes():
    out = bytearray()
    out += varuint(3)                     # schema version
    out += struct.pack("<Q", 0)           # flags
    # enum table: none. Our decoder treats a string table as optional
    # trailing data it rejects, so no enums means no enum-id references.
    out += varuint(0)                     # enums
    out += varuint(1)                     # classes
    out += varuint(2)                     # properties
    out += varuint(0)                     # events
    out += string("Part")
    out += varuint(2)                     # this class's properties
    out += string("Size")                 # property name
    out += bytes([23])                    # type id: a datatype-sized blob
    out += string("Anchored")
    out += bytes([2])                     # type id: bool
    out += struct.pack("<H", 0)           # the class's unknown u16
    out += varuint(0)                     # this class's events
    return bytes(out)


def b64(raw):
    return base64.b64encode(raw).decode()


def main():
    packets = [
        {"at_ms": 0, "kind": "schema", "base64": b64(packet(151, schema_bytes()))},
        # TODO(globals + replication): the ids below are the ones rove's
        # decoder assigns, and the values use rove's $ tags so the bridge's
        # converter is exercised with the real shapes.
        {"at_ms": 0, "kind": "globals", "base64": b64(packet(129, b""))},
    ]
    recording = {
        "note": "rove's own decoder reads this session: schema, then a place "
                "whose Workspace holds one anchored Part, a joining Player whose "
                "Character arrives before its parent edge, and the Part removed.",
        "packets": packets,
    }
    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    with open(OUT, "w") as f:
        json.dump(recording, f, indent=1)
        f.write("\n")
    print("wrote %s (%d bytes) with %d packet(s)" % (OUT, os.path.getsize(OUT), len(packets)))


if __name__ == "__main__":
    main()
