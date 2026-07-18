import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { buildHost, isHelloAckLine } from "../release/host.ts";
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
					stdout: '{"kind":"res","method":"hello","payload":{"protocolVersion":1,"compatibilityVersion":"0.80.10"}}\n',
					stderr: "",
				};
			}
			return OK_RUN;
		});

		const host = await buildHost({
			repoRoot: "/workspace",
			stagingRoot: work,
			plan,
			skipTests: true,
			skipRuntimeImport: true, // skipped because we don't have mock workspaces setup
			skipHandshake: false,
			runner,
		});

		expect(host.kind).toBe("compiled");
		if (host.kind === "compiled") {
			expect(host.binaryPath).toBe(sidecarBuildPath);
		}

		const subcommands = runner.calls.map((c) => c.args[0]);
		expect(subcommands).not.toContain("test");
		expect(subcommands.filter((s) => s === "build")).toHaveLength(1);

		const handshakeCall = runner.calls.find((c) => c.command.includes("pi-extension-host"));
		expect(handshakeCall).toBeDefined();
		expect(handshakeCall?.options?.stdin).toContain('"method":"hello"');

		const fixtureCall = runner.calls.find((c) => c.command.includes("runtime-import-test"));
		expect(fixtureCall).toBeUndefined();
		expect(runner.calls.every((call) => (call.options?.timeoutMs ?? 0) > 0)).toBe(true);
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
			skipTests: true,
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

describe("isHelloAckLine", () => {
	test("accepts only strict protocol and compatibility acknowledgements", () => {
		const canonical = JSON.stringify({
			kind: "res",
			method: "hello",
			payload: { protocolVersion: 1, compatibilityVersion: "0.80.10" },
		});
		expect(isHelloAckLine(canonical)).toBe(true);
		expect(isHelloAckLine(`prefix ${canonical}`)).toBe(false);
		expect(
			isHelloAckLine(
				JSON.stringify({
					kind: "res",
					method: "hello",
					payload: { protocolVersion: 1, compatibilityVersion: "wrong" },
				}),
			),
		).toBe(false);
	});
});
