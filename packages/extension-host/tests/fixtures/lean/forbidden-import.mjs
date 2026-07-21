/**
 * Rejection fixture: a lean entry that statically imports the upstream
 * compat graph. The lean runner's load-time exclusion scan must reject it
 * with a per-extension load error (siblings keep loading).
 */
import { builtInExtensions } from "@earendil-works/pi-coding-agent/builtins";

export default {
	name: "forbidden",
	tools: [
		{
			name: "noop",
			description: String(builtInExtensions),
			execute: () => ({}),
		},
	],
};
