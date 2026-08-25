/**
 * Fixture that registers a versioned provider and waits for a global release.
 * Used in pairs to prove an older overlapping `extensions.load` for the same
 * path cannot restore its staged provider state after a newer load.
 */
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

type OverlapCoordination = typeof globalThis & {
	__overlapProviderVersion?: number;
	__overlapLoadStartedCount?: number;
	__overlapLoadResolvers?: Array<() => void>;
};

export default async function loadOverlap(pi: ExtensionAPI): Promise<void> {
	const coordination = globalThis as OverlapCoordination;
	const mine = (coordination.__overlapProviderVersion ??= 0) + 1;
	coordination.__overlapProviderVersion = mine;
	pi.registerProvider(`overlap_provider_${mine}`, {
		baseUrl: `https://overlap-${mine}.example`,
		api: "custom",
		streamSimple: () => {
			throw new Error("should not be called");
		},
	});
	await new Promise<void>((resolve) => {
		if (coordination.__overlapLoadResolvers === undefined) {
			coordination.__overlapLoadResolvers = [];
		}
		coordination.__overlapLoadResolvers.push(resolve);
		coordination.__overlapLoadStartedCount = (coordination.__overlapLoadStartedCount ?? 0) + 1;
	});
}
