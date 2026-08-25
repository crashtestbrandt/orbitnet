//! Wire encoding.
//!
//! Frames carry no property names and no per-field type tags — just packed fields in schema order,
//! preceded by a changed-property bitmask. That is most of why they are small, and it is why
//! [`crate::protocol::SchemaBuilder::hash`] exists to catch a disagreement before it decodes into
//! garbage.
//!
//! **A block names its entity by a dense 16-bit session slot, not by the 64-bit entity id.** The id
//! is an FNV-1a hash spread across the whole 64-bit range, so writing it as a varint cost 9.5 bytes
//! on average — a third of a full block and nearly half of a delta. [`crate::slots`] states what
//! replaced it and what distributing a slot table costs; [`encode_manifest_full`] and
//! [`encode_manifest_delta`] are the channel that distributes it.
//!
//! The decoder is the one piece of this crate that reads bytes chosen by a remote peer, so it is
//! written to be total: every read is bounds-checked and returns [`CodecError`] rather than
//! panicking or indexing out of range. A netcode decoder that panics on a malformed packet is a
//! remote denial of service, and `forbid(unsafe_code)` at the crate root means a bounds bug cannot
//! become memory unsafety either.

use core::fmt;
use std::collections::BTreeMap;

use crate::auth::{confirm_tag, derive_session_key, KEY_LEN};
use crate::columnar::changed_mask;
use crate::protocol::{protocol_major, PropSchema, PROTOCOL_VERSION};
use crate::seats::SeatIndex;

/// Frame magic, present only on the reliable handshake.
pub const MAGIC: [u8; 4] = *b"OBNW";

/// Payload ceiling for a single frame, chosen to sit under a typical path MTU.
///
/// Frames are never fragmented: when a tick's content exceeds this, low-priority entities are
/// deferred to a later tick instead. That is what stops a crowded fight from stalling a server.
///
/// The frame header and the [`crate::auth::TRAILER_LEN`] authentication trailer both ride ABOVE this
/// ceiling, so a full datagram is this plus both. The headroom under a 1500-byte path MTU covers it.
pub const MAX_FRAME_PAYLOAD: usize = 1200;

/// Something went wrong reading or validating a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecError {
    /// The buffer ended in the middle of a field.
    UnexpectedEof,
    /// The handshake did not begin with [`MAGIC`].
    BadMagic,
    /// A varint ran past the width of its target type.
    VarintOverflow,
    /// The frame's kind byte is not one this build understands.
    UnknownFrameKind(u8),
    /// The peer speaks an incompatible protocol major version.
    ProtocolMismatch {
        /// The remote peer's version.
        theirs: u32,
        /// This peer's version.
        ours: u32,
    },
    /// The peer's handshake carried no 16-byte session nonce, so nothing it sends afterward can be
    /// authenticated. An older build, or a truncated handshake.
    MissingSessionNonce,
    /// This peer holds a **session secret** and the remote handshake could not confirm the same one:
    /// its [`Handshake::confirm`] tag is absent, or it is a tag over some other secret.
    ///
    /// One side configured a secret and the other did not, or the two were handed different bytes.
    /// See [`Handshake::check_compatibility`] for which direction of that misconfiguration this
    /// reports and which one cannot be reported at all.
    SecretMismatch,
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodecError::UnexpectedEof => write!(f, "frame ended mid-field"),
            CodecError::BadMagic => write!(f, "handshake did not start with the OrbitNet magic"),
            CodecError::VarintOverflow => write!(f, "varint overflowed its target type"),
            CodecError::UnknownFrameKind(kind) => write!(f, "unknown frame kind 0x{kind:02x}"),
            CodecError::ProtocolMismatch { theirs, ours } => write!(
                f,
                "OrbitNet protocol mismatch: peer speaks v{}, we speak v{}. \
                 Both sides must run the same OrbitNet major version.",
                version_string(*theirs),
                version_string(*ours)
            ),
            CodecError::MissingSessionNonce => write!(
                f,
                "OrbitNet handshake carried no session nonce, so no datagram from this peer can be \
                 authenticated. The peer is an older build, or its handshake was truncated."
            ),
            CodecError::SecretMismatch => write!(
                f,
                "OrbitNet handshake could not confirm this session's shared secret. This peer holds \
                 one and the joining peer proved a different one, or none at all. Both ends must be \
                 handed the same secret before they start."
            ),
        }
    }
}

impl std::error::Error for CodecError {}

/// Render a packed protocol version as `major.minor.patch`.
#[must_use]
pub fn version_string(version: u32) -> String {
    format!(
        "{}.{}.{}",
        version >> 16,
        (version >> 8) & 0xff,
        version & 0xff
    )
}

/// What a frame carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
    /// Authoritative state, server to client. Hot path, unreliable.
    ServerSnapshot = 0x01,
    /// Player intent, client to server. Hot path, unreliable.
    ClientInput = 0x02,
    /// Clock probe, client to server. Unreliable — a lost sample is just a missed data point.
    Ping = 0x03,
    /// Clock probe reply, server to client. Unreliable.
    Pong = 0x04,
    /// Join reply, server to client. Reliable — carries the tick seed the client starts from.
    Welcome = 0x06,
    /// Entity schema manifest, server to client. Reliable — lets a client validate that its
    /// locally built entity schema matches the server's before misapplying a single byte.
    ///
    /// **The whole table, carrying the generation it stands at.** See [`encode_manifest_full`].
    EntityManifest = 0x07,
    /// A change to the entity manifest, server to client. Reliable, and **ordered against the
    /// [`FrameKind::EntityManifest`] frames on the same channel**, which is what a delta needs and a
    /// snapshot does not.
    ///
    /// Restating the whole table on every change cost one republish per net tick per peer, of
    /// ~22.5 bytes per named entity. See [`ManifestDelta`] for the layout and for what a delta gives
    /// up against the complete table it replaces.
    EntityManifestDelta = 0x08,
    /// The whole of one peer's interest set, server to client. Reliable, and **per peer** — unlike
    /// every other reliable frame here, its contents differ per recipient.
    ///
    /// The repair path for the interest delta, and the answer to
    /// [`FrameHeader::FLAG_WANT_INTEREST`]. See [`encode_interest_table`] for why a set rather than
    /// a diff, and [`InterestDeltaSection::generation`] for what stops an older delta undoing it.
    InterestTable = 0x09,
}

impl FrameKind {
    /// Parse a kind byte.
    pub fn from_tag(tag: u8) -> Result<Self, CodecError> {
        match tag {
            0x01 => Ok(FrameKind::ServerSnapshot),
            0x02 => Ok(FrameKind::ClientInput),
            0x03 => Ok(FrameKind::Ping),
            0x04 => Ok(FrameKind::Pong),
            0x06 => Ok(FrameKind::Welcome),
            0x07 => Ok(FrameKind::EntityManifest),
            0x08 => Ok(FrameKind::EntityManifestDelta),
            0x09 => Ok(FrameKind::InterestTable),
            other => Err(CodecError::UnknownFrameKind(other)),
        }
    }

    /// The kind byte.
    #[must_use]
    pub fn tag(self) -> u8 {
        self as u8
    }
}

/// Appends values to a byte buffer.
#[derive(Debug, Default, Clone)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    /// A new empty writer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A writer with room for `capacity` bytes reserved up front.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buf: Vec::with_capacity(capacity),
        }
    }

    /// Bytes written so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether nothing has been written.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Borrow the written bytes.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }

    /// Take the written bytes.
    #[must_use]
    pub fn into_inner(self) -> Vec<u8> {
        self.buf
    }

    /// Roll the buffer back to a length recorded earlier, discarding everything written since.
    ///
    /// The send path needs this because an entity block's encoded size is not known until it has been
    /// written: the budget can only be enforced by writing the block and un-writing it when it does not
    /// fit. Longer than the current length is a no-op, so a caller can never grow the buffer with it.
    pub fn truncate(&mut self, len: usize) {
        if len < self.buf.len() {
            self.buf.truncate(len);
        }
    }

    /// Drop everything written, keeping the allocation for reuse.
    pub fn clear(&mut self) {
        self.buf.clear();
    }

    /// Write one byte.
    pub fn u8(&mut self, value: u8) {
        self.buf.push(value);
    }

    /// Write one signed byte.
    pub fn i8(&mut self, value: i8) {
        self.buf.push(value as u8);
    }

    /// Write a little-endian `u32`.
    pub fn u32(&mut self, value: u32) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    /// Write a little-endian `u64`.
    pub fn u64(&mut self, value: u64) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    /// Write a little-endian `u16`.
    pub fn u16(&mut self, value: u16) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    /// Write a little-endian `f32`.
    pub fn f32(&mut self, value: f32) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    /// Write a little-endian `f64`.
    pub fn f64(&mut self, value: f64) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    /// Write raw bytes.
    pub fn bytes(&mut self, value: &[u8]) {
        self.buf.extend_from_slice(value);
    }

    /// Write an LEB128 varint.
    pub fn varint(&mut self, mut value: u64) {
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            self.buf.push(byte);
            if value == 0 {
                break;
            }
        }
    }

    /// Write a signed integer zigzag-encoded as a varint.
    pub fn zigzag(&mut self, value: i64) {
        self.varint(((value << 1) ^ (value >> 63)) as u64);
    }

    /// Write a packed bitmask, one bit per entry, LSB first.
    pub fn bitmask(&mut self, bits: &[bool]) {
        for chunk in bits.chunks(8) {
            let mut byte = 0u8;
            for (index, &set) in chunk.iter().enumerate() {
                if set {
                    byte |= 1 << index;
                }
            }
            self.buf.push(byte);
        }
    }
}

/// Reads values back out of a byte buffer.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// Read from `buf`, starting at the beginning.
    #[must_use]
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Bytes not yet consumed.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Whether every byte has been consumed.
    #[must_use]
    pub fn is_exhausted(&self) -> bool {
        self.remaining() == 0
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], CodecError> {
        if self.remaining() < count {
            return Err(CodecError::UnexpectedEof);
        }
        let slice = &self.buf[self.pos..self.pos + count];
        self.pos += count;
        Ok(slice)
    }

    /// Read one byte.
    pub fn u8(&mut self) -> Result<u8, CodecError> {
        Ok(self.take(1)?[0])
    }

    /// Read one signed byte.
    pub fn i8(&mut self) -> Result<i8, CodecError> {
        Ok(self.take(1)?[0] as i8)
    }

    /// Read a little-endian `u16`.
    pub fn u16(&mut self) -> Result<u16, CodecError> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    /// Read a little-endian `u32`.
    pub fn u32(&mut self) -> Result<u32, CodecError> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// Read a little-endian `u64`.
    pub fn u64(&mut self) -> Result<u64, CodecError> {
        let bytes = self.take(8)?;
        let mut array = [0u8; 8];
        array.copy_from_slice(bytes);
        Ok(u64::from_le_bytes(array))
    }

    /// Read a little-endian `f32`.
    pub fn f32(&mut self) -> Result<f32, CodecError> {
        let bytes = self.take(4)?;
        Ok(f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// Read a little-endian `f64`.
    pub fn f64(&mut self) -> Result<f64, CodecError> {
        let bytes = self.take(8)?;
        let mut array = [0u8; 8];
        array.copy_from_slice(bytes);
        Ok(f64::from_le_bytes(array))
    }

    /// Read `count` raw bytes.
    pub fn bytes(&mut self, count: usize) -> Result<&'a [u8], CodecError> {
        self.take(count)
    }

    /// Borrow `count` bytes at `offset` past the cursor without consuming anything.
    ///
    /// The input-row accessor uses this to hand out per-tick rows in any order while the caller
    /// advances the cursor once, after the whole block is dealt with.
    #[must_use]
    pub fn peek_bytes(&self, offset: usize, count: usize) -> Option<&'a [u8]> {
        let start = self.pos.checked_add(offset)?;
        let end = start.checked_add(count)?;
        if end > self.buf.len() {
            return None;
        }
        Some(&self.buf[start..end])
    }

    /// Read an LEB128 varint.
    pub fn varint(&mut self) -> Result<u64, CodecError> {
        let mut result: u64 = 0;
        let mut shift = 0u32;
        loop {
            let byte = self.u8()?;
            let payload = u64::from(byte & 0x7f);
            if shift >= 64 || (shift == 63 && payload > 1) {
                return Err(CodecError::VarintOverflow);
            }
            result |= payload << shift;
            if byte & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
        }
    }

    /// Read a zigzag-encoded signed varint.
    pub fn zigzag(&mut self) -> Result<i64, CodecError> {
        let raw = self.varint()?;
        Ok(((raw >> 1) as i64) ^ -((raw & 1) as i64))
    }

    /// Read a packed bitmask of `count` bits into `out`.
    pub fn bitmask_into(&mut self, count: usize, out: &mut Vec<bool>) -> Result<(), CodecError> {
        out.clear();
        let byte_count = count.div_ceil(8);
        let bytes = self.take(byte_count)?;
        for index in 0..count {
            let byte = bytes[index / 8];
            out.push(byte & (1 << (index % 8)) != 0);
        }
        Ok(())
    }
}

/// The header every hot frame begins with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    /// Whether this is server state or client input.
    pub kind: FrameKind,
    /// The tick this frame describes.
    pub tick: u32,
    /// The newest tick the sender has received from the recipient.
    pub ack_tick: u32,
    /// Bitfield of the 32 ticks before `ack_tick` that were also received.
    ///
    /// Acks ride the hot frame rather than a separate reliable message, which is one whole class of
    /// per-peer RPC the GDScript backend sent every tick and this one does not.
    pub ack_bits: u32,
    /// The proof that `ack_tick` names a frame the sender received, rather than a number it chose.
    ///
    /// Two roles, one slot, decided by [`FrameHeader::kind`]:
    ///
    /// | Kind | What it carries |
    /// | --- | --- |
    /// | [`FrameKind::ServerSnapshot`] | The token this frame is minted with, for the client to quote back. |
    /// | [`FrameKind::ClientInput`] | The token of the snapshot frame `ack_tick` names. |
    ///
    /// **The server mints it from a per-peer secret it never transmits**, so a client cannot compute the
    /// token of a frame that never reached it. The server recomputes the expected value on arrival and
    /// discards an ack that does not carry it — no round-trip sample, no `acked_base` promotion.
    ///
    /// `0` when there is nothing to prove: an input frame with `ack_tick` still at `0`, and every peer
    /// that has yet to receive a snapshot.
    pub ack_token: u32,
    /// How early (positive) or late (negative) the recipient's newest input arrived.
    ///
    /// The client steers its tick lead to hold this slightly positive, which is what keeps the
    /// server's resimulation window shallow.
    pub margin_ticks: i8,
    /// Frame-level flags, see the `FLAG_*` constants.
    pub flags: u8,
    /// How many entity blocks follow.
    pub entity_count: u32,
}

impl FrameHeader {
    /// Client → server: "my last delta base broke, send full masks for everything once."
    ///
    /// This is the cheap NACK that bounds how long a lost packet can freeze an entity: instead of
    /// waiting out the periodic full-state phase, the client raises this bit on its next input
    /// frame and the server responds with full blocks.
    pub const FLAG_WANT_FULL: u8 = 1 << 0;

    /// Server → client: an [`InterestDeltaSection`] follows the entity blocks.
    ///
    /// **The flag bits are per direction, and this one is SERVER-TO-CLIENT only.** A client never
    /// sets it and a server never reads it; bit 2 is claimed in the other direction. Bit 0 above is
    /// the exception that is read on both sides.
    ///
    /// **A trailing section is invisible to a peer that does not know about it**, which is what
    /// makes it safe to append to the hot frame. A receiver reads exactly `entity_count` blocks and
    /// stops, so an older build never looks at the bytes after them and never notices the bit. It is
    /// still a major bump, because a peer that skips the section skips the events too and a game
    /// built on them would silently receive none.
    pub const FLAG_INTEREST_DELTA: u8 = 1 << 1;

    /// Client → server: "the entity manifest stream broke for me; send the whole table again."
    ///
    /// **CLIENT-TO-SERVER only**, the mirror of [`FrameHeader::FLAG_INTEREST_DELTA`] one bit up. A
    /// server never sets it and a client never reads it.
    ///
    /// It reuses the shape [`FrameHeader::FLAG_WANT_FULL`] already established — a bit on an input
    /// frame the client is sending anyway — so the repair path for a manifest costs **no frame kind
    /// and no bytes**. A client raises it when it cannot apply a [`ManifestDelta`]: the delta names a
    /// base generation it does not hold, or the frame did not decode. The server answers by clearing
    /// what it believes that peer holds, which makes that peer's next publish a full table.
    ///
    /// **The raise is self-sustaining**, so losing the input frame that carried it costs one tick:
    /// the client zeroed its own generation at the same moment, so the next delta fails its base
    /// check as well and raises the bit again.
    pub const FLAG_WANT_MANIFEST: u8 = 1 << 2;

    /// Client → server: "I could not apply an interest delta; send me the whole set."
    ///
    /// **CLIENT-TO-SERVER only**, and the exact shape [`FrameHeader::FLAG_WANT_MANIFEST`]
    /// established one bit down: a bit on an input frame the client is sending anyway, so the repair
    /// costs no frame kind on the way up.
    ///
    /// A client raises it for the three cases only it can see: an `entered` slot its manifest has not
    /// bound yet, a section stamped at a generation it does not hold, and a frame it acknowledged but
    /// could not read to the end — the ack window slides before a block is parsed, so a snapshot that
    /// breaks partway is counted delivered whatever became of the section in it. The interest delta rides an UNRELIABLE snapshot while the manifest that names its
    /// slots rides a RELIABLE channel, and the two have no ordering relationship — so a snapshot can
    /// arrive naming a slot whose binding is still in ENet's retransmit queue. Dropping that enter
    /// silently is what left a client's mirror permanently short of an entity whose rows kept
    /// arriving.
    ///
    /// The server's own three cases — a pending queue that overflowed, a prefix given up on
    /// unacknowledged, and a rekey on a live connection — need no bit, because the server is the side
    /// that knows.
    ///
    /// **The raise is LATCHED, not self-sustaining.** The manifest's clears when the frame carrying it
    /// goes out and re-raises on the next delta it cannot apply; this one stays up until it is
    /// ANSWERED, by a whole set the client could name in full. Clearing it on send made it a one-shot
    /// NACK on an unreliable frame, with nothing to raise it again on a session quiet enough to send
    /// no further sections.
    pub const FLAG_WANT_INTEREST: u8 = 1 << 3;

    /// Append this header to `writer`.
    pub fn encode(&self, writer: &mut Writer) {
        writer.u8(self.kind.tag());
        writer.varint(u64::from(self.tick));
        // Zigzag delta: ack_tick trails `tick` on a snapshot but may lead it on an input frame,
        // so the difference genuinely takes both signs.
        writer.zigzag(i64::from(self.tick) - i64::from(self.ack_tick));
        writer.u32(self.ack_bits);
        writer.u32(self.ack_token);
        writer.i8(self.margin_ticks);
        writer.u8(self.flags);
        writer.varint(u64::from(self.entity_count));
    }

    /// Read a header from `reader`.
    pub fn decode(reader: &mut Reader<'_>) -> Result<Self, CodecError> {
        let kind = FrameKind::from_tag(reader.u8()?)?;
        let tick = u32::try_from(reader.varint()?).map_err(|_| CodecError::VarintOverflow)?;
        let ack_delta = reader.zigzag()?;
        // Saturating, not plain subtraction: `ack_delta` comes straight off the wire, so a hostile
        // peer can send a zigzag varint decoding to i64::MIN, and `tick - i64::MIN` overflows —
        // which panics in a debug build. A remote peer must never be able to panic the process.
        let ack_tick = u32::try_from(
            i64::from(tick)
                .saturating_sub(ack_delta)
                .clamp(0, i64::from(u32::MAX)),
        )
        .map_err(|_| CodecError::VarintOverflow)?;
        let ack_bits = reader.u32()?;
        let ack_token = reader.u32()?;
        let margin_ticks = reader.i8()?;
        let flags = reader.u8()?;
        let entity_count =
            u32::try_from(reader.varint()?).map_err(|_| CodecError::VarintOverflow)?;
        Ok(Self {
            kind,
            tick,
            ack_tick,
            ack_bits,
            ack_token,
            margin_ticks,
            flags,
            entity_count,
        })
    }
}

/// The reliable frame a joining peer sends, and the reply it gets.
///
/// **It is the one datagram OrbitNet does not authenticate, because it is what establishes the key
/// everything else is authenticated with.** [`crate::auth`] states exactly what that buys and what it
/// does not.
///
/// Its 16-byte [`Self::session_nonce`] is that key when no session secret is configured, and only a
/// nonce when one is — in which case [`Self::confirm`] carries the proof that the sender holds the same
/// secret. The offsets and widths are identical either way; the regime is a local decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Handshake {
    /// The sender's packed protocol version.
    pub protocol_version: u32,
    /// The sender's tick rate in hertz.
    pub tickrate: u16,
    /// The sender's **session identity**, or `0` for "no identity — seat me as a newcomer".
    ///
    /// A transport peer id names a CONNECTION and is reassigned on every reconnect, so it cannot answer
    /// "is this the player who was here a moment ago". This can: a client mints one token and resends it
    /// verbatim on every join, so the server matches a rejoiner to the session it dropped.
    ///
    /// **It is asserted by the client and verified by nobody.** Anything that must not be forgeable —
    /// account identity, entitlement, ban evasion — belongs in an authenticated layer above this field. What
    /// it is adequate for is the thing it exists for: a player whose link dropped getting their own entity
    /// back instead of a stranger's.
    pub session_id: u64,
    /// The 16 bytes this session's key is **taken from or derived with**, minted fresh by the joining
    /// peer on every join and carried in the clear.
    ///
    /// It is one field with two regimes, and which one is in force is a local decision neither end
    /// puts on the wire:
    ///
    /// | Regime | What these bytes are | What an on-path observer learns |
    /// | --- | --- | --- |
    /// | no session secret configured | the session key itself | everything the client knows |
    /// | a session secret configured | a **nonce**, fed with the secret to [`crate::auth::derive_session_key`] | the nonce, and nothing else |
    ///
    /// It is named for the nonce because that is the role that survives both regimes: a fresh draw per
    /// join, which is what keeps sequence numbers from restarting under a key an observer already has.
    ///
    /// All zeroes is refused by [`Handshake::check_compatibility`] under either regime: it is what a
    /// peer that sent no bytes at all decodes to, and under a secret it is also the one nonce a lazy
    /// caller would reuse across joins.
    pub session_nonce: [u8; KEY_LEN],
    /// The **server-minted resume token** this peer was handed the last time it presented
    /// [`Self::session_id`], quoted back to prove the identity is its own. `0` quotes none.
    ///
    /// **The identity names the player; this is what a claim on that identity has to quote.** The server mints
    /// it once per identity, sends it in [`Welcome::resume_token`], and matches it for equality against the
    /// token on the record a rejoiner is claiming. A presented value that does not match the record answers
    /// no resume.
    ///
    /// **What it closes**: a peer that merely OBSERVED another peer's session id — off a roster broadcast, a
    /// kill feed, a log line, a screenshot — cannot take that player's body, because it never saw the token.
    ///
    /// **What it does not close**: an on-path observer, who reads the welcome and can then quote the token
    /// verbatim. That is the same boundary [`Self::session_nonce`] already has under a session with no
    /// secret, and closing it needs a secret both ends already hold.
    ///
    /// The client persists it BESIDE the session id. A process that stored one and not the other presents a
    /// `0` here and is seated as a newcomer.
    pub resume_token: u64,
    /// Proof the sender holds this session's **shared secret**: [`crate::auth::confirm_tag`] over
    /// [`Self::session_nonce`] and [`Self::protocol_version`], under the key the secret derives. `0` is
    /// the absent value and means "this peer configured no secret".
    ///
    /// **A trailing, optional field.** It is what turns the one signalable misconfiguration into a
    /// readable rejection at the handshake instead of a session that silently drops every datagram;
    /// [`Handshake::check_compatibility`] is where that refusal happens.
    ///
    /// **It proves possession of the secret, not identity.** Everyone the game handed the secret to can
    /// produce a valid tag over any nonce they like, and the pair `(nonce, confirm)` is in the clear, so
    /// an on-path observer can copy it. What that observer still cannot do is derive the key — which is
    /// the whole of what a secret buys.
    pub confirm: u64,
}

impl Handshake {
    /// Build a handshake for this build at `tickrate`. Carries no session identity, no nonce, no resume
    /// token and no confirmation; see [`Handshake::with_session`], [`Handshake::with_nonce`],
    /// [`Handshake::with_resume_token`] and [`Handshake::with_confirm`].
    #[must_use]
    pub fn local(tickrate: u16) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            tickrate,
            session_id: 0,
            session_nonce: [0; KEY_LEN],
            resume_token: 0,
            confirm: 0,
        }
    }

    /// The same handshake, carrying a session identity.
    #[must_use]
    pub fn with_session(mut self, session_id: u64) -> Self {
        self.session_id = session_id;
        self
    }

    /// The same handshake, carrying the session nonce — which is the session key itself when no secret
    /// is configured. See [`Self::session_nonce`] for the two regimes.
    #[must_use]
    pub fn with_nonce(mut self, session_nonce: [u8; KEY_LEN]) -> Self {
        self.session_nonce = session_nonce;
        self
    }

    /// The same handshake, quoting the resume token a server issued for this identity.
    #[must_use]
    pub fn with_resume_token(mut self, resume_token: u64) -> Self {
        self.resume_token = resume_token;
        self
    }

    /// The same handshake, proving possession of the session secret. `0` proves none.
    ///
    /// The tag is [`crate::auth::confirm_tag`] over [`Self::session_nonce`] and this handshake's own
    /// [`Self::protocol_version`], under [`crate::auth::derive_session_key`]'s output — so a caller
    /// builds the rest of the handshake first and tags the version it is actually sending.
    #[must_use]
    pub fn with_confirm(mut self, confirm: u64) -> Self {
        self.confirm = confirm;
        self
    }

    /// Encode, including the leading magic.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::with_capacity(MAGIC.len() + 30 + KEY_LEN);
        writer.bytes(&MAGIC);
        writer.u32(self.protocol_version);
        writer.u16(self.tickrate);
        writer.u64(self.session_id);
        writer.bytes(&self.session_nonce);
        writer.u64(self.resume_token);
        writer.u64(self.confirm);
        writer.into_inner()
    }

    /// Decode, validating the magic.
    ///
    /// **Everything after the protocol version decodes best-effort**, to a zero tick rate, no session
    /// identity, an all-zero nonce, a `0` resume token and a `0` confirmation. That is not laxity:
    /// `handle_hello` answers a decode error by returning, so a peer whose handshake is short — an older
    /// build, a truncated frame — would be dropped in silence with no rejection message at all. Decoding
    /// it far enough to reach [`Handshake::check_compatibility`] is what produces the operator-readable
    /// version mismatch, and the same check refuses the all-zero nonce a short handshake leaves behind.
    ///
    /// A `0` [`Self::resume_token`] is the absent value and is refused a resume, not a decode: quoting no
    /// token is what a first-time joiner does. A `0` [`Self::confirm`] is refused nothing either, unless
    /// the reading peer holds a secret — see [`Handshake::check_compatibility`].
    pub fn decode(buf: &[u8]) -> Result<Self, CodecError> {
        let mut reader = Reader::new(buf);
        if reader.bytes(MAGIC.len())? != MAGIC {
            return Err(CodecError::BadMagic);
        }
        let protocol_version = reader.u32()?;
        let tickrate = reader.u16().unwrap_or(0);
        let session_id = reader.u64().unwrap_or(0);
        let mut session_nonce = [0u8; KEY_LEN];
        if let Ok(bytes) = reader.bytes(KEY_LEN) {
            session_nonce.copy_from_slice(bytes);
        }
        let resume_token = reader.u64().unwrap_or(0);
        let confirm = reader.u64().unwrap_or(0);
        Ok(Self {
            protocol_version,
            tickrate,
            session_id,
            session_nonce,
            resume_token,
            confirm,
        })
    }

    /// Check a remote handshake against ours, under the session secret this peer holds (`None` for a
    /// peer that holds none).
    ///
    /// Three rules, in the order an operator can act on them:
    ///
    /// 1. **Protocol major must match exactly.** Reported first, because a peer one major behind by
    ///    definition sends nothing else this build can read.
    /// 2. **The remote must carry a non-zero [`Self::session_nonce`].** All zeroes is what a peer that sent
    ///    none decodes to, and under a secret it is also the one nonce that would repeat across joins.
    /// 3. **A peer holding a secret must see a [`Self::confirm`] tag over that secret.** `secret` is the
    ///    already-folded 16 bytes from [`crate::auth::compress_secret`]; the tag is recomputed from the
    ///    remote's own nonce and version and compared.
    ///
    /// A differing tick rate is deliberately *not* an error — it is a policy decision for the caller,
    /// since some games legitimately let peers run at different rates. Nor is a differing session
    /// identity: every client mints its own.
    ///
    /// **[`Handshake::resume_token`] is not checked here either.** A wrong or absent token costs the peer its
    /// resume and nothing more: it is seated as a newcomer. Every honest first-time joiner quotes `0`, so
    /// refusing the connection over the token would lock all of them out.
    ///
    /// **The nonce and the confirmation are checked on `remote` only.** The local handshake in this call is
    /// a version reference built by [`Handshake::local`], and the accepting side mints neither — a
    /// session's nonce is the joiner's.
    ///
    /// **Only one direction of a secret misconfiguration is reportable, and this is it.** A peer holding a
    /// secret against a joiner holding none refuses the join here, with a message that says so, instead of
    /// seating a session whose every datagram then fails its tag. The reverse — a joiner holding a secret
    /// against a peer holding none — cannot be reported at all: the reply is sealed under a key the joiner
    /// will not derive, so nothing the accepting side sends can reach it. That case's symptom is a join
    /// that never completes.
    ///
    /// **The tag is bound to `remote.protocol_version`, not ours.** Rule 1 has already established that the
    /// two agree on major, and minor and patch are legitimately allowed to differ — so tagging against our
    /// own version would refuse every honest peer one patch away.
    pub fn check_compatibility(
        &self,
        remote: &Handshake,
        secret: Option<&[u8; KEY_LEN]>,
    ) -> Result<(), CodecError> {
        if protocol_major(remote.protocol_version) != protocol_major(self.protocol_version) {
            return Err(CodecError::ProtocolMismatch {
                theirs: remote.protocol_version,
                ours: self.protocol_version,
            });
        }
        if remote.session_nonce == [0u8; KEY_LEN] {
            return Err(CodecError::MissingSessionNonce);
        }
        if let Some(secret) = secret {
            let key = derive_session_key(secret, &remote.session_nonce);
            let expected = confirm_tag(&key, &remote.session_nonce, remote.protocol_version);
            // XOR then one test against zero, so nothing branches on the tag's CONTENTS — the same
            // property `crate::auth` folds a difference down for on the receive path. A comparison that
            // returned at the first differing byte would leak how much of a guessed tag was right.
            if (remote.confirm ^ expected) != 0 {
                return Err(CodecError::SecretMismatch);
            }
        }
        Ok(())
    }
}

/// One entity's slice of a [`FrameKind::ServerSnapshot`] frame — the metadata half.
///
/// Blocks are length-prefixed so a peer that does not (yet) know a slot — the entity's spawn may
/// still be in flight on the reliable spawner channel, or the manifest binding the slot may not
/// have landed — can skip the block cleanly instead of losing the rest of the packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateBlockMeta {
    /// The entity's dense session slot. Resolve it to a 64-bit entity id through the receiver's
    /// [`crate::slots::SlotTable`]; a slot this peer holds no binding for is skipped.
    pub slot: u16,
    /// The tick this block's state describes. Per-entity, at or before the frame tick: an entity
    /// whose latest simulation used predicted input broadcasts its newest *authoritative* tick.
    pub tick: u64,
    /// Whether the payload carries every property (no mask, no reference).
    pub full: bool,
    /// This block belongs to the state-sync lane: applied on receipt, never restored in rollback.
    pub state_lane: bool,
    /// For a masked block, the tick whose row the mask was diffed against.
    pub reference_tick: Option<u64>,
    /// Bytes of mask + payload remaining after the meta fields (consumed by body decode or skip).
    pub body_len: usize,
}

/// Block-level flags inside a state block.
const STATE_BLOCK_FULL: u8 = 1 << 0;
const STATE_BLOCK_LANE: u8 = 1 << 1;

/// Append one entity state block to `writer`.
///
/// `reference` is the previously-sent row this block may delta against; `None` (or a reference
/// tick the encoder no longer holds) forces a full block. The mask/payload are computed here so
/// every call site shares one definition of "changed".
/// Returns whether the block written was a full row rather than a masked delta.
///
/// The encoder has the last word on fullness; a caller may not infer it from having supplied a
/// `reference`. A reference is a request, not a guarantee: the delta branch is taken only when the
/// base names an **earlier** tick than the row being sent, and an entity whose tick has not
/// advanced since the last send hands over a reference equal to it. That block goes out full and
/// repairs a broken delta chain as well as a requested one, so the keyframe clock has to see it.
/// Inferring fullness at the call site makes the clock miss those, and the miss is invisible: the
/// block is correct either way, only the bookkeeping is wrong.
///
/// `slot` is the entity's dense session index, not its 64-bit id. It is written **fixed-width**
/// rather than as a varint: a `u16` varint costs 1 byte below 128 and 3 above it, so a session with
/// more than 128 entities would pay MORE than the flat 2 bytes for most of its blocks.
#[allow(clippy::too_many_arguments)]
pub fn encode_state_block(
    writer: &mut Writer,
    scratch: &mut Vec<bool>,
    props: &[PropSchema],
    slot: u16,
    frame_tick: u64,
    entity_tick: u64,
    reference: Option<(u64, &[u8])>,
    row: &[u8],
    state_lane: bool,
) -> bool {
    writer.u16(slot);

    let mut body = Writer::new();
    let mut flags = 0u8;
    if state_lane {
        flags |= STATE_BLOCK_LANE;
    }
    let full = match reference {
        Some((reference_tick, base)) if reference_tick < entity_tick => {
            changed_mask(props, base, row, scratch);
            body.u8(flags);
            body.varint(entity_tick - reference_tick);
            body.bitmask(scratch);
            let before = body.len();
            crate::quant::write_masked_wire(props, scratch, row, &mut body.buf);
            debug_assert_eq!(
                body.len() - before,
                crate::quant::masked_wire_size(props, scratch)
            );
            false
        }
        _ => {
            body.u8(flags | STATE_BLOCK_FULL);
            crate::quant::encode_row(props, row, &mut body.buf);
            true
        }
    };

    writer.varint(frame_tick.saturating_sub(entity_tick));
    writer.varint(body.len() as u64);
    writer.bytes(body.as_slice());
    full
}

/// Read a state block's metadata, leaving the reader at its mask/payload.
///
/// The caller either resolves `meta.slot` to a local entity and calls [`decode_state_block_into`],
/// or calls [`skip_state_block_body`] to hop over an entity it cannot name.
pub fn decode_state_block_meta(
    reader: &mut Reader<'_>,
    frame_tick: u64,
) -> Result<StateBlockMeta, CodecError> {
    let slot = reader.u16()?;
    let tick_delta = reader.varint()?;
    let tick = frame_tick.saturating_sub(tick_delta);
    let body_len = usize::try_from(reader.varint()?).map_err(|_| CodecError::VarintOverflow)?;
    if body_len > reader.remaining() || body_len < 1 {
        return Err(CodecError::UnexpectedEof);
    }
    let before = reader.remaining();
    let flags = reader.u8()?;
    let full = flags & STATE_BLOCK_FULL != 0;
    let state_lane = flags & STATE_BLOCK_LANE != 0;
    let reference_tick = if full {
        None
    } else {
        let ref_delta = reader.varint()?;
        Some(tick.saturating_sub(ref_delta))
    };
    let consumed = before - reader.remaining();
    // The flags/ref-delta bytes count against the declared body length; a hostile length smaller
    // than what was just consumed must reject as malformed, not underflow the subtraction.
    let Some(body_len) = body_len.checked_sub(consumed) else {
        return Err(CodecError::UnexpectedEof);
    };
    Ok(StateBlockMeta {
        slot,
        tick,
        full,
        state_lane,
        reference_tick,
        body_len,
    })
}

/// Skip the mask/payload of a block whose entity is unknown locally.
pub fn skip_state_block_body(
    reader: &mut Reader<'_>,
    meta: &StateBlockMeta,
) -> Result<(), CodecError> {
    reader.bytes(meta.body_len).map(|_| ())
}

/// Decode a state block's mask/payload into `out_row`.
///
/// For a full block, `out_row` is overwritten entirely. For a masked block, `base_row` must be
/// the local copy of the row at `meta.reference_tick`; pass `None` when that base is not held —
/// the block is then skipped (returns `Ok(false)`) and the caller should raise
/// [`FrameHeader::FLAG_WANT_FULL`].
pub fn decode_state_block_into(
    reader: &mut Reader<'_>,
    meta: &StateBlockMeta,
    props: &[PropSchema],
    scratch: &mut Vec<bool>,
    base_row: Option<&[u8]>,
    out_row: &mut [u8],
) -> Result<bool, CodecError> {
    if meta.full {
        let payload = reader.bytes(meta.body_len)?;
        if payload.len() != crate::quant::wire_row_stride(props) {
            // Schema disagreement (the manifest check should have caught it) — refuse to
            // misapply rather than shear the row.
            return Ok(false);
        }
        return Ok(crate::quant::decode_row(props, payload, out_row).is_some());
    }

    let mask_bytes = props.len().div_ceil(8);
    if mask_bytes > meta.body_len {
        return Err(CodecError::UnexpectedEof);
    }
    reader.bitmask_into(props.len(), scratch)?;
    let payload = reader.bytes(meta.body_len - mask_bytes)?;
    let Some(base) = base_row else {
        return Ok(false);
    };
    if base.len() != out_row.len() {
        return Ok(false);
    }
    out_row.copy_from_slice(base);
    match crate::quant::apply_masked_wire(props, scratch, payload, out_row) {
        Some(consumed) if consumed == payload.len() => Ok(true),
        Some(_) => Ok(false),
        None => Err(CodecError::UnexpectedEof),
    }
}

/// One entity's slice of a [`FrameKind::ClientInput`] frame.
///
/// Input rides full rows with redundancy — the newest `count` ticks, descending — because input
/// rows are small and redundancy is the loss armor: a lost packet's ticks arrive in the next one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputBlockMeta {
    /// The entity's dense session slot, resolved through the receiver's
    /// [`crate::slots::SlotTable`]. A slot the server holds no binding for is skipped.
    pub slot: u16,
    /// The newest input tick carried.
    pub newest_tick: u64,
    /// How many consecutive rows follow (newest first, ticks descending by one).
    pub count: u32,
    /// Bytes of row payload remaining.
    pub body_len: usize,
}

/// Append one entity input block: the newest `rows.len()` ticks, newest first.
///
/// `slot` is the entity's dense session index. A client learns it from the entity manifest, so a
/// body whose slot has not arrived yet sends no input block at all rather than one the server would
/// have to guess at.
pub fn encode_input_block(
    writer: &mut Writer,
    props: &[PropSchema],
    slot: u16,
    frame_tick: u64,
    newest_tick: u64,
    rows: &[&[u8]],
) {
    writer.u16(slot);
    // Zigzag: an input stamp normally LEADS the frame tick (input delay / adaptive lead), so the
    // delta genuinely takes both signs.
    writer.zigzag(frame_tick as i64 - newest_tick as i64);
    let wire_stride = crate::quant::wire_row_stride(props);
    let count = rows.len().min(255);
    writer.varint((wire_stride * count) as u64 + 1);
    writer.u8(u8::try_from(count).unwrap_or(255));
    for row in rows.iter().take(255) {
        crate::quant::encode_row(props, row, &mut writer.buf);
    }
}

/// Read an input block's metadata, leaving the reader at the packed rows.
pub fn decode_input_block_meta(
    reader: &mut Reader<'_>,
    frame_tick: u64,
) -> Result<InputBlockMeta, CodecError> {
    let slot = reader.u16()?;
    let tick_delta = reader.zigzag()?;
    // Saturating on both sides: the delta comes off the wire and must not overflow either way.
    let newest_tick = if tick_delta >= 0 {
        frame_tick.saturating_sub(tick_delta.unsigned_abs())
    } else {
        frame_tick.saturating_add(tick_delta.unsigned_abs())
    };
    let body_len = usize::try_from(reader.varint()?).map_err(|_| CodecError::VarintOverflow)?;
    if body_len > reader.remaining() || body_len < 1 {
        return Err(CodecError::UnexpectedEof);
    }
    let count = u32::from(reader.u8()?);
    Ok(InputBlockMeta {
        slot,
        newest_tick,
        count,
        body_len: body_len - 1,
    })
}

/// Borrow the `index`-th WIRE input row (0 = newest) of a block, validating the stride math.
///
/// `wire_stride` is the receiver's [`crate::quant::wire_row_stride`] for the entity's input
/// schema. Returns `None` when the advertised count/stride and the actual payload disagree — a
/// malformed block yields no rows rather than sheared ones. The caller decodes the returned wire
/// bytes into a native row with [`crate::quant::decode_row`].
pub fn input_block_row<'a>(
    reader: &Reader<'a>,
    meta: &InputBlockMeta,
    wire_stride: usize,
    index: u32,
) -> Option<&'a [u8]> {
    if wire_stride == 0 || index >= meta.count {
        return None;
    }
    let expected = wire_stride.checked_mul(meta.count as usize)?;
    if expected != meta.body_len {
        return None;
    }
    let start = wire_stride * index as usize;
    reader.peek_bytes(start, wire_stride)
}

/// Advance the reader past an input block's rows.
pub fn skip_input_block_body(
    reader: &mut Reader<'_>,
    meta: &InputBlockMeta,
) -> Result<(), CodecError> {
    reader.bytes(meta.body_len).map(|_| ())
}

/// The reliable join reply: the tick seed and rate the client starts from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Welcome {
    /// The server's packed protocol version.
    pub protocol_version: u32,
    /// The server's current simulation tick at send time.
    pub server_tick: u64,
    /// The server's configured tick rate in hertz.
    pub tickrate: u16,
    /// The **resume token** this server issued for the identity the peer's handshake carried, or `0` for a
    /// peer it seated without one.
    ///
    /// Minted once per identity and re-sent on every welcome that identity is granted, so the retried
    /// handshake a lost welcome provokes gets the same value rather than a fresh one.
    ///
    /// **The client stores it beside the session id and quotes it back in
    /// [`Handshake::resume_token`].** A `0` here means "this connection holds no identity of ours" — a
    /// peer that claimed none, or one whose resume was refused — and the client keeps whatever it already
    /// had rather than overwriting a live token with nothing.
    pub resume_token: u64,
}

impl Welcome {
    /// Encode, with the frame kind tag leading.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::with_capacity(24);
        writer.u8(FrameKind::Welcome.tag());
        writer.u32(self.protocol_version);
        writer.varint(self.server_tick);
        writer.u16(self.tickrate);
        writer.u64(self.resume_token);
        writer.into_inner()
    }

    /// Decode the payload after the kind tag has been consumed.
    ///
    /// **[`Self::resume_token`] decodes best-effort to `0`**, the same rule the handshake's own trailing
    /// fields follow: a frame that stops before it yields the documented absent value rather than an error,
    /// and a welcome that failed to decode would leave a joining client unsynced with nothing to say why.
    pub fn decode(reader: &mut Reader<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            protocol_version: reader.u32()?,
            server_tick: reader.varint()?,
            tickrate: reader.u16()?,
            resume_token: reader.u64().unwrap_or(0),
        })
    }
}

/// A clock probe. The client stamps its send time; the server echoes it with its own clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ping {
    /// Client-chosen sequence number.
    pub seq: u64,
    /// Client monotonic microseconds at send.
    pub client_us: u64,
}

impl Ping {
    /// Encode, with the frame kind tag leading.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::with_capacity(12);
        writer.u8(FrameKind::Ping.tag());
        writer.varint(self.seq);
        writer.varint(self.client_us);
        writer.into_inner()
    }

    /// Decode the payload after the kind tag has been consumed.
    pub fn decode(reader: &mut Reader<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            seq: reader.varint()?,
            client_us: reader.varint()?,
        })
    }
}

/// A clock probe reply: the echoed client stamp plus the server's simulation time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pong {
    /// Echoed sequence number.
    pub seq: u64,
    /// Echoed client monotonic microseconds from the ping.
    pub client_us: u64,
    /// The server's simulation clock, in seconds since its tick 0, at reply time.
    pub server_time: f64,
}

impl Pong {
    /// Encode, with the frame kind tag leading.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::with_capacity(20);
        writer.u8(FrameKind::Pong.tag());
        writer.varint(self.seq);
        writer.varint(self.client_us);
        writer.f64(self.server_time);
        writer.into_inner()
    }

    /// Decode the payload after the kind tag has been consumed.
    pub fn decode(reader: &mut Reader<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            seq: reader.varint()?,
            client_us: reader.varint()?,
            server_time: reader.f64()?,
        })
    }
}

/// One entity's row in a [`FrameKind::EntityManifest`] frame: its slot binding, its schema
/// fingerprints, and the seat that drives it.
///
/// **The manifest names every replicated entity, both lanes.** It covered the rollback lane only
/// while it was purely a schema check, because a state-lane entity has no input schema to disagree
/// about. It is now also the only channel that tells a client what a wire slot names, and state-lane
/// blocks carry slots too, so it has to name every replicated entity.
///
/// **THE SEAT ROSTER RIDES HERE RATHER THAN ON A FRAME OF ITS OWN.** A seat exists because some
/// entity says it is driven by that connection under that label ([`crate::seats`]), so the roster is
/// a projection of this table and cannot disagree with it. A separate frame would be a second source
/// of truth arriving on its own schedule, and the two would differ for exactly as long as one of
/// them was in flight.
///
/// # What one row costs, from the encoder rather than from an estimate
///
/// | Field | Bytes |
/// | --- | --- |
/// | `slot` | 2, fixed |
/// | `id`, an LEB128 varint over a full-width FNV-1a hash | **9.5 on average**, uniform over 2<sup>64</sup> |
/// | `state_hash` | 4 |
/// | `input_hash` | 4 |
/// | `owner`, a varint over a small positive peer id | 1 |
/// | `seat` | 2 |
/// | **one row** | **~22.5** |
///
/// That is what made restating the whole table on every change untenable: the table is rebuilt and
/// broadcast whenever anything dirties it — a registration, an unregistration, a slot reconcile, a
/// seat or authority write, or any hello — and it is flushed once per frame that advanced a tick, so
/// the ceiling was one whole-table republish per net tick per peer. At 8,000 named entities that is
/// ~180 kB per peer per republish, against an unreliable hot lane of ~36 kB/s per peer.
/// [`ManifestDelta`] is what a change costs instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManifestEntry {
    /// The dense session slot this entity's blocks are named by on the wire.
    ///
    /// **The key of the whole table.** Both halves of a [`ManifestDelta`] name a row by its slot: a
    /// removal is a bare slot, and an addition replaces whatever the receiver held at that slot.
    pub slot: u16,
    /// Stable entity id (the FNV-1a hash of the synchronizer root's node path).
    pub id: u64,
    /// Hash of the entity's state schema.
    pub state_hash: u32,
    /// Hash of the entity's input schema. `0` for a state-lane entity, which has no input schema.
    pub input_hash: u32,
    /// The transport peer whose input drives this entity, or `0` when nobody's does.
    ///
    /// **Not an authority grant.** The server still checks a received input block against the
    /// entity's live multiplayer authority; this is what it has already decided, published so a
    /// client can name the same seats the server does.
    ///
    /// **A DEDICATED SERVER WRITES `0` FOR A BODY IT HOLDS ITSELF.** Handing input back to peer 1 is
    /// how a game says a body is unclaimed, and a server with no local player has no viewpoint to
    /// announce; a LISTEN server writes `1`, because there peer 1 is the host player. The two are
    /// indistinguishable to a client, so the decision is made where it is known.
    pub owner: i32,
    /// Which seat on `owner` drives it — `0` for every body that declares none, and meaningless
    /// when `owner` is `0`.
    pub seat: SeatIndex,
}

/// A change to the entity manifest, stated against the exact table it was computed from.
///
/// ```text
/// kind 0x08 | base_generation varint | generation varint
///           | removed_count varint | R x slot u16
///           | added_count varint   | A x entry     (the ManifestEntry layout, unchanged)
/// ```
///
/// **One record covers three cases**, because binding a slot already replaces both directions
/// ([`crate::slots::SlotTable::bind`]): a slot that was not bound, a slot reissued to a different
/// entity, and a row whose `owner`, `seat` or schema hash changed on a slot that stayed bound are
/// all one `added` row. Applying an `added` row is therefore **idempotent**, and a receiver needs no
/// case analysis to apply one.
///
/// **`generation` is sent explicitly rather than implied as `base_generation + 1`**, so a server may
/// coalesce several dirty ticks into one delta and a receiver still lands on the number the server
/// holds.
///
/// # What a delta gives up, and what replaces it
///
/// Rebuilding from a complete table was **self-repairing**: it retired the binding of every entity
/// that had unregistered since the last frame, with no removal record to lose. A delta reintroduces
/// a removal record. A receiver that misses one keeps a slot bound to an entity the server has
/// unregistered; past [`crate::slots::SLOT_QUARANTINE_TICKS`] that slot is reissued, and the stale
/// receiver applies the new entity's rows to the old one — silently, with every block decoding
/// cleanly.
///
/// Three things stand in for the complete table, and all three are needed:
///
/// | Guarantee | What it covers |
/// | --- | --- |
/// | The manifest channel is **reliable and ordered** | a removal cannot be dropped or reordered while the connection lives |
/// | `base_generation` names the exact table this was computed from | a receiver holding any other table refuses the delta rather than half-applying it |
/// | Every way the stream can break resolves to a **full table** | a reconnect, a rekey on a live connection, an undecodable delta, and a delta against the wrong base |
///
/// The generation counter is **not loss recovery** — the channel already gives that. It is what
/// makes "this receiver is not holding the table I diffed against" detectable at all, and every
/// path that can desynchronize a receiver has to zero it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ManifestDelta {
    /// The generation of the table this delta was computed against.
    ///
    /// A receiver holding any other generation **cannot apply it** and asks for a full table with
    /// [`FrameHeader::FLAG_WANT_MANIFEST`].
    pub base_generation: u64,
    /// The generation the table stands at once this delta is applied.
    pub generation: u64,
    /// Slots whose binding is retired, ascending. **Two bytes each**: a row is named by its slot, so
    /// nothing else about it has to be restated to drop it.
    pub removed: Vec<u16>,
    /// Rows that are in force now, ascending by slot. A new binding, a rebind of a reissued slot and
    /// a changed row on a slot that stayed bound are all this, at the full ~22.5 bytes of a row.
    pub added: Vec<ManifestEntry>,
}

impl ManifestDelta {
    /// Whether a receiver holding `generation` may apply this delta.
    ///
    /// **The receiver's whole gate, as one named rule a test can call.** A receiver at any other
    /// generation is not holding the table this was diffed against, so applying it would leave a
    /// table that matches neither end. It refuses instead, zeroes its own generation, and raises
    /// [`FrameHeader::FLAG_WANT_MANIFEST`] to be sent the whole table.
    #[must_use]
    pub fn applies_to(&self, generation: u64) -> bool {
        self.base_generation == generation
    }
}

/// Encode a whole entity manifest, at the generation it stands at.
///
/// ```text
/// kind 0x07 | generation varint | count varint | count x entry
/// ```
///
/// **The leading generation is what made this a major protocol bump**: it shifts the offset of every
/// field after it, so a peer that does not know about it reads the count out of the generation's
/// bytes and decodes garbage rather than stopping short.
///
/// A full table is what a receiver gets when it holds no table, or holds one this server cannot
/// diff against. Every other publish is an [`encode_manifest_delta`].
#[must_use]
pub fn encode_manifest_full(generation: u64, entries: &[ManifestEntry]) -> Vec<u8> {
    let mut writer = Writer::with_capacity(12 + entries.len() * 23);
    writer.u8(FrameKind::EntityManifest.tag());
    writer.varint(generation);
    write_manifest_entries(entries, &mut writer);
    writer.into_inner()
}

/// Decode a whole entity manifest after the kind tag has been consumed, answering
/// `(generation, entries)`.
pub fn decode_manifest_full(
    reader: &mut Reader<'_>,
) -> Result<(u64, Vec<ManifestEntry>), CodecError> {
    let generation = reader.varint()?;
    Ok((generation, read_manifest_entries(reader)?))
}

/// Encode one entity-manifest delta. See [`ManifestDelta`] for the layout and the guarantees.
#[must_use]
pub fn encode_manifest_delta(delta: &ManifestDelta) -> Vec<u8> {
    let mut writer = Writer::with_capacity(24 + delta.removed.len() * 2 + delta.added.len() * 23);
    writer.u8(FrameKind::EntityManifestDelta.tag());
    writer.varint(delta.base_generation);
    writer.varint(delta.generation);
    writer.varint(delta.removed.len() as u64);
    for &slot in &delta.removed {
        writer.u16(slot);
    }
    write_manifest_entries(&delta.added, &mut writer);
    writer.into_inner()
}

/// Decode one entity-manifest delta after the kind tag has been consumed.
///
/// Bounds-checked like every other decoder here, and **both counts are capped the way
/// [`decode_manifest_full`] caps its own**: the reserve is `count.min(4096)`, never `count`, so a
/// four-byte frame claiming `u64::MAX` records reports [`CodecError::UnexpectedEof`] when the reads
/// run out of buffer instead of driving a remote out-of-memory. The cap bounds the *reserve* and not
/// the record count, so a legitimate delta larger than 4096 records still decodes.
pub fn decode_manifest_delta(reader: &mut Reader<'_>) -> Result<ManifestDelta, CodecError> {
    Ok(ManifestDelta {
        // Field order IS wire order: a struct literal evaluates its fields as written, and every one
        // of these reads advances the same cursor.
        base_generation: reader.varint()?,
        generation: reader.varint()?,
        removed: decode_slot_run(reader)?,
        added: read_manifest_entries(reader)?,
    })
}

/// The minimal delta that carries `previous` to `current`, as `(removed slots, added rows)`.
///
/// **Pure and order-independent.** Both tables are keyed by [`ManifestEntry::slot`], because the
/// slot is what a removal names on the wire; neither argument has to arrive sorted, and both halves
/// of the answer come out ascending by slot.
///
/// | Change | What it produces |
/// | --- | --- |
/// | a row on a slot that was not bound | one `added` row |
/// | a row gone from a slot with nothing to replace it | one `removed` slot |
/// | a slot reissued to a different entity | one `added` row — a bind replaces both directions |
/// | `owner`, `seat` or either schema hash changed on an unmoved slot | one `added` row |
/// | an entity that moved from one slot to another | one `removed` slot and one `added` row |
/// | a row that did not change | **nothing** |
///
/// The last line is the one that matters: the frame is rebuilt whenever anything dirties it, and
/// almost every rebuild reproduces a table identical to the one already published.
#[must_use]
pub fn diff_manifest(
    previous: &[ManifestEntry],
    current: &[ManifestEntry],
) -> (Vec<u16>, Vec<ManifestEntry>) {
    let mut before: BTreeMap<u16, ManifestEntry> =
        previous.iter().map(|entry| (entry.slot, *entry)).collect();
    let mut added: Vec<ManifestEntry> = Vec::new();
    for entry in current {
        // Taking the row out is what leaves `before` holding exactly the removals at the end: a slot
        // that survived has been consumed, whether or not its row changed.
        match before.remove(&entry.slot) {
            Some(held) if held == *entry => {}
            _ => added.push(*entry),
        }
    }
    added.sort_unstable_by_key(|entry| entry.slot);
    // `BTreeMap::into_keys` is already ascending, which is the order the wire wants.
    (before.into_keys().collect(), added)
}

/// Apply `delta` to a table of rows, answering the table it reaches. The inverse of
/// [`diff_manifest`], and pure for the same reason: the algebra is testable without a session.
///
/// **Removals are applied before additions**, and the order is load-bearing rather than incidental.
/// A well-formed delta never names one slot in both halves — [`diff_manifest`] cannot produce that —
/// but the bytes are chosen by a remote peer, and applying the removals second would drop a row the
/// same frame had just installed.
#[must_use]
pub fn apply_manifest_delta(rows: &[ManifestEntry], delta: &ManifestDelta) -> Vec<ManifestEntry> {
    let mut table: BTreeMap<u16, ManifestEntry> =
        rows.iter().map(|entry| (entry.slot, *entry)).collect();
    for &slot in &delta.removed {
        table.remove(&slot);
    }
    for entry in &delta.added {
        table.insert(entry.slot, *entry);
    }
    table.into_values().collect()
}

/// Write `count varint | count x entry`, the run both manifest frames carry.
fn write_manifest_entries(entries: &[ManifestEntry], writer: &mut Writer) {
    writer.varint(entries.len() as u64);
    for entry in entries {
        writer.u16(entry.slot);
        // The full 64-bit id, still a varint. This is the one frame that has to carry it — a
        // receiver derives the same id from the same node path and needs the pairing to find its
        // own entity — and it now rides only when THAT ROW changed rather than whenever anything in
        // the table did, so its ~9.5 bytes are spent per entity per change to that entity.
        writer.varint(entry.id);
        writer.u32(entry.state_hash);
        writer.u32(entry.input_hash);
        // A transport peer id is positive and small — 1 for the server, then one per joiner — so a
        // varint spends one byte on every session that will ever exist. A negative value is not a
        // peer id and is written as the `0` that means "nobody drives this".
        writer.varint(entry.owner.max(0) as u64);
        writer.u16(entry.seat);
    }
}

/// Read `count varint | count x entry`.
///
/// The reserve is `count.min(4096)` and never `count`: a one-byte count field can claim `u64::MAX`
/// rows, and reserving for that claim is a remote out-of-memory rather than a decode error.
fn read_manifest_entries(reader: &mut Reader<'_>) -> Result<Vec<ManifestEntry>, CodecError> {
    let count = reader.varint()?;
    // Each entry is at least 14 bytes; a hostile count cannot make us over-allocate.
    let cap = usize::try_from(count.min(4096)).unwrap_or(0);
    let mut entries = Vec::with_capacity(cap);
    for _ in 0..count {
        // Field order here IS wire order: a struct literal evaluates its fields as written, and
        // every one of these reads advances the same cursor.
        entries.push(ManifestEntry {
            slot: reader.u16()?,
            id: reader.varint()?,
            state_hash: reader.u32()?,
            input_hash: reader.u32()?,
            // A peer id past `i32::MAX` cannot have been minted by any transport, so it is a
            // corrupt or hostile frame; it reads as unowned rather than as a wrapped id that would
            // name somebody.
            owner: i32::try_from(reader.varint()?).unwrap_or(0),
            seat: reader.u16()?,
        });
    }
    Ok(entries)
}

/// Which entities became relevant to ONE peer, and which stopped being, since the last such section
/// that peer acknowledged.
///
/// **It rides the snapshot frame that peer is already receiving**, appended after the entity blocks
/// and announced by [`FrameHeader::FLAG_INTEREST_DELTA`]. The manifest cannot carry this: that frame
/// is a session-wide table broadcast identically to every peer, so it says "this entity exists" and
/// never "this entity is relevant to you".
///
/// **Slots, not ids.** A 64-bit entity id is an FNV-1a hash spread across the whole range and costs
/// ~9.5 bytes as a varint; a slot is a flat 2. The receiver resolves each one against the table the
/// manifest already gave it and **ignores an unbound slot silently**, exactly as `handle_snapshot`
/// already ignores a block naming one. That case is not rare — a leave whose cause is an unregister
/// names a slot the very next manifest releases — and [`crate::slots`]'s 256-tick reuse quarantine
/// is what stops a released slot naming a different entity inside the window a snapshot can be
/// reordered by.
///
/// **The section is applied idempotently**: remove each `left` slot from a mirrored set, add each
/// `entered` slot to it, and emit only on a set that actually changed. That is what makes a re-send
/// free, which is what lets an unreliable datagram carry an event at all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InterestDeltaSection {
    /// The generation of the interest set this delta was built against.
    ///
    /// **IT PLACES THE SECTION, AND THE MATCH IS EXACT** — see [`Self::applies_to`]. A section
    /// states a change against one baseline, and a receiver holding any other is not holding the set
    /// it was diffed from. A whole [`FrameKind::InterestTable`] is reliable and a section is not, so
    /// a section built either side of a table can arrive on the wrong side of it.
    ///
    /// **It is not a chain.** [`ManifestDelta::base_generation`] names one exact predecessor and
    /// refuses a gap, because its channel is ordered and a gap there is a fault. This moves only when
    /// a whole set is sent, so an ordinary run of sections all carry the same one and a dropped
    /// datagram costs nothing.
    pub generation: u64,
    /// Slots that left this peer's interest, ascending.
    pub left: Vec<u16>,
    /// Slots that entered it, ascending.
    pub entered: Vec<u16>,
}

impl InterestDeltaSection {
    /// Whether a receiver holding `generation` may apply this section.
    ///
    /// **EXACT, DELIBERATELY.** A section states a change against one baseline, and a receiver
    /// holding any other is not holding the set it was diffed from. Greater-or-equal would admit
    /// the case this exists to refuse: a table is reliable and a section is not, so a section built
    /// AFTER a table can arrive before it, and applying it early would have the table then undo it.
    ///
    /// A re-send of a prefix carries the generation it was built at, so retransmission — which the
    /// whole reliability model rests on — still matches. A receiver that drops a section for this
    /// reason asks for the whole set, exactly as it does for a slot it cannot name.
    #[must_use]
    pub fn applies_to(&self, generation: u64) -> bool {
        self.generation == generation
    }
}

/// Append an interest-delta section to `writer`.
///
/// Layout, in order:
/// `varint generation | varint left_count | [u16 slot]* | varint entered_count | [u16 slot]*`. The
/// counts are varints because they are usually one byte and never need more than three at a payload
/// this size; every slot is a fixed 2 bytes, for the reason [`ManifestEntry::slot`] states.
///
/// **The leading generation is what made this a major bump.** It shifts the offset of both counts,
/// so a peer that does not know about it reads a slot run out of the generation's bytes.
pub fn encode_interest_delta(generation: u64, left: &[u16], entered: &[u16], writer: &mut Writer) {
    writer.varint(generation);
    writer.varint(left.len() as u64);
    for &slot in left {
        writer.u16(slot);
    }
    writer.varint(entered.len() as u64);
    for &slot in entered {
        writer.u16(slot);
    }
}

/// Decode an interest-delta section from the bytes after a frame's entity blocks.
///
/// Bounds-checked like every other decoder here: a hostile count reports
/// [`CodecError::UnexpectedEof`] rather than driving an allocation, because the reserve is capped at
/// the same 4096 [`decode_manifest_full`] uses and the reads that follow run out of buffer.
pub fn decode_interest_delta(reader: &mut Reader<'_>) -> Result<InterestDeltaSection, CodecError> {
    Ok(InterestDeltaSection {
        // Field order IS wire order: a struct literal evaluates its fields as written, and all
        // three of these advance the same cursor.
        generation: reader.varint()?,
        left: decode_slot_run(reader)?,
        entered: decode_slot_run(reader)?,
    })
}

/// Encode one peer's whole interest set, at the generation it stands at.
///
/// ```text
/// kind 0x09 | generation varint | count varint | count x u16 slot
/// ```
///
/// **A SET RATHER THAN A DIFF**, because this frame exists for the cases where no diff can be
/// trusted: a delta naming a slot the receiver could not resolve, a pending queue that overflowed,
/// and a prefix given up on unacknowledged. Each leaves the two ends disagreeing about the set
/// itself, and only a set settles that.
///
/// It is **per peer**, which no other reliable frame here is — an interest set is the one piece of
/// server state that is not the same for everybody. At 2 bytes a slot a thousand-entity set is 2 kB
/// on a reliable channel, which ENet fragments; the manifest already sends its whole table the same
/// way.
///
/// The receiver adopts the set wholesale, emits its own enters and leaves by diffing against what it
/// held, and stores `generation` so a delta built before this frame cannot undo it.
pub fn encode_interest_table(generation: u64, slots: &[u16]) -> Vec<u8> {
    let mut writer = Writer::with_capacity(12 + slots.len() * 2);
    writer.u8(FrameKind::InterestTable.tag());
    writer.varint(generation);
    writer.varint(slots.len() as u64);
    for &slot in slots {
        writer.u16(slot);
    }
    writer.into_inner()
}

/// Decode an interest table, returning its generation and the slots it names.
///
/// The kind byte is expected to be consumed already, exactly as [`decode_manifest_full`] expects.
/// Bounds-checked the same way: a hostile count reserves at most 4096 and then runs out of buffer.
pub fn decode_interest_table(reader: &mut Reader<'_>) -> Result<(u64, Vec<u16>), CodecError> {
    let generation = reader.varint()?;
    Ok((generation, decode_slot_run(reader)?))
}

/// One `varint count | [u16 slot]*` run.
///
/// The capacity is `count.min(4096)` and never `count`: a two-byte frame can claim `u64::MAX` slots,
/// and reserving for that claim is a remote out-of-memory rather than a decode error.
fn decode_slot_run(reader: &mut Reader<'_>) -> Result<Vec<u16>, CodecError> {
    let count = reader.varint()?;
    let cap = usize::try_from(count.min(4096)).unwrap_or(0);
    let mut slots = Vec::with_capacity(cap);
    for _ in 0..count {
        slots.push(reader.u16()?);
    }
    Ok(slots)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_round_trip_across_widths() {
        for value in [
            0u64,
            1,
            127,
            128,
            300,
            16_383,
            16_384,
            u64::from(u32::MAX),
            u64::MAX,
        ] {
            let mut writer = Writer::new();
            writer.varint(value);
            let bytes = writer.into_inner();
            let mut reader = Reader::new(&bytes);
            assert_eq!(reader.varint().unwrap(), value, "varint {value} broke");
            assert!(reader.is_exhausted());
        }
    }

    #[test]
    fn small_varints_are_actually_small() {
        let mut writer = Writer::new();
        writer.varint(127);
        assert_eq!(writer.len(), 1);
        writer.clear();
        writer.varint(128);
        assert_eq!(writer.len(), 2);
        assert!(!writer.is_empty());
    }

    #[test]
    fn zigzag_round_trips_both_signs() {
        for value in [0i64, 1, -1, 63, -64, i32::MAX as i64, i64::MIN, i64::MAX] {
            let mut writer = Writer::new();
            writer.zigzag(value);
            let bytes = writer.into_inner();
            let mut reader = Reader::new(&bytes);
            assert_eq!(reader.zigzag().unwrap(), value, "zigzag {value} broke");
        }
    }

    #[test]
    fn varint_overflow_is_an_error_not_a_panic() {
        // Eleven continuation bytes cannot fit in a u64.
        let bytes = [0xffu8; 11];
        let mut reader = Reader::new(&bytes);
        assert_eq!(reader.varint(), Err(CodecError::VarintOverflow));
    }

    #[test]
    fn truncated_reads_report_eof() {
        let bytes = [1u8, 2];
        let mut reader = Reader::new(&bytes);
        assert_eq!(reader.u32(), Err(CodecError::UnexpectedEof));
        let mut reader = Reader::new(&bytes);
        assert_eq!(reader.bytes(9), Err(CodecError::UnexpectedEof));
        let mut reader = Reader::new(&[]);
        assert_eq!(reader.u8(), Err(CodecError::UnexpectedEof));
        assert_eq!(reader.varint(), Err(CodecError::UnexpectedEof));
    }

    #[test]
    fn scalars_round_trip() {
        let mut writer = Writer::new();
        writer.u8(0xab);
        writer.i8(-5);
        writer.u16(0x1234);
        writer.u32(0xdead_beef);
        writer.f32(1.5);
        writer.f64(-2.25);
        writer.bytes(b"hi");
        let bytes = writer.into_inner();

        let mut reader = Reader::new(&bytes);
        assert_eq!(reader.u8().unwrap(), 0xab);
        assert_eq!(reader.i8().unwrap(), -5);
        assert_eq!(reader.u16().unwrap(), 0x1234);
        assert_eq!(reader.u32().unwrap(), 0xdead_beef);
        assert_eq!(reader.f32().unwrap(), 1.5);
        assert_eq!(reader.f64().unwrap(), -2.25);
        assert_eq!(reader.bytes(2).unwrap(), b"hi");
        assert!(reader.is_exhausted());
    }

    /// The lossless path must reproduce a float exactly, or a bit-exact resimulation cannot hold.
    #[test]
    fn f64_round_trips_bit_exactly() {
        let awkward = [
            0.1f64,
            1.0 / 3.0,
            f64::MIN_POSITIVE,
            -0.0,
            1e300,
            std::f64::consts::PI,
        ];
        for value in awkward {
            let mut writer = Writer::new();
            writer.f64(value);
            let bytes = writer.into_inner();
            let decoded = Reader::new(&bytes).f64().unwrap();
            assert_eq!(
                decoded.to_bits(),
                value.to_bits(),
                "f64 {value} lost bits on the wire"
            );
        }
    }

    #[test]
    fn bitmask_round_trips_past_a_byte_boundary() {
        let bits = vec![
            true, false, true, true, false, false, false, true, // byte 0
            true, false, true, // byte 1, partial
        ];
        let mut writer = Writer::new();
        writer.bitmask(&bits);
        let bytes = writer.into_inner();
        assert_eq!(bytes.len(), 2, "11 bits should pack into 2 bytes");

        let mut out = Vec::new();
        Reader::new(&bytes)
            .bitmask_into(bits.len(), &mut out)
            .unwrap();
        assert_eq!(out, bits);
    }

    #[test]
    fn empty_bitmask_consumes_nothing() {
        let mut writer = Writer::new();
        writer.bitmask(&[]);
        assert_eq!(writer.len(), 0);
        let mut out = vec![true];
        Reader::new(&[]).bitmask_into(0, &mut out).unwrap();
        assert!(out.is_empty());
    }

    fn sample_header() -> FrameHeader {
        FrameHeader {
            kind: FrameKind::ServerSnapshot,
            tick: 12_345,
            ack_tick: 12_340,
            ack_bits: 0b1011,
            ack_token: 0xdead_beef,
            margin_ticks: -3,
            flags: FrameHeader::FLAG_WANT_FULL,
            entity_count: 17,
        }
    }

    #[test]
    fn frame_header_round_trips() {
        let header = sample_header();
        let mut writer = Writer::new();
        header.encode(&mut writer);
        let bytes = writer.into_inner();
        let decoded = FrameHeader::decode(&mut Reader::new(&bytes)).unwrap();
        assert_eq!(decoded, header);
    }

    /// On a client input frame the ack can lead the frame's own tick, so the delta is signed.
    #[test]
    fn frame_header_handles_an_ack_ahead_of_the_tick() {
        let header = FrameHeader {
            kind: FrameKind::ClientInput,
            tick: 100,
            ack_tick: 140,
            ack_bits: 0,
            ack_token: 7,
            margin_ticks: 2,
            flags: 0,
            entity_count: 1,
        };
        let mut writer = Writer::new();
        header.encode(&mut writer);
        let bytes = writer.into_inner();
        assert_eq!(
            FrameHeader::decode(&mut Reader::new(&bytes)).unwrap(),
            header
        );
    }

    /// A hostile peer can put any zigzag value in the ack-delta field. `i64::MIN` is the nasty one:
    /// `tick - i64::MIN` overflows, which panics in a debug build — a remote process kill.
    #[test]
    fn frame_header_survives_an_extreme_ack_delta() {
        // (zigzag payload, decoded delta, expected saturated ack_tick)
        let cases = [
            (u64::MAX, "i64::MIN", u32::MAX), // tick - i64::MIN saturates high
            (u64::MAX - 1, "i64::MAX", 0),    // tick - i64::MAX saturates low
            (0, "0", 10u32),                  // the ordinary case still works
        ];
        for (encoded_delta, label, expected_ack) in cases {
            let mut writer = Writer::new();
            writer.u8(FrameKind::ServerSnapshot.tag());
            writer.varint(10); // tick
            writer.varint(encoded_delta); // ack delta, zigzag
            writer.u32(0); // ack bits
            writer.u32(0); // ack token
            writer.i8(0); // margin
            writer.u8(0); // flags
            writer.varint(1); // entity count
            let bytes = writer.into_inner();

            let decoded = FrameHeader::decode(&mut Reader::new(&bytes))
                .unwrap_or_else(|e| panic!("delta {label} should decode, got {e}"));
            assert_eq!(decoded.tick, 10);
            assert_eq!(
                decoded.ack_tick, expected_ack,
                "ack delta {label} did not saturate as expected"
            );
        }
    }

    #[test]
    fn frame_header_rejects_an_unknown_kind() {
        let bytes = [0x7f, 0x01, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x00];
        assert_eq!(
            FrameHeader::decode(&mut Reader::new(&bytes)),
            Err(CodecError::UnknownFrameKind(0x7f))
        );
    }

    /// The 16 bytes a handshake carries. The session key itself under no secret, a nonce under one.
    const TEST_NONCE: [u8; KEY_LEN] = [
        0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
        0x01,
    ];

    /// A secret as a game would distribute it, already folded to the 16 bytes the derivation keys on.
    fn test_secret() -> [u8; KEY_LEN] {
        crate::auth::compress_secret(b"a secret the lobby handed both ends")
    }

    /// The handshake a joiner holding `secret` sends over `nonce`: the nonce in the clear, and the
    /// confirmation over the key it derives. The two lines every caller of this scheme writes.
    fn hello_under(secret: &[u8; KEY_LEN], nonce: [u8; KEY_LEN]) -> Handshake {
        let hello = Handshake::local(60).with_nonce(nonce);
        let key = derive_session_key(secret, &nonce);
        hello.with_confirm(confirm_tag(&key, &nonce, hello.protocol_version))
    }

    #[test]
    fn handshake_round_trips() {
        let hello = Handshake::local(60).with_nonce(TEST_NONCE);
        let decoded = Handshake::decode(&hello.encode()).unwrap();
        assert_eq!(decoded, hello);
        assert_eq!(decoded.protocol_version, PROTOCOL_VERSION);
        assert_eq!(decoded.session_nonce, TEST_NONCE);
        assert_eq!(decoded.confirm, 0, "a peer with no secret confirms nothing");
    }

    #[test]
    fn handshake_carries_a_session_identity_verbatim() {
        let hello = Handshake::local(60)
            .with_session(0xdead_beef_c0de_1234)
            .with_nonce(TEST_NONCE);
        let decoded = Handshake::decode(&hello.encode()).unwrap();
        assert_eq!(decoded.session_id, 0xdead_beef_c0de_1234);
        assert_eq!(decoded, hello);
    }

    /// The resume token rides the handshake verbatim, all 64 bits of it. It is compared for equality
    /// against the token on the server's record and interpreted no further, so any transformation on the
    /// way through — a truncation to 32 bits, a sign extension — turns every honest resume into a refusal.
    #[test]
    fn handshake_carries_a_resume_token_verbatim() {
        let hello = Handshake::local(60)
            .with_session(0xdead_beef_c0de_1234)
            .with_nonce(TEST_NONCE)
            .with_resume_token(0xfeed_face_dead_c0de);
        let decoded = Handshake::decode(&hello.encode()).unwrap();
        assert_eq!(decoded.resume_token, 0xfeed_face_dead_c0de);
        assert_eq!(decoded, hello);
        assert_eq!(
            Handshake::decode(&Handshake::local(60).with_nonce(TEST_NONCE).encode())
                .unwrap()
                .resume_token,
            0,
            "a peer that quotes no token decodes to the absent value"
        );
    }

    /// The confirmation rides the handshake verbatim too, and it is the last field on the frame. All 64
    /// bits: it is compared against a locally recomputed tag, so any transformation on the way through
    /// refuses every honest joiner that holds the right secret.
    #[test]
    fn handshake_carries_a_confirm_tag_verbatim() {
        let secret = test_secret();
        let hello = hello_under(&secret, TEST_NONCE).with_session(9);
        let decoded = Handshake::decode(&hello.encode()).unwrap();
        assert_eq!(decoded, hello);
        assert_eq!(
            decoded.confirm,
            confirm_tag(
                &derive_session_key(&secret, &TEST_NONCE),
                &TEST_NONCE,
                PROTOCOL_VERSION
            )
        );
        assert_ne!(decoded.confirm, 0, "a held secret produces a real tag");
    }

    /// The 16-byte field kept its offset and its width when it became a nonce, so the frame grew by
    /// exactly the trailing confirmation and nothing moved.
    #[test]
    fn the_handshake_layout_is_the_declared_widths_in_the_declared_order() {
        let bytes = hello_under(&test_secret(), TEST_NONCE)
            .with_session(7)
            .with_resume_token(11)
            .encode();
        assert_eq!(bytes.len(), MAGIC.len() + 4 + 2 + 8 + KEY_LEN + 8 + 8);
        let nonce_at = MAGIC.len() + 4 + 2 + 8;
        assert_eq!(&bytes[nonce_at..nonce_at + KEY_LEN], &TEST_NONCE[..]);
        assert_eq!(
            &bytes[nonce_at + KEY_LEN..nonce_at + KEY_LEN + 8],
            &11u64.to_le_bytes()[..],
            "the resume token still sits directly after the 16 bytes"
        );
    }

    /// The token is a TRAILING field, so a frame that stops before it decodes to `0` rather than erroring.
    /// A decode error here would be answered by `handle_hello` returning, and the peer would be dropped in
    /// silence instead of being seated as the newcomer a tokenless hello describes.
    #[test]
    fn a_handshake_truncated_before_its_resume_token_decodes_to_no_token() {
        let full = Handshake::local(60)
            .with_session(7)
            .with_nonce(TEST_NONCE)
            .with_resume_token(0x0123_4567_89ab_cdef);
        let bytes = full.encode();
        for keep in (bytes.len() - 16)..(bytes.len() - 8) {
            let decoded = Handshake::decode(&bytes[..keep]).unwrap();
            assert_eq!(decoded.resume_token, 0, "keep {keep}");
            assert_eq!(
                decoded.session_nonce, TEST_NONCE,
                "and every field before it survives, keep {keep}"
            );
        }
        assert_eq!(
            Handshake::decode(&bytes).unwrap().resume_token,
            0x0123_4567_89ab_cdef,
            "the untruncated frame still carries it"
        );
    }

    /// The confirmation is the newest trailing field and decodes to `0` — "this peer configured no
    /// secret" — when it is absent. `0` is refused nothing by a peer that holds no secret either, which is
    /// what keeps a session with no secret configured on exactly the path it was on before.
    #[test]
    fn a_handshake_truncated_before_its_confirm_tag_decodes_to_no_confirmation() {
        let full = hello_under(&test_secret(), TEST_NONCE)
            .with_session(7)
            .with_resume_token(0x0123_4567_89ab_cdef);
        let bytes = full.encode();
        for keep in (bytes.len() - 8)..bytes.len() {
            let decoded = Handshake::decode(&bytes[..keep]).unwrap();
            assert_eq!(decoded.confirm, 0, "keep {keep}");
            assert_eq!(
                decoded.resume_token, 0x0123_4567_89ab_cdef,
                "and the field before it survives, keep {keep}"
            );
            assert!(
                Handshake::local(60)
                    .check_compatibility(&decoded, None)
                    .is_ok(),
                "a peer holding no secret does not look at it, keep {keep}"
            );
        }
        assert_eq!(
            Handshake::decode(&bytes).unwrap().confirm,
            full.confirm,
            "the untruncated frame still carries it"
        );
    }

    /// A short handshake must reach `check_compatibility` rather than fail to decode: `handle_hello`
    /// answers a decode error by returning, so the joiner would see no rejection message at all. This is
    /// the shape an older build's handshake arrives in.
    #[test]
    fn a_truncated_handshake_decodes_far_enough_to_be_rejected_readably() {
        let full = Handshake::local(60)
            .with_session(7)
            .with_nonce(TEST_NONCE)
            .with_resume_token(0x0123_4567_89ab_cdef);
        let bytes = full.encode();
        // The last byte of the session nonce. Everything short of this leaves an all-zero nonce behind,
        // which is what `check_compatibility` refuses by name; past it only the trailing resume token and
        // confirmation are lost, and those cost a resume and a secret check rather than the connection.
        let nonce_end = MAGIC.len() + 4 + 2 + 8 + KEY_LEN;
        for keep in 8..nonce_end {
            let decoded = Handshake::decode(&bytes[..keep]).unwrap();
            assert_eq!(decoded.protocol_version, PROTOCOL_VERSION, "keep {keep}");
            let err = Handshake::local(60)
                .check_compatibility(&decoded, None)
                .unwrap_err();
            assert_eq!(err, CodecError::MissingSessionNonce, "keep {keep}");
            assert!(err.to_string().contains("session nonce"), "{err}");
        }
        for keep in nonce_end..bytes.len() {
            let decoded = Handshake::decode(&bytes[..keep]).unwrap();
            assert!(
                Handshake::local(60)
                    .check_compatibility(&decoded, None)
                    .is_ok(),
                "a hello short only of its trailing fields is compatible, keep {keep}"
            );
            let expected_token = if keep >= nonce_end + 8 {
                0x0123_4567_89ab_cdef
            } else {
                0
            };
            assert_eq!(
                decoded.resume_token, expected_token,
                "each trailing field survives exactly its own bytes, keep {keep}"
            );
            assert_eq!(
                decoded.confirm, 0,
                "and none of these reach it, keep {keep}"
            );
        }
    }

    /// The identity is not part of the agreement check: two peers with different sessions are compatible,
    /// which is what lets every client mint its own.
    #[test]
    fn a_differing_session_identity_is_not_an_incompatibility() {
        let ours = Handshake::local(60).with_session(1).with_nonce(TEST_NONCE);
        let theirs = Handshake::local(60).with_session(2).with_nonce(TEST_NONCE);
        assert!(ours.check_compatibility(&theirs, None).is_ok());
    }

    #[test]
    fn handshake_rejects_bad_magic() {
        let mut bytes = Handshake::local(60).with_nonce(TEST_NONCE).encode();
        bytes[0] = b'X';
        assert_eq!(Handshake::decode(&bytes), Err(CodecError::BadMagic));
    }

    #[test]
    fn handshake_accepts_a_matching_peer() {
        let ours = Handshake::local(60);
        let theirs = Handshake::local(60).with_nonce(TEST_NONCE);
        assert!(ours.check_compatibility(&theirs, None).is_ok());
    }

    #[test]
    fn handshake_tolerates_a_differing_patch_version() {
        let ours = Handshake::local(60);
        let theirs = Handshake {
            protocol_version: PROTOCOL_VERSION + 1, // patch bump
            ..Handshake::local(60).with_nonce(TEST_NONCE)
        };
        assert!(ours.check_compatibility(&theirs, None).is_ok());
    }

    /// Version skew is reported BEFORE the missing nonce, so a peer one major behind — which by
    /// definition sends no nonce this build can read — is told the thing it can act on.
    #[test]
    fn handshake_rejects_a_major_version_gap() {
        let ours = Handshake::local(60);
        let theirs = Handshake {
            protocol_version: crate::PROTOCOL_VERSION + 0x0001_0000,
            ..Handshake::local(60)
        };
        let err = ours.check_compatibility(&theirs, None).unwrap_err();
        assert!(matches!(err, CodecError::ProtocolMismatch { .. }));
        // The operator-facing message must name both versions.
        let text = err.to_string();
        let ours_text = version_string(crate::PROTOCOL_VERSION);
        let theirs_text = version_string(crate::PROTOCOL_VERSION + 0x0001_0000);
        assert!(text.contains(&theirs_text), "{text}");
        assert!(text.contains(&ours_text), "{text}");
    }

    #[test]
    fn differing_tickrate_is_not_a_handshake_error() {
        let ours = Handshake::local(60).with_nonce(TEST_NONCE);
        let theirs = Handshake::local(30).with_nonce(TEST_NONCE);
        assert!(ours.check_compatibility(&theirs, None).is_ok());
    }

    // --- the session secret, and the one misconfiguration that can be reported ------------------

    /// The happy path: both ends folded the same secret, so the joiner's tag recomputes.
    #[test]
    fn a_peer_holding_a_secret_accepts_a_joiner_that_confirms_the_same_one() {
        let secret = test_secret();
        let hello = hello_under(&secret, TEST_NONCE);
        assert!(Handshake::local(60)
            .check_compatibility(&hello, Some(&secret))
            .is_ok());
        // And across the wire, which is the only form the accepting side ever sees it in.
        let decoded = Handshake::decode(&hello.encode()).unwrap();
        assert!(Handshake::local(60)
            .check_compatibility(&decoded, Some(&secret))
            .is_ok());
    }

    /// THE ONE SIGNALABLE MISCONFIGURATION. A server holding a secret against a client holding none
    /// would otherwise seat a session whose every datagram fails its tag, silently — the client would see
    /// a join that goes through and then nothing. The confirmation turns it into one readable rejection.
    #[test]
    fn a_peer_holding_a_secret_refuses_a_joiner_that_cannot_confirm_it() {
        let secret = test_secret();
        let cases = [
            (
                "a joiner that configured no secret at all",
                Handshake::local(60).with_nonce(TEST_NONCE),
            ),
            (
                "a joiner holding a different secret",
                hello_under(
                    &crate::auth::compress_secret(b"some other secret"),
                    TEST_NONCE,
                ),
            ),
            (
                "a tag lifted from a different nonce",
                Handshake::local(60)
                    .with_nonce(TEST_NONCE)
                    .with_confirm(hello_under(&secret, [0x5au8; KEY_LEN]).confirm),
            ),
            (
                "a guessed tag",
                Handshake::local(60)
                    .with_nonce(TEST_NONCE)
                    .with_confirm(0xffff_ffff_ffff_ffff),
            ),
        ];
        for (label, hello) in cases {
            let err = Handshake::local(60)
                .check_compatibility(&hello, Some(&secret))
                .unwrap_err();
            assert_eq!(err, CodecError::SecretMismatch, "{label}");
            assert!(err.to_string().contains("secret"), "{label}: {err}");
        }
    }

    /// The negative control for the rule above, and the compatibility promise for every session that
    /// configures nothing: a peer holding no secret does not look at the confirmation, whatever is in it.
    #[test]
    fn a_peer_holding_no_secret_ignores_the_confirm_tag_entirely() {
        for confirm in [0u64, 1, 0xdead_beef_c0de_1234, u64::MAX] {
            let hello = Handshake::local(60)
                .with_nonce(TEST_NONCE)
                .with_confirm(confirm);
            assert!(
                Handshake::local(60)
                    .check_compatibility(&hello, None)
                    .is_ok(),
                "confirm {confirm:#x}"
            );
        }
    }

    /// The version is inside the tag, and the tag is recomputed against the version the REMOTE stamped
    /// on its own frame. Major must already match to get this far, and minor and patch are legitimately
    /// allowed to differ — so checking against our own version would refuse every honest peer one patch
    /// away, which is the failure mode this pins.
    #[test]
    fn a_confirmation_is_checked_against_the_version_its_sender_stamped() {
        let secret = test_secret();
        let mut hello = Handshake::local(60).with_nonce(TEST_NONCE);
        hello.protocol_version = PROTOCOL_VERSION + 1; // patch bump
        let key = derive_session_key(&secret, &TEST_NONCE);
        let hello = hello.with_confirm(confirm_tag(&key, &TEST_NONCE, hello.protocol_version));
        assert!(Handshake::local(60)
            .check_compatibility(&hello, Some(&secret))
            .is_ok());
        // And a peer whose version field was altered in flight no longer confirms, because the tag was
        // taken over the version it actually sent.
        let mut tampered = hello;
        tampered.protocol_version = PROTOCOL_VERSION;
        assert_eq!(
            Handshake::local(60)
                .check_compatibility(&tampered, Some(&secret))
                .unwrap_err(),
            CodecError::SecretMismatch
        );
    }

    /// The all-zero refusal survived the field becoming a nonce, and it is checked BEFORE the
    /// confirmation — a joiner that sent no 16 bytes is told that, not told its secret is wrong.
    #[test]
    fn an_all_zero_nonce_is_refused_under_a_secret_as_well() {
        let secret = test_secret();
        let hello = hello_under(&secret, [0u8; KEY_LEN]);
        assert_eq!(
            Handshake::local(60)
                .check_compatibility(&hello, Some(&secret))
                .unwrap_err(),
            CodecError::MissingSessionNonce,
            "a correctly tagged all-zero nonce is still an all-zero nonce"
        );
    }

    #[test]
    fn version_strings_render() {
        assert_eq!(version_string(0x0001_0000), "1.0.0");
        assert_eq!(version_string(0x0003_0201), "3.2.1");
    }

    use crate::protocol::{PropKind, PropRole, SchemaBuilder};

    fn block_schema() -> SchemaBuilder {
        let mut builder = SchemaBuilder::new();
        builder.push("net_pos", PropKind::Vec3, PropRole::State);
        builder.push("net_boots_on", PropKind::Bool, PropRole::State);
        builder.push("net_gait", PropKind::F64, PropRole::State);
        builder
    }

    #[test]
    fn full_state_block_round_trips() {
        let schema = block_schema();
        let row: Vec<u8> = (0..schema.row_stride() as u8).collect();
        let mut writer = Writer::new();
        let mut scratch = Vec::new();
        encode_state_block(
            &mut writer,
            &mut scratch,
            schema.props(),
            42,
            100,
            98,
            None,
            &row,
            false,
        );
        let bytes = writer.into_inner();

        let mut reader = Reader::new(&bytes);
        let meta = decode_state_block_meta(&mut reader, 100).unwrap();
        assert_eq!(meta.slot, 42);
        assert_eq!(meta.tick, 98);
        assert!(meta.full);
        assert!(!meta.state_lane);
        assert_eq!(meta.reference_tick, None);

        let mut out = vec![0u8; schema.row_stride()];
        let applied = decode_state_block_into(
            &mut reader,
            &meta,
            schema.props(),
            &mut scratch,
            None,
            &mut out,
        )
        .unwrap();
        assert!(applied);
        assert_eq!(out, row);
        assert!(reader.is_exhausted());
    }

    #[test]
    fn masked_state_block_round_trips_against_its_reference() {
        let schema = block_schema();
        let base: Vec<u8> = vec![1; schema.row_stride()];
        let mut next = base.clone();
        next[0] = 9; // net_pos changed
        next[13] = 9; // net_gait changed

        let mut writer = Writer::new();
        let mut scratch = Vec::new();
        encode_state_block(
            &mut writer,
            &mut scratch,
            schema.props(),
            7,
            50,
            50,
            Some((40, &base)),
            &next,
            true,
        );
        let bytes = writer.into_inner();

        let mut reader = Reader::new(&bytes);
        let meta = decode_state_block_meta(&mut reader, 50).unwrap();
        assert!(!meta.full);
        assert!(meta.state_lane);
        assert_eq!(meta.reference_tick, Some(40));

        let mut out = vec![0u8; schema.row_stride()];
        let applied = decode_state_block_into(
            &mut reader,
            &meta,
            schema.props(),
            &mut scratch,
            Some(&base),
            &mut out,
        )
        .unwrap();
        assert!(applied);
        assert_eq!(out, next);
        assert!(reader.is_exhausted());
    }

    #[test]
    fn masked_block_without_a_local_base_is_skipped_cleanly() {
        let schema = block_schema();
        let base = vec![1u8; schema.row_stride()];
        let mut next = base.clone();
        next[0] = 5;

        let mut writer = Writer::new();
        let mut scratch = Vec::new();
        encode_state_block(
            &mut writer,
            &mut scratch,
            schema.props(),
            7,
            50,
            50,
            Some((40, &base)),
            &next,
            false,
        );
        // A second block after it must still decode.
        encode_state_block(
            &mut writer,
            &mut scratch,
            schema.props(),
            8,
            50,
            49,
            None,
            &base,
            false,
        );
        let bytes = writer.into_inner();

        let mut reader = Reader::new(&bytes);
        let meta = decode_state_block_meta(&mut reader, 50).unwrap();
        let mut out = vec![0u8; schema.row_stride()];
        let applied = decode_state_block_into(
            &mut reader,
            &meta,
            schema.props(),
            &mut scratch,
            None,
            &mut out,
        )
        .unwrap();
        assert!(!applied, "no base row should mean no application");

        let second = decode_state_block_meta(&mut reader, 50).unwrap();
        assert_eq!(second.slot, 8);
        assert_eq!(second.tick, 49);
    }

    #[test]
    fn unknown_entity_blocks_can_be_skipped_wholesale() {
        let schema = block_schema();
        let row = vec![3u8; schema.row_stride()];
        let mut writer = Writer::new();
        let mut scratch = Vec::new();
        encode_state_block(
            &mut writer,
            &mut scratch,
            schema.props(),
            999,
            10,
            10,
            None,
            &row,
            false,
        );
        encode_state_block(
            &mut writer,
            &mut scratch,
            schema.props(),
            5,
            10,
            10,
            None,
            &row,
            false,
        );
        let bytes = writer.into_inner();

        let mut reader = Reader::new(&bytes);
        let meta = decode_state_block_meta(&mut reader, 10).unwrap();
        assert_eq!(meta.slot, 999);
        skip_state_block_body(&mut reader, &meta).unwrap();
        let second = decode_state_block_meta(&mut reader, 10).unwrap();
        assert_eq!(second.slot, 5);
    }

    #[test]
    fn state_block_meta_rejects_a_lying_length() {
        let mut writer = Writer::new();
        writer.varint(1); // id
        writer.varint(0); // tick delta
        writer.varint(1000); // body_len far past the buffer
        writer.u8(STATE_BLOCK_FULL);
        let bytes = writer.into_inner();
        assert_eq!(
            decode_state_block_meta(&mut Reader::new(&bytes), 10),
            Err(CodecError::UnexpectedEof)
        );
    }

    #[test]
    fn state_block_meta_rejects_a_body_shorter_than_its_own_meta() {
        // A masked block consumes >= 2 meta bytes (flags + ref delta) against the declared body
        // length; a hostile body_len of 1 must reject as malformed, not underflow the remainder.
        let mut writer = Writer::new();
        writer.varint(1); // id
        writer.varint(0); // tick delta
        writer.varint(1); // body_len: covers the flags byte only
        writer.u8(0); // masked (FULL bit clear)
        writer.varint(3); // ref delta — already past the declared body
        writer.bytes(&[0xAA, 0xBB]); // trailing bytes keep body_len <= remaining
        let bytes = writer.into_inner();
        assert_eq!(
            decode_state_block_meta(&mut Reader::new(&bytes), 10),
            Err(CodecError::UnexpectedEof)
        );
    }

    #[test]
    fn input_block_round_trips_with_redundancy() {
        // Bool + I32: 5 native bytes, lossless, so wire rows equal native rows.
        let mut schema = SchemaBuilder::new();
        schema.push("jump", PropKind::Bool, PropRole::Input);
        schema.push("wheel", PropKind::I32, PropRole::Input);
        let stride = 5usize;
        assert_eq!(schema.row_stride(), stride);
        let newest = vec![9u8; stride];
        let older = vec![8u8; stride];
        let oldest = vec![7u8; stride];
        let rows: Vec<&[u8]> = vec![&newest, &older, &oldest];

        let mut writer = Writer::new();
        encode_input_block(&mut writer, schema.props(), 11, 120, 123, &rows);
        let bytes = writer.into_inner();

        let mut reader = Reader::new(&bytes);
        let meta = decode_input_block_meta(&mut reader, 120).unwrap();
        assert_eq!(meta.slot, 11);
        assert_eq!(
            meta.newest_tick, 123,
            "an input stamp may lead the frame tick"
        );
        assert_eq!(meta.count, 3);
        assert_eq!(meta.body_len, stride * 3);

        assert_eq!(
            input_block_row(&reader, &meta, stride, 0),
            Some(&newest[..])
        );
        assert_eq!(
            input_block_row(&reader, &meta, stride, 2),
            Some(&oldest[..])
        );
        assert_eq!(input_block_row(&reader, &meta, stride, 3), None);
        // A stride disagreement yields no rows rather than sheared ones.
        assert_eq!(input_block_row(&reader, &meta, stride + 1, 0), None);
        skip_input_block_body(&mut reader, &meta).unwrap();
        assert!(reader.is_exhausted());
    }

    #[test]
    fn welcome_ping_pong_round_trip() {
        let welcome = Welcome {
            protocol_version: PROTOCOL_VERSION,
            server_tick: 4242,
            tickrate: 120,
            resume_token: 0xfeed_face_dead_c0de,
        };
        let bytes = welcome.encode();
        let mut reader = Reader::new(&bytes);
        assert_eq!(
            FrameKind::from_tag(reader.u8().unwrap()),
            Ok(FrameKind::Welcome)
        );
        assert_eq!(Welcome::decode(&mut reader).unwrap(), welcome);

        let ping = Ping {
            seq: 7,
            client_us: 123_456_789,
        };
        let bytes = ping.encode();
        let mut reader = Reader::new(&bytes);
        assert_eq!(
            FrameKind::from_tag(reader.u8().unwrap()),
            Ok(FrameKind::Ping)
        );
        assert_eq!(Ping::decode(&mut reader).unwrap(), ping);

        let pong = Pong {
            seq: 7,
            client_us: 123_456_789,
            server_time: 35.375,
        };
        let bytes = pong.encode();
        let mut reader = Reader::new(&bytes);
        assert_eq!(
            FrameKind::from_tag(reader.u8().unwrap()),
            Ok(FrameKind::Pong)
        );
        assert_eq!(Pong::decode(&mut reader).unwrap(), pong);
    }

    /// The welcome's resume token is a TRAILING field and decodes best-effort to `0`.
    ///
    /// A welcome that failed to decode leaves a joining client unsynced with nothing to say why, and `0` is
    /// the value that tells a client to KEEP whatever token it already stored rather than to forget one.
    #[test]
    fn a_welcome_truncated_before_its_resume_token_decodes_to_no_token() {
        let welcome = Welcome {
            protocol_version: PROTOCOL_VERSION,
            server_tick: 4242,
            tickrate: 120,
            resume_token: 0xfeed_face_dead_c0de,
        };
        let bytes = welcome.encode();
        for keep in (bytes.len() - 8)..bytes.len() {
            let mut reader = Reader::new(&bytes[..keep]);
            reader.u8().unwrap();
            let short = Welcome::decode(&mut reader).unwrap();
            assert_eq!(short.resume_token, 0, "keep {keep}");
            assert_eq!(
                short.server_tick, 4242,
                "and the fields before it, keep {keep}"
            );
            assert_eq!(short.tickrate, 120, "keep {keep}");
        }
    }

    /// The demo's own state entity: 20 bytes of wire payload across three properties.
    fn demo_state_schema() -> SchemaBuilder {
        use crate::protocol::QuantKind;
        let mut builder = SchemaBuilder::new();
        builder.push_quantized("position", PropKind::Vec3, PropRole::State, QuantKind::Half);
        builder.push_quantized("net_aux", PropKind::Vec3, PropRole::State, QuantKind::Half);
        builder.push("net_meta", PropKind::I64, PropRole::State);
        builder
    }

    /// The measurement the slot exists for, pinned so a framing change cannot quietly undo it.
    ///
    /// A full block of the demo's state entity is **25 bytes**: 2 slot + 1 frame-tick delta + 1
    /// body length + 1 flags + 20 payload. The 64-bit id it replaced was an FNV-1a hash spread
    /// across the whole range, so its varint cost 9 or 10 bytes — a 32.5-byte block, of which 29%
    /// was the identifier.
    #[test]
    fn a_full_block_of_the_demo_entity_is_twenty_five_bytes() {
        let schema = demo_state_schema();
        assert_eq!(crate::quant::wire_row_stride(schema.props()), 20);
        let row = vec![0u8; schema.row_stride()];
        let mut writer = Writer::new();
        let mut scratch = Vec::new();
        let full = encode_state_block(
            &mut writer,
            &mut scratch,
            schema.props(),
            0,
            100,
            100,
            None,
            &row,
            true,
        );
        assert!(full);
        assert_eq!(writer.len(), 25);
    }

    /// Every slot costs the same two bytes, so the budget arithmetic holds at any session size.
    ///
    /// This is the reason the slot is written fixed-width rather than as a varint: a `u16` varint
    /// is 1 byte below 128 and 3 bytes above it, so past 128 entities most blocks would pay MORE
    /// than the flat 2.
    #[test]
    fn block_size_does_not_move_with_the_slot() {
        let schema = demo_state_schema();
        let row = vec![0u8; schema.row_stride()];
        let mut sizes = Vec::new();
        for slot in [0u16, 1, 127, 128, 1_000, u16::MAX] {
            let mut writer = Writer::new();
            let mut scratch = Vec::new();
            encode_state_block(
                &mut writer,
                &mut scratch,
                schema.props(),
                slot,
                100,
                100,
                None,
                &row,
                true,
            );
            let bytes = writer.into_inner();
            let meta = decode_state_block_meta(&mut Reader::new(&bytes), 100).unwrap();
            assert_eq!(
                meta.slot, slot,
                "slot {slot} did not survive the round trip"
            );
            sizes.push(bytes.len());
        }
        assert!(
            sizes.windows(2).all(|pair| pair[0] == pair[1]),
            "block size moved with the slot: {sizes:?}"
        );
    }

    /// A delta carrying one changed 6-byte property is 13 bytes: 2 slot + 1 frame-tick delta +
    /// 1 body length + 1 flags + 1 reference-tick delta + 1 mask + 6 payload. The id it replaced
    /// made that 20.5 bytes, 46% of it identifier.
    #[test]
    fn a_one_property_delta_of_the_demo_entity_is_thirteen_bytes() {
        let schema = demo_state_schema();
        let base = vec![0u8; schema.row_stride()];
        let mut row = base.clone();
        // Move `net_aux` only — the second property, one 6-byte half-precision Vec3 on the wire.
        let aux = &schema.props()[1];
        row[aux.offset..aux.offset + aux.kind.stride()].fill(0x11);
        let mut writer = Writer::new();
        let mut scratch = Vec::new();
        let full = encode_state_block(
            &mut writer,
            &mut scratch,
            schema.props(),
            9,
            100,
            100,
            Some((99, &base)),
            &row,
            true,
        );
        assert!(!full);
        assert_eq!(writer.len(), 13);
    }

    #[test]
    fn manifest_round_trips() {
        let entries = vec![
            ManifestEntry {
                slot: 0,
                id: 1,
                state_hash: 0xaaaa_bbbb,
                input_hash: 0xcccc_dddd,
                owner: 4,
                seat: 0,
            },
            ManifestEntry {
                slot: u16::MAX,
                id: u64::MAX,
                state_hash: 0,
                input_hash: 1,
                owner: i32::MAX,
                seat: u16::MAX,
            },
            // A state-lane entity: a slot binding and a state hash, no input schema to disagree
            // about, and no seat because nothing drives its input.
            ManifestEntry {
                slot: 7,
                id: 0x0f0f_0f0f_0f0f_0f0f,
                state_hash: 0x1234_5678,
                input_hash: 0,
                owner: 0,
                seat: 0,
            },
        ];
        let bytes = encode_manifest_full(9, &entries);
        let mut reader = Reader::new(&bytes);
        assert_eq!(
            FrameKind::from_tag(reader.u8().unwrap()),
            Ok(FrameKind::EntityManifest)
        );
        // The generation leads the count. A build that read the count first would take it out of
        // the generation's bytes, which is the offset shift the major bump is for.
        assert_eq!(decode_manifest_full(&mut reader).unwrap(), (9, entries));
        assert!(reader.is_exhausted());
    }

    #[test]
    fn manifest_seats_survive_a_second_body_on_the_same_connection() {
        // Local split-screen as the wire carries it: one connection, two labels, and a third body
        // it does not drive at all.
        let entries = vec![
            ManifestEntry {
                slot: 1,
                id: 11,
                state_hash: 1,
                input_hash: 2,
                owner: 3,
                seat: 0,
            },
            ManifestEntry {
                slot: 2,
                id: 12,
                state_hash: 1,
                input_hash: 2,
                owner: 3,
                seat: 1,
            },
            ManifestEntry {
                slot: 3,
                id: 13,
                state_hash: 1,
                input_hash: 2,
                owner: 9,
                seat: 0,
            },
        ];
        let bytes = encode_manifest_full(1, &entries);
        let mut reader = Reader::new(&bytes);
        let _ = reader.u8().unwrap();
        assert_eq!(decode_manifest_full(&mut reader).unwrap(), (1, entries));
    }

    #[test]
    fn a_manifest_owner_that_cannot_be_a_peer_id_decodes_as_unowned() {
        // A varint past i32::MAX names no connection any transport minted, so it reads as "nobody
        // drives this" rather than wrapping into an id that would name somebody.
        let mut writer = Writer::new();
        writer.u8(FrameKind::EntityManifest.tag());
        writer.varint(3); // generation
        writer.varint(1); // count
        writer.u16(5);
        writer.varint(42);
        writer.u32(1);
        writer.u32(2);
        writer.varint(u64::from(u32::MAX));
        writer.u16(3);
        let bytes = writer.into_inner();
        let mut reader = Reader::new(&bytes);
        let _ = reader.u8().unwrap();
        let (generation, entries) = decode_manifest_full(&mut reader).unwrap();
        assert_eq!(generation, 3);
        assert_eq!(entries[0].owner, 0);
    }

    #[test]
    fn manifest_with_a_hostile_count_reports_eof_without_overallocating() {
        let mut writer = Writer::new();
        writer.varint(0); // generation
        writer.varint(u64::MAX); // count
        let bytes = writer.into_inner();
        assert_eq!(
            decode_manifest_full(&mut Reader::new(&bytes)),
            Err(CodecError::UnexpectedEof)
        );
    }

    // ------------------------------------------------------------------
    // Entity-manifest deltas: the algebra, the frame, and the chain.
    // ------------------------------------------------------------------

    /// One manifest row, with everything but the slot and the id derived so a test can state a
    /// table as a list of pairs. `epoch` moves the mutable columns of a fraction of the rows, which
    /// is how a row changes on a slot that did not move.
    fn manifest_row(slot: u16, id: u64, epoch: u64) -> ManifestEntry {
        let drives = id.is_multiple_of(7);
        ManifestEntry {
            slot,
            id,
            state_hash: (id as u32) ^ 0x5555_5555,
            input_hash: if drives { 0x1234 } else { 0 },
            owner: if drives { (epoch % 3 + 1) as i32 } else { 0 },
            seat: if drives { (epoch % 2) as u16 } else { 0 },
        }
    }

    fn manifest_rows(pairs: &[(u16, u64)]) -> Vec<ManifestEntry> {
        pairs
            .iter()
            .map(|&(slot, id)| manifest_row(slot, id, 0))
            .collect()
    }

    #[test]
    fn a_manifest_delta_round_trips_every_shape() {
        let added = manifest_rows(&[(0, 11), (7, 0x0f0f_0f0f_0f0f_0f0f), (u16::MAX, u64::MAX)]);
        for delta in [
            // Removals only: a burst of unregisters.
            ManifestDelta {
                base_generation: 4,
                generation: 5,
                removed: vec![0, 3, u16::MAX],
                added: Vec::new(),
            },
            // Additions only: the ordinary spawn.
            ManifestDelta {
                base_generation: 5,
                generation: 6,
                removed: Vec::new(),
                added: added.clone(),
            },
            // Both halves, and a generation that skips several — a server coalescing dirty ticks.
            ManifestDelta {
                base_generation: 6,
                generation: 19,
                removed: vec![1, 2],
                added: added.clone(),
            },
            // Empty. Never published, but it has to decode: the bytes are chosen by a remote peer.
            ManifestDelta::default(),
        ] {
            let bytes = encode_manifest_delta(&delta);
            let mut reader = Reader::new(&bytes);
            assert_eq!(
                FrameKind::from_tag(reader.u8().unwrap()),
                Ok(FrameKind::EntityManifestDelta)
            );
            assert_eq!(decode_manifest_delta(&mut reader).unwrap(), delta);
            assert!(reader.is_exhausted(), "the delta wrote bytes nothing reads");
        }
    }

    /// Both counts, independently. A count field is the one place a remote peer can ask a decoder
    /// for an allocation, so each is capped at the same 4096 reserve the full table uses.
    #[test]
    fn a_manifest_delta_with_hostile_counts_reports_eof_without_overallocating() {
        // A hostile removed_count, with nothing behind it.
        let mut writer = Writer::new();
        writer.varint(1); // base_generation
        writer.varint(2); // generation
        writer.varint(u64::MAX); // removed_count
        assert_eq!(
            decode_manifest_delta(&mut Reader::new(writer.as_slice())),
            Err(CodecError::UnexpectedEof)
        );

        // A well-formed removed half, then a hostile added_count.
        let mut writer = Writer::new();
        writer.varint(1);
        writer.varint(2);
        writer.varint(1);
        writer.u16(9);
        writer.varint(u64::MAX); // added_count
        assert_eq!(
            decode_manifest_delta(&mut Reader::new(writer.as_slice())),
            Err(CodecError::UnexpectedEof)
        );
    }

    /// Every kind of change, each asserted to produce the MINIMAL record. An over-eager diff is not
    /// a correctness bug and is exactly the bug that makes a delta cost as much as the table.
    #[test]
    fn diffing_a_manifest_produces_the_minimal_record() {
        let base = manifest_rows(&[(0, 100), (1, 200), (2, 300)]);

        // Nothing changed: the case almost every rebuild hits, and the whole saving.
        let (removed, added) = diff_manifest(&base, &base);
        assert!(removed.is_empty() && added.is_empty());

        // A row added on a slot that was not bound.
        let mut current = base.clone();
        current.push(manifest_row(3, 400, 0));
        let (removed, added) = diff_manifest(&base, &current);
        assert!(removed.is_empty());
        assert_eq!(added, vec![manifest_row(3, 400, 0)]);

        // A row removed, with nothing to replace it: two bytes, no row restated.
        let current = manifest_rows(&[(0, 100), (2, 300)]);
        let (removed, added) = diff_manifest(&base, &current);
        assert_eq!(removed, vec![1]);
        assert!(added.is_empty(), "a removal restates no row");

        // A slot rebound to a different entity. ONE added row and NO removal: binding a slot
        // already replaces both directions, so the old id needs no record of its own.
        let current = manifest_rows(&[(0, 100), (1, 999), (2, 300)]);
        let (removed, added) = diff_manifest(&base, &current);
        assert!(removed.is_empty(), "a reissued slot is not a removal");
        assert_eq!(added, vec![manifest_row(1, 999, 0)]);

        // A row whose seat and owner changed on a slot that stayed bound to the same entity.
        // 700 is divisible by 7, so `manifest_row` moves its owner and seat with the epoch.
        let held = manifest_rows(&[(0, 100), (1, 700)]);
        let mut moved = held.clone();
        moved[1] = manifest_row(1, 700, 1);
        assert_ne!(held[1].owner, moved[1].owner);
        let (removed, added) = diff_manifest(&held, &moved);
        assert!(removed.is_empty());
        assert_eq!(added, vec![manifest_row(1, 700, 1)]);

        // An entity that moved from one slot to another: the old slot is retired and the new row
        // stated, because the wire names a removal by slot and nothing else identifies the old one.
        let current = manifest_rows(&[(0, 100), (2, 300), (9, 200)]);
        let (removed, added) = diff_manifest(&base, &current);
        assert_eq!(removed, vec![1]);
        assert_eq!(added, vec![manifest_row(9, 200, 0)]);
    }

    /// The argument order is not an assumption the caller has to satisfy, and both halves come out
    /// ascending by slot — the order the wire and the receiver's table are both in.
    #[test]
    fn diffing_a_manifest_does_not_depend_on_the_order_it_is_handed() {
        let previous = manifest_rows(&[(5, 500), (1, 100), (9, 900)]);
        let current = manifest_rows(&[(9, 900), (2, 200), (1, 111)]);
        let (removed, added) = diff_manifest(&previous, &current);
        assert_eq!(removed, vec![5]);
        assert_eq!(
            added,
            vec![manifest_row(1, 111, 0), manifest_row(2, 200, 0)],
            "both halves ascend by slot"
        );
    }

    /// `apply_manifest_delta` is the inverse of `diff_manifest`, which is the law the receive path
    /// leans on: a receiver holding the table the server diffed against lands on the server's table.
    #[test]
    fn applying_a_delta_undoes_the_diff_that_produced_it() {
        let previous = manifest_rows(&[(0, 100), (1, 200), (2, 300), (5, 500)]);
        let current = manifest_rows(&[(0, 100), (1, 999), (3, 400), (5, 500)]);
        let (removed, added) = diff_manifest(&previous, &current);
        let delta = ManifestDelta {
            base_generation: 1,
            generation: 2,
            removed,
            added,
        };
        assert_eq!(apply_manifest_delta(&previous, &delta), current);
    }

    /// A slot named in both halves cannot come out of `diff_manifest` — but the bytes are chosen by
    /// a remote peer, and applying the removals second would drop the row the same frame installed.
    #[test]
    fn a_delta_naming_one_slot_twice_keeps_the_row_it_added() {
        let held = manifest_rows(&[(1, 100)]);
        let delta = ManifestDelta {
            base_generation: 0,
            generation: 1,
            removed: vec![1],
            added: vec![manifest_row(1, 200, 0)],
        };
        assert_eq!(
            apply_manifest_delta(&held, &delta),
            vec![manifest_row(1, 200, 0)]
        );
    }

    /// The refusal that keeps a receiver from half-applying a delta computed against a table it is
    /// not holding — and the assertion that the refusal is load-bearing rather than cosmetic.
    #[test]
    fn a_delta_against_the_wrong_generation_is_refused() {
        let held = manifest_rows(&[(1, 100), (2, 200)]);
        let delta = ManifestDelta {
            base_generation: 5,
            generation: 6,
            removed: vec![1],
            added: vec![manifest_row(3, 300, 0)],
        };
        assert!(delta.applies_to(5));
        for generation in [0u64, 4, 6, 7, u64::MAX] {
            assert!(
                !delta.applies_to(generation),
                "a receiver at generation {generation} must refuse a delta based on 5"
            );
        }

        // What a refusal has to leave behind: the table exactly as it was. The receiver never calls
        // `apply_manifest_delta`, and this is what it would have cost if it had.
        let mut receiver = held.clone();
        if delta.applies_to(4) {
            receiver = apply_manifest_delta(&receiver, &delta);
        }
        assert_eq!(receiver, held, "a refused delta must move nothing");
        assert_ne!(
            apply_manifest_delta(&held, &delta),
            held,
            "and the delta really would have moved it"
        );
    }

    /// **The load-bearing one.** A few hundred ticks of real slot-table churn — spawns, despawns, a
    /// respawn under the same id, and slot reissues past the reuse quarantine — with a receiver
    /// driven only by deltas, asserted at every step to hold exactly the table a receiver rebuilt
    /// from the full frame holds.
    ///
    /// This is the test that catches a lost removal. A delta gave up the complete table's
    /// self-repair: a receiver that keeps a slot bound past its unregister applies the next
    /// entity's rows to the departed one, silently, with every block decoding cleanly.
    #[test]
    fn a_chain_of_deltas_reaches_the_same_table_as_a_full_rebuild() {
        use crate::slots::{SlotTable, SLOT_QUARANTINE_TICKS};

        let mut server = SlotTable::new();
        let mut registered: std::collections::BTreeSet<u64> = (1..=24u64).collect();

        // The receiver driven by DELTAS, and the receiver driven by FULL tables. Both hold rows and
        // a slot table, because a wrong row and a wrong binding fail differently.
        let mut delta_rows: Vec<ManifestEntry> = Vec::new();
        let mut delta_slots = SlotTable::new();
        let mut full_slots = SlotTable::new();

        let mut published: Vec<ManifestEntry> = Vec::new();
        let mut generation = 0u64;
        let mut held_generation = 0u64;

        let mut saw_respawn = false;
        let mut saw_reissue = false;
        // Every id each slot has ever named, so a reissue is detectable at all.
        let mut last_named: std::collections::BTreeMap<u16, u64> =
            std::collections::BTreeMap::new();
        let mut deltas = 0usize;
        let mut delta_bytes = 0usize;
        let mut full_bytes = 0usize;

        // The run outlasts the reuse quarantine twice over, which is what makes a slot reissue —
        // the case that makes a lost removal silent — reachable at all.
        let ticks = SLOT_QUARANTINE_TICKS * 2 + 88;
        let mut rng = 0x9e37_79b9_7f4a_7c15u64;
        for tick in 0..ticks {
            rng = rng.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let roll = rng >> 33;

            // Churn. The id pool is small and node-path-derived ids are reused, so a body leaving
            // and coming back asks for the SAME id — the respawn case.
            match roll % 5 {
                0 => {
                    registered.remove(&(1 + roll % 24));
                }
                1 => {
                    let id = 1 + roll % 24;
                    if registered.insert(id) && tick > 0 {
                        saw_respawn = true;
                    }
                }
                2 => {
                    registered.insert(100 + tick);
                }
                3 => {
                    registered.remove(&(100 + tick.saturating_sub(50)));
                }
                _ => {}
            }
            // One long-lived body retired early, so its slot is well past the quarantine when the
            // churn above asks for a name again.
            if tick == 5 {
                registered.remove(&7);
            }
            server.reconcile(&registered, tick);

            // The server's table, exactly as the send path builds it: ascending by slot, with the
            // mutable columns of a fraction of the rows moving every 50 ticks.
            let epoch = tick / 50;
            let mut current: Vec<ManifestEntry> = server
                .bindings()
                .map(|(slot, id)| manifest_row(slot, id, epoch))
                .collect();
            current.sort_unstable_by_key(|entry| entry.slot);

            let (removed, added) = diff_manifest(&published, &current);
            if removed.is_empty() && added.is_empty() {
                // Nothing is published, which is the whole point: no frame, no bytes.
                continue;
            }
            // A REISSUE is an added row on a slot that named a DIFFERENT entity earlier in the
            // session — not one that names a different entity in the table published last, because
            // the removal record retired that binding before the quarantine even started.
            for entry in &added {
                if last_named
                    .insert(entry.slot, entry.id)
                    .is_some_and(|before| before != entry.id)
                {
                    saw_reissue = true;
                }
            }
            generation += 1;
            let delta = ManifestDelta {
                base_generation: generation - 1,
                generation,
                removed,
                added,
            };

            // THE DELTA RECEIVER. Through the wire, because a decode bug and an apply bug are
            // different bugs and only the round trip catches the first.
            let bytes = encode_manifest_delta(&delta);
            delta_bytes += bytes.len();
            deltas += 1;
            let mut reader = Reader::new(&bytes);
            assert_eq!(
                FrameKind::from_tag(reader.u8().unwrap()),
                Ok(FrameKind::EntityManifestDelta)
            );
            let decoded = decode_manifest_delta(&mut reader).unwrap();
            assert!(decoded.applies_to(held_generation));
            delta_rows = apply_manifest_delta(&delta_rows, &decoded);
            for &slot in &decoded.removed {
                delta_slots.unbind(slot);
            }
            for entry in &decoded.added {
                delta_slots.bind(entry.slot, entry.id);
            }
            held_generation = decoded.generation;

            // THE FULL RECEIVER, rebuilt from the complete table the same publish would have sent.
            let bytes = encode_manifest_full(generation, &current);
            full_bytes += bytes.len();
            let mut reader = Reader::new(&bytes);
            let _ = reader.u8().unwrap();
            let (full_generation, entries) = decode_manifest_full(&mut reader).unwrap();
            full_slots.clear();
            for entry in &entries {
                full_slots.bind(entry.slot, entry.id);
            }
            let full_rows = entries;

            assert_eq!(full_generation, held_generation);
            assert_eq!(
                delta_rows, full_rows,
                "tick {tick}: the delta chain diverged from the full rebuild"
            );
            assert_eq!(
                delta_rows, current,
                "tick {tick}: the delta chain diverged from the SERVER"
            );
            assert_eq!(
                delta_slots.bindings().collect::<Vec<_>>(),
                server.bindings().collect::<Vec<_>>(),
                "tick {tick}: a slot binding survived its removal record"
            );
            assert_eq!(
                delta_slots.bindings().collect::<Vec<_>>(),
                full_slots.bindings().collect::<Vec<_>>()
            );
            published = current;
        }

        assert!(
            saw_respawn,
            "the churn never respawned a body under its old id"
        );
        assert!(
            saw_reissue,
            "the churn never reissued a slot past its quarantine, so the case that makes a lost \
             removal silent was never exercised"
        );
        assert!(
            deltas > 50,
            "only {deltas} publishes — too little churn to prove anything"
        );
        // Measured at 273 publishes: 6,113 B as deltas against 254,265 B as whole tables, at a
        // table of a few dozen rows. The ratio grows with the table, because a delta is priced by
        // the change and a whole table by the session.
        assert!(
            delta_bytes * 20 < full_bytes,
            "{deltas} publishes cost {delta_bytes} B as deltas against {full_bytes} B as whole \
             tables, which is not worth the removal record a delta costs"
        );
    }

    /// The byte costs the manifest's own doc table quotes, measured from the encoder rather than
    /// estimated. The ratio between a row and a removal is the whole case for a delta.
    #[test]
    fn a_manifest_row_costs_about_twenty_two_and_a_half_bytes() {
        // Entity ids are FNV-1a output, so they are spread over the whole 64-bit range and their
        // LEB128 varints average 9.5 bytes. A deterministic spread stands in for that here.
        let mut state = 0x243f_6a88_85a3_08d3u64;
        let entries: Vec<ManifestEntry> = (0..4096u16)
            .map(|slot| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                ManifestEntry {
                    slot,
                    id: state | 1,
                    state_hash: 0xaaaa_bbbb,
                    input_hash: 0xcccc_dddd,
                    owner: 3,
                    seat: 1,
                }
            })
            .collect();

        // kind (1) + generation varint (1) + count varint (2 at 4096).
        let full = encode_manifest_full(1, &entries);
        let per_row = (full.len() - 4) as f64 / entries.len() as f64;
        assert!(
            (per_row - 22.5).abs() < 0.4,
            "one manifest row measured {per_row} bytes, not ~22.5"
        );

        // What one change costs instead, at any table size.
        let empty = encode_manifest_delta(&ManifestDelta {
            base_generation: 1,
            generation: 2,
            removed: Vec::new(),
            added: Vec::new(),
        });
        assert_eq!(empty.len(), 5, "kind, two generations, two zero counts");
        let retired = encode_manifest_delta(&ManifestDelta {
            base_generation: 1,
            generation: 2,
            removed: vec![entries[0].slot],
            added: Vec::new(),
        });
        assert_eq!(
            retired.len(),
            7,
            "one retired binding is the framing plus 2 B"
        );
        let bound = encode_manifest_delta(&ManifestDelta {
            base_generation: 1,
            generation: 2,
            removed: Vec::new(),
            added: vec![entries[0]],
        });
        assert!(
            bound.len() <= 5 + 23,
            "one new binding measured {} bytes",
            bound.len()
        );

        // AT REST a delta costs nothing at all: a rebuild that reproduces the published table
        // publishes no frame. That is stated here rather than only in prose because it is the
        // saving, and `send_manifest_if_dirty` is the only other place it is enforced.
        let (removed, added) = diff_manifest(&entries, &entries);
        assert!(removed.is_empty() && added.is_empty());
    }

    /// The whole set is the repair path, so it has to survive its own wire trip at the sizes a
    /// repair actually happens at — a joining peer's first set, not a two-slot example.
    #[test]
    fn an_interest_table_round_trips_at_every_size() {
        for count in [0usize, 1, 2, 255, 256, 1000] {
            let slots: Vec<u16> = (0..count as u16).collect();
            let bytes = encode_interest_table(42, &slots);
            let mut reader = Reader::new(&bytes);
            assert_eq!(
                reader.u8().unwrap(),
                FrameKind::InterestTable.tag(),
                "the kind byte leads, as every other frame's does"
            );
            let (generation, decoded) = decode_interest_table(&mut reader).unwrap();
            assert_eq!(generation, 42);
            assert_eq!(decoded, slots, "a set of {count} slots");
        }
    }

    /// The table rides a reliable channel, so a truncated one is a fault rather than an ordinary
    /// event — but it still reports rather than panicking or inventing slots, like every decoder here.
    #[test]
    fn a_truncated_interest_table_reports_eof_rather_than_panicking() {
        let bytes = encode_interest_table(7, &[1, 2, 3, 4]);
        for cut in 1..bytes.len() {
            let mut reader = Reader::new(&bytes[..cut]);
            let _ = reader.u8();
            let outcome = decode_interest_table(&mut reader);
            if cut < bytes.len() {
                assert!(
                    outcome.is_err() || outcome.unwrap().1.len() < 4,
                    "a section cut at {cut} claimed a whole table"
                );
            }
        }
    }

    /// **EXACT, AND BOTH DIRECTIONS MATTER.** A whole set is reliable and a section is not, so a
    /// section built either side of a table can arrive on the wrong side of it. Below the held
    /// generation it is one the table already superseded; above it, one diffed against a set that
    /// has not arrived — and applying that early would let the table then undo it. What the match
    /// must still admit is a re-send, which carries the generation it was built at.
    #[test]
    fn a_section_applies_only_at_the_generation_it_was_built_against() {
        let section = InterestDeltaSection {
            generation: 4,
            left: Vec::new(),
            entered: Vec::new(),
        };
        assert!(
            section.applies_to(4),
            "the baseline it was diffed from, and every re-send of it"
        );
        assert!(
            !section.applies_to(3),
            "a section built against a set this peer has not adopted yet"
        );
        assert!(
            !section.applies_to(5),
            "and one the whole set it now holds has already superseded"
        );
    }

    #[test]
    fn an_interest_delta_round_trips_both_halves() {
        let left = vec![0u16, 7, 4096, u16::MAX];
        let entered = vec![1u16, 2, 3];
        let mut writer = Writer::new();
        encode_interest_delta(9, &left, &entered, &mut writer);
        let bytes = writer.into_inner();
        // varint(9) + varint(4) + 4x2 + varint(3) + 3x2 = 1 + 1 + 8 + 1 + 6.
        assert_eq!(
            bytes.len(),
            17,
            "a generation varint, two count varints, 2 bytes per slot"
        );
        assert_eq!(
            decode_interest_delta(&mut Reader::new(&bytes))
                .unwrap()
                .generation,
            9,
            "and the generation leads the section"
        );

        let section = decode_interest_delta(&mut Reader::new(&bytes)).unwrap();
        assert_eq!(section.left, left);
        assert_eq!(section.entered, entered);
    }

    /// A leave-only tick is the case the empty-frame gate has to let through, and an enter-only tick
    /// is the ordinary one. Both halves are independently empty, and an all-empty section is two
    /// bytes — which is what the server spends once to tell a joining peer it is filtering at all.
    #[test]
    fn each_half_of_an_interest_delta_may_be_empty_on_its_own() {
        for (left, entered) in [
            (vec![9u16], Vec::new()),
            (Vec::new(), vec![9u16]),
            (Vec::new(), Vec::new()),
        ] {
            let mut writer = Writer::new();
            encode_interest_delta(0, &left, &entered, &mut writer);
            let bytes = writer.into_inner();
            let section = decode_interest_delta(&mut Reader::new(&bytes)).unwrap();
            assert_eq!(section.left, left);
            assert_eq!(section.entered, entered);
        }

        let mut writer = Writer::new();
        encode_interest_delta(0, &[], &[], &mut writer);
        assert_eq!(
            writer.len(),
            3,
            "an empty section is one byte for the generation and one per count"
        );
    }

    /// The section rides an unreliable datagram, so a truncated one is an ordinary event rather than
    /// an attack. Every cut reports an error instead of panicking or inventing slots.
    #[test]
    fn a_truncated_interest_delta_reports_eof_rather_than_panicking() {
        let mut writer = Writer::new();
        encode_interest_delta(0, &[1, 2, 3], &[4, 5], &mut writer);
        let full = writer.into_inner();
        for cut in 0..full.len() {
            assert_eq!(
                decode_interest_delta(&mut Reader::new(&full[..cut])),
                Err(CodecError::UnexpectedEof),
                "a section cut at {cut} decoded"
            );
        }
        assert!(decode_interest_delta(&mut Reader::new(&full)).is_ok());
    }

    #[test]
    fn an_interest_delta_with_a_hostile_count_reports_eof_without_overallocating() {
        let mut writer = Writer::new();
        writer.varint(u64::MAX); // left_count, with no slots behind it
        let bytes = writer.into_inner();
        assert_eq!(
            decode_interest_delta(&mut Reader::new(&bytes)),
            Err(CodecError::UnexpectedEof)
        );

        // And the same claim in the SECOND half, past a well-formed first one.
        let mut writer = Writer::new();
        writer.varint(1);
        writer.u16(3);
        writer.varint(u64::MAX);
        let bytes = writer.into_inner();
        assert_eq!(
            decode_interest_delta(&mut Reader::new(&bytes)),
            Err(CodecError::UnexpectedEof)
        );
    }

    /// Every header flag owns a bit of its own, whichever direction it travels in. Bit 0 is the
    /// client's `WANT_FULL` NACK, bit 1 is the server's interest-delta announcement and bit 2 is the
    /// client's `WANT_MANIFEST` NACK, so no two can collide on one frame.
    #[test]
    fn every_header_flag_is_its_own_bit() {
        assert_eq!(FrameHeader::FLAG_WANT_FULL, 1);
        assert_eq!(FrameHeader::FLAG_INTEREST_DELTA, 2);
        assert_eq!(FrameHeader::FLAG_WANT_MANIFEST, 4);
        assert_eq!(FrameHeader::FLAG_WANT_INTEREST, 8);
        let bits = [
            FrameHeader::FLAG_WANT_FULL,
            FrameHeader::FLAG_INTEREST_DELTA,
            FrameHeader::FLAG_WANT_MANIFEST,
            FrameHeader::FLAG_WANT_INTEREST,
        ];
        for (index, &flag) in bits.iter().enumerate() {
            assert_eq!(
                flag.count_ones(),
                1,
                "flag {index} claims more than one bit"
            );
            for &other in &bits[index + 1..] {
                assert_eq!(flag & other, 0, "two flags share a bit");
            }
        }

        // They survive the header round trip, which is where a receiver reads them from — including
        // both client-to-server NACKs raised on one frame, which is the case a client that lost its
        // delta base and its manifest in the same tick sends.
        for flags in [
            FrameHeader::FLAG_INTEREST_DELTA,
            FrameHeader::FLAG_WANT_MANIFEST,
            FrameHeader::FLAG_WANT_FULL | FrameHeader::FLAG_WANT_MANIFEST,
        ] {
            let mut header = sample_header();
            header.flags = flags;
            let mut writer = Writer::new();
            header.encode(&mut writer);
            let bytes = writer.into_inner();
            let decoded = FrameHeader::decode(&mut Reader::new(&bytes)).unwrap();
            assert_eq!(decoded.flags, flags);
        }
    }

    #[test]
    fn peek_bytes_does_not_consume() {
        let bytes = [1u8, 2, 3, 4];
        let mut reader = Reader::new(&bytes);
        reader.u8().unwrap();
        assert_eq!(reader.peek_bytes(0, 2), Some(&[2u8, 3][..]));
        assert_eq!(reader.peek_bytes(2, 1), Some(&[4u8][..]));
        assert_eq!(reader.peek_bytes(2, 2), None);
        assert_eq!(reader.peek_bytes(usize::MAX, 1), None);
        assert_eq!(reader.remaining(), 3, "peeks must not consume");
    }

    /// A malformed packet from a remote peer must never panic the process.
    #[test]
    fn decoders_never_panic_on_hostile_input() {
        // A deterministic pseudo-random sweep — no external dependency, reproducible.
        let mut state = 0x1234_5678u32;
        let mut buf = Vec::new();
        for len in 0..64usize {
            buf.clear();
            for _ in 0..len {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                buf.push((state >> 24) as u8);
            }
            // Each of these returns Result; the assertion is simply that we get here.
            let _ = FrameHeader::decode(&mut Reader::new(&buf));
            if let Ok(hello) = Handshake::decode(&buf) {
                // The compatibility check reads three remote-chosen fields — version, nonce and
                // confirmation — and derives a key from the last two, so it is swept under both regimes.
                let _ = Handshake::local(60).check_compatibility(&hello, None);
                let _ = Handshake::local(60).check_compatibility(&hello, Some(&test_secret()));
            }
            let _ = Welcome::decode(&mut Reader::new(&buf));
            let _ = Ping::decode(&mut Reader::new(&buf));
            let _ = Pong::decode(&mut Reader::new(&buf));
            let _ = decode_manifest_full(&mut Reader::new(&buf));
            let _ = decode_manifest_delta(&mut Reader::new(&buf));
            let _ = decode_interest_delta(&mut Reader::new(&buf));
            let schema = block_schema();
            let mut scratch = Vec::new();
            let mut row = vec![0u8; schema.row_stride()];
            let mut reader = Reader::new(&buf);
            if let Ok(meta) = decode_state_block_meta(&mut reader, 100) {
                let _ = decode_state_block_into(
                    &mut reader,
                    &meta,
                    schema.props(),
                    &mut scratch,
                    None,
                    &mut row,
                );
            }
            let mut reader = Reader::new(&buf);
            if let Ok(meta) = decode_input_block_meta(&mut reader, 100) {
                let _ = input_block_row(&reader, &meta, 8, 0);
                let _ = skip_input_block_body(&mut reader, &meta);
            }
            let mut reader = Reader::new(&buf);
            let _ = reader.varint();
            let _ = reader.zigzag();
            let mut out = Vec::new();
            let _ = reader.bitmask_into(len * 8, &mut out);
        }
    }

    #[test]
    fn truncating_a_valid_frame_at_every_offset_is_handled() {
        let mut writer = Writer::new();
        sample_header().encode(&mut writer);
        let full = writer.into_inner();
        for cut in 0..full.len() {
            let _ = FrameHeader::decode(&mut Reader::new(&full[..cut]));
        }
        // The untruncated frame still decodes.
        assert!(FrameHeader::decode(&mut Reader::new(&full)).is_ok());
    }
}
