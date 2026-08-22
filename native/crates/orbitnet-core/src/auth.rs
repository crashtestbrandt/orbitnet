//! What the receive path refuses: datagram authenticity, replay, and per-peer volume.
//!
//! Before this module the transport's sender id was the whole of a datagram's identity. Anything that
//! could put a packet on the socket under a connected peer's id could write that peer's input, and a
//! captured datagram could be sent again unchanged for as long as its tick stayed inside the history
//! ring. Three checks close that, in the order a datagram meets them:
//!
//! 1. **A MAC over every byte.** Each session has a 16-byte key, and every datagram but the handshake
//!    carries an 8-byte [`TAG_LEN`] tag over its payload, its sequence number and a direction byte.
//!    A tag that does not verify is dropped before a single field is decoded.
//! 2. **A replay window.** Each datagram carries a 32-bit sequence number. [`ReplayWindow`] accepts a
//!    sequence once and refuses a repeat, and refuses one more than [`REPLAY_WINDOW`] behind the
//!    newest accepted — the same sliding bitmap IPsec uses, sized to tolerate normal reordering.
//! 3. **A per-peer receive budget.** [`ReceiveBudget`] caps how many input blocks one peer can make
//!    the server resolve in one tick, and abandons the rest of a frame once too many of them named an
//!    entity the sender does not own.
//!
//! ## The MAC, and what it is and is not worth
//!
//! [`siphash24`] is SipHash-2-4: a keyed pseudo-random function designed for exactly this — short
//! messages, a 64-bit tag, no table lookups. It is ~40 lines of integer arithmetic, which is what lets
//! `orbitnet-core` stay at zero dependencies.
//!
//! **The key is minted by the client and crosses the wire in the handshake in cleartext.** So what
//! this authenticates is a datagram's membership in a session, not a peer's identity:
//!
//! - An attacker who cannot read the session's traffic cannot forge a datagram at all, whatever
//!   sender id it puts on it. That is the case the transport does not cover.
//! - One connected peer cannot forge another's datagrams: each session has its own key.
//! - **An on-path observer who can read the handshake can do everything the client can.** Closing that
//!   needs a key exchange and therefore a real asymmetric primitive, which is a dependency and a
//!   larger change. It is recorded as a limit in `README.md` rather than hidden here.
//!
//! ## The direction byte
//!
//! The two directions of a session share one key, so without domain separation an attacker could
//! reflect a client's datagram back at the client and have it verify. The direction — [`Direction`] —
//! is mixed into the MAC and is **not transmitted**: each side authenticates with the direction it
//! expects to receive, so a reflected datagram fails the tag check.
//!
//! ## Sequence numbers are refused rather than wrapped
//!
//! 32 bits at 60 Hz is 2.2 years of one session. Past it [`SessionAuth::seal`] returns `None` and the
//! datagram is not sent, because a wrapped sequence would re-open the replay window on every datagram
//! the attacker captured in the first pass.

/// Bytes of session key. 128 bits, the SipHash key width.
pub const KEY_LEN: usize = 16;

/// Bytes of MAC tag on the wire.
pub const TAG_LEN: usize = 8;

/// Bytes of sequence number on the wire.
pub const SEQ_LEN: usize = 4;

/// Bytes every authenticated datagram carries past its payload: sequence number then tag.
pub const TRAILER_LEN: usize = SEQ_LEN + TAG_LEN;

/// How far behind the newest accepted sequence a datagram may still be accepted.
///
/// 64 is one `u64` of bitmap and is far wider than any reordering a session survives: a datagram 64
/// ticks stale is already outside the history ring at every rate this addon runs at.
pub const REPLAY_WINDOW: u32 = 64;

/// Which way along a session a datagram travels. Mixed into the MAC, never transmitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Direction {
    /// Client to server.
    ToServer = 0x01,
    /// Server to client.
    ToClient = 0x02,
}

/// Why a datagram was refused before it was decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    /// The datagram is shorter than its own trailer, so it carries no tag to check.
    Truncated,
    /// The tag does not match: forged, corrupted, or authenticated for the other direction.
    BadTag,
    /// The sequence number was accepted before, or is further behind than [`REPLAY_WINDOW`].
    Replayed,
}

/// SipHash-2-4, fed in pieces.
///
/// **It streams because the direction byte is not on the wire.** A one-shot hash would need the
/// payload and the direction concatenated into one buffer, which is an allocation per datagram in
/// both directions, on the hot path. Feeding the two pieces costs nothing.
#[derive(Debug, Clone, Copy)]
pub struct SipHasher {
    v0: u64,
    v1: u64,
    v2: u64,
    v3: u64,
    /// The bytes of the current word that have arrived, packed low-first.
    tail: u64,
    /// How many bytes of `tail` are filled.
    ntail: usize,
    /// Total bytes written, whose low byte the finalization mixes in.
    length: usize,
}

impl SipHasher {
    /// A hasher keyed with `key`, having consumed nothing.
    #[must_use]
    pub fn new(key: &[u8; KEY_LEN]) -> Self {
        let k0 = u64::from_le_bytes(key[0..8].try_into().unwrap_or([0; 8]));
        let k1 = u64::from_le_bytes(key[8..16].try_into().unwrap_or([0; 8]));
        Self {
            v0: 0x736f_6d65_7073_6575 ^ k0,
            v1: 0x646f_7261_6e64_6f6d ^ k1,
            v2: 0x6c79_6765_6e65_7261 ^ k0,
            v3: 0x7465_6462_7974_6573 ^ k1,
            tail: 0,
            ntail: 0,
            length: 0,
        }
    }

    /// One SipRound. Every step wraps by design — that is the construction, not an overflow — so each
    /// one says so explicitly, because `overflow-checks` is on in the profile every dev run loads.
    fn round(&mut self) {
        self.v0 = self.v0.wrapping_add(self.v1);
        self.v1 = self.v1.rotate_left(13);
        self.v1 ^= self.v0;
        self.v0 = self.v0.rotate_left(32);
        self.v2 = self.v2.wrapping_add(self.v3);
        self.v3 = self.v3.rotate_left(16);
        self.v3 ^= self.v2;
        self.v0 = self.v0.wrapping_add(self.v3);
        self.v3 = self.v3.rotate_left(21);
        self.v3 ^= self.v0;
        self.v2 = self.v2.wrapping_add(self.v1);
        self.v1 = self.v1.rotate_left(17);
        self.v1 ^= self.v2;
        self.v2 = self.v2.rotate_left(32);
    }

    /// Compress one complete little-endian word.
    fn absorb(&mut self, word: u64) {
        self.v3 ^= word;
        self.round();
        self.round();
        self.v0 ^= word;
    }

    /// Feed `bytes`. Any number of calls in any split produce the same tag as one call with the
    /// concatenation.
    pub fn write(&mut self, bytes: &[u8]) {
        self.length = self.length.wrapping_add(bytes.len());
        let mut rest = bytes;
        if self.ntail > 0 {
            let want = 8 - self.ntail;
            let take = want.min(rest.len());
            for (index, &byte) in rest[..take].iter().enumerate() {
                self.tail |= u64::from(byte) << (8 * (self.ntail + index));
            }
            self.ntail += take;
            rest = &rest[take..];
            if self.ntail < 8 {
                return;
            }
            let word = self.tail;
            self.tail = 0;
            self.ntail = 0;
            self.absorb(word);
        }
        let mut chunks = rest.chunks_exact(8);
        for chunk in &mut chunks {
            self.absorb(u64::from_le_bytes(chunk.try_into().unwrap_or([0; 8])));
        }
        for (index, &byte) in chunks.remainder().iter().enumerate() {
            self.tail |= u64::from(byte) << (8 * index);
        }
        self.ntail = chunks.remainder().len();
    }

    /// The 64-bit tag over everything written.
    ///
    /// The final word is the trailing bytes plus the message length modulo 256 in its top byte, which
    /// is what stops two messages differing only in trailing zero bytes from hashing alike.
    #[must_use]
    pub fn finish(mut self) -> u64 {
        let word = self.tail | ((self.length as u64 & 0xff) << 56);
        self.absorb(word);
        self.v2 ^= 0xff;
        for _ in 0..4 {
            self.round();
        }
        self.v0 ^ self.v1 ^ self.v2 ^ self.v3
    }
}

/// SipHash-2-4 over `msg` under `key`, as a 64-bit tag.
///
/// The reference construction, unmodified: two compression rounds per 8-byte word, four finalization
/// rounds. Test vectors from the SipHash reference implementation are asserted below.
#[must_use]
pub fn siphash24(key: &[u8; KEY_LEN], msg: &[u8]) -> u64 {
    let mut hasher = SipHasher::new(key);
    hasher.write(msg);
    hasher.finish()
}

/// Compare two tags without branching on their contents.
///
/// A comparison that returns at the first differing byte leaks, through its timing, how much of a
/// guessed tag was right — which turns forging one into 8 × 256 guesses instead of 2^64. Folding the
/// difference into one value and testing it once does not.
#[must_use]
fn tags_equal(a: u64, b: u64) -> bool {
    let diff = a ^ b;
    // Fold every bit of the difference down to bit 0, so the single test below sees all of them.
    let folded = (diff | diff.wrapping_shr(32)) as u32;
    let folded = folded | folded.wrapping_shr(16);
    let folded = folded | folded.wrapping_shr(8);
    let folded = folded | folded.wrapping_shr(4);
    let folded = folded | folded.wrapping_shr(2);
    let folded = folded | folded.wrapping_shr(1);
    (folded & 1) == 0
}

/// The sliding window that refuses a sequence number twice.
///
/// `newest` is the highest sequence accepted so far and `bitmap` bit *n* records that
/// `newest - n - 1` was accepted, so bit 0 is the datagram before the newest. A sequence ahead of
/// `newest` shifts the map; one behind it fills a bit; one already set, or further back than
/// [`REPLAY_WINDOW`], is refused.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReplayWindow {
    newest: u32,
    bitmap: u64,
}

impl ReplayWindow {
    /// An empty window, having accepted nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The highest sequence number accepted so far, or `0` for a window that has accepted none.
    #[must_use]
    pub fn newest(&self) -> u32 {
        self.newest
    }

    /// Accept `seq` if it is new, recording it. `false` means the datagram is a replay and must be
    /// dropped.
    ///
    /// Sequence `0` is never issued — [`SessionAuth`] starts at 1 — so it is refused outright rather
    /// than treated as "nothing accepted yet", which a forger could otherwise use to reset a window.
    pub fn accept(&mut self, seq: u32) -> bool {
        if seq == 0 {
            return false;
        }
        if seq > self.newest {
            let shift = seq - self.newest;
            self.bitmap = if shift >= 64 {
                0
            } else {
                // The old newest becomes bit `shift - 1` of the new map.
                (self.bitmap << shift) | (1u64 << (shift - 1))
            };
            self.newest = seq;
            return true;
        }
        let behind = self.newest - seq;
        if behind == 0 || behind > REPLAY_WINDOW {
            return false;
        }
        let bit = 1u64 << (behind - 1);
        if self.bitmap & bit != 0 {
            return false;
        }
        self.bitmap |= bit;
        true
    }
}

/// One session's authentication state: the shared key, this side's send counter, and the window that
/// refuses a repeat from the other side.
///
/// Both directions of a session use one key and one [`Direction`] tells them apart, so a peer holds
/// exactly one of these per session — a client one for the server, a server one per connected peer.
#[derive(Debug, Clone, Copy)]
pub struct SessionAuth {
    key: [u8; KEY_LEN],
    next_seq: u32,
    window: ReplayWindow,
}

impl SessionAuth {
    /// A session under `key`, having sent and received nothing.
    #[must_use]
    pub fn new(key: [u8; KEY_LEN]) -> Self {
        Self {
            key,
            next_seq: 1,
            window: ReplayWindow::new(),
        }
    }

    /// The session key, for comparing a repeated handshake against the one already seated.
    #[must_use]
    pub fn key(&self) -> [u8; KEY_LEN] {
        self.key
    }

    /// Whether this session's send counter is spent. A spent session can still receive.
    #[must_use]
    pub fn exhausted(&self) -> bool {
        self.next_seq == 0
    }

    /// Append the sequence number and tag to `payload`, consuming one sequence number.
    ///
    /// `None` means the counter is spent (see the module header) and the datagram must not be sent —
    /// wrapping it would re-open the replay window on everything captured in the first pass.
    pub fn seal(&mut self, direction: Direction, payload: &mut Vec<u8>) -> Option<()> {
        if self.next_seq == 0 {
            return None;
        }
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        payload.extend_from_slice(&seq.to_le_bytes());
        let tag = tag_over(&self.key, payload, direction);
        payload.extend_from_slice(&tag.to_le_bytes());
        Some(())
    }

    /// Verify `datagram` as arriving in `direction`, answering the payload with the trailer stripped.
    ///
    /// The replay window is advanced **only** on a datagram whose tag verified, so an attacker cannot
    /// burn sequence numbers the real peer has yet to send.
    pub fn open<'a>(
        &mut self,
        direction: Direction,
        datagram: &'a [u8],
    ) -> Result<&'a [u8], AuthError> {
        if datagram.len() < TRAILER_LEN {
            return Err(AuthError::Truncated);
        }
        let split = datagram.len() - TAG_LEN;
        let (signed, tag_bytes) = datagram.split_at(split);
        let tag = u64::from_le_bytes(tag_bytes.try_into().unwrap_or([0; TAG_LEN]));
        if !tags_equal(tag, tag_over(&self.key, signed, direction)) {
            return Err(AuthError::BadTag);
        }
        let payload_len = signed.len() - SEQ_LEN;
        let seq = u32::from_le_bytes(signed[payload_len..].try_into().unwrap_or([0; SEQ_LEN]));
        if !self.window.accept(seq) {
            return Err(AuthError::Replayed);
        }
        Ok(&signed[..payload_len])
    }
}

/// The tag over `signed` — payload and sequence number — in `direction`.
///
/// The direction byte is fed here and nowhere else, which is what keeps it off the wire.
fn tag_over(key: &[u8; KEY_LEN], signed: &[u8], direction: Direction) -> u64 {
    let mut hasher = SipHasher::new(key);
    hasher.write(signed);
    hasher.write(&[direction as u8]);
    hasher.finish()
}

/// Input blocks one peer may make the server resolve in one tick.
///
/// Generous against any honest client: a connection sends one block per owned body per tick, and the
/// send path already caps a frame at one datagram. It is a bound on a peer that ignores both.
pub const MAX_INPUT_BLOCKS_PER_TICK: u32 = 64;

/// Blocks naming an entity the sender does not own, per peer per tick, before the rest of that frame
/// is abandoned.
///
/// Not zero, because a *legitimate* one exists: authority can move between peers, and the frames
/// already in flight when it does still carry the previous owner's blocks. A handful covers one round
/// trip of those; a peer producing more is not handing over anything.
pub const MAX_FOREIGN_INPUT_BLOCKS_PER_TICK: u32 = 8;

/// The per-peer bound on what one peer can spend of the server's receive path in one tick.
///
/// The entity-authority check on a received input block is the substantive one, and it is deliberately
/// a live `get_multiplayer_authority()` call on a resolved node handle — so it is not free, and
/// without a bound a peer could spend the server's tick on blocks naming entities it does not own and
/// have every one of them correctly refused. This is what makes refusing them cheap in aggregate.
///
/// Per **tick**, not per frame: a peer that splits its volume across many frames in one tick is the
/// case a per-frame cap misses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReceiveBudget {
    tick: u64,
    blocks: u32,
    foreign: u32,
    started: bool,
}

impl ReceiveBudget {
    /// An unspent budget.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Point the budget at `tick`, resetting it if this is the first frame of a new tick.
    pub fn open(&mut self, tick: u64) {
        if !self.started || tick != self.tick {
            self.tick = tick;
            self.blocks = 0;
            self.foreign = 0;
            self.started = true;
        }
    }

    /// Charge one block. `false` means this peer is over budget for the tick and the rest of the
    /// frame must be abandoned.
    pub fn admit(&mut self) -> bool {
        if self.blocks >= MAX_INPUT_BLOCKS_PER_TICK {
            return false;
        }
        self.blocks += 1;
        true
    }

    /// Record that the block just admitted named an entity the sender does not own. `false` means
    /// the peer has produced too many and the rest of the frame must be abandoned.
    pub fn note_foreign(&mut self) -> bool {
        self.foreign += 1;
        self.foreign <= MAX_FOREIGN_INPUT_BLOCKS_PER_TICK
    }

    /// Blocks charged in the current tick.
    #[must_use]
    pub fn blocks(&self) -> u32 {
        self.blocks
    }

    /// Foreign blocks recorded in the current tick.
    #[must_use]
    pub fn foreign(&self) -> u32 {
        self.foreign
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REF_KEY: [u8; KEY_LEN] = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f,
    ];

    fn ref_msg(len: usize) -> Vec<u8> {
        (0..len).map(|i| i as u8).collect()
    }

    #[test]
    fn siphash_matches_the_reference_vectors() {
        // Message `i` is the bytes 0..i under the reference key, which is the reference
        // implementation's own test corpus. These cover the empty message, both sides of the 8-byte
        // word boundary, and a multi-word message.
        let cases: [(usize, u64); 7] = [
            (0, 0x726f_db47_dd0e_0e31),
            (1, 0x74f8_39c5_93dc_67fd),
            (7, 0xab02_00f5_8b01_d137),
            (8, 0x93f5_f579_9a93_2462),
            (15, 0xa129_ca61_49be_45e5),
            (16, 0x3f2a_cc7f_57c2_9bdb),
            (32, 0x7127_512f_72f2_7cce),
        ];
        for (len, expected) in cases {
            assert_eq!(siphash24(&REF_KEY, &ref_msg(len)), expected, "len {len}");
        }
    }

    #[test]
    fn siphash_separates_keys_and_trailing_zeroes() {
        let mut other = REF_KEY;
        other[0] ^= 1;
        assert_ne!(siphash24(&REF_KEY, b"abc"), siphash24(&other, b"abc"));
        // The length byte in the final word is what makes these differ.
        assert_ne!(siphash24(&REF_KEY, b"a"), siphash24(&REF_KEY, b"a\0"));
    }

    #[test]
    fn streaming_matches_one_shot_at_every_split() {
        // The property `tag_over` depends on: payload and direction byte fed separately must hash
        // exactly as their concatenation would.
        let msg = ref_msg(37);
        let expected = siphash24(&REF_KEY, &msg);
        for split in 0..=msg.len() {
            let mut hasher = SipHasher::new(&REF_KEY);
            hasher.write(&msg[..split]);
            hasher.write(&msg[split..]);
            assert_eq!(hasher.finish(), expected, "split {split}");
        }
        // And byte at a time, which exercises every partial-word path.
        let mut hasher = SipHasher::new(&REF_KEY);
        for byte in &msg {
            hasher.write(&[*byte]);
        }
        assert_eq!(hasher.finish(), expected);
    }

    #[test]
    fn tags_equal_agrees_with_equality() {
        assert!(tags_equal(0, 0));
        assert!(tags_equal(u64::MAX, u64::MAX));
        assert!(!tags_equal(0, 1));
        assert!(!tags_equal(1 << 63, 0));
        assert!(!tags_equal(u64::MAX, u64::MAX - 1));
    }

    #[test]
    fn a_sealed_datagram_opens_to_its_payload() {
        let mut tx = SessionAuth::new(REF_KEY);
        let mut rx = SessionAuth::new(REF_KEY);
        let mut buf = b"hello".to_vec();
        assert!(tx.seal(Direction::ToServer, &mut buf).is_some());
        assert_eq!(buf.len(), 5 + TRAILER_LEN);
        assert_eq!(rx.open(Direction::ToServer, &buf), Ok(&b"hello"[..]));
    }

    #[test]
    fn an_empty_payload_still_seals_and_opens() {
        let mut tx = SessionAuth::new(REF_KEY);
        let mut rx = SessionAuth::new(REF_KEY);
        let mut buf: Vec<u8> = Vec::new();
        tx.seal(Direction::ToClient, &mut buf).unwrap();
        assert_eq!(rx.open(Direction::ToClient, &buf), Ok(&[][..]));
    }

    #[test]
    fn a_tampered_byte_fails_the_tag() {
        let mut tx = SessionAuth::new(REF_KEY);
        let mut rx = SessionAuth::new(REF_KEY);
        let mut buf = b"hello".to_vec();
        tx.seal(Direction::ToServer, &mut buf).unwrap();
        for index in 0..buf.len() {
            let mut forged = buf.clone();
            forged[index] ^= 0x01;
            let mut fresh = SessionAuth::new(REF_KEY);
            assert_eq!(
                fresh.open(Direction::ToServer, &forged),
                Err(AuthError::BadTag),
                "byte {index}"
            );
        }
        // The untouched original still opens, so the loop above rejected forgeries and not the scheme.
        assert!(rx.open(Direction::ToServer, &buf).is_ok());
    }

    #[test]
    fn a_wrong_key_fails_the_tag() {
        let mut tx = SessionAuth::new(REF_KEY);
        let mut other = REF_KEY;
        other[15] ^= 0x80;
        let mut rx = SessionAuth::new(other);
        let mut buf = b"hello".to_vec();
        tx.seal(Direction::ToServer, &mut buf).unwrap();
        assert_eq!(rx.open(Direction::ToServer, &buf), Err(AuthError::BadTag));
    }

    #[test]
    fn a_reflected_datagram_fails_the_tag() {
        // The whole point of the direction byte: the same key, the same bytes, the other direction.
        let mut tx = SessionAuth::new(REF_KEY);
        let mut rx = SessionAuth::new(REF_KEY);
        let mut buf = b"input".to_vec();
        tx.seal(Direction::ToServer, &mut buf).unwrap();
        assert_eq!(rx.open(Direction::ToClient, &buf), Err(AuthError::BadTag));
    }

    #[test]
    fn a_datagram_shorter_than_its_trailer_is_truncated() {
        let mut rx = SessionAuth::new(REF_KEY);
        for len in 0..TRAILER_LEN {
            assert_eq!(
                rx.open(Direction::ToServer, &vec![0u8; len]),
                Err(AuthError::Truncated),
                "len {len}"
            );
        }
    }

    #[test]
    fn a_replayed_datagram_is_refused_once_it_has_been_accepted() {
        let mut tx = SessionAuth::new(REF_KEY);
        let mut rx = SessionAuth::new(REF_KEY);
        let mut buf = b"input".to_vec();
        tx.seal(Direction::ToServer, &mut buf).unwrap();
        assert!(rx.open(Direction::ToServer, &buf).is_ok());
        assert_eq!(rx.open(Direction::ToServer, &buf), Err(AuthError::Replayed));
    }

    #[test]
    fn a_forged_datagram_does_not_burn_a_sequence_number() {
        let mut tx = SessionAuth::new(REF_KEY);
        let mut rx = SessionAuth::new(REF_KEY);
        let mut first = b"one".to_vec();
        tx.seal(Direction::ToServer, &mut first).unwrap();
        let mut forged = first.clone();
        forged[0] ^= 0xff;
        assert_eq!(
            rx.open(Direction::ToServer, &forged),
            Err(AuthError::BadTag)
        );
        // Sequence 1 was never accepted, so the genuine datagram carrying it still is.
        assert!(rx.open(Direction::ToServer, &first).is_ok());
    }

    #[test]
    fn seal_refuses_once_the_counter_is_spent() {
        // Parked on the last sequence number rather than sealing four billion datagrams to reach it.
        let mut spent = SessionAuth {
            key: REF_KEY,
            next_seq: u32::MAX,
            window: ReplayWindow::new(),
        };
        let mut buf = Vec::new();
        assert!(spent.seal(Direction::ToServer, &mut buf).is_some());
        assert!(spent.exhausted());
        let mut buf = Vec::new();
        assert!(spent.seal(Direction::ToServer, &mut buf).is_none());
        assert!(buf.is_empty());
    }

    #[test]
    fn the_window_accepts_reordering_and_refuses_repeats() {
        let mut window = ReplayWindow::new();
        assert!(window.accept(5));
        assert!(window.accept(3));
        assert!(window.accept(4));
        assert!(!window.accept(3));
        assert!(!window.accept(5));
        assert!(window.accept(6));
        assert_eq!(window.newest(), 6);
    }

    #[test]
    fn the_window_refuses_zero_and_anything_past_its_width() {
        let mut window = ReplayWindow::new();
        assert!(!window.accept(0));
        assert!(window.accept(1000));
        assert!(window.accept(1000 - REPLAY_WINDOW));
        assert!(!window.accept(1000 - REPLAY_WINDOW - 1));
        assert!(!window.accept(1));
    }

    #[test]
    fn a_jump_past_the_window_clears_every_recorded_bit() {
        let mut window = ReplayWindow::new();
        for seq in 1..=10 {
            assert!(window.accept(seq));
        }
        assert!(window.accept(1000));
        // Everything before the jump is now further back than the window reaches.
        assert!(!window.accept(10));
        assert!(window.accept(999));
    }

    #[test]
    fn the_window_fills_every_slot_exactly_once() {
        let mut window = ReplayWindow::new();
        assert!(window.accept(100));
        for behind in 1..=REPLAY_WINDOW {
            assert!(window.accept(100 - behind), "behind {behind}");
        }
        for behind in 1..=REPLAY_WINDOW {
            assert!(!window.accept(100 - behind), "repeat {behind}");
        }
    }

    #[test]
    fn the_budget_resets_on_a_new_tick() {
        let mut budget = ReceiveBudget::new();
        budget.open(7);
        for _ in 0..MAX_INPUT_BLOCKS_PER_TICK {
            assert!(budget.admit());
        }
        assert!(!budget.admit());
        budget.open(8);
        assert!(budget.admit());
        assert_eq!(budget.blocks(), 1);
    }

    #[test]
    fn the_budget_spans_every_frame_of_one_tick() {
        let mut budget = ReceiveBudget::new();
        budget.open(7);
        for _ in 0..MAX_INPUT_BLOCKS_PER_TICK {
            assert!(budget.admit());
        }
        // A second frame in the same tick reopens the budget and gets nothing.
        budget.open(7);
        assert!(!budget.admit());
    }

    #[test]
    fn the_budget_abandons_a_frame_that_keeps_naming_foreign_entities() {
        let mut budget = ReceiveBudget::new();
        budget.open(1);
        for _ in 0..MAX_FOREIGN_INPUT_BLOCKS_PER_TICK {
            assert!(budget.note_foreign());
        }
        assert!(!budget.note_foreign());
        assert_eq!(budget.foreign(), MAX_FOREIGN_INPUT_BLOCKS_PER_TICK + 1);
    }

    #[test]
    fn a_fresh_budget_opens_on_tick_zero() {
        // `started` exists for this: a default budget must not read tick 0 as "already open".
        let mut budget = ReceiveBudget::new();
        budget.open(0);
        assert!(budget.admit());
        assert_eq!(budget.blocks(), 1);
    }
}
