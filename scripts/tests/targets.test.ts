import { describe, expect, test } from "bun:test";
import {
	InvalidTargetError,
	RUST_TARGETS,
	TARGET_PLANS,
	archiveName,
	checksumName,
	isSupportedTarget,
	planFor,
	type TargetPlan,
} from "../release/targets.ts";
import { parseReleaseArgs } from "../release/args.ts";

/** Every supported triple in stable declaration order. */
const EXPECTED_TRIPLES = [
	"x86_64-unknown-linux-gnu",
	"x86_64-unknown-linux-musl",
	"aarch64-unknown-linux-gnu",
	"aarch64-unknown-linux-musl",
	"x86_64-apple-darwin",
	"aarch64-apple-darwin",
	"x86_64-pc-windows-msvc",
] as const;

/**
 * Frozen exhaustive expectation for every derived field of every supported
 * plan. A deliberate change to any field must update this table.
 */
const EXPECTED_PLANS = [
	{
		rustTarget: "x86_64-unknown-linux-gnu",
		bunTarget: "bun-linux-x64-baseline",
		os: "linux",
		arch: "x86_64",
		libc: "gnu",
		archive: "tar.gz",
		windows: false,
		darwin: false,
		piBinaryName: "pi",
		hostBinaryName: "pi-extension-host",
		bunRuntimeName: "bun",
		hostBundleName: "pi-extension-host.js",
		archiveDir: "pi-linux-x64-base",
	},
	{
		rustTarget: "x86_64-unknown-linux-musl",
		bunTarget: "bun-linux-x64-musl-baseline",
		os: "linux",
		arch: "x86_64",
		libc: "musl",
		archive: "tar.gz",
		windows: false,
		darwin: false,
		piBinaryName: "pi",
		hostBinaryName: "pi-extension-host",
		bunRuntimeName: "bun",
		hostBundleName: "pi-extension-host.js",
		archiveDir: "pi-linux-x64-musl-base",
	},
	{
		rustTarget: "aarch64-unknown-linux-gnu",
		bunTarget: "bun-linux-arm64",
		os: "linux",
		arch: "aarch64",
		libc: "gnu",
		archive: "tar.gz",
		windows: false,
		darwin: false,
		piBinaryName: "pi",
		hostBinaryName: "pi-extension-host",
		bunRuntimeName: "bun",
		hostBundleName: "pi-extension-host.js",
		archiveDir: "pi-linux-arm64",
	},
	{
		rustTarget: "aarch64-unknown-linux-musl",
		bunTarget: "bun-linux-arm64-musl",
		os: "linux",
		arch: "aarch64",
		libc: "musl",
		archive: "tar.gz",
		windows: false,
		darwin: false,
		piBinaryName: "pi",
		hostBinaryName: "pi-extension-host",
		bunRuntimeName: "bun",
		hostBundleName: "pi-extension-host.js",
		archiveDir: "pi-linux-arm64-musl",
	},
	{
		rustTarget: "x86_64-apple-darwin",
		bunTarget: "bun-darwin-x64-baseline",
		os: "darwin",
		arch: "x86_64",
		libc: "unknown",
		archive: "tar.gz",
		windows: false,
		darwin: true,
		piBinaryName: "pi",
		hostBinaryName: "pi-extension-host",
		bunRuntimeName: "bun",
		hostBundleName: "pi-extension-host.js",
		archiveDir: "pi-darwin-x64-base",
	},
	{
		rustTarget: "aarch64-apple-darwin",
		bunTarget: "bun-darwin-arm64",
		os: "darwin",
		arch: "aarch64",
		libc: "unknown",
		archive: "tar.gz",
		windows: false,
		darwin: true,
		piBinaryName: "pi",
		hostBinaryName: "pi-extension-host",
		bunRuntimeName: "bun",
		hostBundleName: "pi-extension-host.js",
		archiveDir: "pi-darwin-arm64",
	},
	{
		rustTarget: "x86_64-pc-windows-msvc",
		bunTarget: "bun-windows-x64-baseline",
		os: "windows",
		arch: "x86_64",
		libc: "msvc",
		archive: "zip",
		windows: true,
		darwin: false,
		piBinaryName: "pi.exe",
		hostBinaryName: "pi-extension-host.exe",
		bunRuntimeName: "bun.exe",
		hostBundleName: "pi-extension-host.js",
		archiveDir: "pi-windows-x64-base",
	},
] as const satisfies readonly TargetPlan[];

describe("targets", () => {
	test("lists exactly seven master-plan triples in stable order", () => {
		expect(RUST_TARGETS).toEqual(EXPECTED_TRIPLES);
		expect(RUST_TARGETS).toHaveLength(7);
	});

	test("pins every derived field of all seven plans", () => {
		expect(TARGET_PLANS).toHaveLength(EXPECTED_PLANS.length);
		for (const expected of EXPECTED_PLANS) {
			const plan = planFor(expected.rustTarget);
			for (const [field, value] of Object.entries(expected)) {
				expect(plan[field as keyof TargetPlan], `${expected.rustTarget}.${field}`).toBe(
					value,
				);
			}
		}
	});

	test("freezes the pre-existing five plans' derived values", () => {
		const oldFive = EXPECTED_PLANS.filter((p) => p.libc !== "musl");
		expect(oldFive).toHaveLength(5);
		for (const expected of oldFive) {
			const plan = planFor(expected.rustTarget);
			expect(plan.bunTarget, expected.rustTarget).toBe(expected.bunTarget);
			expect(plan.os, expected.rustTarget).toBe(expected.os);
			expect(plan.arch, expected.rustTarget).toBe(expected.arch);
			expect(plan.archive, expected.rustTarget).toBe(expected.archive);
			expect(plan.windows, expected.rustTarget).toBe(expected.windows);
			expect(plan.darwin, expected.rustTarget).toBe(expected.darwin);
			expect(plan.piBinaryName, expected.rustTarget).toBe(expected.piBinaryName);
			expect(plan.hostBinaryName, expected.rustTarget).toBe(expected.hostBinaryName);
			expect(plan.bunRuntimeName, expected.rustTarget).toBe(expected.bunRuntimeName);
			expect(plan.hostBundleName, expected.rustTarget).toBe(expected.hostBundleName);
			expect(plan.archiveDir, expected.rustTarget).toBe(expected.archiveDir);
			expect(plan.rustTarget, expected.rustTarget).toBe(expected.rustTarget);
		}
	});

	test("resolves explicit libc for every supported OS", () => {
		expect(planFor("x86_64-unknown-linux-gnu").libc).toBe("gnu");
		expect(planFor("x86_64-unknown-linux-musl").libc).toBe("musl");
		expect(planFor("aarch64-unknown-linux-gnu").libc).toBe("gnu");
		expect(planFor("aarch64-unknown-linux-musl").libc).toBe("musl");
		expect(planFor("x86_64-apple-darwin").libc).toBe("unknown");
		expect(planFor("aarch64-apple-darwin").libc).toBe("unknown");
		expect(planFor("x86_64-pc-windows-msvc").libc).toBe("msvc");
	});

	test("isSupportedTarget guards the exact set", () => {
		for (const t of RUST_TARGETS) expect(isSupportedTarget(t)).toBe(true);
		expect(isSupportedTarget("x86_64-unknown-freebsd")).toBe(false);
		expect(isSupportedTarget("x86_64-unknown-linux")).toBe(false);
		expect(isSupportedTarget("aarch64-apple-darwin-musl")).toBe(false);
	});

	test("planFor resolves x86_64-unknown-linux-gnu", () => {
		const plan = planFor("x86_64-unknown-linux-gnu");
		expect(plan.bunTarget).toBe("bun-linux-x64-baseline");
		expect(plan.os).toBe("linux");
		expect(plan.arch).toBe("x86_64");
		expect(plan.libc).toBe("gnu");
		expect(plan.archive).toBe("tar.gz");
		expect(plan.windows).toBe(false);
		expect(plan.piBinaryName).toBe("pi");
		expect(plan.hostBinaryName).toBe("pi-extension-host");
		expect(plan.archiveDir).toBe("pi-linux-x64-base");
	});

	test("planFor resolves x86_64-unknown-linux-musl", () => {
		const plan = planFor("x86_64-unknown-linux-musl");
		expect(plan.bunTarget).toBe("bun-linux-x64-musl-baseline");
		expect(plan.os).toBe("linux");
		expect(plan.arch).toBe("x86_64");
		expect(plan.libc).toBe("musl");
		expect(plan.archive).toBe("tar.gz");
		expect(plan.windows).toBe(false);
		expect(plan.piBinaryName).toBe("pi");
		expect(plan.hostBinaryName).toBe("pi-extension-host");
		expect(plan.archiveDir).toBe("pi-linux-x64-musl-base");
	});

	test("planFor resolves aarch64-unknown-linux-musl", () => {
		const plan = planFor("aarch64-unknown-linux-musl");
		expect(plan.bunTarget).toBe("bun-linux-arm64-musl");
		expect(plan.os).toBe("linux");
		expect(plan.arch).toBe("aarch64");
		expect(plan.libc).toBe("musl");
		expect(plan.archive).toBe("tar.gz");
		expect(plan.windows).toBe(false);
		expect(plan.piBinaryName).toBe("pi");
		expect(plan.hostBinaryName).toBe("pi-extension-host");
		expect(plan.archiveDir).toBe("pi-linux-arm64-musl");
	});

	test("planFor resolves x86_64-pc-windows-msvc", () => {
		const plan = planFor("x86_64-pc-windows-msvc");
		expect(plan.bunTarget).toBe("bun-windows-x64-baseline");
		expect(plan.os).toBe("windows");
		expect(plan.arch).toBe("x86_64");
		expect(plan.libc).toBe("msvc");
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
		expect(plan.libc).toBe("unknown");
		expect(plan.archive).toBe("tar.gz");
		expect(plan.windows).toBe(false);
		expect(plan.archiveDir).toBe("pi-darwin-arm64");
	});

	test("InvalidTargetError lists the supported triples (musl included)", () => {
		let listed: InvalidTargetError | undefined;
		try {
			planFor("x86_64-unknown-linux-gnu-musl");
		} catch (e) {
			listed = e as InvalidTargetError;
		}
		expect(listed).toBeInstanceOf(InvalidTargetError);
		expect(listed?.input).toBe("x86_64-unknown-linux-gnu-musl");
		expect(listed?.message).toContain("x86_64-unknown-linux-musl");
		expect(listed?.message).toContain("aarch64-unknown-linux-musl");
		expect(listed?.message).toContain("x86_64-pc-windows-msvc");
	});

	test("InvalidTargetError rejects non-listed triples with the full set", () => {
		// Listed triples must not raise; only non-listed inputs do.
		for (const t of RUST_TARGETS) expect(() => planFor(t)).not.toThrow();
		for (const input of [
			"x86_64-unknown-linux-gnu-musl",
			"x86_64-unknown-linux",
			"aarch64-apple-darwin-musl",
			"riscv64gc-unknown-linux-gnu",
		]) {
			let caught: InvalidTargetError | undefined;
			try {
				planFor(input);
			} catch (e) {
				caught = e as InvalidTargetError;
			}
			expect(caught, input).toBeInstanceOf(InvalidTargetError);
			expect(caught?.input, input).toBe(input);
			expect(caught?.message, input).toContain("Supported triples:");
			expect(caught?.message, input).toContain(input);
			for (const t of RUST_TARGETS) expect(caught?.message, input).toContain(t);
		}
	});

	test("archiveName and checksumName format deterministically", () => {
		const plan = planFor("x86_64-apple-darwin");
		const name = archiveName("1.2.3", plan);
		expect(name).toBe("pi-1.2.3-pi-darwin-x64-base.tar.gz");
		expect(checksumName(name)).toBe("pi-1.2.3-pi-darwin-x64-base.tar.gz.sha256");
	});
});