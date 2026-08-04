//! OrbitNet — the Godot 4 binding layer.
//!
//! This crate is deliberately thin relative to `orbitnet-core`: every algorithm with behaviour
//! worth testing (tick pacing, clock discipline, resim planning, history storage, the wire
//! codec, freshness, interest management) lives in the core crate with no Godot dependency and
//! runs under plain `cargo test`. What lives here is the marshalling and orchestration shell:
//! Godot class registration, `Variant` ↔ packed-row conversion, the entity registry, signal
//! emission, and the `SceneMultiplayer` packet pump.
//!
//! The rule that keeps it thin: **core never sees a `Variant`.** If a type from `godot` starts
//! appearing in core's signatures, logic has leaked across the boundary.
//!
//! Class surface (all constructed from code by the `Net` facade — no scene ever instances one):
//!
//! | Class | Role |
//! |---|---|
//! | [`orbit_net::OrbitNet`] | Session singleton: tick loop, rollback scheduler, packet pump |
//! | [`sync::OrbitRollbackSynchronizer`] | Rollback state + input for one entity |
//! | [`sync::OrbitStateSynchronizer`] | Server-broadcast state, no rollback restore |
//! | [`interp::OrbitInterpolator`] | Render interpolation between net ticks |
//!
//! One module is not netcode at all: [`crash`] installs a native crash handler, because Godot's own
//! is `DEBUG_ENABLED`-only and a shipped build therefore dies silently. This extension loads in every
//! build, which makes it the only first-party place that can capture a release-template crash.

use godot::prelude::*;

pub mod binding;
pub mod crash;
pub mod interp;
pub mod orbit_net;
pub mod sync;

struct OrbitNetExtension;

#[gdextension]
unsafe impl ExtensionLibrary for OrbitNetExtension {}
