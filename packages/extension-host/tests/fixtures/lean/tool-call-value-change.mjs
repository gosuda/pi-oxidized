/**
 * tool_call hook that changes a real field value on event.input.
 * Witness that genuine mutations still echo `input` on the wire.
 */

export default {
	name: "tool-call-value-change",
	hooks: {
		tool_call: (event) => {
			event.input.a = "changed";
			return { block: false, reason: "value-ack" };
		},
	},
};
