/**
 * Dependency-neutral reconstruction of compact assistant streaming updates.
 * Both extension-host endpoints use this reducer so wire-equivalent deltas
 * produce byte-equivalent hook payloads.
 */
export class AssistantDeltaReducer {
	private activeAssistant: Record<string, unknown> | undefined;
	private readonly activeToolArguments = new Map<number, StreamingJsonParser>();

	seedActiveAssistant(message: Record<string, unknown>): void {
		this.activeAssistant = structuredClone(message);
		this.activeToolArguments.clear();
	}

	clearActiveAssistant(): void {
		this.activeAssistant = undefined;
		this.activeToolArguments.clear();
	}

	applyAssistantDelta(event: Record<string, unknown>): void {
		const meta = isRecord(event["meta"]) ? event["meta"] : {};
		if (this.activeAssistant === undefined) {
			if (event["type"] !== "start") {
				throw new Error("message update arrived before assistant start");
			}
			this.activeAssistant = { ...meta, content: [] };
		} else if (event["type"] === "start") {
			this.activeAssistant = { ...meta, content: [] };
			this.activeToolArguments.clear();
		} else {
			const content = this.activeAssistant["content"];
			this.activeAssistant = { ...this.activeAssistant, ...meta, content };
		}

		const content = this.activeAssistant["content"];
		if (!Array.isArray(content)) {
			throw new Error("active assistant content is not an array");
		}
		const index = event["contentIndex"];
		const type = event["type"];
		const isStart = type === "text_start" || type === "thinking_start" || type === "toolcall_start";
		const isEnd = type === "text_end" || type === "thinking_end" || type === "toolcall_end";
		// A `*_start` must append exactly at the end of the array: a lower index
		// would silently overwrite an already-streamed block (and reset tool-call
		// argument tracking), while a higher one would leave a gap. Delta and end
		// events may only touch blocks that already exist.
		if (typeof index !== "number" || !Number.isInteger(index) || index < 0) {
			return;
		}
		if (isStart ? index !== content.length : index >= content.length) {
			return;
		}
		if ((isStart || isEnd) && isRecord(event["block"])) {
			content[index] = structuredClone(event["block"]);
			if (type === "toolcall_start") this.activeToolArguments.set(index, new StreamingJsonParser());
			if (type === "toolcall_end") this.activeToolArguments.delete(index);
			return;
		}

		const delta = event["delta"];
		const block = content[index];
		if (typeof delta !== "string" || !isRecord(block)) return;
		if (type === "text_delta") {
			block["text"] = `${typeof block["text"] === "string" ? block["text"] : ""}${delta}`;
		} else if (type === "thinking_delta") {
			block["thinking"] = `${typeof block["thinking"] === "string" ? block["thinking"] : ""}${delta}`;
		} else if (type === "toolcall_delta") {
			// Deltas can arrive without a start (hostile or reordered stream);
			// treat a missing parser as an empty fragment buffer, matching the
			// previous `?? ""` accumulation semantics.
			let parser = this.activeToolArguments.get(index);
			if (parser === undefined) {
				parser = new StreamingJsonParser();
				this.activeToolArguments.set(index, parser);
			}
			block["arguments"] = parser.push(delta);
		}
	}

	expandAssistantEvent(
		event: Record<string, unknown>,
		partial: Record<string, unknown>,
	): Record<string, unknown> {
		const type = event["type"] as string;
		const expanded: Record<string, unknown> = { type, partial };
		const index = event["contentIndex"];
		if (typeof index === "number") expanded["contentIndex"] = index;
		if (typeof event["delta"] === "string") expanded["delta"] = event["delta"];
		const content = partial["content"];
		const block = Array.isArray(content) && typeof index === "number" ? content[index] : undefined;
		if (type === "text_end" && isRecord(block)) expanded["content"] = block["text"];
		if (type === "thinking_end" && isRecord(block)) expanded["content"] = block["thinking"];
		if (type === "toolcall_end" && isRecord(block)) expanded["toolCall"] = block;
		return expanded;
	}

	getActiveAssistant(): Record<string, unknown> | undefined {
		return this.activeAssistant;
	}
}

function isRecord(value: unknown): value is Record<string, unknown> {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** Tolerantly parse possibly-incomplete streamed tool-call arguments. */
export function parseStreamingJson(text: string | undefined): Record<string, unknown> {
	if (text === undefined || text.trim() === "") return {};
	try {
		const strict: unknown = JSON.parse(text);
		return isRecord(strict) ? strict : {};
	} catch {
		return recoverStreamingJsonTail(text) ?? {};
	}
}

const MAX_STREAMING_JSON_RECOVERY_CHARS = 512;

interface OpenContainerNode {
	readonly close: "}" | "]";
	readonly parent: OpenContainerNode | undefined;
}

interface RecoveryBoundary {
	readonly end: number;
	readonly inString: boolean;
	readonly escaped: boolean;
	readonly stack: OpenContainerNode | undefined;
}

function recoverStreamingJsonTail(text: string): Record<string, unknown> | undefined {
	const scanner = new StreamingJsonScanner();
	scanner.append(text);
	const boundary = scanner.boundary();
	if (boundary === undefined) return undefined;
	try {
		const parsed: unknown = JSON.parse(closeAtBoundary(text, boundary));
		return isRecord(parsed) ? parsed : {};
	} catch {
		return undefined;
	}
}

type RecoveryPhase =
	| "keyOrEnd"
	| "keyRequired"
	| "colon"
	| "value"
	| "valueOrEnd"
	| "valueRequired"
	| "commaOrEnd";

interface RecoveryFrame {
	readonly node: OpenContainerNode;
	readonly parent: RecoveryFrame | undefined;
	readonly kind: "object" | "array";
	phase: RecoveryPhase;
}

/**
 * How the scan stopped for good. The recorded position/state is final; only
 * the recovery-window gating (`firstCandidate`) keeps moving as text grows.
 */
type TerminalScan =
	| { readonly kind: "stop" }
	| { readonly kind: "string"; readonly end: number; readonly stack: OpenContainerNode | undefined };

/** Mid-string resume state: `index` sits inside a string literal. */
interface StringScanState {
	readonly isKey: boolean;
	escaped: boolean;
	unicodeDigits: number;
}

/**
 * Resumable matcher for one JSON scalar token (`true`/`false`/`null` or a
 * number). Tracks the longest COMPLETE scalar prefix (`accept`) so a token
 * split across fragments never needs re-scanning: a paused token resumes
 * from `state`, and death is confirmed by one char or a delimiter exactly
 * where a whole-buffer scan would confirm it.
 */
interface ScalarScanState {
	readonly start: number;
	dfa:
		| "start" | "minus" | "intZero" | "int" | "dot" | "frac"
		| "expStart" | "expSign" | "exp" | "litDone" | "dead"
		| { readonly word: "true" | "false" | "null"; matched: number };
	/** Absolute end of the longest valid complete scalar prefix, if any. */
	accept: number | undefined;
}

/** One DFA step; returns false when the char kills the token. */
function scalarFeed(state: ScalarScanState, char: string, index: number): boolean {
	const dfa = state.dfa;
	if (dfa === "dead") return false;
	if (typeof dfa === "object") {
		if (char === dfa.word.charAt(dfa.matched)) {
			const matched = dfa.matched + 1;
			state.dfa = matched === dfa.word.length ? "litDone" : { word: dfa.word, matched };
			if (state.dfa === "litDone") state.accept = index + 1;
			return true;
		}
		state.dfa = "dead";
		return false;
	}
	switch (dfa) {
		case "start":
			if (char === "-") { state.dfa = "minus"; return true; }
			if (char === "0") { state.dfa = "intZero"; state.accept = index + 1; return true; }
			if (char >= "1" && char <= "9") { state.dfa = "int"; state.accept = index + 1; return true; }
			if (char === "t") { state.dfa = { word: "true", matched: 1 }; return true; }
			if (char === "f") { state.dfa = { word: "false", matched: 1 }; return true; }
			if (char === "n") { state.dfa = { word: "null", matched: 1 }; return true; }
			break;
		case "minus":
			if (char === "0") { state.dfa = "intZero"; state.accept = index + 1; return true; }
			if (char >= "1" && char <= "9") { state.dfa = "int"; state.accept = index + 1; return true; }
			break;
		case "intZero":
			if (char === ".") { state.dfa = "dot"; return true; }
			if (char === "e" || char === "E") { state.dfa = "expStart"; return true; }
			break;
		case "int":
			if (char >= "0" && char <= "9") { state.accept = index + 1; return true; }
			if (char === ".") { state.dfa = "dot"; return true; }
			if (char === "e" || char === "E") { state.dfa = "expStart"; return true; }
			break;
		case "dot":
			if (char >= "0" && char <= "9") { state.dfa = "frac"; state.accept = index + 1; return true; }
			break;
		case "frac":
			if (char >= "0" && char <= "9") { state.accept = index + 1; return true; }
			if (char === "e" || char === "E") { state.dfa = "expStart"; return true; }
			break;
		case "expStart":
			if (char === "+" || char === "-") { state.dfa = "expSign"; return true; }
			if (char >= "0" && char <= "9") { state.dfa = "exp"; state.accept = index + 1; return true; }
			break;
		case "expSign":
			if (char >= "0" && char <= "9") { state.dfa = "exp"; state.accept = index + 1; return true; }
			break;
		case "exp":
			if (char >= "0" && char <= "9") { state.accept = index + 1; return true; }
			break;
		case "litDone":
			break;
	}
	state.dfa = "dead";
	return false;
}

/**
 * Incremental JSON prefix scanner behind streaming-argument recovery.
 *
 * The scan state machine is a verbatim port of the original one-shot lexer;
 * the difference is WHEN the 512-char recovery window is applied. The batch
 * lexer gated boundary candidates against the final text length mid-scan;
 * here candidates are recorded unconditionally and the window gate is
 * evaluated at query time against the current length. Both produce the same
 * boundary: the window start only moves forward, so any candidate inside
 * today's window was recordable when scanned. Per-push work is proportional
 * to the appended bytes instead of re-lexing the whole buffer per delta.
 */
class StreamingJsonScanner {
	private text = "";
	private root: "value" | "done" = "value";
	private stack: RecoveryFrame | undefined;
	/** Absolute offset of the next unscanned char. */
	private index = 0;
	private inString: StringScanState | undefined;
	private inScalar: ScalarScanState | undefined;
	/** Candidates that passed `canClose`, kept only while inside the window. */
	private candidates: RecoveryBoundary[] = [];
	private terminal: TerminalScan | undefined;
	/** Any non-whitespace char seen so far (the scan may stop before the end). */
	private sawNonWhitespace = false;
	/** End of the last non-whitespace char (strict-parse cache key). */
	private lastNonWhitespaceEnd = 0;
	/** Characters visited by the scan loop (test-observable work counter). */
	scannedChars = 0;

	get sawAnyNonWhitespace(): boolean {
		return this.sawNonWhitespace;
	}

	get contentEnd(): number {
		return this.lastNonWhitespaceEnd;
	}

	/**
	 * True iff the buffer is a complete JSON document: the scanner validates
	 * the full JSON grammar, so an incomplete/invalid scan implies
	 * `JSON.parse` would throw and the caller may skip the attempt.
	 */
	get isCompleteJson(): boolean {
		if (this.terminal !== undefined || this.inString !== undefined) return false;
		if (this.index !== this.text.length) return false;
		const scalar = this.inScalar;
		if (scalar !== undefined) {
			// A top-level scalar completes the document iff it is accepted in
			// full and ends exactly at the buffer end.
			return (
				this.stack === undefined &&
				this.root === "value" &&
				scalar.accept === this.text.length
			);
		}
		return this.root === "done" && this.stack === undefined;
	}

	/** Append and scan new text. No-op scan once a terminal state is recorded. */
	append(fragment: string): void {
		this.text += fragment;
		const text = this.text;
		while (this.terminal === undefined && this.index < text.length) {
			if (this.inString !== undefined) {
				this.scanStringChar();
				continue;
			}
			const char = text.charAt(this.index);
			this.scannedChars += 1;
			if (this.inScalar !== undefined) {
				this.scanScalarChar(char);
				continue;
			}
			if (isJsonWhitespace(char)) {
				this.index += 1;
				this.rememberBoundary(this.index);
				continue;
			}
			this.sawNonWhitespace = true;
			this.lastNonWhitespaceEnd = this.index + 1;
			if (char === "{" || char === "[") {
				if (!this.openContainer(char === "{" ? "object" : "array")) {
					this.terminal = { kind: "stop" };
					return;
				}
				this.index += 1;
				this.rememberBoundary(this.index);
				continue;
			}
			if (char === "}" || char === "]") {
				if (
					this.stack === undefined ||
					(this.stack.kind === "object" ? char !== "}" : char !== "]") ||
					(this.stack.phase !== "keyOrEnd" &&
						this.stack.phase !== "valueOrEnd" &&
						this.stack.phase !== "commaOrEnd")
				) {
					this.terminal = { kind: "stop" };
					return;
				}
				this.stack = this.stack.parent;
				this.index += 1;
				this.rememberBoundary(this.index);
				continue;
			}
			if (char === ",") {
				if (this.stack === undefined || this.stack.phase !== "commaOrEnd") {
					this.terminal = { kind: "stop" };
					return;
				}
				this.stack.phase = this.stack.kind === "object" ? "keyRequired" : "valueRequired";
				this.index += 1;
				continue;
			}
			if (char === ":") {
				if (this.stack?.kind !== "object" || this.stack.phase !== "colon") {
					this.terminal = { kind: "stop" };
					return;
				}
				this.stack.phase = "value";
				this.index += 1;
				continue;
			}
			if (char === '"') {
				const isKey = this.stack?.kind === "object" &&
					(this.stack.phase === "keyOrEnd" || this.stack.phase === "keyRequired");
				if (!isKey && !this.valueExpected()) {
					this.terminal = { kind: "stop" };
					return;
				}
				this.inString = { isKey, escaped: false, unicodeDigits: 0 };
				this.index += 1;
				continue;
			}
			if (!this.valueExpected()) {
				this.terminal = { kind: "stop" };
				return;
			}
			this.inScalar = { start: this.index, dfa: "start", accept: undefined };
			scalarFeed(this.inScalar, char, this.index);
			this.index += 1;
		}
		if (this.terminal === undefined) this.pruneCandidates();
	}

	/**
	 * The furthest tail position closable into valid JSON, computed against the
	 * CURRENT text length — the same result a fresh whole-buffer scan gives.
	 */
	boundary(): RecoveryBoundary | undefined {
		const firstCandidate = Math.max(1, this.text.length - MAX_STREAMING_JSON_RECOVERY_CHARS);
		const lastGated = this.lastCandidateAtOrAfter(firstCandidate);
		const terminal = this.terminal;
		if (terminal !== undefined) {
			if (terminal.kind === "stop") return lastGated;
			return terminal.end >= firstCandidate
				? { end: terminal.end, inString: true, escaped: false, stack: terminal.stack }
				: lastGated;
		}
		const scalar = this.inScalar;
		if (scalar !== undefined) {
			// A live scalar token: a whole-buffer scan would complete the value,
			// accept the token's longest valid prefix as a candidate, then stop
			// on the leftover. Mirror that: the completion mutates the top frame
			// to `commaOrEnd` (or finishes a top-level value), which is exactly
			// what `canCloseAfterScalarCompletion` simulates.
			if (scalar.accept === undefined || !this.canCloseAfterScalarCompletion()) {
				return lastGated;
			}
			if (scalar.accept < firstCandidate) return lastGated;
			return { end: scalar.accept, inString: false, escaped: false, stack: this.stack?.node };
		}
		const inString = this.inString;
		if (inString === undefined || this.index < this.text.length) return lastGated;
		if (inString.isKey) return lastGated;
		if (inString.unicodeDigits > 0) {
			const end = this.text.length + inString.unicodeDigits - 6;
			return end >= firstCandidate
				? { end, inString: true, escaped: false, stack: this.stack?.node }
				: lastGated;
		}
		return {
			end: this.text.length,
			inString: true,
			escaped: inString.escaped,
			stack: this.stack?.node,
		};
	}

	/** Feed one char to a paused string scan. */
	private scanStringChar(): void {
		const state = this.inString;
		if (state === undefined) return;
		const char = this.text.charAt(this.index);
		this.scannedChars += 1;
		if (state.unicodeDigits > 0) {
			if (!isHexDigit(char)) {
				this.terminal = state.isKey
					? { kind: "stop" }
					: {
						kind: "string",
						end: this.index + state.unicodeDigits - 6,
						stack: this.stack?.node,
					};
				return;
			}
			state.unicodeDigits -= 1;
			this.index += 1;
			return;
		}
		if (state.escaped) {
			if (char === "u") state.unicodeDigits = 4;
			else if (!'"\\/bfnrt'.includes(char)) {
				this.terminal = state.isKey
					? { kind: "stop" }
					: { kind: "string", end: this.index - 1, stack: this.stack?.node };
				return;
			}
			state.escaped = false;
			this.index += 1;
			return;
		}
		if (char === "\\") {
			state.escaped = true;
			this.index += 1;
			return;
		}
		if (char === '"') {
			this.inString = undefined;
			if (state.isKey) {
				const frame = this.stack;
				if (frame === undefined) {
					this.terminal = { kind: "stop" };
					return;
				}
				frame.phase = "colon";
			} else if (!this.completeValue()) {
				this.terminal = { kind: "stop" };
				return;
			}
			this.index += 1;
			this.rememberBoundary(this.index);
			return;
		}
		if (char.charCodeAt(0) < 0x20) {
			this.terminal = state.isKey
				? { kind: "stop" }
				: { kind: "string", end: this.index, stack: this.stack?.node };
			return;
		}
		this.index += 1;
	}

	/** Feed one char to a paused scalar scan; delimiters commit or kill it. */
	private scanScalarChar(char: string): void {
		const state = this.inScalar;
		if (state === undefined) return;
		if (isScalarDelimiter(char)) {
			this.commitScalar();
			return;
		}
		this.lastNonWhitespaceEnd = this.index + 1;
		if (scalarFeed(state, char, this.index)) {
			this.index += 1;
			return;
		}
		// One killing char confirms what a whole-buffer scan would only learn
		// at the delimiter: the token's longest valid prefix, then a stop.
		this.finishDeadScalar();
	}

	/** Delimiter reached: accept the token in full, or stop at its prefix. */
	private commitScalar(): void {
		const state = this.inScalar;
		if (state === undefined) return;
		const dead = state.dfa === "dead" || state.accept !== this.index;
		if (dead) {
			this.finishDeadScalar();
			return;
		}
		this.inScalar = undefined;
		if (!this.completeValue()) {
			this.terminal = { kind: "stop" };
			return;
		}
		this.rememberBoundary(this.index);
	}

	/** The token cannot extend further: record its valid prefix and stop. */
	private finishDeadScalar(): void {
		const state = this.inScalar;
		if (state === undefined) return;
		this.inScalar = undefined;
		if (state.accept !== undefined && this.completeValue()) {
			this.rememberBoundary(state.accept);
		}
		this.terminal = { kind: "stop" };
	}

	/**
	 * Whether `canClose` would hold after completing a live scalar value:
	 * `completeValue` must succeed (value expected here), the top frame then
	 * reads `commaOrEnd` (closable), and every parent frame must already be
	 * closable. A top-level scalar leaves an empty stack with root `done`.
	 */
	private canCloseAfterScalarCompletion(): boolean {
		if (!this.valueExpected()) return false;
		for (let frame = this.stack?.parent; frame !== undefined; frame = frame.parent) {
			if (
				frame.phase !== "keyOrEnd" &&
				frame.phase !== "valueOrEnd" &&
				frame.phase !== "commaOrEnd"
			) {
				return false;
			}
		}
		return true;
	}

	private canClose(): boolean {
		if (this.root !== "done") return false;
		for (let frame = this.stack; frame !== undefined; frame = frame.parent) {
			if (
				frame.phase !== "keyOrEnd" &&
				frame.phase !== "valueOrEnd" &&
				frame.phase !== "commaOrEnd"
			) {
				return false;
			}
		}
		return true;
	}

	private rememberBoundary(end: number): void {
		if (!this.canClose()) return;
		const last = this.candidates[this.candidates.length - 1];
		if (last?.end === end) return;
		this.candidates.push({ end, inString: false, escaped: false, stack: this.stack?.node });
	}

	/** Candidates left of the window can never be selected again. */
	private pruneCandidates(): void {
		const firstCandidate = Math.max(1, this.text.length - MAX_STREAMING_JSON_RECOVERY_CHARS);
		let drop = 0;
		while (drop < this.candidates.length) {
			const candidate = this.candidates[drop];
			if (candidate === undefined || candidate.end >= firstCandidate) break;
			drop += 1;
		}
		if (drop > 0) this.candidates.splice(0, drop);
	}

	private lastCandidateAtOrAfter(firstCandidate: number): RecoveryBoundary | undefined {
		for (let index = this.candidates.length - 1; index >= 0; index--) {
			const candidate = this.candidates[index];
			if (candidate !== undefined && candidate.end >= firstCandidate) return candidate;
		}
		return undefined;
	}

	private valueExpected(): boolean {
		return this.stack === undefined
			? this.root === "value"
			: this.stack.kind === "object"
				? this.stack.phase === "value"
				: this.stack.phase === "valueOrEnd" || this.stack.phase === "valueRequired";
	}

	private completeValue(): boolean {
		if (this.stack === undefined) {
			if (this.root !== "value") return false;
			this.root = "done";
			return true;
		}
		if (!this.valueExpected()) return false;
		this.stack.phase = "commaOrEnd";
		return true;
	}

	private openContainer(kind: RecoveryFrame["kind"]): boolean {
		if (!this.completeValue()) return false;
		const node: OpenContainerNode = {
			close: kind === "object" ? "}" : "]",
			parent: this.stack?.node,
		};
		this.stack = {
			node,
			parent: this.stack,
			kind,
			phase: kind === "object" ? "keyOrEnd" : "valueOrEnd",
		};
		return true;
	}
}

/**
 * Incremental tolerant parser for one tool call's argument stream. Each push
 * scans only the new fragment; per-push scan cost is proportional to the
 * fragment length. The strict `JSON.parse` runs only once the buffer scans
 * as a complete JSON document (an incomplete scan implies the parse would
 * throw), and both strict and recovery results are cached by the scan key so
 * unchanged outcomes never re-parse. Results are byte-identical to reparsing
 * the whole buffer per fragment.
 */
export class StreamingJsonParser {
	private text = "";
	private readonly scanner = new StreamingJsonScanner();
	private lastKey: string | undefined;
	private lastResult: Record<string, unknown> = {};
	/** Strict + recovery parse entries (test-observable work counter). */
	parseAttempts = 0;

	get scannedChars(): number {
		return this.scanner.scannedChars;
	}

	push(fragment: string): Record<string, unknown> {
		if (fragment.length > 0) {
			this.text += fragment;
			this.scanner.append(fragment);
		}
		if (!this.scanner.sawAnyNonWhitespace) return {};
		let source: string;
		let key: string;
		if (this.scanner.isCompleteJson) {
			// Whitespace-only growth after completion keeps the same document.
			key = `strict:${this.scanner.contentEnd}`;
			source = this.text;
		} else {
			const boundary = this.scanner.boundary();
			if (boundary === undefined) return {};
			// The stack at a position is a pure function of the text prefix,
			// so (end, inString, escaped) pins the parse outcome.
			key = `${boundary.end}:${boundary.inString}:${boundary.escaped}`;
			source = closeAtBoundary(this.text, boundary);
		}
		if (key === this.lastKey) return this.lastResult;
		this.parseAttempts += 1;
		try {
			const parsed: unknown = JSON.parse(source);
			this.lastResult = isRecord(parsed) ? parsed : {};
		} catch {
			// The completeness gate is a conservative skip hint; a throw here
			// (e.g. pathological nesting) falls back to the tolerant result.
			this.lastResult = {};
		}
		this.lastKey = key;
		return this.lastResult;
	}
}

function isJsonWhitespace(char: string): boolean {
	return char === " " || char === "\t" || char === "\r" || char === "\n";
}

function isScalarDelimiter(char: string): boolean {
	return isJsonWhitespace(char) || char === "," || char === "}" || char === "]";
}

function isHexDigit(char: string): boolean {
	return /[0-9a-fA-F]/.test(char);
}

function closeAtBoundary(text: string, boundary: RecoveryBoundary): string {
	let closed = text.slice(0, boundary.end);
	if (boundary.inString) {
		if (boundary.escaped) closed = closed.slice(0, -1);
		closed += '"';
	}
	for (let node = boundary.stack; node !== undefined; node = node.parent) closed += node.close;
	return closed;
}
