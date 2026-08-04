//! Render interpolation between net ticks.
//!
//! `OrbitInterpolator` is the drop-in for the old backend's TickInterpolator: purely local, no
//! wire traffic. Each tick loop it rotates the declared properties' values (`from ← to`,
//! `to ← live`), re-applies `from`, and then every render frame applies the blend
//! `from → to` at the tick clock's sub-tick factor. Player bodies do NOT use this — they render
//! through engine physics interpolation — but non-rollback replicated objects and the decoupled
//! low-rate configuration do.

use godot::classes::Node;
use godot::prelude::*;

use crate::binding::{self, PropBinding};
use orbitnet_core::{PropRole, SchemaBuilder};

/// Interpolate one Variant pair by `weight`, falling back to a step function.
fn interpolate(from: &Variant, to: &Variant, weight: f64) -> Variant {
    match (from.get_type(), to.get_type()) {
        (VariantType::FLOAT, VariantType::FLOAT) => {
            let a = from.try_to::<f64>().unwrap_or_default();
            let b = to.try_to::<f64>().unwrap_or_default();
            Variant::from(a + (b - a) * weight)
        }
        (VariantType::VECTOR3, VariantType::VECTOR3) => {
            let a = from.try_to::<Vector3>().unwrap_or_default();
            let b = to.try_to::<Vector3>().unwrap_or_default();
            Variant::from(a.lerp(b, weight as f32))
        }
        (VariantType::QUATERNION, VariantType::QUATERNION) => {
            let a = from.try_to::<Quaternion>().unwrap_or_default();
            let b = to.try_to::<Quaternion>().unwrap_or_default();
            if a.dot(b).abs() > 0.9999 {
                Variant::from(b)
            } else {
                Variant::from(a.slerp(b, weight as f32))
            }
        }
        (VariantType::TRANSFORM3D, VariantType::TRANSFORM3D) => {
            let a = from
                .try_to::<Transform3D>()
                .unwrap_or(Transform3D::IDENTITY);
            let b = to.try_to::<Transform3D>().unwrap_or(Transform3D::IDENTITY);
            Variant::from(a.interpolate_with(&b, weight as f32))
        }
        // Discrete types step at the tick boundary.
        _ => {
            if weight >= 1.0 {
                to.clone()
            } else {
                from.clone()
            }
        }
    }
}

/// Render interpolation for one node's declared properties.
#[derive(GodotClass)]
#[class(base=Node)]
pub struct OrbitInterpolator {
    base: Base<Node>,

    /// Node the declared property paths resolve against. Defaults to this node's parent.
    #[export]
    root: Option<Gd<Node>>,

    /// Interpolated entries, each `"NodePath:property"` or a bare `"property"`.
    #[export]
    properties: PackedStringArray,

    /// Whether interpolation runs; when false the live values are left alone.
    #[export]
    enabled: bool,

    bindings: Vec<PropBinding>,
    from_values: Vec<Variant>,
    to_values: Vec<Variant>,
    primed: bool,
    teleporting: bool,
    last_seen_tick: u64,
}

#[godot_api]
impl INode for OrbitInterpolator {
    fn init(base: Base<Node>) -> Self {
        Self {
            base,
            root: None,
            properties: PackedStringArray::new(),
            enabled: true,
            bindings: Vec::new(),
            from_values: Vec::new(),
            to_values: Vec::new(),
            primed: false,
            teleporting: false,
            last_seen_tick: 0,
        }
    }

    fn ready(&mut self) {
        self.base_mut().set_process(true);
        self.process_settings();
    }

    fn process(&mut self, _delta: f64) {
        if !self.enabled {
            return;
        }
        // Rotate at each tick boundary, observed through the published frontier — this node
        // needs no reference to the singleton and stays inert when no loop is running.
        let frontier = crate::orbit_net::global_frontier_tick();
        if frontier != self.last_seen_tick {
            self.last_seen_tick = frontier;
            self.push_state();
        }
        if !self.primed || self.teleporting {
            return;
        }
        let weight = crate::orbit_net::global_tick_factor();
        for (index, binding) in self.bindings.iter().enumerate() {
            if !binding.target.is_instance_valid() {
                continue;
            }
            let value = interpolate(&self.from_values[index], &self.to_values[index], weight);
            let mut target = binding.target.clone();
            target.set(&binding.name, &value);
        }
    }
}

#[godot_api]
impl OrbitInterpolator {
    /// Add an interpolated property (`node` is a `Node`, `NodePath` or string path).
    #[func]
    fn add_property(&mut self, node: Variant, property: GString) {
        let Some(root) = self.resolved_root() else {
            return;
        };
        let prop = property.to_string();
        let entry = if let Ok(target) = node.try_to::<Gd<Node>>() {
            let path = root.get_path_to(&target).to_string();
            GString::from(format!("{path}:{prop}").as_str())
        } else {
            GString::from(format!("{node}:{prop}").as_str())
        };
        if !self.properties.as_slice().contains(&entry) {
            self.properties.push(&entry);
        }
    }

    /// Resolve the declared properties. Call after configuration changes.
    #[func]
    fn process_settings(&mut self) {
        let mut schema = SchemaBuilder::new();
        let mut unresolved = PackedStringArray::new();
        self.bindings.clear();
        binding::resolve_entries(
            self.resolved_root().as_ref(),
            &self.properties,
            PropRole::Cosmetic,
            &mut schema,
            &mut self.bindings,
            &mut unresolved,
        );
        self.from_values = vec![Variant::nil(); self.bindings.len()];
        self.to_values = vec![Variant::nil(); self.bindings.len()];
        self.primed = false;
    }

    /// Record the live values as the new interpolation target (called at each tick boundary).
    #[func]
    fn push_state(&mut self) {
        std::mem::swap(&mut self.from_values, &mut self.to_values);
        for (index, binding) in self.bindings.iter().enumerate() {
            if binding.target.is_instance_valid() {
                self.to_values[index] = binding.target.get(&binding.name);
            }
        }
        if !self.primed {
            self.from_values = self.to_values.clone();
            self.primed = true;
        }
        self.teleporting = false;
    }

    /// Snap both interpolation endpoints to the live values (no smoothing across a teleport).
    #[func]
    fn teleport(&mut self) {
        for (index, binding) in self.bindings.iter().enumerate() {
            if binding.target.is_instance_valid() {
                self.to_values[index] = binding.target.get(&binding.name);
            }
        }
        self.from_values = self.to_values.clone();
        self.primed = true;
        self.teleporting = true;
    }
}

impl OrbitInterpolator {
    fn resolved_root(&self) -> Option<Gd<Node>> {
        self.root.clone().or_else(|| self.base().get_parent())
    }
}
