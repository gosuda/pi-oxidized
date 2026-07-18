/**
 * Public API for the pi extension host package.
 *
 * The host drives the REAL reference `ExtensionRunner` over a structured JSONL
 * bridge. See `host.ts` for the bridge, `sanitize.ts` for the ANSI sanitizer,
 * and `protocol.ts` for the protocol mirror.
 */

export {
	ExtensionHost,
	EXTENSION_HOOK_TIMEOUT_MS,
	EXTENSION_INPUT_TIMEOUT_MS,
	EXTENSION_INPUT_QUEUE_CAPACITY,
} from "./host.ts";
export type {
	TerminalInputHandler,
	TerminalInputHandlerResult,
} from "./host.ts";
export { parseAnsiLine, parseAnsiLines, MAX_HYPERLINK_ID_BYTES, MAX_HYPERLINK_URI_BYTES } from "./sanitize.ts";
export { COMPATIBILITY_VERSION } from "./version.ts";
export { getExtensionAliases, createExtensionJiti } from "./virtual-modules.ts";
