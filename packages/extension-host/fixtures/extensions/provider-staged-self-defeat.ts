/**
 * Factory that stages a provider registration during load (order N), signals
 * that it has staged, pauses until released, and then stages an unregister of
 * the same name (order N+1) before completing.  The load-completion flush
 * applies register(N) then unregister(N+1); the higher-order unregister
 * tombstone must defeat the registration at commit despite the deferred,
 * async ordering.
 *
 * No timers or sleeps — coordination is via global callbacks, following the
 * same convention as provider-staged-defeated.ts.
 */
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

type ProviderLoadCoordination = typeof globalThis & {
	__providerStagedStarted?: () => void;
	__providerStagedRelease?: () => void;
};

export default async function providerStagedSelfDefeat(pi: ExtensionAPI): Promise<void> {
	const coordination = globalThis as ProviderLoadCoordination;

	// Stage a registration during load — the reference loader holds it in
	// pendingRuntimeChanges until the load completes.
	pi.registerProvider("race_provider", {
		baseUrl: "https://staged.example",
		api: "custom",
		streamSimple: () => {
			throw new Error("should not survive — defeated by the staged unregister at N+1");
		},
	});

	// Install the release hook BEFORE signalling, so a synchronous release
	// from the started callback cannot be dropped.
	const released = new Promise<void>((resolve) => {
		coordination.__providerStagedRelease = resolve;
	});

	// Signal that the registration has been staged and we are about to pause.
	coordination.__providerStagedStarted?.();

	// Wait for release — after release we stage the higher-order unregister
	// that must defeat the registration above.
	await released;
	pi.unregisterProvider("race_provider");
}
