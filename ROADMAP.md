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


**Every row here has landed. The parent can close.**

## Tier 3 — Encryption

Parent: [#103](https://github.com/crashtestbrandt/orbitnet/issues/103) `epic(protocol)`.

**Every row here has landed, and the tier's premise no longer holds.** It opened on "no payload is encrypted
under either regime". A session that configures `Net.set_session_secret()` now runs ChaCha20-Poly1305 over
every datagram, a recorded join can no longer be replayed, a client may pin the server's static key and derive
the session key from an authenticated X25519 exchange, and the constant-time measurement runs nightly on the
self-hosted box.

What did not change: a session that configures neither still carries every payload in the clear, because both
nonce halves cross the wire and whoever can read the payload could compute the key that hid it. That is stated
where a reader will be standing — [docs/protocol.md](docs/protocol.md#two-regimes-and-which-one-you-are-in)
and the README's Limits section — rather than as a backlog row. The parent can close.

## Tier 4 — Reach

Parent: [#104](https://github.com/crashtestbrandt/orbitnet/issues/104) `epic(ci)`.


**Every row here has landed. The parent can close.**

## Tier 5 — Toward 1.0

Parent: [#105](https://github.com/crashtestbrandt/orbitnet/issues/105) `epic(release)`.

| Item | Issue | Why now |
| --- | --- | --- |
| The send path is single-threaded | [#91](https://github.com/crashtestbrandt/orbitnet/issues/91) | `docs/architecture.md:214` already names what is movable and why a worker pool should be feature-gated above a peer threshold. Parent [#106](https://github.com/crashtestbrandt/orbitnet/issues/106) `epic(perf)`, not this tier's: it is send-path cost. Its deliverable is a peer threshold **measured rather than picked**, so it waits on Tier 1 having netbench history the threshold can be read off. |

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
  cryptographic dependency (`native/crates/orbitnet-core/Cargo.toml`). The **authenticated** form is what
  shipped: an X25519 exchange against a server key the client pinned, which is a different trade entirely.
- **`reloadable` stays false.** A netcode singleton owns sockets, tick state and history, and hot reload is
  the least-exercised corner of the toolchain.
