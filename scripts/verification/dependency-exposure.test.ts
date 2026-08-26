import { afterEach, describe, expect, test } from "bun:test";
import { existsSync, mkdtempSync, mkdirSync, readFileSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { join, resolve, sep } from "node:path";

import { HOST_PACKAGE_DIR } from "../release/host.ts";
import { RecordingRunner, SpawnRunner } from "../release/runner.ts";
import {
	REPO_ROOT,
	SCHEMA,
	SENTINEL_FAILED,
	appendLedgerRows,
	assertRelevantWorktreeClean,
	buildBundleEvidence,
	cargoLockChangedNames,
	classify,
	deriveAutoFromTexts,
	escapeMarkdownCell,
	evaluateCargoKinds,
	evaluateMetafileReachability,
	evaluateNpmMembership,
	evaluateSideAwareReachability,
	evaluateStaging,
	expandScripts,
	expandWithRealpaths,
	findLifecycleScripts,
	findUnknownCompileSites,
	fingerprintHeadEvidence,
	inputMatchesMetafile,
	listCargoTomlPaths,
	listHeadEvidencePaths,
	materializeBase,
	parseCargoLockIdentities,
	parseCargoMetadata,
	parseInputSpec,
	parseMetafile,
	parseNpmSurface,
	resolvePathPair,
	scanCorpusText,
	selfTest,
	verdictFromChecks,
} from "./dependency-exposure.ts";

const temps: string[] = [];

afterEach(() => {
	while (temps.length > 0) {
		const path = temps.pop();
		if (path !== undefined) rmSync(path, { recursive: true, force: true });
	}
});

function tempDir(prefix: string): string {
	const scratch = join(REPO_ROOT, "target", "deps-r2-tmp");
	mkdirSync(scratch, { recursive: true });
	const path = mkdtempSync(join(scratch, prefix));
	temps.push(path);
	return path;
}

function surface(path: string, deps: Record<string, Record<string, string>>) {
	return parseNpmSurface(path, JSON.stringify(deps));
}

describe("dependency-exposure unit checks", () => {
	test("M1: dependencies-field membership fails E1", () => {
		const base = [surface("packages/extension-host/package.json", {
			dependencies: { typebox: "1.0.0" },
			devDependencies: {},
		})];
		const head = [surface("packages/extension-host/package.json", {
			dependencies: {},
			devDependencies: { typebox: "1.0.0" },
		})];
		expect(evaluateNpmMembership("typebox", base, head).status).toBe("fail");
	});

	test("M2: cargo kind union fails on null/build and passes cfg-gated dev-only", () => {
		const mixed = parseCargoMetadata(JSON.stringify({
			packages: [{ id: "tempfile 3.0.0", name: "tempfile", manifest_path: "/tmp/Cargo.toml" }],
			resolve: {
				nodes: [{
					deps: [{
						pkg: "tempfile 3.0.0",
						dep_kinds: [{ kind: "dev" }, { kind: null }],
					}],
				}],
			},
		}));
		expect(evaluateCargoKinds("tempfile", mixed, mixed).status).toBe("fail");

		const buildOnly = parseCargoMetadata(JSON.stringify({
			packages: [{ id: "cc 1.0.0", name: "cc", manifest_path: "/tmp/Cargo.toml" }],
			resolve: {
				nodes: [{
					deps: [{
						pkg: "cc 1.0.0",
						dep_kinds: [{ kind: "build" }],
					}],
				}],
			},
		}));
		expect(evaluateCargoKinds("cc", buildOnly, buildOnly).status).toBe("fail");

		const cfgDev = parseCargoMetadata(JSON.stringify({
			packages: [{ id: "insta 1.0.0", name: "insta", manifest_path: "/tmp/Cargo.toml" }],
			resolve: {
				nodes: [{
					deps: [{
						pkg: "insta 1.0.0",
						dep_kinds: [{ kind: "dev", target: "cfg(unix)" }],
					}],
				}],
			},
		}));
		expect(evaluateCargoKinds("insta", cfgDev, cfgDev).status).toBe("pass");
	});

	test("M3: metafile prefix matching is segment-boundaried", () => {
		expect(inputMatchesMetafile("node_modules/typebox/index.js", ["node_modules/typebox/"])).toBe(true);
		expect(inputMatchesMetafile("node_modules/typeboxx/index.js", ["node_modules/typebox/"])).toBe(false);
		expect(inputMatchesMetafile("../../.references/pi/node_modules/typebox/build/index.mjs", ["node_modules/typebox/"])).toBe(true);
		expect(
			evaluateMetafileReachability("typebox", ["node_modules/typebox/"], [{
				side: "base",
				name: "compiled",
				mode: "compiled",
				inputs: ["node_modules/typeboxx/index.js"],
			}]).status,
		).toBe("pass");
		expect(
			evaluateMetafileReachability("typebox", ["node_modules/typebox/"], [{
				side: "base",
				name: "compiled",
				mode: "compiled",
				inputs: ["node_modules/typebox/index.js"],
			}]).status,
		).toBe("fail");
	});

	test("M4: before-only reachability fails E2", () => {
		const result = evaluateMetafileReachability("typebox", ["node_modules/typebox/"], [
			{
				side: "base",
				name: "compiled",
				mode: "compiled",
				inputs: ["node_modules/typebox/index.js"],
			},
			{
				side: "head",
				name: "compiled",
				mode: "compiled",
				inputs: ["src/main.ts"],
			},
		]);
		expect(result.status).toBe("fail");
	});

	test("M5: quoted ignore fails while prose ignore passes", () => {
		expect(
			scanCorpusText("ignore", [], [{ path: "scripts/release/host.ts", text: 'import "ignore";' }]).status,
		).toBe("fail");
		expect(
			scanCorpusText("ignore", [], [{ path: "scripts/release/host.ts", text: "please ignore this note" }]).status,
		).toBe("pass");
	});

	test("M6: transitive package-script resolution catches tsc", () => {
		const scripts = new Map([["check", "tsc --noEmit -p tsconfig.check.json"]]);
		const expanded = expandScripts(
			[{ path: "scripts/release/host.ts", text: 'await runner.run("bun", ["run", "check"]);' }],
			scripts,
		);
		expect(scanCorpusText("typescript", ["tsc"], expanded).status).toBe("fail");
	});

	test("M7: staging intersection fails E4", () => {
		const staged = ["/repo/crates/pi/assets/theme/dark.css"];
		expect(evaluateStaging(["/repo/crates/pi/assets"], staged).status).toBe("fail");
		expect(evaluateStaging(["/repo/node_modules/typebox"], staged).status).toBe("pass");
	});

	test("M8: unknown compile witness makes E2 undecidable via scan", async () => {
		const root = tempDir("pi-deps-r2-m8-");
		mkdirSync(join(root, "scripts"), { recursive: true });
		writeFileSync(join(root, "scripts", "rogue.ts"), 'console.log("bun build --compile");\n');
		const unknown = await findUnknownCompileSites(root);
		expect(unknown).toContain("scripts/rogue.ts");
	});

	test("M9: runner failures map to undecidable Class S", async () => {
		const cargoReject = new RecordingRunner((call) => {
			if (call.command === "cargo") {
				return { exitCode: 1, stdout: "", stderr: "metadata boom" };
			}
			return { exitCode: 0, stdout: "", stderr: "" };
		});
		const cargo = await classify({
			base: "HEAD",
			inputs: ["cargo:serde"],
			repoRoot: REPO_ROOT,
			runner: cargoReject,
			identity: true,
			now: () => new Date("2026-08-26T00:00:00.000Z"),
		});
		expect(cargo.failedClosed).toBe(true);
		expect(cargo.report.verdicts[0]?.class).toBe("S");
		expect(cargo.report.verdicts[0]?.checks.E1.status).toBe("undecidable");

		const buildReject = new RecordingRunner((call) => {
			if (call.command === "bun" && call.args[0] === "build") {
				return { exitCode: 1, stdout: "", stderr: "build boom" };
			}
			return { exitCode: 0, stdout: "", stderr: "" };
		});
		const npm = await classify({
			base: "HEAD",
			inputs: ["npm:@types/bun"],
			repoRoot: REPO_ROOT,
			runner: buildReject,
			identity: true,
			now: () => new Date("2026-08-26T00:00:00.000Z"),
		});
		expect(npm.failedClosed).toBe(true);
		expect(npm.report.verdicts[0]?.class).toBe("S");
		expect(npm.report.verdicts[0]?.checks.E2.status).toBe("undecidable");
	});

	test("M10: auto derivation and malformed lock fail closed", () => {
		const derived = deriveAutoFromTexts({
			npmBefore: [
				JSON.stringify({ dependencies: {}, devDependencies: { a: "1.0.0" } }),
				JSON.stringify({ dependencies: { typebox: "1.0.0" }, devDependencies: {} }),
				JSON.stringify({ dependencies: {}, devDependencies: {} }),
			],
			npmAfter: [
				JSON.stringify({ dependencies: {}, devDependencies: { a: "1.0.1" } }),
				JSON.stringify({ dependencies: { typebox: "1.0.0" }, devDependencies: {} }),
				JSON.stringify({ dependencies: {}, devDependencies: {} }),
			],
			bunBefore: ['{"packages":{"leftpad":["leftpad@1.0.0"]}}'],
			bunAfter: ['{"packages":{"leftpad":["leftpad@1.0.1"]}}'],
			cargoBefore: '[[package]]\nname = "serde"\nversion = "1.0.0"\n',
			cargoAfter: '[[package]]\nname = "serde"\nversion = "1.0.1"\n',
			toolBefore: {
				"bun-runtime": ["export const BUN_RUNTIME_VERSION = \"1.3.14\";"],
				"rust-toolchain": ["1.97.1", "edition"],
				"bun-bundler": ["bun-version: 1.3.14"],
			},
			toolAfter: {
				"bun-runtime": ["export const BUN_RUNTIME_VERSION = \"1.3.15\";"],
				"rust-toolchain": ["1.97.1", "edition"],
				"bun-bundler": ["bun-version: 1.3.14"],
			},
		});
		expect(derived).toContain("npm:a");
		expect(derived).toContain("npm:leftpad");
		expect(derived).toContain("cargo:serde");
		expect(derived).toContain("tool:bun-runtime");
		expect(() =>
			deriveAutoFromTexts({
				npmBefore: ["{}", "{}", "{}"],
				npmAfter: ["{}", "{}", "{}"],
				bunBefore: ["not-json"],
				bunAfter: ["{}"],
				cargoBefore: "[[package]]\nname = \"serde\"\nversion = \"1.0.0\"\n",
				cargoAfter: "[[package]]\nname = \"serde\"\nversion = \"1.0.0\"\n",
				toolBefore: {},
				toolAfter: {},
			})
		).toThrow();
	});

	test("Class E requires four explicit passes and schema stays complete", () => {
		const e = verdictFromChecks("npm:@types/bun", {
			E1: { status: "pass", detail: "ok" },
			E2: { status: "pass", detail: "ok" },
			E3: { status: "pass", detail: "ok" },
			E4: { status: "pass", detail: "ok" },
		});
		expect(e.class).toBe("E");
		const s = verdictFromChecks("npm:typebox", {
			E1: { status: "fail", detail: "deps" },
			E2: { status: "pass", detail: "ok" },
			E3: { status: "pass", detail: "ok" },
			E4: { status: "pass", detail: "ok" },
		});
		expect(s.class).toBe("S");
		expect(parseMetafile(JSON.stringify({ inputs: { "src/main.ts": {} } }))).toEqual(["src/main.ts"]);
		expect(parseInputSpec("tool:bun-runtime").kind).toBe("tool");
	});
});

describe("dependency-exposure fail-closed integration", () => {
	test("invalid base emits all-undecidable S report and writes --json", async () => {
		const out = join(tempDir("pi-deps-r2-json-"), "report.json");
		const result = await classify({
			base: "definitely-not-a-real-ref-deps-r2",
			inputs: ["npm:typebox", "cargo:serde"],
			repoRoot: REPO_ROOT,
			now: () => new Date("2026-08-26T00:00:00.000Z"),
		});
		expect(result.failedClosed).toBe(true);
		expect(result.report.schema).toBe(SCHEMA);
		expect(result.report.overall).toBe("S");
		for (const verdict of result.report.verdicts) {
			expect(verdict.class).toBe("S");
			expect(verdict.checks.E1.status).toBe("undecidable");
			expect(verdict.checks.E2.status).toBe("undecidable");
			expect(verdict.checks.E3.status).toBe("undecidable");
			expect(verdict.checks.E4.status).toBe("undecidable");
		}
		writeFileSync(out, JSON.stringify(result.report, null, 2));
		expect(JSON.parse(readFileSync(out, "utf8")).schema).toBe(SCHEMA);
	});

	test("--record appends fail-closed undecidable Class S and rejects only structural invalidity", async () => {
		const ledger = join(tempDir("pi-deps-r2-ledger-"), "DEPS_INVARIANT_LEDGER.md");
		writeFileSync(ledger, "| Date (UTC) | Change | Base→Head | Input | Class | E1 | E2 | E3 | E4 | Evidence sha256 |\n");
		const failed = {
			schema: SCHEMA,
			decidedAt: "2026-08-26T00:00:00.000Z",
			base: "HEAD",
			head: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
			overall: "S" as const,
			verdicts: [verdictFromChecks("npm:x", {
				E1: { status: "undecidable", detail: "boom" },
				E2: { status: "pass", detail: "ok" },
				E3: { status: "pass", detail: "ok" },
				E4: { status: "pass", detail: "ok" },
			})],
		};
		await appendLedgerRows(ledger, failed);
		const afterFailed = readFileSync(ledger, "utf8");
		expect(afterFailed).toContain("npm:x");
		expect(afterFailed).toContain("| S | undecidable | pass | pass | pass |");
		const ok = {
			schema: SCHEMA,
			decidedAt: "2026-08-26T00:00:00.000Z",
			base: "HEAD",
			head: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
			overall: "E" as const,
			verdicts: [verdictFromChecks("npm:@types/bun", {
				E1: { status: "pass", detail: "ok" },
				E2: { status: "pass", detail: "ok" },
				E3: { status: "pass", detail: "ok" },
				E4: { status: "pass", detail: "ok" },
			})],
		};
		await appendLedgerRows(ledger, ok);
		const text = readFileSync(ledger, "utf8");
		expect(text).toContain("npm:@types/bun");
		expect(text).toContain("| E | pass | pass | pass | pass |");
		await expect(appendLedgerRows(ledger, {
			...ok,
			schema: "not.a.schema" as typeof SCHEMA,
		})).rejects.toThrow(/structurally invalid report schema/);

		const noNl = join(tempDir("pi-deps-r2-ledger-nl-"), "ledger.md");
		writeFileSync(noNl, "| Date (UTC) | Change | Base→Head | Input | Class | E1 | E2 | E3 | E4 | Evidence sha256 |");
		await appendLedgerRows(noNl, failed);
		const normalized = readFileSync(noNl, "utf8");
		expect(normalized.startsWith("| Date (UTC) | Change | Base→Head | Input | Class | E1 | E2 | E3 | E4 | Evidence sha256 |\n| ")).toBe(true);
		expect(normalized).toContain("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
	});

	test("nonzero bun exit rejects even when metafile exists", async () => {
		const root = tempDir("pi-deps-r2-nonzero-");
		mkdirSync(join(root, "packages/extension-host"), { recursive: true });
		writeFileSync(
			join(root, "packages/extension-host/package.json"),
			JSON.stringify({
				scripts: {
					build: "bun build ./src/main.ts --compile --minify --outfile dist/pi-extension-host",
				},
			}),
		);
		const runner = new RecordingRunner((call) => {
			if (call.command === "bun" && call.args[0] === "install") {
				return { exitCode: 0, stdout: "", stderr: "" };
			}
			if (call.command === "bun" && call.args[0] === "build") {
				const metaArg = call.args.find((arg) => arg.startsWith("--metafile="));
				if (metaArg !== undefined) {
					writeFileSync(metaArg.slice("--metafile=".length), JSON.stringify({ inputs: { "src/main.ts": {} } }));
				}
				return { exitCode: 1, stdout: "", stderr: "rename failed despite metafile" };
			}
			return { exitCode: 0, stdout: "", stderr: "" };
		});
		const evidence = await buildBundleEvidence(root, root, runner);
		expect(evidence.error).toBeDefined();
		expect(evidence.error).toMatch(/metafile build failed|rename failed/);
	});

	test("side-aware reachability and staging honor moved file: package roots", () => {
		const baseLoc = {
			packageJson: "/base/packages/old-proto/package.json",
			root: "/base/packages/old-proto",
			aliases: ["/base/packages/old-proto"],
			prefixes: ["../old-proto/", "node_modules/pkg/"],
			bins: [],
		};
		const headLoc = {
			packageJson: "/head/packages/new-proto/package.json",
			root: "/head/packages/new-proto",
			aliases: ["/head/packages/new-proto"],
			prefixes: ["../new-proto/", "node_modules/pkg/"],
			bins: [],
		};
		const movedHit = evaluateSideAwareReachability("pkg", { base: baseLoc, head: headLoc }, [
			{ side: "base", name: "compiled", mode: "compiled", inputs: ["../old-proto/src/index.ts"] },
			{ side: "head", name: "compiled", mode: "compiled", inputs: ["../new-proto/src/index.ts"] },
		]);
		expect(movedHit.status).toBe("fail");
		const crossSideMiss = evaluateSideAwareReachability("pkg", { base: baseLoc, head: headLoc }, [
			{ side: "base", name: "compiled", mode: "compiled", inputs: ["../new-proto/src/index.ts"] },
			{ side: "head", name: "compiled", mode: "compiled", inputs: ["../old-proto/src/index.ts"] },
		]);
		expect(crossSideMiss.status).toBe("pass");
		expect(evaluateSideAwareReachability("pkg", { base: baseLoc }, [
			{ side: "base", name: "compiled", mode: "compiled", inputs: ["../old-proto/src/index.ts"] },
		]).status).toBe("undecidable");
		expect(evaluateStaging([baseLoc.root, headLoc.root], ["/head/packages/new-proto/readme"]).status).toBe("fail");
	});

	test("worktree remove failure fails closed after filesystem cleanup", async () => {
		const real = new SpawnRunner();
		let removedPath: string | undefined;
		const runner = {
			async run(command: string, args: readonly string[], options?: Parameters<SpawnRunner["run"]>[2]) {
				if (command === "git" && args[0] === "status" && args.includes("--porcelain")) {
					return { exitCode: 0, stdout: "", stderr: "" };
				}
				if (command === "git" && args[0] === "worktree" && args[1] === "remove") {
					removedPath = args.includes("--force") ? args[3] : args[2];
					return { exitCode: 1, stdout: "", stderr: "remove refused" };
				}
				if (command === "bun") {
					if (args[0] === "install") return { exitCode: 0, stdout: "", stderr: "" };
					if (args[0] === "build") {
						return { exitCode: 1, stdout: "", stderr: "skip live builds in cleanup test" };
					}
				}
				return real.run(command, args, options);
			},
		};
		const result = await classify({
			base: "HEAD",
			inputs: ["npm:@types/bun"],
			repoRoot: REPO_ROOT,
			runner,
			identity: false,
			now: () => new Date("2026-08-26T00:00:00.000Z"),
		});
		expect(result.failedClosed).toBe(true);
		expect(result.report.overall).toBe("S");
		expect(result.report.verdicts[0]?.checks.E1.status).toBe("undecidable");
		expect(result.report.verdicts[0]?.checks.E1.detail).toMatch(/worktree cleanup failure|remove refused/);
		expect(/^[0-9a-f]{40}$/i.test(result.report.head)).toBe(true);
		expect(removedPath).toBeDefined();
		if (removedPath !== undefined) {
			expect(existsSync(removedPath)).toBe(false);
		}
	});

	test("isolated classification never bun install/builds in the live host package", async () => {
		const liveHost = resolve(join(REPO_ROOT, HOST_PACKAGE_DIR));
		const real = new SpawnRunner();
		const installBuildCwds: string[] = [];
		const runner = {
			async run(command: string, args: readonly string[], options?: Parameters<SpawnRunner["run"]>[2]) {
				if (command === "bun" && (args[0] === "install" || args[0] === "build")) {
					installBuildCwds.push(resolve(options?.cwd ?? ""));
					if (args[0] === "install") return { exitCode: 0, stdout: "", stderr: "" };
					const metaArg = args.find((arg) => arg.startsWith("--metafile="));
					if (metaArg !== undefined) {
						writeFileSync(metaArg.slice("--metafile=".length), JSON.stringify({ inputs: {} }));
					}
					return { exitCode: 0, stdout: "", stderr: "" };
				}
				return real.run(command, args, options);
			},
		};
		const isolated = await materializeBase(REPO_ROOT, "HEAD", runner);
		const isolatedRoot = resolve(isolated.root);
		try {
			await classify({
				base: "HEAD",
				inputs: ["npm:@types/bun"],
				repoRoot: isolated.root,
				runner,
				identity: true,
				now: () => new Date("2026-08-26T00:00:00.000Z"),
			});
		} finally {
			await isolated.cleanup();
		}
		expect(installBuildCwds.length).toBeGreaterThan(0);
		expect(installBuildCwds.every((cwd) => cwd !== liveHost)).toBe(true);
		expect(installBuildCwds.every((cwd) => cwd.startsWith(isolatedRoot))).toBe(true);
	});

	test("live self-test sanity map is hermetic and deterministic", async () => {
		const first = await selfTest(REPO_ROOT);
		const second = await selfTest(REPO_ROOT);
		expect(first).toEqual([]);
		expect(second).toEqual([]);
	}, 1_200_000);
});

describe("dependency-exposure security regressions", () => {
	test("A: install uses --ignore-scripts and lifecycle scripts fail closed", async () => {
		expect(findLifecycleScripts(JSON.stringify({ scripts: { postinstall: "node x.js", build: "bun build" } }))).toEqual([
			"postinstall",
		]);
		const root = tempDir("pi-deps-r2-lifecycle-");
		mkdirSync(join(root, "packages/extension-host"), { recursive: true });
		writeFileSync(
			join(root, "packages/extension-host/package.json"),
			JSON.stringify({
				scripts: {
					postinstall: "node ./evil.js",
					build: "bun build ./src/main.ts --compile --outfile dist/pi-extension-host",
				},
			}),
		);
		const runner = new RecordingRunner(() => ({ exitCode: 0, stdout: "", stderr: "" }));
		const evidence = await buildBundleEvidence(root, root, runner);
		expect(evidence.error).toMatch(/lifecycle scripts|ignore-scripts/);
		const install = runner.calls.find((call) => call.command === "bun" && call.args[0] === "install");
		expect(install).toBeUndefined();
	});

	test("A: successful install argv includes --ignore-scripts", async () => {
		const root = tempDir("pi-deps-r2-ignore-");
		mkdirSync(join(root, "packages/extension-host"), { recursive: true });
		writeFileSync(
			join(root, "packages/extension-host/package.json"),
			JSON.stringify({
				scripts: { build: "bun build ./src/main.ts --compile --outfile dist/pi-extension-host" },
			}),
		);
		const runner = new RecordingRunner((call) => {
			if (call.command === "bun" && call.args[0] === "install") {
				return { exitCode: 0, stdout: "", stderr: "" };
			}
			if (call.command === "bun" && call.args[0] === "build") {
				const metaArg = call.args.find((arg) => arg.startsWith("--metafile="));
				if (metaArg !== undefined) {
					writeFileSync(metaArg.slice("--metafile=".length), JSON.stringify({ inputs: {} }));
				}
				return { exitCode: 0, stdout: "", stderr: "" };
			}
			return { exitCode: 0, stdout: "", stderr: "" };
		});
		await buildBundleEvidence(root, root, runner);
		const install = runner.calls.find((call) => call.command === "bun" && call.args[0] === "install");
		expect(install?.args).toEqual(["install", "--ignore-scripts", "--frozen-lockfile"]);
		expect(install?.options?.maxOutputBytes).toBeGreaterThan(0);
	});

	test("B: cargo lock auto-diff preserves identity/integrity fields", () => {
		const before = `[[package]]
name = "serde"
version = "1.0.0"
source = "registry+https://example"
checksum = "aaa"
dependencies = [
 "serde_derive",
]
`;
		const checksumOnly = before.replace('checksum = "aaa"', 'checksum = "bbb"');
		expect(cargoLockChangedNames(before, checksumOnly)).toEqual(["serde"]);
		const depsOnly = before.replace('"serde_derive"', '"serde_derive",\n "pkg"');
		expect(cargoLockChangedNames(before, depsOnly)).toEqual(["serde"]);
		expect(() => parseCargoLockIdentities(`[[package]]\nversion = "1.0.0"\n`)).toThrow(/unclassifiable|malformed/);
	});

	test("C: realpath staging rejects broken and escaping symlinks", async () => {
		const root = tempDir("pi-deps-r2-realpath-");
		mkdirSync(join(root, "inside"), { recursive: true });
		writeFileSync(join(root, "inside", "file.txt"), "ok");
		const broken = join(root, "broken-link");
		symlinkSync(join(root, "missing-target"), broken);
		await expect(resolvePathPair(broken, root)).rejects.toThrow(/broken|unresolvable/);
		await expect(expandWithRealpaths([broken], root)).rejects.toThrow(/broken|unresolvable/);
		const escape = join(root, "escape-link");
		symlinkSync("/tmp", escape);
		await expect(resolvePathPair(escape, root)).rejects.toThrow(/escapes repository root|ambiguous/);
		// Staging expansion keeps in-repo logical paths and external reals so
		// intentional worktree `.references` aliases remain classifiable.
		const expandedEscape = await expandWithRealpaths([escape], root);
		expect(expandedEscape).toContain(resolve(escape));
		expect(expandedEscape.some((path) => path === resolve("/tmp") || path.startsWith(`${resolve("/tmp")}/`))).toBe(true);
		const outside = tempDir("pi-deps-r2-outside-");
		mkdirSync(join(outside, "examples"), { recursive: true });
		writeFileSync(join(outside, "examples", "x.txt"), "x");
		mkdirSync(join(root, ".references"), { recursive: true });
		symlinkSync(outside, join(root, ".references", "pi"));
		const refs = await expandWithRealpaths([join(root, ".references/pi/examples")], root);
		expect(refs).toContain(resolve(root, ".references/pi/examples"));
		expect(refs).toContain(resolve(outside, "examples"));
		const ok = await resolvePathPair(join(root, "inside"), root);
		expect(ok.real.includes("inside")).toBe(true);
	});

	test("D: classify reports immutable base SHA", async () => {
		const result = await classify({
			base: "HEAD",
			inputs: ["npm:@deps-r2/nonexistent"],
			repoRoot: REPO_ROOT,
			runner: new RecordingRunner((call) => {
				if (call.command === "git" && call.args[0] === "rev-parse") {
					return { exitCode: 0, stdout: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n", stderr: "" };
				}
				return { exitCode: 0, stdout: "", stderr: "" };
			}),
			identity: true,
			now: () => new Date("2026-08-26T00:00:00.000Z"),
		});
		expect(result.report.base).toBe("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
		expect(result.report.head).toBe("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
	});
});

describe("dependency-exposure review wave-2 regressions", () => {
	test("1: head fingerprint includes every non-excluded Cargo.toml; path-set drift changes digest", async () => {
		const root = tempDir("pi-deps-r2-cargo-fp-");
		mkdirSync(join(root, "crates", "alpha"), { recursive: true });
		writeFileSync(join(root, "Cargo.toml"), "[workspace]\nmembers=[\"crates/alpha\"]\n");
		writeFileSync(join(root, "crates/alpha/Cargo.toml"), "[package]\nname=\"alpha\"\nversion=\"0.1.0\"\n");
		const paths = await listCargoTomlPaths(root);
		expect(paths).toEqual(["Cargo.toml", "crates/alpha/Cargo.toml"]);
		const evidence = await listHeadEvidencePaths(root);
		expect(evidence).toContain("Cargo.toml");
		expect(evidence).toContain("crates/alpha/Cargo.toml");
		const before = await fingerprintHeadEvidence(root);
		mkdirSync(join(root, "crates", "beta"), { recursive: true });
		writeFileSync(join(root, "crates/beta/Cargo.toml"), "[package]\nname=\"beta\"\nversion=\"0.1.0\"\n");
		const after = await fingerprintHeadEvidence(root);
		expect(after).not.toBe(before);
		expect(await listCargoTomlPaths(root)).toEqual([
			"Cargo.toml",
			"crates/alpha/Cargo.toml",
			"crates/beta/Cargo.toml",
		]);
	});

	test("2: ordinary classify refuses dirty head and never bun install/builds in live root", async () => {
		const liveHost = resolve(join(REPO_ROOT, HOST_PACKAGE_DIR));
		const dirty = await classify({
			base: "HEAD",
			inputs: ["npm:@types/bun"],
			repoRoot: REPO_ROOT,
			runner: new RecordingRunner((call) => {
				if (call.command === "git" && call.args[0] === "rev-parse") {
					return { exitCode: 0, stdout: "cccccccccccccccccccccccccccccccccccccccc\n", stderr: "" };
				}
				if (call.command === "git" && call.args[0] === "status") {
					return { exitCode: 0, stdout: " M Cargo.toml\n", stderr: "" };
				}
				return { exitCode: 0, stdout: "", stderr: "" };
			}),
			identity: false,
			now: () => new Date("2026-08-26T00:00:00.000Z"),
		});
		expect(dirty.failedClosed).toBe(true);
		expect(dirty.report.head).toBe("cccccccccccccccccccccccccccccccccccccccc");
		expect(dirty.report.verdicts[0]?.checks.E1.detail).toMatch(/dirty|refusing live-root/);

		const real = new SpawnRunner();
		const installBuildCwds: string[] = [];
		const runner = {
			async run(command: string, args: readonly string[], options?: Parameters<SpawnRunner["run"]>[2]) {
				if (command === "git" && args[0] === "status" && args.includes("--porcelain")) {
					return { exitCode: 0, stdout: "", stderr: "" };
				}
				if (command === "bun" && (args[0] === "install" || args[0] === "build")) {
					installBuildCwds.push(resolve(options?.cwd ?? ""));
					if (args[0] === "install") return { exitCode: 0, stdout: "", stderr: "" };
					const metaArg = args.find((arg) => arg.startsWith("--metafile="));
					if (metaArg !== undefined) {
						writeFileSync(metaArg.slice("--metafile=".length), JSON.stringify({ inputs: {} }));
					}
					return { exitCode: 0, stdout: "", stderr: "" };
				}
				return real.run(command, args, options);
			},
		};
		const result = await classify({
			base: "HEAD",
			inputs: ["npm:@types/bun"],
			repoRoot: REPO_ROOT,
			runner,
			identity: false,
			now: () => new Date("2026-08-26T00:00:00.000Z"),
		});
		expect(/^[0-9a-f]{40}$/i.test(result.report.head)).toBe(true);
		expect(installBuildCwds.length).toBeGreaterThan(0);
		expect(installBuildCwds.every((cwd) => cwd !== liveHost)).toBe(true);
		expect(installBuildCwds.every((cwd) => cwd.includes(`${sep}deps-r2-worktrees${sep}`) || cwd.includes("/deps-r2-worktrees/"))).toBe(true);
	});

	test("3: record append inserts exactly one EOF newline when ledger lacks one", async () => {
		const ledger = join(tempDir("pi-deps-r2-eof-"), "ledger.md");
		writeFileSync(ledger, "header-without-newline");
		await appendLedgerRows(ledger, {
			schema: SCHEMA,
			decidedAt: "2026-08-26T00:00:00.000Z",
			base: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
			head: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
			overall: "S",
			verdicts: [verdictFromChecks("npm:x", {
				E1: { status: "undecidable", detail: "boom|line\nfeed" },
				E2: { status: "pass", detail: "ok" },
				E3: { status: "pass", detail: "ok" },
				E4: { status: "pass", detail: "ok" },
			})],
		});
		const text = readFileSync(ledger, "utf8");
		expect(text.startsWith("header-without-newline\n| ")).toBe(true);
		expect(text).not.toContain("header-without-newline\n\n|");
	});

	test("4: input identifier grammar rejects injection; markdown cells escape pipes/newlines", () => {
		expect(() => parseInputSpec("npm:foo|bar")).toThrow(/invalid npm package identifier/);
		expect(() => parseInputSpec("npm:foo\nbar")).toThrow(/invalid npm package identifier/);
		expect(() => parseInputSpec("cargo:serde;rm")).toThrow(/invalid cargo crate identifier/);
		expect(() => parseInputSpec("tool:not-a-tool")).toThrow(/invalid or unknown tool id/);
		expect(parseInputSpec("npm:@types/bun").name).toBe("@types/bun");
		expect(parseInputSpec("cargo:serde").name).toBe("serde");
		expect(parseInputSpec("tool:bun-runtime").name).toBe("bun-runtime");
		expect(escapeMarkdownCell("a|b\nc\rd")).toBe("a\\|b c d");
	});
});

describe("dependency-exposure platform-gated live probes", () => {
	const cargoAvailable = (() => {
		try {
			const result = Bun.spawnSync(["cargo", "--version"]);
			return result.exitCode === 0;
		} catch {
			return false;
		}
	})();
	test.skipIf(!cargoAvailable)("live cargo metadata: serde shipped, insta/proptest are E1-pass", async () => {
		// Preserve immutable-base identity (D): forward real git rev-parse while
		// still injecting live cargo metadata through the recording seam.
		const realRunner = new SpawnRunner();
		const runner = new RecordingRunner(async (call) => {
			if (call.command === "git") {
				return realRunner.run(call.command, call.args, call.options);
			}
			if (call.command !== "cargo") return { exitCode: 0, stdout: "", stderr: "" };
			const real = Bun.spawnSync(["cargo", ...call.args], {
				cwd: call.options?.cwd ?? REPO_ROOT,
				stdout: "pipe",
				stderr: "pipe",
			});
			return {
				exitCode: real.exitCode ?? 1,
				stdout: real.stdout.toString(),
				stderr: real.stderr.toString(),
			};
		});
		const result = await classify({
			base: "HEAD",
			inputs: ["cargo:serde", "cargo:insta", "cargo:proptest", "cargo:tempfile"],
			repoRoot: REPO_ROOT,
			runner,
			identity: true,
			now: () => new Date("2026-08-26T00:00:00.000Z"),
		});
		expect(/^[0-9a-f]{40}$/i.test(result.report.base)).toBe(true);
		const byInput = Object.fromEntries(result.report.verdicts.map((verdict) => [verdict.input, verdict]));
		expect(byInput["cargo:serde"]?.checks.E1.status).toBe("fail");
		expect(byInput["cargo:tempfile"]?.checks.E1.status).toBe("fail");
		expect(byInput["cargo:insta"]?.checks.E1.status).toBe("pass");
		expect(byInput["cargo:proptest"]?.checks.E1.status).toBe("pass");
	}, 600_000);
});
