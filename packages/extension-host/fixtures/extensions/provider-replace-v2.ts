/**
 * Provider replacement fixture v2: registers the SAME provider id
 * "replace_provider" but with baseUrl "https://v2.example" and a
 * streamSimple that yields a start event with model "v2-marker".
 * Used to prove that replacing an extension rebuilds provider
 * registrations from the current set — the stale v1 capture must
 * not survive.
 */
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { createAssistantMessageEventStream } from "@earendil-works/pi-ai";

function baseMessage(model: string) {
	return {
		role: "assistant" as const,
		content: [] as Array<{ type: "text"; text: string }>,
		api: "custom",
		provider: "replace_provider",
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

export default function providerReplaceV2(pi: ExtensionAPI): void {
	pi.registerProvider("replace_provider", {
		baseUrl: "https://v2.example",
		api: "custom",
		streamSimple: () => {
			const stream = createAssistantMessageEventStream();
			void (async () => {
				stream.push({ type: "start", partial: baseMessage("v2-marker") });
				stream.push({ type: "done", reason: "stop", message: baseMessage("v2-marker") });
				stream.end();
			})();
			return stream;
		},
	});
}
