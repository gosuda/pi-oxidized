/**
 * tool_call hook that only reorders object keys on event.input.
 * Witness for order-insensitive mutation detection: Rust must see no `input`
 * in the response because the JSON value is unchanged.
 */

export default {
	name: "tool-call-reorder",
	hooks: {
		tool_call: (event) => {
			const entries = Object.entries(event.input).reverse();
			for (const key of Object.keys(event.input)) {
				delete event.input[key];
			}
			for (const [key, value] of entries) {
				event.input[key] = value;
			}
			return { block: false, reason: "reorder-ack" };
		},
	},
};
