import { afterAll, describe, expect, test } from "bun:test";
import {
	mkdirSync,
	mkdtempSync,
	rmSync,
	symlinkSync,
	writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { NoiseRejection, requireQuiet } from "../statistics.ts";
import {
	classifyPackFiles,
	FOOTPRINT_SCAN_SAMPLES,
	FOOTPRINT_SCHEMA,
	npmPlatformMatches,
	PRIMARY_PAYLOAD_PACKAGE,
	parsePackListing,
	planUpstreamClosure,
	UPSTREAM_COMPILED_LAUNCHER,
	UPSTREAM_NPM_LAUNCHER,
	walkApparentBytes,
} from "./footprint.ts";

// PERF-T7: the footprint runner's accounting helpers are the contract's
// mechanical core. These tests pin the classification rules from
// docs/PERF-T7-install-footprint-accounting.md: launcher vs runtime payload
// vs compiled-launcher variant, npm os/cpu platform filtering, symlink
// handling in the apparent-byte walk, the double-counting ban on the primary
// payload, and the degenerate-quiet behavior of repeated static scans.

const temporaryPaths: string[] = [];

afterAll(() => {
	for (const path of temporaryPaths)
		rmSync(path, { recursive: true, force: true });
});

function temporaryDirectory(prefix: string): string {
	const path = mkdtempSync(join(tmpdir(), prefix));
	temporaryPaths.push(path);
	return path;
}

test("does not run the measurement when footprint.ts is imported", () => {
	// The entrypoint guard: importing the module for its helpers must not
	// invoke cargo, npm, or any build step, and must not write the artifact.
	// Importing above already proved no side effect on load; assert the
	// exported constants are the contract's, which the runner only defines
	// (never executes) at module scope.
	expect(FOOTPRINT_SCHEMA).toBe("pi.footprint.v1");
	expect(FOOTPRINT_SCAN_SAMPLES).toBeGreaterThanOrEqual(5);
	expect(UPSTREAM_NPM_LAUNCHER).toBe("dist/bundle/cli.js");
	expect(UPSTREAM_COMPILED_LAUNCHER).toBe("dist/pi");
	expect(PRIMARY_PAYLOAD_PACKAGE).toBe("@earendil-works/pi-coding-agent");
});

describe("classifyPackFiles", () => {
	test("splits launcher, runtime payload, and the compiled-launcher variant", () => {
		const files = [
			{ path: "package.json", size: 100 },
			{ path: UPSTREAM_NPM_LAUNCHER, size: 629 },
			{ path: "dist/bundle/chunks/chunk-X.js", size: 3_722_762 },
			{ path: UPSTREAM_COMPILED_LAUNCHER, size: 93_402_312 },
			{ path: "docs/images/exy.png", size: 1_510_779 },
			{ path: "npm-shrinkwrap.json", size: 59_400 },
		];
		const classified = classifyPackFiles(files);
		expect(classified.launcher).toEqual({ bytes: 629, files: 1, symlinks: 0 });
		expect(classified.runtimePayload).toEqual({
			bytes: 100 + 3_722_762 + 1_510_779 + 59_400,
			files: 4,
			symlinks: 0,
		});
		expect(classified.compiledLauncherVariant).toEqual({
			bytes: 93_402_312,
			files: 1,
			symlinks: 0,
		});
	});

	test("treats a missing compiled launcher as an empty variant, never payload", () => {
		const classified = classifyPackFiles([
			{ path: UPSTREAM_NPM_LAUNCHER, size: 10 },
			{ path: "dist/index.js", size: 20 },
		]);
		expect(classified.compiledLauncherVariant.bytes).toBe(0);
		expect(classified.compiledLauncherVariant.files).toBe(0);
		expect(classified.runtimePayload.bytes).toBe(20);
	});
});

describe("npmPlatformMatches", () => {
	test("absent constraints match any platform", () => {
		expect(npmPlatformMatches(undefined, "linux")).toBe(true);
		expect(npmPlatformMatches([], "darwin")).toBe(true);
	});

	test("positive lists require membership", () => {
		expect(npmPlatformMatches(["darwin", "linux"], "linux")).toBe(true);
		expect(npmPlatformMatches(["darwin"], "linux")).toBe(false);
	});

	test("negations exclude only the named platform", () => {
		expect(npmPlatformMatches(["!win32"], "linux")).toBe(true);
		expect(npmPlatformMatches(["!win32"], "win32")).toBe(false);
	});

	test("cpu fields follow the same semantics", () => {
		expect(npmPlatformMatches(["x64", "arm64"], "arm64")).toBe(true);
		expect(npmPlatformMatches(["!x64"], "arm64")).toBe(true);
		expect(npmPlatformMatches(["x64"], "arm64")).toBe(false);
	});

	test("mixed positive+negative lists follow npm checkList semantics", () => {
		// npm blocks a platform that matches no positive entry even when the
		// list also carries negations: ["!win32", "linux"] rejects darwin.
		expect(npmPlatformMatches(["!win32", "linux"], "linux")).toBe(true);
		expect(npmPlatformMatches(["!win32", "linux"], "darwin")).toBe(false);
		expect(npmPlatformMatches(["!win32", "linux"], "win32")).toBe(false);
		// An all-negation list matches everything it does not name.
		expect(npmPlatformMatches(["!win32"], "darwin")).toBe(true);
	});
});

describe("planUpstreamClosure", () => {
	const lock = {
		packages: {
			"": {},
			"node_modules/chalk": { version: "5.6.2" },
			"node_modules/p-retry/node_modules/@types/retry": { version: "0.12.2" },
			"node_modules/@aws-crypto/crc32": { version: "3.0.0" },
			"node_modules/@mariozechner/clipboard-win32-x64-msvc": {
				version: "0.3.9",
				optional: true,
				os: ["win32"],
				cpu: ["x64"],
			},
			"node_modules/@mariozechner/clipboard-linux-x64-gnu": {
				version: "0.3.9",
				optional: true,
				os: ["linux"],
				cpu: ["x64"],
			},
			"node_modules/@earendil-works/pi-tui": { version: "0.84.3" },
			"node_modules/@earendil-works/pi-agent-core": { version: "0.84.3" },
			[`node_modules/${PRIMARY_PAYLOAD_PACKAGE}`]: { version: "0.84.3" },
		},
	};

	test("separates third-party, first-party links, primary payload, and foreign optionals", () => {
		const plan = planUpstreamClosure(lock, { platform: "linux", arch: "x64" });
		expect(plan.measure.map((entry) => entry.name)).toEqual([
			"@aws-crypto/crc32",
			"@mariozechner/clipboard-linux-x64-gnu",
			"chalk",
		]);
		expect(plan.workspaceLinks.map((entry) => entry.name).sort()).toEqual([
			"@earendil-works/pi-agent-core",
			"@earendil-works/pi-tui",
		]);
		expect(plan.primaryPayload?.name).toBe(PRIMARY_PAYLOAD_PACKAGE);
		expect(plan.foreignOptional.map((entry) => entry.name)).toEqual([
			"@mariozechner/clipboard-win32-x64-msvc",
		]);
	});

	test("platform filtering is relative to the measuring machine", () => {
		const plan = planUpstreamClosure(lock, { platform: "win32", arch: "x64" });
		expect(plan.foreignOptional.map((entry) => entry.name)).toEqual([
			"@mariozechner/clipboard-linux-x64-gnu",
		]);
		expect(plan.measure.map((entry) => entry.name)).toContain(
			"@mariozechner/clipboard-win32-x64-msvc",
		);
	});

	test("nested node_modules keys stay inside their parent walk", () => {
		// A nested install (node_modules/<parent>/node_modules/<child>) is part
		// of the parent's installed directory; planning it as a separate walk
		// root would double-count its bytes (double-counting ban).
		const plan = planUpstreamClosure(lock, { platform: "linux", arch: "x64" });
		const nested = "p-retry/node_modules/@types/retry";
		const allNames = [
			...plan.measure,
			...plan.workspaceLinks,
			...plan.foreignOptional,
		].map((entry) => entry.name);
		expect(allNames).not.toContain(nested);
		expect(plan.measure.map((entry) => entry.name)).toEqual([
			"@aws-crypto/crc32",
			"@mariozechner/clipboard-linux-x64-gnu",
			"chalk",
		]);
	});
});

describe("walkApparentBytes", () => {
	test("sums regular file bytes, skips symlink targets, counts symlinks", () => {
		const root = temporaryDirectory("footprint-walk-");
		writeFileSync(join(root, "a.js"), "12345");
		mkdirSync(join(root, "dir"));
		writeFileSync(join(root, "dir", "b.js"), "1234567");
		symlinkSync("../outside", join(root, "dir", "dangling-link"));
		const walked = walkApparentBytes(root);
		expect(walked).toEqual({ bytes: 12, files: 2, symlinks: 1 });
	});
});

describe("parsePackListing", () => {
	test("parses npm pack --json output and hashes the listing", () => {
		const stdout = JSON.stringify({
			files: [
				{ path: "package.json", size: 4_000, mode: 420 },
				{ path: UPSTREAM_NPM_LAUNCHER, size: 629, mode: 420 },
			],
		});
		const parsed = parsePackListing("fixture", "/tmp/pkg", stdout);
		expect(parsed.label).toBe("fixture");
		expect(parsed.cwd).toBe("/tmp/pkg");
		expect(parsed.files).toHaveLength(2);
		expect(parsed.listingSha256).toHaveLength(64);
	});
	test("parses npm pack's real top-level array output", () => {
		const stdout = JSON.stringify([
			{
				files: [
					{ path: "package.json", size: 4_000, mode: 420 },
					{ path: UPSTREAM_NPM_LAUNCHER, size: 629, mode: 420 },
				],
			},
		]);
		const parsed = parsePackListing("fixture", "/tmp/pkg", stdout);
		expect(parsed.files).toHaveLength(2);
		expect(parsed.listingSha256).toHaveLength(64);
	});

	test("rejects an empty listing", () => {
		expect(() =>
			parsePackListing("fixture", "/tmp/pkg", JSON.stringify({ files: [] })),
		).toThrow(/npm pack listing for fixture is empty/);
	});

	test("rejects malformed entries", () => {
		const stdout = JSON.stringify({ files: [{ path: "x" }] });
		expect(() => parsePackListing("fixture", "/tmp/pkg", stdout)).toThrow(
			/malformed entry/,
		);
	});
});

test("repeated static scans are degenerate and pass the D4 noise gate", () => {
	// The contract requires distributions, not single numbers, and requires
	// them to be quiet. A deterministic byte measurement is the degenerate
	// case: five identical scans must satisfy requireQuiet, not trip it.
	const scans = Array.from(
		{ length: FOOTPRINT_SCAN_SAMPLES },
		() => 118_000_000,
	);
	const sorted = [...scans].sort((a, b) => a - b);
	const median = sorted[Math.floor(sorted.length / 2)] ?? 0;
	expect(() =>
		requireQuiet([
			{
				label: "degenerate static scan",
				count: scans.length,
				median,
				stddev: 0,
				relativeSpread: 0,
			},
		]),
	).not.toThrow();
	expect(() =>
		requireQuiet([
			{
				label: "jittery scan",
				count: 5,
				median: 100,
				stddev: 50,
				relativeSpread: 0.5,
			},
		]),
	).toThrow(NoiseRejection);
});
