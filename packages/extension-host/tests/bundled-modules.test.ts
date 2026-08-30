import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { spawn } from "node:child_process";
import { mkdtemp, mkdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import {
	COMPATIBILITY_VERSION,
	PROTOCOL_VERSION,
	encodeFrameString,
	type Frame,
} from "@earendil-works/pi-tui-protocol";

async function run(command: string, args: readonly string[], cwd: string): Promise<void> {
	const { promise, resolve: resolvePromise, reject: rejectPromise } = Promise.withResolvers<void>();
	const child = spawn(command, args, { cwd, stdio: ["ignore", "ignore", "pipe"] });
	let stderr = "";
	child.stderr.on("data", (chunk: Buffer) => { stderr += chunk.toString(); });
	child.once("error", rejectPromise);
	child.once("exit", (code) => {
		if (code === 0) resolvePromise();
		else rejectPromise(new Error(`${command} exited with code ${String(code)}: ${stderr}`));
	});
	return await promise;
}

async function loadExtension(
	hostPath: string,
	cwd: string,
	extensionPath: string,
): Promise<Record<string, unknown>> {
	const { promise, resolve: resolvePromise, reject: rejectPromise } =
		Promise.withResolvers<Record<string, unknown>>();
	const child = spawn(hostPath, ["--cwd", cwd], {
		cwd,
		stdio: ["pipe", "pipe", "pipe"],
	});
	let stdout = "";
	let stderr = "";
	let settled = false;
	// A real compiled process has no fake clock; this bounds a stuck child process.
	const timeout = setTimeout(
		() => finish(new Error("compiled host did not answer extensions.load")),
		30_000,
	);
	function finish(error?: Error, payload?: Record<string, unknown>): void {
		if (settled) return;
		settled = true;
		clearTimeout(timeout);
		child.stdin.end();
		child.kill("SIGTERM");
		if (error) rejectPromise(error);
		else resolvePromise(payload ?? {});
	}
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
			if (frame.id === 1 && frame.kind === "res") {
				child.stdin.write(encodeFrameString({
					id: 2,
					kind: "req",
					method: "extensions.load",
					payload: { extensionPaths: [extensionPath], cwd, projectTrusted: true },
				}));
			}
			if (frame.id === 2 && frame.kind === "res") {
				finish(undefined, frame.payload as Record<string, unknown>);
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
	child.stdin.write(encodeFrameString({
		id: 1,
		kind: "req",
		method: "hello",
		payload: { protocolVersion: PROTOCOL_VERSION, compatibilityVersion: COMPATIBILITY_VERSION },
	}));
	return await promise;
}

const REFERENCE_MODULES = [
	["@earendil-works/pi-coding-agent", "packages/coding-agent/src/core/extensions/index.ts"],
	["@earendil-works/pi-coding-agent/builtins", "packages/coding-agent/src/extensions/index.ts"],
	["pi-coding-agent-full", "packages/coding-agent/src/index.ts"],
	["@earendil-works/pi-agent-core", "packages/agent/src/index.ts"],
	["@earendil-works/pi-tui", "packages/tui/src/index.ts"],
	["@earendil-works/pi-ai", "packages/ai/src/compat.ts"],
	["@earendil-works/pi-ai/compat", "packages/ai/src/compat.ts"],
	["@earendil-works/pi-ai/oauth", "packages/ai/src/oauth.ts"],
	["@earendil-works/pi-ai/providers/all", "packages/ai/src/providers/all.ts"],
	["@mariozechner/pi-coding-agent", "packages/coding-agent/src/core/extensions/index.ts"],
	["@mariozechner/pi-agent-core", "packages/agent/src/index.ts"],
	["@mariozechner/pi-tui", "packages/tui/src/index.ts"],
	["@mariozechner/pi-ai", "packages/ai/src/compat.ts"],
	["@mariozechner/pi-ai/compat", "packages/ai/src/compat.ts"],
	["@mariozechner/pi-ai/oauth", "packages/ai/src/oauth.ts"],
	["@mariozechner/pi-ai/providers/all", "packages/ai/src/providers/all.ts"],
] as const;

test("resolves reference modules from this workspace", () => {
	const hostRoot = resolve(import.meta.dirname, "..");
	const referenceRoot = resolve(hostRoot, "../..", ".references", "pi-2.0");
	const importer = resolve(hostRoot, "src", "main.ts");

	for (const [specifier, expectedPath] of REFERENCE_MODULES) {
		expect(Bun.resolveSync(specifier, importer)).toBe(resolve(referenceRoot, expectedPath));
	}
});

/**
 * Release contract: the compiled host bundles the reference packages (jiti
 * `virtualModules`, mirroring the upstream loader), so a bare binary in an
 * empty archive dir, run from a cwd OUTSIDE the repository, must load an
 * extension importing `@earendil-works/pi-coding-agent` and `typebox`.
 */
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
		await run(process.execPath, [
			"build",
			"./src/main.ts",
			"--compile",
			"--outfile",
			hostPath,
		], hostDir);

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
		);
		expect(result["errors"]).toEqual([]);
		expect(result["extensions"]).toBe(1);
		expect(result["tools"]).toEqual(expect.arrayContaining([
			expect.objectContaining({ name: "bundled" }),
		]));
	});

	test("loads an extension importing pi-ai image exports through the root alias", async () => {
		const fixturePath = resolve(
			import.meta.dirname,
			"..",
			"fixtures",
			"extensions",
			"pi-ai-images-import.ts",
		);
		const result = await loadExtension(hostPath, outsideCwd, fixturePath);
		expect(result["errors"]).toEqual([]);
		expect(result["extensions"]).toBe(1);
		expect(result["tools"]).toEqual(expect.arrayContaining([
			expect.objectContaining({
				name: "piAiImagesProbe",
				description: "generate=true;models=true;api=true",
			}),
		]));
	});
});
