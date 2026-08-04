/**
 * Runtime shim for createAssistantMessageEventStream.
 *
 * tsc sees only the bridge declaration in `refs.d.ts` (no reference source
 * pulled in); Bun loads the real implementation via dynamic require at
 * runtime, avoiding the reference source's pre-existing type errors.
 */
import type { AssistantMessageEventStream } from "@earendil-works/pi-ai";

export function createAssistantMessageEventStream(): AssistantMessageEventStream {
	const mod = require("../../.references/pi/packages/ai/src/utils/event-stream.ts");
	return mod.createAssistantMessageEventStream();
}
