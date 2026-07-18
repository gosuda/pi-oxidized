import { type Frame, type Method } from "./types.js";
/** Protocol encode/decode/validation failure. */
export declare class ProtocolError extends Error {
    readonly details?: unknown | undefined;
    readonly code: "frame_too_large" | "invalid_utf8" | "invalid_json" | "malformed_frame" | "invalid_frame" | "version_mismatch" | "compatibility_mismatch" | "unknown_method" | "truncated";
    constructor(code: ProtocolError["code"], message: string, details?: unknown | undefined);
}
/**
 * Validate id/kind rules and optionally the method allowlist.
 * @throws {ProtocolError}
 */
export declare function validateFrame(frame: Frame, requireAllowlisted?: boolean): void;
/** Build a typed request frame. */
export declare function requestFrame(id: number, method: Method, payload?: unknown): Frame;
/** Build a typed response frame. */
export declare function responseFrame(id: number, method: Method, payload?: unknown): Frame;
/** Build a typed event frame. */
export declare function eventFrame(id: number, method: Method, payload?: unknown): Frame;
/** Build an error frame. */
export declare function errorFrame(id: number, method: Method, error: {
    code: string;
    message: string;
    retryable?: boolean;
    data?: unknown;
}): Frame;
/**
 * Encode a frame to a UTF-8 JSON line including the trailing `\n`.
 * @throws {ProtocolError}
 */
export declare function encodeFrame(frame: Frame): Uint8Array;
/**
 * Encode a frame as a UTF-8 string including the trailing newline.
 * @throws {ProtocolError}
 */
export declare function encodeFrameString(frame: Frame): string;
/**
 * Decode one complete JSON line (no trailing newline required).
 * @throws {ProtocolError}
 */
export declare function decodeFrameLine(line: string | Uint8Array): Frame;
/**
 * Decode one complete JSON line string into a frame.
 * @throws {ProtocolError}
 */
export declare function decodeFrameStr(line: string): Frame;
/**
 * Decode and require an allowlisted method.
 * @throws {ProtocolError}
 */
export declare function decodeFrameStrStrict(line: string): Frame;
/**
 * Incremental JSONL frame decoder with a hard size bound.
 *
 * Accepts partial chunks, multiple frames per push, LF or CRLF separators,
 * and rejects oversize lines before the internal buffer grows past the limit.
 */
export declare class FrameDecoder {
    private buf;
    private readonly maxFrameBytes;
    private readonly textDecoder;
    constructor(maxFrameBytes?: number);
    /** Bytes currently buffered (incomplete line). */
    get bufferedLen(): number;
    /**
     * Push bytes and return every complete frame decoded from this chunk.
     * @throws {ProtocolError}
     */
    push(chunk: Uint8Array): Frame[];
    /**
     * Finish the stream: error if a partial non-whitespace line remains.
     * @throws {ProtocolError}
     */
    finish(): Frame | undefined;
    /**
     * Finish accepting a final line without a trailing newline (EOF flush).
     * @throws {ProtocolError}
     */
    finishWithFinalLine(): Frame | undefined;
    /** Reset buffered state. */
    reset(): void;
    private decodeBytes;
}
//# sourceMappingURL=codec.d.ts.map