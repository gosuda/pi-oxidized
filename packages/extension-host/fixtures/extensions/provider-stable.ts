/**
 * Fixture: registers "stable_provider" with a streamSimple that yields
 * model "stable-marker". Used as the "unrelated" extension loaded after
 * a live register or unregister to verify the prior state survives.
 */
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { createAssistantMessageEventStream } from "@earendil-works/pi-ai";

function baseMessage(model: string) {
	return {
		role: "assistant" as const,
		content: [] as Array<{ type: "text"; text: string }>,
		api: "custom",
		provider: "stable_provider",
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

export default function providerStable(pi: ExtensionAPI): void {
	pi.registerProvider("stable_provider", {
		baseUrl: "https://stable.example",
		api: "custom",
		streamSimple: () => {
			const stream = createAssistantMessageEventStream();
			void (async () => {
				stream.push({ type: "start", partial: baseMessage("stable-marker") });
				stream.push({ type: "done", reason: "stop", message: baseMessage("stable-marker") });
				stream.end();
			})();
			return stream;
		},
	});
}
