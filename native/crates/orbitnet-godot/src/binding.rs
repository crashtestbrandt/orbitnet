//! Property binding: the bridge between Godot `Variant` properties and packed history rows.
//!
//! Everything expensive is resolved exactly once, at registration: the `"NodePath:property"`
//! entries become cached `(Gd<Node>, StringName)` handles with a storage kind and a fixed byte
//! offset assigned by the schema. After that, capturing a tick is a walk over the handles doing
//! one `Object::get` each into a preallocated row, and restoring is the mirror image — no
//! property-table walks, no per-tick allocation, no `Dictionary`.
//!
//! **A lane may replace that walk with one crossing.** A synchronizer that declares a
//! `bulk_capture_method`, `bulk_restore_method` or `bulk_apply_method` is handed a preallocated
//! `Array` and fills or reads it in a single `Object::call`, so the crossing count for that walk
//! drops from `S` to `1` — see [`BulkHook`]. It is opt-in per synchronizer, the walk stays for
//! every lane that declares none, and the row layout is unchanged either way: the hook supplies the
//! `Variant`s and nothing else.
//!
//! Three hook **directions**, one per walk. They differ in which slots the array carries and in
//! what multiplies the walk they replace:
//!
//! | Direction | The walk it replaces | Slots | Multiplier |
//! | --- | --- | --- | --- |
//! | capture | [`capture_row`] | every binding in the lane | replayed ticks × planned entities |
//! | restore | [`apply_row`] with `restored_only` | the restored subset | replayed ticks × planned entities |
//! | apply | [`apply_row`] on the receive path | every binding in the lane | delivered blocks, no replay multiplier |
//! | apply | [`apply_quantized_row`], the write-back | every binding in the lane | replayed ticks × planned entities |
//!
//! **The apply slots are the CAPTURE slots, not the restore slots.** The two lists differ by
//! exactly the lane's `Cosmetic` entries, which are captured and replicated but never restored, so
//! a game that hands its restore method to the apply direction on a lane declaring cosmetics reads
//! shifted slots and writes wrong values with nothing erroring.
//!
//! The role split matters during rollback: `State` and `Input` are restored before a replayed
//! tick; `Cosmetic` is captured and replicated but never restored and never counted as a
//! misprediction (see docs/api.md — the test is "does the simulation read it
//! back", and the game's own state script is the authority on which props qualify).

use godot::classes::Node;
use godot::prelude::*;

use orbitnet_core::{PropKind, PropRole, QuantKind, SchemaBuilder};

/// The untyped Godot `Array` a bulk hook marshals through.
type VariantArray = Array<Variant>;

/// One resolved replicated property.
pub struct PropBinding {
    /// The node the property lives on.
    pub target: Gd<Node>,
    /// Cached property name.
    pub name: StringName,
    /// Storage type.
    pub kind: PropKind,
    /// Rollback treatment.
    pub role: PropRole,
    /// Byte offset in the row.
    pub offset: usize,
    /// Wire quantizer (from the entry's `@ss3` / `@half` annotation).
    pub quant: QuantKind,
}

/// Split a declared entry into its resolvable part and its quantizer annotation.
///
/// `"Node:prop@ss3"` → (`"Node:prop"`, `Some(Ss3)`); an unknown annotation returns `None` in the
/// second slot with `annotated = true` so the caller can warn instead of silently shipping
/// lossless.
///
/// **The `@` is looked for in the PROPERTY COMPONENT ONLY**, which is why this finds the last `:`
/// first rather than splitting the whole entry on its last `@`. Godot generates node names
/// containing `@` for anything added without an explicit name — `@Node3D@27` — so a perfectly
/// ordinary entry like `"@Node3D@27:health"` splits on the wrong character otherwise: `27:health`
/// reads as an unknown annotation, the diagnostic names the wrong problem, and the truncated head
/// `"@Node3D"` cannot resolve, so the property falls off the wire with a message about something
/// else. A property name may not contain `@`, so the last `@` after the last `:` is unambiguous.
pub fn split_quant(entry: &str) -> (&str, Option<QuantKind>, bool) {
    // The property component starts after the last `:`, or at 0 for a bare `"property"`.
    let prop_at = match entry.rfind(':') {
        Some(colon) => colon + 1,
        None => 0,
    };
    match entry[prop_at..].rfind('@') {
        Some(offset) => {
            let cut = prop_at + offset;
            let head = &entry[..cut];
            match &entry[cut + 1..] {
                "ss3" => (head, Some(QuantKind::Ss3), true),
                "half" => (head, Some(QuantKind::Half), true),
                _ => (head, None, true),
            }
        }
        None => (entry, Some(QuantKind::None), false),
    }
}

/// Map a Godot value onto the storage type OrbitNet records it as.
///
/// `Int` and `Float` widen to 64 bits because that is what Godot's `int` and `float` actually are.
/// Narrowing `float` to `f32` here would round every replayed value and quietly break a bit-exact
/// resimulation — the failure would surface far from its cause, so it is worth being explicit.
pub fn prop_kind_for(value: &Variant) -> Option<PropKind> {
    match value.get_type() {
        VariantType::BOOL => Some(PropKind::Bool),
        VariantType::INT => Some(PropKind::I64),
        VariantType::FLOAT => Some(PropKind::F64),
        VariantType::VECTOR3 => Some(PropKind::Vec3),
        VariantType::QUATERNION => Some(PropKind::Quat),
        VariantType::VECTOR2 => Some(PropKind::Vec2),
        VariantType::BASIS => Some(PropKind::Basis),
        VariantType::TRANSFORM3D => Some(PropKind::Transform),
        _ => None,
    }
}

/// Map a Godot `Variant.Type` ordinal onto a storage kind.
pub fn kind_for_variant_type(ord: i64) -> Option<PropKind> {
    match ord {
        x if x == VariantType::BOOL.ord() as i64 => Some(PropKind::Bool),
        x if x == VariantType::INT.ord() as i64 => Some(PropKind::I64),
        x if x == VariantType::FLOAT.ord() as i64 => Some(PropKind::F64),
        x if x == VariantType::VECTOR3.ord() as i64 => Some(PropKind::Vec3),
        x if x == VariantType::QUATERNION.ord() as i64 => Some(PropKind::Quat),
        x if x == VariantType::VECTOR2.ord() as i64 => Some(PropKind::Vec2),
        x if x == VariantType::BASIS.ord() as i64 => Some(PropKind::Basis),
        x if x == VariantType::TRANSFORM3D.ord() as i64 => Some(PropKind::Transform),
        _ => None,
    }
}

fn put_vec3(out: &mut [u8], v: Vector3) {
    out[0..4].copy_from_slice(&v.x.to_le_bytes());
    out[4..8].copy_from_slice(&v.y.to_le_bytes());
    out[8..12].copy_from_slice(&v.z.to_le_bytes());
}

fn get_vec3(bytes: &[u8]) -> Vector3 {
    Vector3::new(
        f32::from_le_bytes(le4(&bytes[0..4])),
        f32::from_le_bytes(le4(&bytes[4..8])),
        f32::from_le_bytes(le4(&bytes[8..12])),
    )
}

/// Split a `"NodePath:property"` entry into its parts.
///
/// A bare `"property"` resolves against the root itself, which is the common case.
pub fn split_entry(entry: &str) -> (&str, &str) {
    match entry.rsplit_once(':') {
        Some((path, prop)) => (path, prop),
        None => (".", entry),
    }
}

/// Resolve the storage kind of one `"NodePath:property"` entry against a root node.
///
/// Prefers the declared property type over the current value's type: a property that happens to
/// hold an int right now while being declared `float` would otherwise infer the wrong width and
/// silently corrupt the row layout. Falls back to the live value, which covers
/// dynamically-added properties.
pub fn resolve_entry(root: &Gd<Node>, entry: &str) -> Option<(Gd<Node>, StringName, PropKind)> {
    let (path, prop) = split_entry(entry);
    let target: Gd<Node> = if path == "." || path.is_empty() {
        root.clone()
    } else {
        root.get_node_or_null(path)?
    };

    for row in target.get_property_list().iter_shared() {
        let Some(name) = row.get("name") else {
            continue;
        };
        if name.to_string() != prop {
            continue;
        }
        let declared = row
            .get("type")
            .and_then(|v| v.try_to::<i64>().ok())
            .unwrap_or(-1);
        if let Some(kind) = kind_for_variant_type(declared) {
            return Some((target, StringName::from(prop), kind));
        }
    }

    let kind = prop_kind_for(&target.get(prop))?;
    Some((target, StringName::from(prop), kind))
}

/// What one pass of [`resolve_entries`] could not honour, kept so a game can assert on it.
///
/// One value rather than two out-parameters because the two lists answer one question between
/// them — "which declarations are not doing what they say" — and because a lane accumulates both
/// across several calls (a rollback synchronizer resolves state, cosmetic and input entries into
/// the same report).
#[derive(Default)]
pub struct EntryReport {
    /// Declared entries that did not resolve against the root. Each is a property that never
    /// reaches the wire.
    pub unresolved: PackedStringArray,
    /// Declared entries whose `@` quantizer annotation is **not in force**, verbatim. Three ways
    /// in, and all three ship the property at its native width or not at all:
    ///
    /// | Cause | What happens to the property |
    /// | --- | --- |
    /// | the annotation is not `@ss3` or `@half` | ships lossless |
    /// | the pairing is invalid for the resolved type | ships lossless |
    /// | the entry did not resolve | does not ship at all (also in `unresolved`) |
    pub fallbacks: PackedStringArray,
}

/// The pairing table and the byte costs both quantizer diagnostics quote.
///
/// Written out rather than derived because a message a reader has to look up is a message they do
/// not read. The numbers are what `orbitnet_core::quant::wire_stride` produces, and the protocol
/// suite pins them.
const QUANT_HELP: &str = "@half applies to Vector3 (12 B -> 6 B), Vector2 (8 B -> 4 B) and F32 \
     (4 B -> 2 B); @ss3 applies to Quaternion (16 B -> 6 B) and Basis (36 B -> 6 B). A GDScript \
     `float` is an f64 and an `int` is an i64, so a bare scalar cannot be narrowed at all -- pack \
     scalars into a Vector3 and quantize that.";

/// Raise a quantizer diagnostic, as an ERROR in a checked build and a WARNING otherwise.
///
/// `debug-assertions` is set only in `[profile.template-debug]`, which is the library Godot loads
/// for every editor session and every run from source. The exported `template_release` build
/// compiles the error arm away, so a shipped game logs a warning where a developer gets a stack
/// trace and a red line they cannot scroll past. A dropped annotation is a **bandwidth** bug: the
/// game runs, the frames decode, and the only symptom is a wire that is two to six times wider
/// than the property list claims, which is exactly the class of mistake a warning loses.
macro_rules! quant_diagnostic {
    ($($arg:tt)*) => {
        if cfg!(debug_assertions) {
            godot_error!($($arg)*);
        } else {
            godot_warn!($($arg)*);
        }
    };
}

/// The annotation spelling for a quantizer, as it appears in a declared entry.
fn quant_tag(quant: QuantKind) -> &'static str {
    match quant {
        QuantKind::Ss3 => "@ss3",
        QuantKind::Half => "@half",
        QuantKind::None => "(none)",
    }
}

/// Resolve a list of declared entries into bindings, pushing them into a schema.
///
/// `label` names the synchronizer in every diagnostic this raises, the same way
/// [`resolve_hook`] and the membership resolver take one. Without it a session with dozens of
/// entities reports which entry is wrong but not which of the fifty nodes declaring it.
///
/// Unresolvable entries are collected rather than silently dropped: a typo in a property path
/// otherwise becomes a state field that never replicates, which is painful to diagnose from the
/// symptom. So is a dropped quantizer, for the opposite reason — everything works, and only the
/// byte count is wrong — so those are collected too. The caller publishes both lists.
pub fn resolve_entries(
    root: Option<&Gd<Node>>,
    entries: &PackedStringArray,
    role: PropRole,
    schema: &mut SchemaBuilder,
    bindings: &mut Vec<PropBinding>,
    report: &mut EntryReport,
    label: &str,
) {
    for entry in entries.as_slice() {
        let entry = entry.to_string();
        let (resolvable, parsed_quant, annotated) = split_quant(&entry);
        let resolved = root.and_then(|r| resolve_entry(r, resolvable));
        match resolved {
            Some((target, name, kind)) => {
                let quant = match parsed_quant {
                    Some(q) if q.valid_for(kind) => q,
                    Some(q) => {
                        // Bound before the call: the godot log macros take positional arguments
                        // only, so everything a message names has to be an identifier in scope.
                        let tag = quant_tag(q);
                        let native = kind.stride();
                        quant_diagnostic!(
                            "{label}: {entry:?} asks for {tag} on a property that resolved to \
                             {kind:?}, which {tag} does not apply to. The annotation is DROPPED \
                             and the property ships LOSSLESS at {native} B per tick, saving \
                             nothing. {QUANT_HELP}"
                        );
                        report.fallbacks.push(&entry);
                        QuantKind::None
                    }
                    None => {
                        let native = kind.stride();
                        quant_diagnostic!(
                            "{label}: {entry:?} carries an @ annotation that is neither @ss3 nor \
                             @half. It is DROPPED and the property, which resolved to {kind:?}, \
                             ships LOSSLESS at {native} B per tick. {QUANT_HELP}"
                        );
                        report.fallbacks.push(&entry);
                        QuantKind::None
                    }
                };
                // The full annotated entry is the hashed name: peers disagreeing on a
                // quantizer disagree on the schema, which the manifest check reports.
                let index = schema.push_quantized(entry, kind, role, quant);
                let offset = schema.props()[index].offset;
                bindings.push(PropBinding {
                    target,
                    name,
                    kind,
                    role,
                    offset,
                    quant,
                });
            }
            None => {
                report.unresolved.push(&entry);
                if annotated {
                    // An entry that never resolved carries its annotation off the wire with it,
                    // so it belongs on the fallback list as much as an invalid pairing does. This
                    // is what lets a boot check on one list catch a mistyped PATH as well as a
                    // mistyped pairing, instead of a game having to assert on two.
                    report.fallbacks.push(&entry);
                }
            }
        }
    }
}

/// Report the declared entries that did not resolve, naming the synchronizer.
///
/// Shared by all three lanes so the three messages are the same message. The state lane and the
/// interpolator used to raise none at all: one collected the list into a getter nothing was
/// obliged to read, the other allocated the list and dropped it, so a mistyped path on either was
/// visible only to somebody who already suspected it.
///
/// A **warning**, not an error, unlike the quantizer diagnostics above: an unresolved entry is
/// also reachable transiently while a scene is being assembled, since `process_settings` may
/// legitimately run before a child exists.
pub fn warn_unresolved(label: &str, unresolved: &PackedStringArray) {
    if unresolved.is_empty() {
        return;
    }
    let annotated = unresolved
        .as_slice()
        .iter()
        .filter(|entry| {
            let text = entry.to_string();
            split_quant(&text).2
        })
        .count();
    let dropped = if annotated == 0 {
        String::new()
    } else {
        format!(
            " {annotated} of them carried an @ quantizer annotation, dropped along with the \
             entry and listed by quantizer_fallbacks()."
        )
    };
    let count = unresolved.len();
    let plural = if count == 1 { "y" } else { "ies" };
    godot_warn!(
        "{label}: {count} declared propert{plural} did not resolve against the root: \
         {unresolved:?} — an unresolved entry silently falls off the wire, so fix the path or \
         the type.{dropped}"
    );
}

/// Encode one property value into its row slice. Returns false on a type mismatch.
fn encode_value(kind: PropKind, value: &Variant, out: &mut [u8]) -> bool {
    match kind {
        PropKind::Bool => {
            let Ok(v) = value.try_to::<bool>() else {
                return false;
            };
            out[0] = u8::from(v);
        }
        PropKind::I32 => {
            let Ok(v) = value.try_to::<i64>() else {
                return false;
            };
            out.copy_from_slice(&(v as i32).to_le_bytes());
        }
        PropKind::I64 => {
            let Ok(v) = value.try_to::<i64>() else {
                return false;
            };
            out.copy_from_slice(&v.to_le_bytes());
        }
        PropKind::F32 => {
            let Ok(v) = value.try_to::<f32>() else {
                return false;
            };
            out.copy_from_slice(&v.to_le_bytes());
        }
        PropKind::F64 => {
            let Ok(v) = value.try_to::<f64>() else {
                return false;
            };
            out.copy_from_slice(&v.to_le_bytes());
        }
        PropKind::Vec3 => {
            let Ok(v) = value.try_to::<Vector3>() else {
                return false;
            };
            out[0..4].copy_from_slice(&v.x.to_le_bytes());
            out[4..8].copy_from_slice(&v.y.to_le_bytes());
            out[8..12].copy_from_slice(&v.z.to_le_bytes());
        }
        PropKind::Quat => {
            let Ok(v) = value.try_to::<Quaternion>() else {
                return false;
            };
            out[0..4].copy_from_slice(&v.x.to_le_bytes());
            out[4..8].copy_from_slice(&v.y.to_le_bytes());
            out[8..12].copy_from_slice(&v.z.to_le_bytes());
            out[12..16].copy_from_slice(&v.w.to_le_bytes());
        }
        PropKind::Vec2 => {
            let Ok(v) = value.try_to::<Vector2>() else {
                return false;
            };
            out[0..4].copy_from_slice(&v.x.to_le_bytes());
            out[4..8].copy_from_slice(&v.y.to_le_bytes());
        }
        PropKind::Basis => {
            let Ok(v) = value.try_to::<Basis>() else {
                return false;
            };
            put_vec3(&mut out[0..12], v.rows[0]);
            put_vec3(&mut out[12..24], v.rows[1]);
            put_vec3(&mut out[24..36], v.rows[2]);
        }
        PropKind::Transform => {
            let Ok(v) = value.try_to::<Transform3D>() else {
                return false;
            };
            put_vec3(&mut out[0..12], v.basis.rows[0]);
            put_vec3(&mut out[12..24], v.basis.rows[1]);
            put_vec3(&mut out[24..36], v.basis.rows[2]);
            put_vec3(&mut out[36..48], v.origin);
        }
    }
    true
}

fn le4(bytes: &[u8]) -> [u8; 4] {
    [bytes[0], bytes[1], bytes[2], bytes[3]]
}

fn le8(bytes: &[u8]) -> [u8; 8] {
    [
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]
}

/// Decode one property value from its row slice.
fn decode_value(kind: PropKind, bytes: &[u8]) -> Variant {
    match kind {
        PropKind::Bool => Variant::from(bytes[0] != 0),
        PropKind::I32 => Variant::from(i64::from(i32::from_le_bytes(le4(bytes)))),
        PropKind::I64 => Variant::from(i64::from_le_bytes(le8(bytes))),
        PropKind::F32 => Variant::from(f64::from(f32::from_le_bytes(le4(bytes)))),
        PropKind::F64 => Variant::from(f64::from_le_bytes(le8(bytes))),
        PropKind::Vec3 => Variant::from(Vector3::new(
            f32::from_le_bytes(le4(&bytes[0..4])),
            f32::from_le_bytes(le4(&bytes[4..8])),
            f32::from_le_bytes(le4(&bytes[8..12])),
        )),
        PropKind::Quat => Variant::from(Quaternion::new(
            f32::from_le_bytes(le4(&bytes[0..4])),
            f32::from_le_bytes(le4(&bytes[4..8])),
            f32::from_le_bytes(le4(&bytes[8..12])),
            f32::from_le_bytes(le4(&bytes[12..16])),
        )),
        PropKind::Vec2 => Variant::from(Vector2::new(
            f32::from_le_bytes(le4(&bytes[0..4])),
            f32::from_le_bytes(le4(&bytes[4..8])),
        )),
        PropKind::Basis => {
            let mut basis = Basis::IDENTITY;
            basis.rows[0] = get_vec3(&bytes[0..12]);
            basis.rows[1] = get_vec3(&bytes[12..24]);
            basis.rows[2] = get_vec3(&bytes[24..36]);
            Variant::from(basis)
        }
        PropKind::Transform => {
            let mut basis = Basis::IDENTITY;
            basis.rows[0] = get_vec3(&bytes[0..12]);
            basis.rows[1] = get_vec3(&bytes[12..24]);
            basis.rows[2] = get_vec3(&bytes[24..36]);
            Variant::from(Transform3D::new(basis, get_vec3(&bytes[36..48])))
        }
    }
}

/// Capture every binding's live value into `row`.
pub fn capture_row(bindings: &[PropBinding], row: &mut [u8]) {
    for binding in bindings {
        if !binding.target.is_instance_valid() {
            continue;
        }
        let value = binding.target.get(&binding.name);
        let end = binding.offset + binding.kind.stride();
        if !encode_value(binding.kind, &value, &mut row[binding.offset..end]) {
            // Type drifted from what registration saw (e.g. a Variant property that changed
            // shape). Leave the slice untouched — zeros — rather than shearing bytes.
        }
        // Canonicalize a quantized property at the source: the row must hold exactly the value
        // a peer reconstructs from the wire, or masks and mispredict compares fall apart.
        orbitnet_core::quant::canonicalize_value(
            binding.kind,
            binding.quant,
            &mut row[binding.offset..end],
        );
    }
}

/// Write a row's QUANTIZED properties back onto their bound objects.
///
/// The state-capture write-back: forward simulation must continue from the canonical
/// (wire-representable) value, or a replay restored from the row would diverge from the forward
/// pass that recorded it — every peer would see phantom mispredicts at quantization scale.
///
/// **The targeted walk, and it stays the answer for a lane with fewer than two quantized
/// properties.** It writes `Q` properties, not `S`, and `Q` is zero for a lane that declares no
/// quantizer. A lane that declares a `bulk_apply_method` **and** carries at least two quantized
/// properties routes this through that hook instead — `Q` crossings become one, and the game is
/// handed the whole set because handing back one array is one call rather than `Q` writes. The
/// caller decides, once at resolve time, from [`quantized_count`]; this function is what it falls
/// back to, and what runs whenever the hook cannot.
pub fn apply_quantized_row(bindings: &[PropBinding], row: &[u8]) {
    for binding in bindings {
        if binding.quant == QuantKind::None {
            continue;
        }
        if !binding.target.is_instance_valid() {
            continue;
        }
        let end = binding.offset + binding.kind.stride();
        let value = decode_value(binding.kind, &row[binding.offset..end]);
        let mut target = binding.target.clone();
        target.set(&binding.name, &value);
    }
}

/// Apply `row` back onto the bound properties.
///
/// `restored_only`: apply just the roles the rollback loop restores (`State` + `Input`),
/// skipping `Cosmetic`. Pass `false` on the receive path, where cosmetics must land too.
///
/// **The fallback for both apply directions.** A lane with a resolved restore hook (`true`) or
/// apply hook (`false`) stages a hook call instead; this runs when the lane declared none, when the
/// node behind the hook has been freed, and when the game resized the array. The row lands either
/// way, so a broken hook is a slower lane rather than a lane that stops applying.
pub fn apply_row(bindings: &[PropBinding], row: &[u8], restored_only: bool) {
    for binding in bindings {
        if restored_only && !binding.role.is_restored() {
            continue;
        }
        if !binding.target.is_instance_valid() {
            continue;
        }
        let end = binding.offset + binding.kind.stride();
        let value = decode_value(binding.kind, &row[binding.offset..end]);
        let mut target = binding.target.clone();
        target.set(&binding.name, &value);
    }
}

/// FNV-1a over a byte string, 64-bit — the stable entity id derived from a node path.
///
/// Both peers derive the same id because the `MultiplayerSpawner` guarantees identical node
/// names — the invariant any node-path-derived identity scheme leans on, made
/// explicit here instead of implicit in RPC dispatch.
#[must_use]
pub fn fnv64(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

// ======================================================================================
// Bulk marshalling hooks
// ======================================================================================

/// One staged bulk-hook call: the game's marshalling method, and the values array it marshals
/// through.
///
/// Every field is a handle, so staging one costs a refcount bump rather than a copy. It exists as
/// a separate value because the call has to be made with **every `bind` dropped** — the same rule
/// phase 2 of the rollback loop follows for `_rollback_tick`, and for the same reason: this is
/// game code, and game code legally calls back into the facade. See the re-entrancy paragraph in
/// the `orbit_net` module header.
pub struct HookCall {
    node: Gd<Node>,
    method: StringName,
    lane: i64,
    values: VariantArray,
}

impl HookCall {
    /// Run the game's marshalling method. A freed node is skipped, not an error.
    pub fn invoke(&mut self) {
        if self.node.is_instance_valid() {
            self.node.call(
                &self.method,
                &[Variant::from(self.lane), self.values.to_variant()],
            );
        }
    }
}

/// A resolved bulk marshalling hook: one game method that fills (capture) or reads (restore,
/// apply) a whole lane's values in **one** script-boundary crossing instead of one per property.
///
/// The per-property walk costs `S` `Object::get` calls to capture a lane and up to `S` `Object::set`
/// calls to restore it, and the rollback loop pays both **per replayed tick, per entity**. A hook
/// replaces each of those walks with a single `Object::call`, so the crossing count per lane per
/// tick drops from `S` to `1`.
///
/// **The direction lives in the slot list, not in this type.** A capture hook and an apply hook are
/// resolved over every binding in the lane; a restore hook is resolved over the restored subset.
/// Everything below is the same code for all three.
///
/// **Opt-in per synchronizer.** A lane with no declared hook keeps the per-property walk, byte for
/// byte, so nothing existing changes behaviour.
///
/// **The row layout is unchanged.** The hook only supplies the `Variant`s; the encode, the byte
/// offsets and the quantized canonicalization are the same code [`capture_row`] runs. Masks, delta
/// bases and the mispredict compare read that layout, so it is not the hook's to decide.
pub struct BulkHook {
    /// The node carrying the game's marshalling method.
    target: Gd<Node>,
    /// Cached method name.
    method: StringName,
    /// Synchronizer path plus lane, for the one diagnostic this can raise. Built at resolve time
    /// rather than at the call site so the hot path formats nothing.
    label: String,
    /// The lane ordinal handed to the method as its first argument, so one method can serve both
    /// lanes of a rollback synchronizer.
    lane: i64,
    /// Indices into the lane's binding list, in the order the values array carries them.
    slots: Vec<usize>,
    /// Reused every call, so a hooked lane allocates nothing per tick.
    values: VariantArray,
    /// Set once the game handed back an array of the wrong length, so the fallback reports itself
    /// once rather than once per entity per replayed tick.
    warned: bool,
}

impl BulkHook {
    /// Stage this hook's call, to be run once every `bind` is dropped.
    ///
    /// `None` when the node behind the hook has been freed — cloning a freed handle panics under
    /// godot-rust's balanced safeguards, so it must be filtered rather than cloned.
    pub fn stage(&self) -> Option<HookCall> {
        if !self.target.is_instance_valid() {
            return None;
        }
        Some(HookCall {
            node: self.target.clone(),
            method: self.method.clone(),
            lane: self.lane,
            values: self.values.clone(),
        })
    }

    /// The binding indices this hook marshals, in array order.
    #[must_use]
    pub fn slots(&self) -> &[usize] {
        &self.slots
    }
}

/// Validate a declared bulk-hook method name against a root node, once per declaration.
///
/// `None` — every lane keeps the per-property walk — for an empty declaration (the default, and not
/// a diagnostic), no root, or a method the root does not have. Only the last is an error, and it is
/// a loud one: a typo in the method name would otherwise read as "the hook is on" while every tick
/// still paid the full walk. Separate from [`resolve_hook`] so one typo is one message rather than
/// one per lane.
pub fn hook_target(root: Option<&Gd<Node>>, method: &GString, label: &str) -> Option<Gd<Node>> {
    let name = method.to_string();
    if name.is_empty() {
        return None;
    }
    let root = root?;
    if !root.has_method(&name) {
        godot_error!(
            "{label}: bulk hook {name:?} is not a method on {} — every lane keeps the \
             per-property walk. Declare `func {name}(lane: int, values: Array) -> void`.",
            root.get_path()
        );
        return None;
    }
    Some(root.clone())
}

/// Build one lane's hook against a validated target.
///
/// `None` for no target (see [`hook_target`]) or a lane with nothing in it — an empty slot list is
/// a crossing that would marshal nothing.
pub fn resolve_hook(
    target: Option<&Gd<Node>>,
    method: &GString,
    lane: i64,
    slots: Vec<usize>,
    label: &str,
) -> Option<BulkHook> {
    let target = target?;
    if slots.is_empty() {
        return None;
    }
    let mut values = VariantArray::new();
    values.resize(slots.len(), &Variant::nil());
    Some(BulkHook {
        target: target.clone(),
        method: StringName::from(method.to_string().as_str()),
        label: format!("{label} lane {lane}"),
        lane,
        slots,
        values,
        warned: false,
    })
}

/// Encode the values the hook's method wrote into `row`.
///
/// Returns `false` when the array came back the wrong length, which the caller answers with the
/// per-property walk — a game that resized the array has broken the contract, and recording a
/// short row would shear the layout every peer decodes against.
///
/// Every slot the game leaves alone keeps last tick's value, because the array is preallocated and
/// reused. **Fill every slot.** There is no sentinel for "unset" and none can be added: `null` is a
/// value the encode would reject like any other type mismatch, which leaves the previous bytes in
/// place — the same outcome, reached less obviously.
pub fn capture_row_from_hook(
    hook: &mut BulkHook,
    bindings: &[PropBinding],
    row: &mut [u8],
) -> bool {
    if hook.values.len() != hook.slots.len() {
        if !hook.warned {
            hook.warned = true;
            godot_error!(
                "{}: bulk capture hook returned {} values for {} properties — this lane falls \
                 back to the per-property walk. Do not resize the array it is handed.",
                hook.label,
                hook.values.len(),
                hook.slots.len()
            );
        }
        return false;
    }
    for (slot, &index) in hook.slots.iter().enumerate() {
        let binding = &bindings[index];
        let end = binding.offset + binding.kind.stride();
        let value = hook.values.at(slot);
        if !encode_value(binding.kind, &value, &mut row[binding.offset..end]) {
            // Type drifted from what registration saw. Leave the slice untouched — the same
            // answer capture_row gives, for the same reason: zeros shear less than a half-written
            // value, and a stale value shears not at all.
        }
        // Canonicalize at the source, exactly as the per-property walk does: the row must hold the
        // value a peer reconstructs from the wire, or masks and mispredict compares fall apart.
        orbitnet_core::quant::canonicalize_value(
            binding.kind,
            binding.quant,
            &mut row[binding.offset..end],
        );
    }
    true
}

/// Decode `row` into the hook's array and stage the call that hands it to the game.
///
/// The decode happens here, with the synchronizer bound; the call happens after every `bind` is
/// dropped.
///
/// **Serves the restore direction and the apply direction unchanged.** It is driven entirely by
/// `hook.slots()`, so the only difference between the two is the slot list the hook was resolved
/// with: the restored subset for a restore hook, every binding in the lane for an apply hook. The
/// name is the direction it was written for, not a restriction on the direction it serves.
///
/// `None` means the hook cannot run — a freed node, or an array the game resized — and the caller
/// answers it with the per-property walk, so the row lands either way. The wrong-length case is
/// reported once and then keeps falling back, the same contract [`capture_row_from_hook`] enforces
/// on the other direction: silently resizing the array back would leave a game that broke the
/// contract with no signal that it had.
pub fn stage_restore_from_row(
    hook: &mut BulkHook,
    bindings: &[PropBinding],
    row: &[u8],
) -> Option<HookCall> {
    if hook.values.len() != hook.slots.len() {
        if !hook.warned {
            hook.warned = true;
            godot_error!(
                "{}: bulk restore hook's array is {} values for {} properties — this lane falls \
                 back to the per-property walk. Do not resize the array it is handed.",
                hook.label,
                hook.values.len(),
                hook.slots.len()
            );
        }
        return None;
    }
    for (slot, &index) in hook.slots.iter().enumerate() {
        let binding = &bindings[index];
        let end = binding.offset + binding.kind.stride();
        hook.values
            .set(slot, &decode_value(binding.kind, &row[binding.offset..end]));
    }
    hook.stage()
}

/// How many of a lane's bindings carry a wire quantizer — the `Q` the write-back walks.
///
/// Counted **once, at resolve time**, because it is the gate on routing the write-back through an
/// apply hook: at `Q >= 2` one hook call replaces `Q` `Object::set` calls, at `Q <= 1` the targeted
/// walk is the same crossing count or cheaper and keeps it. Recomputing it per tick would spend the
/// walk it exists to avoid.
#[must_use]
pub fn quantized_count(bindings: &[PropBinding]) -> usize {
    bindings
        .iter()
        .filter(|binding| binding.quant != QuantKind::None)
        .count()
}

/// The declared entry names a hook marshals, in the order its array carries them.
///
/// Published so a game can assert the order it wrote its hook against rather than infer it from
/// the order it happened to declare its properties in. Reordering a property list silently
/// reorders this.
pub fn hook_order(props: &[orbitnet_core::PropSchema], slots: &[usize]) -> PackedStringArray {
    let mut out = PackedStringArray::new();
    for &index in slots {
        out.push(&GString::from(props[index].name.as_str()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole of [`split_quant`], because it is the one function in this file pure enough to
    /// test here — everything else holds a `Gd<Node>` or a `Variant` and needs a live engine. It
    /// also carries the only failure mode in the file that was invisible: a wrong split hands the
    /// resolver a truncated path, so the property falls off the wire while the message blames the
    /// annotation.
    #[test]
    fn split_quant_reads_the_property_component_only() {
        // A bare property, no annotation: the whole entry is resolvable, and lossless.
        assert_eq!(
            split_quant("prop"),
            ("prop", Some(QuantKind::None), false),
            "an unannotated bare property"
        );

        // The two annotations, on a bare property and on a path.
        assert_eq!(
            split_quant("prop@half"),
            ("prop", Some(QuantKind::Half), true),
            "the annotation comes off and the head stays resolvable"
        );
        assert_eq!(
            split_quant("Path:prop@half"),
            ("Path:prop", Some(QuantKind::Half), true),
            "a path keeps its colon"
        );
        assert_eq!(
            split_quant("Path:prop@ss3"),
            ("Path:prop", Some(QuantKind::Ss3), true),
            "and the other annotation reads the same way"
        );

        // GODOT-GENERATED NODE NAMES CONTAIN `@`. A node added without an explicit name is named
        // `@Node3D@27`, so this is an ordinary entry rather than an exotic one. Splitting the
        // whole entry on its last `@` parsed `27:prop` as an annotation, warned about the wrong
        // thing, and left `"@Node3D"` as the path to resolve — which cannot.
        assert_eq!(
            split_quant("@Node3D@27:prop"),
            ("@Node3D@27:prop", Some(QuantKind::None), false),
            "an @ in the NODE NAME is not an annotation"
        );
        assert_eq!(
            split_quant("@Node3D@27:prop@half"),
            ("@Node3D@27:prop", Some(QuantKind::Half), true),
            "and a real annotation on such a node still parses, path intact"
        );

        // An unrecognized tag: `None` in the second slot with `annotated = true`, which is what
        // separates "the game meant something by this" from "the game said nothing".
        assert_eq!(
            split_quant("prop@bogus"),
            ("prop", None, true),
            "an unknown tag is reported rather than assumed lossless"
        );
    }
}
