/**
 * Bridge-local declaration for the verification extension's runtime import.
 *
 * tsc resolves `./runtime` to this declaration (no reference source pulled
 * in); Bun resolves the real `runtime.ts` which re-exports from the
 * reference event-stream module.
 */
declare module "./runtime" {
	import type { AssistantMessageEventStream } from "@earendil-works/pi-ai";
	export function createAssistantMessageEventStream(): AssistantMessageEventStream;
}
