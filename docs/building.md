# Building

The Rust toolchain, the build recipes, and the binary distribution policy.

**You do not need any of this to consume OrbitNet.** An Asset Library install or a release zip carries the
binaries already built. A `git clone` does not — `just native-install` builds them. This page is for
changing the backend.

## Toolchain

- **Rust**, pinned exactly in `native/rust-toolchain.toml`. `rustup` honors the pin automatically. The Nix
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
| `binaries.yml` | push to main touching `native/**` | Builds both descriptor profiles on every platform leg, uploads them as **artifacts**. The Windows and macOS legs then run the load smoke and the four unit suites against what they just built. |
| `release.yml` | a `v*` tag | Builds all three profiles on every platform leg, publishes the binaries and the AssetLib zip as Release assets, stamps the version, and commits the manifest. |

So main is always *proven* to build on every platform, and history carries digests rather than bytes.

**Two builds per platform, and they are not the same bytes.** `template_debug` and `template_release` differ
by their checks, which is the whole reason the descriptor names both.

### The zip is gated before it is published

Every gate above loads a library it built itself, out of the working tree. The zip a consumer installs is
assembled by a path none of them touch — `tools/make-assetlib-zip.py`, from a tree whose `bin/` was filled
by downloaded build artifacts, with the licence files copied in. **`tools/check-assetlib-zip.py` is the gate
for that artifact.**

| Assertion | What it catches |
|---|---|
| Both addon directories present, nothing outside them | a packaging walk that picked up the repository root or a synced mirror copy |
| Every file under `bin/` is one the zip's own descriptor names | a renamed profile, a library nothing loads |
| Each library non-empty and not Git LFS pointer text | the failure class above, reaching a user as a few hundred bytes of text |
| The licence files `release.yml` copies in are present | an addon installed from a zip carries no repository with it |
| `binaries.json` stays out | it hashes the archive, so the copy in the tree carries the previous tag's digests |
| `plugin.cfg` and the EditorPlugin script it names are present and non-empty | a plugin that installs and then fails the moment the user enables it |
| The archive unpacks into a throwaway project that boots | the addon's GDScript compiles, `Net` instantiates, and the zip's own binaries register their classes |

Where it runs:

- **`just assetlib-check`** — assemble from this tree, assert, boot. In `just check`.
- **`check.yml`** — `--self-test` in the gates job (the negative control: one synthetic archive per failure
  class, each asserted to be caught), and the full run in the Godot job. A pull request holds one platform's
  libraries, so it cannot produce the real artifact; what it can break is the packaging script, the
  descriptor and the addon layout.
- **`release.yml`** — `--complete` on the real zip, between building it and publishing it. The only tree
  that holds every platform's libraries, so the only place the descriptor's full named set can be required.
  A failure there fails the release.

## Platform keys

**A platform key names one shipped filename per profile.** It is finer-grained than an operating system:
the two Linux architectures are two keys, and the three Android ABIs three more, because each is its own
file and its own descriptor entry, while macOS is one key because its two architectures end up in one
file. `tools/build-native.sh` takes the key, both workflows pass it as `matrix.platform`, and `PLATFORMS`
in `tools/check-descriptor-parity.sh` is the list all three are checked against.

| Key | Cargo target(s) | Shipped as | Built on |
|---|---|---|---|
| `linux` | host `x86_64-unknown-linux-gnu` | `liborbitnet.linux.<profile>.x86_64.so` | the x86_64 Linux runner |
| `linux_arm64` | `aarch64-unknown-linux-gnu` | `liborbitnet.linux.<profile>.arm64.so` | the same x86_64 Linux runner, cross-compiled |
| `windows` | `x86_64-pc-windows-msvc` | `orbitnet.windows.<profile>.x86_64.dll` | the Windows runner |
| `macos` | `x86_64-apple-darwin` + `aarch64-apple-darwin` | `liborbitnet.macos.<profile>.universal.dylib` | the arm64 macOS runner |
| `android_arm64` | `aarch64-linux-android` | `liborbitnet.android.<profile>.arm64.so` | the same x86_64 Linux runner, cross-compiled |
| `android_arm32` | `armv7-linux-androideabi` | `liborbitnet.android.<profile>.arm32.so` | the same x86_64 Linux runner, cross-compiled |
| `android_x86_64` | `x86_64-linux-android` | `liborbitnet.android.<profile>.x86_64.so` | the same x86_64 Linux runner, cross-compiled |

**Use Godot's architecture names in a shipped name.** The `[libraries]` key in the `.gdextension` is
`<platform>.<debug|release>.<arch>`, and Godot spells the two arm ABIs `arm64` and `arm32` — never the
NDK's `arm64-v8a` and `armeabi-v7a`, and never a Rust target triple. A wrong name there fails at `dlopen`
on that ABI and nowhere else, which is the failure `just descriptor-parity` exists to catch.

**macOS is built universal, `profiling` included.** Every profile builds both `x86_64-apple-darwin` and
`aarch64-apple-darwin` and `lipo`s them together, including a local `just native-install`. A single-arch
dylib works on the machine that built it and fails on the other half of the Mac install base — a bug that
only ever arrives as an unreproducible report.

**The macOS `profiling` `.dSYM` is universal too.** `dsymutil` runs on the lipo'd dylib, not once per
architecture.

- **`lipo` first, `dsymutil` second.** `dsymutil` runs on the fat dylib, walks each slice's debug map in
  turn, and writes one bundle carrying a UUID per slice. A UUID is what a debugger matches an image
  against, so that one bundle symbolizes both halves.
- **Two per-architecture bundles cannot be combined after the fact.** That is why the previous build
  shipped arm64 only.
- **Each slice is built with `-C split-debuginfo=unpacked`**, which leaves the object files `dsymutil`
  reads in place. `packed` runs `dsymutil` per architecture and deletes them.
- **`tools/build-native.sh` fails the build unless the bundle's UUID set equals the dylib's.** A bundle
  covering one slice symbolizes on the machine that built it and hands the other half of the install base
  raw addresses.

**Linux arm64 is cross-compiled on the x86_64 Linux runner.** The leg carries the same runner labels as
`linux` and targets `aarch64-unknown-linux-gnu`. No arm64 hardware is involved, the same way the macOS leg
cross-builds an x86_64 slice on an arm64 box.

It needs an aarch64 **linker**, which `rustup target add` does not install — `rustup` supplies the Rust std
libraries, and `rustc` still shells out to a C linker for the cdylib.

```sh
sudo apt-get install -y gcc-aarch64-linux-gnu    # Debian/Ubuntu
sudo dnf install -y gcc-aarch64-linux-gnu        # Fedora/RHEL
sudo pacman -S aarch64-linux-gnu-gcc             # Arch
```

`tools/build-native.sh` looks for `aarch64-linux-gnu-gcc`, then `aarch64-unknown-linux-gnu-gcc`, then
`aarch64-linux-gnu-cc`, and sets `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER` to the first it finds.
Export that variable yourself to point at some other toolchain. **With no linker the build refuses to
start** and prints the install lines above. A leg that skipped or produced nothing would upload an empty
artifact and fail the tag after every other platform had already built.

**The CI legs install it themselves.** `binaries.yml` and `release.yml` each carry an
`Install the aarch64 cross linker` step ahead of the build, guarded to the `linux_arm64` leg. It is a no-op
when a linker is already present or `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER` is set, and otherwise
installs the package above through whichever of `apt-get`, `dnf` or `pacman` the runner has. It needs
non-interactive `sudo` for the account the runner service runs as; without that, or on a box using another
package manager, the step fails naming what to install by hand. The install lives in CI rather than in
`tools/build-native.sh` because a build script that installs packages is one that can change a contributor's
machine.

On an arm64 Linux host none of this applies. `tools/build-native.sh host` reports `linux_arm64` there, and
`just native-install` builds and stages the library that host's Godot will load.

### Android is three keys, one per ABI

Android has no universal container. Each ABI ships its own `.so`, which needs its own descriptor entry,
which needs its own platform key. All three cross-compile on the x86_64 Linux runner.

| ABI | Key | Why it ships |
|---|---|---|
| `arm64` | `android_arm64` | every phone and tablet shipping today |
| `arm32` | `android_arm32` | still an export-preset checkbox; a project that ticks it otherwise exports an app whose extension is absent on those devices |
| `x86_64` | `android_x86_64` | the Android emulator and ChromeOS, which is where the extension is loaded while a game is developed on a desktop |

**There is no `x86_32` key.** No Android device ships that ABI and the emulator system images for it are
gone. A project that needs one adds it the way any platform is added — see
[Adding a platform](#adding-a-platform).

**Each android leg needs the NDK on its box.** `rustup target add` supplies the Rust std libraries;
rustc still shells out to a C linker for the cdylib, and for an android ABI that linker is the NDK's clang
wrapper, carrying Bionic's libc and crt objects. Nothing else from the NDK is needed — the workspace has
no `cc` or `bindgen` in its dependency graph, so no C is compiled.

```sh
sdkmanager --install "ndk;26.3.11579264"   # the command-line tools
# or Android Studio: SDK Manager -> SDK Tools -> NDK (Side by side)
sudo apt-get install -y google-android-ndk-installer   # Debian/Ubuntu
sudo pacman -S android-ndk                             # Arch
```

`tools/build-native.sh ndk-root` prints the NDK the build will use, and is the only search in the
repository — both workflows call it rather than carrying a copy. It reads, and the first hit wins:

1. `ANDROID_NDK_HOME`, `ANDROID_NDK_ROOT`, `ANDROID_NDK`, `NDK_HOME`, `ANDROID_NDK_LATEST_HOME`.
2. `ndk/<version>` under `ANDROID_SDK_ROOT`, `ANDROID_HOME`, `$HOME/Android/Sdk`,
   `$HOME/Library/Android/sdk`, `/usr/lib/android-sdk`, `/opt/android-sdk` — in that order, and the
   newest version within whichever root matches first.
3. `/usr/lib/android-ndk` and `/opt/android-ndk*`, the distribution packages that are an NDK root
   themselves.

**Root order decides which NDK wins**, and version comparison happens only within one root. A box
carrying the pinned NDK under `ANDROID_SDK_ROOT` and a stale one under `$HOME` uses the pinned one.
Point `ANDROID_NDK_HOME` at a particular install to override the search, or set the target's own
`CARGO_TARGET_<TRIPLE>_LINKER` to bypass it entirely. **With no NDK the build refuses to start** and
prints the lines above, for the same reason the aarch64 leg does: a leg that produced nothing would
upload an empty artifact and fail the tag after every other platform had already built.

**The CI legs resolve it themselves.** `binaries.yml` and `release.yml` each carry a
`Locate the Android NDK` step ahead of the build, guarded to the android legs. It calls
`tools/build-native.sh ndk-root`, so CI and the build agree on what counts as an NDK and on which one
wins; it installs the pinned version through `sdkmanager` when the box has one and the search came up
empty, and otherwise fails naming what to install. `ANDROID_NDK_VERSION` in that step is the only place
the installed version is spelled. An NDK is a licensed SDK component rather than a distribution package,
so a runner carrying no command-line tools cannot be provisioned from CI and has to be set up by hand.

**API 21 is the floor.** `tools/build-native.sh` builds against API 21 by default and falls back to the
lowest level the installed NDK ships. A library built against 21 loads on every device at or above it, so
the lowest available level is the one that excludes the fewest devices; Godot's export template sets the
minimum SDK the app itself declares. `ORBITNET_ANDROID_API` overrides the default.

**The 64-bit ABIs are linked with 16 KB page alignment.** Android devices with 16 KB memory pages reject a
library whose segments are aligned to 4 KB, and Play requires that alignment of apps targeting recent API
levels. NDK r27 and newer link that way already and the flag is then a no-op; an older NDK does not, and
the failure is a `dlopen` on a 16 KB device and nowhere else. `arm32` is unaffected.

**The android libraries are published untested.** Every push to `main` touching `native/**` compiles all
three and every tag publishes them, so a change that breaks an android build fails a gate. Loading one
needs a device or an emulator and no runner in this fleet has either, so nothing here proves the classes
register or a tick advances on Android. Two things remain unmeasured, and both are runtime questions
rather than build ones: the rollback loop's per-tick budget on a thermally throttled phone, and what the
reconciliation path costs on a mobile radio.

**The extension installs no native crash handler on Android.** Bionic has no `<execinfo.h>` for the POSIX
branch to link against, and `debuggerd` already writes a symbolized tombstone for every fatal signal in
release builds. `Net.install_native_crash_handler()` still returns `true` there and writes no
`crash-native.log` — see [crash-capture.md](crash-capture.md).

### iOS is not a key yet

`tools/build-native.sh` stages **one file per platform per profile**, the descriptor names that file and
the release publishes it as its own asset. iOS does not fit that shape, and the mismatch is structural
rather than a missing toolchain:

- **The artifact is a directory.** An iOS GDExtension is packaged as an `.xcframework` holding a device
  slice and a simulator slice. They cannot be merged into one file with `lipo`, because both are `arm64`
  — an `.xcframework` exists precisely for the case `lipo` cannot cover.
- **It needs Xcode on a macOS box**, not just the command line tools, for `xcodebuild -create-xcframework`.
  The macOS runner has the command line tools, which is what `dsymutil` and `lipo` need and all the
  current legs ask of it.

**The static library itself is not an obstacle.** An iOS GDExtension links statically, and `cargo rustc
--crate-type staticlib` sets that for one invocation without touching
`native/crates/orbitnet-godot/Cargo.toml` or any other platform's build — `tools/build-native.sh` already
uses `cargo rustc` for per-platform overrides.

Closing it means teaching the build path to stage a bundle and the release path to publish one, which is
a larger change than another key. ENet over UDP needs no protocol change either way — this is a
build-matrix question rather than a netcode one.

### Windows and macOS test where they build

`check.yml` runs all of its jobs on `ubuntu-latest` and cannot speak for the other two platforms. A bad
`[libraries]` entry, a wrong-architecture build or a missing entry symbol fails at `dlopen` on the affected
platform and nowhere else, so a Linux-only gate stays green while a Windows checkout takes the `Net` autoload
down. The Windows and macOS legs of `binaries.yml` already hold a library their own runner can load, so they
test it:

| Step | What it proves |
|---|---|
| `tools/orbitnet-smoke.sh --skip-build` | A throwaway Godot project asserts the classes register, exported properties bind, signals reach GDScript, ticks advance, and freeing a registered entity does not panic the frame. Before that it checks the staged file's magic bytes against this platform's object format, and its exported symbols for `gdext_rust_init`. |
| The four unit suites | The addon's GDScript, on this platform. No scene tree, no physics, no sockets. |

- **The Godot assertions are the part that always gates.** The binary check fails only on positive evidence
  that a file is wrong. The **symbol check** runs where the platform carries a reader the script can drive
  (`nm`, or `dumpbin` on Windows); where none does, the step says so and continues into the Godot run rather
  than failing a build that is fine.
- **Both steps need Godot on the runner.** The leg checks for it first and fails naming what to install,
  rather than reporting `godot: command not found` from inside a test script. Set **`GODOT_BIN`** to the
  binary's path on a runner where Godot is not on `PATH` under the name `godot`.
- **The probes stay Linux-only.** They are multi-process, they bind UDP ports and they are slow.
- **Neither Linux leg runs either step, for different reasons.** `linux` skips them because `check.yml`
  already runs both on Linux for every pull request. **`linux_arm64` cannot run them at all**: it is
  cross-built on the x86_64 box, so the runner that produced the artifact cannot load it. The steps name the
  two legs that do run rather than excluding the ones that do not, so a future cross-built leg has to opt in.
- **The three android legs cannot run them either**, for a stronger version of the same reason: their
  artifacts target another operating system as well as another ABI, and loading one needs a device or an
  emulator. The allowlist is what kept them out with no change to these steps.

## The version

**One version, written to three files.** A release tag `vX.Y.Z` is stamped into all of them by
`tools/version-parity.sh`, which is the only thing that writes any of them.

| File | Field | Who reads it |
|---|---|---|
| `addons/orbitnet/plugin.cfg` | `version=` | Godot's plugin list, and the AssetLib entry |
| `native/Cargo.toml` | `[workspace.package] version` | `CARGO_PKG_VERSION`, a crash report, a cargo consumer of `orbitnet-core` |
| `native/Cargo.lock` | one entry per workspace member | `cargo build --locked`, and any vendoring consumer |

Both crates inherit the workspace version with `version.workspace = true`, so the member manifests carry no
version of their own.

**Where the check runs.** `just version-parity` fails a tree whose three files disagree.

| Where | When |
|---|---|
| `just check` | locally, before a pull request |
| `release.yml` | at tag time, immediately after stamping — the only place a stamp can be proved, because a rewrite whose pattern stops matching exits 0 and would otherwise publish the old version |

It is **not a `check.yml` step yet**, so a pull request that moves one file without the others passes CI
and the divergence lands on main until the next tag re-stamps it.

**The stamp returns to `main`.** The manifest pull request carries `plugin.cfg`, `native/Cargo.toml` and
`native/Cargo.lock` alongside `binaries.json`. Stamped on the tag alone, the crate version would revert on
the next commit and the divergence would reopen at once.

To bump locally before tagging: `just version-stamp 0.5.0`.

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

1. Add a platform case to `tools/build-native.sh` — the cargo output filename, the two halves of the
   shipped name around the profile, and `CROSS_TARGET` if it is not a host build.
2. Add `[libraries]` entries to `addons/orbitnet_native/orbitnet.gdextension`, one per descriptor profile.
3. Add a matrix leg to `.github/workflows/binaries.yml` and `.github/workflows/release.yml`, and add the
   platform to `PLATFORMS` in `tools/check-descriptor-parity.sh`.
4. Add the platform to the `for p in …` list in `release.yml`'s publish job, which collects the uploaded
   artifacts into `bin/`.

`just descriptor-parity` fails the PR if any of the four disagree, so a half-added platform cannot reach a
tag. Use the same spelling everywhere — the parity check matches a leg by `platform: <key>` and the publish
list by `for p in <keys>`, and accepts `[a-z0-9_]` only, so an underscore is the separator that works in
every place the key is spelled.

**A key that needs a toolchain the runner lacks needs a fifth edit**: a preflight in
`tools/build-native.sh` that refuses to start and names the install, and a step in both workflows that
provisions it. Without the preflight a leg with no linker can exit 0 having produced nothing, and the tag
then fails in the publish job after every other platform has already built.

The Rust itself is architecture-agnostic. There are no web entries because Godot's web export cannot load a
GDExtension at all, and no ios entries for the reasons above.
