/**
 * Lean message_update cancel fixture. Returns CancelWire on text_delta so
 * the runner must forward `{ cancel, reason }` on the message_update_delta
 * response (matching Mode 1 / Rust emit_message_update_delta). Non-delta
 * updates return void so the wire keeps `{ ok: true }`.
 */

export default {
	name: "message-update-cancel",
	hooks: {
		message_update: (event) => {
			if (event.assistantMessageEvent?.type === "text_delta") {
				if (event.assistantMessageEvent?.delta === "veto") {
					return { cancel: true, reason: "stop-from-lean" };
				}
				// Non-cancel return must keep the `{ ok: true }` response.
				return { cancel: false };
			}
		},
	},
};
