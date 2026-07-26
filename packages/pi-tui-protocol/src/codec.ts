import {
	type Frame,
	type FrameKind,
	isMethod,
	MAX_FRAME_BYTES,
	type Method,
} from "./types.js";

/** Protocol encode/decode/validation failure. */
export class ProtocolError extends Error {
	readonly code:
		| "frame_too_large"
		| "invalid_utf8"
		| "invalid_json"
		| "malformed_frame"
		| "invalid_frame"
		| "version_mismatch"
		| "compatibility_mismatch"
		| "unknown_method"
		| "truncated";

	constructor(
		code: ProtocolError["code"],
		message: string,
		readonly details?: unknown,
	) {
		super(message);
		this.name = "ProtocolError";
		this.code = code;
	}
}

const FRAME_KINDS: ReadonlySet<string> = new Set(["req", "res", "event", "error"]);
const HYPERLINK_CONTROL_CHARS = /[\u0000-\u001F\u007F-\u009F]/u;

function requiresNonzeroId(kind: FrameKind): boolean {
	return kind === "req" || kind === "res";
}

/**
 * Validate id/kind rules and optionally the method allowlist.
 * @throws {ProtocolError}
 */
export function validateFrame(frame: Frame, requireAllowlisted = false): void {
	if (!FRAME_KINDS.has(frame.kind)) {
		throw new ProtocolError("malformed_frame", `unknown frame kind: ${String(frame.kind)}`);
	}
	if (requiresNonzeroId(frame.kind) && frame.id === 0) {
		throw new ProtocolError("invalid_frame", `kind ${frame.kind} requires nonzero id`);
	}
	if (typeof frame.method !== "string" || frame.method.length === 0) {
		throw new ProtocolError("invalid_frame", "method must be a non-empty string");
	}
	if (requireAllowlisted && !isMethod(frame.method)) {
		throw new ProtocolError("unknown_method", `unknown protocol method: ${frame.method}`);
	}
	const payload = frame.payload;
	if (
		payload !== null &&
		payload !== undefined &&
		(typeof payload === "boolean" ||
			typeof payload === "number" ||
			typeof payload === "string")
	) {
		throw new ProtocolError("invalid_frame", "payload must be a JSON object or array");
	}
	if (frame.method === "uiSlot") {
		validateUiSlotPayload(payload);
	}
}

function validateUiSlotPayload(payload: unknown): void {
	if (typeof payload !== "object" || payload === null || !("runs" in payload)) {
		throw new ProtocolError("invalid_frame", "invalid uiSlot payload: runs must be present");
	}
	const runs = payload.runs;
	if (!Array.isArray(runs)) {
		throw new ProtocolError("invalid_frame", "invalid uiSlot payload: runs must be an array");
	}
	for (const line of runs) {
		if (!Array.isArray(line)) {
			throw new ProtocolError("invalid_frame", "invalid uiSlot payload: each line must be an array");
		}
		for (const run of line) {
			if (typeof run !== "object" || run === null || !("style" in run) || run.style === undefined) {
				continue;
			}
			const style = run.style;
			if (typeof style !== "object" || style === null || !("link" in style) || style.link === undefined) {
				continue;
			}
			validateHyperlink(style.link);
		}
	}
	if ("overlayOptions" in payload && payload.overlayOptions !== undefined) {
		validateOverlayOptions(payload.overlayOptions);
	}
}

function validateHyperlink(value: unknown): void {
	if (typeof value !== "object" || value === null || !("uri" in value) || typeof value.uri !== "string") {
		throw new ProtocolError("invalid_frame", "hyperlink must contain a string uri");
	}
	if (new TextEncoder().encode(value.uri).byteLength > 2048) {
		throw new ProtocolError("invalid_frame", "hyperlink uri exceeds 2048 bytes");
	}
	if (HYPERLINK_CONTROL_CHARS.test(value.uri)) {
		throw new ProtocolError("invalid_frame", "hyperlink uri contains a control character");
	}
	if (!(value.uri.startsWith("http://") || value.uri.startsWith("https://"))) {
		throw new ProtocolError("invalid_frame", "hyperlink uri must use http or https");
	}
	if ("id" in value && value.id !== undefined) {
		if (typeof value.id !== "string") {
			throw new ProtocolError("invalid_frame", "hyperlink id must be a string");
		}
		if (new TextEncoder().encode(value.id).byteLength > 128) {
			throw new ProtocolError("invalid_frame", "hyperlink id exceeds 128 bytes");
		}
		if (HYPERLINK_CONTROL_CHARS.test(value.id)) {
			throw new ProtocolError("invalid_frame", "hyperlink id contains a control character");
		}
	}
}

function validateOverlayOptions(value: unknown): void {
	if (typeof value !== "object" || value === null) {
		throw new ProtocolError("invalid_frame", "overlayOptions must be an object");
	}
	if (!("margin" in value) || value.margin === undefined) {
		return;
	}
	const margin = value.margin;
	if (typeof margin === "number") {
		validateCellCount(margin, "overlayOptions.margin");
		return;
	}
	if (typeof margin !== "object" || margin === null || Array.isArray(margin)) {
		throw new ProtocolError("invalid_frame", "overlayOptions.margin must be a number or object");
	}
	for (const side of ["top", "right", "bottom", "left"] as const) {
		const sideValue = Reflect.get(margin, side);
		if (sideValue !== undefined) {
			validateCellCount(sideValue, `overlayOptions.margin.${side}`);
		}
	}
}

function validateCellCount(value: unknown, field: string): void {
	if (typeof value !== "number" || !Number.isInteger(value) || value < 0 || value > 65_535) {
		throw new ProtocolError("invalid_frame", `${field} must be an integer in 0..65535`);
	}
}

/** Build a typed request frame. */
export function requestFrame(id: number, method: Method, payload: unknown = {}): Frame {
	return { id, kind: "req", method, payload };
}

/** Build a typed response frame. */
export function responseFrame(id: number, method: Method, payload: unknown = {}): Frame {
	return { id, kind: "res", method, payload };
}

/** Build a typed event frame. */
export function eventFrame(id: number, method: Method, payload: unknown = {}): Frame {
	return { id, kind: "event", method, payload };
}

/** Build an error frame. */
export function errorFrame(
	id: number,
	method: Method,
	error: { code: string; message: string; retryable?: boolean; data?: unknown },
): Frame {
	const payload: Record<string, unknown> = {
		code: error.code,
		message: error.message,
		retryable: error.retryable ?? false,
	};
	if (error.data !== undefined) {
		payload.data = error.data;
	}
	return { id, kind: "error", method, payload };
}

/**
 * Encode a frame to a UTF-8 JSON line including the trailing `\n`.
 * @throws {ProtocolError}
 */
export function encodeFrame(frame: Frame): Uint8Array {
	validateFrame(frame, false);
	const json = JSON.stringify(frame);
	const encoder = new TextEncoder();
	const body = encoder.encode(json);
	if (body.byteLength > MAX_FRAME_BYTES) {
		throw new ProtocolError(
			"frame_too_large",
			`frame exceeds maximum size of ${MAX_FRAME_BYTES} bytes`,
		);
	}
	const out = new Uint8Array(body.byteLength + 1);
	out.set(body, 0);
	out[body.byteLength] = 0x0a;
	return out;
}

/**
 * Encode a frame as a UTF-8 string including the trailing newline.
 * @throws {ProtocolError}
 */
export function encodeFrameString(frame: Frame): string {
	validateFrame(frame, false);
	const json = JSON.stringify(frame);
	const bytes = new TextEncoder().encode(json);
	if (bytes.byteLength > MAX_FRAME_BYTES) {
		throw new ProtocolError(
			"frame_too_large",
			`frame exceeds maximum size of ${MAX_FRAME_BYTES} bytes`,
		);
	}
	return `${json}\n`;
}

function isFrameKind(value: unknown): value is FrameKind {
	return typeof value === "string" && FRAME_KINDS.has(value);
}

/**
 * Decode one complete JSON line (no trailing newline required).
 * @throws {ProtocolError}
 */
export function decodeFrameLine(line: string | Uint8Array): Frame {
	let text: string;
	if (typeof line === "string") {
		text = line;
	} else {
		try {
			text = new TextDecoder("utf-8", { fatal: true }).decode(line);
		} catch (error) {
			throw new ProtocolError("invalid_utf8", `invalid UTF-8 in protocol stream: ${String(error)}`);
		}
	}
	return decodeFrameStr(text);
}

/**
 * Decode one complete JSON line string into a frame.
 * @throws {ProtocolError}
 */
export function decodeFrameStr(line: string): Frame {
	const trimmed = line.endsWith("\r") ? line.slice(0, -1) : line;
	if (trimmed.length === 0) {
		throw new ProtocolError("malformed_frame", "empty line");
	}
	const byteLength = new TextEncoder().encode(trimmed).byteLength;
	if (byteLength > MAX_FRAME_BYTES) {
		throw new ProtocolError(
			"frame_too_large",
			`frame exceeds maximum size of ${MAX_FRAME_BYTES} bytes`,
		);
	}
	let parsed: unknown;
	try {
		parsed = JSON.parse(trimmed) as unknown;
	} catch (error) {
		throw new ProtocolError("invalid_json", `invalid JSON frame: ${String(error)}`);
	}
	if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) {
		throw new ProtocolError("malformed_frame", "frame must be a JSON object");
	}
	const obj = parsed as Record<string, unknown>;
	if (typeof obj.id !== "number" || !Number.isInteger(obj.id) || obj.id < 0) {
		throw new ProtocolError("malformed_frame", "id must be a non-negative integer");
	}
	if (!isFrameKind(obj.kind)) {
		throw new ProtocolError("malformed_frame", "kind must be req|res|event|error");
	}
	if (typeof obj.method !== "string") {
		throw new ProtocolError("malformed_frame", "method must be a string");
	}
	const frame: Frame = {
		id: obj.id,
		kind: obj.kind,
		method: obj.method,
		payload: obj.payload === undefined ? {} : obj.payload,
	};
	validateFrame(frame, false);
	return frame;
}

/**
 * Decode and require an allowlisted method.
 * @throws {ProtocolError}
 */
export function decodeFrameStrStrict(line: string): Frame {
	const frame = decodeFrameStr(line);
	validateFrame(frame, true);
	return frame;
}

/**
 * Incremental JSONL frame decoder with a hard size bound.
 *
 * Accepts partial chunks, multiple frames per push, LF or CRLF separators,
 * and rejects oversize lines before the internal buffer grows past the limit.
 */
export class FrameDecoder {
	private buf: Uint8Array = new Uint8Array(0);
	private readonly maxFrameBytes: number;
	private readonly textDecoder = new TextDecoder("utf-8", { fatal: true });

	constructor(maxFrameBytes: number = MAX_FRAME_BYTES) {
		this.maxFrameBytes = maxFrameBytes;
	}

	/** Bytes currently buffered (incomplete line). */
	get bufferedLen(): number {
		return this.buf.byteLength;
	}

	/**
	 * Push bytes and return every complete frame decoded from this chunk.
	 * @throws {ProtocolError}
	 */
	push(chunk: Uint8Array): Frame[] {
		const out: Frame[] = [];
		let offset = 0;
		while (offset < chunk.byteLength) {
			const newlineAt = chunk.indexOf(0x0a, offset);
			if (newlineAt === -1) {
				const pending = this.buf.byteLength + (chunk.byteLength - offset);
				if (pending > this.maxFrameBytes) {
					this.buf = new Uint8Array(0);
					throw new ProtocolError(
						"frame_too_large",
						`frame exceeds maximum size of ${this.maxFrameBytes} bytes`,
					);
				}
				this.buf = concat(this.buf, chunk.subarray(offset));
				break;
			}
			const pending = this.buf.byteLength + (newlineAt - offset);
			if (pending > this.maxFrameBytes) {
				this.buf = new Uint8Array(0);
				throw new ProtocolError(
					"frame_too_large",
					`frame exceeds maximum size of ${this.maxFrameBytes} bytes`,
				);
			}
			const lineBytes = concat(this.buf, chunk.subarray(offset, newlineAt));
			this.buf = new Uint8Array(0);
			const stripped =
				lineBytes.byteLength > 0 && lineBytes[lineBytes.byteLength - 1] === 0x0d
					? lineBytes.subarray(0, lineBytes.byteLength - 1)
					: lineBytes;
			out.push(this.decodeBytes(stripped));
			offset = newlineAt + 1;
		}
		return out;
	}

	/**
	 * Finish the stream: error if a partial non-whitespace line remains.
	 * @throws {ProtocolError}
	 */
	finish(): Frame | undefined {
		if (this.buf.byteLength === 0) {
			return undefined;
		}
		if (isAsciiWhitespace(this.buf)) {
			this.buf = new Uint8Array(0);
			return undefined;
		}
		this.buf = new Uint8Array(0);
		throw new ProtocolError("truncated", "truncated protocol frame at end of stream");
	}

	/**
	 * Finish accepting a final line without a trailing newline (EOF flush).
	 * @throws {ProtocolError}
	 */
	finishWithFinalLine(): Frame | undefined {
		if (this.buf.byteLength === 0) {
			return undefined;
		}
		if (this.buf.byteLength > this.maxFrameBytes) {
			this.buf = new Uint8Array(0);
			throw new ProtocolError(
				"frame_too_large",
				`frame exceeds maximum size of ${this.maxFrameBytes} bytes`,
			);
		}
		const lineBytes =
			this.buf.byteLength > 0 && this.buf[this.buf.byteLength - 1] === 0x0d
				? this.buf.subarray(0, this.buf.byteLength - 1)
				: this.buf;
		this.buf = new Uint8Array(0);
		if (lineBytes.byteLength === 0) {
			return undefined;
		}
		return this.decodeBytes(lineBytes);
	}

	/** Reset buffered state. */
	reset(): void {
		this.buf = new Uint8Array(0);
	}

	private decodeBytes(bytes: Uint8Array): Frame {
		let text: string;
		try {
			text = this.textDecoder.decode(bytes);
		} catch (error) {
			throw new ProtocolError("invalid_utf8", `invalid UTF-8 in protocol stream: ${String(error)}`);
		}
		return decodeFrameStr(text);
	}
}

function concat(a: Uint8Array, b: Uint8Array): Uint8Array {
	if (a.byteLength === 0) {
		return new Uint8Array(b);
	}
	if (b.byteLength === 0) {
		return new Uint8Array(a);
	}
	const out = new Uint8Array(a.byteLength + b.byteLength);
	out.set(a, 0);
	out.set(b, a.byteLength);
	return out;
}

function isAsciiWhitespace(bytes: Uint8Array): boolean {
	for (let i = 0; i < bytes.byteLength; i += 1) {
		const b = bytes[i];
		if (b === undefined) {
			return false;
		}
		// space, tab, CR, LF, VT, FF
		if (!(b === 0x20 || b === 0x09 || b === 0x0d || b === 0x0a || b === 0x0b || b === 0x0c)) {
			return false;
		}
	}
	return true;
}
