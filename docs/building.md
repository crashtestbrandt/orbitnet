# Building

The Rust toolchain, the build recipes, and the binary distribution policy.

**You do not need any of this to consume OrbitNet.** An Asset Library install or a release zip carries the
binaries already built. A `git clone` does not — `just native-install` builds them. This page is for
changing the backend.

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
| `just native-build` | Both descriptor profiles for this host, into `addons/orbitnet_native/bin/`. |
| `just native-smoke` | Loads the extension in a **throwaway** project and asserts the Rust classes register, exported properties round-trip, signals reach GDScript, ticks advance, and freeing a registered entity does not panic the frame. It checks every name the descriptor resolves, so a profile that failed to stage fails here rather than one export later. |
| `just native-install` | `native-build` plus a re-sync into every project. A fresh clone has no binary until this runs. |
| `just native-check` | All of the above, in the order CI runs them. |

## Profiles

**Never the cargo debug profile.** Godot selects the `template_debug` entry whenever a project runs *from
source* — every dev run, every probe, every CI job — and a cargo *debug* build there would be 10–50× slower
and would poison every performance number taken from a dev run. All three profiles below inherit
`[profile.release]`.

| Profile | Cargo | Who loads it | Measured (Linux) |
|---|---|---|---|
| `template_debug` | `--profile template-debug` | a project run from source | 4.41 MB |
| `template_release` | `--release` | an exported game | 3.99 MB |
| `profiling` | `--profile profiling` | a developer, by hand | 15.0 MB |

**`template_debug` adds `debug-assertions` and `overflow-checks`, and nothing else.** That is what makes
`debug_assert!` reachable at all. With the codec's declared-size check deliberately falsified, `--release`
passes 189/189 silently and this profile fails 2 of them; that check is the encoder agreeing with itself
about how many bytes it wrote, and disagreement there corrupts a delta chain.

**`profiling` retains debug information** (`debug = 1`, `strip = "none"`) so a native profiler can attribute
frames to Rust functions and source lines. It is published as a release asset and is deliberately not a
descriptor entry — shipping it would put 11 MB of debug information nobody loads into every export.

Two settings on `[profile.release]` are load-bearing:

- **`panic = "abort"` is deliberately NOT set**, and no profile overrides that. gdext converts a panic at
  the `#[func]` boundary into a Godot error. Aborting would turn a recoverable bug into a hard process
  kill that takes the editor with it.
- **`strip = "debuginfo"`** is inherited by both descriptor profiles. `profiling` deliberately overrides
  it to `"none"`, which is the whole reason that build is 15.0 MB against 4 MB. An unstripped gdext
  cdylib is 30–80 MB; stripped it is 2–5 MB.

**`tools/build-native.sh` is the only place that maps a platform and a profile onto a filename.** Both build
workflows, the load smoke, the PR gate and `just native-install` ask it rather than spelling names out, and
`tools/check-descriptor-parity.sh` fails the PR when the descriptor and that script disagree.

## The binary distribution policy

**No binary is committed to this repository.** `addons/orbitnet_native/bin/` is gitignored and empty in a
fresh clone. Three ways to fill it:

| Path | Who uses it |
|---|---|
| `just native-install` | a contributor, and every CI job before it loads anything |
| the release zip `orbitnet-<version>.zip` | an Asset Library or manual install |
| the loose release assets, pinned to a tag | a consuming project that fetches at setup |

**What the repository commits instead is a digest.** `release.yml` writes
`addons/orbitnet_native/binaries.json` — the size and sha256 of every asset it published — and commits that
to `main`. A consumer verifies a download against something in the commit graph, which a checksum file
published beside the asset cannot do: that verifies transport, not tamper.

**Git LFS is not an option for the shipped artifact either way**, for two independent and individually fatal
reasons:

1. **The Asset Library installs from a repository tarball.** LFS content arrives in a tarball as *pointer
   files* — a few hundred bytes of text. `dlopen` then fails with "invalid ELF header" behind a confusing
   parse cascade, and the user has no way to distinguish that from a broken build.
2. **GitHub's free LFS allowance is account-wide**, not per-repository: 1 GiB of storage and 1 GiB per month
   of bandwidth. A public addon's download volume is unbounded by construction, and when the quota is
   exhausted the smudge filter silently leaves pointer files — producing failure mode 1 for everyone.

| Workflow | Trigger | What it does |
|---|---|---|
| `check.yml` | every PR and push | Builds both descriptor profiles for Linux, confirms each is a real ELF object, and runs every gate against them. |
| `binaries.yml` | push to main touching `native/**` | Builds both descriptor profiles on all three platforms, uploads them as **artifacts**. |
| `release.yml` | a `v*` tag | Builds all three profiles on all three platforms, publishes the binaries and the AssetLib zip as Release assets, and commits the manifest. |

So main is always *proven* to build on every platform, and history carries digests rather than bytes.

**Two builds per platform, and they are not the same bytes.** `template_debug` and `template_release` differ
by their checks, which is the whole reason the descriptor names both.

**macOS is built universal.** Every profile builds both `x86_64-apple-darwin` and `aarch64-apple-darwin` and
`lipo`s them together, including a local `just native-install`. A single-arch dylib works on the machine that
built it and fails on the other half of the Mac install base — a bug that only ever arrives as an
unreproducible report.

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

1. Add a platform case to `tools/build-native.sh` — the cargo output filename and the two halves of the
   shipped name around the profile.
2. Add `[libraries]` entries to `addons/orbitnet_native/orbitnet.gdextension`, one per descriptor profile.
3. Add a matrix leg to `.github/workflows/binaries.yml` and `.github/workflows/release.yml`, and add the
   platform to `PLATFORMS` in `tools/check-descriptor-parity.sh`.

That gate fails the PR if any of the three disagree, so a half-added platform cannot reach a tag.

The Rust itself is architecture-agnostic; `linux.arm64` is absent only because nothing builds it yet. There
are no web entries because Godot's web export cannot load a GDExtension at all.
