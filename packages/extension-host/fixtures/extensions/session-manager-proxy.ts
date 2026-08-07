/**
 * SessionManager proxy thenable fixture: exercises `setup` with the
 * SessionManager proxy to verify it is not a broken thenable.
 * The proxy must return `undefined` for `then` so awaiting it
 * (or resolving a promise with it) does not throw.
 *
 * Consumers of the completed-path report ({ setupRan, managerIsObject,
 * thenIsUndefined }):
 *   - host.test.ts "awaiting the SessionManager proxy does not throw"
 *     asserts all three fields are true.
 * The cancelled-path report ({ cancelled: true }) is distinct from silence
 * so verification can tell "cancelled" from "never executed".
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function sessionManagerProxyExtension(pi: ExtensionAPI): void {
	pi.registerCommand("sessionManagerThenProbe", {
		description: "Verify the SessionManager proxy is not a thenable",
		async handler(_args, ctx) {
			let setupRan = false;
			let managerIsObject = false;
			let thenIsUndefined = false;
			const result = await ctx.newSession({
				parentSession: "parent-1",
				setup: async (manager) => {
					// If the proxy were a thenable, awaiting it would call
					// `then` and throw. Instead, this should resolve normally.
					await manager;
					setupRan = true;
					managerIsObject = typeof manager === "object" && manager !== null;
					// Assert the value the runtime actually reads: thenable resolution
					// consults only the `get` trap, not `has`. A Proxy can answer
					// `"then" in manager` with true while `get` returns undefined.
					thenIsUndefined = Reflect.get(manager, "then") === undefined;
				},
				withSession: async (freshCtx) => {
					// Report from the fresh context (the old ctx is stale after newSession).
					freshCtx.ui.notify(
						JSON.stringify({ setupRan, managerIsObject, thenIsUndefined }),
						"info",
					);
				},
			});
			// A cancelled replacement skips setup and withSession, so the
			// completed-path notify never fires. Emit an explicit cancelled
			// outcome so the consuming verification can distinguish "cancelled"
			// from "probe never ran".
			if (result.cancelled) {
				ctx.ui.notify(JSON.stringify({ cancelled: true }), "info");
			}
		},
	});
}
