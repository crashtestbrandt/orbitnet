set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

# Every recipe invokes Godot through tools/godot-quiet.sh, which filters one cosmetic upstream diagnostic
# and passes everything else -- stdout, stderr, and the exit code -- through untouched. Set GODOT_BIN to
# pick a specific binary; `nix develop` puts a pinned one on PATH.
godot := justfile_directory() / "tools" / "godot-quiet.sh"

default:
    @just --list

# =====================================================================================================
# addon sync
#
# The repo root is NOT a Godot project. addons/orbitnet + addons/orbitnet_native are the canonical source
# and the AssetLib payload; every Godot project here gets a mirror-copy. Run `sync-addons` after touching
# the addon, and `addon-drift` is the gate that nobody edited a copy by mistake.
# =====================================================================================================

# Mirror the canonical addon (and the shared test harness) into every Godot project.
sync-addons:
    tools/sync-addons.sh

# Fail if any synced copy has diverged from the canonical source -- i.e. if someone edited a copy instead
# of the canonical addon. A LOCAL gate: it needs the copies to exist, so it is meaningless on a fresh
# checkout where they are gitignored and absent.
addon-drift:
    tools/sync-addons.sh --check

# Fail if a synced copy was ever COMMITTED. That would create a second source of truth which silently
# diverges from the canonical addon, and it is the failure this layout exists to prevent -- so it is the
# variant CI runs, where nothing can have drifted because nothing has been synced yet.
addon-tracked:
    tools/sync-addons.sh --check-tracked

# =====================================================================================================
# gates
# =====================================================================================================

# The facade boundary gate: fails if anything but addons/orbitnet/net.gd names a backend class. Pure grep,
# no Godot, runs in milliseconds. It scans the DEMOS too -- a demo that reaches past the facade has found a
# hole in the facade.
net-check:
    tools/net-check.sh

# Headless project load for each Godot project -- catches every GDScript compile and parse error, with the
# project's warnings-as-errors promotion applied.
lint: (lint-project "harness") (lint-project "demos/rts")

lint-project PROJECT:
    GODOT="{{godot}}" tools/lint-gdscript.sh {{PROJECT}}

# The unit suites. Sub-second, no scene tree, no physics, no sockets. This is where coverage belongs unless
# it genuinely needs a live session -- see CONTRIBUTING.md.
test: harness-test rts-test

harness-test:
    "{{godot}}" --headless --path harness --script tests/support/run_unit_tests.gd

rts-test:
    "{{godot}}" --headless --path demos/rts --script tests/support/run_unit_tests.gd

# Prove the extension loads and registers its classes in a THROWAWAY project -- no autoload, no plugin, no
# addon GDScript. Catches the pointer-file / wrong-architecture / stale-filename class of failure.
harness-smoke:
    # --quit-after is a frame-count backstop only: smoke.gd quits itself as soon as it has a verdict.
    "{{godot}}" --headless --path harness --quit-after 600

# The two-peer networked gate: identical worlds, orders replicated, a forged order refused. This is the PR
# gate, and the only scene-bound one.
rts-probe:
    GODOT="{{godot}}" tools/rts-probe.sh

# Everything a PR must pass, in the order that fails fastest first.
check: addon-tracked addon-drift net-check native-test lint test rts-probe

# =====================================================================================================
# the native backend (Rust)
#
# Sources in native/ -- a cargo workspace at the REPO ROOT, deliberately outside the shipped addon, so the
# AssetLib payload is exactly the two addons/ directories. The loaded binaries live in
# addons/orbitnet_native/bin/, which is GITIGNORED: no binary is committed to this repository. A fresh
# clone has none until `just native-install` builds them. See the .gdextension header.
# =====================================================================================================

# Pure-Rust gates: fmt, clippy as errors, and the orbitnet-core suites. No Godot involved.
native-test:
    cd native && cargo fmt --all --check
    cd native && cargo clippy --workspace --all-targets -- -D warnings
    cd native && cargo test --workspace

# Both descriptor profiles, into addons/orbitnet_native/bin/ under the shipped names. `template_debug` is
# what Godot loads when a project runs from source (every dev run, every CI probe) and is the only build
# carrying `debug-assertions`; `template_release` is what an exported game loads. Neither is the 10-50x
# slower cargo debug build -- both inherit `[profile.release]`. tools/build-native.sh owns the naming.
native-build:
    tools/build-native.sh build "$(tools/build-native.sh host)" addons/orbitnet_native/bin

# Load the freshly built extension in a throwaway project and assert the Rust classes register, exported
# properties round-trip, signals reach GDScript, ticks advance, and freeing a registered entity does not
# panic the frame.
native-smoke:
    GODOT="{{godot}}" tools/orbitnet-smoke.sh

# native-build plus a re-sync into every project. Nothing here is committed -- bin/ is gitignored, so this
# is how a fresh clone gets a backend at all.
native-install: native-build
    tools/sync-addons.sh
    echo "orbitnet: built both descriptor profiles and re-synced every project"

# native-test + native-build + native-smoke, in the order CI runs them.
native-check: native-test native-build native-smoke

# =====================================================================================================
# the RTS demo
# =====================================================================================================

# Single player. No peer, no socket -- the facade stays OFFLINE and every handle it hands out is inert, so
# this is the same code path a hosted session takes, minus the network.
rts:
    "{{godot}}" --path demos/rts

rts-editor:
    "{{godot}}" --editor --path demos/rts

# Listen server: authoritative AND a local player. Run this, then `just rts-join` in a second terminal.
rts-host PORT="47800":
    "{{godot}}" --path demos/rts -- --host={{PORT}}

rts-join ADDR="127.0.0.1":
    "{{godot}}" --path demos/rts -- --join={{ADDR}}

# Dedicated server: authoritative, headless, no local player.
rts-serve PORT="47800":
    "{{godot}}" --headless --path demos/rts -- --dedicated={{PORT}}

# A long single-peer soak with the bot driving, for watching the diagnostics under sustained load without
# standing up a second machine.
rts-stress SECONDS="120":
    "{{godot}}" --headless --path demos/rts -- --host --bench --bench-bot=strafe_fire \
        --bench-duration={{SECONDS}} --quit-after={{SECONDS}}

rts-lint: (lint-project "demos/rts")

# =====================================================================================================
# netbench -- the netcode test bench
#
# A below-ENet-reliability UDP impairment relay, a fleet of real headless bot clients driven through the
# real input path, and tick-domain gates. NOT a PR gate: it is a measurement tool, and its numbers depend
# on the machine. See docs/netbench.md.
# =====================================================================================================

netbench CLIENTS="4" PROFILE="congested_wifi" SECONDS="20" SEED="1" POLICY="strafe":
    tools/netbench/bench.sh {{CLIENTS}} {{PROFILE}} {{SECONDS}} {{SEED}} {{POLICY}}

# Multi-machine bench: one SSH controller drives a server host plus bot-client hosts. Needs real reachable
# hosts, passwordless SSH and Godot on each. GAUNTLET_DRYRUN=1 prints the plan without running it.
netbench-gauntlet:
    tools/netbench/gauntlet.sh

# =====================================================================================================
# exports
# =====================================================================================================

export-rts:
    mkdir -p build/rts-linux
    "{{godot}}" --headless --path demos/rts --export-release "Linux" ../../build/rts-linux/orbitnet-rts.x86_64

# The server image boots authoritative with NO argument (rts_main.gd checks the dedicated_server feature),
# which is what makes it deployable: an operator runs the binary, not a command line.
export-rts-server:
    mkdir -p build/rts-server
    "{{godot}}" --headless --path demos/rts --export-release "Linux Server" ../../build/rts-server/orbitnet-rts-server.x86_64

# Build the Asset Library payload: exactly the two addon directories, nothing else. This is what a user
# installs through AssetLib -> Install from file, and what release.yml attaches to a tag.
assetlib-zip VERSION="dev":
    #!/usr/bin/env bash
    set -euo pipefail
    out="build/orbitnet-{{VERSION}}.zip"
    mkdir -p build
    rm -f "$out"
    # Include the licences: an addon installed from a zip carries no repository around with it, and a user
    # who cannot find the licence terms inside the thing they installed will assume the worst.
    cp LICENSE LICENSE-MIT LICENSE-APACHE THIRD_PARTY.md addons/orbitnet/
    zip -qr "$out" addons/orbitnet addons/orbitnet_native
    rm -f addons/orbitnet/LICENSE addons/orbitnet/LICENSE-MIT addons/orbitnet/LICENSE-APACHE addons/orbitnet/THIRD_PARTY.md
    echo "wrote $out"
    unzip -l "$out" | tail -5
