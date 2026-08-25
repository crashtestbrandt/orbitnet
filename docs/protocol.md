# Protocol and the tick model

What is on the wire, how the clock works, and what `is_fresh` guarantees. Read this before changing
`native/crates/orbitnet-core/`, or when a replication bug does not make sense.

## Wire format

Little-endian.

**Hot frame** — unreliable, one per peer per tick:

```
frame kind | tick | ack tick (zigzag delta) | 32-bit ack bitfield | 32-bit ack token | input-arrival margin byte
           | header flags byte | entity count (varint)
CLIENT TO SERVER ONLY, before the blocks: the interest generation this client holds (varint)
then per entity:  { slot (u16) | frame-tick delta | body length | flags | changed-property bitmask | packed payload }
then, SERVER TO CLIENT, if the header's flags say so: the interest-delta section
then the trailer: { sequence (u32) | MAC tag (u64) }
```

**The client's interest generation rides BEFORE the blocks**, because the server's block loop stops early
when its receive budget refuses one — so anything after them is not reliably reached. It is what lets the
server decline to build an interest-delta section at all until the two ends agree on the baseline; see
[the whole interest set](#the-whole-interest-set).

No property names, no type tags — the schema is positional and agreed in advance. Client input carries the
last N ticks for redundancy, so a single lost packet costs nothing.

**A block names its entity by a 16-bit session slot, not by the 64-bit entity id.** See
[entity slots](#entity-slots) for what that costs and what it saves.

**Control frames** — reliable and **ordered**: handshake, welcome, entity manifest, entity manifest
delta, and the interest table. All but the handshake carry the same 12-byte trailer. Ordering is what a manifest delta needs
and a snapshot does not; every frame goes out `TRANSFER_MODE_RELIABLE` on one channel.

**Handshake** — magic, protocol version, tick rate, a **session id**, the **session nonce**, the
**resume token**, then the **confirm tag**:

```
magic | protocol version (u32) | tickrate (u16) | session id (u64) | session nonce (16 bytes) | resume token (u64) | confirm (u64)
```

| Field | Width | What it is |
| --- | --- | --- |
| magic | 4 | `OBNW` |
| protocol version | 4 | `(major << 16) \| (minor << 8) \| patch` |
| tickrate | 2 | the sender's rate in hertz |
| session id | 8 | the player identity, client-asserted |
| session nonce | 16 | **the session key itself with no secret configured, a nonce with one** |
| resume token | 8 | the server-minted value a claim on the identity quotes |
| confirm | 8 | proof the sender holds the shared secret; `0` proves none |

The session id is what makes a reconnecting player recognizable: a peer id names the connection and is
reassigned every time, this names the player and is resent verbatim on every join. It is **asserted by the
client and verified by nobody** — adequate for giving a player their own entity back, inadequate for anything
that must not be forged. `0` means "no identity"; see [api.md](api.md#session-identity-and-reconnection).

The resume token is what makes that claim checkable. It is **minted by the server**, one per identity, sent
in the welcome, and quoted back here; `0` quotes none. See [the resume token](#the-resume-token).

The session nonce and the confirm tag are the two regimes of
[datagram authentication](#datagram-authentication). The 16-byte field kept its offset and its width when it
became a nonce, and which regime is in force is a local decision neither end puts on the wire.

Everything after the protocol version decodes **best-effort**, to a zero tick rate, no session id, an
all-zero nonce, a `0` resume token and a `0` confirmation. `handle_hello` answers a decode error by returning,
so a peer whose handshake is short would otherwise be dropped in silence; decoding it far enough to reach
the compatibility check is what produces the operator-readable version mismatch. An all-zero nonce is then
refused with a message of its own. A `0` resume token is refused nothing — it is what a first-time joiner
sends, and it costs that peer only its resume. A `0` confirmation is refused nothing either, unless the
reading peer holds a secret.

**Welcome** — the join reply, carrying the token the client stores for its next join:

```
frame kind | protocol version (u32) | server tick (varint) | tickrate (u16) | resume token (u64)
```

Its resume token decodes best-effort to `0` for the same reason, and a welcome that failed to decode would
leave a joining client unsynced with nothing to say why. `0` there means "this connection holds no identity
of ours", and the client keeps whatever token it already stored rather than forgetting one.

**Versioning.** `PROTOCOL_VERSION` packs `(major << 16) | (minor << 8) | patch`; **major must match exactly**.

| Major | What changed |
| --- | --- |
| 2 | Quantized wire encodings. |
| 3 | Per-datagram authentication, and the handshake's session key. |
| 4 | The hot-frame header carries an **ack token**. |
| 5 | Blocks name entities by a **16-bit session slot**; the entity manifest distributes the slot table for both lanes. |
| 6 | Each entity manifest entry also carries the entity's **input owner and seat**, which is what distributes the seat roster to clients. |
| 7 | A snapshot frame may carry a trailing **interest-delta section**, naming the slots that entered and left that one peer's interest. The handshake and the welcome each carry a trailing **resume token**, which is what a claim on a session identity has to quote. The handshake's 16-byte session key becomes the **session nonce**, and the handshake gains a trailing **confirm tag**; with a shared secret configured the key is derived from `(secret, nonce)` rather than read off the wire. The entity manifest opens with a **generation** and states a **change** rather than the whole table, on a new `EntityManifestDelta` frame kind. |
| 8 | The interest-delta section opens with a **generation**, one peer's whole interest set has a frame kind of its own (`InterestTable`), and a client asks for one with `WANT_INTEREST` (flags bit 3). Before it, a section naming a slot the receiver could not resolve was dropped in silence and then retired on that frame's ack, so the two ends disagreed about that entity for the rest of the session. A client input frame also carries, before its blocks, the interest generation that client holds, so the server builds a section only for a peer that provably holds the baseline it is diffed against. The leading generation shifts the offsets of the section's own counts and the echo shifts every block's, which is what makes this a major rather than a trailing addition. |

**Minor is not checked and records a change no peer can misread** — the only kind that qualifies is an
optional *trailing* field on a control frame, where an older peer stops decoding before it and gets the
documented absent value. Anything that shifts an existing field's offset is a major bump, because there the
older peer decodes garbage.

Schema agreement is **per entity, not per session**: the server states each replicated entity's state and
input hash in its `EntityManifest` frame — `0` for the input hash of a state-lane entity, which has no input
schema — and a client whose locally built schema hashes differently is told
so by name. The hash is FNV-1a over `(name, kind, role)` in **declaration order** — deliberately
order-sensitive, because two peers registering the same properties in different orders would otherwise
silently misapply state, which is miserable to diagnose. The handshake carried a session-wide schema hash
until major 3; there is no such thing to hash, both call sites passed `0`, and the field was removed.

**Robustness.** The decoder is the one component parsing bytes chosen by a remote peer, so every read is
bounds-checked and returns an error rather than panicking — a decoder that panics on a malformed packet is a
remote denial of service. `#![forbid(unsafe_code)]` means a bounds bug cannot become memory unsafety either.
Tests sweep truncated and pseudo-random buffers.

## Entity slots

A block used to open with the entity id: 64 bits of FNV-1a over the synchronizer root's node path, written
as a varint. Hash output is spread across the whole 64-bit range, so that varint cost **9.5 bytes on
average** — measured over 6000 plausible node paths under both lane salts, which is also the theoretical
mean for a value uniform over 2<sup>64</sup>.

Against the RTS demo's own state entity — 20 B of properties — that was **29% of a full block and 46% of a
delta**:

| | Full block | Delta, one changed 6 B property |
|---|---|---|
| id, as a varint | 9.5 B | 9.5 B |
| **slot, fixed** | **2 B** | **2 B** |
| other framing | 3 B | 5 B |
| payload | 20 B | 6 B |
| block, before | 32.5 B | 20.5 B |
| **block, now** | **25 B** | **13 B** |

At the default 1200 B budget that is 37 full blocks per frame before and **48 after** — 30% more entities
refreshed at the same budget. Across 100 peers at 30 Hz it saves 764 kB/s (6.1 Mbit/s) of server egress.

**Fixed width, not a varint.** A `u16` varint costs 1 byte below 128 and 3 above it, so past 128 entities
most blocks would pay more than the flat 2.

### What a slot costs

The id needed no distribution: every peer derived the same value from the same node path, which is why a
reconnecting client re-derives its ids with no handshake. A slot is **assigned by the server**, so it has to
be distributed and held.

- **The entity manifest carries it**, reliably, as `(slot, id, state hash, input hash, owner, seat)` per
  entity. That frame covered the rollback lane only while it was purely a schema check; it now names **both
  lanes**, because state-lane blocks carry slots too.
- **It states a change, not the whole table.** See [the manifest states a change](#the-manifest-states-a-change-not-the-table)
  for the layout, what that gave up, and what stands in for it.
- **A block whose slot has no binding is skipped**, exactly as a block for an unknown entity id was. Blocks
  stay length-prefixed for that reason. An unreliable snapshot can overtake the reliable manifest that binds
  its slot; the block is lost, the next one lands.
- **A client sends no input block for a body whose slot has not arrived.** Input rides `INPUT_REDUNDANCY`
  ticks of history, so the first block after the binding lands re-sends what those ticks held.

### The manifest states a change, not the table

The manifest was rebuilt and broadcast whole whenever anything dirtied it — a registration, an
unregistration, a slot reconcile, a seat or authority write, or **any hello** — and it is flushed
once per frame that advanced a tick. So the ceiling was one whole-table republish per net tick per
peer, and a single join cost every peer already in the session a copy of the entire table.

**One row is 22.5 bytes**, measured from the encoder rather than estimated:

| Field | Bytes |
|---|---|
| `slot` | 2, fixed |
| `id`, a varint over a full-width FNV-1a hash | **9.5 on average**, uniform over 2<sup>64</sup> |
| `state hash` | 4 |
| `input hash` | 4 |
| `owner`, a varint over a small positive peer id | 1 |
| `seat` | 2 |
| **one row** | **22.5** |

Two frames now, both reliable:

```
full  0x07 | generation varint | count varint | count x entry
delta 0x08 | base_generation varint | generation varint
           | removed_count varint | R x slot (u16)
           | added_count varint   | A x entry      (the entry layout, unchanged)
```

- **The leading `generation` on the full table is what made this a major bump.** It shifts the offset
  of every field after it, so a peer that does not know about it reads the count out of the
  generation's bytes rather than stopping short.
- **`removed` is bare slots at 2 bytes each.** A row is named by its slot and nothing else has to be
  restated to drop it.
- **`added` covers three cases in one record**: a new binding, a rebind of a reissued slot, and a
  changed row on a slot that stayed bound. Binding a slot already replaces both directions, so
  applying an added row is idempotent and needs no case analysis.
- **`generation` is sent explicitly rather than implied as base + 1**, so a server may coalesce
  several dirty ticks into one delta.
- **Both counts are capped exactly as the full table's count is** — a reserve of `count.min(4096)`,
  never `count` — so a four-byte frame claiming `u64::MAX` records is a decode error rather than a
  remote out-of-memory.

**A rebuild that reproduces the published table publishes nothing at all.** That is most of the
saving: almost every dirty flush is a rebuild of a table that did not change.

#### What it costs

At **8,000 named entities and 30 Hz, per peer**, with 20 entities registering or unregistering per
second — 10 spawns and 10 despawns:

| | Before | Now |
|---|---|---|
| the whole table, once | 180 kB | 180 kB, and only to a peer that holds no usable table |
| a settled tick, nothing changed | 180 kB | **0** — no frame |
| one entity registers | 180 kB | **28 B** |
| one entity unregisters | 180 kB | **7 B** |
| a seat, owner or schema write on one entity | 180 kB | **28 B** |
| **20 churn events per second** | **3.6 MB/s** | **350 B/s** |
| the ceiling: something dirties it every tick | 5.4 MB/s | 30 x the changes on that tick |
| one join, with 8 peers already in session | 1.44 MB — every peer | 180 kB — the joiner alone |

The unreliable hot lane is **36 kB/s per peer** at that rate (1200 B per frame at 30 Hz). The
manifest was two orders of magnitude above the lane it rides beside; it is now under 1% of it.

#### What a delta gave up

Rebuilding from a complete table was **self-repairing**: it retired the binding of every entity that
had unregistered since the last frame, with no removal record to lose. A delta reintroduces that
record. A client that misses one keeps a slot bound to an entity the server has unregistered; past
the 256-tick quarantine that slot is reissued, and the stale client applies the new entity's rows to
the old one — silently, with the block decoding cleanly.

Three things stand in for the rebuild, and all three are needed:

| Guarantee | What it covers |
|---|---|
| the channel is **reliable and ordered** | a removal cannot be dropped or reordered while the connection lives |
| a delta names the **base generation** it was diffed against | a client holding any other table refuses it whole rather than applying part of it |
| every path that can desynchronize a client **zeroes its generation** | and a zeroed generation is answered with the full table |

**The generation is not loss recovery** — the channel already gives that. It is what makes "this
client is not holding the table I diffed against" detectable at all.

#### Three ways the stream breaks, and one answer

| Break | How the generation is zeroed |
|---|---|
| a reconnect | the rejoiner arrives on a fresh peer state, at generation 0 |
| a **rekey on a live connection** — a hello carrying a different session key | reset in the same block that replaces that peer's auth; the client restarted its session and its table went with it |
| a delta the client cannot apply — wrong base generation, or a decode error | the client zeroes its own generation and raises `FLAG_WANT_MANIFEST` (bit 2) on its next input frame; the server answers by clearing that peer's generation |

- **The NACK reuses the client-to-server shape `WANT_FULL` already established**: one bit on an input
  frame the client is sending anyway. **No frame kind and no bytes.**
- **Losing the frame that carries the NACK costs one tick.** The client zeroed its own generation at
  the same moment, so the next delta fails its base check and raises the bit again.
- **A full table older than the one a client holds is ignored.** Ordered delivery should make that
  unreachable; the alternative is a client that adopts a table the server has moved past and then
  refuses every delta built on the newer one.
- **A delta is applied into a scratch table swapped in only on success**, so a decode error part-way
  leaves the client's table untouched rather than half-updated. A failed manifest decode used to be
  dropped on the floor, which was safe only because the next complete table repaired it.
- **A client keeps its stale table until the replacement lands.** Clearing it would stop every block
  resolving for a round trip, which is a worse outage than the one being repaired, and the reuse
  quarantine already covers a binding that is wrong rather than merely old.

### Reissuing a freed slot

Ids are reused — a body respawning under its old node name reclaims the same id — and slots are reused too.
Reuse is the one way a slot can be *wrong* rather than merely unknown: a snapshot naming slot `N` can
overtake the manifest that rebound `N` from entity A to entity B, and the receiver would apply B's row to A.

- **A freed slot is quarantined for 256 ticks before it may name a different entity** — ~4.3 s at 60 Hz,
  ~12.8 s at 20 Hz, far longer than the reliable retransmit it has to outlast.
- **It now also covers the repair of a broken manifest stream.** A complete table retired a binding by
  omission; a delta retires it by a record, and a client whose stream broke holds its stale table for
  the one round trip its NACK takes. The window is what stops that stale binding naming a *different*
  entity in the meantime.
- **The oldest expired slot is reissued first**, so churn spends the whole free list instead of cycling one
  slot.
- **Reuse is preferred over minting**, which holds the index space near the session's peak concurrency
  rather than letting it climb with every spawn. Not exactly at the peak: a slot freed inside the quarantine
  window is unavailable to the next caller, so a fast-churning session does mint past it.

### The cap is declared and refused

16 bits caps a session at **65,536 concurrent entities**. Past that the server refuses to name the entity
and says so once, rather than wrapping an index — a wrapped index would alias two live entities onto one
wire name. A refusal while every free slot is still cooling is transient and retried on the next tick.

**Rate tiering still phases on the 64-bit id**, not the slot. Dense sequential indices spread across an
interval *more* evenly than hashes do, so the choice is about stability: a slot is released and reissued,
and an entity that took a different one would jump its tier phase and its keyframe phase mid-interval.

## Per-peer relevancy: the interest-delta section

Interest filtering decides what each peer is sent, and until major 7 nothing on the wire reported that
decision: a culled or withheld entity was a node frozen at its last received pose, indistinguishable from one
whose packets were lost. The section carries the decision, on the snapshot the peer is already receiving.

```
header flags bit 1 set, then after the entity blocks:
varint generation | varint left_count | [slot (u16)]* | varint entered_count | [slot (u16)]*
```

- **Server to client only.** Every flag owns a bit of its own: bit 0 is the client's `WANT_FULL`
  NACK, bit 1 is this and is never set by a client, bit 2 is the client's `WANT_MANIFEST` NACK and
  bit 3 its `WANT_INTEREST` NACK, neither of which a server sets.
- **A trailing section is invisible to a peer that does not know about it.** A receiver reads exactly
  `entity_count` blocks and stops, so an older build never looks at the bytes after them. It is a major bump
  anyway, because a peer that skips the section receives none of the events.
- **Slots, not ids** — 2 fixed bytes against ~9.5 for a varint id, resolved against the table the entity
  manifest already distributes. An unbound slot in the `left` half is dropped in silence, exactly as a block
  naming one is; see [entity slots](#entity-slots).
- **An unbound slot in the `entered` half asks for the whole set.** The section rides an unreliable snapshot
  and the manifest that binds its slots rides a reliable channel, with no ordering between them, so a
  snapshot can name a slot whose binding is still being retransmitted. That enter cannot be held for later —
  the server retires it on this frame's ack — so the client raises `WANT_INTEREST` instead.

### The whole interest set

`InterestTable` (kind `0x09`) is the repair path, and the only reliable frame here whose contents differ per
recipient.

```
kind 0x09 | varint generation | varint count | [slot (u16)]*
```

Three things owe a connection one, and the server knows two of them without being told:

| Cause | Noticed by |
| --- | --- |
| a pending half overflowed its cap | the server, queuing a transition |
| a prefix was given up on unacknowledged | the server, retiring it |
| a section named an `entered` slot the peer could not resolve | the client, via `WANT_INTEREST` |

- **A section is only built for a peer that holds its baseline.** The client echoes the generation it holds
  on the input frame it is already sending, and the server declines to build a section until the two agree.
  That is what stops a section overtaking the whole set it was computed after: there is nothing to overtake,
  because none is sent. The pending halves hold meanwhile and ride the first frame after the client catches
  up.
- **The generation places a section, and the match is exact.** A section states a change against one
  baseline, and a receiver holding any other is not holding the set it was diffed from. The table is reliable
  and a section is not, so a section built either side of a table can arrive on the wrong side of it; a
  receiver applies one only at the exact generation it holds and asks for the whole set otherwise. A re-send
  of a prefix carries the generation it was built at, so retransmission still matches.
- **It is not a chain.** A manifest delta names one exact predecessor and refuses a gap, because its channel
  is ordered and a gap there is a fault. This generation moves only when a whole set is sent, so an ordinary
  run of sections all carry the same one and a dropped datagram costs nothing.
- **The receiver emits the diff, not the set.** A resync that announced every slot would re-announce every
  entity the peer already had.
- **The manifest cannot carry this.** It is a session-wide table broadcast identically to every peer, so it
  says "this entity exists" and never "this entity is relevant to you".

### What it costs

| | Bytes |
|---|---|
| the section's generation varint | 1 in practice, and 10 reserved |
| the section's two count varints | 2 |
| per event | 2 |
| a frame's cap (32 of each half) | 141 reserved off the send budget |
| at rest — a settled tick with no transitions | **0**, and no flag bit |

The generation is reserved at a `u64` varint's worst case rather than measured. It reaches two bytes only
after 127 resyncs on one connection and ten is unreachable, but the reserve is what keeps an unreliable
datagram inside the path MTU, and a bound that holds only for small values is not a bound.

The reserve is taken off the byte budget **before** the admit loop runs, not after it. A section appended to a
frame already filled to the payload ceiling is a datagram past the path MTU, which fragments, and a lost
fragment costs the whole frame.

### How an unreliable datagram carries an event

- **The pending delta is a net difference, not a log.** Queuing a leave drops any pending enter for the same
  entity and vice versa, so an id is named in at most one half and the section says where that entity stands
  now.
- **It is re-sent until the peer's `ack_tick` reaches the tick it first rode on.** The ack window and the frame
  token are already verified per frame, so this needs no reliable channel of its own — only a tick stamp. The
  stamp does not move on a re-send: what an ack has to reach is the frame whose arrival proves the client
  applied those entries.
- **The client applies it idempotently** — remove each `left` from a mirrored set, add each `entered` — and
  announces only a set that actually changed, which is what makes a repeat free.
- **Two bounds, and both are needed.** At most 32 entries of each half ride one frame, so a joining peer's
  burst is spread over frames rather than eating the budget in one. A prefix unacknowledged for 64 ticks is
  dropped unconfirmed: past that an ack can no longer confirm the frame anyway. The events lost with it are
  not reconstructible, so that drop owes the connection a whole set rather than another delta — see
  [the whole interest set](#the-whole-interest-set).

### One signal for two causes

`Net.entity_left_interest` fires for a cull, a membership refusal, a cap eviction, a per-peer veto **and** an
unregister. A client emits it from an entity manifest rebuild as well as from the section, because "the server
stopped sending you this" and "this entity unregistered" are the same fact to a game holding a node it can no
longer update. Both paths gate on the mirrored set actually holding the id, so an entity culled and
unregistered on the same tick fires it exactly once.

**A session that culls nothing announces nothing, and reads as "everything is in interest".** The interest pass
does not run without a radius or a declared membership, so there is no set to diff; `is_entity_in_interest()`
answers `true` for every registered entity there rather than `false` for a session replicating all of them to
everybody.

**The addon does not act on the leave.** The node stays in the scene, holding the last pose it received.
Hide rather than free — a cap eviction oscillates at the boundary and freeing turns that into spawn churn —
and call `NetInterpolatorHandle.teleport()` on re-entry, since a body that moved while it was away would
otherwise fly to its new pose over one tick.

## The seat roster

A **seat** is one owned viewpoint: `(peer, seat label)`. A connection may hold several — local split-screen
is two players behind one socket — and each is anchored, culled and world-filtered on its own.

- **The roster is derived from ownership, never declared beside it.** A seat exists because some replicated
  entity says its input is driven by that connection under that label. A seat table the game wrote directly
  would be a second source of truth about ownership, and ownership is what the anti-forgery check on a
  received input block reads.
- **It rides the entity manifest**, as the `owner` and `seat` columns above, rather than on a frame of its
  own. The roster is a projection of that table and so cannot disagree with it; a separate frame would differ
  from it for as long as either was in flight. A client projects the roster from **the manifest rows it
  holds**, not from the frame that just arrived: a delta that says nothing about an entity says that
  entity's seat has not changed, so a roster built from a delta's rows alone would drop every seat the
  delta was silent about.
- **A change republishes the manifest.** Registration already dirtied it; an authority or label write on an
  entity that stays registered is the case nothing else noticed.
- **Both ends diff their own roster and announce the transitions** as `Net.seat_opened` / `Net.seat_closed`,
  on a tick boundary. A server announces from its registry, a client one manifest later.
- **No hot-path frame carries a seat.** Interest runs where state authority is, and a client never authors a
  seat — it is told which connection and label drive each entity.

## Datagram authentication

Every datagram but the handshake carries a **32-bit sequence number and a 64-bit MAC tag**, and is dropped
before a single field is decoded unless both check out.

- **The MAC** is SipHash-2-4 over the payload, the sequence number, and a **direction byte that is not
  transmitted**. Each side authenticates with the direction it expects to receive, so a datagram reflected
  back at its sender fails the tag check.
- **The replay window** is a 64-entry sliding bitmap, the same construction IPsec uses. A sequence number is
  accepted once; a repeat, or one more than 64 behind the newest accepted, is refused. A datagram whose tag
  fails does not advance the window, so a forger cannot burn sequence numbers the real peer has yet to send.
- **Sequence numbers are refused rather than wrapped.** 32 bits at 60 Hz is 2.2 years of one session.
- The server keeps one key per connected peer; a peer that has not handshaken has none, and everything it
  sends is refused — including the ping a server used to answer for any connected sender.

### Two regimes, and which one you are in

The key is 16 bytes either way, and the handshake's 16-byte field is 16 bytes either way. What differs is
whether those are the same 16 bytes. `Net.set_session_secret()` chooses, on both ends, before
`Net.set_mode()`.

| | **No secret** — the default | **A shared secret** |
| --- | --- | --- |
| The handshake's 16 bytes | the session key itself | a **nonce** |
| Where the key comes from | the client's `Crypto` draw | `derive_session_key(secret, nonce)` on both ends |
| What an on-path observer learns | everything the client knows | the nonce, and nothing else |
| Can an on-path observer forge? | **yes, anything the client can** | no |
| Who may join | anyone the transport accepts | anyone holding the secret — or, on-path, anyone who recorded a join under it |
| What the confirm tag holds | `0` | the tag |

- **With no secret the key crosses the wire in the clear**, so this authenticates a datagram's membership in
  a session, not a peer's identity. An attacker who cannot read the session's traffic cannot forge a datagram
  at all, whatever sender id it puts on one, and one connected peer cannot forge another's. **An on-path
  observer who can read the handshake can do everything the client can.** Recorded as a limit in the
  [README](../README.md#limits).
- **With a secret only the nonce is in the clear.** The secret never crosses the wire, so an on-path observer
  reads every payload and derives no key — it cannot forge a datagram, take a session identity, or quote a
  resume token into a session it can authenticate.
- **What it does not close: an on-path observer can REPLAY a join it recorded.** The nonce is chosen by the
  client and the confirm tag is a pure function of it, so presenting the same pair again derives the same key
  and is admitted. The observer still learns no secret and can author nothing new; what it gets is a session
  in which the datagrams it captured under that nonce verify. They land nowhere: an input block is checked
  against the entity's live multiplayer authority, which is bound to the connection rather than to the key,
  and the ticks it carries have long rotated out of history. The nonce still buys what it is for — without it
  a constant key would make every captured datagram valid in **every** later session, including honest ones the
  attacker never observed. Closing the replay as well needs a value the acceptor contributes, and therefore a
  second round trip before a client may send anything.
- **The secret is a derivation input and is never seated as the key.** Sequence numbers restart at 1 on every
  join and the replay window only ever knows the session in front of it, so a key that did not change between
  joins would make every datagram captured in one session a valid, unreplayed datagram in the next. The
  per-join nonce is the only thing keeping the key per-join, which is why an all-zero nonce is refused at the
  handshake under both regimes.
- **The confirm tag** is `SipHash(derived key, "orbitnet-confirm" ‖ nonce ‖ protocol version)`, and it is
  checked against the version the sender stamped on its own frame — major must already match, and minor and
  patch are allowed to differ.

### Three ceilings a secret does not lift

- **The tag is still 64 bits and the key still 128.** A secret changes *who* can forge a datagram. It does
  not change how hard forging one is for somebody who cannot read the secret.
- **The derivation adds no strength beyond the secret's own entropy.** A secret a lobby prints on screen, or
  one short enough to guess, derives a key worth exactly that much. Any length is accepted and folded to 16
  bytes; the fold cannot add entropy that was not supplied.
- **None of this encrypts anything.** Every payload is still on the wire in the clear, under both regimes. A
  MAC says a datagram was not written by somebody outside the session, and says nothing else.

### What it costs

| | Bytes |
|---|---|
| the handshake's 16-byte field | **0 new** — it kept its offset and its width and changed meaning |
| the handshake's trailing confirm tag | 8, present under both regimes and `0` under the first |
| per join, both directions | **8**, on a frame that is already reliable, and only from the joining side |
| at rest — every snapshot, input, manifest and welcome frame | **0** |

**Configuring a secret costs nothing beyond that.** The 8 bytes are the field, not the tag: a session with no
secret writes `0` into it and pays the same 8. The derivation itself is two SipHash passes over 16 bytes, run
once per join on each end.

### What a misconfiguration looks like

Both ends must be handed the same secret and both must be handed it before `Net.set_mode()`. When one is not,
the two ends derive different keys and **nothing either of them sends opens at the other**. The symptom the
joining player sees is the same in every wrong combination: a join that never completes while the handshake
retries. What differs is whether the other end can say why.

| Server | Client | What happens |
| --- | --- | --- |
| no secret | no secret | the cleartext-key regime; every existing session |
| secret | the same secret | keys derive equal, the session runs |
| **secret** | **none, or a different one** | one readable rejection in the server's log; the join is refused |
| **none** | **a secret** | nothing is logged anywhere; the join hangs |

- **Server with a secret, client without** is the direction the confirm tag reports. The client's tag is
  absent or is a tag over other bytes, the compatibility check refuses the hello by name, and an operator
  reading the server's log is told which configuration is wrong. Without the tag the server would seat the
  session, refuse every datagram from it as unauthenticated, and log a generic warning about a forged or
  replayed packet.
- **Client with a secret, server without** cannot be reported at all, in either direction. The server derives
  the nonce as the key and seals its welcome under it; the client, holding a different key, refuses that
  welcome as a bad tag and never reads a byte of it — including a rejection, if one were written. The server
  meanwhile sees a hello it has no reason to refuse. **Compare `Net.has_session_secret()` on both ends when a
  join hangs**; it is the only thing that distinguishes this from a dead link.

**Why not a key exchange instead.** An X25519 exchange inside `orbitnet-core` is roughly 400 lines of new
field arithmetic in a crate with zero dependencies and `overflow-checks` on, with no constant-time groundwork
past a ten-line tag compare and no timing harness to prove the compiler did not reintroduce a branch. And an
**unauthenticated** exchange does not close the hole above anyway: the attacker in question is on-path, and an
exchange with no key the client already trusts is substituted by exactly that attacker. It would demote the
adversary from on-path to passive-only, at that cost. A secret the game already authenticated closes both.

## The resume token

The session id names a player. The **resume token** is what a claim on that identity has to quote, and it is
the value only the client that owns the identity was ever sent.

Matching on the identity alone let a peer that had merely *seen* another player's session id take that
player's body. `handle_hello` walked every connected peer, stripped the identity off any that matched the
presented one, and reported the match as an ordinary resume. Nothing tested whether that connection was
alive. The incumbent kept its socket, received no error, and stopped driving its entity.

- **Server-minted, one per identity.** It is drawn from the same generator as the session key — 63 bits, the
  sign bit cleared so it round-trips through a GDScript `int` and every save format — at the first hello that
  seats an identity, and it is **not re-minted on a retried hello**. A hello is retried until the welcome
  lands, and a fresh token would strand the one the client took from the welcome that did arrive.
- **It is not the ack-token salt.** That value is deliberately never transmitted, and transmitting it would
  let a client compute the ack token for frames it never received. The two are separate draws.
- **A granted resume carries the token forward**, so the value the client stored stays the value the server
  holds, for as long as the identity lives.
- **A mismatched claim leaves the held session in place.** Refusing the claim is what closes the takeover;
  refusing it *and* spending the window would turn one forged hello into a denial of service — the real
  player comes back inside the grace window and finds nothing held.
- **A refused claim on an identity somebody else holds is seated anonymously**, with session id `0` and
  `resumed_from` `0`. Two live peers under one identity is what makes `Net.peer_session_id()` and
  `Net.is_session_held()` lie, and a refused claimant seated under a *held* identity would overwrite that
  record — token and all — on its own later drop.

**What it closes**: a peer that observed an identity off a roster broadcast, a kill feed, a log line or a
screenshot. It never saw the token.

**What it does not close on its own**: an on-path observer, who reads the welcome and can then quote the token
verbatim. That is the same boundary [the cleartext session key](#datagram-authentication) has, and it closes
the same way — a **shared session secret**. Under one, that observer can still copy the token but cannot
authenticate the handshake that quotes it, so the claim never reaches the resume decision.

**A client stores one token, naming whichever server last issued one.** Joining a second server under the
same identity replaces it and forfeits the resume on the first. Storing one per server would need a server
identity the protocol does not carry, and what it buys is a player alternating between two servers inside one
grace window — 30 s by default.

### What it costs

| | Bytes |
|---|---|
| the handshake's trailing token | 8 |
| the welcome's trailing token | 8 |
| per join, both directions | **16**, on frames that are already reliable |
| at rest — every snapshot, input and manifest frame | **0** |

### The policy: which claims a server grants at all

`Net.set_resume_policy()`, server-side, and `resume_grant` is the whole rule. In order: `NEVER` refuses;
a non-zero token on record that the presented token does not match refuses; a **live** incumbent under any
policy but `ALWAYS` refuses; otherwise the claim is granted.

| Policy | What it grants |
| --- | --- |
| `ALWAYS` (default) | any claim quoting the right token, including against a connection that is still up |
| `ONLY_IF_DROPPED` | the same claim only once the server has seen the incumbent drop |
| `NEVER` | nothing; a returning player is a new player |

**The default is `ALWAYS`, and the token is why.** The reachable attack was the observer, and the token
refuses it under every policy. What `ALWAYS` is still open to is the on-path observer — who can read the
traffic and therefore already do everything the client can, and who can wait for a drop like anybody else, so
`ONLY_IF_DROPPED` buys nothing against it. What it costs is every honest fast reconnect: a relaunched client
routinely arrives before the transport reports its old socket gone, measured at anywhere from 45 s to never
on ENet's defaults. A session under a **shared session secret** removes the on-path observer from that
sentence entirely, which is the case `ONLY_IF_DROPPED` was left with.

**Under `ONLY_IF_DROPPED` the supersede step does not run.** The incumbent keeps its identity, so its own
disconnect still opens a real grace window and the returning player claims it on the next join.

### Compatibility, stated rather than hidden

**A peer that quotes `0` cannot resume once a token is on record.** Both fields are trailing and both decode
to `0` when absent, so an older peer connects, plays, and is simply never resumed.

There is no mixed-version session to support: this lands in one release with one protocol major, and
**major must match exactly**, so a peer that does not send the token is refused at the handshake for the
version, not for the token. A server that wants to refuse an older client does that by pinning its own
`PROTOCOL_VERSION` — there is no per-field capability negotiation and none is planned.

## The ack token

`ack_tick` says which snapshot frame a client holds. The server spends that number twice — it is the base
every masked delta is encoded against, and it is the round-trip sample that sets that peer's rewind depth —
so it is checked rather than believed.

- **The server mints a token per snapshot frame**, from a 16-byte secret it draws per connection at the
  handshake and **never transmits**. The token is SipHash-2-4 over the frame's tick under that secret, so it
  is derived rather than stored and any tick can be checked, including one the sent log has expired.
- **The client quotes back the token of the frame its `ack_tick` names.** It moves only when
  `ack_tick` moves, so the two always name the same frame.
- **An ack that does not carry the right token is discarded whole**: no `newest_ack`, no round-trip sample,
  no `acked_base` promotion. Every one of those is granted on the strength of the claim. It is counted as
  `unproven_acks_s` in [`bandwidth_metrics()`](api.md); an honest client cannot produce one.
- The 32 ack **bits** name frames older than `ack_tick` and prove nothing themselves. They are consumed
  because the tick they hang off was proven, and refused with it when it was not. A peer that lies in the
  bits breaks its own delta chain and NACKs.

**What it does not settle.** A token says the peer received the frame it names, not that it received nothing
newer. A client that advances its ack at full rate while holding a constant lag quotes a real token every
time and is measured at that lag — indistinguishable from a peer behind a traffic shaper, and it gains
nothing that peer does not gain honestly. No wire field closes that: `current - ack` is the whole round trip
whatever tick lead the client runs at, so there is no second quantity the server could derive an independent
figure from. The containment is `Net.rtt_believed_max_ms`, 250 ms by default, which clamps the sample at the read;
`NetLagComp.max_delay_ms` separately bounds the rewind itself. The two are different bounds that happen to
share a default.

## What the receive path refuses, and what it does not

The backend checks the wire. It does not check your game.

| Refused by the backend | |
| --- | --- |
| A datagram whose MAC does not verify | Forged, corrupted, or reflected. |
| A datagram replaying a sequence number | Or one further back than the replay window reaches. |
| An ack for a frame the peer cannot prove it received | The frame token; see above. The rest of that frame's input blocks are still processed. |
| Anything from a peer that has not handshaken | Including pings. |
| A peer speaking a different protocol major | Rejected at the handshake with a readable message. |
| A handshake carrying an all-zero session nonce | An older build, or a truncated frame. Rejected by name. |
| A handshake that cannot confirm this peer's session secret | Only when this peer holds one. Rejected at the handshake rather than after the first failed tag; see [datagram authentication](#datagram-authentication). |
| A resume claim that does not quote the server's token | Seated as a newcomer; the incumbent keeps its identity and the held window is not spent. See [the resume token](#the-resume-token). |
| A block naming a slot with no binding | The spawn or the manifest is still in flight. Skipped cleanly; the rest of the frame decodes. |
| An entity-manifest delta against a generation the client does not hold | Refused whole, never in part. The client zeroes its generation and asks for the table; see [the manifest states a change](#the-manifest-states-a-change-not-the-table). |
| An entity manifest that does not decode | The same answer, and the same NACK. It used to be dropped in silence, which was safe only while the next frame carried the whole table. |
| An entity manifest whose generation is older than the one held | Ignored. Adopting it would make the client refuse every delta built on the newer table. |
| An interest-delta entry naming a slot with no binding | A `left` entry is usually an unregister naming a slot the manifest has released: dropped in silence, and the manifest rebuild emits that leave. An `entered` entry has no second source, so it raises `WANT_INTEREST`. |
| An input block for an entity the sender does not own | The live `get_multiplayer_authority()` check on the input node. |
| An input block stamped too far into the future | Past `INPUT_FUTURE_HORIZON_TICKS` ahead of the server. |
| An input row of the wrong wire stride, or for a tick history has rotated past | |
| An input row carrying a **non-finite float** | A NaN or an infinity in any float lane. Dropped, never sanitized; see [below](#the-one-input-value-that-is-refused). |
| More than 64 input blocks from one peer in one tick | The per-peer receive budget. The rest of that frame is abandoned, and so is the rest of a frame that names more than 8 entities the sender does not own. |

| **Not** checked by the backend — yours | |
| --- | --- |
| **Input values, apart from finiteness** | A row that decodes at the correct stride and holds only finite floats is written into input history as-is. Range, rate and plausibility are your job, inside `_rollback_tick`. A client is free to send a movement axis of 10<sup>9</sup>. |
| **Command payloads** | `NetCommand` resolves the sender; the *handler* decides whether that sender may do that thing. See [api.md](api.md#netcommand). |
| **Session identity** | Client-asserted, verified by nobody. What it can no longer do on its own is resume somebody else's session — see [the resume token](#the-resume-token). |
| **Account identity, entitlement, bans** | An authenticated layer above this, whose verified id goes into `set_session_id()`. |

### The one input value that is refused

**Range, rate and plausibility stay in the game's column.** A non-finite float is refused because it
is different in kind: a poison value that breaks the simulation for every peer rather than only for
its sender.

- `PropRole::Input` is **restored**, so the poisoned row is written onto the game's input node before
  every replayed tick and the resim runs through it.
- The non-finite state that results is recorded and goes back out on the **state lane to every
  peer**.
- A non-finite position **has no grid cell**, so the interest filter classifies that body as
  uncullable and it replicates to every peer in every world for as long as the pose stays
  non-finite. One poisoned float is a wire-cost regression as well as a simulation one.

**Most of this surface was already closed.** Both quantizers are total over poison: the `@half`
decode maps an exponent of 31 to `0.0`, and the `@ss3` decode renormalizes and clamps. The exposure
is the **unannotated** float property, which is what a plain `float` or `Vector3` input property gets
by default.

**Dropped, never sanitized.** The refused tick keeps no row, which is the state a lost datagram
leaves, and restore resolves it through the closest row at or before and stamps it `Extrapolated` —
the same well-exercised path. The body coasts on its last honest intent. Zeroing the offending lane
would invent intent the player did not author.

**It is visible, because a dropped row otherwise looks like packet loss.** Refused rows are counted
and printed as `input_nonfinite` in the per-second `ORBITNET_DEBUG` wire line, beside `input_novel`,
and the first refusal from a connection names that peer in one warning. Further refusals from that
peer are silent for the rest of the session, for the reason the unauthenticated-datagram warning is
latched: under a flood the log is the second thing to fall over.

**The check is not in the shared row decoder.** That decoder serves the state lane and masked deltas
too, where the sender *is* the authority, so checking there would cost bytes-per-row on a lane that
does not need it and would change that lane's behavior.

**The pattern to copy for everything else.** Clamp axes, bound rates and reject impossible states in
`_rollback_tick`, on the server, where the values mean something.

## The clock

The server is ground truth. The client estimates offset from ping/pong samples and trusts the **lowest-RTT
half** of the window — a fast sample spent least time queued, so its offset reading is least polluted.
Correction is a bounded time stretch, not a jump.

**Catch-up must not spiral.** When a frame runs long, running the whole backlog makes the next frame longer
still. `TickAccumulator` caps ticks per frame and **discards** the backlog it refuses to run, reporting that it
did. Re-aligning afterward is the clock's job.

**Stretch is pinned to exactly 1.0 in coupled mode.** Any stretch ≠ 1.0 slides tick boundaries across physics
frames, producing 0-tick and 2-tick frames that render as judder. Coupled mode runs exactly one net tick per
physics frame and absorbs error by adjusting the client's tick *lead* instead. `stretch()` returns exactly
`1.0` at zero offset — a neutral point at the range midpoint would make a fully synced clock run fast, drift,
and be dragged back forever.

**Adaptive tick lead** closes that loop: the server reports in each snapshot header how early or late that
peer's newest input arrived, and the client steers its lead to keep the margin slightly positive. This is what
collapses the server's per-entity resim window to a single tick for a well-connected peer — one header byte,
large payoff.

## `_rollback_tick(delta, tick, is_fresh)`

Called on the synchronizer root, on every peer that predicts the body.

One invariant that is easy to break and expensive to debug: if your handler re-queries the physics world at
the restored pose — a shapecast, a raycast — the rollback loop must run in `_physics_process` **before** the
physics step. A phase change silently breaks determinism.

### Within-tick entity ordering

The rollback loop replays **one tick across every planned entity at a time**, in three phases — restore all,
simulate all, record all — and within a tick it visits entities in **ascending entity id**. `ResimPlanner`
keeps them in a `BTreeMap` for exactly that reason: a consumer may gate on a bit-exact resim, and a
nondeterministic iteration order would surface there as a phantom desync.

An entity id is FNV-1a of the node path, so **that order is identical on every peer** and is already covered by
whatever gate proves the peers built the same paths. What it is *not* is game-meaningful: whether entity A has
already advanced when entity B's tick runs falls out of two hashes.

The rule that follows: **one direction of cross-entity read per pair, and no cross-entity writes.** A body that
reads another's replicated state sees it either at the start or the end of the tick depending on that fixed
order — the same answer on every peer, which is what matters. A body that *writes* into another's restored
state would have the write land before or after that body's own simulation depending on the same hashes, which
is a coin flip nobody can see.

## Remote prediction reconciles

`net.remote_resim` un-exempts bodies this peer owns neither the state nor the input of, so they join the
rollback loop and are simulated forward every tick. An authoritative row for such a body takes the **predicting**
integration path, exactly as an owner's own body does: the row is compared against the recorded prediction, a
difference marks the planner, and the loop replays from that tick.

It has to. A body that predicted forward without ever re-basing on the server's rows would drift for the whole
session with nothing erroring — and an **inputless shared body** (a puck, a ball, a physics prop) makes that
obvious within seconds, where a remote player body hides it because its own owner's corrections keep the pose
roughly plausible.

Exempt bodies — the default, since `remote_resim` is off unless a game asks for it — are unaffected: they
apply the newest row at the tick boundary and never simulate.

## `is_fresh`

Keyed on **input novelty**, not tick visitation. The backend tracks per-`(entity, tick)` input confidence:

| Level | Meaning |
|---|---|
| `Predicted` | no input at all for this tick |
| `Extrapolated` | input repeated from an older tick |
| `Authoritative` | real received (or locally authored) input stamped for this tick |

**`is_fresh` is true on the first simulation of a tick whose input is `Authoritative` for the simulating
peer.**

- *Owner* — its own input is always authoritative, so a one-shot fires immediately, once.
- *Server* — a remote client's input becomes authoritative when the packet lands, so `is_fresh` fires on the
  resim pass that first carries it, once, however many predicted passes preceded it.

The naive definition ("this `(node, tick)` pair has not been visited") gets this wrong: when the server
predicts tick `T` with no input and *then* receives the client's real input for `T`, it resimulates with
`is_fresh = false` even though that was the first time the real input was seen. Games built on that definition
end up carrying high-water marks and non-replicated tick logs to work around it. Here, one-shot effects gate on
`is_fresh` directly.

For the residual case — an effect that must fire once *and* be undone if a later correction invalidates the
tick — the per-tick memo ring (`NetRollbackHandle.memo_set`/`memo_get`) records on the fresh pass and returns
the same value on every replayed pass.

## Entity lifecycle: the registry outlives the node, briefly

Registration and unregistration are both **deferred** — a synchronizer pushes onto a pending list and
`drain_pending()` applies it at the top of the next `process`/`physics_process`. That keeps game code from
mutating the registry mid-loop, but it means the registry legitimately holds handles to **already-freed nodes**
for a bounded window.

The window is not theoretical. `MultiplayerAPI::poll()` — which emits `peer_packet` — runs **before**
`Node::_process` in the same iteration, so an inbound packet is handled *ahead of* that frame's
`drain_pending`. A client keeps sending input for its avatar for a full round trip after the server freed it.

Two rules fall out:

1. **Never clone a registry handle before checking it.** `Gd::clone` is not infallible: under godot-rust's
   *balanced* safeguards — what a release build ships — `RawGd::clone` calls `check_rtti`, which **panics** on
   a dead instance. So `let s = sync.clone(); if !s.is_instance_valid() { … }` never reaches its own guard.
   Resolve every handle through `live_handle()`, which validates the *borrowed* handle first and only then
   clones.
2. **Re-validate inside the rollback loop.** `run_rollback` builds its ranges once and replays many ticks;
   phase 2 calls `_rollback_tick` under a `base_mut()` surrender, so game code runs *between* phases and may
   despawn a body. Each phase resolves its handle again.

`PendingOp::Unregister` carries the requesting synchronizer's `InstanceId` for a related reason: entity ids
derive from the node path, so a body respawning under its old name reclaims the **same id**, and the
replacement's `Register` can drain ahead of the corpse's `Unregister`. Removing by bare id there would
unregister the live body. Per-peer delta bookkeeping is cleared either way — it describes the departed body,
and a fresh body must not be delta-encoded against it.

The **wire slot** is released by a reconciliation pass rather than beside that removal, because the
registries lose entries three ways — an `Unregister` op, a respawn that supersedes its predecessor, and the
`is_instance_valid` sweep for a node freed without leaving the tree cleanly — and only the first has a call
site to hang a release on. The pass runs when a registration changed, or when the table and the registries
disagree on size, which is what catches the silent sweep.

`tools/orbitnet-smoke.sh` gates all of this by freeing a registered body in each window. Note that such a panic
is *recovered* — the process keeps running with a corrupted frame — so it can only be caught by reading the
log, never by an exit code.
