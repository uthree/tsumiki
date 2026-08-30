//! Shared test helpers: real UDP over 127.0.0.1, hard-timeout pumping so a
//! stalled handshake fails the test instead of hanging CI.

use std::thread;
use std::time::{Duration, Instant};

/// Tick size used to drive `tick(dt)` while pumping in tests.
pub const TICK_DT: f32 = 0.005;

/// Repeatedly calls `step` (expected to drive+flush every transport under
/// test, then attempt whatever it's waiting for), sleeping briefly between
/// calls, until it returns `Some` or `timeout` elapses (in which case `None`
/// is returned — callers should `.expect(...)` to fail the test with a clear
/// message rather than hang).
///
/// Takes a single closure (rather than separate "tick" and "poll" closures)
/// so it can capture multiple transports mutably without two closures
/// fighting over the same `&mut` borrow.
pub fn pump_until<T>(timeout: Duration, mut step: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = step() {
            return Some(v);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(2));
    }
}

/// Pumps for a fixed wall-clock duration (no early exit), for asserting a
/// negative ("nothing arrives") outcome. Not every test file needs this.
#[allow(dead_code)]
pub fn pump_for(duration: Duration, mut tick: impl FnMut()) {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        tick();
        thread::sleep(Duration::from_millis(2));
    }
}
