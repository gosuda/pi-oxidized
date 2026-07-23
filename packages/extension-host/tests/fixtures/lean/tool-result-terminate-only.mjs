/**
 * tool_result hook that only sets `terminate`. Content/details/isError must
 * stay absent from the response so Rust does not mark them changed.
 */

export default {
	name: "tool-result-terminate-only",
	hooks: {
		tool_result: () => ({ terminate: true }),
	},
};
