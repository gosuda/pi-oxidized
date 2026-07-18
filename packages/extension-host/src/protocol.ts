/**
 * Protocol mirror for the extension host. Re-exports the versioned JSONL
 * protocol surface from the sibling `@earendil-works/pi-tui-protocol` package,
 * keeping a single source of truth alongside `crates/pi-ext/src/protocol.rs`.
 *
 * The host is the *trusted* side of this bridge: it receives Rust requests
 * and emits structured events, but never forwards raw ANSI or terminal bytes.
 */

export * from "@earendil-works/pi-tui-protocol";
