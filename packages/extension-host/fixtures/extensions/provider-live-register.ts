/**
 * Registers "live_provider" from a command after bindCore. The command also
 * releases the concurrent-load fixture when that test is active.
 */
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { createAssistantMessageEventStream } from "@earendil-works/pi-ai";

function baseMessage(model: string) {
	return {
		role: "assistant" as const,
		content: [] as Array<{ type: "text"; text: string }>,
		api: "custom",
		provider: "live_provider",
		model,
		usage: {
			input: 0,
			output: 0,
			cacheRead: 0,
			cacheWrite: 0,
			totalTokens: 0,
			cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
		},
		stopReason: "stop" as const,
		timestamp: 0,
	};
}

export default function providerLiveRegister(pi: ExtensionAPI): void {
	pi.registerCommand("registerLiveProvider", {
		description: "Register a provider after bindCore",
		async handler() {
			pi.registerProvider("live_provider", {
				baseUrl: "https://live.example",
				api: "custom",
				streamSimple: () => {
					const stream = createAssistantMessageEventStream();
					void (async () => {
						stream.push({ type: "start", partial: baseMessage("live-marker") });
						stream.push({ type: "done", reason: "stop", message: baseMessage("live-marker") });
						stream.end();
					})();
					return stream;
				},
			});
			const coordination = globalThis as typeof globalThis & {
				__providerLoadRelease?: () => void;
			};
			coordination.__providerLoadRelease?.();
			delete coordination.__providerLoadRelease;
		},
	});
}
