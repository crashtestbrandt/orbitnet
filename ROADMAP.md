# Roadmap

What is open, ranked. One row per issue, so the ranking is actionable rather than descriptive.

- **The tiers order the work.** Nothing here carries a date, and a lower tier waits on a higher one only
  where a row says so.
- **This file is a backlog.** `CONTRIBUTING.md`'s rule on decisions stands. A decision goes in the README, a
  `docs/` page, or the header comment of the file it governs. A row that resolves into a decision records it
  there.
- **The README's [Limits](README.md#limits) section is the companion to this one.** Limits states what is known
  and open about the shipped behavior. This file states what is planned about the project.
- **Every open issue also sits under one parent epic.** The tier headings below name the parent that owns
  their rows. A row names its own parent only where it differs from its tier's — three do.

## Tier 1 — Measurement

Parent: [#101](https://github.com/crashtestbrandt/orbitnet/issues/101) `epic(measurement)`.

Ranked first because every other tier is measured through it, and a gap here hides the rest.

| Item | Issue | Why now |
| --- | --- | --- |
| `want_full_nacks_s` has no threshold | [#69](https://github.com/crashtestbrandt/orbitnet/issues/69) | PR #70 landed the server-side window and the scheduled nightly now records it. `bench.sh:272` still reports the counter without asserting it, which is that issue's item 2; the threshold needs run history behind it before it can be picked. |

## Tier 2 — Coverage

Parent: [#102](https://github.com/crashtestbrandt/orbitnet/issues/102) `epic(coverage)`.

Surfaces no gate reaches.

| Item | Issue | Why now |
| --- | --- | --- |
| No generator reaches the wire decoders | [#79](https://github.com/crashtestbrandt/orbitnet/issues/79) | `codec`, `protocol`, `columnar`, `quant` and `slots` are hand-rolled parsers of attacker-chosen bytes, covered by hand-written cases only. |
| No test compares simulation across peers | [#80](https://github.com/crashtestbrandt/orbitnet/issues/80) | The probes gate deterministic node naming. Restore order, storage width and quantization placement can each diverge with nothing noticing. |

## Tier 3 — Encryption

Parent: [#103](https://github.com/crashtestbrandt/orbitnet/issues/103) `epic(protocol)`.

Every datagram but the handshake carries a SipHash-2-4 tag, a sequence number and a 64-entry replay window.
**No payload is encrypted under either regime** (`docs/protocol.md:566`), so a passive observer on the path
reads every position, input, manifest row and state value a session replicates.

**The zero-dependency rule no longer prices these rows.** `orbitnet-core` may take a vetted cryptographic
dependency, so a cipher or an exchange is work against a reviewed implementation rather than several hundred
hand-written lines. `native/crates/orbitnet-core/Cargo.toml`'s header carries that decision and what such a
dependency has to clear.

**This tier is scoped to the ENet path.** A Steam session's packets are already encrypted and its peer
identity already authenticated by SteamNetworkingSockets — OrbitNet contributes nothing to either, and a game
inherits both from an export-preset feature tag rather than from anything visible at a call site. See
[docs/steam.md](docs/steam.md#what-each-transport-authenticates-and-encrypts), which cites Valve's
documentation per claim.

**The tier keeps its place and its rows keep their order.** ENet is what every non-Steam export gets and the
only transport open to a project with no account service, so these rows still cover the shipped default. What
the Steam path changes is the payoff per row, not the ranking.

| Item | Issue | Why now |
| --- | --- | --- |
| Payloads travel in the clear | [#82](https://github.com/crashtestbrandt/orbitnet/issues/82) | Worth doing only under a session secret, since a client-minted key travels in the handshake. Breaks the wire. Buys confidentiality on the ENet path only. |
| A recorded join can be replayed | [#83](https://github.com/crashtestbrandt/orbitnet/issues/83) | The nonce is the joiner's choice. Closing it needs a value the acceptor contributes, and a round trip before a client may send. |
| No confidentiality without a pre-shared secret | [#84](https://github.com/crashtestbrandt/orbitnet/issues/84) | The default configuration on the ENet path, and the only one open to a game with no account service. An authenticated exchange closes it where an unauthenticated one does not. |
| The timing measurement runs in no job | [#108](https://github.com/crashtestbrandt/orbitnet/issues/108) | `native/crates/orbitnet-core/tests/constant_time.rs` gates the compare's branchlessness on every pull request and leaves its timing measurement `#[ignore]`d. A dudect-style measurement needs a low noise floor, so a shared GitHub-hosted runner cannot render a verdict on it. It belongs on the self-hosted leg, or in the scheduled netbench job that already reaches it. |

## Tier 4 — Reach

Parent: [#104](https://github.com/crashtestbrandt/orbitnet/issues/104) `epic(ci)`.

| Item | Issue | Why now |
| --- | --- | --- |
| No linux arm64 build | [#92](https://github.com/crashtestbrandt/orbitnet/issues/92) | ARM is the cheap tier at every cloud host, and a dedicated server is the build most likely to run on one. The Rust is architecture-agnostic. |
| No android or ios build | [#93](https://github.com/crashtestbrandt/orbitnet/issues/93) | The descriptor calls mobile a build-matrix question rather than a netcode one. The Platforms row is desktop only. |
| Windows and macOS run no test | [#94](https://github.com/crashtestbrandt/orbitnet/issues/94) | Both are build-only legs. A bad descriptor entry fails at `dlopen` on the affected platform and nowhere else. |
| The macOS profiling library is arm64 only | [#95](https://github.com/crashtestbrandt/orbitnet/issues/95) | The shipped dylibs are universal for a documented reason, and the profiling build does not follow it. |
| Nothing boots the published zip | [#96](https://github.com/crashtestbrandt/orbitnet/issues/96) | Every gate runs against a library it built itself. The zip a consumer installs is assembled by a path no gate exercises. |

## Tier 5 — Toward 1.0

Parent: [#105](https://github.com/crashtestbrandt/orbitnet/issues/105) `epic(release)`.

| Item | Issue | Why now |
| --- | --- | --- |
| The crate version and the addon version disagree | [#87](https://github.com/crashtestbrandt/orbitnet/issues/87) | `native/Cargo.toml` is `0.1.0`; `plugin.cfg` is `0.3.1`. Only the second is stamped from the tag. |
| No CHANGELOG | [#89](https://github.com/crashtestbrandt/orbitnet/issues/89) | Generated notes plus a hand-written README upgrade table does not carry past a third minor. |
| The send path is single-threaded | [#91](https://github.com/crashtestbrandt/orbitnet/issues/91) | `docs/architecture.md:214` already names what is movable and why a worker pool should be feature-gated above a peer threshold. Parent [#106](https://github.com/crashtestbrandt/orbitnet/issues/106) `epic(perf)`, not this tier's: it is send-path cost, and neither it nor #65 can be accepted before Tier 1 has a recorded baseline. |

## Also filed

| Issue | Item |
| --- | --- |
| [#65](https://github.com/crashtestbrandt/orbitnet/issues/65) | The `ss3` respack paid per admitted block on the send path, below current bench resolution. Parent [#106](https://github.com/crashtestbrandt/orbitnet/issues/106) `epic(perf)`, with #91. |

## Recorded and not scheduled

Positions the repository already argues for, each with its reasoning recorded beside the code it governs.
Listed so the roadmap is not read as a list of oversights.

- **The interest grid stays unused.** It is written and tested and applies the same rules as the flat scan,
  and it is slower at the extents a session runs at. `net.perf`'s `interest_ms` is the number that would
  reopen it (`docs/architecture.md:114`).
- **Web is out.** Godot's web export cannot load a GDExtension at all, so no build matrix reaches it.
- **An unauthenticated X25519 exchange is declined.** It is substituted by exactly the on-path attacker it
  would defend against, so it buys a demotion to passive-only and nothing more. That reason stands on its own:
  the hand-written cost that used to sit beside it is gone, because `orbitnet-core` may now take a vetted
  cryptographic dependency (`native/crates/orbitnet-core/Cargo.toml`). #84 is the authenticated form, which is
  a different trade and ordinary work.
- **`reloadable` stays false.** A netcode singleton owns sockets, tick state and history, and hot reload is
  the least-exercised corner of the toolchain.
