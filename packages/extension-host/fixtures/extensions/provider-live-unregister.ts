/**
 * Registers "unreg_provider" during load and removes it from a command after
 * bindCore.
 */
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function providerLiveUnregister(pi: ExtensionAPI): void {
	pi.registerProvider("unreg_provider", {
		baseUrl: "https://unreg.example",
		api: "custom",
		streamSimple: () => {
			throw new Error("should not be called after unregister");
		},
	});
	pi.registerCommand("unregisterLiveProvider", {
		description: "Unregister a provider after bindCore",
		async handler() {
			pi.unregisterProvider("unreg_provider");
		},
	});
}
