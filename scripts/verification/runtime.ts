/**
 * Runtime loader for the reference createAssistantMessageEventStream.
 *
 * The implementation lives in the optional `.references/pi` checkout and is
 * loaded dynamically so the reference source's pre-existing type errors never
 * reach this program. Every prerequisite at that boundary is validated before
 * the factory is called: the resolved reference path must exist, the module
 * must load, and the `createAssistantMessageEventStream` export must be a
 * function. A missing or misshapen prerequisite fails here with a clear
 * operator-facing message instead of surfacing as an opaque stack trace later
 * inside the provider stream.
 */
import { existsSync } from "node:fs";
import { resolve } from "node:path";
import type { AssistantMessageEventStream } from "@earendil-works/pi-ai";

const REFERENCE_MODULE = "../../.references/pi/packages/ai/src/utils/event-stream.ts";

type CreateAssistantMessageEventStream = () => AssistantMessageEventStream;

/** Opaque shape of the dynamically loaded reference event-stream module. */
interface ReferenceEventStreamModule {
	readonly createAssistantMessageEventStream?: CreateAssistantMessageEventStream;
}

export function createAssistantMessageEventStream(): AssistantMessageEventStream {
	const modulePath = resolve(import.meta.dirname, REFERENCE_MODULE);

	if (!existsSync(modulePath)) {
		throw new Error(
			`[verification] reference prerequisite missing: ${modulePath}. ` +
				"Restore the .references/pi checkout before running verification.",
		);
	}

	let raw: unknown;
	try {
		raw = require(modulePath);
	} catch (cause) {
		throw new Error(
			`[verification] reference prerequisite failed to load: ${modulePath} (${(cause as Error).message}). ` +
				"Restore or rebuild .references/pi before running verification.",
			{ cause },
		);
	}

	if (raw === null || typeof raw !== "object") {
		throw new Error(
			`[verification] reference prerequisite ${modulePath} did not export a module object. ` +
				"Ensure .references/pi is current.",
		);
	}

	const eventStreamModule = raw as ReferenceEventStreamModule;
	if (typeof eventStreamModule.createAssistantMessageEventStream !== "function") {
		throw new Error(
			`[verification] reference prerequisite ${modulePath} did not export ` +
				"createAssistantMessageEventStream as a function. Ensure .references/pi is current.",
		);
	}

	return eventStreamModule.createAssistantMessageEventStream();
}
