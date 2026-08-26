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
//! **WHAT IT CATCHES, MEASURED RATHER THAN CLAIMED.** Seven defects this repository actually shipped
//! were re-injected one at a time. It fails on five:
//!
//! | Defect | |
//! | --- | --- |
//! | the echo settles a demand no table ever answered | caught |
//! | a retry mints a fresh generation, so a slow link never catches up | caught |
//! | the retry stamp is cleared on the settle, so there is no backoff | caught |
//! | a re-sent table is admitted at a generation already held | caught |
//! | the server stays silent while the gate is shut | caught |
//! | a section applies from a frame that is not the newest | **missed** |
//! | a stale echo regresses the baseline the server believes | **missed** |
//!
//! The two it misses share a shape: both converge once the link is quiet, paying a whole set to get
//! there, so an assertion about convergence cannot see them and the cost budget below is too loose to
//! catch them either. Both have their own unit tests. Widening this to catch them means asserting on
//! the events emitted rather than only on the final sets, which is worth doing and is not done here.
//!
//! It is a bug-finder rather than a proof: absence of a counterexample is not a guarantee, and the
//! model covers the RULES rather than their call sites — a defect in code no free function names is
//! out of its reach by construction.

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

/// How long a delayed frame sits in flight. Inside the replay window, so it is reordering.
const DELAY_TICKS: u64 = 3;

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
    unbound_since: Option<u64>,
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
        self.mirror.unbound_since = None;
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
            self.post(down, Wire::Table(generation, stated));
            return;
        }
        let mut left = Vec::new();
        let mut entered = Vec::new();
        let carries = build_interest_section(
            &self.server_slots,
            &mut self.peer,
            true,
            self.tick,
            &mut left,
            &mut entered,
        );
        let gate_shut = self.peer.interest_generation_acked != self.peer.interest_generation;
        if snapshot_frame_is_skipped(false, carries, gate_shut) {
            return;
        }
        if carries {
            let section = InterestDeltaSection {
                generation: self.peer.interest_generation,
                left: left.clone(),
                entered: entered.clone(),
            };
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
            Fate::Deliver => self.in_flight.push((self.tick, frame)),
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
                self.peer.retire_interest_delta(self.tick);
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
        let unbound = carried
            .iter()
            .any(|&id| self.client_slots.slot_of(id).is_none());
        let (ask, since) =
            manifest_ask_for_frame(self.mirror.unbound_since, self.tick, true, unbound);
        self.mirror.unbound_since = since;
        self.mirror.want_manifest |= ask;
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
        self.server_flush(down);
        self.state_blocks();
        self.deliver();
        self.client_flush(up);
        self.deliver();
        self.tick += 1;
    }

    /// Run a perfect link until both ends stop changing, and answer whether they agree.
    fn quiesce(&mut self, ticks: u64) -> bool {
        for _ in 0..ticks {
            self.step(Fate::Deliver, Fate::Deliver);
        }
        let truth: HashSet<u64> = self.peer.interest.iter().collect();
        self.mirror.held == truth
    }

    fn diverged(&self) -> String {
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
) -> Result<(), String> {
    let mut session = Session::new(ids, unnameable);
    for (index, &(down, up)) in schedule.iter().enumerate() {
        if rekey_at == Some(index) {
            session.rekey();
        }
        // The world moves under the protocol: each tick a different subset is present, so entities
        // enter and leave while frames are in flight.
        let present: Vec<u64> = churn
            .iter()
            .copied()
            .filter(|id| !(index as u64 + id).is_multiple_of(3))
            .collect();
        session.set_world(&present);
        session.step(down, up);
    }
    if !session.quiesce(3 * INTEREST_DELTA_RETRY_TICKS + 32) {
        return Err(session.diverged());
    }
    // **A WHOLE SET PER ROUND TRIP IS A FAILURE THAT CONVERGES.** One reliable frame carrying the
    // peer's entire interest, once per retry window, for the life of the connection -- which is what
    // clearing the retry stamp on the settle produced. Convergence cannot see it, so the budget does.
    // Four causes can owe a set across one schedule, and each may legitimately need a retry or two.
    let budget = 8;
    if session.tables_sent > budget {
        return Err(format!(
            "{} whole sets sent for a schedule of {} ticks -- the retry rate limit is not holding",
            session.tables_sent,
            schedule.len()
        ));
    }
    Ok(())
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
                                if let Err(why) = run_schedule(
                                    &ids,
                                    unnameable,
                                    &schedule,
                                    &[10, 11, 12],
                                    Some(1),
                                ) {
                                    panic!(
                                        "schedule {schedule:?} (unnameable {unnameable:?}) \
                                         left the two ends disagreeing: {why}"
                                    );
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
    }
}

/// **DEEPER SCHEDULES, SAMPLED DETERMINISTICALLY.** The exhaustive pass above cannot reach a prefix
/// given up on unacknowledged, which needs [`INTEREST_DELTA_RETRY_TICKS`] to elapse. These run long
/// enough for the retry, the give-up and the whole set that follows it.
#[test]
fn long_lossy_schedules_converge_once_the_link_is_clean() {
    let ids = [10u64, 11, 12, 13];
    let mut failures: Vec<String> = Vec::new();
    for seed in 0..400u64 {
        let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
        let length = 8 + (lcg(&mut state) % (2 * INTEREST_DELTA_RETRY_TICKS + 8)) as usize;
        let schedule: Vec<(Fate, Fate)> = (0..length)
            .map(|_| {
                (
                    FATES[(lcg(&mut state) % 3) as usize],
                    FATES[(lcg(&mut state) % 3) as usize],
                )
            })
            .collect();
        let unnameable: &[u64] = if seed % 3 == 0 { &[13] } else { &[] };
        // A rekey somewhere in the run, so a whole set is actually minted and the generation moves
        // off 0 -- without it the echo's monotonicity and the table admit rule are unreachable.
        let rekey_at = Some((lcg(&mut state) % length as u64) as usize);
        if let Err(why) = run_schedule(&ids, unnameable, &schedule, &[10, 11, 12, 13], rekey_at) {
            failures.push(format!("seed {seed}: {why}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of 400 seeds left the two ends disagreeing:\n  {}",
        failures.len(),
        failures.join("\n  ")
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

    // The first copy of the whole set goes out and lands.
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

    // Round thirteen: the manifest ask cannot fire if a named slot in the same frame clears it.
    let (ask, _) = manifest_ask_for_frame(Some(0), INTEREST_DELTA_RETRY_TICKS, true, true);
    assert!(
        ask,
        "a frame that could not name a slot still runs the clock"
    );
    let (_, cleared) = manifest_ask_for_frame(Some(0), 10, true, false);
    assert_eq!(cleared, None, "and one that named everything clears it");
}
