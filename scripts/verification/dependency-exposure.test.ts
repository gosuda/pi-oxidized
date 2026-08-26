import { describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import {
	CANONICAL_REFERENCE_DIR,
	ExposureError,
	REPO_ROOT,
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

	verdictFromChecks,
	checkAuthorityIntegrity,
	pass,
	fail,
	undecidable,
	type CargoGraphProjection,
	type NpmSurface,
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
		expect(resolveInputOwner("../../.references/pi/node_modules/typebox/build/index.mjs")).toBe("typebox");
		expect(resolveInputOwner("a/node_modules/@scope/pkg/src/x.ts")).toBe("@scope/pkg");
		expect(resolveInputOwner("packages/extension-host/src/main.ts")).toBeNull();
		expect(resolveInputOwner("node_modules/a/node_modules/b/i.js")).toBe("b");
	});

	test("reachable npm subject fails; zero-reachability passes; unrelated names are not substring-matched", () => {
		const inputs = [
			"../../.references/pi/node_modules/typebox/build/index.mjs",
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
