//! PAR-PTY-GRILL (issue #46): OSC52 clipboard encoder adjudication.
//!
//! C13/OSC52 claim: the OSC 52 encoder produces correct escape sequences
//! with base64 payload and rejects oversized payloads. This is unit-level
//! evidence; the PTY fixture does not exercise clipboard actions.

use pi::core::platform::clipboard::osc52_encode;

/// OSC52 VERIFIED (unit-level): encoder produces correct ESC ]52;c;...BEL
/// sequences with base64 payload.
#[expect(
    clippy::expect_used,
    reason = "test assertion: OSC52 must encode small text"
)]
#[test]
fn grill_osc52_encoder_correct_sequence() {
    let encoded = osc52_encode("hello").expect("OSC52 must encode small text");
    assert!(
        encoded.starts_with("\u{1b}]52;c;"),
        "OSC52: must start with ESC ]52;c;"
    );
    assert!(
        encoded.ends_with('\u{07}'),
        "OSC52: must terminate with BEL"
    );
    // base64 of "hello" = "aGVsbG8="
    assert!(
        encoded.contains("aGVsbG8="),
        "OSC52: must contain correct base64 payload"
    );
}

/// OSC52 VERIFIED (unit-level): oversized payloads are rejected.
#[test]
fn grill_osc52_rejects_oversized() {
    let big = "x".repeat(200_000);
    assert!(
        osc52_encode(&big).is_none(),
        "OSC52: must reject payloads exceeding MAX_OSC52_ENCODED_LENGTH"
    );
}
