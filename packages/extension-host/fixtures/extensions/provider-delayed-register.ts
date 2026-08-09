/**
 * Factory that schedules a delayed registration as an async continuation of
 * the load scope, then returns immediately.  The continuation inherits the
 * ProviderLoadScope via AsyncLocalStorage; when released by the test it
 * calls registerProvider while the scope is "committed" and still active,
 * proving that delayed descendants of a successful load apply live.
 *
 * No timers or sleeps — coordination is via a global callback promise,
 * following the same convention as provider-slow-load.ts.
 */
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { createAssistantMessageEventStream } from "@earendil-works/pi-ai";

type ProviderLoadCoordination = typeof globalThis & {
	__providerDelayedRegisterRelease?: () => void;
};

function baseMessage(model: string) {
	return {
		role: "assistant" as const,
		content: [] as Array<{ type: "text"; text: string }>,
		api: "custom",
		provider: "delayed_provider",
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

export default function providerDelayedRegister(pi: ExtensionAPI): void {
	const coordination = globalThis as ProviderLoadCoordination;

	// Start an async continuation that inherits the current providerLoadScope.
	// It waits for the test to release it, then registers while the scope is
	// already "committed" (the factory has returned and the scope was activated).
	void (async () => {
		await new Promise<void>((resolve) => {
			coordination.__providerDelayedRegisterRelease = resolve;
		});
		pi.registerProvider("delayed_provider", {
			baseUrl: "https://delayed.example",
			api: "custom",
			streamSimple: () => {
				const stream = createAssistantMessageEventStream();
				void (async () => {
					stream.push({ type: "start", partial: baseMessage("delayed-marker") });
					stream.push({ type: "done", reason: "stop", message: baseMessage("delayed-marker") });
					stream.end();
				})();
				return stream;
			},
		});
	})();

	// Return immediately — load succeeds, scope transitions to "committed".
	// The async continuation above is still pending and will apply live later.
}
