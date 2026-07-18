import { describe, expect, test } from "bun:test";
import {
	RUST_TARGETS,
	TARGET_PLANS,
	archiveName,
	checksumName,
	isSupportedTarget,
	planFor,
} from "../release/targets.ts";
import { parseReleaseArgs } from "../release/args.ts";

describe("targets", () => {
	test("provides exactly five master-plan triples", () => {
		expect(RUST_TARGETS).toHaveLength(5);
		expect(RUST_TARGETS).toContain("x86_64-unknown-linux-gnu");
		expect(RUST_TARGETS).toContain("aarch64-unknown-linux-gnu");
		expect(RUST_TARGETS).toContain("x86_64-apple-darwin");
		expect(RUST_TARGETS).toContain("aarch64-apple-darwin");
		expect(RUST_TARGETS).toContain("x86_64-pc-windows-msvc");
	});

	test("isSupportedTarget guards the exact set", () => {
		for (const t of RUST_TARGETS) expect(isSupportedTarget(t)).toBe(true);
		expect(isSupportedTarget("x86_64-unknown-freebsd")).toBe(false);
	});

	test("planFor resolves x86_64-unknown-linux-gnu", () => {
		const plan = planFor("x86_64-unknown-linux-gnu");
		expect(plan.bunTarget).toBe("bun-linux-x64-baseline");
		expect(plan.os).toBe("linux");
		expect(plan.arch).toBe("x86_64");
		expect(plan.archive).toBe("tar.gz");
		expect(plan.windows).toBe(false);
		expect(plan.piBinaryName).toBe("pi");
		expect(plan.hostBinaryName).toBe("pi-extension-host");
		expect(plan.archiveDir).toBe("pi-linux-x64-base");
	});

	test("planFor resolves x86_64-pc-windows-msvc", () => {
		const plan = planFor("x86_64-pc-windows-msvc");
		expect(plan.bunTarget).toBe("bun-windows-x64-baseline");
		expect(plan.os).toBe("windows");
		expect(plan.arch).toBe("x86_64");
		expect(plan.archive).toBe("zip");
		expect(plan.windows).toBe(true);
		expect(plan.piBinaryName).toBe("pi.exe");
		expect(plan.hostBinaryName).toBe("pi-extension-host.exe");
		expect(plan.bunRuntimeName).toBe("bun.exe");
		expect(plan.archiveDir).toBe("pi-windows-x64-base");
	});

	test("planFor resolves aarch64-apple-darwin", () => {
		const plan = planFor("aarch64-apple-darwin");
		expect(plan.bunTarget).toBe("bun-darwin-arm64");
		expect(plan.os).toBe("darwin");
		expect(plan.arch).toBe("aarch64");
		expect(plan.archive).toBe("tar.gz");
		expect(plan.windows).toBe(false);
		expect(plan.archiveDir).toBe("pi-darwin-arm64");
	});

	test("archiveName and checksumName format deterministically", () => {
		const plan = planFor("x86_64-apple-darwin");
		const name = archiveName("1.2.3", plan);
		expect(name).toBe("pi-1.2.3-pi-darwin-x64-base.tar.gz");
		expect(checksumName(name)).toBe("pi-1.2.3-pi-darwin-x64-base.tar.gz.sha256");
	});
});

describe("args", () => {
	test("parseReleaseArgs resolves happy path", () => {
		const args = parseReleaseArgs(
			["--target", "aarch64-apple-darwin", "--dry-run", "--out", "/tmp/out"],
			"/cwd",
			"123",
		);
		expect(args.plan.rustTarget).toBe("aarch64-apple-darwin");
		expect(args.dryRun).toBe(true);
		expect(args.outDir).toBe("/tmp/out");
		expect(args.sourceDateEpoch).toBe("123");
	});

	test("parseReleaseArgs rejects missing value", () => {
		expect(() => parseReleaseArgs(["--target"])).toThrow(/requires a value/);
		expect(() => parseReleaseArgs(["--target", "--out"])).toThrow(/requires a value/);
	});

	test("parseReleaseArgs rejects invalid source date epoch", () => {
		expect(() =>
			parseReleaseArgs(["--target", "x86_64-apple-darwin", "--source-date-epoch", "1.5"]),
		).toThrow(/base-10 integer/);
	});
});
