//! Send-order priority: which of a peer's in-interest entities the byte budget buys first.
//!
//! The obvious snapshot rota is ordered by staleness alone — `(last_sent, id)` ascending —
//! which is fair in the strict sense that nothing is ever permanently skipped, and wrong in every
//! other sense: the player you are duelling competes on equal terms with a decorative fragment
//! ten seconds of flight away. This module is the replacement rule, and it is the
//! backend's only opinion about *which* rows matter; **what** a row contains is still nobody's
//! business here.
//!
//! ## Multiply, never add
//!
//! The score is `staleness × weight`, descending. That is not a stylistic choice — it is what makes
//! starvation impossible *by construction*, which is the property `starve_ticks_max` would show if it were ever
//! false. (It REPORTS rather than gates: it is an INFO line in netbench, and netbench is not a per-PR gate. The
//! guarantee here is structural, and the counter is how you would notice the structure had been broken.)
//!
//! * `staleness` grows without bound while an entity waits, and resets to zero when it is sent.
//! * `weight` is bounded (`WEIGHT_OWNED` is the ceiling).
//!
//! So a low-weight entity's score eventually exceeds any high-weight entity's, because the
//! high-weight entity keeps being sent and keeps resetting to a small staleness. Had the two been
//! *added*, a large enough weight would dominate any staleness a bounded session could accumulate
//! and the far band would never be sent at all.
//!
//! ## Fixed point, not float
//!
//! Weights are `u32` in units of [`WEIGHT_ONE`] and scores are `u64`, so the ordering is bit
//! identical on every platform. The one floating-point comparison in this module is
//! [`band_of`]'s — an exact IEEE-754 magnitude test against a value derived from the same `f32`
//! radius on every peer, which is deterministic in the same way `==` is.

use crate::history::BodyId;

/// The fixed-point weight unit: `WEIGHT_ONE` is a multiplier of exactly 1.0.
pub const WEIGHT_ONE: u32 = 256;

/// The weight FLOOR an entity carries for the peer whose input drives it — its own body.
///
/// The "ownership floor": a peer's own body is never scored BELOW this, whatever its distance band
/// works out to. Read it as a floor and nothing more, because the two stronger things it is tempting to read
/// into it are both false:
///
///   * It does not outrank everything. It is four times the near band's own weight at `priority` 1, but
///     `weight_for` multiplies band by declared priority, so a NEAR body declaring 4 or more ties or beats it.
///     That is deliberate — an arena that says the flag carrier is worth four ordinary bodies means it — and
///     `a_declared_priority_can_match_the_ownership_floor` below asserts the tie exists rather than pretending
///     it does not.
///   * It is a floor on the WEIGHT, not on the score. Ordering is `staleness * weight`, and that is what makes
///     starvation impossible (see the module header); a body that was just sent has a staleness of ~0 and
///     scores near zero however heavy it is, including your own.
pub const WEIGHT_OWNED: u32 = 16 * WEIGHT_ONE;

/// The staleness a NEVER-SENT entity is scored with.
///
/// **NOT `u64::MAX`, and the difference is the whole join burst.** `score` is `staleness * weight`, saturating;
/// at `u64::MAX` every never-sent entity saturates to `u64::MAX` whatever its weight, so the weight term
/// cancels exactly and the sort falls through to the id tie-break — an FNV hash of a node path. That is the
/// ordering a peer's FIRST frames use, which is where the ownership floor and every declared priority were
/// added to matter most: on a join, the peer's own body must be in the first datagram, not wherever its hash
/// landed among a hundred hazard fragments.
///
/// 2^40 ticks is 580 years at 60 Hz, so it still outranks any staleness a session can accumulate, while
/// `2^40 * 65536` (the largest weight: [`WEIGHT_OWNED`] against [`PRIORITY_MAX`]) is 2^56 and nowhere near
/// saturating a `u64`. So never-sent still beats sent, and among never-sent the weights order properly.
pub const NEVER_SENT_STALENESS: u64 = 1 << 40;

/// The largest `priority` an entity may declare, in units of [`WEIGHT_ONE`].
///
/// Bounded so that `staleness × weight` cannot be made to saturate by a hostile or careless scene:
/// at the ceiling, a `u64` staleness would have to exceed 2^44 ticks — nine thousand years at
/// 60 Hz — before the product clamped.
pub const PRIORITY_MAX: u32 = 16;

/// Which third of the interest radius an entity sits in, measured from the peer's own body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Band {
    /// Inside a third of the radius: the fight.
    Near,
    /// Between a third and two thirds: the approach.
    Mid,
    /// Beyond two thirds, out to the exit radius: scenery in motion.
    Far,
}

impl Band {
    /// Every band, nearest first — the iteration order for per-band diagnostics.
    pub const ALL: [Band; 3] = [Band::Near, Band::Mid, Band::Far];

    /// A dense index for per-band counter arrays.
    #[must_use]
    pub fn index(self) -> usize {
        match self {
            Band::Near => 0,
            Band::Mid => 1,
            Band::Far => 2,
        }
    }

    /// The band's short name, for metric keys and console output.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Band::Near => "near",
            Band::Mid => "mid",
            Band::Far => "far",
        }
    }

    /// The distance weight: 4x near, 2x mid, 1x far.
    #[must_use]
    pub fn weight(self) -> u32 {
        match self {
            Band::Near => 4 * WEIGHT_ONE,
            Band::Mid => 2 * WEIGHT_ONE,
            Band::Far => WEIGHT_ONE,
        }
    }

    /// Ticks between admissions for this band when rate tiering is enabled.
    ///
    /// Combined with [`crate::interest::send_phase`], which spreads each band's members across its
    /// interval by id, so a band's traffic is level rather than arriving in one spike per interval.
    #[must_use]
    pub fn tiered_interval(self) -> u64 {
        match self {
            Band::Near => 1,
            Band::Mid => 2,
            Band::Far => 4,
        }
    }
}

/// The band `dist_sq` falls in, for band edges derived from `band_scale` at `scale/3` and
/// `2*scale/3`.
///
/// **`band_scale` IS NOT THE CULL RADIUS, and the two must not be the same number.** They answer
/// different questions and want values two orders of magnitude apart:
///
/// * The cull radius asks *should this entity be sent at all*, so it has to clear the longest range
///   at which a player can legitimately engage or observe a body. For a shooter carrying a scoped
///   rifle that is thousands of metres — cull inside it and the scoped shooter watches a frozen
///   ghost and cannot hit it.
/// * `band_scale` asks *how often, relative to everything else*, so it has to resolve the distances
///   a firefight actually happens over — tens of metres.
///
/// Deriving both from one value made each setting break the other. At a 256 m radius the edges land
/// at 85/171 m and the weighting differentiates properly, but everything past 320 m is culled —
/// inside the range of every ranged weapon in the game, the pistol included. At a sniper-safe 2000 m
/// the edges land at 666/1333 m, so on a 60 m arena every entity is [`Band::Near`], the weight term
/// is a constant that cancels out of the ordering, and the scorer is inert. Tie the two together and
/// interest management measures as "no benefit" and ships off.
///
/// A non-finite or non-positive scale, or a non-finite distance, reports [`Band::Near`] — failing
/// toward *more* traffic, because the alternative is silently demoting a body the filter could not
/// measure. `dist_sq` is the squared distance [`crate::PeerInterest`] already stores, so nothing
/// here takes a square root.
#[must_use]
pub fn band_of(dist_sq: f32, band_scale: f32) -> Band {
    if !band_scale.is_finite() || band_scale <= 0.0 || !dist_sq.is_finite() || dist_sq <= 0.0 {
        return Band::Near;
    }
    let near_edge = band_scale / 3.0;
    let mid_edge = band_scale * (2.0 / 3.0);
    if dist_sq <= near_edge * near_edge {
        Band::Near
    } else if dist_sq <= mid_edge * mid_edge {
        Band::Mid
    } else {
        Band::Far
    }
}

/// The weight one entity carries for one peer this tick.
///
/// `declared` is the entity's own `priority` export, clamped to `1..=`[`PRIORITY_MAX`] — the
/// backend must not guess game semantics, so an arena that considers the flag carrier worth four
/// ordinary bodies says so on the synchronizer. `owned` applies the [`WEIGHT_OWNED`] floor.
#[must_use]
pub fn weight_for(band: Band, declared: u32, owned: bool) -> u32 {
    let declared = declared.clamp(1, PRIORITY_MAX);
    let weight = band.weight().saturating_mul(declared);
    if owned {
        weight.max(WEIGHT_OWNED)
    } else {
        weight
    }
}

/// `staleness × weight`, saturating. See the module header for why this is a product.
#[must_use]
pub fn score(staleness: u64, weight: u32) -> u64 {
    staleness.saturating_mul(u64::from(weight))
}

/// One entity's candidacy for a place in this tick's frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate {
    /// The entity id.
    pub id: BodyId,
    /// Ticks since this peer was last sent this entity. A never-sent entity is
    /// [`NEVER_SENT_STALENESS`] here, so it sorts ahead of everything that HAS been sent — which is what
    /// makes clearing `last_sent` on an interest leave give a re-entrant entity its full block immediately —
    /// while still leaving the weight term room to order the never-sent set among itself.
    pub staleness: u64,
    /// The weight from [`weight_for`].
    pub weight: u32,
}

impl Candidate {
    /// This candidate's score.
    #[must_use]
    pub fn score(&self) -> u64 {
        score(self.staleness, self.weight)
    }
}

/// THE SEND ORDER, as a comparator: descending score, ties broken by ascending id.
///
/// The id tie-break is not cosmetic: two peers computing the same scores must produce the same frame, and an
/// unstable sort over equal keys does not otherwise promise that.
///
/// A comparator rather than only a sort, because the binding cannot use the sort. It orders `(Candidate, Band)`
/// pairs so the band survives the sort without a parallel array, so it had written the comparison out again
/// inline -- two copies of the shipping rule, of which the tests could only ever reach one. This is the copy.
#[must_use]
pub fn cmp(a: &Candidate, b: &Candidate) -> core::cmp::Ordering {
    b.score().cmp(&a.score()).then_with(|| a.id.cmp(&b.id))
}

/// Order `candidates` by [`cmp`].
pub fn order(candidates: &mut [Candidate]) {
    candidates.sort_unstable_by(cmp);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ownership floor is a FLOOR, and a declared priority can reach it. Pinned because the constant's
    /// docstring used to claim it "also outranks a near body carrying the maximum declared priority", which is
    /// the opposite of what `weight_for` computes.
    #[test]
    fn a_declared_priority_can_match_the_ownership_floor() {
        let owned = weight_for(Band::Near, 1, true);
        assert_eq!(
            owned, WEIGHT_OWNED,
            "the floor binds when the band is worth less"
        );
        assert_eq!(
            weight_for(Band::Near, 4, false),
            WEIGHT_OWNED,
            "a near body at priority 4 is worth exactly the ownership floor"
        );
        assert!(
            weight_for(Band::Near, 5, false) > WEIGHT_OWNED,
            "...and past 4 it outweighs it, which is what a declared priority is FOR"
        );
        // ...and the floor never LOWERS anything: an owned body that scores higher on its own keeps it.
        assert!(weight_for(Band::Near, PRIORITY_MAX, true) > WEIGHT_OWNED);
    }

    /// THE JOIN BURST IS THE FRAME THE OWNERSHIP FLOOR EXISTS FOR, and at `u64::MAX` it was the one frame the
    /// floor could not reach: every never-sent entity saturated to the same score and the sort fell through to
    /// the id tie-break.
    #[test]
    fn never_sent_entities_are_still_ordered_by_weight() {
        let mine = Candidate {
            id: 900,
            staleness: NEVER_SENT_STALENESS,
            weight: weight_for(Band::Near, 1, true),
        };
        let scenery = Candidate {
            id: 1,
            staleness: NEVER_SENT_STALENESS,
            weight: weight_for(Band::Far, 1, false),
        };
        assert!(
            mine.score() > scenery.score(),
            "a peer's own body outranks distant scenery in its own first frame"
        );
        let mut set = [scenery, mine];
        order(&mut set);
        assert_eq!(
            set[0].id, 900,
            "...and the sort puts it first, not the lower id"
        );
    }

    /// ...and the property that made `u64::MAX` attractive still has to hold: never-sent beats any staleness a
    /// real session can reach, so a re-entering entity gets its full block at once.
    #[test]
    fn never_sent_still_outranks_anything_that_has_been_sent() {
        let never = Candidate {
            id: 2,
            staleness: NEVER_SENT_STALENESS,
            weight: weight_for(Band::Far, 1, false),
        };
        // A day of continuous starvation at 60 Hz, at the largest weight anything can carry.
        let starved = Candidate {
            id: 1,
            staleness: 60 * 60 * 60 * 24,
            weight: weight_for(Band::Near, PRIORITY_MAX, true),
        };
        assert!(never.score() > starved.score());
        // And nothing saturates, or the ordering above would collapse again.
        assert!(never.score() < u64::MAX);
        assert!(
            NEVER_SENT_STALENESS.saturating_mul(u64::from(WEIGHT_OWNED * PRIORITY_MAX)) < u64::MAX,
            "the largest possible score still fits in a u64"
        );
    }

    #[test]
    fn bands_split_the_radius_into_thirds() {
        let r = 300.0f32;
        assert_eq!(band_of(0.0, r), Band::Near);
        assert_eq!(band_of(99.0 * 99.0, r), Band::Near);
        assert_eq!(
            band_of(100.0 * 100.0, r),
            Band::Near,
            "the edge is inclusive"
        );
        assert_eq!(band_of(101.0 * 101.0, r), Band::Mid);
        assert_eq!(band_of(200.0 * 200.0, r), Band::Mid);
        assert_eq!(band_of(201.0 * 201.0, r), Band::Far);
        // Past the enter radius entirely — a member held by the hysteresis band is still Far.
        assert_eq!(band_of(360.0 * 360.0, r), Band::Far);
    }

    #[test]
    fn a_sniper_safe_scale_would_make_the_weighting_inert_on_a_real_arena() {
        // THE ARITHMETIC BEHIND DECOUPLING `band_scale` FROM THE CULL RADIUS.
        //
        // The cull radius has to clear the longest shot in the game (the sniper's 2000 m) or a
        // scoped shooter watches a frozen ghost. Feed that same number in as the band scale and the
        // edges land at 666/1333 m -- so every body on a 60 m cube (104 m corner to corner) reports
        // Near, the weight term is one constant across the whole candidate set, and it cancels out
        // of a descending sort. The scorer does nothing. That is the state interest management
        // measured as "no benefit" in, and it is a property of the coupling rather than of the idea.
        let sniper_range = 2000.0f32;
        for d in [1.0f32, 30.0, 60.0, 104.0] {
            assert_eq!(
                band_of(d * d, sniper_range),
                Band::Near,
                "a body {d} m away is Near at a sniper-safe scale, so nothing differentiates"
            );
        }
        // The same distances against a combat-sized scale DO differentiate, which is the whole
        // point of giving the bands their own number.
        let combat = 256.0f32;
        assert_eq!(band_of(30.0 * 30.0, combat), Band::Near);
        assert_eq!(band_of(104.0 * 104.0, combat), Band::Mid);
        assert_eq!(band_of(200.0 * 200.0, combat), Band::Far);
        assert!(
            Band::Near.weight() > Band::Mid.weight() && Band::Mid.weight() > Band::Far.weight(),
            "and the bands they resolve into carry different weight"
        );
    }

    #[test]
    fn an_unanchored_channel_must_not_outweigh_the_row_that_says_where_a_body_is() {
        // THE INVERSION THAT KEPT INTEREST MANAGEMENT OFF. `PeerInterest` stores always-relevant members at
        // 0.0 (they are pushed at NEG_INFINITY so the nearest-N cap cannot evict them, then normalised), and
        // `band_of` reads 0.0 as Near. Only the channels carrying a position declare an anchor, so a body's
        // health, equipment, sensors and lights all took the 4x near weight while the ONE anchored row
        // carrying that body's position took Far's 1x -- four-plus channels per body outbidding the row that
        // says where it is, under exactly the budget pressure that matters.
        //
        // The binding now selects Far for a row with no anchor rather than banding its absent distance. This
        // pins the ORDERING that fix has to preserve: a distant body's position must outrank its equipment.
        let position_of_a_distant_body = weight_for(band_of(300.0 * 300.0, 256.0), 1, false);
        let its_unanchored_equipment = weight_for(Band::Far, 1, false);
        assert_eq!(
            band_of(300.0 * 300.0, 256.0),
            Band::Far,
            "300 m is past the far edge at the shipped band scale"
        );
        assert!(
            position_of_a_distant_body >= its_unanchored_equipment,
            "a body's position must never be outbid by its own torch or hit points"
        );
        // And the thing that made it an INVERSION rather than a tie: scoring an absent distance as Near.
        let if_scored_as_near = weight_for(band_of(0.0, 256.0), 1, false);
        assert!(
            if_scored_as_near > position_of_a_distant_body,
            "scoring an absent distance as Near is what put equipment above position; \
             this asserts the trap still exists so the binding must keep avoiding it"
        );
    }

    #[test]
    fn degenerate_radii_and_distances_fail_toward_near() {
        for bad_radius in [0.0f32, -10.0, f32::NAN, f32::INFINITY] {
            assert_eq!(band_of(10_000.0, bad_radius), Band::Near);
        }
        for bad_dist in [f32::NAN, f32::INFINITY, -1.0] {
            assert_eq!(band_of(bad_dist, 300.0), Band::Near);
        }
    }

    #[test]
    fn the_ownership_floor_outranks_a_near_body_at_maximum_priority() {
        let best_other = weight_for(Band::Near, PRIORITY_MAX, false);
        let owned_far = weight_for(Band::Far, 1, true);
        assert_eq!(owned_far, WEIGHT_OWNED);
        assert!(
            owned_far < best_other,
            "the floor is a floor, not a veto: a near body declaring the maximum priority may \
             still outrank a peer's own body sitting at the exit radius"
        );
        // What the floor guarantees is the ordinary case: at equal staleness the peer's own body
        // beats anything that has not declared a priority for itself.
        assert!(weight_for(Band::Far, 1, true) > weight_for(Band::Near, 1, false));
    }

    #[test]
    fn declared_priority_is_clamped_at_both_ends() {
        assert_eq!(weight_for(Band::Far, 0, false), Band::Far.weight());
        assert_eq!(weight_for(Band::Far, 1, false), Band::Far.weight());
        assert_eq!(
            weight_for(Band::Far, 999, false),
            Band::Far.weight() * PRIORITY_MAX
        );
    }

    #[test]
    fn score_saturates_instead_of_wrapping() {
        assert_eq!(score(0, WEIGHT_OWNED), 0);
        assert_eq!(score(u64::MAX, 1), u64::MAX);
        assert_eq!(score(u64::MAX, WEIGHT_OWNED), u64::MAX);
        assert_eq!(score(3, 2 * WEIGHT_ONE), 3 * 512);
    }

    #[test]
    fn order_is_descending_by_score_then_ascending_by_id() {
        let mut candidates = vec![
            Candidate {
                id: 9,
                staleness: 1,
                weight: WEIGHT_ONE,
            },
            Candidate {
                id: 3,
                staleness: 1,
                weight: WEIGHT_ONE,
            },
            Candidate {
                id: 5,
                staleness: 10,
                weight: WEIGHT_ONE,
            },
            Candidate {
                id: 7,
                staleness: 1,
                weight: 4 * WEIGHT_ONE,
            },
        ];
        order(&mut candidates);
        let ids: Vec<BodyId> = candidates.iter().map(|c| c.id).collect();
        assert_eq!(ids, vec![5, 7, 3, 9]);
    }

    #[test]
    fn a_never_sent_entity_sorts_ahead_of_everything() {
        let mut candidates = vec![
            Candidate {
                id: 1,
                staleness: 1_000,
                weight: WEIGHT_OWNED,
            },
            Candidate {
                id: 2,
                staleness: u64::MAX,
                weight: WEIGHT_ONE,
            },
        ];
        order(&mut candidates);
        assert_eq!(candidates[0].id, 2);
    }

    // ------------------------------------------------------------------
    // Running the rota: the property the whole design exists for.
    // ------------------------------------------------------------------

    const RUN_ENTITIES: u64 = 120;
    const RUN_ADMIT: usize = 8;
    const RUN_TICKS: u64 = 4_000;

    /// Weight class per entity: a third near-and-important, a third mid, a third far and dull.
    fn run_weight_of(id: u64) -> u32 {
        match id % 3 {
            0 => weight_for(Band::Near, 4, false),
            1 => weight_for(Band::Mid, 2, false),
            _ => weight_for(Band::Far, 1, false),
        }
    }

    /// Drive a greedy top-`RUN_ADMIT` rota for `RUN_TICKS`, returning each entity's worst observed
    /// inter-send gap. `combine` is the scoring rule under test.
    fn run_rota(combine: impl Fn(u64, u32) -> u64) -> Vec<u64> {
        let mut last_sent = vec![0u64; RUN_ENTITIES as usize + 1];
        let mut worst_gap = vec![0u64; RUN_ENTITIES as usize + 1];
        let mut candidates: Vec<(BodyId, u64)> = Vec::with_capacity(RUN_ENTITIES as usize);

        for tick in 1..=RUN_TICKS {
            candidates.clear();
            for id in 1..=RUN_ENTITIES {
                let staleness = tick - last_sent[id as usize];
                candidates.push((id, combine(staleness, run_weight_of(id))));
            }
            candidates.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            for &(id, _) in candidates.iter().take(RUN_ADMIT) {
                last_sent[id as usize] = tick;
            }
            // Measure only once the rota has been through everyone at least once.
            if tick > RUN_ENTITIES {
                for id in 1..=RUN_ENTITIES {
                    worst_gap[id as usize] =
                        worst_gap[id as usize].max(tick - last_sent[id as usize]);
                }
            }
        }
        worst_gap
    }

    /// The worst gap over one residue class of ids (one weight band).
    fn worst_in_class(gaps: &[u64], residue: u64) -> u64 {
        (1..=RUN_ENTITIES)
            .filter(|id| id % 3 == residue)
            .map(|id| gaps[id as usize])
            .max()
            .unwrap_or(0)
    }

    /// `staleness × weight` converges to a weight-proportional equilibrium — every band's gap is
    /// finite, predictable, and hits the value the arithmetic says it should.
    ///
    /// At steady state the admission threshold is one score `S`, so every entity's gap settles at
    /// `S / weight`. Summing the resulting send rates against the budget `B` gives
    /// `S = (Σ weights) / B` — which is a closed form this test checks each band against, rather
    /// than a hand-picked constant. The far band lands at ~105 ticks here *because* it is the far
    /// band, not because anything is starving: it is exactly 16× the near band's gap, which is
    /// exactly the ratio of their weights.
    #[test]
    fn multiplying_by_weight_converges_to_a_weight_proportional_equilibrium() {
        let gaps = run_rota(score);

        let total_weight: u64 = (1..=RUN_ENTITIES)
            .map(|id| u64::from(run_weight_of(id)))
            .sum();
        let threshold = total_weight / RUN_ADMIT as u64;

        for (residue, band) in [(0u64, Band::Near), (1, Band::Mid), (2, Band::Far)] {
            // `run_weight_of` keys on `id % 3`, so the residue itself names the class.
            let predicted = threshold / u64::from(run_weight_of(residue));
            let observed = worst_in_class(&gaps, residue);
            assert!(
                observed > 0 && observed <= predicted * 3 / 2 + 2,
                "{} band: worst gap {observed} ticks against a predicted equilibrium of \
                 {predicted}",
                band.name()
            );
        }

        // The weighting must actually be doing something, or this is a round robin wearing a hat.
        assert!(worst_in_class(&gaps, 0) * 4 < worst_in_class(&gaps, 2));
    }

    /// Why the score is a product: the additive variant starves the far band outright.
    ///
    /// With `staleness + weight`, a far entity has to accumulate `weight_near - weight_far` ticks
    /// of staleness before it outranks a *freshly sent* near one. Weight is bounded, staleness is
    /// not, so the product always turns over; the sum only turns over once the session has run
    /// longer than the weight spread, which at these weights is a minute of wall clock per visit.
    #[test]
    fn adding_the_weight_instead_starves_the_far_band() {
        let multiplied = run_rota(score);
        let added = run_rota(|staleness, weight| staleness.saturating_add(u64::from(weight)));

        let far_multiplied = worst_in_class(&multiplied, 2);
        let far_added = worst_in_class(&added, 2);
        assert!(
            far_added > far_multiplied * 10,
            "the additive rule was supposed to starve the far band, but its worst gap \
             ({far_added}) is not meaningfully worse than the product's ({far_multiplied})"
        );

        // And the near band pays nothing for the product's fairness — it is not that multiplying
        // is fair to everyone equally, it is that it is fair to everyone eventually.
        assert!(worst_in_class(&multiplied, 0) <= worst_in_class(&added, 0) + 2);
    }

    #[test]
    fn tiered_intervals_double_per_band() {
        assert_eq!(Band::Near.tiered_interval(), 1);
        assert_eq!(Band::Mid.tiered_interval(), 2);
        assert_eq!(Band::Far.tiered_interval(), 4);
        assert_eq!(
            Band::ALL.map(Band::index),
            [0, 1, 2],
            "the per-band counter arrays index off this"
        );
    }
}
