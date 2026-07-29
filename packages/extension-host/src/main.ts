/**
 * Extension host entrypoint. Reads JSONL protocol frames from stdin, writes
 * structured frames to stdout (protocol only), and stderr for logs.
 *
 * Usage:  pi-extension-host [--cwd <dir>] [--extension <path>]... [--no-builtins]
 *
 * The Rust binary spawns this process and drives it via the JSONL protocol.
 */

import { ExtensionHost } from "./host.ts";

export interface HostArgs {
	cwd: string;
	extensionPaths: string[];
	noBuiltins: boolean;
}

export function parseArgs(argv: string[]): HostArgs {
	let cwd = process.cwd();
	const extensionPaths: string[] = [];
	let noBuiltins = false;
	for (let i = 2; i < argv.length; i++) {
		const arg = argv[i];
		if (arg === undefined) continue;
		if (arg === "--no-builtins") {
			noBuiltins = true;
		} else if (arg === "--cwd" || arg === "-C") {
			const next = argv[i + 1];
			if (next !== undefined) {
				cwd = next;
				i += 1;
			}
		} else if (arg === "--extension" || arg === "-e") {
			const next = argv[i + 1];
			if (next !== undefined) {
				extensionPaths.push(next);
				i += 1;
			}
		}
	}
	return { cwd, extensionPaths, noBuiltins };
}

export async function loadRunOptions(argv: string[]) {
	const { cwd, extensionPaths, noBuiltins } = parseArgs(argv);
	const factories = noBuiltins
		? []
		: (await import("@earendil-works/pi-coding-agent/builtins")).builtInExtensions;
	return { cwd, extensionPaths, factories };
}

/** Wrap process.stdout as a ByteWritable (protocol frames only). */
class StdoutSink {
	write(chunk: Uint8Array): void {
		process.stdout.write(chunk as Buffer);
	}
}

async function main(): Promise<void> {
	const { cwd, extensionPaths, factories } = await loadRunOptions(process.argv);
	const host = new ExtensionHost(process.stdin, new StdoutSink());
	await host.run({ cwd, extensionPaths, factories });
}

if (import.meta.main) {
	main().catch((err) => {
		console.error("[host] uncaught:", err);
		process.exit(1);
	});
}
