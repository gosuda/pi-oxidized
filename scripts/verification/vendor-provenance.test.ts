#!/usr/bin/env bun
/**
 * Tests for the shared vendored-inventory authority
 * (scripts/verification/vendor-provenance.ts).
 *
 * Every scenario runs against a real disposable git repository (or the real
 * checkout for the crossterm contract): no mocks, no faked index state. The
 * contract under test: the root Cargo.toml `[patch.*]` path values are the
 * only vendor inventory authority, a declared crate with zero indexed input
 * fails closed (naming package and path) in the inventory itself and in both
 * capture consumers, and compiled vendored crates that no patch declares are
 * refused.
 */

import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";

import { afterEach, describe, expect, test } from "bun:test";

import { captureContent, captureSnapshot, toolchainChannel } from "./deps-sbom.ts";
import { captureReference } from "./dependency-exposure.ts";
import {
	assertCompiledVendorCoverage,
	readVendorSourcePins,
	rootPatchCrates,
	rootPatchTables,
	vendorCrateInputPaths,
	vendorInputScopes,
} from "./vendor-provenance.ts";

export const REPO_ROOT = resolve(import.meta.dirname, "../..");
const SCRATCH_ROOT = resolve(REPO_ROOT, "target/vendor-provenance-tests");

/**
 * Real Git redirection variables must never leak into fixture git invocations,
 * or a developer's precommit/index environment could aim fixture `git add` at
 * real repository state. Every fixture git subprocess receives this scrubbed
 * env.
 */
const REDIRECTED_GIT_VARS: readonly string[] = [
	"GIT_DIR",
	"GIT_WORK_TREE",
	"GIT_INDEX_FILE",
	"GIT_COMMON_DIR",
	"GIT_OBJECT_DIRECTORY",
	"GIT_ALTERNATE_OBJECT_DIRECTORIES",
];

function scrubGitEnv(): Record<string, string> {
	const env: Record<string, string> = {};
	for (const [key, value] of Object.entries(process.env)) {
		if (value !== undefined && !REDIRECTED_GIT_VARS.includes(key)) env[key] = value;
	}
	return env;
}

const FIXTURE_GIT_ENV = scrubGitEnv();

const createdRoots: string[] = [];

afterEach(() => {
	while (createdRoots.length > 0) {
		rmSync(createdRoots.pop() as string, { recursive: true, force: true });
	}
});

let rustChannelCache: string | undefined;

function rustChannel(): string {
	if (rustChannelCache === undefined) rustChannelCache = toolchainChannel(REPO_ROOT);
	return rustChannelCache;
}

function freshRoot(prefix: string): string {
	mkdirSync(SCRATCH_ROOT, { recursive: true });
	const root = mkdtempSync(resolve(SCRATCH_ROOT, `${prefix}-XXXXXX`));
	createdRoots.push(root);
	const git = (args: readonly string[]): void => {
		const result = spawnSync("git", args, { cwd: root, encoding: "utf8", env: FIXTURE_GIT_ENV });
		if (result.status !== 0) throw new Error(`git ${args.join(" ")} failed: ${result.stderr ?? ""}`);
	};
	git(["init", "-b", "main"]);
	// Fixture-only isolation: hooks and signing are disabled through repo-local
	// config inside the disposable fixture, never real or global config.
	mkdirSync(resolve(root, ".githooks-empty"), { recursive: true });
	git(["config", "core.hooksPath", resolve(root, ".githooks-empty")]);
	git(["config", "commit.gpgsign", "false"]);
	git(["config", "user.email", "test@example.com"]);
	git(["config", "user.name", "Test User"]);
	return root;
}

function commitAll(root: string, message: string): void {
	const add = spawnSync("git", ["add", "."], { cwd: root, encoding: "utf8", env: FIXTURE_GIT_ENV });
	if (add.status !== 0) throw new Error(`git add failed: ${add.stderr ?? ""}`);
	const commit = spawnSync("git", ["commit", "-m", message], { cwd: root, encoding: "utf8", env: FIXTURE_GIT_ENV });
	if (commit.status !== 0) throw new Error(`git commit failed: ${commit.stderr ?? ""}`);
}

function writeSbomAuthorities(root: string, bunVersion = "1.2.3", target = "x86_64-unknown-linux-musl"): void {
	mkdirSync(resolve(root, "packages/extension-host"), { recursive: true });
	mkdirSync(resolve(root, "packages/pi-tui-protocol"), { recursive: true });
	mkdirSync(resolve(root, "scripts/release"), { recursive: true });
	mkdirSync(resolve(root, ".github/workflows"), { recursive: true });

	writeFileSync(
		resolve(root, "package.json"),
		JSON.stringify({ name: "root", dependencies: { typescript: "5.0.0" } }),
	);
	writeFileSync(
		resolve(root, "bun.lock"),
		JSON.stringify({
			lockfileVersion: 0,
			packages: { typescript: ["typescript@5.0.0", "", {}, "sha256-deadbeef"] },
		}),
	);
	writeFileSync(
		resolve(root, "packages/extension-host/package.json"),
		JSON.stringify({ name: "extension-host", devDependencies: { typebox: "1.3.19" } }),
	);
	writeFileSync(
		resolve(root, "packages/extension-host/bun.lock"),
		JSON.stringify({
			lockfileVersion: 0,
			packages: { typebox: ["typebox@1.3.19", "", {}, "sha256-cafebabe"] },
		}),
	);
	writeFileSync(
		resolve(root, "packages/pi-tui-protocol/package.json"),
		JSON.stringify({ name: "pi-tui-protocol" }),
	);
	writeFileSync(
		resolve(root, "scripts/release/runtime.ts"),
		`export const BUN_RUNTIME_VERSION = "${bunVersion}";\n` +
			`const ASSET_PINS = {\n` +
			`  "${target}": {\n` +
			`    bunTarget: "linux-x64-musl",\n` +
			`    fileName: "bun-linux-x64-musl.zip",\n` +
			`    sha256: "0000000000000000000000000000000000000000000000000000000000000000",\n` +
			`  },\n` +
			`};\n`,
	);
	writeFileSync(
		resolve(root, "scripts/release/targets.ts"),
		`export const RUST_TARGETS = ["${target}"] as const;\nexport type RustTarget = (typeof RUST_TARGETS)[number];\n`,
	);
	writeFileSync(
		resolve(root, ".github/workflows/release-verification.yml"),
		`toolchain: ${rustChannel()}\nbun-version: ${bunVersion}\n`,
	);
	writeFileSync(
		resolve(root, "rust-toolchain.toml"),
		`[toolchain]\nchannel = "${rustChannel()}"\n`,
	);
}

function writeCrate(root: string, crateDir: string, name: string, version: string): void {
	mkdirSync(resolve(root, crateDir, "src"), { recursive: true });
	writeFileSync(
		resolve(root, crateDir, "Cargo.toml"),
		`[package]\nname = "${name}"\nversion = "${version}"\nedition = "2021"\n`,
	);
	writeFileSync(
		resolve(root, crateDir, "Cargo.lock"),
		`version = 4\n\n[[package]]\nname = "${name}"\nversion = "${version}"\n`,
	);
	writeFileSync(resolve(root, crateDir, "src", "lib.rs"), `pub fn ${name.replace(/-/g, "_")}() {}\n`);
}

/** A root whose only tracked content declares one path patch; the target bytes are not tracked. */
function makeGhostRoot(): string {
	const root = freshRoot("vprov-ghost");
	writeFileSync(
		resolve(root, "Cargo.toml"),
		`[workspace]\nmembers = []\nresolver = "2"\n\n[patch.crates-io]\nghost = { path = "vendor/ghost-crate-1.0.0" }\n`,
	);
	commitAll(root, "declare ghost patch");
	return root;
}

describe("declared-crate inventory fails closed", () => {
	test("a declared crate with no vendored bytes at all is refused, naming package and path", () => {
		const root = makeGhostRoot();
		expect(() => vendorCrateInputPaths(root)).toThrow(/vendored patch ghost at vendor\/ghost-crate-1\.0\.0/);
		expect(() => vendorCrateInputPaths(root)).toThrow(/requires an indexed Cargo\.toml and indexed src\/\*\* bytes/);
		expect(() => readVendorSourcePins(root)).toThrow(/vendored patch ghost/);
	});

	test("untracked-only vendored bytes are refused like missing ones, naming package and path", () => {
		const root = makeGhostRoot();
		writeCrate(root, "vendor/ghost-crate-1.0.0", "ghost", "1.0.0");
		// The bytes exist in the worktree but were never added to the index:
		// they carry no provenance and must not silently satisfy the pin.
		expect(() => vendorCrateInputPaths(root)).toThrow(/vendored patch ghost at vendor\/ghost-crate-1\.0\.0/);
		expect(() => readVendorSourcePins(root)).toThrow(/untracked-only/);
	});

	test("an indexed manifest with untracked-only sources still fails the per-crate requirement", () => {
		const root = makeGhostRoot();
		writeCrate(root, "vendor/ghost-crate-1.0.0", "ghost", "1.0.0");
		const partial = spawnSync("git", ["add", "vendor/ghost-crate-1.0.0/Cargo.toml"], {
			cwd: root,
			encoding: "utf8",
			env: FIXTURE_GIT_ENV,
		});
		if (partial.status !== 0) throw new Error(`git add failed: ${partial.stderr ?? ""}`);
		expect(() => vendorCrateInputPaths(root)).toThrow(/vendored patch ghost at vendor\/ghost-crate-1\.0\.0/);
	});

	test("the inventory is empty only when no path patch is declared", () => {
		const root = freshRoot("vprov-nopatch");
		writeFileSync(resolve(root, "Cargo.toml"), `[workspace]\nmembers = []\nresolver = "2"\n`);
		commitAll(root, "no patch table");
		mkdirSync(resolve(root, "vendor/stray-0.1.0/src"), { recursive: true });
		writeFileSync(resolve(root, "vendor/stray-0.1.0/Cargo.toml"), `[package]\nname = "stray"\nversion = "0.1.0"\n`);
		// With no declaration there is no vendor expectation, so the empty
		// result is legitimate even though tracked-shaped bytes exist nearby.
		expect(vendorCrateInputPaths(root)).toEqual([]);
		expect(readVendorSourcePins(root)).toEqual([]);
	});
});

describe("both capture consumers fail closed per declared crate", () => {
	test("a zero-inventory declared crate refuses capture in every mode and publishes nothing", async () => {
		const root = makeGhostRoot();
		writeCrate(root, "vendor/ghost-crate-1.0.0", "ghost", "1.0.0");

		const out = resolve(root, "out", "sbom.json");
		mkdirSync(resolve(root, "out"), { recursive: true });
		writeFileSync(out, "old-output");

		// The declared crate has untracked-only bytes: both capture entrypoints
		// must refuse before any provenance is recorded or output replaced.
		expect(() => captureSnapshot(root, out, "committed")).toThrow(/vendored patch ghost at vendor\/ghost-crate-1\.0\.0/);
		expect(() => captureSnapshot(root, out, "staged")).toThrow(/vendored patch ghost at vendor\/ghost-crate-1\.0\.0/);
		// The default guard keeps refusing too.
		expect(() => captureSnapshot(root, out)).toThrow(/vendored patch ghost/);
		await expect(captureReference(root, resolve(root, "exposure-out"))).rejects.toThrow(
			/vendored patch ghost at vendor\/ghost-crate-1\.0\.0/,
		);
		await expect(captureReference(root, resolve(root, "exposure-out"), { inputMode: "staged" })).rejects.toThrow(
			/vendored patch ghost at vendor\/ghost-crate-1\.0\.0/,
		);

		// No capture replaced the previous output and no reference directory was created.
		expect(readFileSync(out, "utf8")).toBe("old-output");
		expect(existsSync(resolve(root, "exposure-out"))).toBe(false);
	});

	test("inventory scope additions are refused outside the approved vendor tree", () => {
		const root = freshRoot("vprov-outside");
		writeFileSync(
			resolve(root, "Cargo.toml"),
			`[workspace]\nmembers = []\nresolver = "2"\n\n[patch.crates-io]\nlocal = { path = "crates/local" }\n`,
		);
		// A declared path patch that cannot be pinned from the vendored tree
		// must refuse instead of silently escaping provenance.
		expect(() => vendorInputScopes(root)).toThrow(/local/);
		expect(() => vendorInputScopes(root)).toThrow(/crates\/local/);
		expect(() => vendorInputScopes(root)).toThrow(/literal crate directory under vendor\//);
	});
});

describe("inventory derives from declared patch paths", () => {
	test("declared crates are pinned at their declared location, including shapes the fixed vendor/* glob never matched", () => {
		const root = freshRoot("vprov-derived");
		writeFileSync(
			resolve(root, "Cargo.toml"),
			[
				"[workspace]",
				`members = []`,
				`resolver = "2"`,
				`exclude = ["vendor/plain-0.1.0", "vendor/nested/deep/buried-crate-0.3.1"]`,
				"",
				"[patch.crates-io]",
				`plain = { path = "vendor/plain-0.1.0" }`,
				`buried = { path = "vendor/nested/deep/buried-crate-0.3.1" }`,
				"",
			].join("\n"),
		);
		writeCrate(root, "vendor/plain-0.1.0", "plain", "0.1.0");
		writeCrate(root, "vendor/nested/deep/buried-crate-0.3.1", "buried", "0.3.1");
		// A tracked vendored crate that no patch declares must not enter the inventory:
		// derivation follows declarations, not directory existence.
		writeCrate(root, "vendor/undeclared-0.9.0", "undeclared", "0.9.0");
		commitAll(root, "declare and track vendor tree");
		const paths = readVendorSourcePins(root).map((pin) => pin.path);
		expect(paths).toContain("vendor/plain-0.1.0/Cargo.lock");
		expect(paths).toContain("vendor/plain-0.1.0/Cargo.toml");
		expect(paths).toContain("vendor/plain-0.1.0/src/lib.rs");
		expect(paths).toContain("vendor/nested/deep/buried-crate-0.3.1/Cargo.lock");
		expect(paths).toContain("vendor/nested/deep/buried-crate-0.3.1/Cargo.toml");
		expect(paths).toContain("vendor/nested/deep/buried-crate-0.3.1/src/lib.rs");
		expect(paths.every((path) => path.startsWith("vendor/plain-0.1.0/") || path.startsWith("vendor/nested/deep/buried-crate-0.3.1/"))).toBe(true);

		// The exact scope list both capture tools guard with is the declared
		// crates' fixed input classes, sorted. Cargo.lock is pinned when tracked
		// but never required for the per-crate fail-closed check.
		expect(vendorInputScopes(root)).toEqual([
			"vendor/nested/deep/buried-crate-0.3.1/.cargo_vcs_info.json",
			"vendor/nested/deep/buried-crate-0.3.1/Cargo.lock",
			"vendor/nested/deep/buried-crate-0.3.1/Cargo.toml",
			"vendor/nested/deep/buried-crate-0.3.1/Cargo.toml.orig",
			"vendor/nested/deep/buried-crate-0.3.1/LICENSE",
			"vendor/nested/deep/buried-crate-0.3.1/VENDORED.txt",
			"vendor/nested/deep/buried-crate-0.3.1/src/**",
			"vendor/plain-0.1.0/.cargo_vcs_info.json",
			"vendor/plain-0.1.0/Cargo.lock",
			"vendor/plain-0.1.0/Cargo.toml",
			"vendor/plain-0.1.0/Cargo.toml.orig",
			"vendor/plain-0.1.0/LICENSE",
			"vendor/plain-0.1.0/VENDORED.txt",
			"vendor/plain-0.1.0/src/**",
		]);
	});

	test("patch paths that cannot be pinned are refused rather than silently skipped", () => {
		const root = freshRoot("vprov-refuse");
		writeFileSync(
			resolve(root, "Cargo.toml"),
			`[patch.crates-io]\nglobby = { path = "vendor/glob-0.1.*" }\n`,
		);
		expect(() => rootPatchCrates(root)).toThrow(/globby/);
		expect(() => rootPatchCrates(root)).toThrow(/literal crate directory under vendor\//);
		writeFileSync(
			resolve(root, "Cargo.toml"),
			`[patch.crates-io]\nback = { path = "../elsewhere/crate-0.2.0" }\n`,
		);
		expect(() => rootPatchCrates(root)).toThrow(/back/);
		expect(() => rootPatchCrates(root)).toThrow(/\.\.\/elsewhere\/crate-0\.2\.0/);
	});
});

describe("patch-table serialization and rebase", () => {
	function writePatchRoot(root: string, patchBody: string): void {
		writeFileSync(resolve(root, "Cargo.toml"), `${patchBody}\n`);
	}

	test("non-path tables keep their written shape; relative paths rebase to the parsed root; empty sources are dropped", () => {
		const root = freshRoot("vprov-serialize");
		writePatchRoot(
			root,
			[
				"[patch.crates-io]",
				`vendored = { path = "vendor/vendored-0.1.0", features = ["serde"] }`,
				`legacy = "1.2.3"`,
				`remote = { version = "2.0.1", default-features = false }`,
				`absolute = { path = "/elsewhere/abs-crate-0.4.0" }`,
				"",
				"[patch.empty-src]",
				"",
				`[patch."odd source"]`,
				`weird = { git = "https://example.invalid/x" }`,
			].join("\n"),
		);

		const lines = rootPatchTables(root);
		expect(lines).toEqual([
			"[patch.crates-io]",
			`vendored = { path = ${JSON.stringify(resolve(root, "vendor/vendored-0.1.0"))}, features = ["serde"] }`,
			`legacy = "1.2.3"`,
			`remote = { version = "2.0.1", default-features = false }`,
			`absolute = { path = "/elsewhere/abs-crate-0.4.0" }`,
			"",
			`[patch."odd source"]`,
			`weird = { git = "https://example.invalid/x" }`,
			"",
		]);

		// The generated standalone manifest must parse back to the patches the
		// workspace compiles against, with the rebased absolute path.
		const reparsed = Bun.TOML.parse(lines.join("\n")) as Record<string, unknown>;
		const patch = reparsed["patch"] as Record<string, unknown>;
		const cratesIo = patch["crates-io"] as Record<string, unknown>;
		expect((cratesIo["vendored"] as Record<string, unknown>)["path"]).toBe(resolve(root, "vendor/vendored-0.1.0"));
		expect(cratesIo["legacy"]).toBe("1.2.3");
		const oddSource = patch["odd source"] as Record<string, unknown>;
		expect(((oddSource["weird"] as Record<string, unknown>)["git"])).toBe("https://example.invalid/x");
		expect("empty-src" in patch).toBe(false);
	});

	test("registry-only patch tables serialize without creating a vendor expectation", () => {
		const root = freshRoot("vprov-registry");
		writePatchRoot(
			root,
			["[patch.crates-io]", `legacy = "1.2.3"`, `remote = { version = "2.0.1" }`].join("\n"),
		);
		expect(rootPatchCrates(root)).toEqual([]);
		expect(rootPatchTables(root)).toEqual([
			"[patch.crates-io]",
			`legacy = "1.2.3"`,
			`remote = { version = "2.0.1" }`,
			"",
		]);
	});
});

describe("compiled vendored crates must carry declared pins", () => {
	test("the coverage guard refuses an unclaimed vendored package and passes a claimed one", () => {
		const claimed = { path: "vendor/claimed-0.1.0/Cargo.toml", sha256: "0".repeat(64) };
		expect(() =>
			assertCompiledVendorCoverage(
				"/",
				[
					{ name: "claimed", manifest_path: "/vendor/claimed-0.1.0/Cargo.toml", source: null },
					{ name: "external", manifest_path: "/registry/external-9.9.9/Cargo.toml", source: "registry+https://github.com/rust-lang/crates.io-index" },
					{ name: "pi", manifest_path: "/crates/pi/Cargo.toml", source: null },
				],
				[claimed],
			),
		).not.toThrow();
		expect(() =>
			assertCompiledVendorCoverage("/", [{ name: "claimed", manifest_path: "/vendor/claimed-0.1.0/Cargo.toml", source: null }], []),
		).toThrow(/compiled vendored crate claimed at vendor\/claimed-0\.1\.0/);
		expect(() =>
			assertCompiledVendorCoverage("/", [{ name: "claimed", manifest_path: "/vendor/claimed-0.1.0/Cargo.toml", source: null }], []),
		).toThrow(/no pinned manifest from a root Cargo\.toml path patch/);
	});
});

describe("cargo metadata cross-check", () => {
	/**
	 * Real cargo graph: `foreign` is wired exactly like the workspace's
	 * crossterm (registry requirement redirected by `[patch.crates-io]` to a
	 * vendored path), `sneaky` compiles from a direct path dependency into the
	 * vendor tree that no patch declares. Offline; no network fetch.
	 */
	function makeGraphRoot(): string {
		const root = freshRoot("vprov-graph");
		writeSbomAuthorities(root);
		mkdirSync(resolve(root, "crates/pi/src"), { recursive: true });
		writeFileSync(
			resolve(root, "Cargo.toml"),
			[
				"[workspace]",
				`members = ["crates/pi"]`,
				`resolver = "2"`,
				`exclude = ["vendor/foreign-0.1.0", "vendor/sneaky-0.2.0"]`,
				"",
				"[patch.crates-io]",
				`foreign = { path = "vendor/foreign-0.1.0" }`,
			].join("\n"),
		);
		writeFileSync(
			resolve(root, "crates/pi/Cargo.toml"),
			`[package]\nname = "pi"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\nforeign = "0.1.0"\nsneaky = { path = "../../vendor/sneaky-0.2.0" }\n`,
		);
		writeFileSync(resolve(root, "crates/pi/src/main.rs"), "fn main() {}\n");
		writeCrate(root, "vendor/foreign-0.1.0", "foreign", "0.1.0");
		writeFileSync(resolve(root, "vendor/foreign-0.1.0/LICENSE"), "MIT\n");
		writeFileSync(resolve(root, "vendor/foreign-0.1.0/VENDORED.txt"), "Vendored dependency: foreign 0.1.0\n");
		writeCrate(root, "vendor/sneaky-0.2.0", "sneaky", "0.2.0");
		return root;
	}

	function regenerateLock(root: string): void {
		const result = spawnSync("cargo", ["generate-lockfile", "--offline"], {
			cwd: root,
			encoding: "utf8",
			env: FIXTURE_GIT_ENV,
		});
		if (result.status !== 0) throw new Error(`cargo generate-lockfile failed: ${result.stderr ?? ""}`);
	}

	test("a compiled vendored crate without a declaring patch refuses SBOM capture, naming package and path", () => {
		const root = makeGraphRoot();
		regenerateLock(root);
		commitAll(root, "graph with undeclared vendored path dependency");
		// cargo metadata compiles `sneaky` (source null) from under vendor/,
		// but only `foreign` is declared: the mismatch must fail the capture.
		expect(() => captureContent(root)).toThrow(/compiled vendored crate sneaky/);
		expect(() => captureContent(root)).toThrow(/vendor\/sneaky-0\.2\.0/);
	}, 120_000);

	test("curing the mismatch by removing the undeclared dependency lets SBOM capture pass", () => {
		const root = makeGraphRoot();
		// Cure the mismatch the way the contract requires: the compiled graph
		// may only contain vendored crates that the root patch table declares.
		writeFileSync(
			resolve(root, "crates/pi/Cargo.toml"),
			`[package]\nname = "pi"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\nforeign = "0.1.0"\n`,
		);
		regenerateLock(root);
		commitAll(root, "declared vendored crate only");

		const content = captureContent(root);
		// A declared, compiled vendor crate must surface as SBOM vendor pins:
		// the previous fail-open shape omitted them entirely.
		const pins = content.rust.vendorPins;
		expect(pins).toBeDefined();
		const paths = (pins ?? []).map((pin) => pin.path);
		expect(paths).toContain("vendor/foreign-0.1.0/Cargo.lock");
		expect(paths).toContain("vendor/foreign-0.1.0/Cargo.toml");
		expect(paths).toContain("vendor/foreign-0.1.0/src/lib.rs");
		const foreign = content.rust.packages.find((p) => p.name === "foreign");
		expect(foreign).toBeDefined();
		expect(foreign?.source).toBe("path");
		expect(foreign?.direct).toBe(true);
		// The undeclared crate is no longer in the compiled graph.
		expect(content.rust.packages.some((p) => p.name === "sneaky")).toBe(false);
	}, 120_000);
});

describe("existing crossterm contract", () => {
	test("the real checkout pins the declared crossterm vendor crate", () => {
		const pins = readVendorSourcePins(REPO_ROOT);
		const paths = pins.map((pin) => pin.path);
		expect(paths).toContain("vendor/crossterm-0.29.0/Cargo.toml");
		expect(paths).toContain("vendor/crossterm-0.29.0/Cargo.toml.orig");
		expect(paths).toContain("vendor/crossterm-0.29.0/.cargo_vcs_info.json");
		expect(paths).toContain("vendor/crossterm-0.29.0/VENDORED.txt");
		expect(paths).toContain("vendor/crossterm-0.29.0/src/lib.rs");
		expect(paths).toContain("vendor/crossterm-0.29.0/src/event/sys/unix.rs");
		expect(paths.every((path) => path.startsWith("vendor/crossterm-0.29.0/"))).toBe(true);
		expect(pins.every((pin) => /^[0-9a-f]{64}$/.test(pin.sha256))).toBe(true);
		expect(rootPatchCrates(REPO_ROOT)).toEqual([{ name: "crossterm", path: "vendor/crossterm-0.29.0" }]);
	});

	test("the snippet lane manifest reuses the same parsed patch authority", () => {
		expect(rootPatchTables(REPO_ROOT)).toEqual([
			"[patch.crates-io]",
			`crossterm = { path = ${JSON.stringify(resolve(REPO_ROOT, "vendor/crossterm-0.29.0"))} }`,
			"",
		]);
	});
});
