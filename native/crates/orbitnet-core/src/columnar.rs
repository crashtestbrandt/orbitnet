//! Columnar packed per-entity history.
//!
//! The GDScript backend stored history as `Dictionary[tick, Dictionary[property, Variant]]` — a
//! fresh allocation per property per tick, hashed twice on every touch. This module is the flat
//! replacement: one `Vec<u8>` of `capacity × stride` bytes per entity, addressed `tick % capacity`,
//! with every property at a fixed offset assigned by the schema
//! ([`crate::protocol::SchemaBuilder`]). Recording a tick is a bounds-checked `copy_from_slice`;
//! trimming is implicit in the ring; steady state allocates nothing.
//!
//! The changed-property mask falls out of the layout: comparing two rows property-by-property is a
//! handful of fixed-size `memcmp`s ([`changed_mask`]), which is what the delta encoder rides. The
//! comparison is on **encoded bytes**, not decoded values — so `-0.0` vs `0.0` and NaN payload
//! differences count as changes. That is deliberate: the wire must reproduce the authoritative
//! row bit-exactly or the resimulation gate fails, so "different bits" *is* the definition of
//! "changed".
//!
//! Same eviction discipline as [`crate::history::TickRing`]: a write to a tick that has already
//! fallen out of the window is refused rather than silently corrupting the newer entry occupying
//! its slot, and tick indices near `u64::MAX` (which can originate from a decoded frame) must not
//! overflow the staleness math.

use crate::protocol::PropSchema;

/// Fixed-capacity, tick-addressed storage of fixed-stride rows.
#[derive(Debug, Clone)]
pub struct ColumnarHistory {
    stride: usize,
    rows: Vec<u8>,
    /// Slot occupancy: the tick stored in each slot, or `None`.
    ticks: Vec<Option<u64>>,
    latest: Option<u64>,
}

impl ColumnarHistory {
    /// Create a history of `capacity` rows (minimum 1) of `stride` bytes each.
    ///
    /// A zero `stride` is legal — an entity with no registered properties stores empty rows, and
    /// every row lookup returns an empty slice.
    #[must_use]
    pub fn new(stride: usize, capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            stride,
            rows: vec![0; stride * capacity],
            ticks: vec![None; capacity],
            latest: None,
        }
    }

    /// Bytes per row.
    #[must_use]
    pub fn stride(&self) -> usize {
        self.stride
    }

    /// Rows the ring can hold.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.ticks.len()
    }

    /// The newest tick stored, if any.
    #[must_use]
    pub fn latest_tick(&self) -> Option<u64> {
        self.latest
    }

    /// Whether `tick` is currently resident.
    #[must_use]
    pub fn has(&self, tick: u64) -> bool {
        self.ticks[self.slot(tick)] == Some(tick)
    }

    fn slot(&self, tick: u64) -> usize {
        (tick % self.ticks.len() as u64) as usize
    }

    /// Whether a write for `tick` would be refused as stale (see [`Self::begin_row`]).
    #[must_use]
    pub fn is_stale(&self, tick: u64) -> bool {
        match self.latest {
            // Saturating: `tick` may come off the wire, and a value near u64::MAX must not
            // overflow the comparison.
            Some(latest) => tick.saturating_add(self.ticks.len() as u64) <= latest,
            None => false,
        }
    }

    /// Start (or overwrite) the row for `tick`, returning it for the caller to fill.
    ///
    /// Returns `None` — storing nothing — when the tick has already fallen out of the window,
    /// which would otherwise clobber a newer tick's slot. A re-write of a resident tick hands
    /// back its existing bytes (the resim-correction path overwrites in place).
    pub fn begin_row(&mut self, tick: u64) -> Option<&mut [u8]> {
        if self.is_stale(tick) {
            return None;
        }
        let slot = self.slot(tick);
        if self.ticks[slot] != Some(tick) {
            // Evicting a different tick: zero the row so stale bytes from the evicted tick can
            // never masquerade as recorded state if the caller only partially fills it.
            let start = slot * self.stride;
            self.rows[start..start + self.stride].fill(0);
            self.ticks[slot] = Some(tick);
        }
        self.latest = Some(match self.latest {
            Some(latest) => latest.max(tick),
            None => tick,
        });
        let start = slot * self.stride;
        Some(&mut self.rows[start..start + self.stride])
    }

    /// Copy `src` in as the row for `tick`. Returns `false` on a stale tick or a stride mismatch.
    pub fn write_row(&mut self, tick: u64, src: &[u8]) -> bool {
        if src.len() != self.stride {
            return false;
        }
        match self.begin_row(tick) {
            Some(row) => {
                row.copy_from_slice(src);
                true
            }
            None => false,
        }
    }

    /// Borrow the row stored for `tick`, if resident.
    #[must_use]
    pub fn row(&self, tick: u64) -> Option<&[u8]> {
        let slot = self.slot(tick);
        if self.ticks[slot] == Some(tick) {
            let start = slot * self.stride;
            Some(&self.rows[start..start + self.stride])
        } else {
            None
        }
    }

    /// The newest resident row at or before `tick`.
    ///
    /// This is the display-body and input-extrapolation lookup: render or repeat the most recent
    /// data actually held rather than nothing at all when the exact tick never arrived.
    #[must_use]
    pub fn closest_at_or_before(&self, tick: u64) -> Option<(u64, &[u8])> {
        let span = self.ticks.len() as u64;
        let floor = tick.saturating_sub(span - 1);
        let mut probe = tick;
        loop {
            if self.has(probe) {
                return self.row(probe).map(|row| (probe, row));
            }
            if probe == floor {
                return None;
            }
            probe -= 1;
        }
    }

    /// Drop every stored row.
    pub fn clear(&mut self) {
        self.ticks.fill(None);
        self.rows.fill(0);
        self.latest = None;
    }
}

/// Compare two rows property-by-property, pushing one changed-flag per schema entry.
///
/// `out` is cleared first. Rows shorter than the schema demands are treated as fully changed —
/// that only happens on a caller bug, and "send everything" is the failure mode that stays
/// correct on the wire.
pub fn changed_mask(props: &[PropSchema], a: &[u8], b: &[u8], out: &mut Vec<bool>) {
    out.clear();
    for prop in props {
        let end = prop.offset + prop.kind.stride();
        if end > a.len() || end > b.len() {
            out.push(true);
            continue;
        }
        out.push(a[prop.offset..end] != b[prop.offset..end]);
    }
}

/// Total payload bytes the masked properties occupy.
#[must_use]
pub fn masked_size(props: &[PropSchema], mask: &[bool]) -> usize {
    props
        .iter()
        .zip(mask)
        .filter(|(_, &changed)| changed)
        .map(|(prop, _)| prop.kind.stride())
        .sum()
}

/// Append the masked properties of `row`, in schema order, to `out`.
pub fn write_masked(props: &[PropSchema], mask: &[bool], row: &[u8], out: &mut Vec<u8>) {
    for (prop, &changed) in props.iter().zip(mask) {
        if changed {
            let end = prop.offset + prop.kind.stride();
            out.extend_from_slice(&row[prop.offset..end]);
        }
    }
}

/// Copy the masked properties from a packed `payload` (schema order, changed-only) into `row`.
///
/// Returns the number of payload bytes consumed, or `None` if the payload is too short — a
/// truncated or hostile buffer must be reported, never sliced out of range.
pub fn apply_masked(
    props: &[PropSchema],
    mask: &[bool],
    payload: &[u8],
    row: &mut [u8],
) -> Option<usize> {
    let mut cursor = 0usize;
    for (prop, &changed) in props.iter().zip(mask) {
        if !changed {
            continue;
        }
        let width = prop.kind.stride();
        let end = prop.offset + width;
        if cursor + width > payload.len() || end > row.len() {
            return None;
        }
        row[end - width..end].copy_from_slice(&payload[cursor..cursor + width]);
        cursor += width;
    }
    Some(cursor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{PropKind, PropRole, SchemaBuilder};

    fn schema() -> SchemaBuilder {
        let mut builder = SchemaBuilder::new();
        builder.push("pos", PropKind::Vec3, PropRole::State);
        builder.push("flag", PropKind::Bool, PropRole::State);
        builder.push("energy", PropKind::F64, PropRole::State);
        builder
    }

    fn row_of(schema: &SchemaBuilder, fill: u8) -> Vec<u8> {
        vec![fill; schema.row_stride()]
    }

    #[test]
    fn stores_and_reads_back_rows() {
        let schema = schema();
        let mut history = ColumnarHistory::new(schema.row_stride(), 4);
        let row = row_of(&schema, 7);
        assert!(history.write_row(10, &row));
        assert_eq!(history.row(10), Some(row.as_slice()));
        assert_eq!(history.latest_tick(), Some(10));
        assert!(history.has(10));
        assert!(!history.has(9));
    }

    #[test]
    fn rejects_stride_mismatch() {
        let schema = schema();
        let mut history = ColumnarHistory::new(schema.row_stride(), 4);
        assert!(!history.write_row(1, &[0u8; 3]));
        assert!(!history.has(1));
    }

    #[test]
    fn evicts_by_wrapping_and_zeroes_the_slot() {
        let schema = schema();
        let mut history = ColumnarHistory::new(schema.row_stride(), 4);
        history.write_row(0, &row_of(&schema, 1));
        history.write_row(4, &row_of(&schema, 2));
        assert!(!history.has(0));
        assert_eq!(history.row(4), Some(row_of(&schema, 2).as_slice()));

        // A partially-filled begin_row after eviction must not leak evicted bytes.
        let mut history = ColumnarHistory::new(schema.row_stride(), 2);
        history.write_row(0, &row_of(&schema, 0xff));
        let row = history.begin_row(2).expect("not stale");
        assert!(row.iter().all(|&b| b == 0), "evicted bytes leaked");
    }

    #[test]
    fn refuses_ticks_that_fell_out_of_the_window() {
        let schema = schema();
        let mut history = ColumnarHistory::new(schema.row_stride(), 4);
        history.write_row(100, &row_of(&schema, 1));
        assert!(history.is_stale(96));
        assert!(!history.write_row(96, &row_of(&schema, 2)));
        assert!(history.begin_row(96).is_none());
        // 97 is still inside the window.
        assert!(history.write_row(97, &row_of(&schema, 3)));
    }

    #[test]
    fn survives_extreme_tick_indices() {
        let schema = schema();
        let mut history = ColumnarHistory::new(schema.row_stride(), 4);
        assert!(history.write_row(u64::MAX, &row_of(&schema, 1)));
        assert!(history.has(u64::MAX));
        assert!(!history.write_row(0, &row_of(&schema, 2)));
        assert!(history.closest_at_or_before(0).is_none());
    }

    #[test]
    fn rewriting_a_resident_tick_overwrites_in_place() {
        let schema = schema();
        let mut history = ColumnarHistory::new(schema.row_stride(), 4);
        history.write_row(5, &row_of(&schema, 1));
        history.write_row(5, &row_of(&schema, 9));
        assert_eq!(history.row(5), Some(row_of(&schema, 9).as_slice()));
    }

    #[test]
    fn closest_at_or_before_walks_back() {
        let schema = schema();
        let mut history = ColumnarHistory::new(schema.row_stride(), 8);
        history.write_row(10, &row_of(&schema, 1));
        history.write_row(13, &row_of(&schema, 2));
        let (tick, row) = history.closest_at_or_before(15).expect("13 is resident");
        assert_eq!(tick, 13);
        assert_eq!(row, row_of(&schema, 2).as_slice());
        assert_eq!(history.closest_at_or_before(12).map(|(t, _)| t), Some(10));
        assert!(history.closest_at_or_before(9).is_none());
    }

    #[test]
    fn zero_stride_history_is_legal() {
        let mut history = ColumnarHistory::new(0, 4);
        assert!(history.write_row(3, &[]));
        assert_eq!(history.row(3), Some(&[][..]));
    }

    #[test]
    fn clear_empties_everything() {
        let schema = schema();
        let mut history = ColumnarHistory::new(schema.row_stride(), 4);
        history.write_row(1, &row_of(&schema, 1));
        history.clear();
        assert!(!history.has(1));
        assert_eq!(history.latest_tick(), None);
        assert!(
            history.write_row(0, &row_of(&schema, 2)),
            "clear resets staleness"
        );
    }

    #[test]
    fn changed_mask_flags_exactly_the_differing_props() {
        let schema = schema();
        let a = row_of(&schema, 0);
        let mut b = row_of(&schema, 0);
        // Flip one byte inside the f64 at offset 13.
        b[14] = 1;
        let mut mask = Vec::new();
        changed_mask(schema.props(), &a, &b, &mut mask);
        assert_eq!(mask, vec![false, false, true]);
    }

    #[test]
    fn changed_mask_treats_identical_rows_as_clean() {
        let schema = schema();
        let a = row_of(&schema, 3);
        let mut mask = Vec::new();
        changed_mask(schema.props(), &a, &a.clone(), &mut mask);
        assert!(mask.iter().all(|&c| !c));
    }

    #[test]
    fn changed_mask_is_bit_level_not_value_level() {
        // -0.0 and 0.0 compare equal as floats but differ in bits: the wire must reproduce the
        // authoritative bits exactly, so this counts as a change.
        let mut builder = SchemaBuilder::new();
        builder.push("x", PropKind::F64, PropRole::State);
        let a = 0.0f64.to_le_bytes().to_vec();
        let b = (-0.0f64).to_le_bytes().to_vec();
        let mut mask = Vec::new();
        changed_mask(builder.props(), &a, &b, &mut mask);
        assert_eq!(mask, vec![true]);
    }

    #[test]
    fn masked_round_trip_reproduces_the_changed_props() {
        let schema = schema();
        let base = row_of(&schema, 0);
        let mut next = base.clone();
        next[0] = 42; // pos.x low byte
        next[13] = 7; // energy low byte

        let mut mask = Vec::new();
        changed_mask(schema.props(), &base, &next, &mut mask);
        assert_eq!(mask, vec![true, false, true]);

        let mut payload = Vec::new();
        write_masked(schema.props(), &mask, &next, &mut payload);
        assert_eq!(payload.len(), masked_size(schema.props(), &mask));
        assert_eq!(payload.len(), 12 + 8);

        let mut rebuilt = base.clone();
        let consumed =
            apply_masked(schema.props(), &mask, &payload, &mut rebuilt).expect("payload complete");
        assert_eq!(consumed, payload.len());
        assert_eq!(rebuilt, next);
    }

    #[test]
    fn apply_masked_reports_truncated_payloads() {
        let schema = schema();
        let mask = vec![true, true, true];
        let mut row = row_of(&schema, 0);
        assert!(apply_masked(schema.props(), &mask, &[1, 2, 3], &mut row).is_none());
    }

    #[test]
    fn short_rows_read_as_fully_changed() {
        let schema = schema();
        let full = row_of(&schema, 1);
        let short = vec![1u8; 4];
        let mut mask = Vec::new();
        changed_mask(schema.props(), &short, &full, &mut mask);
        assert_eq!(mask, vec![true, true, true]);
    }
}
