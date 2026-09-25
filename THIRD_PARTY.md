# Third-party code

OrbitNet is **MIT OR Apache-2.0** (see `LICENSE`). This file inventories everything else that ends up in a
build and answers the question people actually have about the MPL-2.0 dependency.

## The short version

Install the addon, ship a game — **you inherit no copyleft obligation.** MPL-2.0 is a *file-level* license,
its obligation attaches to changes to *its own files*, and no MPL file is modified by this project or by your
game. Keeping this notice with your build is what the notice requirements amount to in practice.

If you *fork gdext itself* and modify its files, MPL-2.0 requires you to publish those modified files. That is
a real obligation, and it is on you rather than on OrbitNet.

## What ships in the compiled extension

The libraries published with each release, and built into `addons/orbitnet_native/bin/` by
`just native-install`, are Rust cdylibs statically linked from the crates below.
Regenerate this list with `cargo tree` in `native/` after any dependency change.

| Crate | License | Why it is here |
|---|---|---|
| `godot`, `godot-core`, `godot-ffi`, `godot-macros`, `godot-codegen`, `godot-bindings`, `godot-cell` | **MPL-2.0** | godot-rust (gdext) — the GDExtension binding. This is the only copyleft dependency; see below. |
| `gdextension-api` | MIT | The GDExtension C API headers/JSON gdext generates from. |
| `glam` | MIT OR Apache-2.0 | Vector/matrix math behind gdext's builtin types. |
| `libc` | MIT OR Apache-2.0 | POSIX signal handling for the native crash handler (`crash.rs`, Unix only). Windows uses raw `extern "system"` declarations instead of a second dependency. |
| `nanoserde`, `nanoserde-derive` | MIT | Build-time JSON parsing inside gdext's codegen. Not linked into the shipped library. |
| `heck`, `proc-macro2`, `quote`, `unicode-ident`, `venial` | MIT OR Apache-2.0 (`venial`: MIT) | Proc-macro machinery used at build time by gdext's macros. Not linked into the shipped library. |

`orbitnet-core` and `orbitnet-godot` are this project's own crates and carry this project's license.
`orbitnet-core` has **zero runtime dependencies** by design; the table below is its dev-only tree.

## `orbitnet-core`'s dev-dependencies

**Dev-only, and none of it is linked into a build.** `[dev-dependencies]` is compiled for
`cargo test` and for nothing else, so no crate below appears in `cargo build`, in the cdylib
`orbitnet-godot` links, or in any release asset. The zero-dependency rule in
`native/crates/orbitnet-core/Cargo.toml` governs `[dependencies]`, which stays empty.

Regenerate this list with `cargo tree -p orbitnet-core --edges normal,build,dev` in `native/`.

| Crate | License | Why it is here |
|---|---|---|
| `proptest` | MIT OR Apache-2.0 | The generator behind `crates/orbitnet-core/tests/wire_properties.rs`: round-trip properties over the wire codec, and the arbitrary-bytes sweep of every public decoder. |
| `bitflags`, `num-traits`, `regex-syntax`, `unarray` | MIT OR Apache-2.0 | `proptest`'s own dependencies. |
| `rand`, `rand_core`, `rand_chacha`, `rand_xorshift`, `getrandom`, `ppv-lite86` | MIT OR Apache-2.0 | The seeded RNG `proptest` generates and shrinks cases with. |
| `zerocopy` | BSD-2-Clause OR Apache-2.0 OR MIT | Byte-level casts inside `ppv-lite86`. |
| `cfg-if`, `libc` | MIT OR Apache-2.0 | Platform selection under `getrandom`. `libc` is already in the tree for the crash handler. |
| `autocfg` | Apache-2.0 OR MIT | `num-traits`' build script. |

The command above prints **15 crates**, which is every row of the table.

**Five more crates are locked and never built.** `native/Cargo.lock` resolves a dependency's optional
features and every target's backends, so it pins crates no build of this workspace compiles. They are listed
for completeness, and each one's license is compatible anyway:

| Crate | License | Why it never builds |
|---|---|---|
| `r-efi` | MIT OR Apache-2.0 OR LGPL-2.1-or-later | `getrandom`'s UEFI backend. No platform OrbitNet builds for selects it. |
| `wasip2`, `wit-bindgen` | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | `getrandom`'s WASI backend, selected on no platform OrbitNet builds for. |
| `zerocopy-derive` | BSD-2-Clause OR Apache-2.0 OR MIT | `zerocopy`'s optional `derive` feature, which `ppv-lite86` does not enable. |
| `syn` | MIT OR Apache-2.0 | `zerocopy-derive`'s dependency, and reached from nowhere else. |

The choice of `proptest` over `cargo-fuzz` is recorded in the header comment of
`native/crates/orbitnet-core/tests/wire_properties.rs`.

## Why MPL-2.0 in the dependency tree is fine

MPL-2.0 is **weak, file-scoped copyleft**. Its central obligation (§3.1): if you distribute *Covered Software*
in Source Code Form, you must make the source of **those files** available under the MPL. §3.2 explicitly
permits distributing the software in **Executable Form** under a license of your choosing, provided the
Covered Software's source stays available under the MPL.

Three consequences:

1. **Linking is explicitly contemplated.** §1.7's definition of "Larger Work" and §3.3 exist precisely to
   permit combining MPL code with code under other licenses. This is the difference between MPL and the GPL
   family, and it is why MPL is the license Mozilla chose for exactly this kind of reuse.
2. **The obligation is per-file, and no MPL file here is modified.** OrbitNet consumes gdext as an unmodified
   upstream crate from crates.io, pinned to an exact version. Nothing in `native/` edits a gdext source file,
   so there are no modifications to publish.
3. **Your game is not Covered Software.** Your game links a binary that contains gdext; it does not
   incorporate gdext source files. §3.2 lets you ship that executable under whatever terms you like.

What you should do: keep a copy of this file (or an equivalent notice) with your distributed build, and point
at gdext's source. That is `https://github.com/godot-rust/gdext`, and the exact version is pinned in
`native/crates/orbitnet-godot/Cargo.toml` and locked in `native/Cargo.lock`.

**This is not legal advice.** It is an explanation of why the maintainers consider the combination
unproblematic. If your organization has a policy that treats any MPL dependency as disqualifying, that policy
governs, not this file.

## Godot itself

Godot Engine is **MIT** (© 2014-present Juan Linietsky, Ariel Manzur and Godot Engine contributors). OrbitNet
does not vendor or redistribute Godot; it builds against the GDExtension API. Your game's own Godot
attribution obligations are unchanged by using this addon — see
[Godot's complying-with-licenses guide](https://docs.godotengine.org/en/stable/about/complying_with_licenses.html).

## Steam

`addons/orbitnet/steam_transport.gd` is the Steam arm of the transport factory. It contains **no Steamworks
code and no Steamworks headers** — every access is dynamic (`Engine.has_singleton`, `callv`, `ClassDB`), so
this repository redistributes nothing of Valve's and a non-Steam build carries zero Steam dependency. Using
that path requires you to install [GodotSteam](https://godotsteam.com/) yourself and to accept the Steamworks
SDK license directly with Valve. See `docs/steam.md`.
