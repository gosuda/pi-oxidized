/**
 * Dependency-neutral reconstruction of compact assistant streaming updates.
 * Both extension-host endpoints use this reducer so wire-equivalent deltas
 * produce byte-equivalent hook payloads.
 */
export class AssistantDeltaReducer {
	private activeAssistant: Record<string, unknown> | undefined;
	private readonly activeToolArguments = new Map<number, string>();

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
			if (type === "toolcall_start") this.activeToolArguments.set(index, "");
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
			const fragments = `${this.activeToolArguments.get(index) ?? ""}${delta}`;
			this.activeToolArguments.set(index, fragments);
			block["arguments"] = parseStreamingJson(fragments);
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
	const boundary = collectRecoveryBoundary(text);
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

/** Finds the furthest tail position that can be made valid by closing JSON. */
function collectRecoveryBoundary(text: string): RecoveryBoundary | undefined {
	const firstCandidate = Math.max(1, text.length - MAX_STREAMING_JSON_RECOVERY_CHARS);
	let root: "value" | "done" = "value";
	let stack: RecoveryFrame | undefined;
	let lastBoundary: RecoveryBoundary | undefined;

	const canClose = (): boolean => {
		if (root !== "done") return false;
		for (let frame = stack; frame !== undefined; frame = frame.parent) {
			if (
				frame.phase !== "keyOrEnd" &&
				frame.phase !== "valueOrEnd" &&
				frame.phase !== "commaOrEnd"
			) {
				return false;
			}
		}
		return true;
	};

	const rememberBoundary = (end: number): void => {
		if (end >= firstCandidate && canClose()) {
			lastBoundary = { end, inString: false, escaped: false, stack: stack?.node };
		}
	};

	const stringBoundary = (end: number): RecoveryBoundary | undefined =>
		end >= firstCandidate
			? { end, inString: true, escaped: false, stack: stack?.node }
			: lastBoundary;

	const valueExpected = (): boolean =>
		stack === undefined
			? root === "value"
			: stack.kind === "object"
				? stack.phase === "value"
				: stack.phase === "valueOrEnd" || stack.phase === "valueRequired";

	const completeValue = (): boolean => {
		if (stack === undefined) {
			if (root !== "value") return false;
			root = "done";
			return true;
		}
		if (!valueExpected()) return false;
		stack.phase = "commaOrEnd";
		return true;
	};

	const openContainer = (kind: RecoveryFrame["kind"]): boolean => {
		if (!completeValue()) return false;
		const node: OpenContainerNode = {
			close: kind === "object" ? "}" : "]",
			parent: stack?.node,
		};
		stack = {
			node,
			parent: stack,
			kind,
			phase: kind === "object" ? "keyOrEnd" : "valueOrEnd",
		};
		return true;
	};

	for (let index = 0; index < text.length; ) {
		const char = text.charAt(index);
		if (isJsonWhitespace(char)) {
			index += 1;
			rememberBoundary(index);
			continue;
		}
		if (char === "{" || char === "[") {
			if (!openContainer(char === "{" ? "object" : "array")) return lastBoundary;
			index += 1;
			rememberBoundary(index);
			continue;
		}
		if (char === "}" || char === "]") {
			if (
				stack === undefined ||
				(stack.kind === "object" ? char !== "}" : char !== "]") ||
				(stack.phase !== "keyOrEnd" &&
					stack.phase !== "valueOrEnd" &&
					stack.phase !== "commaOrEnd")
			) {
				return lastBoundary;
			}
			stack = stack.parent;
			index += 1;
			rememberBoundary(index);
			continue;
		}
		if (char === ",") {
			if (stack === undefined || stack.phase !== "commaOrEnd") return lastBoundary;
			stack.phase = stack.kind === "object" ? "keyRequired" : "valueRequired";
			index += 1;
			continue;
		}
		if (char === ":") {
			if (stack?.kind !== "object" || stack.phase !== "colon") return lastBoundary;
			stack.phase = "value";
			index += 1;
			continue;
		}
		if (char === '"') {
			const isKey = stack?.kind === "object" &&
				(stack.phase === "keyOrEnd" || stack.phase === "keyRequired");
			if (!isKey && !valueExpected()) return lastBoundary;
			let escaped = false;
			let unicodeDigits = 0;
			let cursor = index + 1;
			for (; cursor < text.length; cursor += 1) {
				const stringChar = text.charAt(cursor);
				if (unicodeDigits > 0) {
					if (!isHexDigit(stringChar)) {
						return isKey ? lastBoundary : stringBoundary(cursor + unicodeDigits - 6);
					}
					unicodeDigits -= 1;
					continue;
				}
				if (escaped) {
					if (stringChar === "u") unicodeDigits = 4;
					else if (!'"\\/bfnrt'.includes(stringChar)) return isKey ? lastBoundary : stringBoundary(cursor - 1);
					escaped = false;
					continue;
				}
				if (stringChar === "\\") {
					escaped = true;
					continue;
				}
				if (stringChar === '"') break;
				if (stringChar.charCodeAt(0) < 0x20) return isKey ? lastBoundary : stringBoundary(cursor);
			}
			if (cursor === text.length) {
				if (unicodeDigits > 0) {
					return isKey ? lastBoundary : stringBoundary(cursor + unicodeDigits - 6);
				}
				if (isKey) return lastBoundary;
				return { end: cursor, inString: true, escaped, stack: stack?.node };
			}
			if (isKey) stack!.phase = "colon";
			else if (!completeValue()) return lastBoundary;
			index = cursor + 1;
			rememberBoundary(index);
			continue;
		}
		if (!valueExpected()) return lastBoundary;
		let cursor = index;
		while (cursor < text.length && !isScalarDelimiter(text.charAt(cursor))) cursor += 1;
		const scalarEnd = findCompleteJsonScalarEnd(text, index, cursor);
		if (scalarEnd === undefined) return lastBoundary;
		if (!completeValue()) return lastBoundary;
		rememberBoundary(scalarEnd);
		if (scalarEnd !== cursor) return lastBoundary;
		index = cursor;
	}
	return lastBoundary;
}

function isJsonWhitespace(char: string): boolean {
	return char === " " || char === "\t" || char === "\r" || char === "\n";
}

function isScalarDelimiter(char: string): boolean {
	return isJsonWhitespace(char) || char === "," || char === "}" || char === "]";
}

function isCompleteJsonScalar(value: string): boolean {
	return (
		value === "true" ||
		value === "false" ||
		value === "null" ||
		/^-?(?:0|[1-9]\d*)(?:\.\d+)?(?:[eE][+-]?\d+)?$/.test(value)
	);
}

function findCompleteJsonScalarEnd(text: string, start: number, end: number): number | undefined {
	const value = text.slice(start, end);
	if (isCompleteJsonScalar(value)) return end;
	const literal = /^(?:true|false|null)/.exec(value)?.[0];
	const number = /^-?(?:0|[1-9]\d*)(?:\.\d+)?(?:[eE][+-]?\d+)?/.exec(value)?.[0];
	const prefix = literal ?? number;
	return prefix === undefined ? undefined : start + prefix.length;
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
