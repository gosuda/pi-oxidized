import { describe, expect, test } from "bun:test";
import { join } from "node:path";

import { smokeUnpacked } from "../package-release.ts";
import { RecordingRunner, type Fs, type RunResult } from "../release/runner.ts";
import { planFor } from "../release/targets.ts";

const FILE_STAT = { isFile: true, isDir: false, size: 1, mode: 0o755 } as const;

function existingFilesFs(paths: readonly string[]): Fs {
	const files = new Set(paths);
	return {
		async mkdir() {},
		async rm() {},
		async writeFile() {},
		async readFile() {
			return new Uint8Array();
		},
		async copyFile() {},
		async cp() {},
		async chmod() {},
		async stat(path) {
			if (files.has(path)) return FILE_STAT;
			throw new Error(`ENOENT: ${path}`);
		},
		async readdir() {
			return [];
		},
	};
}

describe("smokeUnpacked", () => {
	test("runs only pi --version and host hello without a runtime-import executable", async () => {
		const plan = planFor("x86_64-unknown-linux-gnu");
		const archiveDir = "/unpacked/pi-linux-x64-base";
		const pi = join(archiveDir, plan.piBinaryName);
		const host = join(archiveDir, plan.hostBinaryName);
		const fs = existingFilesFs([pi, host]);
		const hello: RunResult = {
			exitCode: 0,
			stdout:
				JSON.stringify({
					id: 1,
					kind: "res",
					method: "hello",
					payload: { protocolVersion: 1 },
				}) + "\n",
			stderr: "",
		};
		const runner = new RecordingRunner((call) => {
			if (call.command === pi) {
				return { exitCode: 0, stdout: "pi 0.1.0\n", stderr: "" };
			}
			if (call.command === host) return hello;
			throw new Error(`ENOENT: ${call.command}`);
		});

		await smokeUnpacked({ fs, runner, archiveDir, plan, dryRun: false });

		expect(runner.calls.map(({ command, args }) => [command, args])).toEqual([
			[pi, ["--version"]],
			[host, []],
		]);
	});
});
