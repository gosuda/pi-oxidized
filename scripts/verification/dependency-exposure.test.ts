import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { spawn, spawnSync } from "node:child_process";
import {
	appendFileSync,
	chmodSync,
	copyFileSync,
	existsSync,
	mkdirSync,
	mkdtempSync,
	readFileSync,
	readdirSync,
	rmSync,
	statSync,
	writeFileSync,
} from "node:fs";
import { createServer, type Server, type Socket } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import {
	CANONICAL_REFERENCE_DIR,
	ExposureError,
	REPO_ROOT,
	captureReference,
	e1Cargo,
	e1Npm,
	e2Reachability,
	e4Verdict,
	enumerateStagedInputs,
	loadReferenceBundle,
	normalizeCompileArgv,
	compileEntryConforms,
	parseCargoGraphProjection,
	parseNpmSurface,
	parseReferenceManifest,
	parseSeamCall,
	parseSubject,
	resolveInputOwner,
	scanSeamSites,
	selfCheck,
	sha256FileAt,
	sha256Text,
	verdictFromChecks,
	checkAuthorityIntegrity,
	pass,
	fail,
	undecidable,
	type CaptureOptions,
	type CargoGraphProjection,
	type NpmSurface,
	type ReferenceManifest,
	type ReferenceNpmSurface,
	type Subject,
} from "./dependency-exposure.ts";

function subjectOf(raw: string): Subject {
	return parseSubject(raw);
}

function surfaceFromJson(json: string, relPath: string): NpmSurface {
	const dir = mkdtempSync(join(tmpdir(), "de-surface-"));
	writeFileSync(join(dir, "package.json"), json);
	const surface = parseNpmSurface(join(dir, "package.json"), dir);
	return { ...surface, relPath };
}

function refSurface(json: string, relPath: string): ReferenceNpmSurface {
	const surface = surfaceFromJson(json, relPath);
	return {
		path: relPath,
		sha256: surface.sha256,
		packageName: surface.packageName,
		depFields: surface.depFields,
	};
}

const EMPTY_SURFACE = '{"name":"x","devDependencies":{}}';

describe("parseSubject", () => {
	test("accepts npm (scoped and bare), crate, and tool subjects", () => {
		expect(subjectOf("npm:typebox").kind).toBe("npm");
		expect(subjectOf("npm:@types/bun").name).toBe("@types/bun");
		expect(subjectOf("crate:serde").kind).toBe("crate");
		expect(subjectOf("tool:rust-toolchain").kind).toBe("tool");
	});
	test("rejects malformed subjects", () => {
		expect(() => subjectOf("typebox")).toThrow(ExposureError);
		expect(() => subjectOf("npm:")).toThrow(ExposureError);
		expect(() => subjectOf("tool:not-a-tool")).toThrow(ExposureError);
		expect(() => subjectOf("py:requests")).toThrow(ExposureError);
	});
});

describe("E1 npm field position across all three surfaces", () => {
	const pre = [
		refSurface('{"name":"a","dependencies":{"typebox":"1.0.0"},"devDependencies":{"@types/bun":"1.0.0"}}', "packages/extension-host/package.json"),
		refSurface(EMPTY_SURFACE, "package.json"),
		refSurface(EMPTY_SURFACE, "packages/pi-tui-protocol/package.json"),
	];

	test("non-dev field in any surface fails", () => {
		const result = e1Npm(subjectOf("npm:typebox"), pre, [
			surfaceFromJson('{"name":"a","dependencies":{"typebox":"1.0.0"}}', "packages/extension-host/package.json"),
			surfaceFromJson(EMPTY_SURFACE, "package.json"),
			surfaceFromJson(EMPTY_SURFACE, "packages/pi-tui-protocol/package.json"),
		]);
		expect(result.status).toBe("fail");
	});

	test("devDependencies-only across every surface (pre and post) passes", () => {
		const devEverywhere = (rel: string): NpmSurface =>
			surfaceFromJson('{"name":"a","devDependencies":{"@types/bun":"1.0.0"}}', rel);
		const result = e1Npm(subjectOf("npm:@types/bun"), pre, [
			devEverywhere("package.json"),
			devEverywhere("packages/extension-host/package.json"),
			devEverywhere("packages/pi-tui-protocol/package.json"),
		]);
		expect(result.status).toBe("pass");
	});

	test("removal from a non-dev field (pre prod, post absent) fails: removal of a shipped dep is Class S", () => {
		const result = e1Npm(subjectOf("npm:typebox"), pre, [
			surfaceFromJson('{"name":"a"}', "packages/extension-host/package.json"),
			surfaceFromJson(EMPTY_SURFACE, "package.json"),
			surfaceFromJson(EMPTY_SURFACE, "packages/pi-tui-protocol/package.json"),
		]);
		expect(result.status).toBe("fail");
	});

	test("surface set change is undecidable (a new surface cannot be classified against a stale reference)", () => {
		const result = e1Npm(subjectOf("npm:@types/bun"), pre, [
			surfaceFromJson(EMPTY_SURFACE, "package.json"),
			surfaceFromJson(EMPTY_SURFACE, "packages/extension-host/package.json"),
			surfaceFromJson(EMPTY_SURFACE, "packages/pi-tui-protocol/package.json"),
			surfaceFromJson(EMPTY_SURFACE, "packages/new-surface/package.json"),
		]);
		expect(result.status).toBe("undecidable");
	});

	test("malformed surface JSON throws and maps to undecidable via the classify guard", () => {
		const dir = mkdtempSync(join(tmpdir(), "de-bad-"));
		writeFileSync(join(dir, "package.json"), "{ not json");
		expect(() => parseNpmSurface(join(dir, "package.json"), dir)).toThrow(ExposureError);
	});
});

describe("E1 cargo edge position (graph only, never manifest text)", () => {
	const graphOf = (edges: readonly { from: string; to: string; kinds: string[] }[]): CargoGraphProjection =>
		parseCargoGraphProjection(
			JSON.stringify({
				schema: "pi.deps.exposure-cargo-graph.v1",
				argv: ["metadata", "--format-version", "1", "--locked", "--offline", "--all-features"],
				workspaceMembers: ["pi", "pi-tui"],
				edges,
			}),
			"test-graph",
		);

	test("dev edges in both graphs pass", () => {
		const dev = graphOf([{ from: "pi-tui", to: "criterion", kinds: ["dev"] }]);
		expect(e1Cargo(subjectOf("crate:criterion"), dev, dev).status).toBe("pass");
	});

	test("post-graph flipping the edge to normal fails", () => {
		const pre = graphOf([{ from: "pi-tui", to: "serde", kinds: ["dev"] }]);
		const post = graphOf([{ from: "pi-tui", to: "serde", kinds: ["normal"] }]);
		const result = e1Cargo(subjectOf("crate:serde"), pre, post);
		expect(result.status).toBe("fail");
		expect(result.detail).toContain("kinds=[normal]");
	});

	test("graph wins over manifest text: a manifest claiming dev-dependencies while the graph says normal still fails", () => {
		// e1Cargo receives only graphs; the manifest text below is never an input.
		const manifestText = "[dev-dependencies]\nserde = \"1.0\"\n";
		expect(manifestText).toContain("dev-dependencies");
		const graph = graphOf([{ from: "pi", to: "serde", kinds: ["normal"] }]);
		expect(e1Cargo(subjectOf("crate:serde"), graph, graph).status).toBe("fail");
	});

	test("missing either graph is undecidable", () => {
		const dev = graphOf([{ from: "pi-tui", to: "criterion", kinds: ["dev"] }]);
		expect(e1Cargo(subjectOf("crate:criterion"), undefined, dev).status).toBe("undecidable");
		expect(e1Cargo(subjectOf("crate:criterion"), dev, undefined).status).toBe("undecidable");
	});
});

describe("E2 metafile reachability and ownership", () => {
	const surfaces = [{ relPath: "packages/pi-tui-protocol/package.json", packageName: "@earendil-works/pi-tui-protocol" }];

	test("npm ownership follows innermost node_modules segments", () => {
		expect(resolveInputOwner("node_modules/typebox/build/index.mjs")).toBe("typebox");
		expect(resolveInputOwner("../../.references/pi-2.0/node_modules/typebox/build/index.mjs")).toBe("typebox");
		expect(resolveInputOwner("a/node_modules/@scope/pkg/src/x.ts")).toBe("@scope/pkg");
		expect(resolveInputOwner("packages/extension-host/src/main.ts")).toBeNull();
		expect(resolveInputOwner("node_modules/a/node_modules/b/i.js")).toBe("b");
	});

	test("reachable npm subject fails; zero-reachability passes; unrelated names are not substring-matched", () => {
		const inputs = [
			"../../.references/pi-2.0/node_modules/typebox/build/index.mjs",
			"../pi-tui-protocol/src/index.ts",
		];
		expect(e2Reachability(subjectOf("npm:typebox"), inputs, surfaces).status).toBe("fail");
		expect(e2Reachability(subjectOf("npm:typebox-helpers"), inputs, surfaces).status).toBe("pass");
		expect(e2Reachability(subjectOf("npm:@types/bun"), inputs, surfaces).status).toBe("pass");
	});

	test("workspace package source bundled into the sidecar is reachable", () => {
		const result = e2Reachability(subjectOf("npm:@earendil-works/pi-tui-protocol"), ["../pi-tui-protocol/src/index.ts"], surfaces);
		expect(result.status).toBe("fail");
	});

	test("crate subjects are vacuous; tools fail on the bundler surface", () => {
		expect(e2Reachability(subjectOf("crate:serde"), [], surfaces).status).toBe("pass");
		expect(e2Reachability(subjectOf("tool:bun-bundler"), [], surfaces).status).toBe("fail");
		expect(e2Reachability(subjectOf("tool:bun-runtime"), [], surfaces).status).toBe("fail");
		expect(e2Reachability(subjectOf("tool:rust-toolchain"), [], surfaces).status).toBe("pass");
	});

	test("authority drift makes the module graph untrustworthy (undecidable)", () => {
		const bundle = loadReferenceBundle(CANONICAL_REFERENCE_DIR);
		const good = checkAuthorityIntegrity(REPO_ROOT, bundle.manifest);
		expect(good.status).toBe("pass");
		const tampered = { ...bundle.manifest, authority: [{ path: "scripts/release/host.ts", sha256: "0".repeat(64) }] };
		expect(checkAuthorityIntegrity(REPO_ROOT, tampered).status).toBe("undecidable");
	});
});

describe("--compile entry conformance", () => {
	const authority = normalizeCompileArgv([
		"build", "./src/main.ts", "--compile", "--minify", "--compile-autoload-tsconfig",
		"--compile-autoload-package-json", "--target", "bun-linux-x64-baseline", "--outfile", "/tmp/out",
	]);

	test("package.json build script omitting --target still conforms (value flags may be omitted)", () => {
		const local = normalizeCompileArgv(
			"bun build ./src/main.ts --compile --minify --compile-autoload-tsconfig --compile-autoload-package-json --outfile dist/pi-extension-host".split(" "),
		);
		expect(compileEntryConforms(local, authority)).toBe(true);
	});

	test("a different entrypoint or an unknown flag diverges", () => {
		const otherEntry = normalizeCompileArgv("bun build ./src/other.ts --compile --minify".split(" "));
		expect(compileEntryConforms(otherEntry, authority)).toBe(false);
		const rogueFlag = normalizeCompileArgv("bun build ./src/main.ts --compile --external foo".split(" "));
		expect(compileEntryConforms(rogueFlag, authority)).toBe(false);
	});
});

describe("E3 seam scan (CommandRunner.run seam)", () => {
	test("attributes literal argvs, authority spreads, and exec-only sites", () => {
		const sources = {
			"a.ts": [
				`const res = await runner.run("bun", ["install", "--frozen-lockfile"], {});`,
				`return runner.run("bun", [...compiled], {});`,
				`const r = await runner.run(sidecarPath, [], {});`,
				`const p = await runner.run("bun", [fixtureSource, sidecarPath, exampleExt], {});`,
			].join("\n"),
		};
		const scan = scanSeamSites(sources);
		expect(scan.problems).toEqual([]);
		expect(scan.sites.length).toBe(4);
		const bunBuild = scan.sites.find((site) => site.command === "bun" && site.spreadNames.includes("compiled"));
		expect(bunBuild).toBeDefined();
	});

	test("build-capable bun site with unattributable emit args is undecidable", () => {
		const sources = {
			"b.ts": `await runner.run("bun", [flagsVar, "--outfile", out], {});`,
		};
		const scan = scanSeamSites(sources);
		expect(scan.problems.length).toBe(1);
		expect(scan.problems[0]).toContain("b.ts:1");
	});

	test("cargo argvs with dynamic segments stay attributable (linkage is graph-decided)", () => {
		const sources = {
			"c.ts": `await runner.run("cargo", ["build", "-p", "pi", "--release", "--locked", "--target", args.plan.rustTarget], {});`,
		};
		expect(scanSeamSites(sources).problems).toEqual([]);
	});

	test("static template-literal argvs are attributed: a backtick build invocation is undecidable, not invisible", () => {
		// `build` and `--compile` are static template literals (no ${}); they
		// must be read as literals so emit intent is visible. The interpolated
		// `--outfile=${out}` stays unresolved, but the visible literals already
		// prove this is a build-capable, unattributable site => undecidable.
		const sources = {
			"d.ts": "await runner.run(`bun`, [`build`, `--compile`, `--outfile=${out}`], {});",
		};
		const scan = scanSeamSites(sources);
		expect(scan.problems.length).toBe(1);
		expect(scan.problems[0]).toContain("d.ts:1");
		const site = scan.sites.find((s) => s.command === "bun");
		expect(site).toBeDefined();
		expect(site?.literalArgs).toContain("build");
		expect(site?.literalArgs).toContain("--compile");
		expect(site?.unresolved).toBe(true);
	});

	test("bare-identifier argv with no emit token stays attributable (execution probe, not a build)", () => {
		// host.ts:263 shape: bun running a fixture probe with variable args and
		// no build/emit literal anywhere => not a shipped-byte-producing site.
		const sources = {
			"e.ts": `const run = await runner.run("bun", [fixtureSource, sidecarPath, exampleExt], {});`,
		};
		expect(scanSeamSites(sources).problems).toEqual([]);
	});
});

describe("E4 staged-input table from the assembly script source", () => {
	test("enumerates both host-kind tables via the byte-verified stage.ts authority", async () => {
		const bundle = loadReferenceBundle(CANONICAL_REFERENCE_DIR);
		const authority = checkAuthorityIntegrity(REPO_ROOT, bundle.manifest);
		const staged = await enumerateStagedInputs(REPO_ROOT, authority);
		expect(staged.problem).toBeUndefined();
		const kinds = new Set(staged.rows.map((row) => row.kind));
		expect(kinds.has("rust-binary")).toBe(true);
		expect(kinds.has("host-binary")).toBe(true);
		expect(kinds.has("host-bundle")).toBe(true);
		expect(kinds.has("bun-runtime")).toBe(true);
	});

	test("authority drift blocks enumeration (undecidable)", async () => {
		const staged = await enumerateStagedInputs(REPO_ROOT, undecidable("authority drifted"));
		expect(staged.problem).toBeDefined();
		expect(e4Verdict(subjectOf("npm:@types/bun"), [], staged.problem).status).toBe("undecidable");
	});

	test("tool products staged into the archive fail E4", () => {
		const rows = [{ kind: "bun-runtime", source: "/staging/bun", destRel: "bun", hostKind: "runtime-bundle" }];
		expect(e4Verdict(subjectOf("tool:bun-runtime"), rows, undefined).status).toBe("fail");
		expect(e4Verdict(subjectOf("tool:rust-toolchain"), [{ kind: "rust-binary", source: "/t/pi", destRel: "pi", hostKind: "compiled" }], undefined).status).toBe("fail");
	});

	test("npm subject staged from its install path fails; ordinary tables pass", () => {
		const hit = [{ kind: "extra", source: "/repo/node_modules/typebox/x.js", destRel: "x.js", hostKind: "compiled" }];
		expect(e4Verdict(subjectOf("npm:typebox"), hit, undefined).status).toBe("fail");
		expect(e4Verdict(subjectOf("npm:@types/bun"), [], undefined).status).toBe("pass");
	});
});

describe("verdict fail-closed algebra", () => {
	const all = { E1: pass("x"), E2: pass("x"), E3: pass("x"), E4: pass("x") };
	test("all pass -> Class E", () => {
		expect(verdictFromChecks(subjectOf("npm:@types/bun"), all).exposureClass).toBe("E");
	});
	test("any fail -> Class S", () => {
		expect(verdictFromChecks(subjectOf("npm:typebox"), { ...all, E2: fail("bundled") }).exposureClass).toBe("S");
	});
	test("any undecidable -> Class S (never an exemption)", () => {
		expect(verdictFromChecks(subjectOf("npm:x"), { ...all, E3: undecidable("?") }).exposureClass).toBe("S");
		const onlyUndecidable = {
			E1: undecidable("a"), E2: undecidable("b"), E3: undecidable("c"), E4: undecidable("d"),
		};
		expect(verdictFromChecks(subjectOf("npm:x"), onlyUndecidable).exposureClass).toBe("S");
	});
});

describe("reference hash chain", () => {
	test("reference.json hash-pins both projections and they verify", () => {
		const bundle = loadReferenceBundle(CANONICAL_REFERENCE_DIR);
		const manifestText = JSON.stringify(bundle.manifest);
		// Round-trip: the manifest must re-parse to the same pins.
		const reparsed = parseReferenceManifest(manifestText, "roundtrip");
		expect(reparsed.metafile.sha256).toBe(bundle.manifest.metafile.sha256);
		expect(Object.keys(bundle.metafile.inputs).length).toBeGreaterThan(2000);
	});
});

describe("self-check against the canonical reference (known members + fail-closed probes)", () => {
	test("typebox=Class S, @types/bun=Class E (recorded verdict), tampered reference=Class S", async () => {
		const tmp = join(tmpdir(), "de-selfcheck-");
		mkdirSync(tmp, { recursive: true });
		const outcomes = await selfCheck(CANONICAL_REFERENCE_DIR, tmp);
		for (const outcome of outcomes) {
			expect(`${outcome.name}=${outcome.actual}`).toBe(`${outcome.name}=${outcome.expected}`);
		}
		expect(outcomes.length).toBe(4);
		rmSync(tmp, { recursive: true, force: true });
	}, 120_000);
});

describe("CLI fail-closed sentinel", () => {
	test("a reference path that cannot be loaded yields Class S report text with the OK sentinel (decided), and a crashed classifier yields exit 1 + FAILED_CLOSED", () => {
		const decided = spawnSync(
			"bun",
			["run", join(REPO_ROOT, "scripts/verification/dependency-exposure.ts"), "classify", "--subject", "npm:typebox", "--reference", "/nonexistent/reference"],
			{ cwd: REPO_ROOT, encoding: "utf8", timeout: 120_000 },
		);
		expect(decided.status).toBe(0);
		expect((decided.stdout ?? "").includes("DEPENDENCY_EXPOSURE_OK")).toBe(true);
		expect((decided.stdout ?? "").includes("class:      S")).toBe(true);

		const crashed = spawnSync(
			"bun",
			["run", join(REPO_ROOT, "scripts/verification/dependency-exposure.ts"), "classify", "--subject", "tool:bogus", "--reference", CANONICAL_REFERENCE_DIR],
			{ cwd: REPO_ROOT, encoding: "utf8", timeout: 120_000 },
		);
		expect(crashed.status).toBe(1);
		expect((crashed.stderr ?? "").includes("DEPENDENCY_EXPOSURE_FAILED_CLOSED")).toBe(true);
	}, 300_000);
});
const STAGED_PROVENANCE_SCHEMA = "pi.deps.staged-inputs.v1";

function sha256Prefix(): string {
	// 64-hex sha256 placeholder (sha256 of empty), not all zeros.
	return "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
}

function sha1Prefix(): string {
	// 40-hex OID (sha1 of the empty string), valid and not all zeros.
	return "da39a3ee5e6b4b0d3255bfef95601890afd80709";
}

interface StagedProvenanceFixture {
	schema: string;
	mode: "staged";
	baseHead: string;
	objectFormat: "sha1";
	pathspecs: readonly string[];
	entries: readonly { path: string; mode: "100644"; blob: string }[];
}

function makeStagedProvenance(baseHead: string, paths: readonly string[]): StagedProvenanceFixture {
	return {
		schema: STAGED_PROVENANCE_SCHEMA,
		mode: "staged",
		baseHead,
		objectFormat: "sha1",
		pathspecs: [...paths].sort(),
		entries: paths
			.map((path): { path: string; mode: "100644"; blob: string } => ({
				path,
				mode: "100644",
				blob: sha1Prefix(),
			}))
			.sort((a, b) => (a.path < b.path ? -1 : a.path > b.path ? 1 : 0)),
	};
}

/**
 * Canonical valid manifest literal; each test passes only its deltas.
 * `captureHead` (and the provenance fields) are emitted only when provided,
 * so their absence stays testable exactly as in the original literals.
 */
function validManifest(
	overrides: {
		readonly captureHead?: string;
		readonly relevantTreeStatus?: string;
		readonly stagedInputProvenance?: StagedProvenanceFixture;
		readonly metafileProjectionPath?: string;
	} = {},
) {
	const { captureHead, relevantTreeStatus, stagedInputProvenance, metafileProjectionPath } = overrides;
	return {
		schema: "pi.deps.exposure-reference.v1" as const,
		capturedAt: new Date().toISOString(),
		...(captureHead !== undefined ? { captureHead } : {}),
		...(relevantTreeStatus !== undefined ? { relevantTreeStatus } : {}),
		...(stagedInputProvenance !== undefined ? { stagedInputProvenance } : {}),
		metafile: {
			projectionPath: metafileProjectionPath ?? "metafile-projection." + sha256Prefix() + ".json",
			sha256: sha256Prefix(),
			entry: "./src/main.ts",
			hostDirRel: "packages/extension-host",
			argv: ["build"],
			metafileSha256: sha256Prefix(),
		},
		cargo: {
			projectionPath: "cargo-graph-projection." + sha256Prefix() + ".json",
			sha256: sha256Prefix(),
			argv: ["metadata", "--locked"],
		},
		npmSurfaces: [] as never[],
		cargoFiles: [] as never[],
		authority: [] as never[],
	};
}

describe("parseReferenceManifest strict provenance and projection basenames", () => {
	test("accepts staged provenance with hash-named projection basenames", () => {
		const stagedHead = sha1Prefix();
		const staged = makeStagedProvenance(stagedHead, ["packages/extension-host/package.json"]);
		const manifest = validManifest({ captureHead: stagedHead, stagedInputProvenance: staged });
		const parsed = parseReferenceManifest(JSON.stringify(manifest), "staged manifest");
		expect(parsed.stagedInputProvenance?.baseHead).toBe(stagedHead);
		expect(parsed.metafile.projectionPath).toBe(manifest.metafile.projectionPath);
		expect(parsed.cargo.projectionPath).toBe(manifest.cargo.projectionPath);
	});

	test("rejects projection basenames that are not plain file names", () => {
		for (const bad of ["../escape.json", "foo/bar.json", "", ".", "..", "foo\\bar.json"]) {
			const manifest = validManifest({ captureHead: sha1Prefix(), metafileProjectionPath: bad });
			expect(() => parseReferenceManifest(JSON.stringify(manifest), `bad-${bad ?? "empty"}`)).toThrow(ExposureError);
		}
	});

	test("rejects staged provenance paired with relevantTreeStatus", () => {
		const stagedHead = sha1Prefix();
		const manifest = validManifest({
			captureHead: stagedHead,
			relevantTreeStatus: "?? packages/extension-host/package.json",
			stagedInputProvenance: makeStagedProvenance(stagedHead, ["packages/extension-host/package.json"]),
		});
		expect(() => parseReferenceManifest(JSON.stringify(manifest), "staged+dirty")).toThrow(ExposureError);
	});

	test("staged provenance requires captureHead and equality with baseHead", () => {
		const { captureHead: _omittedHead, ...base } = validManifest();
		const missingHead = JSON.stringify({
			...base,
			stagedInputProvenance: makeStagedProvenance(sha1Prefix(), ["packages/extension-host/package.json"]),
		});
		const mismatchedHead = JSON.stringify({
			...base,
			captureHead: "1".repeat(40),
			stagedInputProvenance: makeStagedProvenance(sha1Prefix(), ["packages/extension-host/package.json"]),
		});
		expect(() => parseReferenceManifest(missingHead, "missing-head")).toThrow(ExposureError);
		expect(() => parseReferenceManifest(mismatchedHead, "mismatched-head")).toThrow(ExposureError);
	});
});

describe("captureReference input mode guards", () => {
	// The runtime guards exist for untyped (JavaScript) callers; an `unknown`
	// value single-cast at the call boundary simulates that caller honestly.
	test("staged mode with allowDirtyRelevant true throws before capture", async () => {
		const invalid: unknown = { inputMode: "staged", allowDirtyRelevant: true };
		await expect(
			captureReference(
				REPO_ROOT,
				join(tmpdir(), "exposure-reject-" + Math.random().toString(36).slice(2)),
				invalid as CaptureOptions,
			),
		).rejects.toThrow(ExposureError);
	});

	test("unknown input mode throws before capture", async () => {
		const invalid: unknown = { inputMode: "bogus" };
		await expect(
			captureReference(
				REPO_ROOT,
				join(tmpdir(), "exposure-reject-" + Math.random().toString(36).slice(2)),
				invalid as CaptureOptions,
			),
		).rejects.toThrow(ExposureError);
	});
});

describe("CLI capture-reference flag mutual exclusion", () => {
	test("rejects --staged-inputs with --allow-dirty-relevant", () => {
		const result = spawnSync(
			"bun",
			["run", join(REPO_ROOT, "scripts/verification/dependency-exposure.ts"), "capture-reference", "--staged-inputs", "--allow-dirty-relevant", "--out", mkdtempSync(join(tmpdir(), "de-cli-out-"))],
			{ cwd: REPO_ROOT, encoding: "utf8", timeout: 30_000 },
		);
		expect(result.status).toBe(2);
	});

	test("rejects missing --out", () => {
		const result = spawnSync(
			"bun",
			["run", join(REPO_ROOT, "scripts/verification/dependency-exposure.ts"), "capture-reference", "--staged-inputs"],
			{ cwd: REPO_ROOT, encoding: "utf8", timeout: 30_000 },
		);
		expect(result.status).toBe(2);
	});

	test("rejects unknown capture flag", () => {
		const result = spawnSync(
			"bun",
			["run", join(REPO_ROOT, "scripts/verification/dependency-exposure.ts"), "capture-reference", "--out", "/tmp/out", "--bogus"],
			{ cwd: REPO_ROOT, encoding: "utf8", timeout: 30_000 },
		);
		expect(result.status).toBe(2);
	});
});

// ---------------------------------------------------------------------------
// Real producer captures (plan cases 14–19).
//
// Every capture below runs the real `captureReference` against a minimal but
// complete owned Git repository under target/exposure-producer-tests/: real
// fixture authority modules imported by the fresh capture-argv Bun
// subprocess, a real `bun build` of a real entrypoint, and a real
// `cargo metadata --locked --offline --all-features` over a zero-dependency
// workspace. Nothing mocks Git, the build, or the metafile.
//
// Fixture Git subprocesses run with the repository/index redirection
// variables (GIT_DIR, GIT_WORK_TREE, GIT_INDEX_FILE, GIT_COMMON_DIR,
// GIT_OBJECT_DIRECTORY, GIT_ALTERNATE_OBJECT_DIRECTORIES) and any
// GIT_CONFIG_* injection scrubbed from the child environment, so a
// precommit/index ambient environment can never direct fixture `git add`
// into real state. Synthetic commits disable user hooks and signing through
// fixture-only `-c` invocation settings plus `--no-verify`; no real or
// global Git config is read or changed. The same variables are removed from
// this process's environment for the duration of this describe so the
// in-process capture helper sees the same clean view, and restored after.
//
// The mid-capture rendezvous is deterministic, not observational: the real
// captureReference runs in an owned child process (its authority subprocess
// is spawnSync, so an in-process call could never service a handshake), and
// the fixture authority module parks on a conditional top-level-await
// loopback handshake before returning the real build argv — strictly after
// the producer's input inventory and before both postchecks. The parent
// mutates only while the authority is parked, then releases it. For the
// malformed-artifact case a transparent `bun` shim in PATH executes the
// real build, then parks so the parent can corrupt the real produced
// metafile before the producer reads it — no fabricated producer output.
// ---------------------------------------------------------------------------

const PRODUCER_TEST_ROOT = join(REPO_ROOT, "target", "exposure-producer-tests");
const PRODUCER_TIMEOUT_MS = 300_000;
const PRODUCER_OWNED: string[] = [];

const GIT_REDIRECTION_VARS = [
	"GIT_DIR",
	"GIT_WORK_TREE",
	"GIT_INDEX_FILE",
	"GIT_COMMON_DIR",
	"GIT_OBJECT_DIRECTORY",
	"GIT_ALTERNATE_OBJECT_DIRECTORIES",
] as const;
const GIT_CONFIG_INJECTION = /^(?:GIT_CONFIG_COUNT|GIT_CONFIG_KEY_\d+|GIT_CONFIG_VALUE_\d+|GIT_CONFIG_GLOBAL|GIT_CONFIG_SYSTEM)$/;

const FIXTURE_IDENTITY = {
	GIT_AUTHOR_NAME: "Exposure Producer Tests",
	GIT_AUTHOR_EMAIL: "exposure-producer-tests@example.invalid",
	GIT_COMMITTER_NAME: "Exposure Producer Tests",
	GIT_COMMITTER_EMAIL: "exposure-producer-tests@example.invalid",
} as const;

let fixtureGitConfigDir = "";

function fixtureGitEnv(): Record<string, string> {
	const env: Record<string, string> = {};
	for (const [key, value] of Object.entries(process.env)) {
		if (value === undefined) continue;
		if ((GIT_REDIRECTION_VARS as readonly string[]).includes(key)) continue;
		if (GIT_CONFIG_INJECTION.test(key)) continue;
		env[key] = value;
	}
	return {
		...env,
		...FIXTURE_IDENTITY,
		GIT_OPTIONAL_LOCKS: "0",
		// Fixture-only invocation settings: empty global/system config files so
		// user hooks, signing and identity config cannot leak into synthetic
		// repositories. Real/global config files are never modified.
		GIT_CONFIG_GLOBAL: join(fixtureGitConfigDir, "gitconfig.empty"),
		GIT_CONFIG_SYSTEM: join(fixtureGitConfigDir, "gitconfig.empty"),
	};
}

function fixtureGit(repo: string, args: readonly string[]): string {
	const result = spawnSync(
		"git",
		[
			"-c", `core.hooksPath=${join(fixtureGitConfigDir, "no-hooks")}`,
			"-c", `init.templateDir=${join(fixtureGitConfigDir, "no-hooks")}`,
			"-c", "commit.gpgSign=false",
			"-c", "tag.gpgSign=false",
			...args,
		],
		{ cwd: repo, encoding: "utf8", maxBuffer: 64 * 1024 * 1024, env: fixtureGitEnv() },
	);
	if (result.status !== 0) {
		throw new Error(`git ${args.join(" ")} failed in ${repo}: ${result.stderr ?? ""}`);
	}
	return result.stdout ?? "";
}

function writeFixtureFile(repo: string, relPath: string, content: string): void {
	const absolute = join(repo, relPath);
	mkdirSync(dirname(absolute), { recursive: true });
	writeFileSync(absolute, content);
}

const FIXTURE_NAMES_TS = (binaryName: string): string =>
	`export const HOST_BINARY_NAME = ${JSON.stringify(binaryName)};\n`;


/**
 * Generated-module rendezvous: park fixture-child evaluation until the
 * owning test releases the named barrier env var. `announceExpr` is the
 * generated-source expression the child writes to announce itself.
 */
const barrierParkSource = (envVar: string, announceExpr: string, timeoutLabel: string): string =>
	`const barrierPort = process.env.${envVar};
if (typeof barrierPort === "string" && barrierPort.length > 0) {
	// The handshake is the rendezvous; the socket timeout is only a failure
	// bound so a stuck parent can never wedge the producer child forever.
	await new Promise<void>((resolvePromise, rejectPromise) => {
		const socket = connect({ host: "127.0.0.1", port: Number(barrierPort) }, () => {
			socket.write(${announceExpr});
		});
		let buffer = "";
		socket.setTimeout(120000, () => {
			socket.destroy(new Error(${JSON.stringify(timeoutLabel)}));
		});
		socket.on("data", (chunk) => {
			buffer += chunk.toString("utf8");
			if (buffer.includes("\\n")) {
				socket.end();
				resolvePromise();
			}
		});
		socket.on("error", rejectPromise);
	});
}`;
const FIXTURE_TARGETS_TS = (bunTarget: string): string =>
	`import { connect } from "node:net";
import { HOST_BINARY_NAME } from "./names.ts";

// Fixture-only rendezvous: when the owning test names a loopback port, park
// this authority module's evaluation until the parent releases it. The
// variable is set only by the barriered-capture tests, so ordinary captures
// never execute this path.
${barrierParkSource(
	"EXPOSURE_FIXTURE_AUTHORITY_BARRIER_PORT",
	'"authority-parked\\n"',
	"authority barrier release timed out",
)}

export interface FixtureTargetPlan {
	readonly rustTarget: string;
	readonly bunTarget: string;
	readonly hostBinaryName: string;
	readonly hostBundleName: string;
}

export function planFor(rustTarget: string): FixtureTargetPlan {
	return {
		rustTarget,
		bunTarget: ${JSON.stringify(bunTarget)},
		hostBinaryName: HOST_BINARY_NAME,
		hostBundleName: HOST_BINARY_NAME + ".bundle.js",
	};
}
`;

const FIXTURE_HOST_TS = (extraFlags: readonly string[]): string =>
	`import { join } from "node:path";
import type { FixtureTargetPlan } from "./targets.ts";

export function hostBundleCommands(
	plan: FixtureTargetPlan,
	outDir: string,
): { readonly compiled: readonly string[]; readonly runtimeBundle: readonly string[] } {
	return {
		compiled: [
			"build",
			"./src/main.ts",
${extraFlags.map((flag) => `\t\t\t${JSON.stringify(flag)},`).join("\n")}
			"--target",
			plan.bunTarget,
			"--outfile",
			join(outDir, plan.hostBinaryName),
		],
		runtimeBundle: [
			"build",
			"./src/main.ts",
			"--target",
			"bun",
			"--outfile",
			join(outDir, plan.hostBundleName),
		],
	};
}
`;
const FIXTURE_STAGE_TS = `export function stagedInputs(): readonly string[] {
	return [];
}
`;

const FIXTURE_MAIN_TS = `export const fixtureHost = true;
console.log("fixture extension host");
`;

const fixtureCrateToml = (name: string): string => `[package]
name = "${name}"
version = "0.0.0"
edition = "2021"
`;

/** Real `cargo generate-lockfile --offline`: the installed toolchain writes whatever lock it considers current. */
function generateFixtureLockfile(repo: string): void {
	const result = spawnSync("cargo", ["generate-lockfile", "--offline"], { cwd: repo, encoding: "utf8" });
	if (result.status !== 0) {
		throw new Error(`cargo generate-lockfile failed in ${repo}: ${result.stderr ?? ""}`);
	}
}

const FIXTURE_CARGO_TOML = `[workspace]
resolver = "2"
members = ["crates/*"]
`;

/** Create a minimal but complete owned repository that satisfies every capture input. */
function newExposureRepo(label: string): string {
	mkdirSync(PRODUCER_TEST_ROOT, { recursive: true });
	const repo = mkdtempSync(join(PRODUCER_TEST_ROOT, `${label}-`));
	PRODUCER_OWNED.push(repo);
	writeFixtureFile(
		repo,
		"package.json",
		'{"name":"exposure-fixture","private":true,"version":"0.0.0","workspaces":["packages/*"]}\n',
	);
	writeFixtureFile(repo, "Cargo.toml", FIXTURE_CARGO_TOML);
	writeFixtureFile(repo, "crates/fixture-crate/Cargo.toml", fixtureCrateToml("fixture-crate"));
	writeFixtureFile(repo, "crates/fixture-crate/src/lib.rs", "pub fn fixture() {}\n");
	generateFixtureLockfile(repo);
	writeFixtureFile(
		repo,
		"packages/extension-host/package.json",
		'{"name":"fixture-extension-host","version":"0.0.0"}\n',
	);
	writeFixtureFile(repo, "packages/extension-host/tsconfig.json", '{"compilerOptions":{}}\n');
	writeFixtureFile(repo, "packages/extension-host/src/main.ts", FIXTURE_MAIN_TS);
	writeFixtureFile(repo, "packages/pi-tui-protocol/package.json", '{"name":"fixture-protocol","version":"0.0.0"}\n');
	writeFixtureFile(repo, "scripts/release/names.ts", FIXTURE_NAMES_TS("pi-extension-host"));
	writeFixtureFile(repo, "scripts/release/targets.ts", FIXTURE_TARGETS_TS("bun"));
	writeFixtureFile(repo, "scripts/release/host.ts", FIXTURE_HOST_TS([]));
	writeFixtureFile(repo, "scripts/release/stage.ts", FIXTURE_STAGE_TS);
	fixtureGit(repo, ["init", "-q", "-b", "main"]);
	fixtureGit(repo, ["add", "."]);
	fixtureGit(repo, ["commit", "-q", "--no-verify", "-m", "fixture baseline"]);
	return repo;
}

/** Add a second workspace crate so the cargo-graph projection becomes a new generation. */
function addSecondFixtureCrate(repo: string): void {
	writeFixtureFile(repo, "crates/second-crate/Cargo.toml", fixtureCrateToml("second-crate"));
	writeFixtureFile(repo, "crates/second-crate/src/lib.rs", "pub fn second() {}\n");
	generateFixtureLockfile(repo);
	fixtureGit(repo, ["add", "crates/second-crate", "Cargo.lock"]);
}

function ownedOutDir(label: string): string {
	mkdirSync(PRODUCER_TEST_ROOT, { recursive: true });
	const dir = mkdtempSync(join(PRODUCER_TEST_ROOT, `${label}-`));
	PRODUCER_OWNED.push(dir);
	return dir;
}

/** The directory a capture's recorded `--metafile=` argument points into (its unique build scratch). */
function capturedMetafileDir(manifest: ReferenceManifest): string {
	const token = manifest.metafile.argv.find((arg) => arg.startsWith("--metafile="));
	if (token === undefined) throw new Error("captured argv lacks --metafile");
	return dirname(token.slice("--metafile=".length));
}

// Child-side driver: runs the real captureReference from the real module and
// reports a structured one-line result. The capture must run in a child
// because its authority subprocess is spawnSync — an in-process call would
// block this event loop and the rendezvous could never be serviced.
const CAPTURE_DRIVER_SOURCE = `import { pathToFileURL } from "node:url";

const [modulePath, root, outDir, mode] = process.argv.slice(2);
const mod = await import(pathToFileURL(modulePath).href);
const options =
	mode === "staged" ? { inputMode: "staged" } :
	mode === "dirty" ? { allowDirtyRelevant: true } : {};
try {
	const manifest = await mod.captureReference(root, outDir, options);
	process.stdout.write("CAPTURE_RESULT " + JSON.stringify({ ok: true, captureHead: manifest.captureHead }) + "\\n");
} catch (error) {
	const message = error instanceof Error ? error.message : String(error);
	process.stdout.write("CAPTURE_RESULT " + JSON.stringify({ ok: false, error: message }) + "\\n");
}
`;

// Transparent `bun` build wrapper for the malformed-artifact case: executes
// the real build with the real argv, then parks on the barrier so the parent
// can corrupt the real produced metafile before the producer reads it. The
// wrapper synchronizes real I/O; it never fabricates producer output.
const BUILD_WRAPPER_SOURCE = `import { spawnSync } from "node:child_process";
import { connect } from "node:net";

const args = process.argv.slice(2);
const realBun = process.env.EXPOSURE_FIXTURE_REAL_BUN;
if (typeof realBun !== "string" || realBun.length === 0) {
	process.stderr.write("build-wrapper: EXPOSURE_FIXTURE_REAL_BUN unset\\n");
	process.exit(2);
}
const result = spawnSync(realBun, args, { stdio: "inherit" });
const metafileArg = args.find((arg) => arg.startsWith("--metafile="));
if (metafileArg !== undefined) {
	const metafilePath = metafileArg.slice("--metafile=".length);
${barrierParkSource(
	"EXPOSURE_FIXTURE_BUILD_BARRIER_PORT",
	'"built " + JSON.stringify(metafilePath) + "\\n"',
	"build barrier release timed out",
)}
}
process.exit(result.status === null ? 1 : result.status);
`;

/**
 * A `bun` shim directory for the malformed-artifact case: every non-build
 * invocation execs the real binary unchanged; the authority build (the only
 * argv carrying --metafile=) routes through the parking wrapper above.
 */
function writeBunShim(workDir: string): Record<string, string> {
	const shimDir = join(workDir, "shim-bin");
	mkdirSync(shimDir, { recursive: true });
	const wrapperPath = join(workDir, "build-wrapper.ts");
	writeFileSync(wrapperPath, BUILD_WRAPPER_SOURCE);
	const shimPath = join(shimDir, "bun");
	writeFileSync(
		shimPath,
		[
			"#!/bin/sh",
			'case " $* " in',
			`*" --metafile="*) exec "$EXPOSURE_FIXTURE_REAL_BUN" ${JSON.stringify(wrapperPath)} "$@" ;;`,
			'*) exec "$EXPOSURE_FIXTURE_REAL_BUN" "$@" ;;',
			"esac",
			"",
		].join("\n"),
	);
	chmodSync(shimPath, 0o755);
	return {
		PATH: `${shimDir}:${process.env.PATH ?? ""}`,
		EXPOSURE_FIXTURE_REAL_BUN: process.execPath,
	};
}

interface CaptureChildOutcome {
	readonly ok: boolean;
	readonly captureHead?: string;
	readonly error?: string;
}

/**
 * Run the real captureReference in an owned child parked at a deterministic
 * rendezvous. `barrier` selects the fixture seam that announces itself on
 * the loopback socket: "authority" parks inside the fixture authority
 * module's conditional top-level await (after the producer's input
 * inventory, before both postchecks); "build" parks inside the transparent
 * shim after the real build wrote the real metafile. onParked runs while
 * the producer side is blocked — the only moment the parent may mutate or
 * corrupt — then the child is released and its structured result returned.
 * A child that exits without rendezvousing fails loudly, so an unrelated
 * refusal can never masquerade as proof. All waits are bounded failure
 * limits, never synchronization.
 */
async function runBarrieredCapture(options: {
	repo: string;
	outDir: string;
	mode: "committed" | "staged" | "dirty";
	workDir: string;
	barrier: "authority" | "build";
	extraEnv?: Record<string, string>;
	onParked: (announcement: string) => void | Promise<void>;
}): Promise<CaptureChildOutcome> {
	mkdirSync(options.workDir, { recursive: true });
	const driverPath = join(options.workDir, "capture-driver.ts");
	writeFileSync(driverPath, CAPTURE_DRIVER_SOURCE);

	let parkedSocket: Socket | undefined;
	let announce: (line: string) => void = () => {};
	let announceFail: (error: Error) => void = () => {};
	const parked = new Promise<string>((resolvePromise, rejectPromise) => {
		announce = resolvePromise;
		announceFail = rejectPromise;
	});
	const server: Server = createServer((socket) => {
		parkedSocket = socket;
		let buffer = "";
		socket.on("data", (chunk) => {
			buffer += chunk.toString("utf8");
			const newline = buffer.indexOf("\n");
			if (newline !== -1) announce(buffer.slice(0, newline));
		});
		socket.on("error", announceFail);
	});
	await new Promise<void>((resolvePromise, rejectPromise) => {
		server.once("error", rejectPromise);
		server.listen(0, "127.0.0.1", () => resolvePromise());
	});
	const address = server.address();
	if (address === null || typeof address === "string") {
		server.close();
		throw new Error("barrier server has no loopback port");
	}

	const child = spawn(
		process.execPath,
		[
			driverPath,
			join(REPO_ROOT, "scripts/verification/dependency-exposure.ts"),
			options.repo,
			options.outDir,
			options.mode,
		],
		{
			cwd: options.repo,
			env: {
				...fixtureGitEnv(),
				...(options.barrier === "authority"
					? { EXPOSURE_FIXTURE_AUTHORITY_BARRIER_PORT: String(address.port) }
					: { EXPOSURE_FIXTURE_BUILD_BARRIER_PORT: String(address.port) }),
				...options.extraEnv,
			},
		},
	);
	let stdout = "";
	let stderr = "";
	if (child.stdout !== null) {
		child.stdout.setEncoding("utf8").on("data", (chunk: string) => {
			stdout += chunk;
		});
	}
	if (child.stderr !== null) {
		child.stderr.setEncoding("utf8").on("data", (chunk: string) => {
			stderr += chunk;
		});
	}
	const exited = new Promise<number>((resolvePromise) => {
		child.on("close", (code) => resolvePromise(code ?? 1));
		child.on("error", () => resolvePromise(1));
	});
	const watchdog = setTimeout(() => child.kill("SIGKILL"), 240_000);
	const release = (): void => {
		try {
			parkedSocket?.write("go\n");
			parkedSocket?.end();
		} catch {
			// The parked side is already gone.
		}
	};
	const cleanup = (): void => {
		clearTimeout(watchdog);
		parkedSocket?.destroy();
		server.close();
	};
	const rendezvousTimeout = setTimeout(
		() => announceFail(new Error("producer rendezvous timed out")),
		120_000,
	);
	try {
		const announcement = await Promise.race([
			parked,
			exited.then((code) => {
				throw new Error(
					`capture child exited ${code} before the rendezvous: ${stderr.slice(0, 400)}`,
				);
			}),
		]);
		await options.onParked(announcement);
	} catch (error) {
		release();
		child.kill("SIGKILL");
		await exited;
		cleanup();
		throw error;
	} finally {
		clearTimeout(rendezvousTimeout);
	}
	release();
	const code = await exited;
	cleanup();
	const line = stdout.split("\n").find((candidate) => candidate.startsWith("CAPTURE_RESULT "));
	if (line === undefined) {
		throw new Error(`capture child produced no result (exit ${code}): ${stderr.slice(0, 400)}`);
	}
	return JSON.parse(line.slice("CAPTURE_RESULT ".length)) as CaptureChildOutcome;
}

describe("captureReference real producer captures (plan cases 14-19)", () => {
	const savedProcessEnv = new Map<string, string | undefined>();

	beforeAll(() => {
		mkdirSync(PRODUCER_TEST_ROOT, { recursive: true });
		fixtureGitConfigDir = join(PRODUCER_TEST_ROOT, "fixture-git-config");
		mkdirSync(join(fixtureGitConfigDir, "no-hooks"), { recursive: true });
		writeFileSync(join(fixtureGitConfigDir, "gitconfig.empty"), "");
		PRODUCER_OWNED.push(fixtureGitConfigDir);
		// A precommit/index ambient environment must not redirect the in-process
		// capture helper's view of the fixture repositories either.
		for (const key of Object.keys(process.env)) {
			if ((GIT_REDIRECTION_VARS as readonly string[]).includes(key) || GIT_CONFIG_INJECTION.test(key)) {
				savedProcessEnv.set(key, process.env[key]);
				delete process.env[key];
			}
		}
	});

	afterAll(() => {
		for (const [key, value] of savedProcessEnv) {
			if (value === undefined) delete process.env[key];
			else process.env[key] = value;
		}
		for (const dir of PRODUCER_OWNED) rmSync(dir, { recursive: true, force: true });
	});

	// Case 14: real staged capture succeeds; committed dirty refusal and the
	// dep-field-free dirty option remain separate modes.
	test(
		"staged capture records real indexed provenance; committed dirty refusal and dep-field-free option stay separate",
		async () => {
			const repo = newExposureRepo("modes");
			const head = fixtureGit(repo, ["rev-parse", "HEAD"]).trim();

			// A staged (indexed, worktree-identical) dep-free change.
			writeFixtureFile(
				repo,
				"packages/extension-host/package.json",
				'{"name":"fixture-extension-host","version":"0.0.0","description":"staged edit"}\n',
			);
			fixtureGit(repo, ["add", "packages/extension-host/package.json"]);
			const stagedBlob = fixtureGit(repo, ["rev-parse", ":packages/extension-host/package.json"]).trim();

			const outStaged = ownedOutDir("out-staged");
			const staged = await captureReference(repo, outStaged, { inputMode: "staged" });
			expect(staged.captureHead).toBe(head);
			expect(staged.stagedInputProvenance?.baseHead).toBe(head);
			expect(staged.stagedInputProvenance?.objectFormat).toBe("sha1");
			expect(staged.relevantTreeStatus).toBeUndefined();
			const entry = staged.stagedInputProvenance?.entries.find(
				(e) => e.path === "packages/extension-host/package.json",
			);
			expect(entry?.blob).toBe(stagedBlob);
			// The staged (indexed) bytes are what the capture pinned.
			const surface = staged.npmSurfaces.find((s) => s.path === "packages/extension-host/package.json");
			expect(surface?.sha256).toBe(
				sha256Text(readFileSync(join(repo, "packages/extension-host/package.json"), "utf8")),
			);
			expect(staged.metafile.projectionPath).toMatch(/^metafile-projection\.[0-9a-f]{64}\.json$/);
			expect(staged.cargo.projectionPath).toMatch(/^cargo-graph-projection\.[0-9a-f]{64}\.json$/);
			expect(loadReferenceBundle(outStaged).manifest.captureHead).toBe(head);

			// Unstaged relevant dirt: the committed default refuses before any
			// output exists, staged mode refuses too (it is not a dirty bypass),
			// and the dep-field-free option records the status and proceeds.
			writeFixtureFile(repo, "packages/extension-host/src/main.ts", 'export const fixtureHost = "dirty";\n');
			const outCommitted = join(PRODUCER_TEST_ROOT, "never-created-committed");
			await expect(captureReference(repo, outCommitted, {})).rejects.toThrow(/dirty/);
			expect(existsSync(outCommitted)).toBe(false);
			const outStagedDirty = join(PRODUCER_TEST_ROOT, "never-created-staged");
			await expect(captureReference(repo, outStagedDirty, { inputMode: "staged" })).rejects.toThrow();
			expect(existsSync(outStagedDirty)).toBe(false);
			const outDirty = ownedOutDir("out-dirty");
			const dirty = await captureReference(repo, outDirty, { allowDirtyRelevant: true });
			expect(dirty.relevantTreeStatus).toContain("main.ts");
			expect(dirty.stagedInputProvenance).toBeUndefined();
			expect(dirty.captureHead).toBe(head);
			expect(loadReferenceBundle(outDirty).manifest.relevantTreeStatus).toBe(dirty.relevantTreeStatus);
		},
		PRODUCER_TIMEOUT_MS,
	);

	// Case 14 (ordering): combined modes fail before any Git/build/output work.
	test("combined staged + dirty modes reject before any git, build, or output mutation", async () => {
		const missingRoot = join(PRODUCER_TEST_ROOT, "nonexistent-root");
		const out = join(PRODUCER_TEST_ROOT, "never-created-combined");
		const invalid: unknown = { inputMode: "staged", allowDirtyRelevant: true };
		// A nonexistent root would fail in Git if the mode guard did not run first.
		await expect(captureReference(missingRoot, out, invalid as CaptureOptions)).rejects.toThrow(
			/incompatible/,
		);
		expect(existsSync(out)).toBe(false);
	});

	// Case 15: repeated same-process captures use fresh authority exports —
	// transitive (names.ts, reached only through targets.ts) and top-level
	// (targets.ts, host.ts) — and every run gets a unique build scratch whose
	// real path is recorded in the captured argv.
	test(
		"repeated same-process captures see fresh top-level and transitive authority exports with unique scratch",
		async () => {
			const repo = newExposureRepo("fresh");
			const first = await captureReference(repo, ownedOutDir("fresh-1"), { inputMode: "staged" });
			expect(first.metafile.argv[first.metafile.argv.indexOf("--target") + 1]).toBe("bun");

			// Transitive: names.ts is reachable only through targets.ts.
			writeFixtureFile(repo, "scripts/release/names.ts", FIXTURE_NAMES_TS("pi-extension-host-v2"));
			fixtureGit(repo, ["add", "scripts/release/names.ts"]);
			const second = await captureReference(repo, ownedOutDir("fresh-2"), { inputMode: "staged" });
			const outfile2 = second.metafile.argv[second.metafile.argv.indexOf("--outfile") + 1];
			expect(outfile2).toContain("pi-extension-host-v2");

			// Top-level: targets.ts supplies the --target value.
			writeFixtureFile(repo, "scripts/release/targets.ts", FIXTURE_TARGETS_TS("node"));
			fixtureGit(repo, ["add", "scripts/release/targets.ts"]);
			const third = await captureReference(repo, ownedOutDir("fresh-3"), { inputMode: "staged" });
			expect(third.metafile.argv[third.metafile.argv.indexOf("--target") + 1]).toBe("node");

			// Top-level: host.ts supplies the remaining argv shape.
			writeFixtureFile(repo, "scripts/release/host.ts", FIXTURE_HOST_TS(["--minify"]));
			fixtureGit(repo, ["add", "scripts/release/host.ts"]);
			const fourth = await captureReference(repo, ownedOutDir("fresh-4"), { inputMode: "staged" });
			expect(fourth.metafile.argv).toContain("--minify");

			const scratchDirs = [first, second, third, fourth].map(capturedMetafileDir);
			expect(new Set(scratchDirs).size).toBe(4);
		},
		PRODUCER_TIMEOUT_MS,
	);

	// Case 15 (publication): two captures into one outDir never share a
	// metafile.json scratch path, while identical cargo-projection bytes share
	// one immutable name through the no-replace link + byte-check path.
	test(
		"two captures into one outDir share identical immutable projection bytes and never share metafile scratch",
		async () => {
			const repo = newExposureRepo("shared");
			const out = ownedOutDir("shared-out");
			const a = await captureReference(repo, out, { inputMode: "staged" });
			const b = await captureReference(repo, out, { inputMode: "staged" });
			// Unique build scratch per run, recorded in the real argv.
			expect(capturedMetafileDir(a)).not.toBe(capturedMetafileDir(b));
			// The cargo projection has no run-specific content: identical bytes,
			// identical hash name — the second install takes the existing-file
			// byte-check path instead of overwriting.
			expect(a.cargo.projectionPath).toBe(b.cargo.projectionPath);
			// The metafile projection embeds the run-specific argv, so the two
			// runs can never collide on one name.
			expect(a.metafile.projectionPath).not.toBe(b.metafile.projectionPath);
			// Whichever manifest won the rename, the selected bundle is complete.
			const loaded = loadReferenceBundle(out);
			expect([a.capturedAt, b.capturedAt]).toContain(loaded.manifest.capturedAt);
		},
		PRODUCER_TIMEOUT_MS,
	);

	// Case 16: old fixed-name and new hash-named bundles both load through the
	// manifest-selected basenames.
	test(
		"old fixed-name and new hash-named bundles both load through manifest-selected names",
		async () => {
			const repo = newExposureRepo("names");
			const out = ownedOutDir("names-out");
			const manifest = await captureReference(repo, out, { inputMode: "staged" });
			const bundle = loadReferenceBundle(out);
			expect(bundle.manifest.metafile.sha256).toBe(manifest.metafile.sha256);

			// A pre-migration bundle: the same real projections under the fixed
			// basenames, selected by a manifest that names them.
			const legacy = ownedOutDir("legacy-out");
			const parsed = parseReferenceManifest(readFileSync(join(out, "reference.json"), "utf8"), "captured");
			const legacyManifest = {
				...parsed,
				metafile: { ...parsed.metafile, projectionPath: "metafile-projection.json" },
				cargo: { ...parsed.cargo, projectionPath: "cargo-graph-projection.json" },
			};
			copyFileSync(join(out, manifest.metafile.projectionPath), join(legacy, "metafile-projection.json"));
			copyFileSync(join(out, manifest.cargo.projectionPath), join(legacy, "cargo-graph-projection.json"));
			writeFileSync(join(legacy, "reference.json"), `${JSON.stringify(legacyManifest, null, "\t")}\n`);
			const legacyBundle = loadReferenceBundle(legacy);
			expect(legacyBundle.metafile.metafileSha256).toBe(bundle.metafile.metafileSha256);
			expect(legacyBundle.cargoGraph.workspaceMembers).toEqual(bundle.cargoGraph.workspaceMembers);
		},
		PRODUCER_TIMEOUT_MS,
	);

	// Case 16 (caller migration): the self-check copy/tamper mechanism follows
	// manifest-selected basenames, so it works unchanged on a hash-named
	// bundle. The canonical self-check test above exercises the integrated
	// path on whatever names the checked-in manifest selects.
	test(
		"the self-check copy/tamper mechanism follows manifest-selected basenames on a hash-named bundle",
		async () => {
			const repo = newExposureRepo("selfcheck-path");
			const out = ownedOutDir("selfcheck-out");
			const manifest = await captureReference(repo, out, { inputMode: "staged" });
			const probe = ownedOutDir("selfcheck-probe");
			for (const name of [
				"reference.json",
				manifest.metafile.projectionPath,
				manifest.cargo.projectionPath,
			]) {
				copyFileSync(join(out, name), join(probe, name));
			}
			// The copy must be a complete valid bundle first: a missing
			// manifest-selected file would fail for the wrong reason and could
			// masquerade as tamper detection.
			expect(loadReferenceBundle(probe).manifest.captureHead).toBe(manifest.captureHead);
			const cargoPath = join(probe, manifest.cargo.projectionPath);
			writeFileSync(cargoPath, `${readFileSync(cargoPath, "utf8")}\n`);
			expect(() => loadReferenceBundle(probe)).toThrow(/hash-chain/);
			expect(loadReferenceBundle(out).manifest.captureHead).toBe(manifest.captureHead);
		},
		PRODUCER_TIMEOUT_MS,
	);

	// Case 17: a real producer failure (the authority bun build rejects a
	// staged syntax-error entrypoint) leaves the old canonical selection
	// byte-for-byte intact with no new files.
	test(
		"a producer build failure preserves the old canonical selection byte-for-byte",
		async () => {
			const repo = newExposureRepo("producer-fail");
			const out = ownedOutDir("producer-fail-out");
			const first = await captureReference(repo, out, { inputMode: "staged" });
			const referenceBefore = readFileSync(join(out, "reference.json"), "utf8");

			writeFixtureFile(repo, "packages/extension-host/src/main.ts", "export const broken = ;\n");
			fixtureGit(repo, ["add", "packages/extension-host/src/main.ts"]);
			await expect(captureReference(repo, out, { inputMode: "staged" })).rejects.toThrow(/bun build/);

			expect(readFileSync(join(out, "reference.json"), "utf8")).toBe(referenceBefore);
			expect(readdirSync(out).sort()).toEqual(
				["reference.json", first.metafile.projectionPath, first.cargo.projectionPath].sort(),
			);
			expect(loadReferenceBundle(out).manifest.captureHead).toBe(first.captureHead);
		},
		PRODUCER_TIMEOUT_MS,
	);

	// Case 17 (race barrier): the fixture authority module parks inside the
	// real capture-argv subprocess — after the producer's input inventory,
	// before both postchecks — while the parent mutates a relevant file. The
	// mutation is ordered by the handshake, not by timing.
	test(
		"a controlled mid-capture relevant mutation refuses and preserves the old selection",
		async () => {
			const repo = newExposureRepo("race");
			const out = ownedOutDir("race-out");
			const first = await captureReference(repo, out, { inputMode: "staged" });
			const referenceBefore = readFileSync(join(out, "reference.json"), "utf8");

			// A staged change gives the capture real work; the barriered child
			// then mutates a different relevant file's worktree bytes while the
			// authority module is parked inside the real producer invocation.
			writeFixtureFile(
				repo,
				"packages/extension-host/package.json",
				'{"name":"fixture-extension-host","version":"0.0.0","description":"staged"}\n',
			);
			fixtureGit(repo, ["add", "packages/extension-host/package.json"]);
			const target = join(repo, "packages/extension-host/src/main.ts");
			const result = await runBarrieredCapture({
				repo,
				outDir: out,
				mode: "staged",
				workDir: ownedOutDir("race-barrier"),
				barrier: "authority",
				onParked: (announcement) => {
					expect(announcement).toBe("authority-parked");
					appendFileSync(target, "\n// controlled mid-capture mutation\n");
				},
			});
			expect(result.ok).toBe(false);
			expect(result.error).toMatch(/-race/);

			expect(readFileSync(join(out, "reference.json"), "utf8")).toBe(referenceBefore);
			const reloaded = loadReferenceBundle(out);
			expect(reloaded.manifest.captureHead).toBe(first.captureHead);
			expect(reloaded.manifest.metafile.projectionPath).toBe(first.metafile.projectionPath);
			expect(reloaded.manifest.cargo.projectionPath).toBe(first.cargo.projectionPath);
		},
		PRODUCER_TIMEOUT_MS,
	);

	// Case 17 (collision): the cargo projection name is a pure function of the
	// repository state, so a conflicting file can be placed at the exact
	// destination a later capture will compute. The no-replace install must
	// refuse rather than overwrite, leaving the old selection intact.
	test(
		"a conflicting immutable hash destination refuses without replacing the old selection",
		async () => {
			const repo = newExposureRepo("collision");
			const outOld = ownedOutDir("collision-old");
			const old = await captureReference(repo, outOld, { inputMode: "staged" });

			// New cargo-graph generation: learn its deterministic projection name
			// from a real capture into a scratch directory.
			addSecondFixtureCrate(repo);
			const outProbe = ownedOutDir("collision-probe");
			const probe = await captureReference(repo, outProbe, { inputMode: "staged" });
			const newCargoName = probe.cargo.projectionPath;
			expect(newCargoName).not.toBe(old.cargo.projectionPath);

			// The target directory holds the complete OLD selection plus a
			// conflicting file at the name the next capture will compute.
			const out = ownedOutDir("collision-out");
			copyFileSync(join(outOld, "reference.json"), join(out, "reference.json"));
			copyFileSync(join(outOld, old.metafile.projectionPath), join(out, old.metafile.projectionPath));
			copyFileSync(join(outOld, old.cargo.projectionPath), join(out, old.cargo.projectionPath));
			const conflictBytes = '{"conflicting":true}\n';
			writeFileSync(join(out, newCargoName), conflictBytes);

			await expect(captureReference(repo, out, { inputMode: "staged" })).rejects.toThrow(
				/immutable projection/,
			);

			// Old reference and its selected projections are intact and loadable;
			// the conflicting residue was neither overwritten nor selected.
			const reloaded = loadReferenceBundle(out);
			expect(reloaded.manifest.captureHead).toBe(old.captureHead);
			expect(reloaded.manifest.cargo.projectionPath).toBe(old.cargo.projectionPath);
			expect(readFileSync(join(out, newCargoName), "utf8")).toBe(conflictBytes);
			// Residue allowed by design: only the freshly installed (unselected)
			// metafile projection, a content-addressed immutable blob. Run-owned
			// paths — the publish-staging directory included — are swept by the
			// capture's centralized cleanup even on this failure path.
			const residue = readdirSync(out, { withFileTypes: true }).filter(
				(entry) =>
					entry.name !== "reference.json" &&
					entry.name !== old.metafile.projectionPath &&
					entry.name !== old.cargo.projectionPath &&
					entry.name !== newCargoName,
			);
			for (const entry of residue) {
				expect(entry.isDirectory()).toBe(false);
				expect(entry.name).toMatch(/^metafile-projection\.[0-9a-f]{64}\.json$/);
				expect(reloaded.manifest.metafile.projectionPath).not.toBe(entry.name);
			}
		},
		PRODUCER_TIMEOUT_MS,
	);

	// Case 17 (final rename): a pre-existing directory at the canonical
	// reference.json path makes the kernel refuse the final selection rename
	// with EISDIR — a real filesystem failure at the commit point, not an
	// injected hook. The run-owned publish-staging directory and prepared
	// reference temp must be swept, and the prior state must survive
	// byte-for-byte alongside the installed immutable projections.
	test(
		"a filesystem failure at the final canonical rename leaves no run-owned staging residue",
		async () => {
			const repo = newExposureRepo("commit-rename-fail");
			const out = ownedOutDir("commit-rename-fail-out");

			// Prior published state: a directory occupies the exact path the
			// canonical selection rename must replace.
			const sentinel = "prior-state-sentinel\n";
			const priorDir = join(out, "reference.json");
			mkdirSync(priorDir, { recursive: true });
			writeFileSync(join(priorDir, "prior-state"), sentinel);

			await expect(captureReference(repo, out, { inputMode: "staged" })).rejects.toThrow(/EISDIR/);

			// The failure happened at the kernel rename itself: the prior state
			// is intact and was never replaced.
			expect(statSync(priorDir).isDirectory()).toBe(true);
			expect(readFileSync(join(priorDir, "prior-state"), "utf8")).toBe(sentinel);

			// No run-owned residue from this invocation: no publish-staging
			// directory, no prepared reference temp. (The build scratch lives in
			// the shared tmpdir and is swept by the same registry.)
			const names = readdirSync(out);
			for (const name of names) {
				expect(name).not.toMatch(/^exposure-staging-/);
				expect(name).not.toMatch(/^reference-staging-/);
			}

			// Installed immutable outputs are preserved: the freshly installed,
			// unselected projections remain as content-addressed blobs beside
			// the refused selection.
			const blobNames = names.filter((name) => name !== "reference.json");
			expect(blobNames.length).toBe(2);
			for (const name of blobNames) {
				expect(name).toMatch(/^(?:metafile-projection|cargo-graph-projection)\.[0-9a-f]{64}\.json$/);
			}
		},
		PRODUCER_TIMEOUT_MS,
	);

	// Case 17 (malformed projection): a transparent `bun` shim runs the real
	// authority build, then parks so the parent can corrupt the real produced
	// metafile before the producer reads it. The capture must fail on its own
	// malformed-input guard and preserve the old selection — the corruption
	// targets the producer's real artifact, never a fabricated result.
	test(
		"a malformed produced artifact fails the capture and preserves the old selection",
		async () => {
			const repo = newExposureRepo("malformed");
			const out = ownedOutDir("malformed-out");
			const first = await captureReference(repo, out, { inputMode: "staged" });
			const referenceBefore = readFileSync(join(out, "reference.json"), "utf8");

			const workDir = ownedOutDir("malformed-barrier");
			const result = await runBarrieredCapture({
				repo,
				outDir: out,
				mode: "staged",
				workDir,
				barrier: "build",
				extraEnv: writeBunShim(workDir),
				onParked: (announcement) => {
					// The wrapper reports the real artifact path the real
					// `bun build` just wrote inside the producer's scratch.
					expect(announcement.startsWith("built ")).toBe(true);
					const metafilePath = JSON.parse(announcement.slice("built ".length)) as string;
					expect(metafilePath).toContain("exposure-capture-");
					writeFileSync(metafilePath, "{ malformed json\n");
				},
			});
			expect(result.ok).toBe(false);
			expect(result.error).toMatch(/metafile\.json/);

			expect(readFileSync(join(out, "reference.json"), "utf8")).toBe(referenceBefore);
			expect(readdirSync(out).sort()).toEqual(
				["reference.json", first.metafile.projectionPath, first.cargo.projectionPath].sort(),
			);
			expect(loadReferenceBundle(out).manifest.captureHead).toBe(first.captureHead);
		},
		PRODUCER_TIMEOUT_MS,
	);

	// Case 18: after a new publication, a reader holding the old manifest
	// still loads its complete generation from the retained immutable blobs,
	// and unselected blobs in the directory are never selected.
	test(
		"a reader holding the old manifest retains its complete generation after a new publication",
		async () => {
			const repo = newExposureRepo("generations");
			const out = ownedOutDir("generations-out");
			const old = await captureReference(repo, out, { inputMode: "staged" });
			const oldReferenceText = readFileSync(join(out, "reference.json"), "utf8");

			addSecondFixtureCrate(repo);
			const current = await captureReference(repo, out, { inputMode: "staged" });
			expect(current.cargo.projectionPath).not.toBe(old.cargo.projectionPath);
			expect(current.metafile.projectionPath).not.toBe(old.metafile.projectionPath);

			// The old generation's selected bytes are retained in place.
			expect(sha256FileAt(join(out, old.metafile.projectionPath))).toBe(old.metafile.sha256);
			expect(sha256FileAt(join(out, old.cargo.projectionPath))).toBe(old.cargo.sha256);

			// A reader holding the old manifest still resolves its complete
			// generation from the same directory after the swap.
			const readerDir = ownedOutDir("generations-reader");
			writeFileSync(join(readerDir, "reference.json"), oldReferenceText);
			copyFileSync(join(out, old.metafile.projectionPath), join(readerDir, old.metafile.projectionPath));
			copyFileSync(join(out, old.cargo.projectionPath), join(readerDir, old.cargo.projectionPath));
			const oldBundle = loadReferenceBundle(readerDir);
			expect(oldBundle.manifest.captureHead).toBe(old.captureHead);
			expect(oldBundle.cargoGraph.workspaceMembers).toEqual(["fixture-crate"]);

			// The new selection is complete and never mixes generations.
			const currentBundle = loadReferenceBundle(out);
			expect(currentBundle.manifest.cargo.projectionPath).toBe(current.cargo.projectionPath);
			expect(currentBundle.cargoGraph.workspaceMembers).toEqual(["fixture-crate", "second-crate"]);

			// A complete but unselected blob in the directory is never selected:
			// the loader follows only the names reference.json pins.
			const foreignName = `cargo-graph-projection.${"0".repeat(63)}1.json`;
			copyFileSync(join(out, old.cargo.projectionPath), join(out, foreignName));
			expect(loadReferenceBundle(out).manifest.cargo.projectionPath).toBe(current.cargo.projectionPath);
		},
		PRODUCER_TIMEOUT_MS,
	);

	// Case 19 (dirty race): a dep-free relevant file stays porcelain `M`
	// while its bytes change during the capture — only the observation's
	// recorded working bytes can catch it, and the old canonical selection
	// must remain intact.
	test(
		"the dep-field-free dirty observation refuses when the dirty file's bytes change mid-capture",
		async () => {
			const repo = newExposureRepo("dirty-race");
			const out = ownedOutDir("dirty-race-out");
			const first = await captureReference(repo, out, {});
			const referenceBefore = readFileSync(join(out, "reference.json"), "utf8");

			// Dep-free relevant dirt: a source-only edit, left unstaged. The
			// second mutation appends to the already-`M` file, so the porcelain
			// status is identical before and after — status alone cannot
			// detect it.
			const target = join(repo, "packages/extension-host/src/main.ts");
			writeFixtureFile(repo, "packages/extension-host/src/main.ts", 'export const fixtureHost = "dirty";\n');
			const result = await runBarrieredCapture({
				repo,
				outDir: out,
				mode: "dirty",
				workDir: ownedOutDir("dirty-race-barrier"),
				barrier: "authority",
				onParked: (announcement) => {
					expect(announcement).toBe("authority-parked");
					appendFileSync(target, "\n// second mutation: status stays M, bytes change\n");
				},
			});
			expect(result.ok).toBe(false);
			expect(result.error).toMatch(/-race/);

			expect(readFileSync(join(out, "reference.json"), "utf8")).toBe(referenceBefore);
			expect(loadReferenceBundle(out).manifest.captureHead).toBe(first.captureHead);
		},
		PRODUCER_TIMEOUT_MS,
	);
	// Vendored provenance: an unstaged tracked vendored source edit refuses a
	// staged re-capture inside the staged-input helper; staging the same edit
	// re-captures successfully with that file's pin changed.
	test(
		"vendored source mutation refuses staged recapture until staged, then changes the pin",
		async () => {
			const repo = newExposureRepo("vendored");

			// Add an approved vendored path crate and wire it through [patch.crates-io].
			writeFixtureFile(
				repo,
				"vendor/foreign-0.1.0/Cargo.toml",
				`[package]\nname = "foreign"\nversion = "0.1.0"\nedition = "2021"\n`,
			);
			writeFixtureFile(repo, "vendor/foreign-0.1.0/src/lib.rs", "pub fn foreign() {}\n");
			writeFixtureFile(repo, "vendor/foreign-0.1.0/VENDORED.txt", "Vendored dependency: foreign 0.1.0\n");
			writeFixtureFile(repo, "vendor/foreign-0.1.0/LICENSE", "MIT\n");

			const rootCargo = readFileSync(join(repo, "Cargo.toml"), "utf8");
			writeFixtureFile(
				repo,
				"Cargo.toml",
				`${rootCargo.trimEnd()}\nexclude = ["vendor"]\n\n[patch.crates-io]\nforeign = { path = "vendor/foreign-0.1.0" }\n`,
			);
			const crateToml = fixtureCrateToml("fixture-crate").trimEnd();
			writeFixtureFile(
				repo,
				"crates/fixture-crate/Cargo.toml",
				`${crateToml}\n\n[dependencies]\nforeign = "0.1.0"\n`,
			);

			generateFixtureLockfile(repo);
			fixtureGit(repo, ["add", "."]);
			fixtureGit(repo, ["commit", "-q", "--no-verify", "-m", "add vendor"]);

			const out = ownedOutDir("vendored-out");
			const first = await captureReference(repo, out, { inputMode: "staged" });
			const libPin = (path: string): string | undefined =>
				first.vendorFiles?.find((pin) => pin.path === path)?.sha256;
			const before = libPin("vendor/foreign-0.1.0/src/lib.rs");
			expect(before).toBeDefined();

			// An unstaged vendored source edit is rejected by the status guard
			// before any byte observation runs.
			writeFixtureFile(repo, "vendor/foreign-0.1.0/src/lib.rs", "pub fn changed() {}\n");
			await expect(captureReference(repo, out, { inputMode: "staged" })).rejects.toThrow(
				/unstaged worktree changes/,
			);

			// Staging the same edit makes the re-capture legal, and the changed
			// bytes must move that file's pin.
			fixtureGit(repo, ["add", "vendor/foreign-0.1.0/src/lib.rs"]);
			const second = await captureReference(repo, out, { inputMode: "staged" });
			const after = second.vendorFiles?.find((pin) => pin.path === "vendor/foreign-0.1.0/src/lib.rs")?.sha256;
			expect(after).toBeDefined();
			expect(after).not.toBe(before);
		},
		PRODUCER_TIMEOUT_MS,
	);

});

