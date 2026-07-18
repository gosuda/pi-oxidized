/**
 * Runtime-import fixture: a standalone executable that dynamically imports an
 * external `.ts` extension by absolute file URL, resolves the virtual pi
 * modules, and executes its default factory.
 *
 * This verifies the host can load arbitrary TypeScript extensions at runtime
 * without a build step. Run on every release target:
 *
 *   bun run fixtures/runtime-import.ts /absolute/path/to/extension.ts
 */

import { createExtensionJiti } from "../src/virtual-modules.ts";
import {
	createExtensionRuntime,
	loadExtensionFromFactory,
} from "@earendil-works/pi-coding-agent";
import type { ExtensionFactory } from "@earendil-works/pi-coding-agent";
import { createEventBus } from "../src/host.ts";

async function main(): Promise<void> {
	const extPath = process.argv[2];
	if (extPath === undefined || extPath.length === 0) {
		console.error("usage: runtime-import.ts <absolute-extension-path>");
		process.exit(2);
	}
	const cwd = process.cwd();
	const jiti = createExtensionJiti();
	const module = await jiti.import(extPath, { default: true }) as unknown;
	if (typeof module !== "function") {
		console.error(`Extension does not export a valid factory function: ${extPath}`);
		process.exit(3);
	}
	const factory = module as ExtensionFactory;
	const runtime = createExtensionRuntime();
	const bus = createEventBus();
	const ext = await loadExtensionFromFactory(factory, cwd, bus, runtime, extPath);
	const tools = [...ext.tools.keys()];
	const handlers = [...ext.handlers.keys()];
	const commands = [...ext.commands.keys()];
	process.stdout.write(JSON.stringify({
		path: extPath,
		tools,
		handlers,
		commands,
		flags: [...ext.flags.keys()],
		shortcuts: [...ext.shortcuts.keys()],
		messageRenderers: [...ext.messageRenderers.keys()],
	}) + "\n");
}

main().catch((err) => {
	console.error("runtime-import failed:", err);
	process.exit(1);
});
