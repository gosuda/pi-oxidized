/**
 * Custom provider fixture: ordered stream events, optional error and cancel.
 *
 * Wire contract exercised by host `provider.stream`:
 * - emits AssistantMessageEvent payloads as stream-correlated providerEvent frames
 * - throws when model.id === "error"
 * - waits for AbortSignal when model.id === "cancel"
 */
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { createAssistantMessageEventStream } from "@earendil-works/pi-ai";

function baseMessage(text: string) {
	return {
		role: "assistant" as const,
		content: text.length > 0 ? [{ type: "text" as const, text }] : [],
		api: "custom",
		provider: "fixture_provider",
		model: "fixture-1",
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

function modelIdOf(model: unknown): string {
	if (model !== null && typeof model === "object" && "id" in model) {
		const id = model.id;
		return typeof id === "string" ? id : String(id ?? "");
	}
	return "";
}

function signalOf(options: unknown): AbortSignal | undefined {
	if (options !== null && typeof options === "object" && "signal" in options) {
		const signal = options.signal;
		return signal instanceof AbortSignal ? signal : undefined;
	}
	return undefined;
}

export default function providerStreamExtension(pi: ExtensionAPI): void {
	pi.registerProvider("fixture_provider", {
		baseUrl: "https://fixture.example",
		api: "custom",
		streamSimple: (model, _context, options) => {
			const stream = createAssistantMessageEventStream();
			const modelId = modelIdOf(model);
			const signal = signalOf(options);

			void (async () => {
				try {
					if (modelId === "error") {
						throw new Error("provider-stream-error");
					}

					const partial0 = baseMessage("");
					stream.push({ type: "start", partial: partial0 });

					const partial1 = baseMessage("hel");
					stream.push({
						type: "text_delta",
						contentIndex: 0,
						delta: "hel",
						partial: partial1,
					});

					if (modelId === "cancel") {
						await new Promise<void>((_resolve, reject) => {
							if (signal?.aborted) {
								reject(new Error("aborted"));
								return;
							}
							const onAbort = () => {
								reject(new Error("aborted"));
							};
							signal?.addEventListener("abort", onAbort, { once: true });
						});
						return;
					}

					const partial2 = baseMessage("hello");
					stream.push({
						type: "text_delta",
						contentIndex: 0,
						delta: "lo",
						partial: partial2,
					});
					stream.push({
						type: "done",
						reason: "stop",
						message: baseMessage("hello"),
					});
				} catch (err) {
					const message = err instanceof Error ? err.message : String(err);
					const aborted = message === "aborted" || signal?.aborted === true;
					if (!aborted) {
						stream.push({
							type: "error",
							reason: "error",
							error: {
								...baseMessage(""),
								stopReason: "error",
								errorMessage: message,
							},
						});
					}
					stream.end();
				}
			})();

			return stream;
		},
	});
}
