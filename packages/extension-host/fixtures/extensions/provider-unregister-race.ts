/**
 * Provides a command that unregisters "race_provider" live (outside any
 * ProviderLoadScope).  Used in conjunction with provider-staged-defeated.ts
 * to prove that a live unregister at order N+1 defeats a staged registration
 * at order N when the staged registration is later committed.
 */
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function providerUnregisterRace(pi: ExtensionAPI): void {
	pi.registerCommand("unregisterRaceProvider", {
		description: "Unregister race_provider live after bindCore",
		async handler() {
			pi.unregisterProvider("race_provider");
		},
	});
}
