import { afterAll, describe, expect, test } from "bun:test";
import { existsSync, mkdtempSync, mkdirSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import type {
	Api,
	AssistantMessageEvent,
	Context,
	Model,
	SimpleStreamOptions,
} from "../../.references/pi/packages/ai/src/index.ts";
import type { ExtensionAPI } from "../../.references/pi/packages/coding-agent/src/core/extensions/types.ts";
import verificationExtension, {
	DEFAULT_FINAL_MARKER,
	VERIFICATION_MODEL,
	VERIFICATION_PROVIDER,
} from "./extension.ts";
import { PTY_KEYS, spawnPty } from "./pty.ts";

interface RegisteredProvider {
	readonly models?: readonly { readonly id: string }[];
	readonly streamSimple?: (
		model: Model<Api>,
		context: Context,
		options?: SimpleStreamOptions,
	) => AsyncIterable<AssistantMessageEvent>;
}

const temporaryPaths: string[] = [];

afterAll(() => {
	for (const path of temporaryPaths) rmSync(path, { recursive: true, force: true });
});

function temporaryDirectory(prefix: string): string {
	const path = mkdtempSync(join(tmpdir(), prefix));
	temporaryPaths.push(path);
	return path;
}

function fixtureModel(): Model<Api> {
	return {
		id: VERIFICATION_MODEL,
		name: "Verification Model",
		api: "custom",
		provider: VERIFICATION_PROVIDER,
		baseUrl: "https://verification.invalid",
		reasoning: false,
		input: ["text"],
		cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
		contextWindow: 1_000_000,
		maxTokens: 100_000,
	};
}

async function collectEvents(provider: RegisteredProvider, context: Context): Promise<AssistantMessageEvent[]> {
	if (!provider.streamSimple) throw new Error("verification streamSimple was not registered");
	const events: AssistantMessageEvent[] = [];
	for await (const event of provider.streamSimple(fixtureModel(), context)) events.push(event);
	return events;
}

function registerFixture(): RegisteredProvider {
	let provider: RegisteredProvider | undefined;
	const api = {
		registerFlag() {},
		on() {},
		registerShortcut() {},
		registerCommand() {},
		registerProvider(name: string, config: RegisteredProvider) {
			expect(name).toBe(VERIFICATION_PROVIDER);
			provider = config;
		},
	} as ExtensionAPI;
	verificationExtension(api);
	if (!provider) throw new Error("verification provider was not registered");
	return provider;
}

function withEnvironment(values: Readonly<Record<string, string | undefined>>, run: () => Promise<void>): Promise<void> {
	const previous: Record<string, string | undefined> = {};
	for (const [name, value] of Object.entries(values)) {
		previous[name] = process.env[name];
		if (value === undefined) delete process.env[name];
		else process.env[name] = value;
	}
	return run().finally(() => {
		for (const [name, value] of Object.entries(previous)) {
			if (value === undefined) delete process.env[name];
			else process.env[name] = value;
		}
	});
}

describe("verification extension", () => {
	test("registers a deterministic model, stream, tool stages, compaction, and load generation", async () => {
		const directory = temporaryDirectory("pi-verification-extension-");
		const loadCountPath = join(directory, "state", "loads.txt");
		await withEnvironment(
			{
				PI_VERIFICATION_LOAD_COUNT_PATH: loadCountPath,
				PI_VERIFICATION_CHUNK_COUNT: "2",
				PI_VERIFICATION_FINAL_MARKER: "UNIT_FINAL",
			},
			async () => {
				const provider = registerFixture();
				registerFixture();
				expect(readFileSync(loadCountPath, "utf8")).toBe("2\n");
				expect(provider.models?.map((model) => model.id)).toEqual([VERIFICATION_MODEL]);

				const textEvents = await collectEvents(provider, {
					messages: [{ role: "user", content: "ordinary prompt", timestamp: 0 }],
				});
				expect(textEvents.map((event) => event.type)).toEqual([
					"start",
					"text_start",
					"text_delta",
					"text_delta",
					"text_delta",
					"text_end",
					"done",
				]);
				const done = textEvents.at(-1);
				expect(done?.type).toBe("done");
				if (done?.type === "done") expect(done.message.content).toEqual([
					{ type: "text", text: "verification-chunk-0001\nverification-chunk-0002\nUNIT_FINAL" },
				]);

				const toolResults: Context["messages"] = [];
				for (const expectedTool of ["read", "edit", "bash"] as const) {
					const events = await collectEvents(provider, {
						messages: [{ role: "user", content: "verification:tools", timestamp: 0 }, ...toolResults],
					});
					const toolEnd = events.find((event) => event.type === "toolcall_end");
					expect(toolEnd?.type === "toolcall_end" ? toolEnd.toolCall.name : undefined).toBe(expectedTool);
					toolResults.push({
						role: "toolResult",
						toolCallId: `verification-${expectedTool}`,
						toolName: expectedTool,
						content: [{ type: "text", text: "ok" }],
						isError: false,
						timestamp: 0,
					});
				}
				const finalToolEvents = await collectEvents(provider, {
					messages: [{ role: "user", content: "verification:tools", timestamp: 0 }, ...toolResults],
				});
				expect(finalToolEvents.some((event) => event.type === "text_delta" && event.delta.includes("UNIT_FINAL"))).toBe(true);

				process.env.PI_VERIFICATION_MODE = "compaction";
				const compaction = await collectEvents(provider, { messages: [] });
				expect(compaction.some((event) => event.type === "text_delta" && event.delta.includes("## Goal"))).toBe(true);
				delete process.env.PI_VERIFICATION_MODE;
			},
		);
	}, 15_000);
});

// Bun 1.3 terminal spawn is POSIX-only. Windows still runs every portable
// protocol/config/script test; PTY and interactive smoke coverage runs on
// Linux and macOS.
const isWindows = process.platform === "win32";
const bunExecutable = process.execPath;

describe.skipIf(isWindows)("PTY driver", () => {
	test("preserves hostile argv, timestamps chunks, and exits cleanly", async () => {
		const root = temporaryDirectory("pi pty ' $() ");
		const hostileArgument = "spaces 'single' \"double\" $(touch PWNED) `touch PWNED` $HOME;*";
		const process = spawnPty({
			argv: [
				bunExecutable,
				"-e",
				"console.log(`ARGV:${JSON.stringify(process.argv.slice(1))}`); for await (const line of console) { console.log(`APP:${line}`); break; }",
				hostileArgument,
			],
			cwd: root,
		});
		try {
			await process.waitFor((snapshot) => snapshot.rawText.includes(`ARGV:${JSON.stringify([hostileArgument])}`), {
				deadlineMs: 5_000,
				source: "raw",
			});
			expect(existsSync(join(root, "PWNED"))).toBe(false);
			process.writeKeys("ECHO_SENT", PTY_KEYS.enter);
			const snapshot = await process.waitFor(/APP:ECHO_SENT/, { deadlineMs: 5_000, source: "raw" });
			expect(snapshot.chunks.length).toBeGreaterThan(0);
			expect(snapshot.chunks.every((chunk) => chunk.elapsedMs >= 0 && chunk.unixMs > 0)).toBe(true);
			expect(await process.waitForExit(5_000)).toBe(0);
		} finally {
			await process.terminate();
		}
	}, 15_000);

	test("answers fragmented terminal capability queries", async () => {
		const expected = Buffer.from(
			"\x1b[?0u\x1b[?1;2c\x1b[6;16;8t\x1b]11;rgb:0000/0000/0000\x1b\\\x1b[?997;1n\x1b[1;1R",
		).toString("hex");
		const script = [
			"process.stdin.setRawMode?.(true); process.stdin.resume();",
			"let input = Buffer.alloc(0);",
			`const expected = "${expected}";`,
			"process.stdin.once('data', () => {",
			"process.stdin.on('data', (chunk) => {",
			"input = Buffer.concat([input, chunk]);",
			"if (input.toString('hex') === expected) {",
			"process.stdout.write(`RESPONSES:${expected}\\n`); process.exit(0);",
			"}",
			"});",
			"process.stdout.write('u\\x1b[c\\x1b[16t\\x1b]11;?\\x07\\x1b[?996n\\x1b[6n');",
			"});",
			"process.stdout.write('\\x1b[?');",
		].join("");
		const process = spawnPty({
			argv: [bunExecutable, "-e", script],
			cwd: temporaryDirectory("pi-verification-terminal-queries-"),
		});
		try {
			await process.waitFor(
				(snapshot) => snapshot.rawText.includes("\x1b[?"),
				{ deadlineMs: 5_000, source: "raw" },
			);
			process.writeKeys("x");
			const snapshot = await process.waitFor(/RESPONSES:/, {
				deadlineMs: 5_000,
				source: "raw",
			});
			expect(snapshot.rawText).toContain(`RESPONSES:${expected}`);
			expect(await process.waitForExit(5_000)).toBe(0);
		} finally {
			await process.terminate();
		}
	}, 15_000);

	test("terminates a running process", async () => {
		const process = spawnPty({
			argv: [bunExecutable, "-e", "console.log('READY'); setInterval(() => {}, 1_000);"],
			cwd: temporaryDirectory("pi-verification-terminate-"),
		});
		try {
			await process.waitFor(/READY/, { deadlineMs: 5_000, source: "raw" });
			await process.terminate();
			expect(process.exited).toBe(true);
		} finally {
			await process.terminate();
		}
	}, 15_000);

	test("delivers multi-chunk PTY input exactly once under backpressure", async () => {
		const chunkSize = 256 * 1024;
		const payloadSize = chunkSize * 8;
		const payload = Buffer.alloc(payloadSize);
		for (let i = 0; i < payloadSize; i++) payload[i] = 65 + (i % 26);
		const expectedHash = new Bun.CryptoHasher("sha256").update(payload).digest("hex");
		const script = [
			"process.stdin.setRawMode?.(true);",
			"process.stdout.write('READY\\n');",
			// Stay paused so the parent can fill the PTY buffer and exercise drain.
			"await Bun.sleep(400);",
			"process.stdin.resume();",
			"let input = Buffer.alloc(0);",
			"for await (const chunk of process.stdin) {",
			"input = Buffer.concat([input, chunk]);",
			"const end = input.indexOf(Buffer.from('END'));",
			"if (end >= 0) {",
			"const data = input.subarray(0, end);",
			"const hash = new Bun.CryptoHasher('sha256').update(data).digest('hex');",
			"process.stdout.write(`LEN:${data.length}\\nHASH:${hash}\\n`);",
			"process.exit(0);",
			"}",
			"}",
		].join("");
		const process = spawnPty({
			argv: [bunExecutable, "-e", script],
			cwd: temporaryDirectory("pi-verification-backpressure-"),
		});
		try {
			await process.waitFor(/READY/, { deadlineMs: 5_000, source: "raw" });
			for (let offset = 0; offset < payload.length; offset += chunkSize) {
				process.writeKeys(payload.subarray(offset, offset + chunkSize));
			}
			process.writeKeys("END");
			// Accepted input must not alias caller-owned storage after writeKeys returns.
			payload.fill(0);
			const snapshot = await process.waitFor(/HASH:/, { deadlineMs: 15_000, source: "raw" });
			expect(snapshot.rawText).toContain(`LEN:${payloadSize}`);
			expect(snapshot.rawText).toContain(`HASH:${expectedHash}`);
			expect(await process.waitForExit(5_000)).toBe(0);
		} finally {
			await process.terminate();
		}
	}, 30_000);

	test("separates terminal echo from application output", async () => {
		const process = spawnPty({
			argv: ["/bin/sh", "-c", "stty echo; printf APP_READY; IFS= read -r line; printf '\\nAPP:%s\\n' \"$line\""],
			cwd: temporaryDirectory("pi-verification-echo-"),
		});
		try {
			await process.waitFor(/APP_READY/, { deadlineMs: 5_000, source: "raw" });
			process.writeKeys("ECHO_SENT", PTY_KEYS.enter);
			const snapshot = await process.waitFor(/APP:ECHO_SENT/, { deadlineMs: 5_000 });
			expect(snapshot.echoText).toContain("ECHO_SENT");
			expect(snapshot.applicationText).toContain("APP:ECHO_SENT");
			expect(await process.waitForExit(5_000)).toBe(0);
		} finally {
			await process.terminate();
		}
	}, 15_000);

	test("terminates the complete PTY process group", async () => {
		const process = spawnPty({
			argv: ["/bin/sh", "-c", "sleep 300 & printf 'CHILD:%s\\n' \"$!\"; wait"],
			cwd: temporaryDirectory("pi-verification-tree-"),
		});
		const snapshot = await process.waitFor(/CHILD:(\d+)/, { deadlineMs: 5_000, source: "raw" });
		const match = /CHILD:(\d+)/.exec(snapshot.rawText);
		if (!match?.[1]) throw new Error("child pid was not reported");
		const childPid = Number(match[1]);
		await process.terminate();
		// The group kill settles the leader first; the orphaned grandchild is then
		// reaped asynchronously by init, so poll for its death instead of racing it.
		const deadline = Date.now() + 5_000;
		let childAlive = true;
		while (childAlive && Date.now() < deadline) {
			try {
				globalThis.process.kill(childPid, 0);
				await Bun.sleep(10);
			} catch {
				childAlive = false;
			}
		}
		expect(childAlive).toBe(false);
	}, 15_000);

	test("settles waitForExit when a background descendant holds the PTY slave", async () => {
		// Plain `sleep &; exit` is insufficient: session-leader exit SIGHUPs the
		// job, the slave closes, and Bun's terminal EOF arrives anyway. Ignore
		// SIGHUP and arm a marker before the shell exits so the slave stays open;
		// Promise.all-on-EOF then hangs until terminal.close() forces teardown.
		const process = spawnPty({
			argv: [
				"/bin/sh",
				"-c",
				"python3 -c 'import signal,time; signal.signal(signal.SIGHUP, signal.SIG_IGN); open(\"armed\",\"w\").write(\"1\"); print(\"READY\", flush=True); time.sleep(300)' & " +
					"while [ ! -f armed ]; do sleep 0.01; done; exit 42",
			],
			cwd: temporaryDirectory("pi-verification-orphan-slave-"),
		});
		try {
			await process.waitFor(/READY/, { deadlineMs: 5_000, source: "raw" });
			expect(await process.waitForExit(5_000)).toBe(42);
		} finally {
			await process.terminate();
		}
	}, 15_000);
});

interface CliFixture {
	readonly name: string;
	readonly argvPrefix: readonly [string, ...string[]];
}

async function smokeCli(fixture: CliFixture, sharedDirectory: string): Promise<void> {
	const extensionPath = resolve(import.meta.dirname, "extension.ts");
	const hostPath = resolve("packages/extension-host/dist/pi-extension-host");
	const agentDirectory = join(sharedDirectory, "agent");
	const sessionDirectory = join(sharedDirectory, "sessions");
	mkdirSync(agentDirectory, { recursive: true });
	mkdirSync(sessionDirectory, { recursive: true });
	const cli = spawnPty({
		argv: [
			...fixture.argvPrefix,
			"--provider",
			VERIFICATION_PROVIDER,
			"--model",
			VERIFICATION_MODEL,
			"--api-key",
			"verification-key",
			"--extension",
			extensionPath,
			"--no-session",
			"--offline",
			"--no-context-files",
			"--no-skills",
			"--no-themes",
			"--approve",
		],
		cwd: sharedDirectory,
		env: {
			HOME: join(sharedDirectory, "home"),
			PI_CODING_AGENT_DIR: agentDirectory,
			PI_CODING_AGENT_SESSION_DIR: sessionDirectory,
			PI_EXTENSION_HOST: hostPath,
			PI_OFFLINE: "1",
			PI_VERIFICATION_MODE: "text",
			PI_VERIFICATION_CHUNK_COUNT: "1",
			PI_VERIFICATION_CHUNK_DELAY_MS: "0",
			PI_VERIFICATION_FINAL_MARKER: DEFAULT_FINAL_MARKER,
			PI_VERIFICATION_LOAD_COUNT_PATH: join(sharedDirectory, `${fixture.name}-loads.txt`),
		},
	});
	try {
		await cli.waitFor(/0\.0%\/1\.0M \(auto\)/, {
			deadlineMs: 30_000,
			source: "application",
		});
		cli.writeKeys(`foundation prompt for ${fixture.name}`, PTY_KEYS.enter);
		const response = await cli.waitFor(new RegExp(DEFAULT_FINAL_MARKER), {
			deadlineMs: 30_000,
			source: "application",
		});
		expect(response.echoText).not.toContain(DEFAULT_FINAL_MARKER);
		cli.writeKeys("/quit", PTY_KEYS.enter);
		expect(await cli.waitForExit(10_000)).toBe(0);
	} catch (error) {
		const tail = cli.snapshot().rawText.slice(-4_000);
		throw new Error(`${fixture.name} smoke failed: ${error instanceof Error ? error.message : String(error)}\nPTY tail:\n${tail}`);
	} finally {
		await cli.terminate();
	}
}

describe.skipIf(isWindows)("shared interactive provider smoke", () => {
	test("drives Rust and TypeScript CLIs with one extension and model", async () => {
		const rustBinary = resolve("target/debug/pi");
		const hostBinary = resolve("packages/extension-host/dist/pi-extension-host");
		expect(existsSync(rustBinary), `missing ${rustBinary}; run cargo build -p pi`).toBe(true);
		expect(existsSync(hostBinary), `missing ${hostBinary}; run bun run build:extension-host`).toBe(true);
		const bun = Bun.which("bun");
		if (!bun) throw new Error("bun executable not found");
		const sharedDirectory = temporaryDirectory("pi-verification-shared-");
		mkdirSync(join(sharedDirectory, "home"), { recursive: true });
		const fixtures: readonly CliFixture[] = [
			{
				name: "typescript",
				argvPrefix: [bun, resolve(".references/pi/packages/coding-agent/src/cli.ts")],
			},
			{ name: "rust", argvPrefix: [rustBinary] },
		];
		for (const fixture of fixtures) await smokeCli(fixture, sharedDirectory);
	}, 90_000);
});
