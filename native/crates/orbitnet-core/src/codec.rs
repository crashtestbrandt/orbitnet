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
//! replaced it and what distributing a slot table costs; [`encode_manifest`] is the channel that
//! distributes it.
//!
//! The decoder is the one piece of this crate that reads bytes chosen by a remote peer, so it is
//! written to be total: every read is bounds-checked and returns [`CodecError`] rather than
//! panicking or indexing out of range. A netcode decoder that panics on a malformed packet is a
//! remote denial of service, and `forbid(unsafe_code)` at the crate root means a bounds bug cannot
//! become memory unsafety either.

use core::fmt;

use crate::auth::KEY_LEN;
use crate::columnar::changed_mask;
use crate::protocol::{protocol_major, PropSchema, PROTOCOL_VERSION};

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
    /// The peer's handshake carried no session key, so nothing it sends afterwards can be
    /// authenticated. An older build, or a truncated handshake.
    MissingSessionKey,
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
            CodecError::MissingSessionKey => write!(
                f,
                "OrbitNet handshake carried no session key, so no datagram from this peer can be \
                 authenticated. The peer is an older build, or its handshake was truncated."
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
    EntityManifest = 0x07,
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
    /// The key every other datagram of this session is authenticated with.
    ///
    /// Minted by the client, one per session, and **carried in the clear** — so it authenticates a
    /// datagram's membership in a session rather than a peer's identity. All zeroes is refused by
    /// [`Handshake::check_compatibility`]: it is what a peer that sent no key at all decodes to.
    pub session_key: [u8; KEY_LEN],
}

impl Handshake {
    /// Build a handshake for this build at `tickrate`. Carries no session identity and no key; see
    /// [`Handshake::with_session`] and [`Handshake::with_key`].
    #[must_use]
    pub fn local(tickrate: u16) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            tickrate,
            session_id: 0,
            session_key: [0; KEY_LEN],
        }
    }

    /// The same handshake, carrying a session identity.
    #[must_use]
    pub fn with_session(mut self, session_id: u64) -> Self {
        self.session_id = session_id;
        self
    }

    /// The same handshake, carrying the session key.
    #[must_use]
    pub fn with_key(mut self, session_key: [u8; KEY_LEN]) -> Self {
        self.session_key = session_key;
        self
    }

    /// Encode, including the leading magic.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::with_capacity(MAGIC.len() + 14 + KEY_LEN);
        writer.bytes(&MAGIC);
        writer.u32(self.protocol_version);
        writer.u16(self.tickrate);
        writer.u64(self.session_id);
        writer.bytes(&self.session_key);
        writer.into_inner()
    }

    /// Decode, validating the magic.
    ///
    /// **Everything after the protocol version decodes best-effort**, to a zero tick rate, no session
    /// identity and an all-zero key. That is not laxity: `handle_hello` answers a decode error by
    /// returning, so a peer whose handshake is short — an older build, a truncated frame — would be
    /// dropped in silence with no rejection message at all. Decoding it far enough to reach
    /// [`Handshake::check_compatibility`] is what produces the operator-readable version mismatch, and
    /// the same check refuses the all-zero key a short handshake leaves behind.
    pub fn decode(buf: &[u8]) -> Result<Self, CodecError> {
        let mut reader = Reader::new(buf);
        if reader.bytes(MAGIC.len())? != MAGIC {
            return Err(CodecError::BadMagic);
        }
        let protocol_version = reader.u32()?;
        let tickrate = reader.u16().unwrap_or(0);
        let session_id = reader.u64().unwrap_or(0);
        let mut session_key = [0u8; KEY_LEN];
        if let Ok(bytes) = reader.bytes(KEY_LEN) {
            session_key.copy_from_slice(bytes);
        }
        Ok(Self {
            protocol_version,
            tickrate,
            session_id,
            session_key,
        })
    }

    /// Check a remote handshake against ours.
    ///
    /// Protocol major must match exactly and the remote must carry a session key. A differing tick rate
    /// is deliberately *not* an error here — it is a policy decision for the caller, since some games
    /// legitimately let peers run at different rates. Nor is a differing session identity: every client
    /// mints its own.
    ///
    /// **The key is checked on `remote` only.** The local handshake in this call is a version reference
    /// built by [`Handshake::local`], and the server never mints a key of its own — the client's is the
    /// session's.
    pub fn check_compatibility(&self, remote: &Handshake) -> Result<(), CodecError> {
        if protocol_major(remote.protocol_version) != protocol_major(self.protocol_version) {
            return Err(CodecError::ProtocolMismatch {
                theirs: remote.protocol_version,
                ours: self.protocol_version,
            });
        }
        if remote.session_key == [0u8; KEY_LEN] {
            return Err(CodecError::MissingSessionKey);
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
}

impl Welcome {
    /// Encode, with the frame kind tag leading.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::with_capacity(16);
        writer.u8(FrameKind::Welcome.tag());
        writer.u32(self.protocol_version);
        writer.varint(self.server_tick);
        writer.u16(self.tickrate);
        writer.into_inner()
    }

    /// Decode the payload after the kind tag has been consumed.
    pub fn decode(reader: &mut Reader<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            protocol_version: reader.u32()?,
            server_tick: reader.varint()?,
            tickrate: reader.u16()?,
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

/// One entity's row in a [`FrameKind::EntityManifest`] frame: its slot binding and its schema
/// fingerprints.
///
/// **The manifest carries the whole slot table, both lanes, every time.** It covered the rollback
/// lane only while it was purely a schema check, because a state-lane entity has no input schema to
/// disagree about. It is now also the only channel that tells a client what a wire slot names, and
/// state-lane blocks carry slots too, so it has to name every replicated entity.
///
/// It is a **complete snapshot rather than a diff**, which is what makes a receiver's table
/// self-repairing: rebuilding from each manifest drops the binding of every entity that has
/// unregistered since the last one, with no removal record to lose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManifestEntry {
    /// The dense session slot this entity's blocks are named by on the wire.
    pub slot: u16,
    /// Stable entity id (the FNV-1a hash of the synchronizer root's node path).
    pub id: u64,
    /// Hash of the entity's state schema.
    pub state_hash: u32,
    /// Hash of the entity's input schema. `0` for a state-lane entity, which has no input schema.
    pub input_hash: u32,
}

/// Encode an entity manifest frame.
#[must_use]
pub fn encode_manifest(entries: &[ManifestEntry]) -> Vec<u8> {
    let mut writer = Writer::with_capacity(4 + entries.len() * 14);
    writer.u8(FrameKind::EntityManifest.tag());
    writer.varint(entries.len() as u64);
    for entry in entries {
        writer.u16(entry.slot);
        // The full 64-bit id, still a varint. This is the one frame that has to carry it — a
        // receiver derives the same id from the same node path and needs the pairing to find its
        // own entity — and it rides the reliable channel only when the registry changes, so its
        // ~9.5 bytes are spent once per entity per change rather than once per entity per tick.
        writer.varint(entry.id);
        writer.u32(entry.state_hash);
        writer.u32(entry.input_hash);
    }
    writer.into_inner()
}

/// Decode an entity manifest's payload after the kind tag has been consumed.
pub fn decode_manifest(reader: &mut Reader<'_>) -> Result<Vec<ManifestEntry>, CodecError> {
    let count = reader.varint()?;
    // Each entry is at least 11 bytes; a hostile count cannot make us over-allocate.
    let cap = usize::try_from(count.min(4096)).unwrap_or(0);
    let mut entries = Vec::with_capacity(cap);
    for _ in 0..count {
        entries.push(ManifestEntry {
            slot: reader.u16()?,
            id: reader.varint()?,
            state_hash: reader.u32()?,
            input_hash: reader.u32()?,
        });
    }
    Ok(entries)
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

    const TEST_KEY: [u8; KEY_LEN] = [
        0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
        0x01,
    ];

    #[test]
    fn handshake_round_trips() {
        let hello = Handshake::local(60).with_key(TEST_KEY);
        let decoded = Handshake::decode(&hello.encode()).unwrap();
        assert_eq!(decoded, hello);
        assert_eq!(decoded.protocol_version, PROTOCOL_VERSION);
        assert_eq!(decoded.session_key, TEST_KEY);
    }

    #[test]
    fn handshake_carries_a_session_identity_verbatim() {
        let hello = Handshake::local(60)
            .with_session(0xdead_beef_c0de_1234)
            .with_key(TEST_KEY);
        let decoded = Handshake::decode(&hello.encode()).unwrap();
        assert_eq!(decoded.session_id, 0xdead_beef_c0de_1234);
        assert_eq!(decoded, hello);
    }

    /// A short handshake must reach `check_compatibility` rather than fail to decode: `handle_hello`
    /// answers a decode error by returning, so the joiner would see no rejection message at all. This is
    /// the shape an older build's handshake arrives in.
    #[test]
    fn a_truncated_handshake_decodes_far_enough_to_be_rejected_readably() {
        let full = Handshake::local(60).with_session(7).with_key(TEST_KEY);
        let bytes = full.encode();
        for keep in 8..bytes.len() {
            let decoded = Handshake::decode(&bytes[..keep]).unwrap();
            assert_eq!(decoded.protocol_version, PROTOCOL_VERSION, "keep {keep}");
            let err = Handshake::local(60)
                .check_compatibility(&decoded)
                .unwrap_err();
            assert_eq!(err, CodecError::MissingSessionKey, "keep {keep}");
            assert!(err.to_string().contains("session key"), "{err}");
        }
    }

    /// The identity is not part of the agreement check: two peers with different sessions are compatible,
    /// which is what lets every client mint its own.
    #[test]
    fn a_differing_session_identity_is_not_an_incompatibility() {
        let ours = Handshake::local(60).with_session(1).with_key(TEST_KEY);
        let theirs = Handshake::local(60).with_session(2).with_key(TEST_KEY);
        assert!(ours.check_compatibility(&theirs).is_ok());
    }

    #[test]
    fn handshake_rejects_bad_magic() {
        let mut bytes = Handshake::local(60).with_key(TEST_KEY).encode();
        bytes[0] = b'X';
        assert_eq!(Handshake::decode(&bytes), Err(CodecError::BadMagic));
    }

    #[test]
    fn handshake_accepts_a_matching_peer() {
        let ours = Handshake::local(60);
        let theirs = Handshake::local(60).with_key(TEST_KEY);
        assert!(ours.check_compatibility(&theirs).is_ok());
    }

    #[test]
    fn handshake_tolerates_a_differing_patch_version() {
        let ours = Handshake::local(60);
        let theirs = Handshake {
            protocol_version: PROTOCOL_VERSION + 1, // patch bump
            ..Handshake::local(60).with_key(TEST_KEY)
        };
        assert!(ours.check_compatibility(&theirs).is_ok());
    }

    /// Version skew is reported BEFORE the missing key, so a peer one major behind — which by
    /// definition sends no key this build can read — is told the thing it can act on.
    #[test]
    fn handshake_rejects_a_major_version_gap() {
        let ours = Handshake::local(60);
        let theirs = Handshake {
            protocol_version: crate::PROTOCOL_VERSION + 0x0001_0000,
            ..Handshake::local(60)
        };
        let err = ours.check_compatibility(&theirs).unwrap_err();
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
        let ours = Handshake::local(60).with_key(TEST_KEY);
        let theirs = Handshake::local(30).with_key(TEST_KEY);
        assert!(ours.check_compatibility(&theirs).is_ok());
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
            },
            ManifestEntry {
                slot: u16::MAX,
                id: u64::MAX,
                state_hash: 0,
                input_hash: 1,
            },
            // A state-lane entity: a slot binding and a state hash, no input schema to disagree
            // about.
            ManifestEntry {
                slot: 7,
                id: 0x0f0f_0f0f_0f0f_0f0f,
                state_hash: 0x1234_5678,
                input_hash: 0,
            },
        ];
        let bytes = encode_manifest(&entries);
        let mut reader = Reader::new(&bytes);
        assert_eq!(
            FrameKind::from_tag(reader.u8().unwrap()),
            Ok(FrameKind::EntityManifest)
        );
        assert_eq!(decode_manifest(&mut reader).unwrap(), entries);
    }

    #[test]
    fn manifest_with_a_hostile_count_reports_eof_without_overallocating() {
        let mut writer = Writer::new();
        writer.varint(u64::MAX); // count
        let bytes = writer.into_inner();
        assert_eq!(
            decode_manifest(&mut Reader::new(&bytes)),
            Err(CodecError::UnexpectedEof)
        );
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
            let _ = Handshake::decode(&buf);
            let _ = Welcome::decode(&mut Reader::new(&buf));
            let _ = Ping::decode(&mut Reader::new(&buf));
            let _ = Pong::decode(&mut Reader::new(&buf));
            let _ = decode_manifest(&mut Reader::new(&buf));
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
