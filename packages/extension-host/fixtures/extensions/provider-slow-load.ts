import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

type ProviderLoadCoordination = typeof globalThis & {
	__providerLoadStarted?: () => void;
	__providerLoadRelease?: () => void;
};

export default async function providerSlowLoad(_pi: ExtensionAPI): Promise<void> {
	const coordination = globalThis as ProviderLoadCoordination;
	await new Promise<void>((resolve) => {
		coordination.__providerLoadRelease = resolve;
		coordination.__providerLoadStarted?.();
	});
}
