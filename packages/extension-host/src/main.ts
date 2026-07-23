/**
 * Extension host entrypoint. Reads JSONL protocol frames from stdin, writes
 * structured frames to stdout (protocol only), and stderr for logs.
 *
 * Usage:  pi-extension-host [--cwd <dir>] [--extension <path>]... [--lean] [--no-builtins]
 *
 * The Rust binary spawns this process and drives it via the JSONL protocol.
 *
 * Mode selection (frozen contract): this module statically imports NOTHING
 * from the host graphs. It parses `--lean` with the tiny local parser below,
 * then dynamically imports exactly one implementation:
 * - default (Mode 1, compat): `./host.ts` plus upstream builtins — byte-for-
 *   byte the historical behavior, CLI included;
 * - `--lean` (Mode 2): `./lean-runner.ts`, which never evaluates host.ts,
 *   builtins, virtual-modules, or any upstream coding-agent module.
 */

interface CliArgs {
	cwd: string;
	extensionPaths: string[];
	lean: boolean;
	builtins: boolean;
}

/** Tiny local CLI parse; MUST stay dependency-free so mode selection is hermetic. */
function parseArgs(argv: string[]): CliArgs {
	let cwd = process.cwd();
	let lean = false;
	let builtins = true;
	const extensionPaths: string[] = [];
	for (let i = 2; i < argv.length; i++) {
		const arg = argv[i];
		if (arg === undefined) continue;
		if (arg === "--lean") {
			lean = true;
		} else if (arg === "--no-builtins") {
			builtins = false;
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
	return { cwd, extensionPaths, lean, builtins };
}

/** Wrap process.stdout as a ByteWritable (protocol frames only). */
class StdoutSink {
	write(chunk: Uint8Array): void {
		process.stdout.write(chunk as Buffer);
	}
}

async function main(): Promise<void> {
	const { cwd, extensionPaths, lean, builtins } = parseArgs(process.argv);
	if (lean) {
		// Dynamic import is required: mode selection happens at runtime and the
		// lean graph must stay the only graph evaluated in this mode.
		const { LeanRunner } = await import("./lean-runner.ts");
		const runner = new LeanRunner(process.stdin, new StdoutSink());
		try {
			await runner.run({ cwd, extensionPaths });
		} finally {
			runner.dispose();
		}
		return;
	}
	// Dynamic import is required: the compat graph (host + upstream builtins)
	// must not be evaluated until AFTER mode selection rejects --lean.
	const { ExtensionHost } = await import("./host.ts");
	// The builtins module is imported ONLY when enabled: with --no-builtins it
	// is never evaluated and no inline factories reach the host.
	const factories = builtins
		? (await import("@earendil-works/pi-coding-agent/builtins")).builtInExtensions
		: [];
	const host = new ExtensionHost(process.stdin, new StdoutSink());
	await host.run({ cwd, extensionPaths, factories });
}

main().catch((err) => {
	console.error("[host] uncaught:", err);
	process.exit(1);
});
