# Contributing

Short, because most of the rules are enforced rather than described.

```sh
just native-install  # required once after cloning: builds the extension and syncs it
just check           # everything a PR must pass
```

`check` runs fastest-failing first: `addon-tracked` → `addon-drift` → `net-check` → `descriptor-parity` →
`bench-check` → cargo gates → lint → unit suites → the server-shape probe → the two-peer RTS probe → the
arena probe.

## Layout, and where to edit

The repository root is **not** a Godot project — OrbitNet is configured through an `[orbitnet]` block in
`project.godot`, and the demos disagree about those values on purpose, so each is its own project.

```
addons/orbitnet/          CANONICAL addon source — the AssetLib payload
addons/orbitnet_native/   the .gdextension + the binaries manifest (bin/ is gitignored)
native/                   the Rust workspace
harness/                  the addon's own suites + the load smoke
demos/rts/                the RTS demo
```

**The two `addons/` directories at the root are canonical.** Every project gets a mirror-copy from
`tools/sync-addons.sh`; those copies are gitignored build artifacts. Two gates guard that:
`just addon-drift` fails if you edited a copy instead of the canonical source (local — it needs the copies to
exist), and `just addon-tracked` fails if a copy was ever *committed*, which is what CI runs because a fresh
checkout has no copies to drift.

It **copies** rather than symlinks because Git for Windows checks a symlink out as a text file containing the
path unless `core.symlinks` *and* Developer Mode are both on — a fatal, cryptic first-run failure.
`ORBITNET_LINK=1 tools/sync-addons.sh` symlinks instead, for Unix devs iterating on `net.gd`.

## The two boundaries

The first is grep-enforced by `tools/net-check.sh`. The second is a rule the pattern does not yet carry, so
it holds by review rather than by the gate:

- **`net.gd` is the only file that may name a backend class** (`OrbitNet`, `OrbitRollbackSynchronizer`,
  `OrbitStateSynchronizer`, `OrbitInterpolator`).
- **`steam_transport.gd` is the only file that may name Steamworks.** Every Steam access is dynamic, so a
  non-Steam build carries zero Steam dependency.

The gate scans the **demos** too. A demo that needs to reach past the facade has found a hole *in the facade*
— widen the facade, do not exempt the caller. That is how `NetInterpolatorHandle` came to exist.

## GDScript rules

Promoted from warnings to **errors** in each project's `project.godot`. They catch a real class of netcode
bug: an untyped value crossing the wire boundary is how a `Variant` ends up where a typed value was assumed.

- **Every `var`, parameter and return type is annotated.**
- **`Array` and `Dictionary` are always typed**: `Array[Node]`, `Dictionary[String, int]`.
- **Do not `as`-cast a `Variant`, and do not pass one to a typed constructor.** *Assign* it to a typed local
  instead — that conversion is allowed, the cast is not. The most common review note, and it comes up on every
  wire-decoded payload.
- **`@warning_ignore` is banned** unless a comment explains the false positive.
- **No Python idioms**: `size()` not `len()`, `push_back()` not `append()`, `append_array()` not `+=`, `null`
  not `None`.
- **Prefer `%UniqueName` over `$Path/To/Node`.** Enums over magic values.

One deliberate exception: `exclude_addons=true`, because the facade's opaque-`Node` backend calls cannot
satisfy `unsafe_method_access` by construction — that is what makes it a facade.

## Coverage: default to a unit test

Decide in this order.

**Pure function?** — constructible from plain data, no `SceneTree`, `PhysicsServer`, `MultiplayerPeer` or
`RenderingDevice` → **a `tests/unit/*_test.gd` suite**, running in about a second. Steering math, the order
validator, wire packing, the seat roster, the transport factory, the tape codec, the impairment scheduler —
all pure, all unit tests.

Logic on a `Node` is usually *still* unit-testable via `.new()` without calling `_ready()`, and static methods
need no instance at all. Do not reach for a probe just because a class extends `Node`.

**Genuinely needs a live scene, physics or network?** → a probe: a driver script under `tools/`, plus its
instrumentation inside the project it drives (`tools/instr/` in a demo, a scene of its own in `harness/`). But
a probe gates PRs **only** if it guards a fundamental netcode regression: rollback determinism, prediction and
reconciliation, two-peer sync, dedicated-server boot, interest filtering, the facade, or the transport factory.

Three probes gate PRs today, and each reaches one item on that list the other two cannot:

| Probe | What it guards |
| --- | --- |
| `tools/rts-probe.sh` | two-peer sync: identical worlds, orders replicated, a forged order refused |
| `tools/server-shape-probe.sh` | both **server shapes** end to end -- a joining client's own state channel delivers rows against a dedicated server and against a listen server alike |
| `tools/arena-probe.sh` | **interest filtering**: membership across three worlds, a per-peer veto, several seats on one connection, a declared anchor, and a session resume |

**A fourth needs a line on that list none of the three already covers.** If a probe's assertions are pure math,
port them to a unit test and delete the redundant block. And a probe that can only ever pass is not coverage:
the shape probe carries its own negative control (a run with the channel under test vetoed, asserted to FAIL)
and the arena probe asserts a withheld entity's rows stop while its neighbors keep arriving, for exactly that
reason.

An empty run is a failure, not a pass — the runner exits non-zero on no suites or no `test_*` methods.

## Measuring a change

`just check` gates correctness. **A change to the send path, the interest pass or the wire format also has to
be measured**, and `just netbench` is what measures it — see
[docs/netbench.md](docs/netbench.md#comparing-two-runs).

```sh
NETBENCH_OUT=/tmp/nb-before just netbench 4 congested_wifi 25 1 strafe_fire
# the change, then: just native-install
NETBENCH_OUT=/tmp/nb-after  just netbench 4 congested_wifi 25 1 strafe_fire
tools/netbench/compare.py /tmp/nb-before /tmp/nb-after
```

The impairment scheduler is seeded, so the same arguments replay the same link and the two runs differ only by
the change. `compare.py` reports p50 and p95 per column and exits non-zero on a regression past its tolerance.

**Server egress is in the second table, not the first.** Every send-path column reads zero in a client CSV,
because a client is not the authority and runs none of it; the server's own per-second wire line is folded
into `server.csv`.

## Rust

```sh
just native-check    # fmt + clippy -D warnings + tests + build + the load smoke
```

One rule: **`orbitnet-core` never sees a `Variant`.** Zero dependencies, no `godot`, which is why its tests
run in milliseconds. A `godot` type in a core signature means logic has leaked across the boundary.

**Do not commit binaries.** `addons/orbitnet_native/bin/` is gitignored and no workflow ever adds to it.
`binaries.yml` proves every platform builds on every `native/**` push; `release.yml` publishes the bytes as
Release assets and commits only their sha256 manifest. Build your own with `just native-install`.

## Recording decisions

**No ADRs.** A decision goes in the README, a `docs/` page, or the header comment of the file it governs —
next to the code it constrains, where the next person will be standing when they need it.

The corollary: header comments here are longer than you may expect, on purpose. They carry the *why*,
especially for choices that look wrong until you know the reason. Change a decision, change its comment in the
same commit.

## Licensing

Contributions are dual-licensed **MIT OR Apache-2.0**. Inbound equals outbound; **no CLA** — opening a pull
request is the agreement.

Add a dependency, add it to `THIRD_PARTY.md` with its license in the same commit. Anything not MIT /
Apache-2.0 / BSD / MPL-2.0 needs a conversation first.

## Pull requests

- One change per PR, please.
- Say what the change is *for* in the description. The diff already says what it does.
- If you changed a decision, update the comment or doc page that recorded it.
- CI runs on GitHub-hosted runners with `pull_request`, so a fork PR gets no secrets and needs no approval to
  run.
