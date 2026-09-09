# Changelog

All notable changes to the pi Rust port are documented in this file.

## [Unreleased]

### Added
- Test kit `RecordingSession::observe_frame_until` observes a settled frame against a caller predicate and defers the drained raw output to the next recorded output boundary, so transcripts stay whole-frame with no loss or duplication; closing with unrecorded observed output fails closed (`RecordingError::UnrecordedObservation`).
- crossterm is pinned to the exact released 0.29.0 source vendored at `vendor/crossterm-0.29.0` and resolved as a path dependency through `[patch.crates-io]`; upstream version, license, and tag identity are unchanged. Provenance, the local modifications, and their limits are documented in `vendor/crossterm-0.29.0/VENDORED.txt`.
- Release archives stage the repository docs tree, README.md, and CHANGELOG.md, with per-file SHA-256 digests recorded in release.json [#111]
- Release-path CHANGELOG gate: every release build (dry-run and full) fails when the root CHANGELOG.md is missing or its Unreleased section is empty [#111]
- Terminal capability overrides for hyperlinks, inline images, and true color via `PI_HYPERLINKS`, `PI_IMAGE_PROTOCOL`, and `PI_TRUE_COLOR` environment variables and `terminal.hyperlinks`, `terminal.images`, and `terminal.trueColor` JSON settings, with live reload that replaces only those three capabilities.

### Fixed

- Large tool results that cross the context threshold now compact before the next assistant request in the same run. Compaction preserves queued steering and stops cleanly when the run is cancelled or terminated.
- Terminal resizing now keeps the inline viewport aligned with the rendered frame and restores its requested height when the terminal grows.
- The PTY witness verifies real paste and cursor input instead of relying on scripted counters.
- Default terminal controls now ignore key-release events while preserving key repeats and release delivery to extension input handlers and focused slots.
- Terminal query replies (OSC 11 background color, cell size) are consumed as terminal protocol by one persistent input decoder shared across startup probing, steady-state input, and requeries, instead of decoding as keyboard input. An ambiguous lone `ESC` is held under an absolute 50 ms deadline measured from the first `ESC` byte (never reset by later bytes) and then decodes as the exact ordinary key sequence; once a full `ESC ] 11 ;` header is recognized, reply payload and terminator fragments remain protocol across arbitrary idle gaps, timeouts, and handoffs. Recognized OSC 11 replies carry at most a 64-byte payload into a bounded 16-entry reply queue; recognized malformed or oversized reply framing surfaces an explicit input error with in-place recovery instead of replaying payload bytes as keys. Normal keys, bracketed paste, and cursor position/DA1 replies are unchanged. The framing deadline bounds ESC-ambiguity latency only, reply collection requires the usual single-reader discipline, and the event-source drain is fairness-bounded (budget windows with control re-probes, or one read per poll iteration) rather than an all-timing guarantee; the full contract is documented in `vendor/crossterm-0.29.0/VENDORED.txt`.
- The test kit probe responder retains the tail of the combined residual scan across reads, so a terminal query fragmented across three or more input chunks is still recognized and answered instead of losing its earlier prefix.
