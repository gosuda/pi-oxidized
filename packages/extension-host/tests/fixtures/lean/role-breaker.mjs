/**
 * Rejection fixture: a message_end hook that returns a message with the
 * WRONG role. The runner must emit an extensionError naming the same-role
 * rule and ignore the replacement — the original message stands.
 */

export default {
	name: "role-breaker",
	hooks: {
		message_end: (event) => ({
			message: {
				...event.message,
				role: event.message.role === "user" ? "assistant" : "user",
			},
		}),
	},
};
