//! Seats: which owned viewpoints a connection currently holds, and what changed since last tick.
//!
//! A **seat** is one owned, predicted viewpoint behind one transport connection. Local split-screen
//! is two or more of them on a single socket, so ownership is per `(peer, seat)` rather than per
//! peer, and the interest pass keys an anchor on the pair.
//!
//! **The roster is derived, not declared.** A seat exists because some replicated body says it is
//! driven by that connection and carries that label. Nothing here invents a seat and nothing stores
//! one independently of the bodies: a second source of truth for "which seats exist" is a second
//! thing that can disagree with ownership, and ownership is what the anti-forgery check on received
//! input reads.
//!
//! **What this module adds is the DIFF.** Rebuilding the set every tick answers "which seats are
//! there"; it does not answer "which arrived" or "which went away", and those are the two events a
//! presentation layer binds to — a split-screen viewport to open, a camera to release. [`SeatRoster`]
//! holds the last announced set and reports the transitions against a freshly gathered one.
//!
//! Everything here is plain data: no Godot, no scene tree, no allocation past the caller's own
//! buffers.

/// Which seat on a connection a body belongs to, as the game declared it.
///
/// A `u16` because it is a **label** rather than a count: the interest pass holds one set per
/// distinct label present on a connection, so the numbers need not be small or contiguous, and
/// nothing is sized by the value.
pub type SeatIndex = u16;

/// One owned viewpoint: a connection, and which of its seats.
///
/// **The identity ownership could not express before.** "Which connection" is the whole answer only
/// while a connection drives one predicted body. Local split-screen drives several — two players on
/// one couch behind one socket — and each needs its own interest anchor, its own centre and its own
/// world, because the second player's surroundings are not the first player's.
///
/// **Seat is the word the demos already use for a player side**, and this is the same idea: a seat
/// is a player position, and what changes is only that a connection may hold more than one of them.
/// A game whose bodies all leave `seat` at `0` has one seat per connection, which is the bijection
/// the demos assume and is unchanged by any of this.
///
/// Ordered **peer-major**, so a sort groups a connection's seats together and orders them by
/// ascending label within that group — which is what makes both `seats_of` and the per-connection
/// lookups in the send path a `partition_point` rather than a per-tick map.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct SeatId {
    /// The connection this seat sits on.
    pub peer: i32,
    /// Which seat on it. `0` for every body that declares nothing.
    pub seat: SeatIndex,
}

impl SeatId {
    /// A seat on `peer` carrying label `seat`.
    #[must_use]
    pub fn new(peer: i32, seat: SeatIndex) -> Self {
        Self { peer, seat }
    }
}

/// The seats a session currently holds, and the transitions since the last announcement.
///
/// Kept sorted and deduplicated so a connection's seats are one contiguous run and the diff is a
/// merge walk of two ordered slices rather than a set difference over a hash map.
///
/// **Both sides hold one and they hold it for different reasons.** The server derives its roster
/// from the bodies it owns the state of; a client rebuilds its from each entity manifest, which is a
/// complete table and therefore self-repairing. The diff below is the same code for both, so a seat
/// event means the same thing on either end of the link.
#[derive(Clone, Debug, Default)]
pub struct SeatRoster {
    /// Sorted ascending, no duplicates.
    seats: Vec<SeatId>,
}

impl SeatRoster {
    /// An empty roster — a session that has announced nothing yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every seat currently held, ascending and peer-major.
    #[must_use]
    pub fn seats(&self) -> &[SeatId] {
        &self.seats
    }

    /// How many seats are held across the whole session.
    #[must_use]
    pub fn len(&self) -> usize {
        self.seats.len()
    }

    /// Whether the session holds no seat at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.seats.is_empty()
    }

    /// Whether this exact seat is held.
    #[must_use]
    pub fn contains(&self, id: SeatId) -> bool {
        self.seats.binary_search(&id).is_ok()
    }

    /// The run of seats belonging to one connection, ascending by label.
    ///
    /// A `partition_point` pair rather than a filter, which is what the peer-major ordering buys.
    #[must_use]
    pub fn seats_of(&self, peer: i32) -> &[SeatId] {
        let start = self.seats.partition_point(|id| id.peer < peer);
        let end = self.seats.partition_point(|id| id.peer <= peer);
        &self.seats[start..end]
    }

    /// Drop every seat this roster holds, announcing nothing.
    ///
    /// Session teardown only. It is deliberately **not** the same as replacing with an empty set:
    /// that reports every seat as closing, which is what a game wants when a session drains while it
    /// is running, and this is what it wants when the session itself is going away.
    pub fn clear(&mut self) {
        self.seats.clear();
    }

    /// Adopt `next` as the roster, reporting what arrived and what left.
    ///
    /// `next` is sorted and deduplicated **in place**, so the caller may gather it in whatever order
    /// its registry walk produces. `opened` and `closed` are cleared first and refilled ascending;
    /// both are the caller's buffers so a steady session allocates nothing.
    ///
    /// **The two lists are disjoint**, by construction: a seat is in exactly one of the three states
    /// this compares (gone, arrived, unchanged), so no seat can both open and close in one
    /// announcement. The caller may report them in whichever order it likes.
    pub fn replace_into(
        &mut self,
        next: &mut Vec<SeatId>,
        opened: &mut Vec<SeatId>,
        closed: &mut Vec<SeatId>,
    ) {
        next.sort_unstable();
        next.dedup();
        opened.clear();
        closed.clear();

        let (mut old, mut new) = (0usize, 0usize);
        while old < self.seats.len() && new < next.len() {
            match self.seats[old].cmp(&next[new]) {
                std::cmp::Ordering::Less => {
                    closed.push(self.seats[old]);
                    old += 1;
                }
                std::cmp::Ordering::Greater => {
                    opened.push(next[new]);
                    new += 1;
                }
                std::cmp::Ordering::Equal => {
                    old += 1;
                    new += 1;
                }
            }
        }
        closed.extend_from_slice(&self.seats[old..]);
        opened.extend_from_slice(&next[new..]);

        std::mem::swap(&mut self.seats, next);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seat(peer: i32, seat: SeatIndex) -> SeatId {
        SeatId::new(peer, seat)
    }

    /// Round-trips one announcement and answers `(opened, closed)`.
    fn announce(roster: &mut SeatRoster, next: &[SeatId]) -> (Vec<SeatId>, Vec<SeatId>) {
        let mut next = next.to_vec();
        let (mut opened, mut closed) = (Vec::new(), Vec::new());
        roster.replace_into(&mut next, &mut opened, &mut closed);
        (opened, closed)
    }

    #[test]
    fn a_first_announcement_opens_everything_and_closes_nothing() {
        let mut roster = SeatRoster::new();
        let (opened, closed) = announce(&mut roster, &[seat(7, 1), seat(7, 0)]);
        assert_eq!(opened, vec![seat(7, 0), seat(7, 1)]);
        assert!(closed.is_empty());
        assert_eq!(roster.len(), 2);
    }

    #[test]
    fn an_unchanged_set_announces_nothing() {
        let mut roster = SeatRoster::new();
        announce(&mut roster, &[seat(2, 0), seat(3, 0)]);
        let (opened, closed) = announce(&mut roster, &[seat(3, 0), seat(2, 0)]);
        assert!(opened.is_empty(), "gather order is not a change");
        assert!(closed.is_empty());
    }

    #[test]
    fn a_seat_arriving_beside_an_existing_one_opens_only_itself() {
        let mut roster = SeatRoster::new();
        announce(&mut roster, &[seat(4, 0)]);
        let (opened, closed) = announce(&mut roster, &[seat(4, 0), seat(4, 1)]);
        assert_eq!(
            opened,
            vec![seat(4, 1)],
            "the held seat is not re-announced"
        );
        assert!(closed.is_empty());
    }

    #[test]
    fn a_seat_leaving_closes_only_itself() {
        let mut roster = SeatRoster::new();
        announce(&mut roster, &[seat(4, 0), seat(4, 1), seat(9, 0)]);
        let (opened, closed) = announce(&mut roster, &[seat(4, 1), seat(9, 0)]);
        assert!(opened.is_empty());
        assert_eq!(closed, vec![seat(4, 0)]);
    }

    #[test]
    fn a_seat_moving_between_connections_is_one_open_and_one_close() {
        // The same label on a different peer is a DIFFERENT seat: the viewpoint is the pair.
        let mut roster = SeatRoster::new();
        announce(&mut roster, &[seat(4, 0)]);
        let (opened, closed) = announce(&mut roster, &[seat(5, 0)]);
        assert_eq!(opened, vec![seat(5, 0)]);
        assert_eq!(closed, vec![seat(4, 0)]);
    }

    #[test]
    fn duplicates_in_the_gathered_set_are_one_seat() {
        // Several bodies on one seat is the ordinary case, and it is one viewpoint.
        let mut roster = SeatRoster::new();
        let (opened, _) = announce(&mut roster, &[seat(1, 2), seat(1, 2), seat(1, 2)]);
        assert_eq!(opened, vec![seat(1, 2)]);
        assert_eq!(roster.len(), 1);
    }

    #[test]
    fn draining_the_session_closes_every_seat() {
        let mut roster = SeatRoster::new();
        announce(&mut roster, &[seat(1, 0), seat(2, 0)]);
        let (opened, closed) = announce(&mut roster, &[]);
        assert!(opened.is_empty());
        assert_eq!(closed, vec![seat(1, 0), seat(2, 0)]);
        assert!(roster.is_empty());
    }

    #[test]
    fn clearing_announces_nothing() {
        let mut roster = SeatRoster::new();
        announce(&mut roster, &[seat(1, 0)]);
        roster.clear();
        assert!(roster.is_empty(), "teardown drops the set outright");
    }

    #[test]
    fn seats_of_answers_one_connections_run_in_label_order() {
        let mut roster = SeatRoster::new();
        announce(
            &mut roster,
            &[seat(9, 3), seat(2, 1), seat(9, 0), seat(2, 0), seat(11, 0)],
        );
        assert_eq!(roster.seats_of(2), &[seat(2, 0), seat(2, 1)]);
        assert_eq!(roster.seats_of(9), &[seat(9, 0), seat(9, 3)]);
        assert!(
            roster.seats_of(3).is_empty(),
            "a peer with no body holds no seat"
        );
    }

    #[test]
    fn contains_keys_on_the_pair() {
        let mut roster = SeatRoster::new();
        announce(&mut roster, &[seat(4, 1)]);
        assert!(roster.contains(seat(4, 1)));
        assert!(!roster.contains(seat(4, 0)), "the label is part of the key");
        assert!(!roster.contains(seat(5, 1)), "so is the connection");
    }

    #[test]
    fn the_ordering_is_peer_major() {
        // What makes `seats_of` a partition_point pair. Asserted rather than assumed, because the
        // derive follows field declaration order and reordering the struct would silently break it.
        let mut ids = vec![seat(2, 0), seat(1, 9), seat(1, 0)];
        ids.sort_unstable();
        assert_eq!(ids, vec![seat(1, 0), seat(1, 9), seat(2, 0)]);
    }

    #[test]
    fn the_caller_buffers_are_reused_rather_than_appended_to() {
        let mut roster = SeatRoster::new();
        let mut opened = vec![seat(99, 99)];
        let mut closed = vec![seat(98, 98)];
        let mut next = vec![seat(1, 0)];
        roster.replace_into(&mut next, &mut opened, &mut closed);
        assert_eq!(
            opened,
            vec![seat(1, 0)],
            "stale entries are cleared, not kept"
        );
        assert!(closed.is_empty());
    }
}
