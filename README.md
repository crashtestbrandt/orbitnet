![OrbitNet](docs/img/banner.png)

<sub>Godot render. Earth surface from NASA Visible Earth (Blue Marble Next Generation), city lights from NASA
Earth Observatory (Black Marble 2016) and Milky Way from NASA/SVS *Deep Star Maps 2020*. Star positions from the Yale Bright Star Catalog, 5th edition.</sub>

**Rollback netcode for Godot 4, in Rust.** Server-authoritative replication, owner prediction and
reconciliation, batched delta state sync, clock discipline, and interest management — behind one GDScript
facade.

```gdscript
# A replicated, client-predicted player body.
Net.register_rollback_body(
    self, $Input,
    ["position@half", "velocity"],   # state:  server-authored
    ["move", "jump"],                # input:  client-authored, server-validated
    is_multiplayer_authority())
```

```sh
git clone https://github.com/crashtestbrandt/orbitnet && cd orbitnet
just native-install
just rts                         # 96-unit RTS, single player, no networking
just hockey                      # air hockey, a puck every peer predicts
just rts-host                    # then `just rts-join` in another terminal
```

`native-install` builds the extension for this host; a `git clone` carries no binaries.

> **0.x.** `Net` is the surface intended to be stable. The wire format and the Rust internals may change
> between minor versions. Pin a tag.

## Three lanes

Choosing the right one is most of what there is to learn.

| Lane | For | Cost |
|---|---|---|
| **Rollback** — `Net.register_rollback_body()` | an entity whose owner authors continuous per-tick input and predicts locally | a history ring + per-tick compare and replay, per entity |
| **State** — `Net.make_state()` | server-authoritative values pushed every tick, no prediction | one delta block per entity per tick |
| **Command** — `NetCommand` | sparse, discrete, reliable, server-validated requests | one reliable RPC per request |

**The rollback lane restores recorded history onto its properties every tick.** A value written from *outside*
the tick — a command handler, a timer, a signal — is silently overwritten. Those belong on the state lane.
This is the most common OrbitNet bug and it raises no error.

## Install

From a [release](https://github.com/crashtestbrandt/orbitnet/releases), use *AssetLib → Install from file* on
the `orbitnet-*.zip`, then enable **OrbitNet** in *Project → Project Settings → Plugins*. Binaries in the zip
are plain files, not LFS pointers, so they work straight out of it.

To install by hand, copy **both** `addons/orbitnet/` and `addons/orbitnet_native/` from that zip into your
project. Both directories are required: `Net` without the extension is a facade over nothing.

**A `git clone` of this repository carries no binaries** — `addons/orbitnet_native/bin/` is gitignored, and
`just native-install` builds this host's copy. A release also attaches the libraries individually, plus a
`binaries.json` naming the size and sha256 of each, so a project can pin a tag, fetch what its
`.gdextension` names, and verify it.

| | |
|---|---|
| **Godot** | 4.4+ (built against the 4.4 API; loads in anything at or above it) |
| **Language** | GDScript. No C# bindings. |
| **Platforms** | Linux x86_64, Windows x86_64, macOS universal |
| **Transports** | ENet out of the box; Steam via [GodotSteam](https://godotsteam.com/), selected by export-preset feature tag |
| **Not supported** | Web — Godot's web export cannot load a GDExtension |

## Docs

| | |
|---|---|
| [getting-started.md](docs/getting-started.md) | Your first replicated body. **Start here.** |
| [api.md](docs/api.md) | The full surface, wire quantization, and the f64/i64 scalar reality. |
| [rts-demo.md](docs/rts-demo.md) | A worked example that is not a character shooter, with the byte budget spelled out. |
| [hockey-demo.md](docs/hockey-demo.md) | The rollback lane on an object nobody authors, and the correction measured in millimetres. |
| [arena-demo.md](docs/arena-demo.md) | Three interest axes, several seats on one connection, and a rewind sized per shooter and per target. |
| [architecture.md](docs/architecture.md) | Crate layout, batching, history, prop roles, threading. |
| [protocol.md](docs/protocol.md) | Wire format, clock, `is_fresh`, entity lifecycle. |
| [netbench.md](docs/netbench.md) | Impairment relay, bot fleet, tick-domain gates. |
| [building.md](docs/building.md) | Rust toolchain and the binary distribution policy. |
| [steam.md](docs/steam.md) | The Steam transport contract. |
| [crash-capture.md](docs/crash-capture.md) | What a release build records when it dies, and the Windows fail-fast gap. |
| [CONTRIBUTING.md](CONTRIBUTING.md) | Layout, the enforced boundaries, the GDScript rules. |

## The RTS demo

`demos/rts/` is a two-seat skirmish RTS — 96 units, orders, combat — and it exists to show that **which lane
an entity belongs on is decided by the game**:

- **Rollback: one entity per player**, the command cursor. The only thing an RTS continuously authors, and the
  **AOI anchor** — with no rollback body per peer, `set_aoi_radius()` cannot function at all.
- **State: every unit.** Their input is a sparse order, so there is nothing to predict.
- **Command: every order**, one channel per seat.

Its signature number is **order RTT** — click → validate → adjudicate → *observed* — which is what a player
feels and what ping does not measure. Six keybound levers change it live.

## The air hockey demo

![A client's view of a three-peer air hockey session](docs/img/hockey-demo.png)

<sub>A **client** at seat 2, with a host and one other client connected, all three driven by bench bots that
chase the puck continuously. `p50=141.6 mm` is how far this peer's predicted puck was from the authoritative
one — roughly the distance the puck travels in one round trip, which is the number a rollback demo exists to
show. Each orange spike is one correction. The two blue mallets at the near end are team 0's; the one behind is
a team-mate, faded because it overlaps.</sub>

`demos/hockey/` is the coupled 60 Hz counterpart — the configuration the RTS demo's `project.godot` names as
unable to coexist with its own — and it exists to show that **the rollback lane is not only for the body you
author**:

- **Rollback: the puck**, registered with an *empty input list* and `predict = true` on every peer. Nobody
  authors it, so every peer simulates it locally and reconciles against the server.
- **Rollback: every mallet**, server-owned state and client-owned input, on a 32-seat static pool with players
  seated on alternating ends as they arrive.
- **State: the scoreboard**, because a goal found inside the tick would be erased by the next rollback restore.
- **Command: `serve`**, one channel, refused while the puck is live.

Its signature number is the **puck correction in millimetres** — the distance between what this peer predicted
for a tick and what the server said about it, reported beside the wire quantization floor that bounds it.

## The arena demo

`demos/arena/` is the third configuration — **decoupled at 30 Hz with a 128-tick history**, which is what a
shooter wants and what neither other demo has. It exists to show that **who receives what is decided by the
game**:

- **Three arenas in one session**, each rebased to its own origin. What replicates is arena-local, so two
  fighters standing on the same spot in different arenas are zero metres apart — no radius can separate them,
  and `set_membership()` is the only thing in the facade that can.
- **A cloak is a per-peer veto.** `Net.set_entity_hidden()` withholds one fighter from the connections not on
  its team, which is a fact about a *pair* and therefore the one thing neither distance nor membership can
  say. A withheld client is told: `Net.entity_left_interest` fires, and the probe asserts on it.
- **Split-screen is seats.** `--seats=2` drives two locally-predicted fighters on one connection, each with
  its own interest anchor, in two different arenas by default; the connection receives the union.
- **An observer declares where it watches from.** No seat, no body, and therefore no inferred centre or world
  — `Net.set_peer_anchor()` supplies both, and a peer arriving at a full table is admitted rather than
  refused.
- **The scorecard is membership only.** It replicates two integers and no position, so there is no distance to
  cull it by.
- **`just arena-slots 8000` is the slot-table lever.** 24,027 entities in one session, and the readout that
  shows a whole 530 kB manifest becoming a two-row delta for the peer that was already up to date.

Its signature number is the **rewind depth per band** — the three ticks one shot is resolved at, one per
distance band, beside the flat per-shooter window they refine.

## Limits

Known and filed, not hidden.

### The addon reports; the game decides

Three of these are one shape. OrbitNet now publishes a fact it used to keep to itself, and still acts on none
of them — freeing a body, releasing a seat and refusing a resume are game decisions, and a default that made
any of them would be wrong for somebody.

- **Nothing despawns, and a client is now told which entities stopped.** A culled entity — or one
  `Net.set_entity_hidden()` withholds — freezes at its last received pose rather than leaving the scene. The
  per-peer diff the send path already computed rides the snapshot as a flag-guarded trailing section, two
  bytes per changed entity on the ticks that changed and nothing at rest, and reaches the game as
  `Net.entity_left_interest` / `Net.entity_entered_interest`. `Net.entities_in_interest()` answers the same
  question for a handler bound mid-session. **The addon still frees nothing.** Hide rather than free: a
  nearest-N eviction can oscillate at the boundary, and freeing turns that into spawn churn.
- **Releasing a seat is the game's call, and a dropped connection keeps its own by default.** Its bodies hold
  the authority they were given until the game changes them, which is what the reconnect grace window is for.
  `Net.set_seat_release_policy()` says otherwise in one call, and `Net.release_peer_seats(peer)` does it
  directly under any policy. Freeing the node is still yours.
- **A peer that declares nothing still has its centre and world inferred.** Without `Net.set_peer_anchor()` or
  `set_peer_anchor_entity()` both are read off the lowest-id rollback entity each of that peer's **seats**
  drives — so a seat driving more than one body is placed by whichever that is, and a peer driving none has
  neither and sees everything, uncapped, because an entity with no distance is kept uncullable and an
  uncullable entity occupies no slot in `set_aoi_max_entities()`. The inference and its default are unchanged.
  What is new: `Net.peer_anchor()` reports what is actually in effect, an inference whose dropped bodies
  disagreed about the **world** warns once, and `Net.set_unanchored_policy(CLOSED)` makes "declare nothing"
  mean "receive nothing".

### Security

- **A session identity is client-asserted; the token narrows what asserting one buys.** The server mints a
  **resume token** per identity, sends it in the welcome, and a rejoiner must quote it back — so a peer that
  merely *observed* somebody's session id, off a roster broadcast or a log line, can no longer take that
  player's body. **An on-path observer still can**, because it reads the welcome the token travelled in. Under
  the default policy a valid claim still beats a live connection, which is what makes a relaunched client's
  reconnect immediate rather than waiting out a keepalive; `Net.set_resume_policy(ONLY_IF_DROPPED)` refuses
  that, at the cost of every genuinely fast reconnect. **Persist the token beside the session id**, or a
  restarted process cannot resume.
- **The session key crosses the wire in the clear unless the game supplies a secret.** Every datagram but the
  handshake carries a MAC and a replay sequence. With no secret configured the handshake carries the key they
  are checked with: an attacker who cannot read the session's traffic cannot forge a datagram and one connected
  peer cannot forge another's, but **an on-path observer who reads the handshake can do everything the client
  can**. `Net.set_session_secret()` changes that — the handshake's 16 bytes become a **nonce**, the key is
  derived from it and the secret, and an observer reading the handshake learns nothing. The secret has to come
  from a channel the game already authenticates: a lobby, a matchmaker ticket. **None of this encrypts
  anything**, the ceiling is still a 64-bit tag and a 128-bit key, and an on-path observer can still **replay**
  a join it recorded — the nonce is the client's choice, so presenting it again derives the same key. It
  authors nothing new and the captured datagrams land nowhere, but closing that too needs a value the acceptor
  contributes and therefore a second round trip before a client may send anything.
  An X25519 exchange was considered and declined: unauthenticated ECDH is substituted by exactly the on-path
  attacker this bullet is about, so it would demote the adversary to passive-only in exchange for several
  hundred lines of hand-written constant-time field arithmetic in a zero-dependency crate with no timing
  harness to prove it stayed constant-time.
- **A peer's reported round trip is checked, and what the server believes is bounded.** The server mints a
  token per snapshot frame from a secret it never transmits and refuses any acknowledgement that does not quote
  it back, so a peer cannot acknowledge a frame that never reached it. It can still acknowledge a frame
  **older** than the newest it holds, which reads as a slow link and is believed — indistinguishable from a
  peer behind a traffic shaper, and no wire field closes it, because `current - ack` is the whole round trip
  whatever tick lead a client runs at. The containment is `Net.rtt_believed_max_ms`, 250 ms by default: the
  sample is clamped **at the read**, so every acknowledgement still buys everything else it bought and only
  the clock measurement is bounded. `Net.peer_rtt_raw_ms()` keeps a scoreboard ping honest, and
  `bandwidth_metrics()["rtt_at_ceiling_peers"]` says how many connections are asking for the deepest window.

### Scale and validation

- **A session can name 65,536 entities on the wire.** A block carries a 16-bit slot rather than the 64-bit
  entity id ([docs/protocol.md](docs/protocol.md#entity-slots)). Past the cap the server refuses to replicate
  the entity and says so, rather than wrapping an index onto a live one. The slot table is distributed as a
  **delta against a generation the receiver holds**, so a session whose entities churn no longer restates
  bindings that did not move, and a joining peer no longer costs every existing peer a whole table.
- **Input values are checked for finiteness and nothing else.** A non-finite float in a decoded input row is
  refused before it enters history — not as game policy, but because it is a poison value that would be
  restored onto the input node before every replayed tick, recorded into state, sent to every peer, and would
  make the body uncullable so it replicated to all of them. **Range, rate and plausibility are still yours**,
  inside `_rollback_tick`, on the server. The full split is in
  [docs/protocol.md](docs/protocol.md#what-the-receive-path-refuses-and-what-it-does-not).

## Licence

**MIT OR Apache-2.0**, at your option. See [LICENSE](LICENSE).

The compiled extension links godot-rust (gdext), which is MPL-2.0 — *file-scoped* copyleft, with no gdext file
modified here, so shipping a game with OrbitNet inherits no copyleft obligation. Full reasoning and dependency
inventory: [THIRD_PARTY.md](THIRD_PARTY.md).

Contributions are dual-licensed the same way. Inbound equals outbound; no CLA.
