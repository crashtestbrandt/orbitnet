//! What the payload cipher costs per datagram — the figures `docs/protocol.md` quotes.
//!
//! **A session that configures a secret runs ChaCha20-Poly1305 over every datagram it sends and every
//! one it receives, and derives a cipher key once per join. How much is each?** The send path already
//! runs a SipHash pass over the same bytes, so the figure that matters is the difference between the
//! two regimes rather than the cipher's cost on its own. Both regimes are timed here.
//!
//! Three lengths, chosen to bracket what a session actually sends:
//!
//! * **40 bytes** — a ping, a pong, an ack-only input frame. Dominated by the fixed cost: ChaCha20
//!   generates a block for the Poly1305 key whatever the payload's length.
//! * **200 bytes** — a typical client input frame, a few owned bodies and their redundancy window.
//! * **1200 bytes** — `MAX_FRAME_PAYLOAD`, which is what a server's snapshot frame to a peer on a full
//!   budget looks like every send pass.
//!
//! Each arm times `seal` and `open` as a pair, because a session pays both: one end seals, the other
//! opens, and a listen host does both. **The columns report one pass** — half a pair — since that is
//! what a sender pays, and again what a receiver pays.
//!
//! **A cipher key is built per datagram**, as the shipped path does it — ChaCha20's key schedule is the
//! key itself, so there is nothing to amortize and `SessionAuth` stays `Copy`. The cost of building it
//! is inside these figures rather than outside them.
//!
//! **The per-join derivation is timed separately**, in its own arm. It is charged once at the seat
//! rather than per datagram, and the cipher key runs four SipHash passes where the MAC key runs two.
//!
//! Ignored by default so `cargo test` stays fast, and `--test-threads=1` is not optional: the arms are
//! timing loops and run concurrently they contend for the same cores.
//!
//! ```text
//! cargo test -p orbitnet-core --release --test cipher_bench -- --ignored --nocapture \
//!   --test-threads=1
//! ```

use std::time::Instant;

use orbitnet_core::auth::{
    compress_secret, derive_cipher_key, derive_session_key, session_nonce, Direction, SessionAuth,
    KEY_LEN,
};

/// Datagrams per arm. Large enough that the clock's resolution is not the measurement.
const ITERS: u32 = 200_000;

/// Payload lengths, in bytes: a ping, an input frame, a full snapshot frame.
const LENGTHS: [usize; 3] = [40, 200, 1200];

fn keys() -> ([u8; KEY_LEN], [u8; 32]) {
    let secret = compress_secret(b"a secret the lobby handed both ends");
    let nonce = session_nonce(&[0xa5; KEY_LEN], &[0x5a; KEY_LEN]);
    (
        derive_session_key(&secret, &nonce),
        derive_cipher_key(&secret, &nonce),
    )
}

/// Time `ITERS` seal/open pairs over a payload of `len` bytes, answering microseconds per pair.
///
/// The sealed datagram is opened by a receiver that is re-seated every iteration, so the replay
/// window never refuses one. Seating costs a `[u8; 32]` copy and is charged to both arms alike.
fn time_pairs(len: usize, encrypted: bool) -> f64 {
    let (key, cipher_key) = keys();
    let payload = vec![0x42u8; len];
    let mut scratch: Vec<u8> = Vec::with_capacity(len);
    let start = Instant::now();
    for _ in 0..ITERS {
        let mut sender = if encrypted {
            SessionAuth::encrypted(key, cipher_key)
        } else {
            SessionAuth::new(key)
        };
        let mut datagram = payload.clone();
        sender
            .seal(Direction::ToServer, &mut datagram)
            .expect("a fresh session has sequence numbers");
        let mut receiver = if encrypted {
            SessionAuth::encrypted(key, cipher_key)
        } else {
            SessionAuth::new(key)
        };
        let opened = receiver
            .open(Direction::ToServer, &datagram, &mut scratch)
            .expect("what one end sealed, the other opens");
        // The optimizer may not delete the work that produced this.
        std::hint::black_box(opened);
    }
    let elapsed = start.elapsed();
    assert_eq!(scratch.len(), if encrypted { len } else { 0 });
    elapsed.as_secs_f64() * 1e6 / f64::from(ITERS)
}

/// Time `ITERS` derivations of one key, answering microseconds per derivation.
///
/// `cipher` picks [`derive_cipher_key`]'s four SipHash passes over [`derive_session_key`]'s two, over
/// the same compressed secret and folded nonce both ends hold at the seat.
fn time_derivation(cipher: bool) -> f64 {
    let secret = compress_secret(b"a secret the lobby handed both ends");
    let nonce = session_nonce(&[0xa5; KEY_LEN], &[0x5a; KEY_LEN]);
    let start = Instant::now();
    for _ in 0..ITERS {
        if cipher {
            std::hint::black_box(derive_cipher_key(
                std::hint::black_box(&secret),
                std::hint::black_box(&nonce),
            ));
        } else {
            std::hint::black_box(derive_session_key(
                std::hint::black_box(&secret),
                std::hint::black_box(&nonce),
            ));
        }
    }
    start.elapsed().as_secs_f64() * 1e6 / f64::from(ITERS)
}

#[test]
#[ignore = "measurement harness; run with --ignored --nocapture"]
fn the_key_derivations_cost_per_join() {
    println!();
    println!("key derivation, {ITERS} derivations per arm");
    let session = time_derivation(false);
    let cipher = time_derivation(true);
    println!("{:>26}  {:>10}", "derivation", "us/join");
    println!("{:>26}  {session:>10.3}", "session key, 2 passes");
    println!("{:>26}  {cipher:>10.3}", "cipher key, 4 passes");
    println!("{:>26}  {:>10.3}", "both, under a secret", session + cipher);
    println!();
    println!("  charged once per join per end, not per datagram.");
}

#[test]
#[ignore = "measurement harness; run with --ignored --nocapture"]
fn the_payload_cipher_costs_per_datagram() {
    println!();
    println!("payload cipher, {ITERS} seal/open pairs per arm");
    println!(
        "{:>7}  {:>13}  {:>14}  {:>13}  {:>7}",
        "bytes", "clear us/pass", "cipher us/pass", "added us/pass", "ratio"
    );
    for len in LENGTHS {
        let clear = time_pairs(len, false) / 2.0;
        let cipher = time_pairs(len, true) / 2.0;
        println!(
            "{len:>7}  {clear:>13.3}  {cipher:>14.3}  {:>13.3}  {:>6.2}x",
            cipher - clear,
            cipher / clear
        );
    }
    println!();
    println!("  one pass is one direction: what a sender pays, and again what a receiver pays.");
}
