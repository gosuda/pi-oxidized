import { describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { decodeZipArchive, writeZip } from "../release/archive.ts";
import { smokeUnpacked } from "../package-release.ts";
import {
	helloRequestLine,
	HOST_COMPATIBILITY_VERSION,
	HOST_PROTOCOL_VERSION,
} from "../release/host.ts";
import { RecordingRunner, type Fs, type RunResult } from "../release/runner.ts";
import { BUN_RUNTIME_VERSION, bunRuntimeAsset, provisionBunRuntime } from "../release/runtime.ts";
import { planFor, RUST_TARGETS } from "../release/targets.ts";

const FILE_STAT = { isFile: true, isDir: false, size: 1, mode: 0o755 } as const;

function existingFilesFs(paths: readonly string[]): Fs {
	const files = new Set(paths);
	return {
		async mkdir() {},
		async rm() {},
		async writeFile(path) {
			files.add(path);
		},
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

function helloResult(
	payload: Record<string, unknown> = {
		protocolVersion: HOST_PROTOCOL_VERSION,
		compatibilityVersion: HOST_COMPATIBILITY_VERSION,
	},
	exitCode = 0,
): RunResult {
	return {
		exitCode,
		stdout: `${JSON.stringify({ id: 1, kind: "res", method: "hello", payload })}\n`,
		stderr: exitCode === 0 ? "" : "host crashed",
	};
}

describe("smokeUnpacked", () => {
	test("runs pi --version and a strict compiled-host hello handshake", async () => {
		const plan = planFor("x86_64-unknown-linux-gnu");
		const archiveDir = "/unpacked/pi-linux-x64-base";
		const pi = join(archiveDir, plan.piBinaryName);
		const host = join(archiveDir, plan.hostBinaryName);
		const fs = existingFilesFs([pi, host]);
		const runner = new RecordingRunner((call) => {
			if (call.command === pi) return { exitCode: 0, stdout: "pi 0.1.0\n", stderr: "" };
			if (call.command === host) return helloResult();
			throw new Error(`ENOENT: ${call.command}`);
		});

		await smokeUnpacked({ fs, runner, archiveDir, plan, dryRun: false });

		expect(runner.calls.map(({ command, args }) => [command, args])).toEqual([
			[pi, ["--version"]],
			[host, []],
		]);
		const handshake = runner.calls[1];
		expect(handshake?.options?.stdin).toBe(helloRequestLine());
		expect(JSON.parse(handshake?.options?.stdin?.trim() ?? "{}")).toEqual({
			id: 1,
			kind: "req",
			method: "hello",
			payload: {
				protocolVersion: HOST_PROTOCOL_VERSION,
				compatibilityVersion: HOST_COMPATIBILITY_VERSION,
			},
		});
		expect(runner.calls.every((call) => call.options?.timeoutMs === 30_000)).toBe(true);
	});

	test("smokes the Bun plus JavaScript fallback when the compiled host is absent", async () => {
		const plan = planFor("x86_64-unknown-linux-gnu");
		const archiveDir = "/unpacked/pi-linux-x64-base";
		const pi = join(archiveDir, plan.piBinaryName);
		const runtime = join(archiveDir, plan.bunRuntimeName);
		const script = join(archiveDir, plan.hostBundleName);
		const runner = new RecordingRunner((call) =>
			call.command === pi
				? { exitCode: 0, stdout: "pi 0.1.0\n", stderr: "" }
				: helloResult(),
		);

		await smokeUnpacked({
			fs: existingFilesFs([pi, runtime, script]),
			runner,
			archiveDir,
			plan,
			dryRun: false,
		});

		expect(runner.calls[1]?.command).toBe(runtime);
		expect(runner.calls[1]?.args).toEqual([script]);
		expect(runner.calls[1]?.options?.stdin).toBe(helloRequestLine());
	});

	test("rejects a missing pi binary before spawning", async () => {
		const plan = planFor("x86_64-unknown-linux-gnu");
		const runner = new RecordingRunner(() => {
			throw new Error("must not spawn");
		});
		await expect(
			smokeUnpacked({
				fs: existingFilesFs([]),
				runner,
				archiveDir: "/unpacked/pi-linux-x64-base",
				plan,
				dryRun: false,
			}),
		).rejects.toThrow("missing pi");
		expect(runner.calls).toHaveLength(0);
	});

	test("rejects a nonzero pi --version result", async () => {
		const plan = planFor("x86_64-unknown-linux-gnu");
		const archiveDir = "/unpacked/pi-linux-x64-base";
		const pi = join(archiveDir, plan.piBinaryName);
		const host = join(archiveDir, plan.hostBinaryName);
		const runner = new RecordingRunner(() => ({ exitCode: 7, stdout: "", stderr: "boom" }));
		await expect(
			smokeUnpacked({
				fs: existingFilesFs([pi, host]),
				runner,
				archiveDir,
				plan,
				dryRun: false,
			}),
		).rejects.toThrow("pi --version failed (exit 7)");
		expect(runner.calls).toHaveLength(1);
	});

	test("rejects malformed or incompatible hello acknowledgements", async () => {
		const invalidLines = [
			"not-json",
			JSON.stringify({ kind: "event", method: "hello", payload: {} }),
			JSON.stringify({
				kind: "res",
				method: "hello",
				payload: { protocolVersion: 2, compatibilityVersion: HOST_COMPATIBILITY_VERSION },
			}),
			JSON.stringify({
				kind: "res",
				method: "hello",
				payload: { protocolVersion: HOST_PROTOCOL_VERSION },
			}),
			JSON.stringify({
				kind: "res",
				method: "hello",
				payload: { protocolVersion: HOST_PROTOCOL_VERSION, compatibilityVersion: "wrong" },
			}),
		];
		for (const line of invalidLines) {
			const plan = planFor("x86_64-unknown-linux-gnu");
			const archiveDir = "/unpacked/pi-linux-x64-base";
			const pi = join(archiveDir, plan.piBinaryName);
			const host = join(archiveDir, plan.hostBinaryName);
			const runner = new RecordingRunner((call) =>
				call.command === pi
					? { exitCode: 0, stdout: "pi 0.1.0\n", stderr: "" }
					: { exitCode: 0, stdout: `${line}\n`, stderr: "" },
			);
			await expect(
				smokeUnpacked({
					fs: existingFilesFs([pi, host]),
					runner,
					archiveDir,
					plan,
					dryRun: false,
				}),
			).rejects.toThrow("host hello handshake failed");
		}
	});

	test("rejects a host that acknowledges hello and then exits nonzero", async () => {
		const plan = planFor("x86_64-unknown-linux-gnu");
		const archiveDir = "/unpacked/pi-linux-x64-base";
		const pi = join(archiveDir, plan.piBinaryName);
		const host = join(archiveDir, plan.hostBinaryName);
		const runner = new RecordingRunner((call) =>
			call.command === pi
				? { exitCode: 0, stdout: "pi 0.1.0\n", stderr: "" }
				: helloResult(undefined, 9),
		);
		await expect(
			smokeUnpacked({
				fs: existingFilesFs([pi, host]),
				runner,
				archiveDir,
				plan,
				dryRun: false,
			}),
		).rejects.toThrow("exit 9");
	});

	test("dry-run verifies compiled and fallback layouts without spawning", async () => {
		const plan = planFor("x86_64-unknown-linux-gnu");
		const archiveDir = "/unpacked/pi-linux-x64-base";
		const pi = join(archiveDir, plan.piBinaryName);
		const host = join(archiveDir, plan.hostBinaryName);
		const runtime = join(archiveDir, plan.bunRuntimeName);
		const script = join(archiveDir, plan.hostBundleName);
		for (const paths of [[pi, host], [pi, runtime, script]]) {
			const runner = new RecordingRunner(() => {
				throw new Error("must not spawn");
			});
			await smokeUnpacked({ fs: existingFilesFs(paths), runner, archiveDir, plan, dryRun: true });
			expect(runner.calls).toHaveLength(0);
		}
		await expect(
			smokeUnpacked({
				fs: existingFilesFs([pi, runtime]),
				runner: new RecordingRunner(() => undefined),
				archiveDir,
				plan,
				dryRun: true,
			}),
		).rejects.toThrow("incomplete runtime-bundle fallback");
	});

	test("uses Windows executable names from the target plan", async () => {
		const plan = planFor("x86_64-pc-windows-msvc");
		const archiveDir = "/unpacked/pi-windows-x64-base";
		const pi = join(archiveDir, "pi.exe");
		const host = join(archiveDir, "pi-extension-host.exe");
		const runner = new RecordingRunner((call) =>
			call.command === pi
				? { exitCode: 0, stdout: "pi 0.1.0\n", stderr: "" }
				: helloResult(),
		);
		await smokeUnpacked({
			fs: existingFilesFs([pi, host]),
			runner,
			archiveDir,
			plan,
			dryRun: false,
		});
		expect(runner.calls.map((call) => call.command)).toEqual([pi, host]);
	});
});

describe("pinned Bun runtime provisioning", () => {
	test("maps every release target to a checksum-pinned official asset", () => {
		const expectedFile: Readonly<Record<(typeof RUST_TARGETS)[number], string>> = {
			"x86_64-unknown-linux-gnu": "bun-linux-x64-baseline.zip",
			"aarch64-unknown-linux-gnu": "bun-linux-aarch64.zip",
			"x86_64-apple-darwin": "bun-darwin-x64-baseline.zip",
			"aarch64-apple-darwin": "bun-darwin-aarch64.zip",
			"x86_64-pc-windows-msvc": "bun-windows-x64-baseline.zip",
		};
		for (const target of RUST_TARGETS) {
			const plan = planFor(target);
			const asset = bunRuntimeAsset(plan);
			expect(asset.bunTarget).toBe(plan.bunTarget);
			expect(asset.fileName).toBe(expectedFile[target]);
			expect(asset.sha256).toMatch(/^[0-9a-f]{64}$/);
			expect(asset.runtimeMember).toEndWith(`/${plan.bunRuntimeName}`);
			expect(asset.url).toContain(`/bun-v${BUN_RUNTIME_VERSION}/`);
		}
	});

	test("rejects downloaded runtime bytes before extraction when checksum differs", async () => {
		const plan = planFor("x86_64-unknown-linux-gnu");
		await expect(
			provisionBunRuntime({
				plan,
				destination: "/tmp/bun",
				fs: existingFilesFs([]),
				fetcher: async () => ({
					ok: true,
					status: 200,
					async arrayBuffer() {
						return Uint8Array.from([1, 2, 3]).buffer;
					},
				}),
			}),
		).rejects.toThrow("checksum mismatch");
	});
});

describe("portable ZIP validation", () => {
	test("rejects traversal members before extraction", async () => {
		const work = mkdtempSync(join(tmpdir(), "pi-release-zip-"));
		try {
			const archive = join(work, "traversal.zip");
			await writeZip(
				[{ path: "safe.tx", data: new TextEncoder().encode("payload"), mode: 0o644 }],
				archive,
				{ sourceDateEpoch: 0 },
			);
			const bytes = new Uint8Array(readFileSync(archive));
			const archiveBuffer = Buffer.from(bytes.buffer, bytes.byteOffset, bytes.byteLength);
			const safeName = Buffer.from("safe.tx");
			const unsafeName = Buffer.from("../evil");
			for (let offset = archiveBuffer.indexOf(safeName); offset !== -1; ) {
				bytes.set(unsafeName, offset);
				offset = archiveBuffer.indexOf(safeName, offset + safeName.length);
			}
			expect(() => decodeZipArchive(bytes)).toThrow("archive path escapes root");
		} finally {
			rmSync(work, { recursive: true, force: true });
		}
	});
});
