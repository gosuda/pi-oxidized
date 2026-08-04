/**
 * SessionManager proxy thenable fixture: exercises `setup` with the
 * SessionManager proxy to verify it is not a broken thenable.
 * The proxy must return `undefined` for `then` so awaiting it
 * (or resolving a promise with it) does not throw.
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function sessionManagerProxyExtension(pi: ExtensionAPI): void {
	pi.registerCommand("sessionManagerThenProbe", {
		description: "Verify the SessionManager proxy is not a thenable",
		async handler(_args, ctx) {
			let setupRan = false;
			let managerIsObject = false;
			let thenIsUndefined = false;
			await ctx.newSession({
				parentSession: "parent-1",
				setup: async (manager) => {
					// If the proxy were a thenable, awaiting it would call
					// `then` and throw. Instead, this should resolve normally.
					await manager;
					setupRan = true;
					managerIsObject = typeof manager === "object" && manager !== null;
					thenIsUndefined = (manager as unknown as Record<string, unknown>)["then"] === undefined;
				},
				withSession: async (freshCtx) => {
					// Report from the fresh context (the old ctx is stale after newSession).
					freshCtx.ui.notify(
						JSON.stringify({ setupRan, managerIsObject, thenIsUndefined }),
						"info",
					);
				},
			});
		},
	});
}
