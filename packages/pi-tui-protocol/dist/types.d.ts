/**
 * Versioned extension-host / pi-tui protocol types.
 *
 * Mirrors `crates/pi-ext/src/protocol.rs`. Rust is the authoritative
 * validation boundary; this package provides identical discriminants and a
 * stream-only client (no terminal access).
 */
/** Wire protocol version negotiated in hello / helloAck. */
export declare const PROTOCOL_VERSION: 1;
/** Compatibility target: reference `@earendil-works/pi-coding-agent` version. */
export declare const COMPATIBILITY_VERSION: "0.80.10";
/** Maximum UTF-8 byte length of one frame line (excluding trailing newline). */
export declare const MAX_FRAME_BYTES: number;
/** Correlation identifier for request/response/error frames. */
export type FrameId = number;
/** Frame kind discriminant on the wire. */
export type FrameKind = "req" | "res" | "event" | "error";
/** Allowlisted bridge and host-control methods. */
export type Method = "hello" | "toolUpdate" | "providerEvent" | "uiSlot" | "disposeSlot" | "extensionError" | "select" | "confirm" | "input" | "editor" | "notify" | "terminalInput" | "uiEvent" | "measure" | "render";
/** All allowlisted methods in stable order. */
export declare const METHODS: readonly Method[];
/** Returns true when `raw` is an allowlisted method. */
export declare function isMethod(raw: string): raw is Method;
/** One protocol frame. */
export interface Frame {
    id: FrameId;
    kind: FrameKind;
    method: string;
    payload: unknown;
}
/** Client → host hello request payload. */
export interface Hello {
    protocolVersion: number;
    compatibilityVersion: string;
}
/** Host → client hello acknowledgment payload. */
export interface HelloAck {
    protocolVersion: number;
    compatibilityVersion: string;
}
/** Structured error payload for `kind: "error"` frames. */
export interface ErrorPayload {
    code: string;
    message: string;
    retryable: boolean;
    data?: unknown;
}
/** Named ANSI palette colors. */
export type NamedColor = "black" | "red" | "green" | "yellow" | "blue" | "magenta" | "cyan" | "white" | "brightBlack" | "brightRed" | "brightGreen" | "brightYellow" | "brightBlue" | "brightMagenta" | "brightCyan" | "brightWhite";
/** Allowlisted color encoding for styled runs. */
export type WireColor = {
    type: "named";
    name: NamedColor;
} | {
    type: "indexed";
    index: number;
} | {
    type: "rgb";
    r: number;
    g: number;
    b: number;
};
/** Validated OSC 8 hyperlink fields. */
export interface Hyperlink {
    id?: string;
    uri: string;
}
/** Allowlisted text style for structured UI runs (no raw ANSI). */
export interface Style {
    bold?: boolean;
    dim?: boolean;
    italic?: boolean;
    underline?: boolean;
    reverse?: boolean;
    strikethrough?: boolean;
    fg?: WireColor;
    bg?: WireColor;
    link?: Hyperlink;
}
/** One contiguous styled text run. */
export interface StyledRun {
    text: string;
    style?: Style;
}
/** Where a host UI slot is placed. */
export type SlotPlacement = "header" | "footer" | "aboveEditor" | "belowEditor" | "editor" | "messageRenderer" | "overlay";
/** Absolute cells or percent string (`"50%"`). */
export type SizeValue = number | `${number}%`;
/** Overlay anchor points. */
export type OverlayAnchor = "center" | "top-left" | "top-right" | "bottom-left" | "bottom-right" | "top-center" | "bottom-center" | "left-center" | "right-center";
/** Per-side overlay margin. */
export interface OverlayMargin {
    top?: number;
    right?: number;
    bottom?: number;
    left?: number;
}
/** Serializable overlay layout options (no host callbacks). */
export interface OverlayOptions {
    width?: SizeValue;
    minWidth?: number;
    maxHeight?: SizeValue;
    anchor?: OverlayAnchor;
    offsetX?: number;
    offsetY?: number;
    row?: SizeValue;
    col?: SizeValue;
    margin?: OverlayMargin | number;
    nonCapturing?: boolean;
}
/** Cursor cell within a focusable slot. */
export interface SlotCursor {
    col: number;
    row: number;
}
/** Host → Rust `uiSlot` event payload. */
export interface UiSlot {
    key: string;
    generation: number;
    placement: SlotPlacement;
    height: number;
    runs: StyledRun[][];
    focusable?: boolean;
    cursor?: SlotCursor;
    overlayOptions?: OverlayOptions;
}
/** Dispose a keyed slot. */
export interface DisposeSlot {
    key: string;
    generation?: number;
}
/** Non-retryable extension failure event payload. */
export interface ExtensionErrorEvent {
    code: string;
    message: string;
    retryable?: boolean;
    data?: unknown;
}
/** Partial tool update from the host. */
export interface ToolUpdate {
    toolCallId: string;
    toolName: string;
    partialResult: unknown;
}
/** Custom provider stream event from the host. */
export interface ProviderEvent {
    providerId: string;
    callId: string;
    event: string;
    data?: unknown;
}
/** Key modifiers on the wire. */
export interface KeyModifiersWire {
    shift?: boolean;
    alt?: boolean;
    ctrl?: boolean;
    superKey?: boolean;
}
/** Key event kind on the wire. */
export type KeyEventKindWire = "press" | "release" | "repeat";
/** Structured UI event (never terminal-native types). */
export type UiEventWire = {
    type: "key";
    code: string;
    modifiers?: KeyModifiersWire;
    kind?: KeyEventKindWire;
} | {
    type: "paste";
    text: string;
} | {
    type: "focusGained";
} | {
    type: "focusLost";
} | {
    type: "resize";
    width: number;
    height: number;
};
/** Terminal-input rewrite / consume result. */
export interface TerminalInputResult {
    consume?: boolean;
    data?: string;
}
/** Dialog timeout option. */
export interface DialogOptions {
    timeoutMs?: number;
}
/** `select` request payload. */
export interface SelectRequest extends DialogOptions {
    title: string;
    options: string[];
}
/** `select` response payload. */
export interface SelectResponse {
    value?: string | null;
}
/** `confirm` request payload. */
export interface ConfirmRequest extends DialogOptions {
    title: string;
    message: string;
}
/** `confirm` response payload. */
export interface ConfirmResponse {
    confirmed: boolean;
}
/** `input` request payload. */
export interface InputRequest extends DialogOptions {
    title: string;
    placeholder?: string;
}
/** `input` response payload. */
export interface InputResponse {
    value?: string | null;
}
/** `editor` request payload. */
export interface EditorRequest {
    title: string;
    prefill?: string;
}
/** `editor` response payload. */
export interface EditorResponse {
    value?: string | null;
}
/** Notification level. */
export type NotifyLevel = "info" | "warning" | "error";
/** `notify` payload. */
export interface NotifyRequest {
    message: string;
    type?: NotifyLevel;
}
/** Measure/render request shared fields. */
export interface SlotRenderRequest {
    key: string;
    width: number;
    themeGeneration: number;
}
/** Measure response height. */
export interface MeasureResponse {
    height: number;
}
/** Local hello payload for this build. */
export declare function localHello(): Hello;
/** Local hello acknowledgment payload. */
export declare function localHelloAck(): HelloAck;
//# sourceMappingURL=types.d.ts.map