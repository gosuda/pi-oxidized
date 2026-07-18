/**
 * Versioned extension-host / pi-tui protocol types.
 *
 * Mirrors `crates/pi-ext/src/protocol.rs`. Rust is the authoritative
 * validation boundary; this package provides identical discriminants and a
 * stream-only client (no terminal access).
 */
/** Wire protocol version negotiated in hello / helloAck. */
export const PROTOCOL_VERSION = 1;
/** Compatibility target: reference `@earendil-works/pi-coding-agent` version. */
export const COMPATIBILITY_VERSION = "0.80.10";
/** Maximum UTF-8 byte length of one frame line (excluding trailing newline). */
export const MAX_FRAME_BYTES = 8 * 1024 * 1024;
/** All allowlisted methods in stable order. */
export const METHODS = [
    "hello",
    "toolUpdate",
    "providerEvent",
    "uiSlot",
    "disposeSlot",
    "extensionError",
    "select",
    "confirm",
    "input",
    "editor",
    "notify",
    "terminalInput",
    "flags.set",
    "shortcut.execute",
    "uiEvent",
    "measure",
    "render",
];
const METHOD_SET = new Set(METHODS);
/** Returns true when `raw` is an allowlisted method. */
export function isMethod(raw) {
    return METHOD_SET.has(raw);
}
/** Local hello payload for this build. */
export function localHello() {
    return {
        protocolVersion: PROTOCOL_VERSION,
        compatibilityVersion: COMPATIBILITY_VERSION,
    };
}
/** Local hello acknowledgment payload. */
export function localHelloAck() {
    return {
        protocolVersion: PROTOCOL_VERSION,
        compatibilityVersion: COMPATIBILITY_VERSION,
    };
}
//# sourceMappingURL=types.js.map