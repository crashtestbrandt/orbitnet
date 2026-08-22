//! Protocol identity: version, property schema, and the hash peers agree on.
//!
//! Two peers must agree on *exactly* what a state frame contains before a single byte of it means
//! anything. OrbitNet frames carry no property names and no type tags — just packed fields in
//! schema order — which is most of why they are small, and all of why a schema disagreement is
//! catastrophic rather than merely wrong.
//!
//! So the schema is hashed, and the server states the hash of every replicated entity in its
//! `EntityManifest` frame. A client whose locally built schema for that entity hashes differently is
//! told so by name, instead of decoding the bytes into garbage. This is a deliberate departure from
//! the GDScript backend, which silently misapplied state when two peers registered properties in
//! different orders.
//!
//! **Per entity, not per session.** The handshake carried a session-wide schema hash until protocol
//! major 3 and both production call sites passed `0`, because there is no such thing to hash: peers
//! legitimately have different entities registered at any moment, and the client's set is empty when
//! it handshakes. The field compared `0` against `0` between honest builds and was removed.

/// Wire protocol version, as `(major << 16) | (minor << 8) | patch`.
///
/// Peers must agree on **major** exactly. This is bumped whenever the frame layout changes in a way
/// an older peer would misread.
///
/// | Major | What changed |
/// | --- | --- |
/// | 2 | Quantized wire encodings (`QuantKind`). |
/// | 3 | Every datagram but the handshake carries a sequence number and a MAC (`crate::auth`); the handshake carries the session key and no longer carries a session-wide schema hash. |
/// | 4 | The hot-frame header carries `ack_token`: a server-minted per-frame value the client quotes back, so an ack names a frame the peer provably received. |
///
/// **Minor is not checked, and records a change no peer can misread.** The only kind that qualifies is an
/// OPTIONAL TRAILING field on a control frame: an older peer stops decoding before it and gets the
/// documented absent-value behaviour, a newer peer reads it when it is there. Anything that shifts an
/// existing field's offset is a MAJOR bump, because there the older peer decodes garbage.
pub const PROTOCOL_VERSION: u32 = 0x0004_0000;

/// Extract the major component of a protocol version.
#[must_use]
pub fn protocol_major(version: u32) -> u32 {
    version >> 16
}

/// Types a replicated property may have.
///
/// The set is deliberately small and fixed-width. Anything variable-length would defeat the
/// columnar history layout, whose whole point is that a tick's row is a constant-stride slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum PropKind {
    /// A boolean, stored as one byte.
    Bool = 1,
    /// A 32-bit signed integer.
    I32 = 2,
    /// A 64-bit signed integer.
    I64 = 3,
    /// A 32-bit float.
    F32 = 4,
    /// A 64-bit float.
    ///
    /// Godot's `float` is 64-bit. Storing it as [`PropKind::F32`] would quietly break a bit-exact
    /// resimulation, so the lossless path must use this.
    F64 = 5,
    /// Three 32-bit floats.
    Vec3 = 6,
    /// Four 32-bit floats.
    Quat = 7,
    /// Two 32-bit floats.
    Vec2 = 8,
    /// A 3x3 basis: nine 32-bit floats, rows in Godot's x/y/z axis order.
    Basis = 9,
    /// A full 3D transform: basis + origin, twelve 32-bit floats.
    Transform = 10,
}

impl PropKind {
    /// Bytes this property occupies in a history row.
    #[must_use]
    pub fn stride(self) -> usize {
        match self {
            PropKind::Bool => 1,
            PropKind::I32 | PropKind::F32 => 4,
            PropKind::I64 | PropKind::F64 | PropKind::Vec2 => 8,
            PropKind::Vec3 => 12,
            PropKind::Quat => 16,
            PropKind::Basis => 36,
            PropKind::Transform => 48,
        }
    }

    /// The discriminant, as mixed into the schema hash.
    #[must_use]
    pub fn tag(self) -> u8 {
        self as u8
    }
}

/// Which wire quantizer a property opted into at registration (`@ss3` / `@half` annotations).
///
/// Quantization is part of protocol identity — it changes both the wire bytes AND the canonical
/// value every peer stores — so it is hashed exactly like kind and role.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum QuantKind {
    /// Lossless: native bytes on the wire.
    #[default]
    None = 0,
    /// Smallest-three orientation packing: `Quat` and rotation-only `Basis` → 6 bytes.
    Ss3 = 1,
    /// IEEE binary16 per component: `Vec3` → 6, `Vec2` → 4, `F32` → 2 bytes.
    Half = 2,
}

impl QuantKind {
    /// The discriminant, as mixed into the schema hash.
    #[must_use]
    pub fn tag(self) -> u8 {
        self as u8
    }

    /// Whether this quantizer is defined for the given kind. An incompatible pairing must be
    /// rejected at registration, never silently misencoded.
    #[must_use]
    pub fn valid_for(self, kind: PropKind) -> bool {
        match self {
            QuantKind::None => true,
            QuantKind::Ss3 => matches!(kind, PropKind::Quat | PropKind::Basis),
            QuantKind::Half => matches!(kind, PropKind::Vec3 | PropKind::Vec2 | PropKind::F32),
        }
    }
}

/// What a property is *for*, which decides how the rollback loop treats it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum PropRole {
    /// Simulation state: restored before every replayed tick and compared for misprediction.
    State = 1,
    /// Player intent: authored by the owning client, consumed by the simulation.
    Input = 2,
    /// Presentation-only state: replicated, but never restored during rollback and never counted
    /// as a misprediction.
    ///
    /// The test is "does the simulation ever read it back", not "does it look presentational".
    /// An actuation value the sim rewrites every tick from `(state, input)` and never reads back is
    /// genuinely cosmetic. A self-referential integrator — a smoothed heading low-passing over its
    /// own previous value — is `State`, however presentational it looks.
    Cosmetic = 3,
}

impl PropRole {
    /// The discriminant, as mixed into the schema hash.
    #[must_use]
    pub fn tag(self) -> u8 {
        self as u8
    }

    /// Whether the rollback loop restores this property before replaying a tick.
    #[must_use]
    pub fn is_restored(self) -> bool {
        matches!(self, PropRole::State | PropRole::Input)
    }

    /// Whether a difference in this property counts as a misprediction.
    #[must_use]
    pub fn triggers_resim(self) -> bool {
        matches!(self, PropRole::State)
    }
}

/// One replicated property.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropSchema {
    /// Property name as the host engine knows it.
    pub name: String,
    /// Storage type.
    pub kind: PropKind,
    /// How the rollback loop treats it.
    pub role: PropRole,
    /// Byte offset into a history row.
    pub offset: usize,
    /// Wire quantizer (history rows stay native stride regardless).
    pub quant: QuantKind,
}

/// Accumulates properties into a schema and computes its hash.
///
/// Order matters: offsets are assigned in insertion order, and the hash covers that order, so two
/// peers that register the same properties differently produce different hashes and are told so.
#[derive(Debug, Clone, Default)]
pub struct SchemaBuilder {
    props: Vec<PropSchema>,
    stride: usize,
}

impl SchemaBuilder {
    /// An empty schema.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a lossless property, returning its index.
    pub fn push(&mut self, name: impl Into<String>, kind: PropKind, role: PropRole) -> usize {
        self.push_quantized(name, kind, role, QuantKind::None)
    }

    /// Append a property with a wire quantizer, returning its index.
    ///
    /// An invalid `(quant, kind)` pairing falls back to lossless — the caller is expected to have
    /// validated (and warned) already; silently misencoding is never an option.
    pub fn push_quantized(
        &mut self,
        name: impl Into<String>,
        kind: PropKind,
        role: PropRole,
        quant: QuantKind,
    ) -> usize {
        let quant = if quant.valid_for(kind) {
            quant
        } else {
            QuantKind::None
        };
        let index = self.props.len();
        self.props.push(PropSchema {
            name: name.into(),
            kind,
            role,
            offset: self.stride,
            quant,
        });
        self.stride += kind.stride();
        index
    }

    /// Number of properties.
    #[must_use]
    pub fn len(&self) -> usize {
        self.props.len()
    }

    /// Whether the schema is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.props.is_empty()
    }

    /// Total bytes one history row occupies.
    #[must_use]
    pub fn row_stride(&self) -> usize {
        self.stride
    }

    /// The accumulated properties.
    #[must_use]
    pub fn props(&self) -> &[PropSchema] {
        &self.props
    }

    /// Indices of the properties whose changes should trigger resimulation.
    #[must_use]
    pub fn resim_triggering(&self) -> Vec<usize> {
        self.props
            .iter()
            .enumerate()
            .filter(|(_, p)| p.role.triggers_resim())
            .map(|(i, _)| i)
            .collect()
    }

    /// Indices of the properties the rollback loop restores before a replayed tick.
    ///
    /// The twin of [`Self::resim_triggering`], and the order a bulk **restore** hook marshals in.
    /// `Cosmetic` is captured and replicated but never written back, so it is absent here while
    /// being present in the capture order — the two lists differ on exactly that.
    #[must_use]
    pub fn restored(&self) -> Vec<usize> {
        self.props
            .iter()
            .enumerate()
            .filter(|(_, p)| p.role.is_restored())
            .map(|(i, _)| i)
            .collect()
    }

    /// FNV-1a hash over `(name, kind, role)` in declaration order.
    ///
    /// FNV-1a rather than something stronger because this is an agreement check between cooperating
    /// peers, not a security boundary: it needs to be stable across platforms and trivially
    /// reimplementable, which it is.
    #[must_use]
    pub fn hash(&self) -> u32 {
        const OFFSET_BASIS: u32 = 0x811c_9dc5;
        const PRIME: u32 = 0x0100_0193;

        let mut hash = OFFSET_BASIS;
        let mut mix = |byte: u8| {
            hash ^= u32::from(byte);
            hash = hash.wrapping_mul(PRIME);
        };

        for prop in &self.props {
            for byte in prop.name.as_bytes() {
                mix(*byte);
            }
            mix(0xff); // field separator, so ("ab","c") and ("a","bc") differ
            mix(prop.kind.tag());
            mix(prop.role.tag());
            mix(prop.quant.tag());
        }
        hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SchemaBuilder {
        let mut schema = SchemaBuilder::new();
        schema.push("net_pos", PropKind::Vec3, PropRole::State);
        schema.push("net_orient", PropKind::Quat, PropRole::State);
        schema.push("net_boots_on", PropKind::Bool, PropRole::State);
        schema.push("net_rcs_lin", PropKind::Vec3, PropRole::Cosmetic);
        schema
    }

    #[test]
    fn protocol_major_is_extracted() {
        assert_eq!(protocol_major(PROTOCOL_VERSION), 4);
        assert_eq!(protocol_major(0x0005_0201), 5);
    }

    #[test]
    fn strides_match_the_declared_widths() {
        assert_eq!(PropKind::Bool.stride(), 1);
        assert_eq!(PropKind::I32.stride(), 4);
        assert_eq!(PropKind::F32.stride(), 4);
        assert_eq!(PropKind::I64.stride(), 8);
        assert_eq!(PropKind::F64.stride(), 8);
        assert_eq!(PropKind::Vec3.stride(), 12);
        assert_eq!(PropKind::Quat.stride(), 16);
        assert_eq!(PropKind::Vec2.stride(), 8);
        assert_eq!(PropKind::Basis.stride(), 36);
        assert_eq!(PropKind::Transform.stride(), 48);
    }

    /// Godot's `float` is 64-bit. Recording it as f32 would round every replayed value and break
    /// the bit-exact resim gate, so the distinction has to survive refactoring.
    #[test]
    fn f64_is_a_distinct_wider_kind_than_f32() {
        assert_ne!(PropKind::F32, PropKind::F64);
        assert_eq!(PropKind::F64.stride(), 8);
    }

    #[test]
    fn offsets_are_assigned_in_declaration_order() {
        let schema = sample();
        let props = schema.props();
        assert_eq!(props[0].offset, 0);
        assert_eq!(props[1].offset, 12);
        assert_eq!(props[2].offset, 28);
        assert_eq!(props[3].offset, 29);
        assert_eq!(schema.row_stride(), 41);
        assert_eq!(schema.len(), 4);
        assert!(!schema.is_empty());
    }

    #[test]
    fn roles_decide_restore_and_resim_behaviour() {
        assert!(PropRole::State.is_restored());
        assert!(PropRole::Input.is_restored());
        assert!(!PropRole::Cosmetic.is_restored());

        assert!(PropRole::State.triggers_resim());
        assert!(!PropRole::Input.triggers_resim());
        assert!(!PropRole::Cosmetic.triggers_resim());
    }

    #[test]
    fn cosmetic_props_are_excluded_from_the_misprediction_check() {
        let schema = sample();
        // net_rcs_lin is cosmetic, so index 3 must not be a resim trigger.
        assert_eq!(schema.resim_triggering(), vec![0, 1, 2]);
    }

    /// The capture order and the restore order differ on exactly the cosmetics: a bulk hook is
    /// handed every property to fill, and only the restored subset to read back.
    #[test]
    fn cosmetic_props_are_captured_but_never_restored() {
        let schema = sample();
        assert_eq!(schema.restored(), vec![0, 1, 2]);
        assert_eq!(schema.len(), 4, "and the fourth is still captured");
    }

    #[test]
    fn input_props_are_restored_even_though_they_never_trigger_resim() {
        let mut schema = SchemaBuilder::new();
        schema.push("net_pos", PropKind::Vec3, PropRole::State);
        schema.push("nin_move", PropKind::Vec2, PropRole::Input);
        assert_eq!(schema.restored(), vec![0, 1]);
        assert_eq!(schema.resim_triggering(), vec![0]);
    }

    #[test]
    fn hash_is_stable_for_an_identical_schema() {
        assert_eq!(sample().hash(), sample().hash());
    }

    #[test]
    fn hash_is_order_sensitive() {
        let mut reordered = SchemaBuilder::new();
        reordered.push("net_orient", PropKind::Quat, PropRole::State);
        reordered.push("net_pos", PropKind::Vec3, PropRole::State);
        reordered.push("net_boots_on", PropKind::Bool, PropRole::State);
        reordered.push("net_rcs_lin", PropKind::Vec3, PropRole::Cosmetic);
        assert_ne!(sample().hash(), reordered.hash());
    }

    #[test]
    fn hash_notices_type_and_role_changes() {
        let mut retyped = SchemaBuilder::new();
        retyped.push("net_pos", PropKind::Quat, PropRole::State);
        assert_ne!(
            retyped.hash(),
            {
                let mut base = SchemaBuilder::new();
                base.push("net_pos", PropKind::Vec3, PropRole::State);
                base
            }
            .hash()
        );

        let mut rerolled = SchemaBuilder::new();
        rerolled.push("net_pos", PropKind::Vec3, PropRole::Cosmetic);
        assert_ne!(
            rerolled.hash(),
            {
                let mut base = SchemaBuilder::new();
                base.push("net_pos", PropKind::Vec3, PropRole::State);
                base
            }
            .hash()
        );
    }

    /// The separator byte exists so that concatenating names cannot collide.
    #[test]
    fn name_boundaries_are_not_ambiguous() {
        let mut a = SchemaBuilder::new();
        a.push("ab", PropKind::Bool, PropRole::State);
        a.push("c", PropKind::Bool, PropRole::State);

        let mut b = SchemaBuilder::new();
        b.push("a", PropKind::Bool, PropRole::State);
        b.push("bc", PropKind::Bool, PropRole::State);

        assert_ne!(a.hash(), b.hash());
    }

    #[test]
    fn empty_schema_hashes_to_the_offset_basis() {
        assert_eq!(SchemaBuilder::new().hash(), 0x811c_9dc5);
        assert_eq!(SchemaBuilder::new().row_stride(), 0);
    }
}
