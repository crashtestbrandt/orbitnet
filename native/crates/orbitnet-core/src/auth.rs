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
//! ## Deriving the key from a secret both ends already hold
//!
//! A game that already shares a secret with the peer — a lobby token, a matchmaker ticket, anything it
//! authenticated before the join — does not have to mint the key on the client and send it. It can
//! derive the key instead, with [`compress_secret`], [`derive_session_key`] and [`confirm_tag`]. The
//! 16 bytes in the handshake stop being the key and become a **nonce**.
//!
//! | | Key minted by the client | Key derived from a shared secret |
//! | --- | --- | --- |
//! | What the handshake carries | the key itself | a nonce |
//! | What an on-path observer learns | everything the client knows | the nonce, and nothing else |
//! | What the scheme needs | nothing | a secret the game distributes out of band |
//! | Who can join | anyone the transport accepts | anyone holding the secret |
//!
//! The secret is an **input** to the derivation and is never seated as the key itself;
//! [`derive_session_key`] carries the reason, because seating it is the obvious wrong implementation.
//!
//! Three ceilings:
//!
//! - **It adds no strength beyond the secret's own entropy.** A secret a lobby prints on screen, or one
//!   short enough to guess, derives a key worth exactly that much.
//! - **The tag is still 64 bits and the key still 128.** Deriving the key changes who can forge a
//!   datagram. It does not change how hard forging one is for somebody who cannot read the secret.
//! - **None of this encrypts anything.** Every payload is still on the wire in the clear. A MAC says a
//!   datagram was not written by someone outside the session, and says nothing else.
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

// The domain labels below are part of the wire contract even though they never appear on the wire.
// Every shipped client bakes them into the key it derives, so changing one changes every derived key,
// every datagram fails its tag against a peer on the old label, and nothing in the failure says why.
// They are public constants so that a refactor has to edit a documented item to change the derivation,
// and so a port to another language has the exact bytes to reproduce.
//
// A label is a **key** where the hashed input is variable length — the secret has to be the message
// then — and a **message prefix** where the key slot is already taken by the secret.

/// Domain label keying the low half of [`compress_secret`]. Exactly [`KEY_LEN`] bytes, as a SipHash key.
pub const SECRET_LABEL_LOW: [u8; KEY_LEN] = *b"orbitnet-fold-lo";

/// Domain label keying the high half of [`compress_secret`]. Exactly [`KEY_LEN`] bytes, as a SipHash key.
pub const SECRET_LABEL_HIGH: [u8; KEY_LEN] = *b"orbitnet-fold-hi";

/// Domain label prefixing the low half of [`derive_session_key`].
pub const SESSION_KEY_LABEL_LOW: &[u8] = b"orbitnet-session-key-lo";

/// Domain label prefixing the high half of [`derive_session_key`].
pub const SESSION_KEY_LABEL_HIGH: &[u8] = b"orbitnet-session-key-hi";

/// Domain label prefixing [`confirm_tag`], which keeps a confirmation from being any other tag.
pub const CONFIRM_LABEL: &[u8] = b"orbitnet-confirm";

/// Two 64-bit halves as one 128-bit value: little-endian, low half first.
///
/// One function so that the byte order of every derived key is defined in one place.
fn join_halves(low: u64, high: u64) -> [u8; KEY_LEN] {
    let mut out = [0u8; KEY_LEN];
    out[..8].copy_from_slice(&low.to_le_bytes());
    out[8..].copy_from_slice(&high.to_le_bytes());
    out
}

/// Fold a secret of any length to a 16-byte session secret.
///
/// The game supplies the secret out of band and it can be any shape: a lobby token, a matchmaker
/// ticket, a passphrase a player typed. [`SipHasher`] keys on exactly [`KEY_LEN`] bytes, so something
/// has to produce those 16, and this is it — two keyed passes over the secret, under
/// [`SECRET_LABEL_LOW`] and [`SECRET_LABEL_HIGH`], joined low half first.
///
/// **A secret that is already [`KEY_LEN`] bytes long is folded too, never used verbatim.** One code
/// path then produces the session secret whatever the caller supplied:
///
/// - A game that moves from a 40-character token to 16 raw bytes does not change derivations at the
///   same time, and no caller has to know which of the two shapes it is holding.
/// - The length is inside the hash, so `b"key"` and `b"key\0"` are different secrets. A fold that
///   passed 16 bytes through and hashed everything else would make the boundary at 16 bytes a
///   behaviour change nobody can see.
///
/// The fold cannot add entropy, and takes essentially none away: it is a pseudo-random function of the
/// whole secret, and the tag it eventually protects is 64 bits.
#[must_use]
pub fn compress_secret(secret: &[u8]) -> [u8; KEY_LEN] {
    join_halves(
        siphash24(&SECRET_LABEL_LOW, secret),
        siphash24(&SECRET_LABEL_HIGH, secret),
    )
}

/// The session key for one join, derived from the shared secret and the handshake nonce.
///
/// `secret` is the 16 bytes [`compress_secret`] folded out of whatever the game distributes. `nonce` is
/// the 16 bytes the handshake carries, which under this scheme are no longer a key: the joining side
/// mints them fresh per join, the accepting side reads them, and both run this function to arrive at
/// the same key. An observer reading the handshake learns the nonce and nothing else.
///
/// **The secret is an input and is never seated as the key**, however much shorter that implementation
/// looks. The reason is the sequence numbers:
///
/// - [`SessionAuth::new`] starts every session's counter at 1, and [`ReplayWindow`] only ever knows the
///   session in front of it.
/// - So under a key that does not change between joins, every datagram captured in one session is a
///   valid, unreplayed datagram in the next. The replay defence would last exactly one session.
/// - A fresh nonce per join is what keeps the key fresh per join, and that is the only reason the
///   nonce exists. A caller that reuses a nonce under one secret gets the constant-key failure back.
///
/// A peer deriving from a different secret produces a different key, so its datagrams fail the tag
/// check at the other end. That is how a peer without the secret is refused; [`confirm_tag`] moves the
/// refusal forward into the handshake so it does not wait for the first datagram.
#[must_use]
pub fn derive_session_key(secret: &[u8; KEY_LEN], nonce: &[u8; KEY_LEN]) -> [u8; KEY_LEN] {
    let mut low = SipHasher::new(secret);
    low.write(SESSION_KEY_LABEL_LOW);
    low.write(nonce);
    let mut high = SipHasher::new(secret);
    high.write(SESSION_KEY_LABEL_HIGH);
    high.write(nonce);
    join_halves(low.finish(), high.finish())
}

/// Proof the sender holds `secret`: a tag over the nonce and the protocol version.
///
/// `key` is the output of [`derive_session_key`], so producing this tag requires the secret the key was
/// derived from. The joining side sends it beside its nonce; the accepting side derives its own key from
/// its own copy of the secret, recomputes the tag, and refuses the join when the two differ. Without it
/// a peer that does not hold the secret is still refused, but only once it has sent a datagram whose tag
/// fails — it occupies a session slot until then.
///
/// **The protocol version is inside the tag** so that a confirmation cannot be lifted out of a session
/// of one protocol version and replayed into a session of another, where the fields it authorises mean
/// something else.
///
/// Two limits:
///
/// - **It proves possession of the secret, not identity.** Everyone the game handed the secret to can
///   produce a valid tag over any nonce they like.
/// - **Compare it without branching on its contents**, the way the receive path compares datagram tags.
///   A byte-at-a-time comparison that returns early leaks how much of a guess was right.
#[must_use]
pub fn confirm_tag(key: &[u8; KEY_LEN], nonce: &[u8; KEY_LEN], protocol_version: u32) -> u64 {
    let mut hasher = SipHasher::new(key);
    hasher.write(CONFIRM_LABEL);
    hasher.write(nonce);
    hasher.write(&protocol_version.to_le_bytes());
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
            // Every recorded bit moves up by `shift`, and the old newest lands at bit `shift - 1`.
            // Both are `checked_shl` rather than a width test, because the two have DIFFERENT
            // boundaries: a jump of exactly [`REPLAY_WINDOW`] shifts every recorded bit out but puts
            // the old newest on the last bit the window still covers, and the read path below accepts
            // `behind == REPLAY_WINDOW`. Clearing the map wholesale there loses that one bit, and the
            // datagram it recorded is accepted a second time.
            self.bitmap = self.bitmap.checked_shl(shift).unwrap_or(0);
            if let Some(bit) = 1u64.checked_shl(shift - 1) {
                self.bitmap |= bit;
            }
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

    /// A secret of the shape a game actually supplies: not 16 bytes, and not a round number of words.
    const PIN_SECRET: &[u8] = b"orbitnet shared secret";

    /// A fixed stand-in for the 16 bytes a handshake carries.
    const PIN_NONCE: [u8; KEY_LEN] = [
        0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xab, 0xac, 0xad, 0xae,
        0xaf,
    ];

    /// A protocol version written out rather than read from the crate, so that bumping the real one
    /// does not rewrite the pinned vectors below and hide a change to the derivation.
    const PIN_VERSION: u32 = 0x0006_0000;

    #[test]
    fn derive_session_key_is_deterministic() {
        // Both ends run these two lines independently and must land on the same key, or nothing
        // either of them sends opens at the other.
        let secret = compress_secret(PIN_SECRET);
        assert_eq!(
            derive_session_key(&secret, &PIN_NONCE),
            derive_session_key(&secret, &PIN_NONCE)
        );
        assert_eq!(compress_secret(PIN_SECRET), secret);
    }

    #[test]
    fn the_secret_is_never_seated_as_the_session_key() {
        // Seating the secret is the shorter implementation and the wrong one: see
        // `derive_session_key`. Two nonces, including the all-zero one a lazy caller would supply.
        let secret = compress_secret(PIN_SECRET);
        assert_ne!(derive_session_key(&secret, &PIN_NONCE), secret);
        assert_ne!(derive_session_key(&secret, &[0u8; KEY_LEN]), secret);
    }

    #[test]
    fn different_nonces_under_one_secret_give_different_keys() {
        // The cross-session replay property, and the whole reason the nonce exists. Sequence numbers
        // restart at 1 every session, so two joins under one shared secret landing on one key would
        // make every datagram captured in the first join a valid, unreplayed datagram in the second.
        let secret = compress_secret(PIN_SECRET);
        let base = derive_session_key(&secret, &PIN_NONCE);
        for index in 0..KEY_LEN {
            let mut nonce = PIN_NONCE;
            nonce[index] ^= 0x01;
            assert_ne!(
                derive_session_key(&secret, &nonce),
                base,
                "nonce byte {index}"
            );
        }
    }

    #[test]
    fn different_secrets_under_one_nonce_give_different_keys() {
        let secret = compress_secret(PIN_SECRET);
        let base = derive_session_key(&secret, &PIN_NONCE);
        // A secret differing in one character, before the fold.
        assert_ne!(
            derive_session_key(&compress_secret(b"orbitnet shared secreT"), &PIN_NONCE),
            base
        );
        // And every byte of the folded secret, after it.
        for index in 0..KEY_LEN {
            let mut altered = secret;
            altered[index] ^= 0x80;
            assert_ne!(
                derive_session_key(&altered, &PIN_NONCE),
                base,
                "secret byte {index}"
            );
        }
    }

    #[test]
    fn compress_secret_is_length_sensitive() {
        // Length alone separates two secrets: a trailing zero byte is a different secret.
        assert_ne!(compress_secret(b"secret"), compress_secret(b"secret\0"));
        assert_ne!(compress_secret(b""), compress_secret(b"\0"));
        // The empty secret folds to a value like any other, and to the same one every time. Its bytes
        // are pinned below with the rest.
        assert_eq!(compress_secret(b""), compress_secret(&[]));
        // A secret already KEY_LEN bytes long is folded rather than passed through, so one code path
        // produces the session secret whatever the caller supplied.
        assert_ne!(compress_secret(&REF_KEY), REF_KEY);
    }

    #[test]
    fn confirm_tag_changes_with_every_input() {
        let key = derive_session_key(&compress_secret(PIN_SECRET), &PIN_NONCE);
        let base = confirm_tag(&key, &PIN_NONCE, PIN_VERSION);
        // The negative control for the three sweeps below: the same three inputs give the same tag,
        // so a difference is the input that changed and not a call that never repeats.
        assert_eq!(confirm_tag(&key, &PIN_NONCE, PIN_VERSION), base);
        for index in 0..KEY_LEN {
            let mut nonce = PIN_NONCE;
            nonce[index] ^= 0x01;
            assert_ne!(
                confirm_tag(&key, &nonce, PIN_VERSION),
                base,
                "nonce byte {index}"
            );
            let mut other = key;
            other[index] ^= 0x01;
            assert_ne!(
                confirm_tag(&other, &PIN_NONCE, PIN_VERSION),
                base,
                "key byte {index}"
            );
        }
        // The protocol version on its own, both a patch bump and a major one.
        assert_ne!(confirm_tag(&key, &PIN_NONCE, PIN_VERSION + 1), base);
        assert_ne!(
            confirm_tag(&key, &PIN_NONCE, PIN_VERSION + 0x0001_0000),
            base
        );
        assert_ne!(confirm_tag(&key, &PIN_NONCE, 0), base);
    }

    #[test]
    fn the_derivation_matches_its_pinned_byte_vectors() {
        // These bytes exist to make a refactor that changes the derivation fail here. The derivation
        // is baked into every client that has shipped: change a domain label, the byte order of the
        // halves, or which pass is the low one, and a new build derives a different key from the same
        // secret and nonce, every datagram fails its tag against a peer on the old build, and nothing
        // in the failure says why. A failure on this test is that change, and the fix is either to
        // undo it or to treat it as a protocol version bump.
        assert_eq!(
            compress_secret(b""),
            [
                0x33, 0xf3, 0x45, 0x39, 0xec, 0x2d, 0x83, 0x69, 0x57, 0xe7, 0x79, 0x94, 0xcf, 0x9c,
                0x78, 0xa4
            ]
        );
        assert_eq!(
            compress_secret(PIN_SECRET),
            [
                0x26, 0xed, 0x38, 0xf9, 0x98, 0xc9, 0xdb, 0x3b, 0xc1, 0xce, 0x13, 0xc0, 0x25, 0x6f,
                0x59, 0x2d
            ]
        );
        let secret = compress_secret(PIN_SECRET);
        let key = derive_session_key(&secret, &PIN_NONCE);
        assert_eq!(
            key,
            [
                0xb4, 0x54, 0xfb, 0xd2, 0xd2, 0x7f, 0xfe, 0x30, 0x5e, 0x5d, 0x35, 0xb5, 0x94, 0x8d,
                0xa1, 0x33
            ]
        );
        assert_eq!(
            confirm_tag(&key, &PIN_NONCE, PIN_VERSION),
            0xcb13_d7c3_763b_61c6
        );
    }

    #[test]
    fn a_session_under_a_derived_key_carries_both_directions_and_no_other_derivation() {
        let secret = compress_secret(PIN_SECRET);
        let key = derive_session_key(&secret, &PIN_NONCE);
        let mut client = SessionAuth::new(key);
        let mut server = SessionAuth::new(key);
        let mut up = b"input".to_vec();
        client.seal(Direction::ToServer, &mut up).unwrap();
        assert_eq!(server.open(Direction::ToServer, &up), Ok(&b"input"[..]));
        let mut down = b"snapshot".to_vec();
        server.seal(Direction::ToClient, &mut down).unwrap();
        assert_eq!(
            client.open(Direction::ToClient, &down),
            Ok(&b"snapshot"[..])
        );
        // The next join under the same secret takes a fresh nonce, and the previous join's datagrams
        // do not open under it.
        let mut next_nonce = PIN_NONCE;
        next_nonce[0] ^= 0x01;
        let mut next_join = SessionAuth::new(derive_session_key(&secret, &next_nonce));
        assert_eq!(
            next_join.open(Direction::ToServer, &up),
            Err(AuthError::BadTag)
        );
        // Nor does a peer deriving from a different secret over the same nonce open them.
        let stranger_key = derive_session_key(&compress_secret(b"another secret"), &PIN_NONCE);
        let mut stranger = SessionAuth::new(stranger_key);
        assert_eq!(
            stranger.open(Direction::ToServer, &up),
            Err(AuthError::BadTag)
        );
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

    /// The jump-width sweep, because the interesting widths are the ones next to each other: a jump of
    /// exactly [`REPLAY_WINDOW`] shifts every recorded bit out of the map but leaves the pre-jump
    /// newest on the LAST bit the window still covers, and one wider genuinely outruns it. Refusing the
    /// pre-jump sequence is what both must do, for different reasons.
    #[test]
    fn a_forward_jump_never_forgets_a_sequence_it_still_covers() {
        for shift in 1..=(REPLAY_WINDOW + 2) {
            let mut window = ReplayWindow::new();
            assert!(window.accept(1000));
            assert!(window.accept(1000 + shift), "shift {shift}");
            assert!(
                !window.accept(1000),
                "shift {shift}: the pre-jump newest replayed"
            );
        }
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
