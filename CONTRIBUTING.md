# Contributing

Thanks for looking. This page is short because most of the rules are enforced rather than described.

```sh
just sync-addons     # required once after cloning: mirror the addon into the projects
just check           # everything a PR must pass
```

`just check` runs, fastest-failing first: `addon-drift` → `net-check` → the cargo gates → lint → the unit
suites → the two-peer RTS probe.

## Where to edit

**`addons/orbitnet/` and `addons/orbitnet_native/` at the repository root are canonical.** Every Godot
project here gets a mirror-copy from `tools/sync-addons.sh`, and those copies are gitignored build artifacts.
Editing a copy is the one mistake this layout invites, so `just addon-drift` fails loudly if you do.

If you are iterating on `net.gd` itself and the re-sync is tiresome, `ORBITNET_LINK=1 tools/sync-addons.sh`
symlinks instead. Unix only, never used by CI.

## The two boundaries

Both are grep-enforced by `tools/net-check.sh`, and both are the reason this addon is extractable at all:

- **`net.gd` is the only file that may name a backend class** (`OrbitNet`, `OrbitRollbackSynchronizer`,
  `OrbitStateSynchronizer`, `OrbitInterpolator`).
- **`steam_transport.gd` is the only file that may name Steamworks.** Every Steam access is dynamic
  (`Engine.has_singleton`, `callv`, `ClassDB`), so a non-Steam build carries zero Steam dependency.

The gate scans the **demos** too. A demo that needs to reach past the facade has found a hole *in the facade*
— widen the facade, do not exempt the caller. That is exactly how `NetInterpolatorHandle` came to exist: the
RTS demo could not reach `add_property()` without an untyped call, which this project treats as an error.

## GDScript rules

These are promoted from warnings to **errors** in each project's `project.godot`. They are why the codebase
reads consistently, and they catch a real class of netcode bug — an untyped value crossing the wire boundary
is how a `Variant` ends up somewhere a typed value was assumed.

- **Every `var`, parameter and return type is annotated.** `func f(x): return x + 1` is a bug; write
  `func f(x: int) -> int: return x + 1`.
- **`Array` and `Dictionary` are always typed**: `Array[Node]`, `Dictionary[String, int]`.
- **Do not `as`-cast a `Variant`, and do not pass one to a typed constructor.** *Assign* it to a typed local
  instead — that conversion is allowed, the cast is not. This comes up constantly when reading a
  wire-decoded payload, and it is the single most common review note.
- **`@warning_ignore` is banned** unless a comment explains the false positive.
- **No Python idioms**: `size()` not `len()`, `push_back()` not `append()`, `append_array()` not `+=` on
  arrays, `null` not `None`.
- **Prefer `%UniqueName` over `$Path/To/Node`.** Enums over magic values.

One deliberate exception: `gdscript/warnings/exclude_addons=true`, because the facade's opaque-`Node` backend
calls cannot satisfy `unsafe_method_access` by construction — that is what makes it a facade.

## Coverage: default to a unit test

Decide in this order.

**Is it a pure function?** — constructible from plain data, no `SceneTree`, no `PhysicsServer`, no
`MultiplayerPeer`, no `RenderingDevice` → **a `tests/unit/*_test.gd` suite.** The whole suite runs in about a
second. Steering math, the order validator, the wire packing, the seat roster, the transport factory, the
tape codec, the impairment scheduler — all pure, all unit tests.

Logic that lives on a `Node` is usually *still* unit-testable via `.new()` without calling `_ready()`, and
static methods on a Node subclass are callable with no instance at all. Do not reach for a scene probe just
because a class extends `Node`.

**Does it genuinely need a live scene, physics or network?** → a probe under `tools/instr/`. But a probe
gates PRs **only** if it guards a fundamental netcode regression: rollback determinism, prediction and
reconciliation, two-peer sync, dedicated-server boot, the facade, or the transport factory.

Today exactly one probe gates PRs: `tools/rts-probe.sh`. **Keep it that way.** If a probe's assertions are
pure math, port them to a unit test and delete the redundant block.

An empty run is a failure, not a pass — the runner exits non-zero if it finds no suites or no `test_*`
methods. That check has caught more real breakage than most assertions.

## Rust

```sh
just native-check    # fmt + clippy -D warnings + tests + build + the load smoke
```

The crate split has one rule: **`orbitnet-core` never sees a `Variant`.** It is engine-agnostic plain-data
Rust with zero dependencies, which is why its 160 tests run in milliseconds. If a `godot` type starts
appearing in core's signatures, logic has leaked across the boundary.

`orbitnet-godot` is the deliberately thin binding shell: class registration, `Variant` ↔ packed-row
marshalling, the entity registry, signals, and the packet pump.

**Do not commit binaries.** `binaries.yml` proves every platform builds on every `native/**` push;
`release.yml` commits them on a tag. Committing them by hand is how a repository without LFS gets fat.

## Recording decisions

**No ADRs.** A decision goes in the README, in a `docs/` page, or in the header comment of the file it
governs — next to the code it constrains, where the next person will actually be standing when they need it.

The corollary is that header comments here are longer than you may be used to, and that is on purpose: they
carry the *why*, especially for the choices that look wrong until you know the reason. If you change one of
those decisions, change the comment in the same commit.

## Licensing

Contributions are dual-licensed **MIT OR Apache-2.0**, matching the project. Inbound equals outbound; there
is **no CLA**. By opening a pull request you are agreeing to that — nothing else to sign.

If you add a dependency, add it to `THIRD_PARTY.md` with its licence in the same commit. A dependency whose
licence is not MIT / Apache-2.0 / BSD / MPL-2.0 needs a conversation first.

## Pull requests

- One change per PR, please.
- Say what the change is *for* in the description. The diff already says what it does.
- If you changed a decision, update the comment or doc page that recorded it.
- CI runs on GitHub-hosted runners with `pull_request`, so a fork PR gets no secrets and needs no approval to
  run.
