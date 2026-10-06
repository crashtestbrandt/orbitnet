# Roadmap

What is open, ranked. One row per issue, so the ranking is actionable rather than descriptive.

- **The tiers order the work.** Nothing here carries a date, and a lower tier waits on a higher one only
  where a row says so.
- **This file is a backlog.** `CONTRIBUTING.md`'s rule on decisions stands. A decision goes in the README, a
  `docs/` page, or the header comment of the file it governs. A row that resolves into a decision records it
  there.
- **The README's [Limits](README.md#limits) section is the companion to this one.** Limits states what is known
  and open about the shipped behavior. This file states what is planned about the project.
- **Every open issue sits under one parent epic, except where its row says it has none.** The tier headings
  below name the parent that owns their rows.

**Four epics own every open row, and 1.0 waits on all four.**
[What 1.0 freezes](docs/protocol.md#what-10-freezes) is the policy the first tier gives substance to. Every
epic that preceded them landed whole in [0.5.0](CHANGELOG.md#050--2026-10-06): measurement (#101), coverage
(#102), encryption (#103), reach (#104), release (#105) and the performance epic under it (#106).

## Tier 1 — API freeze

Parent: [#156](https://github.com/crashtestbrandt/orbitnet/issues/156) `epic(api)`.

Ranked first because 1.0 is a promise about the `Net` surface, and nothing yet shows that surface has stopped
moving. 0.5.0 changed what one `Net` call does, added six key calls, and changed `NetLagComp` at runtime.

| Issue | What it is | Waits on |
| --- | --- | --- |
| [#157](https://github.com/crashtestbrandt/orbitnet/issues/157) | A refused join reaches the joining peer, as a reject frame and `Net.join_refused(reason)`. Today the accepting peer logs why and the joiner watches for a welcome that never arrives; `docs/protocol.md`'s misconfiguration table records two rows as "the join hangs". | nothing; batches with any pending wire major |
| [#158](https://github.com/crashtestbrandt/orbitnet/issues/158) | The deprecation path the versioning policy describes: a warning on the first call of a deprecated method, and the marker in `docs/api.md`. `net.gd` emits none today. | nothing |
| [#159](https://github.com/crashtestbrandt/orbitnet/issues/159) | One review of the public surface before the freeze, and the classes the freeze covers stated in `docs/api.md`. The exit condition is one release that moves none of them. | #160, for its scope |
| [#160](https://github.com/crashtestbrandt/orbitnet/issues/160) | The recorded 1.x position on host migration and despawn, both of which the README's Limits leave to the game today. | nothing |

## Tier 2 — Validation

Parent: [#161](https://github.com/crashtestbrandt/orbitnet/issues/161) `epic(validation)`.

Every measurement to date is a 25-second loopback bench or a LAN run. A server panicked in its send path on
every nightly from September 26 to October 3, 2026 and the bench reported PASS until a panic check was added.

| Issue | What it is | Waits on |
| --- | --- | --- |
| [#162](https://github.com/crashtestbrandt/orbitnet/issues/162) | A multi-hour soak on the fleet with join and leave churn under loss, watching memory and the per-peer tables, repeated weekly. | nothing |
| [#163](https://github.com/crashtestbrandt/orbitnet/issues/163) | One recorded run with the authority across a WAN hop, with a NAT and the path MTU in play. `docs/netbench.md` calls both "a run across machines only", and the fleet is one LAN. | nothing |
| [#164](https://github.com/crashtestbrandt/orbitnet/issues/164) | The Linux arm64 and Android libraries run on a device, or carry an experimental label in the README. | nothing |

## Tier 3 — Security

Parent: [#165](https://github.com/crashtestbrandt/orbitnet/issues/165) `epic(security)`.

0.5.0 added a payload cipher and an authenticated exchange, and the review that landed them found the pinned
join could not complete. Three rows publish the regimes as a reviewed feature.

| Issue | What it is | Waits on |
| --- | --- | --- |
| [#166](https://github.com/crashtestbrandt/orbitnet/issues/166) | The exchange fold and the cipher key derived with a standard KDF instead of keyed SipHash, which makes no PRF or extractor claim. A protocol major. | nothing; batches with any pending major |
| [#167](https://github.com/crashtestbrandt/orbitnet/issues/167) | `SECURITY.md` with the reporting path, the disclosure window and the threat model per regime, linked from the README. | nothing |
| [#168](https://github.com/crashtestbrandt/orbitnet/issues/168) | One external read of the key schedule, the handshake and the regime table, with its findings filed or accepted. | #166, so it reads what ships |

## Tier 4 — Community

Parent: [#169](https://github.com/crashtestbrandt/orbitnet/issues/169) `epic(community)`.

What a reader who finds the addon on the Asset Library needs: a way to install it by name, a reason to pick
it, a path from C#, and a repository that answers.

| Issue | What it is | Waits on |
| --- | --- | --- |
| [#170](https://github.com/crashtestbrandt/orbitnet/issues/170) | The Asset Library listing with binaries, as a release-zip download or a distribution repository. The Asset Library installs a repository archive, and this repository carries no binaries by rule. | nothing |
| [#171](https://github.com/crashtestbrandt/orbitnet/issues/171) | A showcase capture of the demos, and one numbers page behind the README's comparison. | nothing |
| [#172](https://github.com/crashtestbrandt/orbitnet/issues/172) | A C# page: calling the facade from C#, or a thin wrapper shipped with the addon. | #159, for the surface it covers |
| [#173](https://github.com/crashtestbrandt/orbitnet/issues/173) | Issue templates, a discussion venue, a release checklist, and a manifest PR that triggers its own checks. | nothing |

## Open without a parent

| Issue | What it is | Waits on |
| --- | --- | --- |
| [#149](https://github.com/crashtestbrandt/orbitnet/issues/149) | `step_coupled` does not shift the clock's offset window by the tick a slew adds or drops, the way `step_decoupled` shifts it by its stretch. A model shows reversed slews at a 120 Hz coupled rate, where the slew cooldown ends while half the window predates the slew; at 60 Hz the cooldown outlasts the window and nothing changes. | nothing |

## Recorded and not scheduled

Positions the repository already argues for, each with its reasoning recorded beside the code it governs.
Listed so the roadmap is not read as a list of oversights.

- **The interest grid stays unused.** It is written and tested and applies the same rules as the flat scan,
  and it is slower at the extents a session runs at. `net.perf`'s `interest_ms` is the number that would
  reopen it (`docs/architecture.md`, under the interest pass).
- **Web is out.** Godot's web export cannot load a GDExtension at all, so no build matrix reaches it.
- **An unauthenticated X25519 exchange is declined.** It is substituted by exactly the on-path attacker it
  would defend against, so it buys a demotion to passive-only and nothing more. That reason stands on its own:
  the hand-written cost that used to sit beside it is gone, because `orbitnet-core` may now take a vetted
  cryptographic dependency (`native/crates/orbitnet-core/Cargo.toml`). The **authenticated** form is what
  shipped: an X25519 exchange against a server key the client pinned, which is a different trade entirely.
- **A session that configures neither secret nor pin carries every payload in the clear.** Both nonce halves
  cross the wire, so whoever can read the payload could compute the key that hid it. Stated where a reader
  will be standing, in [docs/protocol.md](docs/protocol.md#three-secret-regimes-and-which-one-you-are-in) and
  the README's Limits, rather than as a backlog row.
- **`reloadable` stays false.** A netcode singleton owns sockets, tick state and history, and hot reload is
  the least-exercised corner of the toolchain.
