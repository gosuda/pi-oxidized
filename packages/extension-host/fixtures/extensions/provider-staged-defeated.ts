/**
 * Factory that stages a provider registration during load (while the
 * ProviderLoadScope is "loading"), signals that it has staged, then pauses
 * until released.  While paused, a live unregister from an already-loaded
 * extension can set a tombstone at a higher order.  When the factory
 * completes, the staged registration is applied but must be defeated by
 * the durable unregister-order tombstone.
 *
 * No timers or sleeps — coordination is via global callbacks, following the
 * same convention as provider-slow-load.ts.
 */
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

type ProviderLoadCoordination = typeof globalThis & {
	__providerStagedStarted?: () => void;
	__providerStagedRelease?: () => void;
};

export default async function providerStagedDefeated(pi: ExtensionAPI): Promise<void> {
	const coordination = globalThis as ProviderLoadCoordination;

	// Stage a registration during load — this goes into scope.operations,
	// not into the live provider map.  The order is assigned by the host.
	pi.registerProvider("race_provider", {
		baseUrl: "https://staged.example",
		api: "custom",
		streamSimple: () => {
			throw new Error("should not survive — defeated by live unregister");
		},
	});

	// Signal that the registration has been staged and we are about to pause.
	coordination.__providerStagedStarted?.();

	// Wait for release — during this pause, the test triggers a live
	// unregister of "race_provider" at a higher order.
	await new Promise<void>((resolve) => {
		coordination.__providerStagedRelease = resolve;
	});
}
