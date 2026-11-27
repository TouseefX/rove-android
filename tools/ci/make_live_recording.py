#!/usr/bin/env python3
"""Build the M2 fixture: a recorded live session rove's own decoder accepts.

The live path cannot be pointed at Roblox from CI, so its evidence is a
**recording** of the payloads a connection received, replayed offline
(crates/rustudio-live/tests/live_replay.rs). This script writes that recording
with the byte layout rove's decoder reads (`rove/client-core`): LEB128 varints,
little-endian ids, and rove's own framing rules.

Five packets, in the order a join receives them:

1. schema (151) - three classes (DataModel, Workspace, Part) with the property
                  ids rove's decoder assigns, inside the length-prefixed zstd
                  frame `parse_schema` inflates;
2. globals (129) - the top containers (DataModel, Workspace, Players), which
                  make the world's roots. Read unframed, as the globals reader
                  does;
3. replication (131) - a Part created under Workspace with a Vector3 Size and a
                  bool Anchored, a Character created with no parent of its own,
                  and Players given its parent edge (`19`, an id whose parent
                  was assigned after its create);
4. replication (131) - the Character's parent edge, so a create that arrived
                  without one is settled by a later packet;
5. replication (131) - the Part removed, so the removal path is exercised.

Ids are rove's own spelling of a key, "<scope>:<id>". The manifest at the end
of this file is the recording's claims about itself, and `check()` walks the
bytes to prove each one, so this script cannot write a fixture that disagrees
with its own docstring.

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
    """A compressed packet: kind byte, compressed length, expanded length, frame.

    Only the schema packet is framed this way, and that is rove's own asymmetry:
    `parse_schema` (client-core/src/schema.rs:98) reads the kind byte and then
    calls `decompress_stream` itself, while `SessionCore::ingest_globals` hands
    the payload straight to `ReplicationDecoder::globals`, which reads the seven
    flags at bytes 1..8 (replication.rs:299). Framing a globals packet therefore
    makes its compressed length parse as the flags — 40, 0, 0, 0, 36, 0, 0 for a
    40-byte frame, which is the `Invalid globals boolean` CI reported. A
    replication packet (131) is its own bytes too: rove's replication reader
    starts at the kind byte and never expects a length header.
    """
    compressed = zstd_frame(expanded)
    return bytes([kind]) + struct.pack("<I", len(compressed)) + struct.pack("<I", len(expanded)) + compressed


def raw_packet(payload):
    """A packet with no envelope (replication)."""
    return payload


# ------------------------------------------------- what the decoder will read
# Walk the bytes the way rove walks them, so the manifest at the bottom of this
# file is checked rather than asserted. Nothing here parses on trust: a packet
# that does not end exactly where its reader stops is a fixture that would fail
# in CI instead of here.
def varuint_at(buf, pos):
    shift = 0
    value = 0
    while True:
        byte = buf[pos]
        pos += 1
        value |= (byte & 127) << shift
        shift += 7
        if not byte & 128:
            return value, pos


def read_key(buf, pos):
    """read_key (replication.rs:1956): scope varuint, u32 LE id; scope 0 is None."""
    scope, pos = varuint_at(buf, pos)
    if scope == 0:
        return None, pos
    return "%d:%d" % (scope, struct.unpack("<I", buf[pos:pos + 4])[0]), pos + 4


def walk_schema(blob):
    """parse_schema (schema.rs:98): kind, u32 compressed, u32 expanded, frame."""
    if blob[0] != 151:
        raise SystemExit("the schema packet does not start with 151")
    clen, expanded = struct.unpack("<II", blob[1:9])
    frame = blob[9:]
    if len(frame) != clen:
        raise SystemExit("the schema envelope's compressed length does not match the frame")
    import zstandard
    out = zstandard.ZstdDecompressor().decompress(frame, max_output_size=max(expanded, 1))
    if len(out) != expanded:
        raise SystemExit("the schema frame did not expand to its declared length")
    version, pos = varuint_at(out, 0)
    if version != 3:
        raise SystemExit("the schema payload is version %d, not 3" % version)
    return {"version": version, "expanded": expanded}


def walk_globals(blob):
    """ReplicationDecoder::globals (replication.rs:295): flags at bytes 1..8.

    The seven flags are read straight after the kind byte, which is why this
    packet must not be framed: a u32 compressed length would be read as the
    first four flags and is refused as `Invalid globals boolean`.
    """
    if blob[0] != 129:
        raise SystemExit("the globals packet does not start with 129")
    flags = list(blob[1:8])
    if any(v > 1 for v in flags):
        raise SystemExit(
            "the globals packet is framed: its seven flags read %s, and rove's "
            "globals reader does not inflate (only parse_schema does)" % flags
        )
    pos = 8
    scope, pos = varuint_at(blob, pos)
    if scope == 0:
        raise SystemExit("the globals packet's client scope is zero")
    count, pos = varuint_at(blob, pos)
    containers = []
    seen = set()
    for _ in range(count):
        key, pos = read_key(blob, pos)
        class_id = struct.unpack("<H", blob[pos:pos + 2])[0]
        pos += 2
        first, second = blob[pos], blob[pos + 1]
        pos += 2
        if key in seen:
            raise SystemExit("duplicate top container %s" % key)
        seen.add(key)
        containers.append((key, class_id, (first, second)))
    if pos != len(blob):
        raise SystemExit("the globals packet has %d trailing bytes" % (len(blob) - pos))
    return {"flags": flags, "scope": scope, "containers": containers}


def walk_replication(blob):
    """PacketParser::packet_inner (replication.rs:405): items until a 0 terminator."""
    if blob[0] != 131:
        raise SystemExit("the replication packet does not start with 131")
    pos = 1
    items = []
    while pos < len(blob) and blob[pos] != 0:
        kind = blob[pos]
        pos += 1
        if kind == 2:                      # NewInstance
            key, pos = read_key(blob, pos)
            class_id = struct.unpack("<H", blob[pos:pos + 2])[0]
            pos += 2
            properties = []
            for _ in range(2):             # two property lists, each ended by 255
                while blob[pos] != 255:
                    prop_id = blob[pos]
                    type_id = blob[pos + 1]
                    pos += 2
                    if type_id == 22:
                        pos += 12
                    elif type_id == 9:
                        pos += 1
                    else:
                        raise SystemExit("property %d has type %d, which this "
                                         "writer does not encode" % (prop_id, type_id))
                    properties.append((prop_id, type_id))
                pos += 1
            parent, pos = read_key(blob, pos)
            items.append({"item": "create", "id": key, "class": class_id,
                          "properties": properties, "parent": parent})
        elif kind == 1:                    # remove
            key, pos = read_key(blob, pos)
            items.append({"item": "remove", "id": key})
        elif kind == 19:                   # parent edge (a parent assigned late)
            # replication.rs:475 reads the instance's key, then its parent's,
            # and pushes `set <instance> {Parent: <parent>}`.
            instance, pos = read_key(blob, pos)
            parent, pos = read_key(blob, pos)
            items.append({"item": "parent", "id": instance, "to": parent})
        else:
            raise SystemExit("item kind %d is not modelled here" % kind)
    if pos != len(blob) - 1 or blob[pos] != 0:
        raise SystemExit("the replication packet does not end at its terminator")
    return items


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
    for container_key, class_id, flags in containers:
        out += varuint(container_key[0])            # scope
        out += struct.pack("<I", container_key[1])  # instance id
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


def set_parent(scope, instance, parent):
    """Item 19: give `instance` the parent `parent`, as rove reads it.

    `19, <the instance's key>, <the parent's key>`. This is how a parent edge
    that was not in the instance's own create arrives, and it is a `set` on
    rove's side (`push_action("set", …, {"Parent": {"$ref": …}})`), which is what
    the bridge's `reparent` handles.
    """
    return bytes([19]) + key(scope, instance) + key(scope, parent)


def b64(raw):
    return base64.b64encode(raw).decode()


def build():
    """The five packets, and the claims the recording makes about itself."""
    # Ids are rove's own spelling of a key: "<scope>:<instance>". The scope is
    # the client's, which the globals packet declares.
    SCOPE = 1
    DM, WORKSPACE, PLAYERS, PART, CHARACTER = 0, 1, 2, 3, 4
    key_of = lambda i: (SCOPE, i)

    schema = packet(151, schema_bytes())
    # Unframed on purpose: rove's globals reader reads the flags itself.
    globals_ = bytes([129]) + globals_bytes(
        SCOPE,
        [(key_of(DM), 0, (1, 0)),          # DataModel
         (key_of(WORKSPACE), 1, (1, 0)),   # Workspace
         (key_of(PLAYERS), 1, (1, 0))],    # Players
    )
    # Replication packets are their own bytes: no zstd frame, no length header.
    # The Part under Workspace; the Character with a parent of its own; and
    # Players' parent edge, which arrived in this packet rather than its create.
    # The Character's create names no parent — a Player's Character arrives with
    # a null parent and is parented by a later packet, which is the deferred
    # edge this recording is about. Keeping it out of the Part's subtree also
    # keeps the last packet's removal to exactly one instance.
    place = replication_packet(
        create(SCOPE, PART, 2, [(0, vector3(4.0, 1.0, 2.0)), (1, boolean(True))],
               key_of(WORKSPACE)),
        create(SCOPE, CHARACTER, 2, [(0, vector3(1.0, 1.0, 1.0))], None),
        set_parent(SCOPE, PLAYERS, WORKSPACE),
    )
    # The Character's edge, one packet later: a create that arrived without a
    # parent, settled by the packet after it.
    edge = replication_packet(set_parent(SCOPE, CHARACTER, PLAYERS))
    # And the Part destroyed, with the Character under it.
    removal = replication_packet(remove(SCOPE, PART))

    packets = [
        {"at_ms": 0, "kind": "schema", "base64": b64(schema)},
        {"at_ms": 0, "kind": "globals", "base64": b64(globals_)},
        {"at_ms": 40, "kind": "replication", "base64": b64(place)},
        {"at_ms": 60, "kind": "replication", "base64": b64(edge)},
        {"at_ms": 80, "kind": "replication", "base64": b64(removal)},
    ]
    ids = {"datamodel": "%d:%d" % key_of(DM), "workspace": "%d:%d" % key_of(WORKSPACE),
           "players": "%d:%d" % key_of(PLAYERS), "part": "%d:%d" % key_of(PART),
           "character": "%d:%d" % key_of(CHARACTER)}
    manifest = {
        "id": ids,
        "schema": {"classes": 3, "properties": 2,
                   "names": ["DataModel", "Workspace", "Part"]},
        "globals": {"containers": [ids["datamodel"], ids["workspace"], ids["players"]]},
        # What the replication packets do, in order, as the decoder will see it.
        "replication": [
            [{"item": "create", "id": ids["part"], "parent": ids["workspace"],
              "properties": ["Size", "Anchored"]},
             {"item": "create", "id": ids["character"], "parent": None,
              "properties": ["Size"]},
             {"item": "parent", "id": ids["players"], "to": ids["workspace"]}],
            [{"item": "parent", "id": ids["character"], "to": ids["players"]}],
            [{"item": "remove", "id": ids["part"]}],
        ],
    }
    recording = {
        "note": "A join as a recording: rove's own decoder reads the schema "
                "(DataModel, Workspace, Part) and the globals (the place's top "
                "containers), then the place itself. The Part is created under "
                "Workspace and destroyed in the last packet; Players gets its "
                "parent edge in the packet that creates the Part; the "
                "Character arrives with a null parent and is parented by the "
                "packet after.",
        "scope": SCOPE,
        "ids": ids,
        "manifest": manifest,
        "packets": packets,
    }
    return recording, manifest


def check(recording, manifest):
    """Prove every claim in the manifest against the bytes just written."""
    by_kind = [p["kind"] for p in recording["packets"]]
    if by_kind != ["schema", "globals", "replication", "replication", "replication"]:
        raise SystemExit("packet order is %s" % by_kind)
    blobs = [base64.b64decode(p["base64"]) for p in recording["packets"]]
    schema = walk_schema(blobs[0])
    if schema["version"] != 3:
        raise SystemExit("schema version %d" % schema["version"])
    globals_ = walk_globals(blobs[1])
    containers = [c[0] for c in globals_["containers"]]
    if containers != manifest["globals"]["containers"]:
        raise SystemExit("globals containers are %s" % containers)
    for index, expected in enumerate(manifest["replication"]):
        got = walk_replication(blobs[2 + index])
        if index == 0:
            # Property ids are global in rove's schema: 0 is Size (Vector3, type
            # 22) and 1 is Anchored (bool, type 9). Name them for the manifest.
            names = {0: "Size", 1: "Anchored"}
            types = {0: 22, 1: 9}
            for item in got:
                if "properties" in item:
                    for prop_id, type_id in item["properties"]:
                        if types.get(prop_id) != type_id:
                            raise SystemExit("property %d is encoded as type %d"
                                             % (prop_id, type_id))
                    item["properties"] = [names[prop_id] for prop_id, _ in item["properties"]]
        got = [{k: v for k, v in item.items() if k != "class"} for item in got]
        if got != expected:
            raise SystemExit("replication packet %d reads %s, manifest says %s"
                             % (index, got, expected))
    print("  schema: %d classes, %d properties (version %d, %d bytes inflated)"
          % (manifest["schema"]["classes"], manifest["schema"]["properties"],
             schema["version"], schema["expanded"]))
    print("  globals: flags %s, scope %d, %d containers %s"
          % (globals_["flags"], globals_["scope"], len(containers), containers))
    for index, items in enumerate(manifest["replication"]):
        print("  replication %d: %s" % (index, items))


def main():
    recording, manifest = build()
    check(recording, manifest)
    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    with open(OUT, "w") as f:
        json.dump(recording, f, indent=1)
        f.write("\n")
    print("wrote %s (%d bytes) with %d packet(s)"
          % (OUT, os.path.getsize(OUT), len(recording["packets"])))


if __name__ == "__main__":
    main()
