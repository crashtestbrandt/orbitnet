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
//! **Releasing a seat when a connection ends is a POLICY, and this module holds only the
//! predicate.** [`releases_seats`] answers whether one drop or one expiry frees the seats behind it,
//! given the [`SeatReleasePolicy`] the game chose. Nothing here acts on the answer, and the answer
//! for a game that chose nothing is `false` on every event.
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
/// one couch behind one socket — and each needs its own interest anchor, its own center and its own
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
/// from the bodies it owns the state of; a client projects its from the entity-manifest rows it
/// holds, which a delta patches rather than replaces. Rebuilding from a complete table was
/// self-repairing, and [`crate::codec::ManifestDelta`] states what stands in for that — a reliable
/// and ordered channel, the base generation a delta names, and a full table on every path that can
/// desynchronize a receiver. The diff below is the same code for both, so a seat event means the
/// same thing on either end of the link.
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

/// What a game wants done with a connection's seats once that connection ends.
///
/// **Opt-in, and [`Hold`](SeatReleasePolicy::Hold) is the default because the existing behavior is
/// the default.** A connection whose transport is gone keeps its seats: the bodies behind it hold
/// the authority they were given until the game changes it. That is what the reconnect grace window
/// is for — a player whose wifi drops a burst of packets comes back to the body they left — and a
/// session that freed the body on every transient drop would despawn players for a hiccup. Choosing
/// anything else is the game saying its own rules differ.
///
/// What a policy buys the game that does want a release is **one call instead of a second table**.
/// The alternative is a peer-to-bodies map maintained beside the roster the backend already derives
/// from ownership, and two tables that answer "which bodies does this connection drive" are two
/// things that can disagree — while only one of them, ownership, is what the anti-forgery check on
/// received input reads.
///
/// The policy decides nothing on its own. [`releases_seats`] is the whole rule, and the caller acts
/// on the answer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum SeatReleasePolicy {
    /// Never release. The bodies keep the peer they were given, through the drop and past the
    /// expiry, until the game changes it itself. The default, and the behavior of every session
    /// that sets no policy.
    #[default]
    Hold,
    /// Release when a held session's grace window closes with nobody having claimed it back. A drop
    /// on its own changes nothing, so a player who reconnects inside the window finds the body where
    /// they left it.
    OnExpiry,
    /// Release the moment the transport connection is gone, without waiting for the window. For a
    /// game with no reconnect story: the seat opens to the next joiner immediately, and a player who
    /// comes back is a new player.
    OnDrop,
}

/// Which connection-ending event is being reported.
///
/// The two are **sequential, not alternative**: a held session drops first and expires later, so a
/// single connection going away for good produces one of each. That is why a policy has to say which
/// of the two it acts on, and why acting on both would release twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SeatReleaseEvent {
    /// The transport connection went away. Reported for every drop, whether or not the session
    /// behind it is being held open.
    Dropped,
    /// A held session's grace window closed with nobody claiming it.
    Expired,
}

/// Whether `event` releases the seats of the connection it names, under `policy`.
///
/// | `peer_is_live` | policy | on `Dropped` | on `Expired` |
/// | --- | --- | --- | --- |
/// | `true` | any | no | no |
/// | `false` | [`Hold`](SeatReleasePolicy::Hold) | no | no |
/// | `false` | [`OnDrop`](SeatReleasePolicy::OnDrop) | **yes** | no |
/// | `false` | [`OnExpiry`](SeatReleasePolicy::OnExpiry) | no | **yes** |
///
/// [`OnDrop`](SeatReleasePolicy::OnDrop) answers `false` on `Expired` because **the release already
/// happened at the drop**. The expiry that follows names seats this policy let go a grace window
/// ago, so a second release there can only reach seats something else has been given since.
///
/// **`peer_is_live == true` releases nothing, whatever the policy says.** This is the guard the rest
/// of the rule rests on, and the reason is that **transport peer ids are reused**:
///
/// - An expiry names the id the session was **last connected under**, and that connection ended up
///   to a whole grace window ago — 30 seconds by default.
/// - Nothing reserves an id for a peer that is gone. The transport hands the next arrival whatever
///   id is free, and 30 seconds is long enough for a newcomer to be holding the one the expiry
///   names.
/// - Releasing on that id would take a **live player's** body away, and nothing would report a
///   problem: the id is valid, the seats behind it are real, and the release does exactly what it
///   was asked to do. The player watches their body stop answering.
///
/// So the caller passes whether that id currently belongs to a connected peer, and the guard is
/// checked before the policy is consulted at all — one rule rather than one per path. The cost is a
/// **missed** release when an id was recycled: that body keeps an owner who no longer plays until
/// the game changes it, which is exactly [`Hold`](SeatReleasePolicy::Hold) and is the direction that
/// is safe to be wrong in.
#[must_use]
pub fn releases_seats(
    policy: SeatReleasePolicy,
    event: SeatReleaseEvent,
    peer_is_live: bool,
) -> bool {
    if peer_is_live {
        return false;
    }
    match policy {
        SeatReleasePolicy::Hold => false,
        SeatReleasePolicy::OnDrop => event == SeatReleaseEvent::Dropped,
        SeatReleasePolicy::OnExpiry => event == SeatReleaseEvent::Expired,
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
    fn the_default_seat_release_policy_holds_the_seats() {
        // The behavior a session gets by setting nothing, pinned so the opt-in stays opt-in.
        assert_eq!(SeatReleasePolicy::default(), SeatReleasePolicy::Hold);
    }

    #[test]
    fn holding_releases_on_neither_the_drop_nor_the_expiry() {
        use SeatReleaseEvent::{Dropped, Expired};
        assert!(!releases_seats(SeatReleasePolicy::Hold, Dropped, false));
        assert!(!releases_seats(SeatReleasePolicy::Hold, Expired, false));
    }

    #[test]
    fn releasing_on_drop_frees_at_the_drop_and_not_again_at_the_expiry() {
        // The expiry names seats this policy let go a grace window earlier.
        use SeatReleaseEvent::{Dropped, Expired};
        assert!(releases_seats(SeatReleasePolicy::OnDrop, Dropped, false));
        assert!(!releases_seats(SeatReleasePolicy::OnDrop, Expired, false));
    }

    #[test]
    fn releasing_on_expiry_waits_for_the_window_and_frees_nothing_at_the_drop() {
        use SeatReleaseEvent::{Dropped, Expired};
        assert!(!releases_seats(SeatReleasePolicy::OnExpiry, Dropped, false));
        assert!(releases_seats(SeatReleasePolicy::OnExpiry, Expired, false));
    }

    #[test]
    fn a_live_peer_id_releases_nothing_under_any_policy_because_ids_are_reused() {
        // The guard, as its own rule: an expiry names an id that dropped up to a grace window ago,
        // and a newcomer can already be holding it. Releasing then takes a live player's body.
        use SeatReleaseEvent::{Dropped, Expired};
        assert!(!releases_seats(SeatReleasePolicy::Hold, Dropped, true));
        assert!(!releases_seats(SeatReleasePolicy::Hold, Expired, true));
        assert!(!releases_seats(SeatReleasePolicy::OnDrop, Dropped, true));
        assert!(!releases_seats(SeatReleasePolicy::OnDrop, Expired, true));
        assert!(!releases_seats(SeatReleasePolicy::OnExpiry, Dropped, true));
        assert!(!releases_seats(SeatReleasePolicy::OnExpiry, Expired, true));
    }

    #[test]
    fn the_live_peer_guard_and_not_a_blanket_no_is_what_silences_those_rows() {
        // Negative control for the test above, deliberately repeating its two releasing rows with
        // the id no longer live: without them, a `releases_seats` that answered `false` everywhere
        // would satisfy the guard test.
        assert!(releases_seats(
            SeatReleasePolicy::OnDrop,
            SeatReleaseEvent::Dropped,
            false
        ));
        assert!(releases_seats(
            SeatReleasePolicy::OnExpiry,
            SeatReleaseEvent::Expired,
            false
        ));
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
