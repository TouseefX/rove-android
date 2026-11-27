# The join: allocation, session, and the real thing

M2 has three halves, and they fail in different places — so they are built and
checked separately, and each one says which stage it reached.

| stage | what it is | what proves it | needs a server |
|---|---|---|---|
| **M2a** | the join-game response, parsed: the dial address, the port, the certificate pins, the token, the identity | `crates/rustudio-join/tests/allocation.rs` against recorded bodies | no |
| | the five login packets | `tests/login_packets.rs` against rove's Python vectors **and** rove's own encoder | no |
| **M2b** | a session fed a recorded stream through the same `Session` a live join uses | `tests/session_replay.rs` — the world it builds is compared to the one `rustudio-live`'s offline replay proved | no |
| **M2c** | a real join: sign in, allocate, dial, log in, receive | a phone, with the overlay on (started from the Start Page's **Play**; `tests/join_worker.rs` covers the pre-request branches) | yes |

## What the wire actually looks like

`POST gamejoin.roblox.com/v1/join-game` (or `/v1/join-game-instance` for a named
server) returns a `joinScript`, and the fields that decide where the client
dials are not where they used to be:

| what | where | fallback |
|---|---|---|
| the address to dial | `joinScript.UdmuxEndpoints[0].Address` | `MachineAddress` |
| the port | `NetStackConfig.port` (percent-encoded JSON) | `NetStackPort`, then `ServerPort` |
| the certificate pins | `NetStackConfig.certHashes` (base64 SHA-256) | none — no pins, no dial |
| the routing token | `NetStackTokenValue` | sixteen zero bytes when absent |
| the backend in the routing header | `MachineAddress` | — |

With udmux in front, the address to dial and the machine the header names are
**different addresses**. `tests/allocation.rs` asserts both, because getting them
from the same field is the mistake that produces a connection that is accepted
and then silently mis-routed.

Two of these are refusals rather than defaults on purpose: an address that is
not globally routable (loopback, private, link-local) is not dialled, and a
`NetStackConfig` whose port disagrees with `NetStackPort` beside it yields no
pins and no port, so the allocation is reported as not transport-ready rather
than dialled unpinned.

## The session

```
dial ──► control opens (applications 0, 4, 6) ──► login packets (4/1) ──► receives
                                                                          │
                                        SessionCore::ingest ──► journal ──┤
                                                                          ▼
                                                       rustudio_live::Live::apply
                                                                          │
                                                                     Document
```

`Session::feed` is the only path from bytes to world state, and `session::run`
calls it with exactly what its reliable stream delivered. That is why the
recording test is evidence for the live path and not a parallel implementation:
both go through `feed`, and `feed` is where rove's decoder, rove's journal and
rustudio's bridge meet.

A refused packet is counted and the session keeps reading. A hostile or broken
server must not be able to end a session by sending one bad packet, and a
refusal is data about the server.

## The five login packets

| # | packet | id | built from |
|---|---|---|---|
| 1 | early auth | `0xa8` | the two blobs in the issued `ClientTicket` |
| 2 | protocol sync | `0x90` | the retained join fields, the platform, six constants |
| 3 | verification | `0x92` | a fresh nonce from the OS |
| 4 | ticket | `0x8a` | the client ticket, its XXH32 and checksum, `SessionId` |
| 5 | server id | `0x8f` | nothing |

The bytes are checked three ways: against the Python reference vectors rove
ships, against rove's own Rust encoder compiled in as a dev-dependency, and
against each other (the vectors cover the shapes; the encoder covers the
branches).

## How a person starts a join

The Android APK is built with `--features live` (`.github/workflows/android.yml`);
a desktop build without it is unchanged and has no QUIC stack.

| where | what |
|---|---|
| Start Page, open-by-ID row | **Play** beside **Open**: Open downloads the place to edit, Play joins an official server of it as a player |
| `RUSTUDIO_JOIN=1818` | starts the same join on the first frame (a phone has no command line, so this is also how the APK can be told to join straight away) |
| the join window | every stage with its time, the failing one named, the session's counters, **Copy the report**, and **Open the place** |

*Open the place* installs the world the session built into the editor: the same
`Document` a file becomes, so what replication created is Explorer rows and
renderable parts, not a number in a log. It appears once a session has replicated
something, and taking it twice takes nothing the second time (a `Document` moves
into the editor; two copies of a live place would be two different worlds).

`src/join_window.rs` is the app's end; `rustudio_join::join` is the whole join,
on a thread of its own, with the report a render loop polls. The cookie comes
from the account the app already holds (`rustudio_account`), is passed to the
worker as a closure, and is never returned or logged.

## M2c: the part only a phone can answer

Nothing in CI can stand in for this, because it needs an account, a live server
and a certificate the server actually presents. What is built for it:

- `--features live`: `AccountSession` (sign in, the join-game POST, the
  anti-CSRF retry), and `session::run` over rove's own WebTransport client.
- `diagnostics::Report`: one line per stage — sign in, allocate, connect, log in,
  receive — with the failed one named and the duration of each. It carries no
  secret: the user id and the endpoint appear, the cookie, token and tickets do
  not.

A report that stops at **allocate** means the account or the request; one that
stops at **connect** means the pins or the address; one that reaches **log in**
and then closes means the server accepted the connection and rejected the
credentials. Those are three different bugs, and the overlay is what tells them
apart without a capture.

The failing branches that need no account are tested in CI
(`crates/rustudio-join/tests/join_worker.rs`, run with `--features live`): no
account, a cookie that cannot go into a header, and polling the report from
another thread. Everything past `sign in` needs a phone.

### Status 12: what the launcher refuses, and why

The first phone run stopped at `allocate`, with

```
allocate failed: refused (http 200, status 12)
```

HTTP 200 and status 12 is Roblox's `gamejoin` launcher answering **"You are not
allowed to play this game"**: it refuses a join request that identifies as a
web client, and admits the same place and the same cookie once the request says
it is a native client. The header that decides it is **`rbx-user-agent`**
(`Roblox/Web` → refused, `Roblox/Darwin` → admitted); the ordinary HTTP
`User-Agent` on its own changes nothing. This was measured on the live endpoint
by the research in [H644b/Roblox-Public-Web-Play-Join](https://github.com/H644b/Roblox-Public-Web-Play-Join),
which reports exactly this pair of responses for one public place.

`rustudio_join::http` sent no `rbx-user-agent` at all, which reads as a web
client. It now sends `rbx-user-agent: Roblox/Darwin` and a native-shaped
`User-Agent`. That is only an admission decision: the session that follows is
still this client's own — its own certificate pins, its own WebTransport
connection, its own five packets.

A refusal also carries the launcher's own sentence now, and the report prints
it, because the number alone cannot tell these apart:

| status | the launcher's message | what it means |
|---|---|---|
| 12 | "You are not allowed to play this game" | the request's identity is refused (or the place is not public) |
| 12 | "… (Playtesters Only)" | the place is not open to this account |
| 12 | "Unable to join Game N" | no server of that place accepted the request |

If a run still reports 12 with the identity header in place, the sentence beside
it is the next thing to act on.

### What to send back

1. Install the APK from the run's artifact.
2. Sign in (Start Page → the account button) with the account that owns the
   place's access.
3. Start Page → paste `1818` (or a URL) → **Play**.
4. When the report settles, **Copy the report** and send that text unchanged.
   It names each stage, the failure and the counters; it holds no credential.

## The stalled join: a logcat, and what it proved

A real session on a phone (place 1818, 2026-11-27 15:38) logged this and then
nothing at all for the rest of the session:

```
session: connected to 128.116.50.33:56753
session: login packet 1 (103 bytes) sent
...
session: the server opened application 4 channel 13
```

No summary line, no packet counters, no report. Three separate faults were
hiding behind that silence, and each one alone was enough to make the join look
stuck.

### 1. Packets were fed after the loop, so the loop had to end first

The read loop collected packets into a `Vec` and handed them to the decoder when
it finished. The borrow checker had pushed the code that way — a `select!` arm
cannot hold `&mut Session` across an await — but the effect on the wire was that
**nothing was decoded until the session ended**. A server keeps its reliable
streams open for the life of the session, so "end" meant the 30 s deadline, and
the overlay showed *logged in* with every counter at zero for the whole time.

A reader task per stream now hands each packet to the loop through a bounded
channel, and the loop feeds it immediately. This is rove's shape
(`enqueue` → world owner), and it is why the counters move during a join.

### 2. The final drain could wait forever

`readers.join_next().await` on a stream the peer has not closed never resolves.
After the loop, that drain ran over every reader, so a join could sit past its
own deadline with the runtime blocked — which is exactly why the summary line
never printed. Every wait is now bounded, the drain takes only what is already
queued, and the rest is aborted.

### 3. Fourteen streams, and a cap of sixteen

Place 1818 opened **fourteen** reliable streams in the first millisecond:
applications 0, 4 and 6 across channels 0, 1, 2, 4, 7, 8, 9, 10, 12, 13 and
`0x6374726c` (the control channel). The cap was 16. Upstream's is **256**, and
now so is this one — a busy place would otherwise have failed its join with
"the server opened too many streams" for a reason unrelated to the join.

### And the packets that are not the world's

Reading every stream is only half of it: a server multiplexes several kinds of
packet onto the same application, and two of them share a first byte with the
world's.

| stream | first byte | what it is | what this session does |
|---|---|---|---|
| 4/1 | `0x97` (151) | schema | decode |
| 4/1 | `0x81` (129) | globals | decode |
| 4/1 | `0x83` (131) | replication | decode |
| 4/5 | `0xa1` (161) | instance state | decode via `ingest_instance_state` |
| 4/1 | `0x93` (147) | feature table | read, and set the property encoding it names |
| 4/1 | `0x84` (132) | marker acknowledgement | counted |
| 4/3 | `0x83` (131) | **replication ping** | counted, *not* decoded |
| anything else | — | — | counted as unhandled |

The ping row is the dangerous one: before the dispatch existed, a ping on 4/3
would have been handed to the replication decoder, which is a corrupt world
rather than a slow one. Pings are counted and not answered — a reply has to
carry measured replication statistics and a complete integrity inventory, and
upstream blocks a reply rather than inventing one. Porting that measurement is
the next step, and the report says how many pings were asked for.

### What the report prints now

```
receive  ok     412 packet(s): 1 schema, 1 globals, 380 replication, 3 refused —
                0 feature(s), 2 marker(s), 26 ping(s), 0 unhandled —
                4/1 id 131 ×380, 4/3 id 131 ×26, 4/1 id 132 ×2, ...
```

A session that receives and builds nothing reads differently from one that
receives nothing, which was the point of asking for the report in the first
place.

### What to send back

Same as above, and the overlay is now worth watching *during* the join: it
shows the stage, the packet count, the other-stream count and the seconds left,
updated as packets arrive. If it stays on `logged in` with zero packets, the
next thing to look at is the arrivals list in the report.

### The pre-login handshake this client was not doing

The phone report settled one question and raised a better one: sign in and
allocate pass, the server opens its streams, and then **zero packets in thirty
seconds**. A server that opens streams and then says nothing is waiting for the
client, and diffing `session::run` against `rove/rust-client/src/live_session.rs`
shows what it was waiting for. Upstream, before a single login packet goes out:

```
clock.initialize_replicator()                 // the replicator wire clock
send(0, CONTROL, [0,0]) send(4, …) send(6, …) // control opens
sender.open(6, 1)                             // the clock register
control_open(6, 2, 2) control_open(6, 3, 4)   // announced clock datagram channels
sync.start(&clock)                            // the time-sync probe
```

This client did the three control opens and then went straight to the login
packets. Everything else was missing, and two of the omissions are the kind a
server notices:

- **The capabilities were never offered.** `transport_allocation` passed
  `capabilities: 0` while `protocol_sync` declared no bytecode range. Upstream
  offers `BYTECODE_RANGE_CAPABILITY` (`0x10_0000`) and logs in with
  `Some((13, 14))`; the two are one promise. Here both are now made, and the
  negotiated mask is checked after the handshake — upstream closes the
  connection at that point rather than continue into a login that asks for a
  feature the handshake refused.
- **The time-sync probe never ran, and its replies are datagrams.** A reply is
  an unreliable packet on wire id 4, so a loop that reads only reliable streams
  can never see one. The loop now polls `receive_unreliable`, feeds wire-4
  packets to `TimeSyncProbe::receive`, and sends its requests on the interval
  `due` asks for.

### The report's times were not the steps' times

Steps were stamped as `elapsed − sum(previous steps)`. `log in` is pushed after
the session *ends*, so it absorbed connect, the login, and the whole thirty
seconds of receive: "log in 33157 ms" named a step that took two seconds. A
report like that cannot say where the time went, which is the one thing it is
for. Each step now measures from the moment the previous one was pushed, and
`a_steps_time_is_its_own_span_not_everything_before_it` pins it.
