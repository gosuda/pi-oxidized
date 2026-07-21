import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { mkdtemp, mkdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import {
	COMPATIBILITY_VERSION,
	PROTOCOL_VERSION,
	encodeFrameString,
	type Frame,
} from "@earendil-works/pi-tui-protocol";
import { isCompiledModuleUrl } from "../src/virtual-modules.ts";


/** Every stage of the compiled lean flow, captured in request order. */
type LeanStages = {
	hello: Record<string, unknown>;
	load: Record<string, unknown>;
	prepare: Record<string, unknown>;
	validate: Record<string, unknown>;
	toolUpdate: Record<string, unknown>;
	execute: Record<string, unknown>;
	command: Record<string, unknown>;
};

/**
 * Discriminates argv construction and the hello handshake for the one
 * compiled binary; a boolean flag would hide which mode a call site drives.
 */
type HostMode =
	| { mode: "compat"; env?: Record<string, string> }
	| { mode: "lean"; compatibilityVersion: string; env?: Record<string, string> };

async function loadExtension(
	hostPath: string,
	cwd: string,
	extensionPath: string,
	options: { mode: "compat"; env?: Record<string, string> },
): Promise<Record<string, unknown>>;
async function loadExtension(
	hostPath: string,
	cwd: string,
	extensionPath: string,
	options: { mode: "lean"; compatibilityVersion: string; env?: Record<string, string> },
): Promise<LeanStages>;
async function loadExtension(
	hostPath: string,
	cwd: string,
	extensionPath: string,
	options: HostMode,
): Promise<Record<string, unknown>> {
	const { promise, resolve: resolvePromise, reject: rejectPromise } =
		Promise.withResolvers<Record<string, unknown>>();
	const argv = options.mode === "lean" ? ["--lean", "--cwd", cwd] : ["--cwd", cwd];
	const child = spawn(hostPath, argv, {
		cwd,
		stdio: ["pipe", "pipe", "pipe"],
		env: { ...process.env, ...options.env },
	});
	const closePromise = new Promise<void>((resolveClose) => {
		child.once("close", () => resolveClose());
	});
	let stdout = "";
	let stderr = "";
	let settled = false;
	let resultError: Error | undefined;
	let resultPayload: Record<string, unknown> | undefined;
	// A compiled child can lock both its executable and cwd on Windows. Keep
	// the integration deadline live until `close`, the deletion-safe boundary.
	const timeout = setTimeout(() => {
		resultError ??= new Error(
			settled
				? `compiled host (${options.mode}) did not exit after the final response`
				: `compiled host (${options.mode}) did not answer`,
		);
		settled = true;
		if (child.exitCode === null) child.kill("SIGKILL");
	}, 30_000);
	void closePromise.then(() => {
		clearTimeout(timeout);
		if (resultError) rejectPromise(resultError);
		else resolvePromise(resultPayload ?? {});
	});
	function finish(error?: Error, payload?: Record<string, unknown>): void {
		if (settled) return;
		settled = true;
		resultError = error;
		resultPayload = payload;
		child.stdin.end();
		if (child.exitCode === null) child.kill("SIGTERM");
	}
	const stages: Partial<LeanStages> = {};
	const send = (frame: Frame): void => {
		child.stdin.write(encodeFrameString(frame));
	};
	child.stdout.on("data", (chunk: Buffer) => {
		stdout += chunk.toString();
		for (;;) {
			const newline = stdout.indexOf("\n");
			if (newline < 0) return;
			const line = stdout.slice(0, newline);
			stdout = stdout.slice(newline + 1);
			let frame: Frame;
			try {
				frame = JSON.parse(line) as Frame;
			} catch (error) {
				finish(error instanceof Error ? error : new Error(String(error)));
				return;
			}
			if (frame.kind === "event") {
				if (frame.method === "toolUpdate") {
					stages.toolUpdate = frame.payload as Record<string, unknown>;
				}
				continue;
			}
			// Error frames must fail fast: waiting on a response id that will
			// never come would turn a defect into a 30s timeout.
			if (frame.kind !== "res") {
				finish(new Error(
					`compiled host answered ${String(frame.method)} with kind=${frame.kind}: ${JSON.stringify(frame.payload)}`,
				));
				return;
			}
			const payload = frame.payload as Record<string, unknown>;
			switch (frame.id) {
				case 1: {
					stages.hello = payload;
					send({
						id: 2,
						kind: "req",
						method: "extensions.load",
						payload: { extensionPaths: [extensionPath], cwd, projectTrusted: true },
					});
					break;
				}
				case 2: {
					if (options.mode === "compat") {
						finish(undefined, payload);
						return;
					}
					stages.load = payload;
					// Each tool stage consumes the previous stage's output, so the
					// dataflow itself proves prepare → validate → execute order.
					send({ id: 3, kind: "req", method: "tool.prepare", payload: { name: "echo", args: { text: "hi" } } });
					break;
				}
				case 3: {
					stages.prepare = payload;
					send({ id: 4, kind: "req", method: "tool.validate", payload: { name: "echo", args: payload["args"] } });
					break;
				}
				case 4: {
					stages.validate = payload;
					send({
						id: 5,
						kind: "req",
						method: "tool.execute",
						payload: { name: "echo", toolCallId: "compiled-lean-1", args: payload["args"], prepared: true },
					});
					break;
				}
				case 5: {
					stages.execute = payload;
					send({ id: 6, kind: "req", method: "command.execute", payload: { command: "greet", args: "from-compiled-lean" } });
					break;
				}
				case 6: {
					stages.command = payload;
					finish(undefined, stages as LeanStages);
					return;
				}
			}
		}
	});
	child.stderr.on("data", (chunk: Buffer) => { stderr += chunk.toString(); });
	child.once("error", (error) => finish(error));
	child.once("exit", (code) => {
		if (!settled) {
			finish(new Error(`compiled host exited early with code ${String(code)}: ${stderr || stdout}`));
		}
	});
	send({
		id: 1,
		kind: "req",
		method: "hello",
		payload: {
			protocolVersion: PROTOCOL_VERSION,
			// Lean validates protocolVersion only while the compat host requires
			// the real compatibilityVersion, so the mode picks the handshake value.
			compatibilityVersion: options.mode === "lean"
				? options.compatibilityVersion
				: COMPATIBILITY_VERSION,
		},
	});
	return await promise;
}

/**
 * Release contract: the compiled host bundles the reference packages (jiti
 * `virtualModules`, mirroring the upstream loader), so a bare binary in an
 * empty archive dir, run from a cwd OUTSIDE the repository, must load an
 * extension importing `@earendil-works/pi-coding-agent` and `typebox`.
 */
test("detects every Bun compiled-filesystem URL form", () => {
	expect(isCompiledModuleUrl("file:///$bunfs/root/src/main.ts")).toBe(true);
	expect(isCompiledModuleUrl("file:///B:/~BUN/root/src/main.ts")).toBe(true);
	expect(isCompiledModuleUrl("file:///B:/%7EBUN/root/src/main.ts")).toBe(true);
	expect(isCompiledModuleUrl("file:///C:/repo/~BUN-copy/main.ts")).toBe(false);
	expect(isCompiledModuleUrl("file:///C:/repo/main.ts?cache=$bunfs")).toBe(false);
	expect(isCompiledModuleUrl("file:///C:/repo/packages/extension-host/src/main.ts")).toBe(false);
});

describe("compiled extension module bundling", () => {
	const hostDir = resolve(import.meta.dirname, "..");
	const executableSuffix = process.platform === "win32" ? ".exe" : "";
	let archiveRoot: string;
	let outsideCwd: string;
	let hostPath: string;

	beforeAll(async () => {
		archiveRoot = await mkdtemp(join(tmpdir(), "pi-extension-bundled-"));
		outsideCwd = await mkdtemp(join(tmpdir(), "pi-extension-outside-"));
		hostPath = join(archiveRoot, `pi-extension-host${executableSuffix}`);
		const build = await Bun.build({
			entrypoints: [resolve(hostDir, "src", "main.ts")],
			compile: { outfile: hostPath },
			plugins: [{
				name: "compat-graph-evaluation-probe",
				setup(builder) {
					builder.onLoad({ filter: /[\\/]host\.ts$/ }, async ({ path }) => {
						const source = await Bun.file(path).text();
						return {
							loader: "ts",
							contents: `
import { writeFileSync as __writeCompatGraphProbe } from "node:fs";
const __compatGraphProbe = process.env["PI_EXTENSION_HOST_PROBE"];
if (__compatGraphProbe) {
	__writeCompatGraphProbe(__compatGraphProbe, "host.ts:evaluated\\n", { flag: "wx" });
}
${source}`,
						};
					});
				},
			}],
		});
		if (!build.success) {
			throw new AggregateError(build.logs, "failed to compile instrumented extension host");
		}

		const extensionPath = join(archiveRoot, "extensions", "bundled.ts");
		await mkdir(resolve(extensionPath, ".."), { recursive: true });
		await writeFile(extensionPath, `
import { defineTool } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";

const tool = defineTool({
	name: "bundled",
	description: "Confirms bundled virtual modules",
	parameters: Type.Object({}),
	execute: async () => ({ content: [{ type: "text", text: "ok" }] }),
});

export default (pi) => pi.registerTool(tool);
`);
	});

	afterAll(async () => {
		await Promise.all([
			rm(archiveRoot, { force: true, recursive: true }),
			rm(outsideCwd, { force: true, recursive: true }),
		]);
	});

	test("loads a TypeScript extension without any repository tree at cwd", async () => {
		const result = await loadExtension(
			hostPath,
			outsideCwd,
			join(archiveRoot, "extensions", "bundled.ts"),
			{ mode: "compat" },
		);
		expect(result["errors"]).toEqual([]);
		expect(result["extensions"]).toBe(1);
		expect(result["tools"]).toEqual(expect.arrayContaining([
			expect.objectContaining({ name: "bundled" }),
		]));
	});

	test("serves the Mode 2 lean flow from an outside cwd with the same binary", async () => {
		const echoEntry = resolve(import.meta.dirname, "fixtures", "lean", "echo.mjs");
		const stages = await loadExtension(hostPath, outsideCwd, echoEntry, {
			mode: "lean",
			// Foreign on purpose: a compat host would terminate on this value,
			// so the helloAck proves the lean graph checked protocolVersion only.
			compatibilityVersion: "0.0.0-not-the-compat-version",
		});
		expect(stages.hello["protocolVersion"]).toBe(PROTOCOL_VERSION);
		expect(stages.load["errors"]).toEqual([]);
		expect(stages.load["extensions"]).toBe(1);
		expect(stages.load["tools"]).toEqual(expect.arrayContaining([
			expect.objectContaining({ name: "echo" }),
			expect.objectContaining({ name: "slow" }),
		]));
		expect(stages.prepare["args"]).toEqual({ text: "hi", preparedBy: "lean" });
		expect(stages.validate["args"]).toEqual({ text: "hi", preparedBy: "lean" });
		expect(stages.toolUpdate).toMatchObject({
			toolCallId: "compiled-lean-1",
			toolName: "echo",
			partialResult: { content: [{ type: "text", text: "echoing…" }] },
		});
		expect(stages.execute["content"]).toEqual([{ type: "text", text: "echo:hi" }]);
		expect(stages.execute["details"]).toMatchObject({
			preparedBy: "lean",
			extensionPath: echoEntry,
		});
		expect(stages.command["ok"]).toBe(true);
	});

	test("compat evaluates host.ts but --lean never does (graph-absence probe)", async () => {
		const compatMarker = join(archiveRoot, "compat-graph.marker");
		const leanMarker = join(archiveRoot, "lean-graph.marker");

		// Positive control: the test-build plugin instruments host.ts, so compat
		// creates the marker when that module is evaluated.
		const compat = await loadExtension(
			hostPath,
			outsideCwd,
			join(archiveRoot, "extensions", "bundled.ts"),
			{ mode: "compat", env: { PI_EXTENSION_HOST_PROBE: compatMarker } },
		);
		expect(compat["errors"]).toEqual([]);
		expect(existsSync(compatMarker)).toBe(true);

		// Negative assertion: --lean must never evaluate host.ts, so its probe
		// (a SEPARATE path) stays unwritten. loadExtension resolves on the
		// child's `close`, so this check runs only after the child is fully
		// reaped — the deletion-safe boundary that also holds on Windows, where
		// a live compiled child locks its own executable and cwd.
		const echoEntry = resolve(import.meta.dirname, "fixtures", "lean", "echo.mjs");
		const lean = await loadExtension(hostPath, outsideCwd, echoEntry, {
			mode: "lean",
			compatibilityVersion: "0.0.0-not-the-compat-version",
			env: { PI_EXTENSION_HOST_PROBE: leanMarker },
		});
		expect(lean.load["errors"]).toEqual([]);
		expect(existsSync(leanMarker)).toBe(false);
	});
});
