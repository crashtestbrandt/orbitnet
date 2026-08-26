//! An adversarial state search over the interest lane, driving the shipping rules.
//!
//! **WHY THIS EXISTS AS A TEST RATHER THAN AS A REVIEW.** The interest resync is a two-ended protocol
//! whose failures are schedules, not lines. Six review rounds read the same code and missed a defect
//! that left both ends quiescent, neither asking, and permanently disagreeing; a search like this one
//! reported it on its first run. Two later rounds each found that the round before had introduced a
//! new one while fixing the last. Reading does not converge on this code. A search does.
//!
//! **IT DRIVES THE REAL RULES.** Every decision below is the shipping function, not a restatement:
//!
//! | Rule | Function |
//! | --- | --- |
//! | is a whole set owed, and may it go out yet | [`interest_table_due`] |
//! | what does a whole set state, and what does stating it retire | [`interest_table_to_send`] |
//! | does an echo settle the demand | [`PeerState::note_interest_echo`] |
//! | what does a section carry | [`build_interest_section`] |
//! | is a prefix retired, and does giving up owe a set | [`PeerState::retire_interest_delta`] |
//! | does a section apply to what the receiver holds | [`InterestDeltaSection::applies_to`] |
//! | what does applying one do to the mirror | [`apply_interest_section`] |
//! | what does adopting a whole set do | [`adopt_whole_set`] |
//! | may this frame be skipped entirely | [`snapshot_frame_is_skipped`] |
//! | does the client owe an input frame | [`input_frame_is_owed`] |
//!
//! **THE ADVERSARY.** Every frame in either direction may be delivered, delayed, or dropped —
//! including a reliable one, because reliable here means retransmitted rather than delivered: one
//! sequence counter and one replay window are shared with the unreliable traffic, so a retransmit
//! landing far enough behind is refused. Delay past the window is therefore a drop, and delay inside
//! it is reordering, which is what several of the defects above needed.
//!
//! **WHAT IT ASSERTS.** From every reachable state, quiescing the link — no loss, no reordering —
//! must bring the client's mirror to the server's set within a bounded number of ticks. A schedule
//! that cannot is either a permanent divergence or a livelock, and both are failures.
//!
//! Kept to a few hundred milliseconds so it can sit in `just check`: exhaustive to a shallow depth
//! over a small universe, then a deterministic pseudo-random sweep for the deeper schedules.
//!
//! **WHAT THE SWEEPS CATCH, AND WHAT THEY DO NOT.** Seven defects this repository actually shipped
//! were re-injected one at a time, running ONLY the two schedule sweeps. They fail on three:
//!
//! | Defect | Sweeps | Covered elsewhere |
//! | --- | --- | --- |
//! | the retry stamp is cleared on the settle, so there is no backoff | **caught** | a unit test |
//! | the server stays silent while the gate is shut | **caught** | a unit test |
//! | a stale echo regresses the baseline | **caught** | a unit test |
//! | the echo settles a demand no table ever answered | blind | a unit test |
//! | a retry mints a fresh generation | blind | two unit tests |
//! | a re-sent table is admitted at a generation already held | blind | the scripted test below |
//! | a section applies from a frame that is not the newest | blind | a unit test |
//!
//! It was two of seven before the fidelity repairs below, and the third came from making the wire
//! take a tick and asserting on the event stream. The count is of the SWEEPS alone — the scripted
//! tests in this file exercise the model but are not a search, and folding them into this score is
//! the mistake this comment used to make.
//!
//! **THE SWEEPS FIND LIVENESS DEFECTS MORE READILY THAN SAFETY ONES.** Something that stops progress
//! or costs a whole set per round trip trips the convergence assertion or the budget. One that leaves
//! the two ends briefly wrong is harder, because this model's client asks more readily than a real
//! one: it raises `want_interest` whenever a section or set fails to resolve, so a schedule that
//! would strand a real client can repair itself here.
//!
//! **FOUR THINGS THAT WERE WRONG WITH THIS FILE, AND ARE THE REASON IT READ STRONGER THAN IT WAS:**
//!
//! | | |
//! | --- | --- |
//! | the model never stamped `interest_delta_tick` | `retire_interest_delta` returned at its first line on every call, so the give-up cause — and the whole set it owes — was unreachable in every schedule |
//! | an i.i.d. per-tick adversary | sixty-four consecutive drops at one third each is (1/3)^64, so the give-up stayed unreachable even once the stamp was there. Every third seed now blacks out one direction outright |
//! | both directions delivered inside the posting tick | the modelled round trip was ZERO, putting every rule triggered by "longer than a round trip" out of reach |
//! | `quiesce` compared only the two sets | a rule whose only output is a SIGNAL was invisible, and most of this lane's rules are exactly that |
//!
//! Each was silent: the sweeps were green throughout, and green read as "searched and found nothing"
//! when it meant "never looked". [`Reached`] is what makes that a failing test instead — it counts
//! the states each sweep entered and asserts they are non-zero.
//!
//! Two limits remain, stated because an overstated search is worse than none:
//!
//! - **It covers rules, not their call sites.** A defect in code no free function names is out of
//!   reach by construction — the model calls `section_is_news`, so mutating the guard where
//!   `handle_snapshot` uses it changes nothing here.
//! - **The scripted tests in this file are not the search.** `a_delayed_duplicate_table_...` and
//!   `the_search_catches_the_defects_it_was_built_from` are hand-built interleavings and direct
//!   assertions.

use super::*;
use std::collections::HashSet;

/// What the adversary may do to one frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Fate {
    /// Arrives on the tick it was sent.
    Deliver,
    /// Arrives later, which is reordering when something else passes it.
    Delay,
    /// Never arrives. A reliable frame reaches this through a retransmit the replay window refused.
    Drop,
}

const FATES: [Fate; 3] = [Fate::Deliver, Fate::Delay, Fate::Drop];

/// How long a delivered frame sits in flight. **ONE TICK EACH WAY, SO A ROUND TRIP IS TWO.**
/// Delivering inside the tick that posted made the modelled round trip zero, which put every rule
/// triggered by "longer than a round trip" out of reach — including the retry window itself.
const DELIVER_TICKS: u64 = 1;

/// How long a delayed frame sits in flight. Inside the replay window, so it is reordering.
const DELAY_TICKS: u64 = 4;

/// A frame on the wire, with the tick it lands on.
#[derive(Debug, Clone)]
enum Wire {
    /// A snapshot's trailing interest section, and the frame tick that carried it.
    Section(InterestDeltaSection, u64),
    /// A whole interest set at a generation.
    Table(u64, Vec<u16>),
    /// A client input frame: the generation it echoes, and the snapshot tick it acknowledges.
    Input(u64, u64),
}

/// The client half. The server half is a real [`PeerState`].
#[derive(Debug, Clone, Default)]
struct Mirror {
    held: HashSet<u64>,
    generation: u64,
    seeded: bool,
    echoed: u64,
    want_interest: bool,
    newest_snapshot_tick: u64,
    unacked: bool,
    unbound: UnboundSlots,
    want_manifest: bool,
}

/// One session under one schedule.
///
/// **TWO SLOT TABLES, BECAUSE THAT IS THE ASYMMETRY EVERY INTERESTING CASE LIVES IN.** The server's
/// is the authority; the client's is a copy that arrives on a different channel and can be behind or
/// missing rows outright. One shared table made the client able to name everything the server could,
/// which is the one configuration in which none of this matters.
struct Session {
    server_slots: SlotTable,
    client_slots: SlotTable,
    /// Bindings the client's manifest has not delivered, repaired when it asks for the table.
    missing: Vec<u64>,
    peer: PeerState,
    mirror: Mirror,
    in_flight: Vec<(u64, Wire)>,
    events: Vec<(i32, u64, bool)>,
    tick: u64,
    /// Whole sets put on the wire. Each is a reliable frame carrying the peer's entire interest, so
    /// a schedule that needs many of them is a cost failure even when it converges.
    tables_sent: u32,
    /// Which of the states worth searching this schedule actually reached. See [`Reached`].
    reached: Reached,
}

/// The states a schedule has to reach for the sweep over it to mean anything.
///
/// **A GREEN SWEEP THAT NEVER ENTERED THE INTERESTING STATE READS EXACTLY LIKE ONE THAT DID.** The
/// give-up branch was unreachable in every schedule this file ran, for as long as it has existed,
/// because the model never stamped `interest_delta_tick` — and nothing said so. These counters are
/// what turn that from a silent hole into a failing test.
#[derive(Debug, Default, Clone)]
struct Reached {
    /// A prefix was given up on unacknowledged, which owes a whole set.
    give_up: u32,
    /// A whole set went out.
    table: u32,
    /// A section was refused because the receiver held a different baseline.
    refused_section: u32,
    /// The gate was shut when the server came to send.
    gate_shut: u32,
}

impl Session {
    fn new(ids: &[u64], unnameable: &[u64]) -> Self {
        let mut server_slots = SlotTable::default();
        let mut client_slots = SlotTable::default();
        for (index, &id) in ids.iter().enumerate() {
            server_slots.bind(index as u16, id);
            // An id the client's manifest has not bound yet: a section can name it and the client
            // cannot place it, which is the case only a client can see.
            if !unnameable.contains(&id) {
                client_slots.bind(index as u16, id);
            }
        }
        let peer = PeerState {
            synced: true,
            ..Default::default()
        };
        Self {
            server_slots,
            client_slots,
            missing: unnameable.to_vec(),
            peer,
            mirror: Mirror::default(),
            in_flight: Vec::new(),
            events: Vec::new(),
            tick: 1,
            tables_sent: 0,
            reached: Reached::default(),
        }
    }

    /// The manifest lands, binding everything the client was missing.
    ///
    /// What `adopt_manifest_full` does, minus the rows this search does not model. Reached only by
    /// the client asking, which is what the ask exists to make happen.
    fn deliver_manifest(&mut self) {
        for id in std::mem::take(&mut self.missing) {
            if let Some(slot) = self.server_slots.slot_of(id) {
                self.client_slots.bind(slot, id);
            }
        }
        self.mirror.unbound.cleared();
        self.mirror.want_manifest = false;
    }

    /// Move the world: run the REAL interest pass over `present`, and queue what it reports.
    ///
    /// Through `update_linear_into` rather than by writing the set directly, so the transitions the
    /// protocol carries are the ones the filter actually produces.
    fn set_world(&mut self, present: &[u64]) {
        let (mut scratch, mut delta) = (SeatScratch::default(), InterestDelta::default());
        let candidates: Vec<InterestCandidate> = present
            .iter()
            .map(|&id| InterestCandidate::anchored(id, [1.0, 0.0, 0.0]))
            .collect();
        self.peer.interest.update_linear_into(
            &AoiConfig::default(),
            &[SeatObserver {
                center: [0.0; 3],
                membership: MEMBERSHIP_GLOBAL,
            }],
            &candidates,
            &mut scratch,
            &mut delta,
        );
        for &id in &delta.leaves {
            self.peer.note_interest_leave(id);
        }
        for &id in &delta.enters {
            self.peer.note_interest_enter(id);
        }
    }

    /// The server's flush for this tick, through the real send rules.
    fn server_flush(&mut self, down: Fate) {
        if interest_table_due(&self.peer, self.tick) {
            let (generation, stated) = interest_table_to_send(&self.server_slots, &mut self.peer);
            self.peer.interest_table_tick = Some(self.tick);
            self.tables_sent += 1;
            self.reached.table += 1;
            self.post(down, Wire::Table(generation, stated));
            return;
        }
        let mut left = Vec::new();
        let mut entered = Vec::new();
        // `build_interest_section` calls `retire_interest_delta`, whose give-up branch is the only
        // thing inside it that owes a whole set. Watching the flag across the call is how this
        // schedule knows it reached that state at all.
        let owed_before = self.peer.interest_full_due;
        let carries = build_interest_section(
            &self.server_slots,
            &mut self.peer,
            true,
            self.tick,
            &mut left,
            &mut entered,
        );
        if !owed_before && self.peer.interest_full_due {
            self.reached.give_up += 1;
        }
        let gate_shut = self.peer.interest_generation_acked != self.peer.interest_generation;
        if gate_shut {
            self.reached.gate_shut += 1;
        }
        if snapshot_frame_is_skipped(false, carries, gate_shut) {
            return;
        }
        if carries {
            let section = InterestDeltaSection {
                generation: self.peer.interest_generation,
                left: left.clone(),
                entered: entered.clone(),
            };
            // **THE STAMP, WHICH THE SEND PATH SETS AFTER HANDING THE FRAME TO THE TRANSPORT.**
            // Without it `retire_interest_delta` returns at its first line, so the give-up branch —
            // and the whole set it owes — were unreachable in every schedule this file ran.
            if self.peer.interest_delta_tick.is_none() {
                self.peer.interest_delta_tick = Some(self.tick);
            }
            self.post(down, Wire::Section(section, self.tick));
        } else {
            // A bare header still moves the client's newest-snapshot mark, which is what makes it
            // owe an input frame and so an echo.
            self.post(
                down,
                Wire::Section(InterestDeltaSection::default(), self.tick),
            );
        }
    }

    /// The client's flush, through the real owed rule.
    fn client_flush(&mut self, up: Fate) {
        let owes_echo = self.mirror.generation != self.mirror.echoed;
        if !input_frame_is_owed(
            false,
            self.mirror.unacked,
            owes_echo,
            false,
            false,
            self.mirror.want_interest,
        ) {
            return;
        }
        self.mirror.unacked = false;
        self.mirror.echoed = self.mirror.generation;
        let ack = self.mirror.newest_snapshot_tick;
        self.post(up, Wire::Input(self.mirror.generation, ack));
    }

    fn post(&mut self, fate: Fate, frame: Wire) {
        match fate {
            Fate::Drop => {}
            Fate::Deliver => self.in_flight.push((self.tick + DELIVER_TICKS, frame)),
            Fate::Delay => self.in_flight.push((self.tick + DELAY_TICKS, frame)),
        }
    }

    /// Deliver everything due this tick, in the order it was posted.
    fn deliver(&mut self) {
        let tick = self.tick;
        let mut due: Vec<Wire> = Vec::new();
        self.in_flight.retain(|(at, frame)| {
            if *at <= tick {
                due.push(frame.clone());
                false
            } else {
                true
            }
        });
        for frame in due {
            self.receive(frame);
        }
    }

    fn receive(&mut self, frame: Wire) {
        match frame {
            Wire::Table(generation, stated) => {
                // The admit rule from `handle_frame`: strictly newer, because a retry re-sends the
                // set in flight verbatim.
                if table_is_news(generation, self.mirror.generation) {
                    let seeding = !self.mirror.seeded;
                    let resolved = adopt_whole_set(
                        &self.client_slots,
                        &mut self.mirror.held,
                        &mut self.events,
                        1,
                        &stated,
                    );
                    self.mirror.generation = generation;
                    self.mirror.seeded = true;
                    self.mirror.want_interest = !resolved;
                    let _ = seeding;
                }
            }
            Wire::Section(section, frame_tick) => {
                let newest = section_is_news(frame_tick, self.mirror.newest_snapshot_tick);
                self.mirror.newest_snapshot_tick = self.mirror.newest_snapshot_tick.max(frame_tick);
                self.mirror.unacked = true;
                if section.left.is_empty() && section.entered.is_empty() {
                    return; // a bare header
                }
                if !newest {
                    return; // an older frame's section is an echo, not news
                }
                if !section.applies_to(self.mirror.generation) {
                    self.mirror.want_interest = true;
                    self.reached.refused_section += 1;
                    return;
                }
                self.mirror.seeded = true;
                let resolved = apply_interest_section(
                    &self.client_slots,
                    &mut self.mirror.held,
                    &mut self.events,
                    1,
                    &section,
                );
                if !resolved {
                    self.mirror.want_interest = true;
                }
            }
            Wire::Input(echoed, ack) => {
                self.peer.note_interest_echo(echoed);
                if ack > 0 {
                    self.peer.note_ack(ack, self.tick, 16.0);
                }
                // **NO `retire_interest_delta` HERE.** `build_interest_section` calls it on the SEND
                // path, which is what puts the give-up -- and the whole set it owes -- in the phase
                // BEFORE an input frame can arrive and settle a demand it never covered. Calling it
                // here as well moved the raise after the echo and hid that ordering entirely.
                if self.mirror.want_interest {
                    self.peer.interest_full_due = true;
                }
            }
        }
    }

    /// The state blocks this tick carries, and the manifest ask they drive on the client.
    ///
    /// The server sends a block per entity in the peer's interest, addressed by slot. A client that
    /// cannot name one runs the real [`manifest_ask_for_frame`] clock, and asking is what brings the
    /// binding. This is the repair path the interest lane does NOT have on its own.
    fn state_blocks(&mut self) {
        let carried: Vec<u64> = self.peer.interest.iter().collect();
        if carried.is_empty() {
            return;
        }
        // Per slot, through the shipping [`UnboundSlots`]. The server addresses a block by the slot
        // ITS table names; a client that cannot resolve that slot is the case the ask exists for.
        for id in carried {
            match self.server_slots.slot_of(id) {
                None => continue,
                Some(slot) if self.client_slots.id_of(slot).is_some() => {
                    self.mirror.unbound.named(slot);
                }
                Some(slot) => {
                    self.mirror.want_manifest |= self.mirror.unbound.unnamed(slot, self.tick);
                }
            }
        }
        if self.mirror.want_manifest {
            // The server answers the next tick; the ask is latched until it does.
            self.deliver_manifest();
        }
    }

    /// A rekey on a live connection: everything this connection held is gone, and a whole set is
    /// what re-seats it. The only cause in reach of a search this small -- the pending cap needs far
    /// more entities to overflow -- and without it the generation never leaves 0, which makes the
    /// echo's monotonicity and the table admit rule unreachable.
    fn rekey(&mut self) {
        self.peer.interest_seeded = false;
        self.peer.owe_whole_interest_set();
    }

    fn step(&mut self, down: Fate, up: Fate) {
        // Arrivals first: what is due this tick was posted on an earlier one, because nothing crosses
        // the wire instantly.
        self.deliver();
        self.server_flush(down);
        self.state_blocks();
        self.client_flush(up);
        self.tick += 1;
    }

    /// Run a perfect link until both ends stop changing, and answer whether they agree.
    fn quiesce(&mut self, ticks: u64) -> bool {
        for _ in 0..ticks {
            self.step(Fate::Deliver, Fate::Deliver);
        }
        let truth: HashSet<u64> = self.peer.interest.iter().collect();
        self.mirror.held == truth && self.events_agree_with_the_mirror()
    }

    /// **THE SIGNALS A GAME ACTS ON MUST REBUILD THE SET THE READ-BACK ANSWERS FROM.**
    ///
    /// Comparing the two sets alone made every rule whose only output is a SIGNAL invisible, and most
    /// of this lane's rules are exactly that: an enter with no matching leave, a leave for something
    /// never entered, a departure announced for an entity that is still in the set. A handler
    /// following the documented pattern — hide on leave, show on enter — holds whatever this
    /// reconstruction holds, so if it disagrees with the mirror the game is wrong even though the
    /// read-back is right.
    fn events_agree_with_the_mirror(&self) -> bool {
        let mut rebuilt: HashSet<u64> = HashSet::new();
        for &(_, id, entered) in &self.events {
            if entered {
                rebuilt.insert(id);
            } else {
                rebuilt.remove(&id);
            }
        }
        rebuilt == self.mirror.held
    }

    fn diverged(&self) -> String {
        if !self.events_agree_with_the_mirror() {
            let mut rebuilt: Vec<u64> = {
                let mut set: HashSet<u64> = HashSet::new();
                for &(_, id, entered) in &self.events {
                    if entered {
                        set.insert(id);
                    } else {
                        set.remove(&id);
                    }
                }
                set.into_iter().collect()
            };
            rebuilt.sort_unstable();
            let mut held: Vec<u64> = self.mirror.held.iter().copied().collect();
            held.sort_unstable();
            return format!(
                "the events rebuild {rebuilt:?} but the mirror holds {held:?} -- a game following \
                 the signals disagrees with the read-back"
            );
        }
        let truth: Vec<u64> = {
            let mut v: Vec<u64> = self.peer.interest.iter().collect();
            v.sort_unstable();
            v
        };
        let mut held: Vec<u64> = self.mirror.held.iter().copied().collect();
        held.sort_unstable();
        format!(
            "mirror {held:?} != server {truth:?} (gen {} acked {} mirror_gen {} echoed {} \
             want {} full_due {} inflight {:?})",
            self.peer.interest_generation,
            self.peer.interest_generation_acked,
            self.mirror.generation,
            self.mirror.echoed,
            self.mirror.want_interest,
            self.peer.interest_full_due,
            self.peer.interest_table_inflight.as_ref().map(|(g, _)| *g),
        )
    }
}

/// Deterministic, so a failure is reproducible from its seed alone.
fn lcg(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    *state >> 33
}

/// Drive one schedule, then quiesce and demand agreement.
fn run_schedule(
    ids: &[u64],
    unnameable: &[u64],
    schedule: &[(Fate, Fate)],
    churn: &[u64],
    rekey_at: Option<usize>,
) -> Result<Reached, String> {
    let mut session = Session::new(ids, unnameable);
    for (index, &(down, up)) in schedule.iter().enumerate() {
        if rekey_at == Some(index) {
            session.rekey();
        }
        // The world moves under the protocol: each tick a different subset is present, so entities
        // enter and leave while frames are in flight.
        // The LAST tick puts everything back in the world, so the unnameable entity is in the set
        // the assertion compares against. Filtered out on the final step, that configuration tested
        // nothing it claimed to.
        let present: Vec<u64> = if index + 1 == schedule.len() {
            churn.to_vec()
        } else {
            churn
                .iter()
                .copied()
                .filter(|id| !(index as u64 + id).is_multiple_of(3))
                .collect()
        };
        session.set_world(&present);
        session.step(down, up);
    }
    if !session.quiesce(3 * INTEREST_DELTA_RETRY_TICKS + 32) {
        return Err(session.diverged());
    }
    // **A WHOLE SET PER ROUND TRIP IS A FAILURE THAT CONVERGES**, so the budget is what sees it.
    // Clearing the retry stamp on the settle produced exactly that: one reliable frame carrying the
    // peer's entire interest, every two ticks, for the life of the connection.
    //
    // A GROSS-RATE CHECK, NOT A TIGHT BOUND. A schedule with a sustained outage legitimately owes
    // several sets — a rekey, then a give-up per retry window it spans — and seed 360 sends five in
    // 146 ticks honestly. Six leaves that room while still catching a per-round-trip failure by more
    // than ten times over.
    let budget = 6;
    if session.tables_sent > budget {
        return Err(format!(
            "{} whole sets sent for a schedule of {} ticks -- the retry rate limit is not holding",
            session.tables_sent,
            schedule.len()
        ));
    }
    Ok(session.reached)
}

/// Fold one schedule's coverage into a running total.
fn fold(total: &mut Reached, one: &Reached) {
    total.give_up += one.give_up;
    total.table += one.table;
    total.refused_section += one.refused_section;
    total.gate_shut += one.gate_shut;
}

/// **EVERY SCHEDULE OF FOUR TICKS, OVER A UNIVERSE WITH A SLOT THE CLIENT CANNOT NAME.**
///
/// Exhaustive rather than sampled at this depth: the defects this exists to catch were all reachable
/// in a handful of ticks, and none of them needed a rare coincidence — only a particular order.
#[test]
fn every_short_schedule_converges_once_the_link_is_clean() {
    let ids = [10u64, 11, 12];
    for unnameable in [&[][..], &[12u64][..]] {
        let mut schedule = [(Fate::Deliver, Fate::Deliver); 4];
        let mut checked = 0usize;
        let mut covered = Reached::default();
        for a in FATES {
            for b in FATES {
                for c in FATES {
                    for d in FATES {
                        for e in FATES {
                            for f in FATES {
                                schedule[0] = (a, b);
                                schedule[1] = (c, d);
                                schedule[2] = (e, f);
                                schedule[3] = (Fate::Deliver, Fate::Deliver);
                                checked += 1;
                                match run_schedule(
                                    &ids,
                                    unnameable,
                                    &schedule,
                                    &[10, 11, 12],
                                    Some(1),
                                ) {
                                    Ok(reached) => fold(&mut covered, &reached),
                                    Err(why) => panic!(
                                        "schedule {schedule:?} (unnameable {unnameable:?}) \
                                         left the two ends disagreeing: {why}"
                                    ),
                                }
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(
            checked, 729,
            "every combination of three tick fates, both directions"
        );
        // **A GREEN SWEEP THAT NEVER ENTERED THE STATE READS LIKE ONE THAT DID.** Four ticks is too
        // short for the retry window, so a give-up is out of reach here by construction and belongs
        // to the long sweep; what these schedules must reach is a whole set and a shut gate.
        assert!(covered.table > 0, "no schedule here ever sent a whole set");
        assert!(
            covered.gate_shut > 0,
            "no schedule here ever found the gate shut"
        );
    }
}

/// **DEEPER SCHEDULES, SAMPLED DETERMINISTICALLY.** The exhaustive pass above cannot reach a prefix
/// given up on unacknowledged, which needs [`INTEREST_DELTA_RETRY_TICKS`] to elapse. These run long
/// enough for the retry, the give-up and the whole set that follows it.
#[test]
fn long_lossy_schedules_converge_once_the_link_is_clean() {
    let ids = [10u64, 11, 12, 13];
    let mut failures: Vec<String> = Vec::new();
    let mut covered = Reached::default();
    for seed in 0..400u64 {
        let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
        let length = 8 + (lcg(&mut state) % (3 * INTEREST_DELTA_RETRY_TICKS)) as usize;

        // **A SUSTAINED OUTAGE, WHICH INDEPENDENT PER-TICK LOSS CANNOT PRODUCE.** Giving up on an
        // unacknowledged prefix needs the ack to be missing for a whole retry window; drawing each
        // tick independently at one third makes that (1/3)^64, so the branch was unreachable and the
        // sweep was green because it never looked. Every third seed blacks out one direction for
        // long enough, which is what a link actually does.
        let outage = if seed.is_multiple_of(3) && length > INTEREST_DELTA_RETRY_TICKS as usize + 4 {
            let start = (lcg(&mut state) % 4) as usize + 1;
            let span = INTEREST_DELTA_RETRY_TICKS as usize + 2;
            let downstream = lcg(&mut state).is_multiple_of(2);
            Some((start, start + span.min(length - start), downstream))
        } else {
            None
        };

        let schedule: Vec<(Fate, Fate)> = (0..length)
            .map(|tick| {
                let (mut down, mut up) = (
                    FATES[(lcg(&mut state) % 3) as usize],
                    FATES[(lcg(&mut state) % 3) as usize],
                );
                if let Some((from, to, downstream)) = outage {
                    if (from..to).contains(&tick) {
                        if downstream {
                            down = Fate::Drop;
                        } else {
                            up = Fate::Drop;
                        }
                    }
                }
                (down, up)
            })
            .collect();
        let unnameable: &[u64] = if seed % 3 == 0 { &[13] } else { &[] };
        // A rekey somewhere in the run, so a whole set is actually minted and the generation moves
        // off 0 -- without it the echo's monotonicity and the table admit rule are unreachable.
        let rekey_at = Some((lcg(&mut state) % length as u64) as usize);
        match run_schedule(&ids, unnameable, &schedule, &[10, 11, 12, 13], rekey_at) {
            Ok(reached) => fold(&mut covered, &reached),
            Err(why) => failures.push(format!("seed {seed}: {why}")),
        }
    }
    assert!(
        failures.is_empty(),
        "{} of 400 seeds left the two ends disagreeing:\n  {}",
        failures.len(),
        failures.join("\n  ")
    );

    // **WHAT THESE SCHEDULES ACTUALLY REACHED.** The give-up branch was unreachable in all 400 seeds
    // for as long as this file existed — the model never stamped `interest_delta_tick`, so
    // `retire_interest_delta` returned at its first line every time — and the sweep was green
    // throughout. Green meant "never looked". These are the states the sweep is for; if a change
    // puts one out of reach again, this says so instead of passing quietly.
    assert!(
        covered.give_up > 0,
        "no schedule gave up on an unacknowledged prefix, so the cause that owes a whole set was \
         never searched: {covered:?}"
    );
    assert!(
        covered.table > 0,
        "no schedule sent a whole set: {covered:?}"
    );
    assert!(
        covered.gate_shut > 0,
        "no schedule found the gate shut: {covered:?}"
    );
}

/// **A SECOND COPY OF ONE GENERATION MUST NOT REWIND THE MIRROR.**
///
/// A retry re-sends the set in flight byte for byte, so two copies of one generation can be on the
/// wire at once. The second is not a repeat: this peer adopted the first, applied the sections that
/// followed it, and adopting again rewinds the mirror to the mint and undoes every one of them.
///
/// Deterministic rather than sampled, because it needs a particular interleaving the random sweep
/// reaches only by luck: table, then section, then the table's delayed twin.
#[test]
fn a_delayed_duplicate_table_does_not_undo_the_sections_after_it() {
    let mut session = Session::new(&[10, 11], &[]);
    session.set_world(&[10]);
    session.rekey();

    // The first copy of the whole set goes out and lands. Two steps, because nothing crosses the
    // wire inside the tick that posted it.
    session.step(Fate::Deliver, Fate::Deliver);
    session.step(Fate::Deliver, Fate::Deliver);
    let minted = session.mirror.generation;
    assert!(minted > 0, "a rekey mints a whole set");
    assert!(session.mirror.held.contains(&10), "which the client adopts");

    // The world moves, and the section carrying it lands.
    session.set_world(&[10, 11]);
    for _ in 0..6 {
        session.step(Fate::Deliver, Fate::Deliver);
    }
    assert!(
        session.mirror.held.contains(&11),
        "the section after the table is applied"
    );

    // The retry's copy of that same set finally arrives, stating the world as it was at the mint.
    let stated = session
        .server_slots
        .slot_of(10)
        .expect("the mint named entity 10");
    session.receive(Wire::Table(minted, vec![stated]));
    assert!(
        session.mirror.held.contains(&11),
        "a second copy of a generation already held is ignored rather than applied -- \
         adopting it would undo every section since the mint"
    );
}

/// **THE SEARCH IS ONLY WORTH ITS RUNTIME IF IT FAILS ON A REAL DEFECT.** Each case below is one this
/// repository actually shipped, reproduced against the model rather than the code, so the assertions
/// stay true when the code is fixed. A change that makes any of these pass has broken the search.
#[test]
fn the_search_catches_the_defects_it_was_built_from() {
    // Round twelve: an ordinary session settles a demand no table ever answered, because both
    // generations sit at 0 for its whole life.
    let mut peer = PeerState::default();
    peer.owe_whole_interest_set();
    let settled_by_generations = peer.interest_generation_acked == peer.interest_generation;
    assert!(
        settled_by_generations,
        "the defective rule's condition is true here, which is what made it a defect"
    );
    assert!(
        !peer.note_interest_echo(0),
        "and the shipping rule settles nothing, because no set was ever sent"
    );

    // Round thirteen: a retry that mints a fresh generation moves the target the client is echoing.
    let slots = {
        let mut s = SlotTable::default();
        s.bind(0, 10);
        s
    };
    let mut session = Session::new(&[10], &[]);
    session.set_world(&[10]);
    let mut retrying = session.peer;
    retrying.owe_whole_interest_set();
    let (first, _) = interest_table_to_send(&slots, &mut retrying);
    let (again, _) = interest_table_to_send(&slots, &mut retrying);
    assert_eq!(
        first, again,
        "a retry quotes the generation already in flight"
    );

    // Round fourteen: a slot's clock is not reset by what OTHER slots did in frames between its
    // own sightings, which is what made every earlier version starvable.
    let mut unbound = UnboundSlots::default();
    assert!(!unbound.unnamed(9, 0), "the first sighting is ordinary lag");
    unbound.named(3);
    unbound.named(4);
    assert!(
        unbound.unnamed(9, INTEREST_DELTA_RETRY_TICKS),
        "and naming other slots in between says nothing about this one"
    );
}
