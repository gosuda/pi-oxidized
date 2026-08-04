/**
 * tool_call hook that only reorders object keys on event.input.
 * Witness for order-insensitive mutation detection: the host must see no `input`
 * in the response because the JSON value is unchanged.
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function toolCallReorderExtension(pi: ExtensionAPI): void {
	pi.on("tool_call", (event) => {
		const entries = Object.entries(event.input).reverse();
		for (const key of Object.keys(event.input)) {
			delete (event.input as Record<string, unknown>)[key];
		}
		for (const [key, value] of entries) {
			(event.input as Record<string, unknown>)[key] = value;
		}
		return { block: false, reason: "reorder-ack" };
	});
}
