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
	const boundaries = collectRecoveryBoundaries(text);
	for (const boundary of boundaries.reverse()) {
		try {
			const parsed: unknown = JSON.parse(closeAtBoundary(text, boundary));
			return isRecord(parsed) ? parsed : {};
		} catch {
			// Try the next shorter recovery boundary.
		}
	}
	return undefined;
}

function collectRecoveryBoundaries(text: string): RecoveryBoundary[] {
	const boundaries: RecoveryBoundary[] = [];
	const firstCandidate = Math.max(1, text.length - MAX_STREAMING_JSON_RECOVERY_CHARS);
	let inString = false;
	let escaped = false;
	let stack: OpenContainerNode | undefined;
	for (let index = 0; index < text.length; index++) {
		if (index >= firstCandidate) boundaries.push({ end: index, inString, escaped, stack });
		const char = text[index];
		if (inString) {
			if (escaped) escaped = false;
			else if (char === "\\") escaped = true;
			else if (char === '"') inString = false;
			continue;
		}
		if (char === '"') inString = true;
		else if (char === "{") stack = { close: "}", parent: stack };
		else if (char === "[") stack = { close: "]", parent: stack };
		else if (char === "}" || char === "]") {
			if (stack === undefined || stack.close !== char) return boundaries;
			stack = stack.parent;
		}
	}
	boundaries.push({ end: text.length, inString, escaped, stack });
	return boundaries;
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
