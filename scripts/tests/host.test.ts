import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

import { buildHost, hostBundleCommands, isHelloAckLine } from "../release/host.ts";
import { OK_RUN, RecordingRunner, type RunResult } from "../release/runner.ts";
import { planFor } from "../release/targets.ts";

let work: string;

beforeEach(() => {
	work = mkdtempSync(join(tmpdir(), "pi-release-host-"));
});

afterEach(() => {
	rmSync(work, { recursive: true, force: true });
});

describe("buildHost", () => {
	test("compiles sidecar and speaks handshake (skipping runtime import)", async () => {
		const plan = planFor("x86_64-unknown-linux-gnu");
		let sidecarBuildPath = "";

		const runner = new RecordingRunner((call): RunResult => {
			if (call.command === "bun" && call.args[0] === "build") {
				if (call.args[1] === "./src/main.ts") {
					const outIdx = call.args.indexOf("--outfile");
					if (outIdx >= 0) {
						sidecarBuildPath = call.args[outIdx + 1] ?? "";
						if (sidecarBuildPath) writeFileSync(sidecarBuildPath, "fake-binary");
					}
				}
				return OK_RUN;
			}
			
			if (call.command.includes("pi-extension-host")) {
				return {
					exitCode: 0,
					stdout: '{"id":1,"kind":"res","method":"hello","payload":{"protocolVersion":1,"compatibilityVersion":"0.80.10"}}\n',
					stderr: "",
				};
			}
			return OK_RUN;
		});

		const host = await buildHost({
			repoRoot: "/workspace",
			stagingRoot: work,
			plan,
			skipRuntimeImport: true, // skipped because we don't have mock workspaces setup
			skipHandshake: false,
			runner,
		});

		expect(host.kind).toBe("compiled");
		if (host.kind === "compiled") {
			expect(host.binaryPath).toBe(sidecarBuildPath);
		}

		const subcommands = runner.calls.map((call) => call.args[0]);
		const ranHostTests = runner.calls.some(
			(call) =>
				call.command === "bun" &&
				(call.args[0] === "test" || (call.args[0] === "run" && call.args[1] === "test")),
		);
		expect(ranHostTests).toBe(false);
		expect(subcommands.filter((subcommand) => subcommand === "build")).toHaveLength(1);

		const handshakeCall = runner.calls.find((c) => c.command.includes("pi-extension-host"));
		expect(handshakeCall).toBeDefined();
		expect(handshakeCall?.options?.stdin).toContain('"method":"hello"');

		const fixtureCall = runner.calls.find((c) => c.command.includes("runtime-import-test"));
		expect(fixtureCall).toBeUndefined();
		expect(runner.calls.every((call) => (call.options?.timeoutMs ?? 0) > 0)).toBe(true);
	});

	test("probes the built sidecar without compiling a second runtime", async () => {
		const repoRoot = resolve(import.meta.dirname, "../..");
		const hostDir = join(repoRoot, "packages", "extension-host");
		const extensionPath = join(hostDir, "fixtures", "extensions", "tool.ts");
		const plan = planFor("x86_64-unknown-linux-gnu");
		let sidecarPath = "";
		const runner = new RecordingRunner((call): RunResult => {
			if (call.command === "bun" && call.args[0] === "build") {
				const outIndex = call.args.indexOf("--outfile");
				const outPath = call.args[outIndex + 1];
				if (outPath !== undefined) writeFileSync(outPath, "artifact");
				if (call.args[1] === "./src/main.ts") sidecarPath = outPath ?? "";
				return OK_RUN;
			}
			if (
				call.command.includes("runtime-import-test") ||
				(call.command === "bun" && call.args[0]?.endsWith("runtime-import.ts"))
			) {
				return {
					exitCode: 0,
					stdout: `${JSON.stringify({
						path: extensionPath,
						tools: ["echo"],
						handlers: ["session_start"],
						commands: ["greet"],
						flags: [],
						shortcuts: [],
						messageRenderers: [],
					})}\n`,
					stderr: "",
				};
			}
			if (call.command.includes("pi-extension-host")) {
				return {
					exitCode: 0,
					stdout: '{"id":1,"kind":"res","method":"hello","payload":{"protocolVersion":1,"compatibilityVersion":"0.80.10"}}\n',
					stderr: "",
				};
			}
			return OK_RUN;
		});

		const host = await buildHost({
			repoRoot,
			stagingRoot: work,
			plan,
			skipRuntimeImport: false,
			skipHandshake: false,
			runner,
		});

		expect(host).toEqual({ kind: "compiled", binaryPath: sidecarPath });
		const buildCalls = runner.calls.filter(
			(call) => call.command === "bun" && call.args[0] === "build",
		);
		expect(buildCalls).toHaveLength(1);
		const probe = runner.calls.find(
			(call) => call.command === "bun" && call.args[0]?.endsWith("runtime-import.ts"),
		);
		expect(probe?.args).toEqual([
			join(hostDir, "fixtures", "runtime-import.ts"),
			sidecarPath,
			extensionPath,
		]);
		expect(probe?.options?.cwd).toBe(hostDir);
		expect(probe?.options?.timeoutMs).toBe(30_000);
	});

	test("fails instead of substituting a runtime bundle when the sidecar probe fails", async () => {
		const repoRoot = resolve(import.meta.dirname, "../..");
		const plan = planFor("x86_64-unknown-linux-gnu");
		let bundled = false;
		const runner = new RecordingRunner((call): RunResult => {
			if (call.command === "bun" && call.args[0] === "build") {
				const outIndex = call.args.indexOf("--outfile");
				const outPath = call.args[outIndex + 1];
				if (outPath !== undefined) writeFileSync(outPath, "artifact");
				if (!call.args.includes("--compile")) bundled = true;
				return OK_RUN;
			}
			if (
				call.command.includes("runtime-import-test") ||
				(call.command === "bun" && call.args[0]?.endsWith("runtime-import.ts"))
			) {
				return { exitCode: 1, stdout: "", stderr: "probe failed" };
			}
			return OK_RUN;
		});

		await expect(
			buildHost({
				repoRoot,
				stagingRoot: work,
				plan,
				skipRuntimeImport: false,
				skipHandshake: true,
				runner,
			}),
		).rejects.toThrow("runtime-import probe failed");
		expect(bundled).toBe(false);
	});

	test("falls back to consistently named Bun and JavaScript assets", async () => {
		const plan = planFor("x86_64-unknown-linux-gnu");
		let buildCount = 0;
		const runner = new RecordingRunner((call): RunResult => {
			if (call.command === "bun" && call.args[0] === "build") {
				buildCount++;
				if (call.args.includes("--compile")) {
					return { exitCode: 1, stdout: "", stderr: "compile unsupported" };
				}
				const outIndex = call.args.indexOf("--outfile");
				const scriptPath = call.args[outIndex + 1];
				if (scriptPath !== undefined) writeFileSync(scriptPath, "bundled-host");
			}
			return OK_RUN;
		});

		const host = await buildHost({
			repoRoot: "/workspace",
			stagingRoot: work,
			plan,
			skipRuntimeImport: true,
			skipHandshake: true,
			runner,
		});

		expect(host).toEqual({
			kind: "runtime-bundle",
			runtimePath: join(work, "host", plan.rustTarget, plan.bunRuntimeName),
			scriptPath: join(work, "host", plan.rustTarget, plan.hostBundleName),
		});
		expect(buildCount).toBe(2);
		const bundleCall = runner.calls.find(
			(call) => call.args[0] === "build" && !call.args.includes("--compile"),
		);
		expect(bundleCall?.args).not.toContain("--outdir");
		expect(bundleCall?.args.at(-2)).toBe("--outfile");
		expect(bundleCall?.args.at(-1)).toBe(
			join(work, "host", plan.rustTarget, plan.hostBundleName),
		);
		expect(runner.calls.every((call) => (call.options?.timeoutMs ?? 0) > 0)).toBe(true);
	});
});

describe("hostBundleCommands", () => {
	test("exposes compiled and runtime-bundle argv from one authority", () => {
		const plan = planFor("x86_64-unknown-linux-gnu");
		const outDir = "/staging/host/x86_64-unknown-linux-gnu";
		const commands = hostBundleCommands(plan, outDir);

		expect(commands.compiled).toEqual([
			"build",
			"./src/main.ts",
			"--compile",
			"--minify",
			"--compile-autoload-tsconfig",
			"--compile-autoload-package-json",
			"--target",
			plan.bunTarget,
			"--outfile",
			join(outDir, plan.hostBinaryName),
		]);
		expect(commands.runtimeBundle).toEqual([
			"build",
			"./src/main.ts",
			"--target",
			"bun",
			"--minify",
			"--outfile",
			join(outDir, plan.hostBundleName),
		]);
	});
});

describe("isHelloAckLine", () => {
	test("accepts a complete hello acknowledgement with id 1", () => {
		const canonical = JSON.stringify({
			id: 1,
			kind: "res",
			method: "hello",
			payload: { protocolVersion: 1, compatibilityVersion: "0.80.10" },
		});
		expect(isHelloAckLine(canonical)).toBe(true);
	});

	test("rejects missing, zero, and mismatched request identifiers", () => {
		const payload = { protocolVersion: 1, compatibilityVersion: "0.80.10" };
		expect(
			isHelloAckLine(JSON.stringify({ kind: "res", method: "hello", payload })),
		).toBe(false); // missing id
		expect(
			isHelloAckLine(JSON.stringify({ id: 0, kind: "res", method: "hello", payload })),
		).toBe(false); // zero id
		expect(
			isHelloAckLine(JSON.stringify({ id: 2, kind: "res", method: "hello", payload })),
		).toBe(false); // mismatched id
		expect(
			isHelloAckLine(JSON.stringify({ id: "1", kind: "res", method: "hello", payload })),
		).toBe(false); // wrong type
	});

	test("preserves strict protocol and compatibility checks", () => {
		const canonical = JSON.stringify({
			id: 1,
			kind: "res",
			method: "hello",
			payload: { protocolVersion: 1, compatibilityVersion: "0.80.10" },
		});
		expect(isHelloAckLine(canonical)).toBe(true);
		expect(isHelloAckLine(`prefix ${canonical}`)).toBe(false);
		expect(
			isHelloAckLine(
				JSON.stringify({
					id: 1,
					kind: "res",
					method: "hello",
					payload: { protocolVersion: 1, compatibilityVersion: "wrong" },
				}),
			),
		).toBe(false);
	});
});
