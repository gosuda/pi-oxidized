/**
 * tool_call hook that returns a block decision without touching event.input.
 * Regression witness for the wire omission contract: Rust must see no `input`
 * in the response so arguments stay unchanged.
 */

export default {
	name: "tool-call-noop",
	hooks: {
		tool_call: () => ({ block: false, reason: "noop-ack" }),
	},
};
