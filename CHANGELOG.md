# Changelog

Every released version, newest first, with the **protocol major** each one speaks.

Two version numbers move here and each answers a different question.

- The **addon version** is the release tag. The Asset Library zip on that release carries it in
  `addons/orbitnet/plugin.cfg`; the committed `plugin.cfg` on `main` catches up one commit after the tag, in
  the same follow-up pull request that lands `binaries.json`. It answers whether game code has to change.
- The **protocol major** is `PROTOCOL_VERSION >> 16`, from `native/crates/orbitnet-core/src/protocol.rs`. It
  answers whether an upgrade has to be coordinated across every peer in a session. Two majors never
  interoperate — the handshake refuses a mismatched major ahead of every other compatibility rule, in both
  directions, so a session is upgraded whole. See
  [Two protocol majors never interoperate](docs/protocol.md#two-protocol-majors-never-interoperate).
- A release may move either number, both, or neither, and neither is derivable from the other. The rule is in
  [Versioning across releases](docs/protocol.md#versioning-across-releases).

| Version | Date | Protocol major | Coordinated upgrade forced |
| --- | --- | --- | --- |
| [0.5.0](#050--2026-10-06) | 2026-10-06 | 9 | yes, against every earlier release |
| [0.4.0](#040--2026-09-17) | 2026-09-17 | 8 | the major did not move |
| [0.3.1](#031--2026-09-16) | 2026-09-16 | 8 | the major did not move |
| [0.3.0](#030--2026-08-26) | 2026-08-26 | 8 | yes, against 0.2.x |
| [0.2.1](#021--2026-08-23) | 2026-08-23 | 6 | the major did not move |
| [0.2.0](#020--2026-08-23) | 2026-08-23 | 6 | yes, against 0.1.x |
| [0.1.1](#011--2026-08-21) | 2026-08-21 | 2 | the major did not move |
| [0.1.0](#010--2026-08-21) | 2026-08-21 | 2 | first release |

**An equal major means the handshake clears, and nothing more.** `PROTOCOL_VERSION`'s minor is never
compared, so no other part of the frame agreement between two different builds is verified. One build on
every peer in a session is the supported configuration, before and after 1.0. See
[What 1.0 freezes](docs/protocol.md#what-10-freezes).

## Format, and how an entry is added

**This file is the curated upgrade story. The generated release notes are the complete list of what merged.**
`release.yml` passes `generate_release_notes: true`, so every release page already carries every merged pull
request title, and every merged pull-request title in this repository is written as a release-notes line.
Restating that list here would be a second source of truth that drifts from it. Each section below instead
links its release page and keeps the three things the generated list cannot state — what breaks, what the
protocol major moved to, and what a consumer does about it.

**Entries are assembled at release time, from the generated notes, then edited.** Per-pull-request entries
would conflict on every merge and duplicate a title that is already written.

| When | What gets written |
| --- | --- |
| A pull request that forces a **protocol major** or changes what an existing `Net` call means | a bullet under `Unreleased`, in that pull request. The pending-major set has to be visible at release time — [Pending wire breaks ship together](docs/protocol.md#pending-wire-breaks-ship-together) |
| Cutting a release | after the tag is pushed and its release page exists: rename `Unreleased` to the version and date, then fill the remaining subsections from `gh release view <tag>` and `git log --oneline <previous tag>..<tag>` |

**A release section lands on `main` after its own tag.** The generated notes do not exist until the tag is
pushed, so a tagged tree never carries its own section. It arrives in the same follow-up pull request as
`binaries.json` and the stamped `plugin.cfg`, which `release.yml` opens against `main` for the same reason —
its header records that a tag commit cannot contain its own digest.

Subsection order within a release, omitting any that is empty: **Breaking**, **Added**, **Changed**,
**Fixed**, **Internal**. `Internal` covers tests, CI and documentation — changes that alter nothing a
consumer calls or sends. It is a short summary of the areas that moved, never a list of merged titles; the
release page already carries those.

## 0.5.0 — 2026-10-06

[Release page](https://github.com/crashtestbrandt/orbitnet/releases/tag/v0.5.0). **Protocol major 9**, up
from 8. Every earlier release speaks an older major, so a peer on one and a current peer refuse each other's
handshake. Two `Net` calls changed what they do or report, two `NetLagComp` members are parse-time breaks, and the per-second counters every bench reads moved; each is listed under
Breaking. A game that configures no secret and reads no counters needs no edit — the extra round trip is
inside the addon, and the join stays a `Net` call.

### Breaking

- **The join is two round trips and both ends contribute to the session key.** The handshake's 16 bytes are
  the joiner's half of the session nonce; the acceptor answers with a `Challenge` frame carrying a half of its
  own, and the joiner repeats its handshake quoting that half back in a new trailing acceptor-nonce field. The
  key is derived from the fold of the two. This closes a replayed join: under major 8 the nonce was the
  joiner's alone, so an on-path observer presenting a recorded handshake had the acceptor derive the key that
  join had used. A peer predating the change fails every MAC, and the major is what refuses it at the
  handshake instead. The handshake also carries a trailing 32-byte exchange key, zero unless the client pinned
  a server key, and the challenge carries the server's.
- **`Net.set_session_secret()` now also encrypts every payload.** Under a secret each datagram is
  ChaCha20-Poly1305 ciphertext and its trailer is 16 bytes rather than 8. A session that configures no secret
  is unchanged on the wire apart from the join. It costs 8 bytes and about a microsecond per full-size
  datagram at each end.
- **`Net.bandwidth_metrics()` and the bench's per-second columns are exact.** Every `*_s` figure was deflated
  by `1 / (1 + idle_frame_wall_fraction)` on an authority rendering above its net tick rate, by 0.63 at about
  145 fps against a 60 Hz tick. A threshold or a baseline captured on an earlier release reads every
  per-second column as having risen; re-capture it.
- **A state-lane block whose delta carries no change is no longer sent**, and three counters moved with it:
  `blocks_culled_s` counts those un-writes, `starve_ticks_max` has a floor near the keyframe interval for an
  idle channel, and `blocks_full_s` approaches `blocks_admitted_s` on a quiet session. A client that
  overwrote a state-lane property locally now waits for the next keyframe rather than the next visit.
- **One snapshot datagram per net tick.** An authority rendering below its net tick rate used to send one per
  rendered frame; it now sends one per tick it advanced, so its egress rises to what the tick rate implies.
- **Two `NetLagComp` members changed, and neither is on `Net`.** `MAX_INTERP_TICKS` is removed and
  `observed_interp_for_band()` takes the tick rate as a third argument. The interpolation ceiling is derived
  from `max_delay_ms` at the rate the loop is running rather than being a flat count of ticks, so it cannot be
  named without a rate. Both are parse-time breaks in a game that calls them, and the ceiling moved for every
  game: a shooter whose measured cadence is above eight ticks is rewound deeper, up to `max_delay_ms`, with no
  call changed. The `Net` surface did not move; this one is open to change until 1.0 freezes it.

### Added

- **A pinned server key.** `Net.set_server_static_key()`, `Net.generate_server_static_key()`,
  `Net.server_public_key()`, `Net.set_pinned_server_key()` and the two `has_` queries run an authenticated
  X25519 exchange on the join. A pinned client derives its session key from bytes only the holder of the
  server's secret can produce, encrypts under it the way a secret does, and refuses a join the server answered
  without an exchange. It needs no shared secret, so the public half may ship in a build. On the server, a
  connection seated through an exchange refuses a later hello that offers none, so a client that restarts its
  session unpinned on a live connection is refused until that connection drops. The pin authenticates the
  server to the client and nobody to the server: a party that can inject on the path can still rekey a
  connection with an exchange of its own, and only a session secret refuses that.
- **Linux arm64 and Android arm64, arm32 and x86_64** join the published binary set, built and published but
  not run on a device; the macOS profiling library is universal.
- **Per-peer frame assembly runs on a worker pool** at `orbitnet/assembly_pool_peers` synced peers and above,
  12 by default and 0 to disable. `Net.bandwidth_metrics()` gains `assembly_ms` and `assembly_pooled`.
- `NetTransport.target_address()`, `NetTransport.target_port()` and `NetTransport.describe_connections()`;
  `NetLagComp.max_interp_ticks(tick_hz)`.
- `netbench` gains a **relayed link profile**, a per-profile gate on the server's steady-state NACK rate, a
  fleet manifest for runs across machines, and a comparator that refuses two runs whose arguments differ.

### Changed

- A join target is parsed in one place on the transport rather than in four.
- An arena bench client is seated with the seats it asks for, one by default, and a client the session never
  seats fails its own gate. Per-peer figures from runs where every client held two seats are not comparable
  with later ones.

### Fixed

- A decoupled client clock converges on a loaded host instead of hunting between its stretch bounds.
- The send rota no longer skips the candidate after every admitted block, which also removes the server
  panic that skip caused.
- A far body measured past eight ticks of cadence was rewound short; the ceiling now follows `max_delay_ms`.
- The windowed timers and the ping interval are charged one wall base per frame.

### Internal

- Coverage grew on the codec, the determinism path, the synchronizer, the Steam transport, the facade's key
  calls, and both secret regimes in the server-shape probe.
- CI gained the Windows and macOS test legs, a version-parity step on every pull request, version stamping
  gated against `plugin.cfg`, a nightly bench and a nightly timing measurement.
- Documentation gained the wire-compatibility policy, the per-transport security statement, and the
  dependency rule for `orbitnet-core`.

## 0.4.0 — 2026-09-17

[Release page](https://github.com/crashtestbrandt/orbitnet/releases/tag/v0.4.0). **Protocol major 8**,
unchanged from 0.3.0.

### Fixed

- An established ENet peer **rides out a stalled frame** instead of being dropped by it. A frame long enough
  to starve the poll no longer costs a connected peer its session.

### Internal

- Documentation gained a ranked list of the open maturity gaps.

## 0.3.1 — 2026-09-16

[Release page](https://github.com/crashtestbrandt/orbitnet/releases/tag/v0.3.1). **Protocol major 8**,
unchanged from 0.3.0.

### Added

- A host can **open its session before advertising it**, so a lobby is created against a session that is
  already accepting.
- The send-path window is published to the server log, which is what lets a bench read counters only the
  server holds.

### Fixed

- `netbench` imports the demo on every run and reports the errors a bringup failure logged.

## 0.3.0 — 2026-08-26

[Release page](https://github.com/crashtestbrandt/orbitnet/releases/tag/v0.3.0). **Protocol major 6 to 8.**
A 0.2.x peer and a 0.3.x one refuse each other's handshake.

### Breaking

Three `Net` calls changed meaning. Each still compiles and still runs, so the compiler reports none of them.

| Change | What breaks | What to do |
| --- | --- | --- |
| **`Net.peer_rtt_ms()` is capped** at `Net.rtt_believed_max_ms`, 250 ms by default | a scoreboard ping reads 250 for every player on a worse link | display `Net.peer_rtt_raw_ms()`, and keep `peer_rtt_ms()` for anything that feeds a rewind |
| **Resuming a seat needs `Net.set_resume_token()`** beside `Net.set_session_id()` | a game that persisted only the session id is seated as a newcomer | persist `Net.resume_token()` too, and restore both before the join |
| **A `NetCommand` validator returning a non-zero `int` is a refusal** carrying that code | a validator that returned a truthy int to mean "applied" now refuses every request | return `true`, or `NetCommand.CODE_OK` (`0`), to apply |

Wire changes forcing the two majors:

- **Major 7.** A snapshot frame may carry a trailing interest-delta section naming the slots that entered and
  left one peer's interest. The handshake and the welcome each carry a trailing resume token. The handshake's
  16-byte session key becomes a session nonce and the handshake gains a confirm tag, so with a shared secret
  configured the key is derived rather than read off the wire. The entity manifest opens with a generation and
  states a change rather than the whole table.
- **Major 8.** The interest-delta section opens with a generation, one peer's whole interest set gets a frame
  kind of its own, and a client input frame carries the interest generation that client holds. Before it, a
  section naming a slot the receiver could not resolve was dropped in silence and retired on that frame's ack,
  so the two ends disagreed about that entity for the rest of the session.

### Added

- A peer is sent its **whole interest set** when a delta cannot repair it.
- Every item on the README's Limits list is closed or narrowed.

### Fixed

- The RTS and hockey demos carry the resume token, so a restarted process keeps its seat.

## 0.2.1 — 2026-08-23

[Release page](https://github.com/crashtestbrandt/orbitnet/releases/tag/v0.2.1). **Protocol major 6**,
unchanged from 0.2.0.

### Fixed

- A **delta base the receiver rejected or has evicted** is no longer trusted. The sender stops diffing against
  a baseline the other end does not hold, which is what left an entity wrong for the rest of a session.

## 0.2.0 — 2026-08-23

[Release page](https://github.com/crashtestbrandt/orbitnet/releases/tag/v0.2.0). **Protocol major 2 to 6.**
A 0.1.x peer and a 0.2.x one refuse each other's handshake.

### Breaking

Wire changes forcing the four majors:

- **Major 3.** Every datagram but the handshake carries a sequence number and a MAC, and the handshake carries
  the session key. The session-wide schema hash is removed — schema agreement is per entity.
- **Major 4.** The hot-frame header carries an ack token the client quotes back, so an ack names a frame the
  peer provably received.
- **Major 5.** A block names its entity by a dense 16-bit session slot instead of the 64-bit id, and the entity
  manifest distributes the slot bindings for both lanes.
- **Major 6.** Each entity manifest entry carries the entity's input owner and seat, which is what distributes
  the seat roster to clients.

### Added

- **Interest** keys relevancy on a membership id as well as distance, takes a declared anchor instead of
  inferring one from input ownership, keys that anchor on a **seat** so one connection can own several
  predicted bodies, and takes a per-peer visibility veto that withholds one entity from one peer.
- **Session resume**: a dropped peer's entity is restored when it reconnects, and a seat arriving at or
  leaving a connection already in session is announced.
- **Lag compensation** derives the rewind window per target rather than from the pooled inter-arrival.
- A release build reports the **Windows Error Reporting dump path** a fail-fast would leave behind.
- The **air-hockey demo**, with a client-predicted puck on the rollback lane, and the **arena demo**, with
  three interest axes and a lag-compensated hit test.

### Changed

- The retained interest grid path gains a leave diff and an always-set, and one candidate list is shared per
  tick.
- An entity marshals a whole lane in one script-boundary crossing.

### Fixed

- The observed-interpolation estimate is scoped per peer.

## 0.1.1 — 2026-08-21

[Release page](https://github.com/crashtestbrandt/orbitnet/releases/tag/v0.1.1). **Protocol major 2**,
unchanged from 0.1.0. No addon change.

### Fixed

- The Windows binaries are built against the **msvc ABI**, which is the one a stock Godot build loads.
- A release proposes its asset manifest as a pull request rather than pushing to a protected branch.

## 0.1.0 — 2026-08-21

[Release page](https://github.com/crashtestbrandt/orbitnet/releases/tag/v0.1.0). **Protocol major 2.** First
release: the addon, the RTS demo and the test harness.

### Added

- The `Net` facade over the Rust backend, with the rollback, state and command lanes.
- Interest filtering, a send-priority rota, bandwidth accounting, and a rewind window denominated in
  milliseconds.
- A swept-sphere overlap query for hit registration, measured through the bench seam.
- Published binaries for Linux x86_64, Windows x86_64 and macOS universal, in a debug, a release and a
  profiling profile, with a committed sha256 manifest. Binaries are never committed.
