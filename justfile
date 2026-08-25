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

# Every filename the .gdextension names is one tools/build-native.sh produces, and every platform that
# script knows has a leg in binaries.yml. A bad entry fails at dlopen on one platform and nowhere else,
# so no other gate can see it.
descriptor-parity:
    tools/check-descriptor-parity.sh

# Headless project load for each Godot project -- catches every GDScript compile and parse error, with the
# project's warnings-as-errors promotion applied.
lint: (lint-project "harness") (lint-project "demos/rts") (lint-project "demos/hockey") (lint-project "demos/arena")

lint-project PROJECT:
    GODOT="{{godot}}" tools/lint-gdscript.sh {{PROJECT}}

# The unit suites. Sub-second, no scene tree, no physics, no sockets. This is where coverage belongs unless
# it genuinely needs a live session -- see CONTRIBUTING.md.
test: harness-test rts-test hockey-test arena-test

harness-test:
    "{{godot}}" --headless --path harness --script tests/support/run_unit_tests.gd

rts-test:
    "{{godot}}" --headless --path demos/rts --script tests/support/run_unit_tests.gd

hockey-test:
    "{{godot}}" --headless --path demos/hockey --script tests/support/run_unit_tests.gd

arena-test:
    "{{godot}}" --headless --path demos/arena --script tests/support/run_unit_tests.gd

# Prove the extension loads and registers its classes in a THROWAWAY project -- no autoload, no plugin, no
# addon GDScript. Catches the pointer-file / wrong-architecture / stale-filename class of failure.
harness-smoke:
    # --quit-after is a frame-count backstop only: smoke.gd quits itself as soon as it has a verdict.
    "{{godot}}" --headless --path harness --quit-after 600

# Both SERVER SHAPES end to end, in the harness project: a joining client's own state channel delivers rows
# against a dedicated server and against a listen server alike, and a third run with that channel vetoed
# proves the assertion can see a channel that delivers none.
server-shape-probe:
    GODOT="{{godot}}" tools/server-shape-probe.sh

# The two-peer networked gate: identical worlds, orders replicated, a forged order refused.
rts-probe:
    GODOT="{{godot}}" tools/rts-probe.sh

# The three-process networked gate: a DEDICATED server and two clients. It covers the interest axis neither
# other probe reaches -- membership filtering across three worlds, a per-peer veto, several seats on one
# connection, a declared anchor, and a session resume. See CONTRIBUTING.md for why three probes gate rather
# than one.
arena-probe:
    GODOT="{{godot}}" tools/arena-probe.sh

# Everything a PR must pass, in the order that fails fastest first. The shape probe runs before the two demo
# probes because it is the addon's own project: a failure there is the addon, where a failure in a demo could
# be either.
check: addon-tracked addon-drift net-check descriptor-parity native-test lint test server-shape-probe rts-probe arena-probe

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
# the air-hockey demo
#
# The COUPLED configuration -- a 60 Hz net tick inside the physics step, a 128-tick history -- which is the
# one demos/rts/project.godot names as unable to coexist with its own. Everything is on the rollback lane,
# including the puck, which has no input and is predicted on every peer.
# =====================================================================================================

# Single player. No peer, no socket -- the facade stays OFFLINE and every handle it hands out is inert, so
# this is the same code path a hosted session takes, minus the network.
hockey:
    "{{godot}}" --path demos/hockey

hockey-editor:
    "{{godot}}" --editor --path demos/hockey

# Listen server: authoritative AND a local player. Run this, then `just hockey-join` in more terminals --
# players are seated on alternating ends as they arrive, and a seat is released the moment one leaves.
hockey-host PORT="47800":
    "{{godot}}" --path demos/hockey -- --host={{PORT}}

hockey-join ADDR="127.0.0.1":
    "{{godot}}" --path demos/hockey -- --join={{ADDR}}

# Dedicated server: authoritative, headless, no local player.
hockey-serve PORT="47800":
    "{{godot}}" --headless --path demos/hockey -- --dedicated={{PORT}}

# A single-peer soak with the bench bot playing itself, writing the per-tick metrics CSV. This is how the
# reconcile-error columns get exercised without standing up a second machine -- BenchSubject defines them and
# the RTS demo cannot fill them, because a commander cursor whose whole simulation is a clamp never
# mispredicts.
hockey-stress SECONDS="60":
    "{{godot}}" --headless --path demos/hockey -- --host --bench --bench-bot=strafe_fire \
        --bench-duration={{SECONDS}} --quit-after={{SECONDS}}

hockey-lint: (lint-project "demos/hockey")


# =====================================================================================================
# the arena demo
#
# The third [orbitnet] configuration -- DECOUPLED at 30 Hz with a 128-tick history -- and the decouple is what
# the demo is about: the two lag-compensation features it shows are about the interpolation delay a RECEIVER
# applies, and a coupled demo has none. Three independent arenas in one session, several seats behind one
# connection, observers that declare where they watch from, and one fighter withheld from one peer.
# =====================================================================================================

# Single player. No peer, no socket -- the facade stays OFFLINE and every handle it hands out is inert.
arena:
    "{{godot}}" --path demos/arena

arena-editor:
    "{{godot}}" --editor --path demos/arena

# Listen server: authoritative AND a local player.
arena-host PORT="47800":
    "{{godot}}" --path demos/arena -- --host={{PORT}}

# A client. SEATS=2 is local split-screen: two locally-driven fighters behind ONE connection, each with its
# own interest anchor, and by default in DIFFERENT arenas -- which is the case worth seeing, because a
# connection with a body in two worlds has no inferred world of its own.
arena-join ADDR="127.0.0.1" SEATS="1":
    "{{godot}}" --path demos/arena -- --join={{ADDR}} --seats={{SEATS}}

# A spectator: no seat, no fighter, and an interest center AND ARENA declared for it by the server. Before
# `Net.set_peer_anchor()` this peer had no center, and a peer with no center is filtered in nowhere.
arena-observe ADDR="127.0.0.1":
    "{{godot}}" --path demos/arena -- --join={{ADDR}} --observe

# Dedicated server: authoritative, headless, no local player.
arena-serve PORT="47800":
    "{{godot}}" --headless --path demos/arena -- --dedicated={{PORT}}

# A single-peer soak with the bench bot playing itself. This demo is the one that can fill BenchSubject's
# HIT REGISTRATION columns, because it is the only one with an authoritative shot to confirm.
arena-stress SECONDS="60":
    "{{godot}}" --headless --path demos/arena -- --host --bench --bench-bot=strafe_fire \
        --bench-duration={{SECONDS}} --quit-after={{SECONDS}}

# Entity-slot pressure. A block names its entity by a 16-bit session slot, and the table binding slots to ids
# is distributed as a DELTA against a generation the receiver holds. PROPS is per arena; the session names
# three times that plus the fighters and the scorecards. Past 65,536 the server refuses to replicate the entity
# and says so, rather than wrapping an index onto a live one.
#
# WHAT --wire-log SHOWS, and it is the reason this recipe exists. At PROPS=8000 the session names 24,027
# entities and one whole table is ~530 kB. A JOINING peer still costs that -- it holds no table to diff
# against -- but every peer already in the session now costs a delta of the rows that actually moved. Measured
# on this machine, a second client joining a session of one: 1,074 kB before the delta manifest, 557 kB after,
# and the difference is the copy the peer that was already up to date used to be sent.
#
# It needs a second process to show anything: with no peer there is nobody to send a manifest to.
#
#   just arena-slots 8000 40 &
#   sleep 12; godot --headless --path demos/arena -- --join=127.0.0.1 --props=8000 --quit-after=20
arena-slots PROPS="8000" SECONDS="20":
    "{{godot}}" --headless --path demos/arena -- --host --props={{PROPS}} --wire-log --quit-after={{SECONDS}}

arena-lint: (lint-project "demos/arena")

# =====================================================================================================
# netbench -- the netcode test bench
#
# A below-ENet-reliability UDP impairment relay, a fleet of real headless bot clients driven through the
# real input path, and tick-domain gates. NOT a PR gate: it is a measurement tool, and its numbers depend
# on the machine. See docs/netbench.md.
# =====================================================================================================

# IT DRIVES A DEMO PROJECT. The repository root is not a Godot project, so every launch names demos/<DEMO>.
# The default is `arena` -- decoupled at 30 Hz with a 128-tick ring, the configuration closest to a shooter,
# and the only one that fills BenchSubject's hit-registration columns.
#
# SEAT COUNT BOUNDS THE FLEET. A client past the demo's seats is admitted as an OBSERVER, drives no body, and
# fails its own gate for having no samples. arena seats 24, hockey 32, rts 2.
#
# NETBENCH_OUT=<dir> writes the artifacts somewhere stable instead of a temp directory, which is what makes a
# before/after comparison possible: the same seed replays the same link, so two runs differ only by the change.
netbench CLIENTS="4" PROFILE="congested_wifi" SECONDS="20" SEED="1" POLICY="strafe" DEMO="arena":
    tools/netbench/bench.sh {{CLIENTS}} {{PROFILE}} {{SECONDS}} {{SEED}} {{POLICY}} {{DEMO}}

# Multi-machine bench: one SSH controller drives a server host plus bot-client hosts. Needs real reachable
# hosts, passwordless SSH and Godot on each. GAUNTLET_DRYRUN=1 prints the plan without running it. DEMO=<name>
# picks the demo project every host runs, and every host must run the same one.
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
    # Include the licenses: an addon installed from a zip carries no repository around with it, and a user
    # who cannot find the license terms inside the thing they installed will assume the worst.
    cp LICENSE LICENSE-MIT LICENSE-APACHE THIRD_PARTY.md addons/orbitnet/
    zip -qr "$out" addons/orbitnet addons/orbitnet_native
    rm -f addons/orbitnet/LICENSE addons/orbitnet/LICENSE-MIT addons/orbitnet/LICENSE-APACHE addons/orbitnet/THIRD_PARTY.md
    echo "wrote $out"
    unzip -l "$out" | tail -5
