# Building

The Rust toolchain, the build recipes, and the binary distribution policy.

**You do not need any of this to use OrbitNet.** The binaries are committed as plain files, so a fresh clone
— or an Asset Library install — already has a working extension. This page is for changing the backend.

## Toolchain

- **Rust**, pinned exactly in `native/rust-toolchain.toml`. `rustup` honours the pin automatically. The Nix
  dev shell installs `rustup` rather than nixpkgs' `rustc`/`cargo` deliberately: the nixpkgs version floats
  with the unstable channel and would silently drift off the pin.
- **A C toolchain.** `rustc` shells out to `cc` to link the cdylib. On a bare container this is the step that
  fails first and least helpfully.
- **Godot 4.4 or newer** for the smoke and the demos. Not needed for `cargo test` — `orbitnet-core` has no
  dependency on Godot at all, which is the point of the crate split.

```sh
nix develop          # or: install rustup + a C toolchain + Godot yourself
just native-check    # fmt + clippy + tests + build + the load smoke
```

## The API baseline is 4.4, deliberately

`native/crates/orbitnet-godot/Cargo.toml` builds against the `api-4-4` feature, not against the newest
Godot. A gdext extension loads in any Godot **at or above** the API it was built against, so a 4.4 baseline
runs on 4.5, 4.6, 4.7 — and stays usable by projects that have not upgraded. `compatibility_minimum` in
`addons/orbitnet_native/orbitnet.gdextension` must agree with it; raising one without the other produces
either a needlessly narrow addon or an extension Godot refuses to load.

It also avoids gdext's `api-custom` feature, which would drag libclang and bindgen into every dev
environment for no benefit here.

## Recipes

| | |
|---|---|
| `just native-test` | `cargo fmt --check`, `clippy -D warnings`, `cargo test --workspace`. No Godot. |
| `just native-build` | The release cdylib. |
| `just native-smoke` | Loads the extension in a **throwaway** project and asserts the Rust classes register, exported properties round-trip, signals reach GDScript, ticks advance, and freeing a registered entity does not panic the frame. With no local build it falls back to the *committed* binary — which is what an AssetLib user actually receives, and therefore the more meaningful thing to test. |
| `just native-install` | Build, install this host's binary into `addons/orbitnet_native/bin/`, and re-sync it into every project. |
| `just native-check` | All of the above, in the order CI runs them. |

**Always the release profile.** Godot selects the `template_debug` entry whenever a project runs *from
source* — every dev run, every probe, every CI job. A cargo *debug* build there would be 10–50× slower and
would poison every performance number taken from a dev run, so both descriptor entries point at one release
artifact.

Two profile settings are load-bearing:

- **`strip = "debuginfo"`.** An unstripped gdext cdylib is 30–80 MB; stripped it is 2–5 MB. That ratio is
  what makes committing the binaries to plain git viable at all.
- **`panic = "abort"` is deliberately NOT set.** gdext converts a panic at the `#[func]` boundary into a
  Godot error. Aborting would turn a recoverable bug into a hard process kill that takes the editor with it.

## The binary distribution policy

**The committed binaries are plain git blobs. Git LFS is not an option here**, for two independent and
individually fatal reasons:

1. **The Asset Library installs from a repository tarball.** LFS content arrives in a tarball as *pointer
   files* — a few hundred bytes of text. `dlopen` then fails with "invalid ELF header" behind a confusing
   parse cascade, and the user has no way to distinguish that from a broken build. An addon distributed
   through AssetLib cannot use LFS for the thing it ships.
2. **GitHub's free LFS allowance is account-wide**, not per-repository: 1 GiB of storage and 1 GiB per month
   of bandwidth. A public addon's download volume is unbounded by construction, and when the quota is
   exhausted the smudge filter silently leaves pointer files — producing failure mode 1 for everyone.

The cost is history growth, and it is managed by *when* binaries are committed:

| Workflow | Trigger | What it does with binaries |
|---|---|---|
| `check.yml` | every PR and push | Verifies the committed Linux binary is a real ELF object, not a pointer. Builds nothing. |
| `binaries.yml` | push to main touching `native/**` | Builds all three platforms, uploads them as **artifacts**. Commits nothing. |
| `release.yml` | a `v*` tag | Builds all three, **commits** them, builds the AssetLib zip, publishes a Release. |

So main is always *proven* to build on every platform, while history only grows when a version is actually
cut. `binaries.yml` existing is what makes committing-only-on-tags safe.

**One file per platform**, with both `.gdextension` entries pointing at it. The two entries were already
byte-identical — both are cargo release builds — so two names for one artifact only doubled the git weight.

**macOS is built universal.** `binaries.yml` and `release.yml` build both `x86_64-apple-darwin` and
`aarch64-apple-darwin` and `lipo` them together. A single-arch dylib works on the machine that built it and
fails on the other half of the Mac install base — a bug that only ever arrives as an unreproducible report.
`just native-install` on a Mac produces a **host-arch** binary under the universal filename, which is correct
for local development and must not be committed.

## CI runs on GitHub-hosted runners

A security property, not a preference. **A public repository must never point fork-PR CI at a self-hosted
runner**: a fork PR can modify the workflow file, and the runner would execute it on your machine with your
filesystem and credentials. Every workflow uses `pull_request` rather than `pull_request_target`, so a fork PR
gets no secrets and no write token. The cost is installing Godot and Rust per job, which the caches make
cheap.

`native/` carries an empty `.gdignore`. Inert here — the root is not a Godot project — but it means that if
anyone does open the root as one, a 10k-LOC cargo workspace is not scanned as game content, and it keeps a
mirror into a project that *does* nest `native/` inside the addon idempotent.

## Adding a platform

1. Add a `[libraries]` entry to `addons/orbitnet_native/orbitnet.gdextension` (both `debug` and `release`
   pointing at one file, per the policy above).
2. Add a matrix entry to `.github/workflows/binaries.yml` and `.github/workflows/release.yml`.
3. Add a `cp` arm to `just native-install` if a developer can build it locally.

The Rust itself is architecture-agnostic; `linux.arm64` is absent only because nothing builds it yet. There
are no web entries because Godot's web export cannot load a GDExtension at all.
