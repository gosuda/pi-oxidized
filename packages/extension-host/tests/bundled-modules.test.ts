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
import { isCompiledModuleUrl } from "../src/virtual-modules.ts";

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
				? "compiled host did not exit after extensions.load"
				: "compiled host did not answer extensions.load",
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
});
