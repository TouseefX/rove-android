#!/usr/bin/env python3
"""Build the M2 fixture: a recorded live session rove's own decoder accepts.

The live path cannot be pointed at Roblox from CI, so its evidence is a
**recording** of the payloads a connection received, replayed offline
(crates/rustudio-live/src/replay.rs). This script writes that recording with
the byte layout rove's decoder reads — the layout rove's own schema tests
encode (`rove/client-core/src/schema.rs`, `fn schema_bytes`):
zstd frames, LEB128 varints, little-endian class/property ids.

It writes three packets, in the order a join receives them:

1. schema   - three classes (DataModel, Workspace, Part) with the property ids
              rove's decoder assigns;
2. globals  - the top containers (DataModel, Workspace, Players), which is what
              makes the world's roots;
3. replication - the place itself: a Part created under Workspace, a second Part
              and a Player created with a null parent, then a packet that gives
              both of them their parent (the "arrives before its parent edge"
              case the bridge defers), and finally the removal of the first
              Part (the bridge destroys its subtree and forgets its id).

Every id in this file is a key as rove spells it, "<scope>:<id>"

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
    """The schema packet's expanded payload.

    Three classes, in the order ids are assigned: 0 DataModel, 1 Workspace,
    2 Part. Property ids are global in rove's schema and run on across classes,
    so Part's two are 0 and 1. There is no enum table and no ISR section: the
    reader takes "payload finished" for "no more sections", and an enum table
    would make type id 7 mean "enum" rather than a varint.
    """
    out = bytearray()
    out += varuint(3)                     # schema version
    out += struct.pack("<Q", 0)           # flags
    out += varuint(0)                     # enums
    out += varuint(3)                     # classes
    out += varuint(2)                     # properties, counted over all classes
    out += varuint(0)                     # events

    for name, props in (
        ("DataModel", ()),
        ("Workspace", ()),
        # The names rustudio's api-dump.json spells, so the bridge's converter
        # is handed the real ones. 22 is Vector3 and 9 is bool, the type ids
        # rove's value reader switches on.
        ("Part", (("Size", 22), ("Anchored", 9))),
    ):
        out += string(name)
        out += varuint(len(props))
        for prop_name, type_id in props:
            out += string(prop_name)
            out += bytes([type_id])
        out += struct.pack("<H", 0)       # the class's unknown u16
        out += varuint(0)                 # its events
    return bytes(out)


def globals_bytes(scope, containers):
    """The globals packet: the world's top containers (parent NULL)."""
    out = bytearray()
    out += bytes([1, 1, 0, 0, 0, 0, 0])   # seven boolean flags
    out += varuint(scope)                 # client scope
    out += varuint(len(containers))
    for key, class_id, flags in containers:
        out += varuint(key[0])            # scope
        out += struct.pack("<I", key[1])  # instance id
        out += struct.pack("<H", class_id)
        out += bytes([flags[0], flags[1]])
    return bytes(out)


def key(scope, instance):
    return varuint(scope) + struct.pack("<I", instance)


def create(scope, instance, class_id, properties, parent):
    """One create item: 2, key, class, [prop order, value] ..., 255, 255, parent.

    The two 255s end the instance's two property lists (rove reads both), and
    `parent` is a key or None for "no parent yet".
    """
    out = bytearray([2])
    out += key(scope, instance)
    out += struct.pack("<H", class_id)
    for prop_id, encoded in properties:
        out += bytes([prop_id])
        out += encoded
    out += bytes([255, 255])
    out += key(*parent) if parent else b"\x00"
    return bytes(out)


def vector3(x, y, z):
    return bytes([22]) + struct.pack("<3f", x, y, z)


def boolean(value):
    return bytes([9, 1 if value else 0])


def replication_packet(*items):
    out = bytearray([131])
    for item in items:
        out += item
    out += bytes([0])                     # packet terminator
    return bytes(out)


def remove(scope, instance):
    return bytes([1]) + key(scope, instance)


def b64(raw):
    return base64.b64encode(raw).decode()


def main():
    # The ids are rove's own spelling of a key: "<scope>:<instance>". The scope
    # is the client's, which the globals packet declares.
    SCOPE = 1
    DM, WORKSPACE, PLAYERS, PART, CHARACTER = 0, 1, 2, 3, 4

    schema = packet(151, schema_bytes())
    globals_ = packet(129, globals_bytes(
        SCOPE,
        [((SCOPE, DM), 0, (1, 0)),        # DataModel
         ((SCOPE, WORKSPACE), 1, (1, 0)),  # Workspace
         ((SCOPE, PLAYERS), 1, (1, 0))],   # Players
    ))
    # 3: a Part created under Workspace.
    # 4 and 2: a second Part and a Player created with no parent yet.
    joins = replication_packet(
        create(SCOPE, PART, 2, [(0, vector3(4.0, 1.0, 2.0)), (1, boolean(True))],
               (SCOPE, WORKSPACE)),
        create(SCOPE, CHARACTER, 2, [(0, vector3(1.0, 1.0, 1.0))], None),
    )
    # 5: their parent edges arrive in a later packet, which is the case the
    # bridge has to hold: a child before its parent would be dropped.
    edges = replication_packet(
        create(SCOPE, PLAYERS, 0, [], (SCOPE, WORKSPACE)),
    )
    # 6: the first Part is destroyed (the bridge removes its subtree).
    removals = replication_packet(remove(SCOPE, PART))

    packets = [
        {"at_ms": 0, "kind": "schema", "base64": b64(schema)},
        {"at_ms": 0, "kind": "globals", "base64": b64(globals_)},
        {"at_ms": 40, "kind": "replication", "base64": b64(joins)},
        {"at_ms": 60, "kind": "replication", "base64": b64(edges)},
        {"at_ms": 80, "kind": "replication", "base64": b64(removals)},
    ]
    recording = {
        "note": "A join as a recording: rove's own decoder reads the schema "
                "(DataModel, Workspace, Part), the globals (the place's top "
                "containers), then the place itself — a Part under Workspace, a "
                "Player and a second Part whose parent edges arrive in a later "
                "packet, and the first Part destroyed.",
        "scope": SCOPE,
        "ids": {"datamodel": "%d:%d" % (SCOPE, DM), "workspace": "%d:%d" % (SCOPE, WORKSPACE),
                "players": "%d:%d" % (SCOPE, PLAYERS), "part": "%d:%d" % (SCOPE, PART),
                "character": "%d:%d" % (SCOPE, CHARACTER)},
        "packets": packets,
    }
    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    with open(OUT, "w") as f:
        json.dump(recording, f, indent=1)
        f.write("\n")
    print("wrote %s (%d bytes) with %d packet(s)" % (OUT, os.path.getsize(OUT), len(packets)))


if __name__ == "__main__":
    main()
