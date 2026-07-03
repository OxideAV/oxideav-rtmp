//! TCP teardown helper shared by every graceful `close()` in the
//! crate.
//!
//! All four session-ending paths (`RtmpClient::close`,
//! `RtmpPlayer::close`, `RtmpSession::close`, `PlaySession::close`)
//! follow the same shape: write the protocol-level goodbye
//! (`closeStream` / `deleteStream` / `UserControl StreamEOF` /
//! `onStatus`), flush, send a write-half FIN — and then the owning
//! struct drops, closing the socket. The hazard is in that last step:
//! if the peer's own final messages (an `onStatus` reply, an
//! Acknowledgement, a mirrored teardown command) are still unread in
//! our receive queue when the descriptor closes, the kernel answers
//! them with an RST instead of completing the orderly shutdown — and
//! an RST is allowed to discard everything the *peer* has not yet
//! read, including the goodbye we just flushed. On a loaded scheduler
//! this loses final frames / status messages nondeterministically.
//!
//! [`drain_until_fin`] closes the window: after the write-half FIN,
//! read and discard inbound bytes until the peer's FIN arrives (clean
//! EOF), bounded by a deadline so a peer that never closes cannot
//! wedge the teardown. Because we already sent our FIN before
//! draining, two peers draining each other cannot deadlock — each
//! side's drain is waiting on a FIN the other side has already sent
//! or will send on its own drop.

use std::io::Read;
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// Per-read poll granularity while draining.
const DRAIN_POLL: Duration = Duration::from_millis(50);

/// Give up once the peer has been silent for this long: whatever it
/// sent in reaction to our goodbye has been consumed, and a peer that
/// merely holds its half open (no FIN) shouldn't stall the teardown.
const DRAIN_IDLE_CUTOFF: Duration = Duration::from_millis(300);

/// Default overall drain budget used by the `close()` paths.
pub(crate) const DRAIN_BUDGET: Duration = Duration::from_secs(2);

/// Read and discard inbound bytes on `stream` until the peer sends
/// its FIN (read returns 0), the line has been idle for
/// [`DRAIN_IDLE_CUTOFF`], an error surfaces, or `budget` elapses.
///
/// Best-effort by design: every failure mode simply returns — the
/// caller is tearing the connection down regardless; the drain only
/// exists so the drop-time `close(2)` finds an empty receive queue
/// and completes as an orderly shutdown rather than an RST.
pub(crate) fn drain_until_fin(stream: &TcpStream, budget: Duration) {
    let mut sock = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    if sock.set_read_timeout(Some(DRAIN_POLL)).is_err() {
        return;
    }
    let deadline = Instant::now() + budget;
    let mut last_data = Instant::now();
    let mut buf = [0u8; 4096];
    while Instant::now() < deadline {
        match sock.read(&mut buf) {
            // Peer FIN — the receive queue is drained and the
            // connection can close cleanly.
            Ok(0) => return,
            // Late peer data (status replies, acks, mirrored
            // teardown) — discard and keep going.
            Ok(_) => last_data = Instant::now(),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                // Poll tick: a peer that has gone quiet without
                // closing has nothing more in flight to protect.
                if last_data.elapsed() >= DRAIN_IDLE_CUTOFF {
                    return;
                }
            }
            // Reset / abort / anything else: nothing left to protect.
            Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use std::thread;

    /// Peer writes a few late bytes then closes: the drain consumes
    /// them and returns promptly on the FIN — well before the budget.
    #[test]
    fn drain_consumes_late_data_and_returns_on_fin() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let peer = thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            sock.write_all(&[0xAB; 512]).expect("late write");
            // Drop → FIN.
        });
        let stream = TcpStream::connect(addr).expect("connect");
        let started = Instant::now();
        drain_until_fin(&stream, Duration::from_secs(5));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "drain must return on the peer FIN, not the budget"
        );
        peer.join().expect("peer");
    }

    /// Peer never closes and sends nothing: the drain gives up at the
    /// idle cutoff instead of wedging the teardown until the budget.
    #[test]
    fn drain_respects_idle_cutoff_when_peer_stays_open() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let stream = TcpStream::connect(addr).expect("connect");
        let (peer, _) = listener.accept().expect("accept");
        let started = Instant::now();
        drain_until_fin(&stream, Duration::from_secs(10));
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(250),
            "drain returned before the idle cutoff: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "drain ran to the budget despite an idle line: {elapsed:?}"
        );
        drop(peer);
    }
}
