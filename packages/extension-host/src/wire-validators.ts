/**
 * Wire-type guards shared by the extension host (`host.ts`) and the lean
 * runner (`lean-runner.ts`). Both sides validate the same JSONL bridge
 * payloads; a single module keeps the two copies from drifting apart.
 */

/** Narrow an unknown wire value to a plain object (rejects arrays and null). */
export function isRecord<T extends Record<string, unknown>>(value: unknown): value is T {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}

/**
 * Structured cancellation only: a real Error (or DOMException, which is
 * not Error-derived in every runtime) named AbortError. Message text is
 * deliberately never consulted — an extension failure that merely says
 * "cancelled" must stay an extension_error.
 */
export function isStructuredAbortError(error: unknown): boolean {
	if (error instanceof Error && error.name === "AbortError") return true;
	return typeof DOMException === "function"
		&& error instanceof DOMException
		&& error.name === "AbortError";
}
