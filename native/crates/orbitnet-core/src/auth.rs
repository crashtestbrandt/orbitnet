//! What the receive path refuses: datagram authenticity, replay, and per-peer volume — and, under a
//! session secret, what the send path encrypts.
//!
//! Before this module the transport's sender id was the whole of a datagram's identity. Anything that
//! could put a packet on the socket under a connected peer's id could write that peer's input, and a
//! captured datagram could be sent again unchanged for as long as its tick stayed inside the history
//! ring. Three checks close that, in the order a datagram meets them:
//!
//! 1. **A tag over every byte.** Each session has a 16-byte key, and every datagram but the handshake
//!    and the challenge carries a tag over its payload, its sequence number and a direction byte. A tag
//!    that does not verify is dropped before a single field is decoded. With no session secret the tag
//!    is [`TAG_LEN`] bytes of SipHash-2-4; under one it is [`CIPHER_TAG_LEN`] bytes of Poly1305 and the
//!    payload beneath it is ciphertext.
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
//! messages, a 64-bit tag, no table lookups. It is ~40 lines of integer arithmetic, which is why it could
//! be written here at all instead of taken as a dependency. That shape does not generalise: a stream cipher
//! is close to it, an elliptic curve is not.
//!
//! **The key is folded from two nonces, one drawn by each end, and from up to two secrets.** The joiner
//! sends its half in the handshake, the acceptor answers with a half of its own, and both ends fold the
//! pair into the session nonce with [`session_nonce`]. A session may then add a secret the game
//! distributed, an X25519 key exchange, both, or neither. With neither, the fold is a public function of
//! two values an observer can read, so what this authenticates is a datagram's membership in a session
//! rather than a peer's identity:
//!
//! - An attacker who cannot read the session's traffic cannot forge a datagram at all, whatever
//!   sender id it puts on it. That is the case the transport does not cover.
//! - One connected peer cannot forge another's datagrams: each session has its own key.
//! - **An on-path observer who can read the exchange can do everything the client can.** With neither
//!   secret configured both halves are in the clear — one in the handshake, one in the challenge — so
//!   the fold is a public function of public values, and an observer that read both frames holds the
//!   key. Either secret narrows it.
//!
//! **`tests/constant_time.rs` is the timing harness.** It asserts that `tags_equal`'s source, compiled
//! on its own under each shipped profile's flags, emits no branch, and that two refused datagrams
//! differing in which tag byte is wrong are not separable by a t-test against a tolerance it measures
//! on the box it runs on. It covers this compare and nothing else. The curve arithmetic below is a
//! dependency's and is covered by that dependency's own review, not by this harness; nothing
//! hand-written here past the compare needs constant time, because SipHash has no secret-dependent
//! branch or table lookup by construction. The branch assertion is in `just native-test`;
//! `just native-timing` runs the measurement by hand, and no job runs it.
//!
//! ## Deriving the key from a secret both ends already hold
//!
//! A game that already shares a secret with the peer — a lobby token, a matchmaker ticket, anything it
//! authenticated before the join — can make that secret an input to every session key, with
//! [`compress_secret`], [`derive_session_key`] and [`confirm_tag`]. Nothing on the wire changes shape;
//! what changes is whether the two nonces are the whole of the key.
//!
//! | | No session secret | A session secret |
//! | --- | --- | --- |
//! | What the key is | [`session_nonce`] of the two halves | [`derive_session_key`] over the secret and that |
//! | What an on-path observer learns | both halves, the key, and every payload | both halves, and nothing else |
//! | What authenticates a datagram | [`siphash24`], a 64-bit tag | Poly1305, a 128-bit tag |
//! | What the payload is | plaintext | ChaCha20 ciphertext under [`derive_cipher_key`] |
//! | What the scheme needs | nothing | a secret the game distributes out of band |
//! | Who can join | anyone the transport accepts | anyone holding the secret |
//!
//! The secret is an **input** to the derivation and is never seated as the key itself;
//! [`derive_session_key`] carries the reason, because seating it is the obvious wrong implementation.
//!
//! Three ceilings:
//!
//! - **It adds no strength beyond the secret's own entropy.** A secret a lobby prints on screen, or one
//!   short enough to guess, derives a key worth exactly that much — the MAC key, the cipher key and
//!   every confirmation alike.
//! - **The key is still 128 bits.** [`derive_cipher_key`] expands to the 32 bytes ChaCha20 takes and
//!   cannot expand the entropy that went in.
//! - **It hides the payload from an observer and not from a peer.** Everyone the game handed the secret
//!   to derives the same key from the same join, so this is confidentiality against somebody outside
//!   the session, not between the peers inside it.
//!
//! ## What a session secret encrypts, and what it leaves readable
//!
//! Under a secret every datagram [`SessionAuth::seal`] produces is **ChaCha20-Poly1305** over the
//! payload: a reviewed AEAD rather than a cipher and a MAC wired together here. Two consequences are
//! worth stating separately from the table above.
//!
//! - **The Poly1305 tag replaces the SipHash one rather than joining it.** One pass over the datagram
//!   instead of two, and the authenticated payload gains 8 bytes rather than 16. It also retires the
//!   64-bit tag ceiling for a session that configured a secret; a session that did not still has it.
//! - **The sequence number stays in the clear**, because the receiver needs it before it holds a
//!   plaintext: it is half of the nonce the decryption runs under. It is named as associated data, so
//!   altering it fails the tag rather than decrypting against a different nonce.
//!
//! What a session secret does **not** hide: how many datagrams a peer sends, when it sends them, and
//! how long each one is. A snapshot frame is as long as the entities it carries, so an observer counting
//! bytes still sees a session's shape. Length hiding would cost padding on every frame, and it is not
//! done.
//!
//! **With no secret configured, nothing here runs and nothing changes.** [`SessionAuth::new`] carries no
//! cipher key, and the wire is the SipHash tag and the plaintext payload it has always been. Encrypting
//! there would buy nothing: both halves of the nonce the key is folded from cross the wire, so an
//! observer that read the join computes the key that hid the payload.
//!
//! ## Deriving the key from an exchange, against an acceptor key the joiner pinned
//!
//! The regime above needs a secret both ends already hold, which needs a channel the game already
//! authenticated. A game with no account service has none. [`joiner_exchange_secret`] and
//! [`acceptor_exchange_secret`] are the other answer: an X25519 exchange on the two legs the join
//! already has, authenticated by an acceptor **static public key the joiner pinned before it
//! connected**.
//!
//! **What authenticates the acceptor is the pin and nothing else.** The exchange contributes two
//! Diffie-Hellman values: one between the joiner's ephemeral and the pinned static key, which only the
//! holder of that key's secret half can compute, and one between the two ephemerals, which is what
//! keeps a static secret leaked later from recovering a recorded join. An exchange with no pinned key
//! is substituted by exactly the on-path attacker it defends against, so **an unpinned joiner does not
//! run one**, and a pinned joiner refuses a join the acceptor answered with no exchange.
//!
//! **The pin is a public key and the session secret is a shared one, and that is the whole difference.**
//! Both have to reach the client out of band; what they demand of that channel is not the same.
//!
//! | | A session secret | A pinned acceptor key |
//! | --- | --- | --- |
//! | What the client holds | the same bytes the acceptor holds | the public half only |
//! | What the channel must provide | integrity **and** confidentiality | integrity |
//! | May it ship inside the game build | no — every player then holds it | yes — it is public by design |
//! | Can a holder impersonate the acceptor | yes | no |
//! | Per what | per trust domain, rotated per lobby or ticket | per acceptor identity, for that key's life |
//! | What a recording is worth if the credential leaks later | every past session's key | nothing, because of the ephemeral half |
//!
//! **That difference is what makes the exchange available to a game with no account service.** A
//! published key, a key baked into the build, a key in a server-browser row — all are channels with
//! integrity and no confidentiality, and all of them carry a pin. None of them can carry a secret.
//!
//! **It authenticates the acceptor to the joiner and nobody to anybody else.** The joiner is anonymous:
//! anyone who can reach the acceptor runs the exchange and derives a key. Refusing a joiner is what the
//! session secret, the resume token and the transport are for, and none of them moves here.
//!
//! **Where a pin cannot come from.** An acceptor whose key changes every session — a listen server a
//! player hosts, reached through a direct-connect address box — has nothing to pin. That case stays on
//! the regime it is on, and the limits above are its limits. Trust on first use is not offered: it is
//! unauthenticated on exactly the join an attacker is present for, and a client that believed its answer
//! would report a security property it does not have.
//!
//! Two ceilings the exchange does not lift, on top of the three above:
//!
//! - **The key is still 128 bits and the tag still 64.** X25519's own level is higher than either, so
//!   what bounds a forged datagram is unchanged.
//! - **It encrypts nothing.** It changes who can derive the key. Every payload is still in the clear.
//!
//! [`fold_secrets`] is what lets a game configure both: the exchange's output and a supplied secret
//! fold into the one input [`derive_session_key`] takes.
//!
//! ## Both ends contribute a nonce, which is what refuses a replayed join
//!
//! **While the nonce was the joiner's alone, an observer could present a recorded one again.** The
//! accepting side then derived the key that join had used, and the observer got a session in which its
//! captured datagrams verified — without learning the key, authoring anything new, or landing those
//! datagrams anywhere. [`session_nonce`] closes it: the acceptor draws 16 bytes of its own per join,
//! both halves go into the fold, and a replayed half derives a key the replayer cannot compute and did
//! not capture.
//!
//! **The price is that a joiner may not send until the acceptor has answered.** The key is not derivable
//! from the joiner's own half, so the join is two round trips rather than one: handshake, the acceptor's
//! nonce, the joiner's confirmation, the reply. `docs/protocol.md` measures that against join time.
//!
//! Two limits:
//!
//! - **With no secret configured it adds nothing.** Both halves are in the clear, so the fold is a
//!   public function of public values and an on-path observer computes the key either way. It runs
//!   there all the same, so one derivation and one frame sequence cover both regimes.
//! - **It refuses a replay, not an injection.** An attacker able to answer the acceptor's nonce in the
//!   joiner's place is authoring a fresh join, and a session secret is what refuses that.
//!
//! ## The direction byte
//!
//! The two directions of a session share one key, so without domain separation an attacker could
//! reflect a client's datagram back at the client and have it verify. The direction — [`Direction`] —
//! is mixed into the tag and is **not transmitted**: each side authenticates with the direction it
//! expects to receive, so a reflected datagram fails the tag check.
//!
//! **Under a secret it is also the first byte of the AEAD nonce**, where it does more than refuse a
//! reflection. One key covers both directions, so without it the client's datagram 5 and the server's
//! would encrypt under one nonce, and an observer holding both would hold the exclusive-or of the two
//! payloads. See [`datagram_nonce`].
//!
//! ## Sequence numbers are refused rather than wrapped
//!
//! 32 bits at 60 Hz is 2.2 years of one session. Past it [`SessionAuth::seal`] returns `None` and the
//! datagram is not sent, because a wrapped sequence would re-open the replay window on every datagram
//! the attacker captured in the first pass.
//!
//! **That refusal is now load-bearing twice.** Under a secret the sequence number is also the varying
//! half of the AEAD nonce, and a repeated nonce under one ChaCha20 key gives an observer the
//! exclusive-or of the two payloads and forges the Poly1305 key outright. Widening the counter instead
//! of refusing it would break both properties at once.

use chacha20poly1305::aead::inout::InOutBuf;
use chacha20poly1305::{AeadInOut, ChaCha20Poly1305, KeyInit};

use x25519_dalek::{PublicKey, SharedSecret, StaticSecret};

/// Bytes of session key. 128 bits, the SipHash key width.
pub const KEY_LEN: usize = 16;

/// Bytes of MAC tag on the wire.
pub const TAG_LEN: usize = 8;

/// Bytes of sequence number on the wire.
pub const SEQ_LEN: usize = 4;

/// Bytes every authenticated datagram carries past its payload: sequence number then tag.
pub const TRAILER_LEN: usize = SEQ_LEN + TAG_LEN;

/// Bytes of ChaCha20-Poly1305 key. 256 bits, the only width the cipher takes.
pub const CIPHER_KEY_LEN: usize = 32;

/// Bytes of Poly1305 tag an encrypted datagram carries.
pub const CIPHER_TAG_LEN: usize = 16;

/// Bytes an encrypted datagram carries past its ciphertext: sequence number then Poly1305 tag.
///
/// Eight more than [`TRAILER_LEN`]. The Poly1305 tag replaces the SipHash one rather than joining it,
/// so the whole of the cost is the wider tag.
pub const CIPHER_TRAILER_LEN: usize = SEQ_LEN + CIPHER_TAG_LEN;

/// Bytes of ChaCha20-Poly1305 nonce, fixed at 96 bits by RFC 8439.
pub const CIPHER_NONCE_LEN: usize = 12;

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
    ///
    /// Under a session secret this is the Poly1305 verification failing, which covers the same three
    /// causes and one more — a ciphertext altered in flight. No plaintext is produced either way.
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

/// Domain labels prefixing the four 64-bit words of [`derive_cipher_key`], low word first.
///
/// Four rather than two because ChaCha20-Poly1305 keys on [`CIPHER_KEY_LEN`] bytes where SipHash keys
/// on [`KEY_LEN`], and each pass produces eight. A label per word is what keeps the four from being
/// four copies of one value.
pub const CIPHER_KEY_LABELS: [&[u8]; 4] = [
    b"orbitnet-cipher-key-0",
    b"orbitnet-cipher-key-1",
    b"orbitnet-cipher-key-2",
    b"orbitnet-cipher-key-3",
];

/// Domain label keying the low half of [`session_nonce`]. Exactly [`KEY_LEN`] bytes, as a SipHash key.
pub const JOIN_LABEL_LOW: [u8; KEY_LEN] = *b"orbitnet-join-lo";

/// Domain label keying the high half of [`session_nonce`]. Exactly [`KEY_LEN`] bytes, as a SipHash key.
pub const JOIN_LABEL_HIGH: [u8; KEY_LEN] = *b"orbitnet-join-hi";

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
///   behavior change nobody can see.
///
/// The fold cannot add entropy, and takes essentially none away. It is a pseudo-random function of the
/// whole secret, and the 16 bytes it produces are the ceiling every key derived from it inherits — the
/// MAC key, the cipher key and every confirmation alike.
#[must_use]
pub fn compress_secret(secret: &[u8]) -> [u8; KEY_LEN] {
    join_halves(
        siphash24(&SECRET_LABEL_LOW, secret),
        siphash24(&SECRET_LABEL_HIGH, secret),
    )
}

/// The 16 bytes one join is keyed from: the joiner's half and the acceptor's, folded together.
///
/// **Both ends draw a half and neither half alone decides the key.** The joiner mints its 16 bytes and
/// sends them in the handshake; the acceptor mints its own and answers with them; both then run this
/// function and arrive at the same 16 bytes. What it closes is a REPLAYED JOIN: an observer presenting a
/// recorded handshake supplies the joiner's half and nothing else, the acceptor's half is drawn fresh,
/// and the session that comes out is keyed on bytes the observer never saw.
///
/// **It is run under both regimes.** With no session secret the output IS the key, and both halves are
/// in the clear, so it adds nothing an on-path observer cannot compute — the reason to run it anyway is
/// that one derivation and one frame sequence then cover a configuration decision neither end puts on
/// the wire. With a secret it is the nonce [`derive_session_key`] folds the secret into.
///
/// **The order of the two arguments is part of the wire contract.** Swapping them at one end derives a
/// different key from the same join, every datagram fails its tag, and nothing in the failure says why.
/// Two keyed passes, under [`JOIN_LABEL_LOW`] and [`JOIN_LABEL_HIGH`], joined low half first — the same
/// construction [`compress_secret`] uses, for the same reason: [`SipHasher`] keys on exactly [`KEY_LEN`]
/// bytes and something has to produce those 16.
///
/// It cannot add entropy. Two halves of 128 bits fold to 128 bits, and no key derived from the result
/// is worth more than that.
#[must_use]
pub fn session_nonce(joiner: &[u8; KEY_LEN], acceptor: &[u8; KEY_LEN]) -> [u8; KEY_LEN] {
    let mut low = SipHasher::new(&JOIN_LABEL_LOW);
    low.write(joiner);
    low.write(acceptor);
    let mut high = SipHasher::new(&JOIN_LABEL_HIGH);
    high.write(joiner);
    high.write(acceptor);
    join_halves(low.finish(), high.finish())
}

/// The session key for one join, derived from the shared secret and that join's session nonce.
///
/// `secret` is the 16 bytes [`compress_secret`] folded out of whatever the game distributes. `nonce` is
/// the 16 bytes [`session_nonce`] folded out of the two halves that join exchanged, one drawn by each
/// end. Both sides run this function over the same two inputs and arrive at the same key. An observer
/// reading the exchange learns both halves and nothing else.
///
/// **The secret is an input and is never seated as the key**, however much shorter that implementation
/// looks. The reason is the sequence numbers:
///
/// - [`SessionAuth::new`] starts every session's counter at 1, and [`ReplayWindow`] only ever knows the
///   session in front of it.
/// - So under a key that does not change between joins, every datagram captured in one session is a
///   valid, unreplayed datagram in the next. The replay defense would last exactly one session.
/// - A fresh nonce per join is what keeps the key fresh per join, and that is the only reason the
///   nonce exists. A caller that reuses a nonce under one secret gets the constant-key failure back,
///   which is why [`session_nonce`] draws a half at each end rather than taking the joiner's word.
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

/// The key one session's payloads are **encrypted** under, from the shared secret and that join's
/// session nonce.
///
/// The twin of [`derive_session_key`] over the same two inputs, under labels of its own, and it exists
/// only where a secret does: with no secret there is nothing to derive from that an observer of the
/// join does not already hold, so [`SessionAuth`] carries no cipher and every payload stays in the
/// clear. [`SessionAuth::seal`] and [`SessionAuth::open`] are what run it.
///
/// **Separate labels rather than a second use of the MAC key.** Reusing one key across a MAC and a
/// cipher is the mistake this construction is shaped to refuse, and deriving both from the secret under
/// distinct labels costs four more SipHash passes per join — one per word of [`CIPHER_KEY_LABELS`] —
/// and nothing per datagram.
///
/// **It expands to 32 bytes and cannot expand the entropy.** The inputs are 16 bytes of compressed
/// secret and 16 of folded nonce, so the cipher key is worth the 128 bits of the weaker of them —
/// which is the secret's own entropy, the same ceiling [`derive_session_key`] has. ChaCha20 takes no
/// shorter key, so something has to produce the 32 bytes, and this is it.
#[must_use]
pub fn derive_cipher_key(secret: &[u8; KEY_LEN], nonce: &[u8; KEY_LEN]) -> [u8; CIPHER_KEY_LEN] {
    let mut key = [0u8; CIPHER_KEY_LEN];
    for (word, label) in CIPHER_KEY_LABELS.iter().enumerate() {
        let mut hasher = SipHasher::new(secret);
        hasher.write(label);
        hasher.write(nonce);
        key[word * 8..word * 8 + 8].copy_from_slice(&hasher.finish().to_le_bytes());
    }
    key
}

/// The ChaCha20-Poly1305 nonce for one datagram: the direction byte, then the sequence number.
///
/// **An AEAD nonce must never repeat under one key, and what makes this one unique is not new.** The
/// two properties it rests on are both already here and both already tested:
///
/// - [`SessionAuth::seal`] issues each sequence number once and **refuses to wrap** — see the module
///   header. That refusal was written for the replay window; it is now also what keeps a nonce from
///   coming round again.
/// - The two directions of a session share one key, so [`Direction`] separates them. Without it the
///   client's datagram 5 and the server's would encrypt under one nonce, and an observer would hold
///   the exclusive-or of the two payloads.
///
/// The remaining seven bytes are zero and reserved. They are not a counter: a session that needs more
/// than [`u32::MAX`] datagrams in one direction is refused rather than widened, because widening here
/// would silently re-open the replay window the same refusal protects.
fn datagram_nonce(direction: Direction, seq: u32) -> [u8; CIPHER_NONCE_LEN] {
    let mut nonce = [0u8; CIPHER_NONCE_LEN];
    nonce[0] = direction as u8;
    nonce[1..1 + SEQ_LEN].copy_from_slice(&seq.to_le_bytes());
    nonce
}

/// The associated data Poly1305 covers beside the ciphertext: the sequence number, then the direction.
///
/// **The sequence number is on the wire in the clear because the receiver needs it before it has a
/// plaintext** — it is half of the nonce the decryption runs under. Naming it here as well is what
/// makes altering it a tag failure rather than a decryption against the wrong nonce, and it keeps the
/// authenticated input the same shape [`tag_over`] signs under the clear regime: payload, sequence,
/// direction.
fn datagram_aad(direction: Direction, seq: u32) -> [u8; SEQ_LEN + 1] {
    let mut aad = [0u8; SEQ_LEN + 1];
    aad[..SEQ_LEN].copy_from_slice(&seq.to_le_bytes());
    aad[SEQ_LEN] = direction as u8;
    aad
}

/// Proof the sender holds `secret`: a tag over the nonce and the protocol version.
///
/// `key` is the output of [`derive_session_key`] and `nonce` the [`session_nonce`] it was derived from,
/// so producing this tag requires the secret the key was derived from AND both halves of that join's
/// nonce. The joining side sends it once the acceptor's half has arrived; the accepting side derives its
/// own key from its own copy of the secret, recomputes the tag, and refuses the join when the two
/// differ. Without it a peer that does not hold the secret is still refused, but only once it has sent a
/// datagram whose tag fails — it occupies a session slot until then.
///
/// **It is a tag over the JOINT nonce, which is what makes it unreplayable.** The acceptor's half is
/// drawn per join, so a tag captured from a recorded join recomputes against nothing the acceptor will
/// ever ask again.
///
/// **The protocol version is inside the tag** so that a confirmation cannot be lifted out of a session
/// of one protocol version and replayed into a session of another, where the fields it authorizes mean
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

// ---------------------------------------------------------------------------------------------
// The key exchange
// ---------------------------------------------------------------------------------------------

/// Bytes of an X25519 key, secret or public. Both are 32.
pub const EXCHANGE_KEY_LEN: usize = 32;

/// Domain label keying the low half of the exchange fold. Exactly [`KEY_LEN`] bytes, as a SipHash key.
pub const EXCHANGE_LABEL_LOW: [u8; KEY_LEN] = *b"orbitnet-exch-lo";

/// Domain label keying the high half of the exchange fold. Exactly [`KEY_LEN`] bytes, as a SipHash key.
pub const EXCHANGE_LABEL_HIGH: [u8; KEY_LEN] = *b"orbitnet-exch-hi";

/// Domain label prefixing the low half of [`fold_secrets`].
pub const MIXED_SECRET_LABEL_LOW: &[u8] = b"orbitnet-mixed-lo";

/// Domain label prefixing the high half of [`fold_secrets`].
pub const MIXED_SECRET_LABEL_HIGH: &[u8] = b"orbitnet-mixed-hi";

/// The public key matching an X25519 secret.
///
/// The acceptor publishes this and the joiner pins it; the two ephemerals put theirs on the wire. A
/// secret is 32 bytes of any value — X25519 clamps the scalar rather than rejecting one — so this is
/// total and a game may hand it whatever its CSPRNG produced.
#[must_use]
pub fn exchange_public_key(secret: &[u8; EXCHANGE_KEY_LEN]) -> [u8; EXCHANGE_KEY_LEN] {
    PublicKey::from(&StaticSecret::from(*secret)).to_bytes()
}

/// The exchange's contribution to the session key, computed by the **joiner**.
///
/// `ephemeral` is this join's ephemeral secret, `pinned_static` the acceptor's long-lived public key
/// this client trusts, and `acceptor_ephemeral_public` the half the [`crate::codec::Challenge`]
/// carried. `None` means the exchange produced nothing usable and the join must be refused; see
/// [`exchange_fold`] for which inputs cause it.
#[must_use]
pub fn joiner_exchange_secret(
    ephemeral: &[u8; EXCHANGE_KEY_LEN],
    pinned_static: &[u8; EXCHANGE_KEY_LEN],
    acceptor_ephemeral_public: &[u8; EXCHANGE_KEY_LEN],
) -> Option<[u8; KEY_LEN]> {
    let ephemeral = StaticSecret::from(*ephemeral);
    let joiner_public = PublicKey::from(&ephemeral).to_bytes();
    let to_static = ephemeral.diffie_hellman(&PublicKey::from(*pinned_static));
    let to_ephemeral = ephemeral.diffie_hellman(&PublicKey::from(*acceptor_ephemeral_public));
    exchange_fold(
        pinned_static,
        &joiner_public,
        acceptor_ephemeral_public,
        &to_static,
        &to_ephemeral,
    )
}

/// The exchange's contribution to the session key, computed by the **acceptor**.
///
/// `static_secret` is the long-lived secret whose public half joiners pin, `ephemeral` this join's
/// ephemeral secret, and `joiner_ephemeral_public` the half the opening
/// [`crate::codec::Handshake`] carried. `None` has the same meaning as on the joiner's side.
#[must_use]
pub fn acceptor_exchange_secret(
    static_secret: &[u8; EXCHANGE_KEY_LEN],
    ephemeral: &[u8; EXCHANGE_KEY_LEN],
    joiner_ephemeral_public: &[u8; EXCHANGE_KEY_LEN],
) -> Option<[u8; KEY_LEN]> {
    let static_secret = StaticSecret::from(*static_secret);
    let ephemeral = StaticSecret::from(*ephemeral);
    let static_public = PublicKey::from(&static_secret).to_bytes();
    let acceptor_public = PublicKey::from(&ephemeral).to_bytes();
    let joiner = PublicKey::from(*joiner_ephemeral_public);
    let to_static = static_secret.diffie_hellman(&joiner);
    let to_ephemeral = ephemeral.diffie_hellman(&joiner);
    exchange_fold(
        &static_public,
        joiner_ephemeral_public,
        &acceptor_public,
        &to_static,
        &to_ephemeral,
    )
}

/// Both shared values and the three public keys, folded to [`KEY_LEN`] bytes.
///
/// **Two Diffie-Hellman values, and they answer different questions.**
///
/// | Value | Between | What it is for |
/// | --- | --- | --- |
/// | `to_static` | the joiner's ephemeral and the acceptor's **pinned static** key | authentication: only the holder of the pinned key's secret half can compute it |
/// | `to_ephemeral` | the two ephemerals | forward secrecy: a static secret leaked later does not recover a recorded join's key |
///
/// **All three public keys are folded in with them**, which binds the result to the transcript. An
/// attacker that substitutes an ephemeral in flight makes the two ends fold different bytes, so the
/// keys differ and the confirmation fails; an attacker that swapped one without this would have both
/// ends agree on a key derived from values only one of them chose.
///
/// **A non-contributory result is refused.** The all-zero public key, and the other low-order points,
/// force a shared value of zero whatever the other side's secret was — so a peer that reads it as a key
/// is keying on a value the attacker chose. `was_contributory` is the constant-time check for it, and
/// either value failing refuses the whole exchange.
///
/// **The fold is SipHash and not a standard KDF**, the same two-pass construction
/// [`compress_secret`] uses on a game-supplied secret, with the labels as keys and the transcript as the
/// message. It is not HKDF and claims nothing HKDF claims. What bounds this exchange is downstream of
/// it: the key is 128 bits and the tag it protects is 64, so a wider fold would change no number a
/// reader of this module cares about, and it would cost a hash dependency to produce.
fn exchange_fold(
    static_public: &[u8; EXCHANGE_KEY_LEN],
    joiner_public: &[u8; EXCHANGE_KEY_LEN],
    acceptor_public: &[u8; EXCHANGE_KEY_LEN],
    to_static: &SharedSecret,
    to_ephemeral: &SharedSecret,
) -> Option<[u8; KEY_LEN]> {
    if !to_static.was_contributory() || !to_ephemeral.was_contributory() {
        return None;
    }
    let mut low = SipHasher::new(&EXCHANGE_LABEL_LOW);
    let mut high = SipHasher::new(&EXCHANGE_LABEL_HIGH);
    for hasher in [&mut low, &mut high] {
        hasher.write(static_public);
        hasher.write(joiner_public);
        hasher.write(acceptor_public);
        hasher.write(to_static.as_bytes());
        hasher.write(to_ephemeral.as_bytes());
    }
    Some(join_halves(low.finish(), high.finish()))
}

/// The one secret [`derive_session_key`] takes, from the two a session may hold.
///
/// `supplied` is [`compress_secret`]'s output for a secret the game distributed; `exchanged` is
/// [`joiner_exchange_secret`] or [`acceptor_exchange_secret`]. Both are optional and independent, so
/// there are four cases:
///
/// | `supplied` | `exchanged` | Result |
/// | --- | --- | --- |
/// | `None` | `None` | `None` — the key is the session nonce alone, and an on-path observer holds it |
/// | `Some` | `None` | the supplied secret |
/// | `None` | `Some` | the exchange's output |
/// | `Some` | `Some` | a fold of the two, keyed on the supplied secret |
///
/// **Configuring both is at least as strong as either**, because the fold is a keyed PRF of one over
/// the other: an attacker has to hold the supplied secret AND break the exchange. A game with a lobby
/// token and a published server key does not have to choose.
#[must_use]
pub fn fold_secrets(
    supplied: Option<&[u8; KEY_LEN]>,
    exchanged: Option<&[u8; KEY_LEN]>,
) -> Option<[u8; KEY_LEN]> {
    match (supplied, exchanged) {
        (None, None) => None,
        (Some(supplied), None) => Some(*supplied),
        (None, Some(exchanged)) => Some(*exchanged),
        (Some(supplied), Some(exchanged)) => {
            let mut low = SipHasher::new(supplied);
            low.write(MIXED_SECRET_LABEL_LOW);
            low.write(exchanged);
            let mut high = SipHasher::new(supplied);
            high.write(MIXED_SECRET_LABEL_HIGH);
            high.write(exchanged);
            Some(join_halves(low.finish(), high.finish()))
        }
    }
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

/// One session's authentication state: the shared key, this side's send counter, the window that
/// refuses a repeat from the other side, and the payload cipher when the game configured a secret.
///
/// Both directions of a session use one key and one [`Direction`] tells them apart, so a peer holds
/// exactly one of these per session — a client one for the server, a server one per connected peer.
///
/// **The cipher key is an `Option` of 32 plain bytes rather than a constructed cipher**, so this stays
/// [`Copy`] and a peer table keeps holding it by value. The cipher is built per datagram from those
/// bytes; ChaCha20's key schedule is the key itself, so there is nothing to amortize.
#[derive(Debug, Clone, Copy)]
pub struct SessionAuth {
    key: [u8; KEY_LEN],
    cipher_key: Option<[u8; CIPHER_KEY_LEN]>,
    next_seq: u32,
    window: ReplayWindow,
}

impl SessionAuth {
    /// A session under `key`, having sent and received nothing, whose payloads are **in the clear**.
    ///
    /// This is the no-secret regime and it is byte-for-byte what it has always been: the SipHash tag,
    /// the 32-bit sequence number, and a payload an observer reads. Encrypting here would buy nothing —
    /// both halves of the nonce the key is folded from are on the wire, so anyone who can read the
    /// payload can compute the key that hid it.
    #[must_use]
    pub fn new(key: [u8; KEY_LEN]) -> Self {
        Self {
            key,
            cipher_key: None,
            next_seq: 1,
            window: ReplayWindow::new(),
        }
    }

    /// A session whose payloads are **encrypted** under `cipher_key`, and whose datagrams are
    /// authenticated by that cipher's own tag rather than by `key`.
    ///
    /// Both keys come from the session secret: [`derive_session_key`] and [`derive_cipher_key`] over
    /// the same folded nonce, under labels of their own. `key` is kept because it is what the two ends
    /// compare a repeated handshake against — see [`Self::key`] — and it is what [`confirm_tag`] was
    /// taken under.
    #[must_use]
    pub fn encrypted(key: [u8; KEY_LEN], cipher_key: [u8; CIPHER_KEY_LEN]) -> Self {
        Self {
            key,
            cipher_key: Some(cipher_key),
            next_seq: 1,
            window: ReplayWindow::new(),
        }
    }

    /// The session key, for comparing a repeated handshake against the one already seated.
    #[must_use]
    pub fn key(&self) -> [u8; KEY_LEN] {
        self.key
    }

    /// Whether this session's payloads are encrypted, which is exactly whether a secret is configured.
    #[must_use]
    pub fn encrypts(&self) -> bool {
        self.cipher_key.is_some()
    }

    /// Bytes [`Self::seal`] appends past the payload, which the two regimes disagree about.
    #[must_use]
    pub fn trailer_len(&self) -> usize {
        if self.encrypts() {
            CIPHER_TRAILER_LEN
        } else {
            TRAILER_LEN
        }
    }

    /// Whether this session's send counter is spent. A spent session can still receive.
    #[must_use]
    pub fn exhausted(&self) -> bool {
        self.next_seq == 0
    }

    /// Turn `payload` into a datagram, consuming one sequence number.
    ///
    /// | Regime | What `payload` becomes |
    /// | --- | --- |
    /// | no secret | payload ‖ sequence ‖ 8-byte SipHash tag |
    /// | a secret | ciphertext ‖ sequence ‖ 16-byte Poly1305 tag |
    ///
    /// `None` means the counter is spent (see the module header) and the datagram must not be sent —
    /// wrapping it would re-open the replay window on everything captured in the first pass, and under
    /// a secret it would repeat an AEAD nonce, which is worse. `payload` may have been rewritten by the
    /// time `None` comes back, so a refused datagram is dropped rather than retried.
    pub fn seal(&mut self, direction: Direction, payload: &mut Vec<u8>) -> Option<()> {
        if self.next_seq == 0 {
            return None;
        }
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        let Some(cipher_key) = self.cipher_key else {
            payload.extend_from_slice(&seq.to_le_bytes());
            let tag = tag_over(&self.key, payload, direction);
            payload.extend_from_slice(&tag.to_le_bytes());
            return Some(());
        };
        let cipher = ChaCha20Poly1305::new(&cipher_key.into());
        let tag = cipher
            .encrypt_inout_detached(
                &datagram_nonce(direction, seq).into(),
                &datagram_aad(direction, seq),
                InOutBuf::from(payload.as_mut_slice()),
            )
            .ok()?;
        payload.extend_from_slice(&seq.to_le_bytes());
        payload.extend_from_slice(&tag);
        Some(())
    }

    /// Verify `datagram` as arriving in `direction`, answering the payload with the trailer stripped.
    ///
    /// **The three checks run in one order under both regimes: authenticate, then replay, then decode.**
    /// Getting that order wrong is the failure this design is known for.
    ///
    /// - **Nothing is decrypted or returned before the tag verifies.** Under a secret the Poly1305 tag
    ///   is taken over the ciphertext, so it is checked before a byte is deciphered, and a datagram that
    ///   fails it leaves `plain` empty rather than holding unauthenticated plaintext for a later caller
    ///   to find.
    /// - **The replay window is advanced only on a datagram that authenticated**, so an attacker cannot
    ///   burn sequence numbers the real peer has yet to send. Under a secret the sequence number the
    ///   window is handed is one Poly1305 covered as associated data, so it is the sender's own.
    /// - **The caller decodes nothing until this returns `Ok`.**
    ///
    /// `plain` is scratch the caller owns and reuses. With no secret it is untouched and the answer
    /// borrows `datagram` directly; under a secret the plaintext is written there and the answer
    /// borrows that. One signature covers both so that no call site can pick the wrong one.
    pub fn open<'a>(
        &mut self,
        direction: Direction,
        datagram: &'a [u8],
        plain: &'a mut Vec<u8>,
    ) -> Result<&'a [u8], AuthError> {
        let Some(cipher_key) = self.cipher_key else {
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
            return Ok(&signed[..payload_len]);
        };
        if datagram.len() < CIPHER_TRAILER_LEN {
            return Err(AuthError::Truncated);
        }
        let split = datagram.len() - CIPHER_TRAILER_LEN;
        let (ciphertext, trailer) = datagram.split_at(split);
        let seq = u32::from_le_bytes(trailer[..SEQ_LEN].try_into().unwrap_or([0; SEQ_LEN]));
        let mut tag = [0u8; CIPHER_TAG_LEN];
        tag.copy_from_slice(&trailer[SEQ_LEN..]);
        plain.clear();
        plain.extend_from_slice(ciphertext);
        let cipher = ChaCha20Poly1305::new(&cipher_key.into());
        let verified = cipher.decrypt_inout_detached(
            &datagram_nonce(direction, seq).into(),
            &datagram_aad(direction, seq),
            InOutBuf::from(plain.as_mut_slice()),
            (&tag).into(),
        );
        if verified.is_err() {
            plain.clear();
            return Err(AuthError::BadTag);
        }
        if !self.window.accept(seq) {
            plain.clear();
            return Err(AuthError::Replayed);
        }
        Ok(plain.as_slice())
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
/// A connection sends one block per owned body per tick, and the send path caps a frame at one
/// datagram — but **the datagram cap alone does not keep a frame under this one**. A one-property
/// input schema packs a block into 9 bytes, so a hundred owned bodies fit inside a single frame and
/// the byte budget refuses none of them. `admit_input_blocks` therefore enforces this count as a
/// second cap and rotates what it refuses, so the tail rides the next tick rather than being
/// truncated at the same index for ever.
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

    /// A fixed stand-in for the acceptor's half of a join's nonce, distinct from [`PIN_NONCE`] so a
    /// fold that ignored one of its two arguments cannot pass the vectors below.
    const PIN_ACCEPTOR: [u8; KEY_LEN] = [
        0x50, 0x51, 0x52, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x5b, 0x5c, 0x5d, 0x5e,
        0x5f,
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
    fn a_replayed_joiner_half_derives_a_key_the_replayer_cannot_compute() {
        // THE WHOLE POINT OF THE ACCEPTOR'S HALF, and the test that fails against a derivation keyed on
        // the joiner's nonce alone. An observer that recorded a join presents the joiner's half again;
        // the acceptor draws a fresh half of its own, and the session that comes out is keyed on bytes
        // the observer never saw. Under the old scheme the two keys below were one key, and every
        // datagram the observer had captured verified in the session it had just opened.
        let secret = compress_secret(PIN_SECRET);
        let recorded = derive_session_key(&secret, &session_nonce(&PIN_NONCE, &PIN_ACCEPTOR));
        let mut replayed_acceptor = PIN_ACCEPTOR;
        replayed_acceptor[0] ^= 0x01;
        let replayed = derive_session_key(&secret, &session_nonce(&PIN_NONCE, &replayed_acceptor));
        assert_ne!(
            recorded, replayed,
            "the same joiner half, a fresh acceptor half"
        );

        let mut captured = b"input for tick 1".to_vec();
        let mut plain: Vec<u8> = Vec::new();
        SessionAuth::new(recorded)
            .seal(Direction::ToServer, &mut captured)
            .unwrap();
        assert_eq!(
            SessionAuth::new(replayed).open(Direction::ToServer, &captured, &mut plain),
            Err(AuthError::BadTag),
            "the replayed join refuses what the recorded one sealed"
        );
        assert!(
            SessionAuth::new(recorded)
                .open(Direction::ToServer, &captured, &mut plain)
                .is_ok(),
            "the negative control: it opens under the join that sealed it"
        );
        // And the confirmation goes the same way, which is what refuses the replay AT THE HANDSHAKE
        // rather than after the first failed tag.
        let joint = session_nonce(&PIN_NONCE, &PIN_ACCEPTOR);
        let replayed_joint = session_nonce(&PIN_NONCE, &replayed_acceptor);
        assert_ne!(
            confirm_tag(&recorded, &joint, PIN_VERSION),
            confirm_tag(&replayed, &replayed_joint, PIN_VERSION)
        );
    }

    #[test]
    fn the_session_nonce_folds_every_byte_of_both_halves() {
        // Neither half may be ignored, and a fold that dropped one would still pass a test that only
        // varied the other. Both sweeps, and the negative control that the same pair repeats.
        let base = session_nonce(&PIN_NONCE, &PIN_ACCEPTOR);
        assert_eq!(session_nonce(&PIN_NONCE, &PIN_ACCEPTOR), base);
        for index in 0..KEY_LEN {
            let mut joiner = PIN_NONCE;
            joiner[index] ^= 0x01;
            assert_ne!(
                session_nonce(&joiner, &PIN_ACCEPTOR),
                base,
                "joiner byte {index}"
            );
            let mut acceptor = PIN_ACCEPTOR;
            acceptor[index] ^= 0x01;
            assert_ne!(
                session_nonce(&PIN_NONCE, &acceptor),
                base,
                "acceptor byte {index}"
            );
        }
    }

    #[test]
    fn the_session_nonce_is_order_sensitive_and_never_returns_a_half() {
        // The argument order is part of the wire contract: an end that swapped them derives a different
        // key from the same join and nothing it sends opens. It is checked here because the two halves
        // have the same type, so nothing but this notices the swap.
        assert_ne!(
            session_nonce(&PIN_NONCE, &PIN_ACCEPTOR),
            session_nonce(&PIN_ACCEPTOR, &PIN_NONCE)
        );
        // Nor is either half handed back, which is the fold's own version of the trap
        // `derive_session_key` names: returning one argument would put the joiner back in sole charge.
        let folded = session_nonce(&PIN_NONCE, &PIN_ACCEPTOR);
        assert_ne!(folded, PIN_NONCE);
        assert_ne!(folded, PIN_ACCEPTOR);
        // The all-zero pair folds to a value like any other. It is refused at the handshake rather than
        // here, because this function cannot tell a drawn zero from an absent field.
        assert_ne!(session_nonce(&[0; KEY_LEN], &[0; KEY_LEN]), [0; KEY_LEN]);
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
        // The fold of the two halves, and the key and confirmation a whole join actually lands on. The
        // vectors above cover the secret and the key; these cover the step in front of them, so a
        // refactor that reordered the halves or changed a join label fails here rather than in the
        // field.
        let joint = session_nonce(&PIN_NONCE, &PIN_ACCEPTOR);
        assert_eq!(
            joint,
            [
                0x27, 0xb2, 0xc9, 0x6b, 0x3b, 0x19, 0x39, 0x72, 0x48, 0x50, 0x6d, 0xc6, 0x15, 0x33,
                0xc7, 0xbc
            ]
        );
        let joined_key = derive_session_key(&secret, &joint);
        assert_eq!(
            joined_key,
            [
                0xa9, 0x1b, 0x93, 0x7c, 0xd4, 0x18, 0x03, 0xb5, 0x33, 0x62, 0x18, 0x3e, 0x70, 0x6c,
                0x9f, 0xde
            ]
        );
        assert_eq!(
            confirm_tag(&joined_key, &joint, PIN_VERSION),
            0x87a5_f3a0_d6d3_6f7a
        );
    }

    #[test]
    fn a_session_under_a_derived_key_carries_both_directions_and_no_other_derivation() {
        let mut plain: Vec<u8> = Vec::new();
        let secret = compress_secret(PIN_SECRET);
        let key = derive_session_key(&secret, &PIN_NONCE);
        let mut client = SessionAuth::new(key);
        let mut server = SessionAuth::new(key);
        let mut up = b"input".to_vec();
        client.seal(Direction::ToServer, &mut up).unwrap();
        assert_eq!(
            server.open(Direction::ToServer, &up, &mut plain),
            Ok(&b"input"[..])
        );
        let mut down = b"snapshot".to_vec();
        server.seal(Direction::ToClient, &mut down).unwrap();
        assert_eq!(
            client.open(Direction::ToClient, &down, &mut plain),
            Ok(&b"snapshot"[..])
        );
        // The next join under the same secret takes a fresh nonce, and the previous join's datagrams
        // do not open under it.
        let mut next_nonce = PIN_NONCE;
        next_nonce[0] ^= 0x01;
        let mut next_join = SessionAuth::new(derive_session_key(&secret, &next_nonce));
        assert_eq!(
            next_join.open(Direction::ToServer, &up, &mut plain),
            Err(AuthError::BadTag)
        );
        // Nor does a peer deriving from a different secret over the same nonce open them.
        let stranger_key = derive_session_key(&compress_secret(b"another secret"), &PIN_NONCE);
        let mut stranger = SessionAuth::new(stranger_key);
        assert_eq!(
            stranger.open(Direction::ToServer, &up, &mut plain),
            Err(AuthError::BadTag)
        );
    }

    // ---------------------------------------------------------------------------------------------
    // The payload cipher. Everything below needs a session secret; with none configured the suites
    // above are the whole of the behavior, which is what
    // `with_no_session_secret_the_wire_is_byte_for_byte_what_it_was` pins.
    // ---------------------------------------------------------------------------------------------

    /// The cipher key one pinned join derives, written out so that a change to the derivation has to
    /// change this array. A peer on the old bytes decrypts nothing a peer on the new ones sent, and
    /// the failure says only `BadTag`.
    #[test]
    fn derive_cipher_key_is_pinned_and_separate_from_the_mac_key() {
        let secret = compress_secret(PIN_SECRET);
        let nonce = session_nonce(&PIN_NONCE, &PIN_ACCEPTOR);
        let cipher_key = derive_cipher_key(&secret, &nonce);
        assert_eq!(
            cipher_key,
            [
                0xae, 0xba, 0x99, 0x8f, 0x40, 0x3b, 0xd3, 0x77, 0xe3, 0xda, 0xff, 0x61, 0x7d, 0x1a,
                0x54, 0x48, 0x36, 0x21, 0x1f, 0xa6, 0x7f, 0xeb, 0x0b, 0x72, 0x3b, 0x55, 0x61, 0x14,
                0xb2, 0x7c, 0x04, 0xb0
            ]
        );
        // The key that hides a payload and the key that authenticated it are separate values. Both
        // halves are checked, because a derivation that reused the MAC key for the low 16 bytes would
        // pass a test that only looked at the high ones.
        let mac_key = derive_session_key(&secret, &nonce);
        assert_ne!(cipher_key[..KEY_LEN], mac_key[..]);
        assert_ne!(cipher_key[KEY_LEN..], mac_key[..]);
        // And the four words are four different passes rather than one repeated.
        for word in 1..4 {
            assert_ne!(
                cipher_key[..8],
                cipher_key[word * 8..word * 8 + 8],
                "word 0 against word {word}"
            );
        }
    }

    #[test]
    fn a_different_secret_or_a_different_join_derives_a_different_cipher_key() {
        let secret = compress_secret(PIN_SECRET);
        let nonce = session_nonce(&PIN_NONCE, &PIN_ACCEPTOR);
        let base = derive_cipher_key(&secret, &nonce);
        assert_ne!(
            base,
            derive_cipher_key(&compress_secret(b"another secret"), &nonce)
        );
        let mut other_acceptor = PIN_ACCEPTOR;
        other_acceptor[0] ^= 0x01;
        assert_ne!(
            base,
            derive_cipher_key(&secret, &session_nonce(&PIN_NONCE, &other_acceptor))
        );
    }

    /// The whole point of the feature: a payload an observer could read is one it cannot.
    #[test]
    fn a_session_secret_puts_the_payload_on_the_wire_as_ciphertext() {
        let secret = compress_secret(PIN_SECRET);
        let nonce = session_nonce(&PIN_NONCE, &PIN_ACCEPTOR);
        let plaintext = b"net_pos 12.5 0.0 -3.25, net_orient ...";
        let mut datagram = plaintext.to_vec();
        let mut tx = SessionAuth::encrypted(
            derive_session_key(&secret, &nonce),
            derive_cipher_key(&secret, &nonce),
        );
        assert!(tx.encrypts());
        assert_eq!(tx.trailer_len(), CIPHER_TRAILER_LEN);
        tx.seal(Direction::ToServer, &mut datagram).unwrap();

        assert_eq!(datagram.len(), plaintext.len() + CIPHER_TRAILER_LEN);
        assert_ne!(&datagram[..plaintext.len()], &plaintext[..]);
        assert!(
            !datagram.windows(7).any(|w| w == b"net_pos"),
            "no run of the plaintext survives on the wire"
        );

        let mut plain: Vec<u8> = Vec::new();
        let mut rx = SessionAuth::encrypted(
            derive_session_key(&secret, &nonce),
            derive_cipher_key(&secret, &nonce),
        );
        assert_eq!(
            rx.open(Direction::ToServer, &datagram, &mut plain),
            Ok(&plaintext[..])
        );
    }

    /// The pinned datagram, so that a port of this protocol has bytes to reproduce and a change to
    /// the nonce layout, the associated data or the trailer order fails here rather than in a session.
    #[test]
    fn an_encrypted_datagram_is_pinned_to_its_bytes() {
        let secret = compress_secret(PIN_SECRET);
        let nonce = session_nonce(&PIN_NONCE, &PIN_ACCEPTOR);
        let mut datagram = b"input".to_vec();
        SessionAuth::encrypted(
            derive_session_key(&secret, &nonce),
            derive_cipher_key(&secret, &nonce),
        )
        .seal(Direction::ToServer, &mut datagram)
        .unwrap();
        assert_eq!(
            datagram,
            vec![
                0xe1, 0x71, 0x1d, 0x5c, 0x3f, 0x01, 0x00, 0x00, 0x00, 0xcc, 0x55, 0xf2, 0x29, 0x8c,
                0x35, 0xbc, 0x61, 0x70, 0xb1, 0x99, 0x21, 0x00, 0xbb, 0xff, 0x2d
            ]
        );
        // The sequence number is the fifth through eighth bytes and is readable: a receiver needs it
        // before it holds a plaintext, because it is half of the nonce.
        assert_eq!(&datagram[5..9], &1u32.to_le_bytes());
    }

    /// **With no secret the wire is what it was**, which is the claim this change rests on. Sealing a
    /// payload under [`SessionAuth::new`] produces exactly the bytes, the sequence number and the
    /// SipHash tag it always did, and the payload is still readable in the datagram.
    #[test]
    fn with_no_session_secret_the_wire_is_byte_for_byte_what_it_was() {
        let mut tx = SessionAuth::new(REF_KEY);
        assert!(!tx.encrypts());
        assert_eq!(tx.trailer_len(), TRAILER_LEN);
        let mut datagram = b"input".to_vec();
        tx.seal(Direction::ToServer, &mut datagram).unwrap();

        let mut expected = b"input".to_vec();
        expected.extend_from_slice(&1u32.to_le_bytes());
        let tag = siphash24(
            &REF_KEY,
            &[b"input".as_slice(), &1u32.to_le_bytes(), &[0x01]].concat(),
        );
        expected.extend_from_slice(&tag.to_le_bytes());
        assert_eq!(datagram, expected);
        assert_eq!(&datagram[..5], b"input", "and the payload is in the clear");
    }

    /// One key covers both directions, so the direction byte has to reach the nonce as well as the
    /// tag. Without it the two flows encrypt under one keystream and an observer holding both
    /// datagrams holds the exclusive-or of their payloads.
    #[test]
    fn the_two_directions_never_share_a_nonce() {
        assert_ne!(
            datagram_nonce(Direction::ToServer, 7),
            datagram_nonce(Direction::ToClient, 7)
        );
        let secret = compress_secret(PIN_SECRET);
        let nonce = session_nonce(&PIN_NONCE, &PIN_ACCEPTOR);
        let key = derive_session_key(&secret, &nonce);
        let cipher_key = derive_cipher_key(&secret, &nonce);

        let mut up = b"the same payload".to_vec();
        SessionAuth::encrypted(key, cipher_key)
            .seal(Direction::ToServer, &mut up)
            .unwrap();
        let mut down = b"the same payload".to_vec();
        SessionAuth::encrypted(key, cipher_key)
            .seal(Direction::ToClient, &mut down)
            .unwrap();
        // Both are sequence 1 under one key. The ciphertexts must differ, or the keystream repeated.
        assert_eq!(&up[16..20], &down[16..20], "the same sequence number");
        assert_ne!(&up[..16], &down[..16]);
    }

    #[test]
    fn each_datagram_of_one_direction_takes_its_own_nonce() {
        assert_ne!(
            datagram_nonce(Direction::ToServer, 1),
            datagram_nonce(Direction::ToServer, 2)
        );
        let secret = compress_secret(PIN_SECRET);
        let nonce = session_nonce(&PIN_NONCE, &PIN_ACCEPTOR);
        let mut tx = SessionAuth::encrypted(
            derive_session_key(&secret, &nonce),
            derive_cipher_key(&secret, &nonce),
        );
        let mut first = b"the same payload".to_vec();
        tx.seal(Direction::ToServer, &mut first).unwrap();
        let mut second = b"the same payload".to_vec();
        tx.seal(Direction::ToServer, &mut second).unwrap();
        assert_ne!(&first[..16], &second[..16]);
    }

    /// The sequence number rides in the clear, so it is the field an attacker can reach. Poly1305
    /// covers it as associated data, which is what makes altering it a refusal rather than a
    /// decryption under a nonce nobody sealed with.
    #[test]
    fn an_altered_sequence_number_is_refused_rather_than_decrypted() {
        let secret = compress_secret(PIN_SECRET);
        let nonce = session_nonce(&PIN_NONCE, &PIN_ACCEPTOR);
        let key = derive_session_key(&secret, &nonce);
        let cipher_key = derive_cipher_key(&secret, &nonce);
        let mut datagram = b"input".to_vec();
        SessionAuth::encrypted(key, cipher_key)
            .seal(Direction::ToServer, &mut datagram)
            .unwrap();

        let mut plain: Vec<u8> = Vec::new();
        for index in 5..5 + SEQ_LEN {
            let mut altered = datagram.clone();
            altered[index] ^= 0x01;
            let mut rx = SessionAuth::encrypted(key, cipher_key);
            assert_eq!(
                rx.open(Direction::ToServer, &altered, &mut plain),
                Err(AuthError::BadTag),
                "sequence byte {index}"
            );
        }
        // And every other byte of the datagram goes the same way: ciphertext and tag alike.
        for index in 0..datagram.len() {
            let mut altered = datagram.clone();
            altered[index] ^= 0x01;
            let mut rx = SessionAuth::encrypted(key, cipher_key);
            assert_eq!(
                rx.open(Direction::ToServer, &altered, &mut plain),
                Err(AuthError::BadTag),
                "byte {index}"
            );
            assert!(plain.is_empty(), "no plaintext survives a refusal");
        }
    }

    /// The ordering this design is known for getting wrong. Refusing after decoding, or advancing the
    /// window before the tag verifies, are both reachable from here.
    #[test]
    fn a_forged_ciphertext_burns_no_sequence_number_and_leaves_no_plaintext() {
        let secret = compress_secret(PIN_SECRET);
        let nonce = session_nonce(&PIN_NONCE, &PIN_ACCEPTOR);
        let key = derive_session_key(&secret, &nonce);
        let cipher_key = derive_cipher_key(&secret, &nonce);
        let mut genuine = b"one".to_vec();
        SessionAuth::encrypted(key, cipher_key)
            .seal(Direction::ToServer, &mut genuine)
            .unwrap();

        let mut forged = genuine.clone();
        forged[0] ^= 0xff;
        let mut plain: Vec<u8> = Vec::new();
        let mut rx = SessionAuth::encrypted(key, cipher_key);
        assert_eq!(
            rx.open(Direction::ToServer, &forged, &mut plain),
            Err(AuthError::BadTag)
        );
        assert!(plain.is_empty());
        // Sequence 1 was never accepted, so the datagram that genuinely carries it still opens.
        assert_eq!(
            rx.open(Direction::ToServer, &genuine, &mut plain),
            Ok(&b"one"[..])
        );
        // And the replay window still refuses the second copy.
        assert_eq!(
            rx.open(Direction::ToServer, &genuine, &mut plain),
            Err(AuthError::Replayed)
        );
        assert!(plain.is_empty(), "a refused replay releases nothing either");
    }

    #[test]
    fn a_peer_on_a_different_secret_decrypts_nothing() {
        let nonce = session_nonce(&PIN_NONCE, &PIN_ACCEPTOR);
        let ours = compress_secret(PIN_SECRET);
        let theirs = compress_secret(b"another secret");
        let mut datagram = b"input".to_vec();
        SessionAuth::encrypted(
            derive_session_key(&ours, &nonce),
            derive_cipher_key(&ours, &nonce),
        )
        .seal(Direction::ToServer, &mut datagram)
        .unwrap();

        let mut plain: Vec<u8> = Vec::new();
        assert_eq!(
            SessionAuth::encrypted(
                derive_session_key(&theirs, &nonce),
                derive_cipher_key(&theirs, &nonce),
            )
            .open(Direction::ToServer, &datagram, &mut plain),
            Err(AuthError::BadTag)
        );
        // A peer holding the secret but not this join's nonce is refused the same way: the cipher key
        // is per join, which is what keeps one session's capture out of the next.
        let mut other_acceptor = PIN_ACCEPTOR;
        other_acceptor[0] ^= 0x01;
        let next = session_nonce(&PIN_NONCE, &other_acceptor);
        assert_eq!(
            SessionAuth::encrypted(
                derive_session_key(&ours, &next),
                derive_cipher_key(&ours, &next),
            )
            .open(Direction::ToServer, &datagram, &mut plain),
            Err(AuthError::BadTag)
        );
    }

    #[test]
    fn an_encrypted_datagram_shorter_than_its_trailer_is_truncated() {
        let secret = compress_secret(PIN_SECRET);
        let nonce = session_nonce(&PIN_NONCE, &PIN_ACCEPTOR);
        let mut plain: Vec<u8> = Vec::new();
        let mut rx = SessionAuth::encrypted(
            derive_session_key(&secret, &nonce),
            derive_cipher_key(&secret, &nonce),
        );
        for len in 0..CIPHER_TRAILER_LEN {
            assert_eq!(
                rx.open(Direction::ToServer, &vec![0u8; len], &mut plain),
                Err(AuthError::Truncated),
                "len {len}"
            );
        }
    }

    #[test]
    fn an_empty_payload_still_encrypts_and_opens() {
        let secret = compress_secret(PIN_SECRET);
        let nonce = session_nonce(&PIN_NONCE, &PIN_ACCEPTOR);
        let key = derive_session_key(&secret, &nonce);
        let cipher_key = derive_cipher_key(&secret, &nonce);
        let mut datagram: Vec<u8> = Vec::new();
        SessionAuth::encrypted(key, cipher_key)
            .seal(Direction::ToClient, &mut datagram)
            .unwrap();
        assert_eq!(datagram.len(), CIPHER_TRAILER_LEN);
        let mut plain: Vec<u8> = Vec::new();
        assert_eq!(
            SessionAuth::encrypted(key, cipher_key).open(
                Direction::ToClient,
                &datagram,
                &mut plain
            ),
            Ok(&[][..])
        );
    }

    /// The associated data is the sequence number then the direction, which is the order
    /// [`tag_over`] signs under the clear regime. Stated as a value rather than left implicit,
    /// because a port that reverses the two produces tags that verify nowhere.
    #[test]
    fn the_associated_data_names_the_sequence_then_the_direction() {
        assert_eq!(datagram_aad(Direction::ToServer, 1), [1, 0, 0, 0, 0x01]);
        assert_eq!(
            datagram_aad(Direction::ToClient, 0x0201),
            [0x01, 0x02, 0, 0, 0x02]
        );
        assert_eq!(
            datagram_nonce(Direction::ToServer, 1)[..5],
            [0x01, 1, 0, 0, 0]
        );
        assert_eq!(
            datagram_nonce(Direction::ToServer, 1)[5..],
            [0; CIPHER_NONCE_LEN - 5],
            "the remaining bytes are reserved and zero"
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
        let mut plain: Vec<u8> = Vec::new();
        let mut tx = SessionAuth::new(REF_KEY);
        let mut rx = SessionAuth::new(REF_KEY);
        let mut buf = b"hello".to_vec();
        assert!(tx.seal(Direction::ToServer, &mut buf).is_some());
        assert_eq!(buf.len(), 5 + TRAILER_LEN);
        assert_eq!(
            rx.open(Direction::ToServer, &buf, &mut plain),
            Ok(&b"hello"[..])
        );
    }

    #[test]
    fn an_empty_payload_still_seals_and_opens() {
        let mut plain: Vec<u8> = Vec::new();
        let mut tx = SessionAuth::new(REF_KEY);
        let mut rx = SessionAuth::new(REF_KEY);
        let mut buf: Vec<u8> = Vec::new();
        tx.seal(Direction::ToClient, &mut buf).unwrap();
        assert_eq!(rx.open(Direction::ToClient, &buf, &mut plain), Ok(&[][..]));
    }

    #[test]
    fn a_tampered_byte_fails_the_tag() {
        let mut plain: Vec<u8> = Vec::new();
        let mut tx = SessionAuth::new(REF_KEY);
        let mut rx = SessionAuth::new(REF_KEY);
        let mut buf = b"hello".to_vec();
        tx.seal(Direction::ToServer, &mut buf).unwrap();
        for index in 0..buf.len() {
            let mut forged = buf.clone();
            forged[index] ^= 0x01;
            let mut fresh = SessionAuth::new(REF_KEY);
            assert_eq!(
                fresh.open(Direction::ToServer, &forged, &mut plain),
                Err(AuthError::BadTag),
                "byte {index}"
            );
        }
        // The untouched original still opens, so the loop above rejected forgeries and not the scheme.
        assert!(rx.open(Direction::ToServer, &buf, &mut plain).is_ok());
    }

    #[test]
    fn a_wrong_key_fails_the_tag() {
        let mut plain: Vec<u8> = Vec::new();
        let mut tx = SessionAuth::new(REF_KEY);
        let mut other = REF_KEY;
        other[15] ^= 0x80;
        let mut rx = SessionAuth::new(other);
        let mut buf = b"hello".to_vec();
        tx.seal(Direction::ToServer, &mut buf).unwrap();
        assert_eq!(
            rx.open(Direction::ToServer, &buf, &mut plain),
            Err(AuthError::BadTag)
        );
    }

    #[test]
    fn a_reflected_datagram_fails_the_tag() {
        let mut plain: Vec<u8> = Vec::new();
        // The whole point of the direction byte: the same key, the same bytes, the other direction.
        let mut tx = SessionAuth::new(REF_KEY);
        let mut rx = SessionAuth::new(REF_KEY);
        let mut buf = b"input".to_vec();
        tx.seal(Direction::ToServer, &mut buf).unwrap();
        assert_eq!(
            rx.open(Direction::ToClient, &buf, &mut plain),
            Err(AuthError::BadTag)
        );
    }

    #[test]
    fn a_datagram_shorter_than_its_trailer_is_truncated() {
        let mut plain: Vec<u8> = Vec::new();
        let mut rx = SessionAuth::new(REF_KEY);
        for len in 0..TRAILER_LEN {
            assert_eq!(
                rx.open(Direction::ToServer, &vec![0u8; len], &mut plain),
                Err(AuthError::Truncated),
                "len {len}"
            );
        }
    }

    #[test]
    fn a_replayed_datagram_is_refused_once_it_has_been_accepted() {
        let mut plain: Vec<u8> = Vec::new();
        let mut tx = SessionAuth::new(REF_KEY);
        let mut rx = SessionAuth::new(REF_KEY);
        let mut buf = b"input".to_vec();
        tx.seal(Direction::ToServer, &mut buf).unwrap();
        assert!(rx.open(Direction::ToServer, &buf, &mut plain).is_ok());
        assert_eq!(
            rx.open(Direction::ToServer, &buf, &mut plain),
            Err(AuthError::Replayed)
        );
    }

    #[test]
    fn a_forged_datagram_does_not_burn_a_sequence_number() {
        let mut plain: Vec<u8> = Vec::new();
        let mut tx = SessionAuth::new(REF_KEY);
        let mut rx = SessionAuth::new(REF_KEY);
        let mut first = b"one".to_vec();
        tx.seal(Direction::ToServer, &mut first).unwrap();
        let mut forged = first.clone();
        forged[0] ^= 0xff;
        assert_eq!(
            rx.open(Direction::ToServer, &forged, &mut plain),
            Err(AuthError::BadTag)
        );
        // Sequence 1 was never accepted, so the genuine datagram carrying it still is.
        assert!(rx.open(Direction::ToServer, &first, &mut plain).is_ok());
    }

    #[test]
    fn seal_refuses_once_the_counter_is_spent() {
        // Parked on the last sequence number rather than sealing four billion datagrams to reach it.
        let mut spent = SessionAuth {
            key: REF_KEY,
            cipher_key: None,
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

    /// The same refusal under a secret, where it is the property the whole construction rests on: the
    /// nonce is the direction byte and this counter, so a counter that wrapped would seal a second
    /// datagram under a nonce already used and hand an observer the exclusive-or of two payloads. The
    /// clear regime's failure is a replayed tag; this one is silent, so it gets its own test rather
    /// than trusting the shared code path.
    #[test]
    fn seal_refuses_once_the_counter_is_spent_under_a_secret() {
        let secret = compress_secret(PIN_SECRET);
        let nonce = session_nonce(&PIN_NONCE, &PIN_ACCEPTOR);
        let mut spent = SessionAuth {
            key: derive_session_key(&secret, &nonce),
            cipher_key: Some(derive_cipher_key(&secret, &nonce)),
            next_seq: u32::MAX,
            window: ReplayWindow::new(),
        };
        let mut buf = b"input".to_vec();
        assert!(spent.seal(Direction::ToServer, &mut buf).is_some());
        assert_eq!(buf.len(), b"input".len() + CIPHER_TRAILER_LEN);
        assert!(spent.exhausted());
        // The counter is spent rather than wrapped, and the refusal produces no datagram at all.
        let mut buf = b"input".to_vec();
        assert!(spent.seal(Direction::ToServer, &mut buf).is_none());
        assert_eq!(buf, b"input");
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
    // --- the key exchange ---------------------------------------------------------------------

    /// One acceptor static secret, one acceptor ephemeral and one joiner ephemeral, all distinct.
    const ACCEPTOR_STATIC: [u8; EXCHANGE_KEY_LEN] = [0x11; EXCHANGE_KEY_LEN];
    const ACCEPTOR_EPHEMERAL: [u8; EXCHANGE_KEY_LEN] = [0x22; EXCHANGE_KEY_LEN];
    const JOINER_EPHEMERAL: [u8; EXCHANGE_KEY_LEN] = [0x33; EXCHANGE_KEY_LEN];

    /// Both sides of one honest exchange, as the join runs it.
    fn honest_exchange() -> (Option<[u8; KEY_LEN]>, Option<[u8; KEY_LEN]>) {
        let acceptor_static_public = exchange_public_key(&ACCEPTOR_STATIC);
        let acceptor_ephemeral_public = exchange_public_key(&ACCEPTOR_EPHEMERAL);
        let joiner_ephemeral_public = exchange_public_key(&JOINER_EPHEMERAL);
        (
            joiner_exchange_secret(
                &JOINER_EPHEMERAL,
                &acceptor_static_public,
                &acceptor_ephemeral_public,
            ),
            acceptor_exchange_secret(
                &ACCEPTOR_STATIC,
                &ACCEPTOR_EPHEMERAL,
                &joiner_ephemeral_public,
            ),
        )
    }

    /// The whole point: two peers that never exchanged a secret arrive at the same 16 bytes, and an
    /// observer who read every public value on the wire cannot produce them.
    #[test]
    fn both_ends_of_an_honest_exchange_derive_the_same_secret() {
        let (joiner, acceptor) = honest_exchange();
        assert_eq!(joiner, acceptor);
        assert!(joiner.is_some());
        assert_ne!(joiner.unwrap(), [0u8; KEY_LEN]);
    }

    /// **The pin is what authenticates the acceptor.** A joiner pinning some other acceptor's key runs
    /// the exchange to completion and derives different bytes, so its confirmation fails and the
    /// acceptor's reply does not open at it. This is the check the whole scheme rests on.
    #[test]
    fn a_joiner_pinning_another_key_derives_a_different_secret() {
        let imposter_static = [0x44; EXCHANGE_KEY_LEN];
        let joiner = joiner_exchange_secret(
            &JOINER_EPHEMERAL,
            &exchange_public_key(&imposter_static),
            &exchange_public_key(&ACCEPTOR_EPHEMERAL),
        );
        let (_, acceptor) = honest_exchange();
        assert!(joiner.is_some());
        assert_ne!(joiner, acceptor);
    }

    /// An on-path attacker that substitutes the acceptor's **ephemeral** cannot supply the static half, so
    /// the two ends disagree. It holds the ephemeral secret it injected and the acceptor's public key,
    /// which is everything an observer has, and it is not enough.
    #[test]
    fn substituting_the_acceptor_ephemeral_breaks_the_agreement() {
        let attacker_ephemeral = [0x55; EXCHANGE_KEY_LEN];
        let joiner = joiner_exchange_secret(
            &JOINER_EPHEMERAL,
            &exchange_public_key(&ACCEPTOR_STATIC),
            &exchange_public_key(&attacker_ephemeral),
        );
        let (honest_joiner, acceptor) = honest_exchange();
        assert_ne!(joiner, acceptor);
        assert_ne!(joiner, honest_joiner);
    }

    /// And substituting the **joiner's** ephemeral breaks it from the other direction: the acceptor folds
    /// the key it received, the joiner folds the one it sent, and the transcript differs even before
    /// the shared values do.
    #[test]
    fn substituting_the_joiner_ephemeral_breaks_the_agreement() {
        let attacker_ephemeral = [0x66; EXCHANGE_KEY_LEN];
        let acceptor = acceptor_exchange_secret(
            &ACCEPTOR_STATIC,
            &ACCEPTOR_EPHEMERAL,
            &exchange_public_key(&attacker_ephemeral),
        );
        let (joiner, _) = honest_exchange();
        assert_ne!(joiner, acceptor);
    }

    /// **A low-order public key is refused rather than keyed on.** The all-zero key forces a shared
    /// value of zero whatever the other side's secret was, so a peer that accepted it would key on
    /// bytes the attacker chose. Both DH values are checked, so either position refuses the exchange.
    #[test]
    fn a_non_contributory_public_key_refuses_the_exchange() {
        let zero = [0u8; EXCHANGE_KEY_LEN];
        assert_eq!(
            joiner_exchange_secret(
                &JOINER_EPHEMERAL,
                &zero,
                &exchange_public_key(&ACCEPTOR_EPHEMERAL)
            ),
            None,
            "an all-zero pinned key"
        );
        assert_eq!(
            joiner_exchange_secret(
                &JOINER_EPHEMERAL,
                &exchange_public_key(&ACCEPTOR_STATIC),
                &zero
            ),
            None,
            "an all-zero acceptor ephemeral"
        );
        assert_eq!(
            acceptor_exchange_secret(&ACCEPTOR_STATIC, &ACCEPTOR_EPHEMERAL, &zero),
            None,
            "and an all-zero joiner ephemeral"
        );
        // A NON-ZERO low-order point, `p - 1`. The all-zero arms above are the easy case and a caller
        // may screen for them by value; this one cannot be told from an honest key without doing the
        // arithmetic, so it is the value the check has to catch.
        let mut order_two = [0xffu8; EXCHANGE_KEY_LEN];
        order_two[0] = 0xec;
        order_two[EXCHANGE_KEY_LEN - 1] = 0x7f;
        assert_eq!(
            acceptor_exchange_secret(&ACCEPTOR_STATIC, &ACCEPTOR_EPHEMERAL, &order_two),
            None,
            "a non-zero low-order joiner ephemeral"
        );
        assert_eq!(
            joiner_exchange_secret(
                &JOINER_EPHEMERAL,
                &exchange_public_key(&ACCEPTOR_STATIC),
                &order_two
            ),
            None,
            "and a non-zero low-order acceptor ephemeral"
        );
    }

    /// **The three public keys bind the fold to the transcript**, and this is the test that fails if
    /// they stop being folded in. Every other exchange test varies a secret, so the two Diffie-Hellman
    /// values already differ and the assertion passes on those alone; here the shared values are held
    /// identical and only the transcript moves, which is the substitution
    /// [`exchange_fold`]'s header says the binding refuses.
    #[test]
    fn the_fold_binds_the_three_public_keys() {
        let a = StaticSecret::from(ACCEPTOR_STATIC);
        let b = StaticSecret::from(JOINER_EPHEMERAL);
        let to_static = a.diffie_hellman(&PublicKey::from(&b));
        let to_ephemeral = b.diffie_hellman(&PublicKey::from(&a));
        let fold = |static_public, joiner_public, acceptor_public| {
            exchange_fold(
                &static_public,
                &joiner_public,
                &acceptor_public,
                &to_static,
                &to_ephemeral,
            )
            .expect("an honest pair of shared values is contributory")
        };
        let base = fold(
            [1u8; EXCHANGE_KEY_LEN],
            [2u8; EXCHANGE_KEY_LEN],
            [3u8; EXCHANGE_KEY_LEN],
        );
        assert_ne!(
            base,
            fold(
                [9u8; EXCHANGE_KEY_LEN],
                [2u8; EXCHANGE_KEY_LEN],
                [3u8; EXCHANGE_KEY_LEN]
            ),
            "the acceptor's static public key"
        );
        assert_ne!(
            base,
            fold(
                [1u8; EXCHANGE_KEY_LEN],
                [9u8; EXCHANGE_KEY_LEN],
                [3u8; EXCHANGE_KEY_LEN]
            ),
            "the joiner's ephemeral public key"
        );
        assert_ne!(
            base,
            fold(
                [1u8; EXCHANGE_KEY_LEN],
                [2u8; EXCHANGE_KEY_LEN],
                [9u8; EXCHANGE_KEY_LEN]
            ),
            "the acceptor's ephemeral public key"
        );
    }

    /// A fresh ephemeral per join is what makes the derived secret per-join, the same property the
    /// nonce halves have. Reusing one would hand every session of that acceptor the same key.
    #[test]
    fn a_fresh_joiner_ephemeral_changes_the_secret() {
        let (first, _) = honest_exchange();
        let second = joiner_exchange_secret(
            &[0x77; EXCHANGE_KEY_LEN],
            &exchange_public_key(&ACCEPTOR_STATIC),
            &exchange_public_key(&ACCEPTOR_EPHEMERAL),
        );
        assert_ne!(first, second);
    }

    /// A public key is a pure function of its secret, and two different secrets do not share one.
    #[test]
    fn a_public_key_is_derived_from_its_secret() {
        assert_eq!(
            exchange_public_key(&ACCEPTOR_STATIC),
            exchange_public_key(&ACCEPTOR_STATIC)
        );
        assert_ne!(
            exchange_public_key(&ACCEPTOR_STATIC),
            exchange_public_key(&ACCEPTOR_EPHEMERAL)
        );
    }

    /// The four cases a session can be in, and the one that matters: configuring both secrets produces
    /// bytes that are neither of them, so an attacker holding one still has to break the other.
    #[test]
    fn folding_two_secrets_depends_on_both() {
        let supplied = compress_secret(b"a lobby token");
        let exchanged = honest_exchange().0.unwrap();
        assert_eq!(fold_secrets(None, None), None);
        assert_eq!(fold_secrets(Some(&supplied), None), Some(supplied));
        assert_eq!(fold_secrets(None, Some(&exchanged)), Some(exchanged));
        let both = fold_secrets(Some(&supplied), Some(&exchanged)).unwrap();
        assert_ne!(both, supplied);
        assert_ne!(both, exchanged);
        assert_eq!(
            fold_secrets(Some(&supplied), Some(&exchanged)),
            Some(both),
            "and it is deterministic, or the two ends would not agree"
        );
        assert_ne!(
            fold_secrets(Some(&exchanged), Some(&supplied)),
            Some(both),
            "the two arguments are not interchangeable"
        );
    }

    /// The exchange feeds the derivation that already exists, so a session under it seats a key
    /// neither half of the wire nonce decides and the confirm tag recomputes over it.
    #[test]
    fn an_exchanged_secret_keys_a_session_end_to_end() {
        let exchanged = honest_exchange().0.unwrap();
        let nonce = session_nonce(&[1; KEY_LEN], &[2; KEY_LEN]);
        let key = derive_session_key(&fold_secrets(None, Some(&exchanged)).unwrap(), &nonce);
        assert_ne!(key, nonce, "the exchange changed the key");
        let tag = confirm_tag(&key, &nonce, 0x0009_0000);
        assert_eq!(tag, confirm_tag(&key, &nonce, 0x0009_0000));
        assert_ne!(tag, confirm_tag(&nonce, &nonce, 0x0009_0000));
    }
}
