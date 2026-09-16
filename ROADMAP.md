# Roadmap

What is open, ranked. One row per issue, so the ranking is actionable rather than descriptive.

- **The tiers order the work.** Nothing here carries a date, and a lower tier waits on a higher one only
  where a row says so.
- **This file is a backlog.** `CONTRIBUTING.md`'s rule on decisions stands. A decision goes in the README, a
  `docs/` page, or the header comment of the file it governs. A row that resolves into a decision records it
  there.
- **The README's [Limits](README.md#limits) section is the companion to this one.** Limits states what is known
  and open about the shipped behavior. This file states what is planned about the project.

## Tier 1 — Measurement

Ranked first because every other tier is measured through it, and a gap here hides the rest.

| Item | Issue | Why now |
| --- | --- | --- |
| `bench-check` runs locally and not in CI | [#74](https://github.com/crashtestbrandt/orbitnet/issues/74) | `compare.py --self-test` is in `just check` and absent from `check.yml`, so the rules that judge every performance claim are ungated. |
| Nothing runs netbench on a schedule | [#75](https://github.com/crashtestbrandt/orbitnet/issues/75) | No workflow carries a `schedule:`. Without run history, no column can be given a threshold. |
| `want_full_nacks_s` has no threshold | [#69](https://github.com/crashtestbrandt/orbitnet/issues/69) | PR #70 landed the server-side window. `bench.sh:272` still reports the counter without asserting it, which is that issue's item 2. Blocked on #75. |
| Every impairment run is loopback | [#76](https://github.com/crashtestbrandt/orbitnet/issues/76) | A relayed link reorders and duplicates on its own schedule. `gauntlet.sh` can measure one and has no recorded run. |

## Tier 2 — Coverage

Surfaces no gate reaches.

| Item | Issue | Why now |
| --- | --- | --- |
| The Steam transport has no test | [#77](https://github.com/crashtestbrandt/orbitnet/issues/77) | 938 lines, no suite and no probe. `transport_factory_test.gd` asserts the harness is not a Steam build, so the branch is covered as not taken. |
| `sync.rs` has no Rust test | [#78](https://github.com/crashtestbrandt/orbitnet/issues/78) | 123 KB and zero `#[test]`, in a workspace carrying 543 of them. |
| No generator reaches the wire decoders | [#79](https://github.com/crashtestbrandt/orbitnet/issues/79) | `codec`, `protocol`, `columnar`, `quant` and `slots` are hand-rolled parsers of attacker-chosen bytes, covered by hand-written cases only. |
| No test compares simulation across peers | [#80](https://github.com/crashtestbrandt/orbitnet/issues/80) | The probes gate deterministic node naming. Restore order, storage width and quantization placement can each diverge with nothing noticing. |

## Tier 3 — Encryption

Every datagram but the handshake carries a SipHash-2-4 tag, a sequence number and a 64-entry replay window.
**No payload is encrypted under either regime** (`docs/protocol.md:566`), so a passive observer on the path
reads every position, input, manifest row and state value a session replicates.

| Item | Issue | Why now |
| --- | --- | --- |
| The zero-dependency rule against cryptography | [#81](https://github.com/crashtestbrandt/orbitnet/issues/81) | The gating decision. Every row below costs several hundred hand-written lines under the rule and ordinary work without it. Settle it first. |
| Payloads travel in the clear | [#82](https://github.com/crashtestbrandt/orbitnet/issues/82) | Worth doing only under a session secret, since a client-minted key travels in the handshake. Breaks the wire. |
| A recorded join can be replayed | [#83](https://github.com/crashtestbrandt/orbitnet/issues/83) | The nonce is the joiner's choice. Closing it needs a value the acceptor contributes, and a round trip before a client may send. |
| No confidentiality without a pre-shared secret | [#84](https://github.com/crashtestbrandt/orbitnet/issues/84) | The default configuration, and the only one open to a game with no account service. An authenticated exchange closes it where an unauthenticated one does not. |
| Which transport encrypts is unstated | [#85](https://github.com/crashtestbrandt/orbitnet/issues/85) | ENet carries raw UDP. A relayed transport may encrypt at its own layer, which would scope the rows above to the native path. |
| No timing harness | [#86](https://github.com/crashtestbrandt/orbitnet/issues/86) | Named in `auth.rs:36-41` as half the reason X25519 was declined. Prerequisite for #84. |

## Tier 4 — Reach

| Item | Issue | Why now |
| --- | --- | --- |
| No linux arm64 build | [#92](https://github.com/crashtestbrandt/orbitnet/issues/92) | ARM is the cheap tier at every cloud host, and a dedicated server is the build most likely to run on one. The Rust is architecture-agnostic. |
| No android or ios build | [#93](https://github.com/crashtestbrandt/orbitnet/issues/93) | The descriptor calls mobile a build-matrix question rather than a netcode one. The Platforms row is desktop only. |
| Windows and macOS run no test | [#94](https://github.com/crashtestbrandt/orbitnet/issues/94) | Both are build-only legs. A bad descriptor entry fails at `dlopen` on the affected platform and nowhere else. |
| The macOS profiling library is arm64 only | [#95](https://github.com/crashtestbrandt/orbitnet/issues/95) | The shipped dylibs are universal for a documented reason, and the profiling build does not follow it. |
| Nothing boots the published zip | [#96](https://github.com/crashtestbrandt/orbitnet/issues/96) | Every gate runs against a library it built itself. The zip a consumer installs is assembled by a path no gate exercises. |

## Tier 5 — Toward 1.0

| Item | Issue | Why now |
| --- | --- | --- |
| The crate version and the addon version disagree | [#87](https://github.com/crashtestbrandt/orbitnet/issues/87) | `native/Cargo.toml` is `0.1.0`; `plugin.cfg` is `0.3.1`. Only the second is stamped from the tag. |
| No wire-compatibility policy | [#88](https://github.com/crashtestbrandt/orbitnet/issues/88) | `PROTOCOL_VERSION` moved major 6 to 8 inside the 0.3 line. A consumer has no rule to plan a pin against, and no statement of what 1.0 freezes. |
| No CHANGELOG | [#89](https://github.com/crashtestbrandt/orbitnet/issues/89) | Generated notes plus a hand-written README upgrade table does not carry past a third minor. |
| Entity identity does not survive a re-parent | [#90](https://github.com/crashtestbrandt/orbitnet/issues/90) | An id is FNV-1a of the node path, so moving a node is a despawn and a spawn. A protocol decision either way, worth taking deliberately. |
| The send path is single-threaded | [#91](https://github.com/crashtestbrandt/orbitnet/issues/91) | `docs/architecture.md:214` already names what is movable and why a worker pool should be feature-gated above a peer threshold. |

## Also filed

| Issue | Item |
| --- | --- |
| [#64](https://github.com/crashtestbrandt/orbitnet/issues/64) | One join-target parser shared between the demos and the bench relay, rather than four restatements. |
| [#65](https://github.com/crashtestbrandt/orbitnet/issues/65) | The `ss3` respack paid per admitted block on the send path, below current bench resolution. |

## Recorded and not scheduled

Positions the repository already argues for, each with its reasoning recorded beside the code it governs.
Listed so the roadmap is not read as a list of oversights.

- **The interest grid stays unused.** It is written and tested and applies the same rules as the flat scan,
  and it is slower at the extents a session runs at. `net.perf`'s `interest_ms` is the number that would
  reopen it (`docs/architecture.md:114`).
- **Web is out.** Godot's web export cannot load a GDExtension at all, so no build matrix reaches it.
- **An unauthenticated X25519 exchange is declined.** It is substituted by exactly the on-path attacker it
  would defend against, at the cost of several hundred lines of hand-written field arithmetic
  (`auth.rs:36-41`). #84 is the authenticated form, which is a different trade.
- **`reloadable` stays false.** A netcode singleton owns sockets, tick state and history, and hot reload is
  the least-exercised corner of the toolchain.
