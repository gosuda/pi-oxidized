#!/usr/bin/env bun
/** Tests for the DEPS-R1 SBOM baseline (scripts/verification/deps-sbom.ts). */

import { spawnSync } from "node:child_process";
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";

import { afterEach, describe, expect, test } from "bun:test";

import {
	BASELINE_PATH,
	REPO_ROOT,
	SBOM_SCHEMA,
	canonicalJson,
	captureContent,
	captureSnapshot,
	contentDigest,
	loadSnapshot,
	toolchainChannel,
	verifySnapshot,
} from "./deps-sbom.ts";
import type { SbomContent } from "./deps-sbom.ts";
import { readVendorSourcePins } from "./vendor-provenance.ts";

/** Recursively strips `readonly` so drift probes can mutate fixture copies. */
type DeepMutable<T> = {
	-readonly [K in keyof T]: T[K] extends readonly (infer U)[]
		? DeepMutable<U>[]
		: T[K] extends object
			? DeepMutable<T[K]>
			: T[K];
};

const SNAPSHOT_PATH = join(REPO_ROOT, BASELINE_PATH);
const SCRATCH_ROOT = resolve(REPO_ROOT, "target/capture-inputs-tests");

/**
 * Real Git redirection variables must never leak into fixture git invocations,
 * or a developer's precommit/index environment could aim fixture `git add` at
 * real repository state. Every fixture git subprocess receives this scrubbed env.
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

let installedRustChannelCache: string | undefined;

/**
 * The fixture's rust-toolchain.toml is executed by the real rustup-proxied
 * cargo, so its channel must be one this environment actually has installed:
 * the channel the repository itself pins. The workflow's toolchain pin stays
 * a fixture-specific data value (it is parsed, never executed). Probed lazily
 * so a missing pin only fails tests that build Rust fixtures.
 */
function installedRustChannel(): string {
	if (installedRustChannelCache === undefined) {
		installedRustChannelCache = toolchainChannel(REPO_ROOT);
	}
	return installedRustChannelCache;
}

function baselineSnapshot() {
	return loadSnapshot(readFileSync(SNAPSHOT_PATH, "utf8"));
}

let currentTestRoot: string | undefined;

afterEach(() => {
	if (currentTestRoot !== undefined) {
		rmSync(currentTestRoot, { recursive: true, force: true });
		currentTestRoot = undefined;
	}
});

function ensureScratch() {
	mkdirSync(SCRATCH_ROOT, { recursive: true });
}

function freshTestRoot(prefix: string): string {
	ensureScratch();
	const root = mkdtempSync(resolve(SCRATCH_ROOT, `${prefix}-XXXXXX`));
	currentTestRoot = root;
	return root;
}

function git(root: string, ...args: string[]): void {
	const result = spawnSync("git", args, { cwd: root, encoding: "utf8", env: FIXTURE_GIT_ENV });
	if (result.status !== 0) {
		throw new Error(`git ${args.join(" ")} failed: ${result.stderr ?? ""}`);
	}
}

function gitAddCommit(root: string, message: string): void {
	git(root, "add", ".");
	git(root, "commit", "-m", message);
}

function writeMinimalCargo(root: string): void {
	mkdirSync(resolve(root, "crates/pi/src"), { recursive: true });
	writeFileSync(
		resolve(root, "Cargo.toml"),
		`[workspace]\nmembers = ["crates/pi"]\nresolver = "2"\n`,
	);
	writeFileSync(
		resolve(root, "crates/pi/Cargo.toml"),
		`[package]\nname = "pi"\nversion = "0.1.0"\nedition = "2021"\n`,
	);
	writeFileSync(resolve(root, "crates/pi/src/main.rs"), "fn main() {}\n");
	writeFileSync(
		resolve(root, "Cargo.lock"),
		`version = 4\n\n[[package]]\nname = "pi"\nversion = "0.1.0"\n`,
	);
}

function writeMinimalNpm(root: string): void {
	mkdirSync(resolve(root, "packages/extension-host"), { recursive: true });
	mkdirSync(resolve(root, "packages/pi-tui-protocol"), { recursive: true });

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
}

function writeReleaseAuthorities(root: string, version: string, target: string): void {
	mkdirSync(resolve(root, "scripts/release"), { recursive: true });
	mkdirSync(resolve(root, ".github/workflows"), { recursive: true });

	writeFileSync(
		resolve(root, "scripts/release/runtime.ts"),
		`export const BUN_RUNTIME_VERSION = "${version}";\n` +
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
		`toolchain: 1.80.0\nbun-version: ${version}\n`,
	);
	writeFileSync(
		resolve(root, "rust-toolchain.toml"),
		`[toolchain]\nchannel = "${installedRustChannel()}"\n`,
	);
}

function writeDotAttributes(root: string): void {
	writeFileSync(resolve(root, ".gitattributes"), "* text=auto eol=lf\n*.json -text\n");
}

function makeMinimalRoot({
	bunVersion = "9.9.9",
	target = "x86_64-unknown-linux-musl",
}: { bunVersion?: string; target?: string } = {}): string {
	const root = freshTestRoot("sbom");
	git(root, "init", "-b", "main");
	// Fixture-only isolation: user hooks and commit signing are disabled through
	// repo-local config inside the disposable fixture, never real/global config.
	mkdirSync(resolve(root, ".githooks-empty"), { recursive: true });
	git(root, "config", "core.hooksPath", resolve(root, ".githooks-empty"));
	git(root, "config", "commit.gpgsign", "false");
	git(root, "config", "tag.gpgsign", "false");
	git(root, "config", "core.fsmonitor", "false");
	git(root, "config", "user.email", "test@example.com");
	git(root, "config", "user.name", "Test User");

	writeDotAttributes(root);
	writeMinimalCargo(root);
	writeMinimalNpm(root);
	writeReleaseAuthorities(root, bunVersion, target);
	gitAddCommit(root, "init");

	return root;
}

/**
 * One crate that already sits in the local Cargo registry cache. The
 * registry leg of the source-classification witness resolves offline against
 * installed artifacts only — never a network fetch, never an unverified new
 * version.
 */
function cachedRegistryCrate(): { name: string; version: string } {
	const cargoHome = process.env["CARGO_HOME"] ?? join(homedir(), ".cargo");
	const cacheRoot = resolve(cargoHome, "registry", "cache");
	const candidates: string[] = [];
	try {
		for (const host of readdirSync(cacheRoot)) {
			for (const file of readdirSync(resolve(cacheRoot, host))) {
				const match = /^(.+)-(\d[\w.+-]*)\.crate$/.exec(file);
				if (match) candidates.push(`${match[1]}-${match[2]}`);
			}
		}
	} catch {
		// Falling through to the explicit failure below keeps the reason visible.
	}
	candidates.sort();
	const first = candidates[0];
	if (first === undefined) {
		throw new Error(
			"no cached registry crate found under CARGO_HOME/registry/cache; the registry-classification witness needs a real offline-resolvable crate",
		);
	}
	const dash = first.lastIndexOf("-");
	return { name: first.slice(0, dash), version: first.slice(dash + 1) };
}

/** Extends a minimal root with an approved, non-workspace vendored path crate. */
function makeVendoredRoot(): { root: string; registryCrate: { name: string; version: string } } {
	const root = makeMinimalRoot();
	const registryCrate = cachedRegistryCrate();
	mkdirSync(resolve(root, "vendor/foreign-0.1.0/src"), { recursive: true });
	writeFileSync(
		resolve(root, "vendor/foreign-0.1.0/Cargo.toml"),
		`[package]\nname = "foreign"\nversion = "0.1.0"\nedition = "2021"\n`,
	);
	writeFileSync(resolve(root, "vendor/foreign-0.1.0/src/lib.rs"), "pub fn foreign() {}\n");
	writeFileSync(resolve(root, "vendor/foreign-0.1.0/LICENSE"), "MIT\n");
	writeFileSync(
		resolve(root, "vendor/foreign-0.1.0/VENDORED.txt"),
		"Vendored dependency: foreign 0.1.0\n",
	);
	// Patched path crate, mirroring the real repo's [patch.crates-io] vendoring:
	// the workspace depends on a registry name/version and Cargo redirects it
	// to the local path source, so cargo metadata reports source as null. The
	// exact-path `exclude` mirrors the real workspace root and pins the
	// non-workspace-member guarantee even though the crate lives under the
	// workspace directory.
	writeFileSync(
		resolve(root, "Cargo.toml"),
		`[workspace]\nmembers = ["crates/pi"]\nexclude = ["vendor/foreign-0.1.0"]\nresolver = "2"\n\n[patch.crates-io]\nforeign = { path = "vendor/foreign-0.1.0" }\n`,
	);
	writeFileSync(
		resolve(root, "crates/pi/Cargo.toml"),
		`[package]\nname = "pi"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\nforeign = "0.1.0"\n${registryCrate.name} = "=${registryCrate.version}"\n`,
	);
	const lock = spawnSync("cargo", ["generate-lockfile", "--offline"], {
		cwd: root,
		encoding: "utf8",
	});
	if (lock.status !== 0) {
		throw new Error(`cargo generate-lockfile failed: ${lock.stderr ?? ""}`);
	}
	gitAddCommit(root, "add vendored path crate");
	return { root, registryCrate };
}

describe("SBOM baseline fixture integrity", () => {
	test("loads with the pinned schema and a self-consistent digest chain", () => {
		const snapshot = baselineSnapshot();
		expect(snapshot.schema).toBe(SBOM_SCHEMA);
		expect(snapshot.contentSha256).toBe(contentDigest(snapshot.content));
		expect(snapshot.captureHead).toMatch(/^[0-9a-f]{40}$/);
		expect(snapshot.capturedAt).toMatch(/^\d{4}-\d{2}-\d{2}$/);
	});

	test("rejects a tampered digest chain", () => {
		const snapshot = baselineSnapshot();
		const tampered = { ...snapshot, contentSha256: "0".repeat(64) };
		expect(() => loadSnapshot(JSON.stringify(tampered))).toThrow(/content does not match/);
	});
});

describe("SBOM live-tree anchor", () => {
	test(
		"the checked-in baseline still describes the current tree",
		() => {
			const drift = verifySnapshot(baselineSnapshot(), captureContent(REPO_ROOT));
			expect(drift).toEqual([]);
		},
		120_000,
	);

	test(
		"capture is deterministic across invocations",
		() => {
			const first = canonicalJson(captureContent(REPO_ROOT));
			const second = canonicalJson(captureContent(REPO_ROOT));
			expect(first).toBe(second);
		},
		120_000,
	);
});

describe("SBOM content structure", () => {
	test("seven release targets with both musl asset pins", () => {
		const { content } = baselineSnapshot();
		expect(content.tools.releaseTargets).toHaveLength(7);
		expect(content.tools.releaseTargets).toContain("x86_64-unknown-linux-musl");
		expect(content.tools.releaseTargets).toContain("aarch64-unknown-linux-musl");
		const pinned = new Set(content.tools.bunAssetPins.map((p) => p.rustTarget));
		for (const target of content.tools.releaseTargets) {
			expect(pinned.has(target)).toBe(true);
		}
	});

	test("every scheduled bin member is pinned at its from-version", () => {
		const { content } = baselineSnapshot();
		const rustIds = new Set(content.rust.packages.map((p) => `${p.name}@${p.version}`));
		const fromVersions = [
			"futures@0.3.34",
			"globset@0.4.20",
			"ignore@0.4.33",
			"jiff@0.2.35",
			"schemars@1.2.2",
			"serde@1.0.229",
			"serde_json@1.0.151",
			"thiserror@2.0.20",
			"tokio-util@0.7.19",
			"aws-config@1.11.0",
			"aws-sdk-bedrockruntime@1.142.0",
			"google-cloud-auth@1.16.0",
			"tokio@1.53.1",
			"uuid@1.26.0",
			"base64@0.23.1",
			"serde-saphyr@1.1.0",
		];
		for (const id of fromVersions) {
			expect(rustIds.has(id)).toBe(true);
		}
		const rootLock = new Set(
			(content.npm.lockfiles[0]?.packages ?? []).map((p) => `${p.name}@${p.version}`),
		);
		expect(rootLock.has("ignore@7.0.6")).toBe(true);
		expect(rootLock.has("typescript@7.0.2")).toBe(true);
		expect(rootLock.has("@types/bun@1.4.0")).toBe(true);
		const hostLock = new Set(
			(content.npm.lockfiles[1]?.packages ?? []).map((p) => `${p.name}@${p.version}`),
		);
		expect(hostLock.has("typebox@1.3.19")).toBe(true);
	});

	test("direct registry and path pins carry licenses and the toolchain/bun pins agree", () => {
		const { content } = baselineSnapshot();
		const direct = content.rust.packages.filter((p) => p.direct && (p.source === "registry" || p.source === "path"));
		expect(direct.length).toBeGreaterThan(40);
		for (const pin of direct) {
			expect(pin.license.length).toBeGreaterThan(0);
		}
		expect(content.rust.toolchainChannel).toBe(content.rust.ciRustToolchain);
		expect(content.tools.bunRuntimeVersion).toBe(content.tools.ciBunVersion);
	});
});

describe("SBOM drift detection", () => {
	test("a version bump, a toolchain move, and a lost pin each fail verification", () => {
		const snapshot = baselineSnapshot();
		const mutable = () =>
			JSON.parse(JSON.stringify(snapshot.content)) as DeepMutable<SbomContent>;
		const live = mutable();
		const serde = live.rust.packages.find((p) => p.name === "serde");
		if (serde === undefined) throw new Error("fixture lost serde");
		serde.version = "1.0.230";
		expect(verifySnapshot(snapshot, live).some((d) => d.includes("serde"))).toBe(true);

		const toolchainMoved = mutable();
		toolchainMoved.rust.toolchainChannel = "1.99.0";
		expect(
			verifySnapshot(snapshot, toolchainMoved).some((d) => d.includes("toolchain")),
		).toBe(true);

		const pinMoved = mutable();
		pinMoved.tools.bunAssetPins = pinMoved.tools.bunAssetPins.map((p) => ({
			...p,
			sha256: "0".repeat(64),
		}));
		expect(
			verifySnapshot(snapshot, pinMoved).some((d) => d.includes("asset pin")),
		).toBe(true);

		const digestDrift = verifySnapshot(snapshot, toolchainMoved).find((d) =>
			d.includes("content digest drift"),
		);
		expect(digestDrift).toBeDefined();
	});
});

// Acceptance 10-13: staged capture, strict modes, metadata, errors, output overlap,
// child-process race and publish failure.

describe("SBOM staged capture root authority", () => {
	test("reads release literals from the root, not static imports", () => {
		const root = makeMinimalRoot({ bunVersion: "9.9.9", target: "aarch64-unknown-linux-musl" });
		const content = captureContent(root);
		expect(content.tools.bunRuntimeVersion).toBe("9.9.9");
		expect(content.tools.releaseTargets).toEqual(["aarch64-unknown-linux-musl"]);
		expect(content.tools.ciBunVersion).toBe("9.9.9");
		expect(content.rust.toolchainChannel).toBe(installedRustChannel());
		expect(content.rust.ciRustToolchain).toBe("1.80.0");
		expect(content.rust.packages.some((p) => p.name === "pi")).toBe(true);
		expect(content.npm.lockfiles).toHaveLength(2);
		expect(content.npm.surfaces).toHaveLength(3);
	});

	test("rejects missing, duplicate, or malformed release authorities", () => {
		const root = makeMinimalRoot();
		writeFileSync(resolve(root, "scripts/release/runtime.ts"), `export const BUN_RUNTIME_VERSION = 1.3.14;\n`);
		expect(() => captureContent(root)).toThrow(/non-literal or malformed/);
		rmSync(root, { recursive: true, force: true });

		const rootMissing = makeMinimalRoot();
		writeFileSync(resolve(rootMissing, "scripts/release/runtime.ts"), `export const OTHER = 1;\n`);
		expect(() => captureContent(rootMissing)).toThrow(/missing BUN_RUNTIME_VERSION/);
		rmSync(rootMissing, { recursive: true, force: true });

		const rootDuplicateVersion = makeMinimalRoot();
		writeFileSync(
			resolve(rootDuplicateVersion, "scripts/release/runtime.ts"),
			`export const BUN_RUNTIME_VERSION = "1.0.0";\nexport const BUN_RUNTIME_VERSION = "2.0.0";\n`,
		);
		expect(() => captureContent(rootDuplicateVersion)).toThrow(/duplicate BUN_RUNTIME_VERSION/);
		rmSync(rootDuplicateVersion, { recursive: true, force: true });

		const root2 = makeMinimalRoot();
		writeFileSync(
			resolve(root2, "scripts/release/targets.ts"),
			`export const RUST_TARGETS = ["aarch64-unknown-linux-musl", "aarch64-unknown-linux-musl"] as const;\n`,
		);
		gitAddCommit(root2, "bad targets");
		expect(() => captureContent(root2)).toThrow(/duplicate target in RUST_TARGETS/);
		rmSync(root2, { recursive: true, force: true });

		const rootNonLiteralTarget = makeMinimalRoot();
		writeFileSync(
			resolve(rootNonLiteralTarget, "scripts/release/targets.ts"),
			`export const RUST_TARGETS = [x86_64-unknown-linux-musl] as const;\n`,
		);
		expect(() => captureContent(rootNonLiteralTarget)).toThrow(/non-literal target in RUST_TARGETS/);
		rmSync(rootNonLiteralTarget, { recursive: true, force: true });

		const rootInvalidTarget = makeMinimalRoot();
		writeFileSync(
			resolve(rootInvalidTarget, "scripts/release/targets.ts"),
			`export const RUST_TARGETS = ["X86_64_UNKNOWN_LINUX_MUSL"] as const;\n`,
		);
		expect(() => captureContent(rootInvalidTarget)).toThrow(/invalid target in RUST_TARGETS/);
		rmSync(rootInvalidTarget, { recursive: true, force: true });

		const rootEmptyTargets = makeMinimalRoot();
		writeFileSync(
			resolve(rootEmptyTargets, "scripts/release/targets.ts"),
			`export const RUST_TARGETS = [] as const;\n`,
		);
		expect(() => captureContent(rootEmptyTargets)).toThrow(/RUST_TARGETS is empty/);
	});
});

describe("SBOM staged capture modes", () => {
	test("default committed mode refuses staged and unstaged relevant dirt", () => {
		const root = makeMinimalRoot();
		const out = resolve(root, "out", "baseline.json");
		mkdirSync(dirname(out), { recursive: true });

		// Unstaged change.
		writeFileSync(resolve(root, "Cargo.lock"), `version = 4\n\n[[package]]\nname = "pi"\nversion = "0.2.0"\n`);
		expect(() => captureSnapshot(root, out, "committed")).toThrow(/dirty/);

		// Discard the unstaged change and stage a different one.
		git(root, "checkout", "--", "Cargo.lock");
		writeFileSync(
			resolve(root, "Cargo.lock"),
			`version = 4\n\n[[package]]\nname = "pi"\nversion = "0.2.0"\n`,
		);
		git(root, "add", "Cargo.lock");
		expect(() => captureSnapshot(root, out, "committed")).toThrow(/dirty/);
	});
	test("staged mode captures indexed content with honest provenance and round-trips", () => {
		const root = makeMinimalRoot({ bunVersion: "1.2.3" });
		const out = resolve(root, "out", "baseline.json");
		mkdirSync(dirname(out), { recursive: true });

		// Introduce a staged dependency change with matching worktree bytes:
		// bump the crate manifest and lockfile together so the real
		// `cargo metadata --locked` still accepts the workspace.
		writeFileSync(
			resolve(root, "crates/pi/Cargo.toml"),
			`[package]\nname = "pi"\nversion = "0.2.0"\nedition = "2021"\n`,
		);
		writeFileSync(
			resolve(root, "Cargo.lock"),
			`version = 4\n\n[[package]]\nname = "pi"\nversion = "0.2.0"\n`,
		);
		git(root, "add", "crates/pi/Cargo.toml", "Cargo.lock");

		const snapshot = captureSnapshot(root, out, "staged");
		expect(snapshot.stagedInputProvenance).toBeDefined();
		expect(snapshot.captureHead).toBe(snapshot.stagedInputProvenance!.baseHead);
		expect(snapshot.captureHead).toMatch(/^[0-9a-f]{40}$/);
		expect(snapshot.stagedInputProvenance!.mode).toBe("staged");
		expect(snapshot.content.rust.packages.find((p) => p.name === "pi")?.version).toBe("0.2.0");

		const text = readFileSync(out, "utf8");
		const loaded = loadSnapshot(text);
		expect(loaded.stagedInputProvenance).toBeDefined();
		expect(verifySnapshot(loaded, captureContent(root))).toEqual([]);
	});

	test("legacy metadata-free fixture still loads", () => {
		// The checked-in baseline records staged provenance since the owning
		// staged capture; a legacy v1 document is that snapshot with the
		// optional property removed.
		const { stagedInputProvenance: _omitted, ...legacy } = baselineSnapshot();
		expect("stagedInputProvenance" in legacy).toBe(false);
		expect(loadSnapshot(JSON.stringify(legacy))).toEqual(legacy);
	});
});

describe("SBOM staged capture error handling", () => {
	test("malformed staged metadata and captureHead mismatch fail load", () => {
		const snapshot = baselineSnapshot();
		const withBadProvenance = {
			...snapshot,
			stagedInputProvenance: { schema: "wrong" },
		};
		expect(() => loadSnapshot(JSON.stringify(withBadProvenance))).toThrow();

		const withBaseHead = {
			...snapshot,
			stagedInputProvenance: {
				schema: "pi.deps.staged-inputs.v1",
				mode: "staged",
				baseHead: "0".repeat(40),
				objectFormat: "sha1",
				pathspecs: ["Cargo.toml"],
				entries: [{ path: "Cargo.toml", mode: "100644", blob: "a".repeat(40) }],
			},
		};
		expect(() => loadSnapshot(JSON.stringify(withBaseHead))).toThrow(/baseHead/);
	});

	test("producer error and output overlap do not replace old output", () => {
		const root = makeMinimalRoot();
		const out = resolve(root, "out", "baseline.json");
		mkdirSync(dirname(out), { recursive: true });
		writeFileSync(out, "old-baseline");

		// Producer error: break Cargo.toml so cargo metadata fails after checks begin.
		writeFileSync(resolve(root, "Cargo.toml"), "not valid toml [");
		git(root, "add", "Cargo.toml");
		git(root, "commit", "-m", "broken");
		expect(() => captureSnapshot(root, out, "staged")).toThrow();
		expect(readFileSync(out, "utf8")).toBe("old-baseline");

		// Output overlap: try to write the snapshot on top of an input file.
		writeFileSync(resolve(root, "Cargo.toml"), `[workspace]\nmembers = ["crates/pi"]\n`);
		git(root, "add", "Cargo.toml");
		git(root, "commit", "-m", "fix");
		expect(() => captureSnapshot(root, resolve(root, "Cargo.toml"), "staged")).toThrow();
		expect(readFileSync(out, "utf8")).toBe("old-baseline");
	});

	test("CLI rejects invalid flags and values with exit 2", () => {
		const cli = resolve(REPO_ROOT, "scripts/verification/deps-sbom.ts");

		const missingOut = spawnSync("bun", [cli, "capture", "--out"], { encoding: "utf8" });
		expect(missingOut.status).toBe(2);

		const badValue = spawnSync("bun", [cli, "capture", "--out", "--staged-inputs"], {
			encoding: "utf8",
		});
		expect(badValue.status).toBe(2);

		const unknown = spawnSync("bun", [cli, "capture", "--allow-dirty"], { encoding: "utf8" });
		expect(unknown.status).toBe(2);

		const verifyStaged = spawnSync("bun", [cli, "verify", "--staged-inputs"], {
			encoding: "utf8",
		});
		expect(verifyStaged.status).toBe(2);
	});

	test("relocated capture wrapper runs committed and staged paths", () => {
		const root = makeMinimalRoot();
		const out = resolve(root, "out", "baseline.json");
		const wrapper = resolve(root, "capture-wrapper.ts");
		writeFileSync(
			wrapper,
			`import { captureSnapshot } from "${resolve(REPO_ROOT, "scripts/verification/deps-sbom.ts")}";\n` +
				`const [root, out, mode] = process.argv.slice(2);\n` +
				`captureSnapshot(root, out, mode as "committed" | "staged");\n`,
		);

		mkdirSync(dirname(out), { recursive: true });
		const committed = spawnSync("bun", [wrapper, root, out, "committed"], {
			encoding: "utf8",
			env: FIXTURE_GIT_ENV,
		});
		expect(committed.status).toBe(0);
		const loaded = loadSnapshot(readFileSync(out, "utf8"));
		expect(loaded.stagedInputProvenance).toBeUndefined();

		// Stage a change and capture again: bump the crate manifest and
		// lockfile together so the real `cargo metadata --locked` still
		// accepts the workspace.
		writeFileSync(
			resolve(root, "crates/pi/Cargo.toml"),
			`[package]\nname = "pi"\nversion = "0.3.0"\nedition = "2021"\n`,
		);
		writeFileSync(
			resolve(root, "Cargo.lock"),
			`version = 4\n\n[[package]]\nname = "pi"\nversion = "0.3.0"\n`,
		);
		git(root, "add", "crates/pi/Cargo.toml", "Cargo.lock");
		const staged = spawnSync("bun", [wrapper, root, out, "staged"], {
			encoding: "utf8",
			env: FIXTURE_GIT_ENV,
		});
		expect(staged.status).toBe(0);
		const reloaded = loadSnapshot(readFileSync(out, "utf8"));
		expect(reloaded.stagedInputProvenance).toBeDefined();
	});
});

describe("SBOM staged capture publication safety", () => {
	test("pre-rename filesystem failure preserves the old canonical selection", () => {
		const root = makeMinimalRoot();
		const out = resolve(root, "out");
		// The canonical output path is an existing non-empty directory, so the
		// rename of the temp file over it must fail and leave it intact.
		const dest = resolve(out, "baseline.json");
		mkdirSync(dest, { recursive: true });
		writeFileSync(resolve(dest, "sentinel.txt"), "old-canonical");

		expect(() => captureSnapshot(root, dest, "staged")).toThrow();
		expect(readFileSync(resolve(dest, "sentinel.txt"), "utf8")).toBe("old-canonical");
		// No temp residue should remain in the destination parent directory.
		for (const name of readdirSync(out)) {
			expect(name.startsWith(".sbom-snapshot-")).toBe(false);
		}
	});

	/**
	 * Writes a transparent `cargo` shim for the race tests. The capture
	 * pipeline is fully synchronous, so the only real I/O boundary inside
	 * captureSnapshot where a mutation can be deterministically ordered is the
	 * `cargo metadata` subprocess spawned by captureContent. The shim is
	 * resolved through PATH ahead of the real toolchain: it parks on a unix
	 * socket rendezvous with the test parent, then runs the real cargo binary
	 * with unchanged argv and stdio. It emits no output of its own — every
	 * byte the producer captures comes from the real cargo process — so this
	 * is a transparent wrapper synchronizing real I/O, not a fake producer or
	 * Git response, and production code carries no test hook.
	 */
	function writeCargoShim(shimDir: string, realCargo: string, sockPath: string): void {
		const shim = resolve(shimDir, "cargo");
		writeFileSync(
			shim,
			`#!/usr/bin/env bun
import { spawnSync } from "node:child_process";

// Park until the test parent releases the rendezvous, then run the real
// cargo with this process's argv and stdio so the producer observes an
// ordinary cargo subprocess.
await new Promise<void>((resolvePromise, reject) => {
	Bun.connect({
		unix: ${JSON.stringify(sockPath)},
		socket: {
			open(socket) { socket.write("R\\n"); },
			data(socket) { resolvePromise(); socket.end(); },
			close() { reject(new Error("rendezvous closed by parent")); },
			end() { reject(new Error("rendezvous ended by parent")); },
			error(_socket, error) { reject(error); },
			connectError(_socket, error) { reject(error); },
		},
	}).catch(reject);
});

const result = spawnSync(${JSON.stringify(realCargo)}, process.argv.slice(2), {
	stdio: "inherit",
});
process.exit(result.status ?? 1);
`,
		);
		chmodSync(shim, 0o755);
	}

	/** Writes the child entrypoint: the real producer, invoked unmodified. */
	function writeCaptureWrapper(path: string): void {
		const depsSbom = resolve(REPO_ROOT, "scripts/verification/deps-sbom.ts");
		writeFileSync(
			path,
			`import { captureSnapshot } from "${depsSbom}";
const [fixtureRoot, out, mode] = process.argv.slice(2);
captureSnapshot(fixtureRoot, out, mode as "committed" | "staged");
`,
		);
	}

	/**
	 * Deterministic rendezvous with the cargo shim over a real unix socket.
	 * `ready` resolves when the shim — and therefore the producer's real
	 * `cargo metadata` subprocess inside captureSnapshot — is parked between
	 * the staged-input baseline and the producer's own postcapture recheck.
	 * `release` lets it exec the real cargo. No sleeps, polling, or watchers.
	 */
	function startCargoBarrier(sockPath: string) {
		let shimSocket: { write(data: string): number } | undefined;
		let resolveReady!: () => void;
		const ready = new Promise<void>((res) => {
			resolveReady = res;
		});
		const listener = Bun.listen({
			unix: sockPath,
			socket: {
				open(socket) {
					shimSocket = socket;
				},
				data() {
					resolveReady();
				},
			},
		});
		return {
			ready,
			release() {
				shimSocket?.write("G\n");
			},
			stop() {
				listener.stop(true);
			},
		};
	}

	function spawnCaptureChild(root: string, wrapper: string, out: string, shimDir: string) {
		return Bun.spawn(["bun", wrapper, root, out, "staged"], {
			cwd: REPO_ROOT,
			env: { ...FIXTURE_GIT_ENV, PATH: `${shimDir}:${FIXTURE_GIT_ENV["PATH"] ?? ""}` },
			stdout: "ignore",
			stderr: "pipe",
		});
	}

	/** Fixture wiring shared by the barrier control and race tests. */
	function setupBarrierFixture(root: string) {
		const outDir = resolve(root, "out");
		const out = resolve(outDir, "baseline.json");
		mkdirSync(outDir, { recursive: true });
		writeFileSync(out, "old-output");
		const shimDir = resolve(root, "shim-bin");
		mkdirSync(shimDir, { recursive: true });
		const sockPath = resolve(root, "c.sock");
		const realCargo = Bun.which("cargo");
		if (realCargo === null) throw new Error("cargo not found on PATH");
		writeCargoShim(shimDir, realCargo, sockPath);
		const wrapper = resolve(root, "capture-wrapper.ts");
		writeCaptureWrapper(wrapper);
		return { outDir, out, shimDir, sockPath, wrapper };
	}

	// The cargo shim relies on a POSIX shebang and a unix socket rendezvous.
	test.skipIf(process.platform === "win32")(
		"cargo-boundary barrier control: clean release publishes through the real captureSnapshot",
		async () => {
			const root = makeMinimalRoot({ bunVersion: "1.2.3" });
			const { out, shimDir, sockPath, wrapper } = setupBarrierFixture(root);

			const barrier = startCargoBarrier(sockPath);
			try {
				const child = spawnCaptureChild(root, wrapper, out, shimDir);
				const ready = await Promise.race([
					barrier.ready.then(() => "ready" as const),
					child.exited.then((code) => `exited:${code}`),
				]);
				if (ready !== "ready") {
					child.kill();
					await child.exited;
					throw new Error(`producer exited before the cargo barrier: ${ready}`);
				}
				barrier.release();

				const [exitCode, stderr] = await Promise.all([
					child.exited,
					new Response(child.stderr).text(),
				]);
				expect(exitCode === 0 ? "" : `exit ${exitCode}: ${stderr}`).toBe("");
				const loaded = loadSnapshot(readFileSync(out, "utf8"));
				expect(loaded.content.tools.bunRuntimeVersion).toBe("1.2.3");
				expect(loaded.stagedInputProvenance).toBeDefined();
			} finally {
				barrier.stop();
			}
		},
		120_000,
	);

	test.skipIf(process.platform === "win32")(
		"cargo-boundary barrier race: a mutation inside captureSnapshot is caught by its own postcapture guard",
		async () => {
			const root = makeMinimalRoot();
			const { outDir, out, shimDir, sockPath, wrapper } = setupBarrierFixture(root);

			const barrier = startCargoBarrier(sockPath);
			try {
				const child = spawnCaptureChild(root, wrapper, out, shimDir);
				// READY means the producer is parked inside the real cargo-metadata
				// subprocess — strictly inside captureSnapshot, after the staged-input
				// baseline and before the producer's own postcapture recheck. The
				// mutation below is therefore ordered mid-capture by explicit
				// synchronization, not by a watcher or a delay.
				const ready = await Promise.race([
					barrier.ready.then(() => "ready" as const),
					child.exited.then((code) => `exited:${code}`),
				]);
				if (ready !== "ready") {
					child.kill();
					await child.exited;
					throw new Error(`producer exited before the cargo barrier: ${ready}`);
				}
				expect(readFileSync(out, "utf8")).toBe("old-output");
				// Mutate a relevant input the remaining producers still accept:
				// valid lockfile JSON with different bytes, read by lockfileRecords
				// after cargo returns. With the producer's own postcapture and
				// pre-rename rechecks removed this would publish mutated content
				// and exit 0 — only those guards can refuse it.
				writeFileSync(
					resolve(root, "bun.lock"),
					JSON.stringify({
						lockfileVersion: 0,
						packages: { typescript: ["typescript@5.0.1", "", {}, "sha256-deadbeef"] },
					}),
				);
				barrier.release();

				const [exitCode, stderr] = await Promise.all([
					child.exited,
					new Response(child.stderr).text(),
				]);
				expect(exitCode).not.toBe(0);
				// This category can only come from the producer's own recheck
				// inside captureSnapshot: the mutation did not exist when the
				// staged-input baseline was taken.
				expect(stderr).toMatch(/relevant inputs changed after the capture began/);
				expect(stderr).toMatch(/bun\.lock/);
				expect(readFileSync(out, "utf8")).toBe("old-output");
				for (const name of readdirSync(outDir)) {
					expect(name.startsWith(".sbom-snapshot-")).toBe(false);
				}
			} finally {
				barrier.stop();
			}
		},
		120_000,
	);

	test("captureSnapshot refuses a mutated working tree without replacing the output", () => {
		const root = makeMinimalRoot();
		const out = resolve(root, "out", "baseline.json");
		mkdirSync(dirname(out), { recursive: true });
		writeFileSync(out, "old-output");
		writeFileSync(
			resolve(root, "Cargo.lock"),
			`version = 4\n\n[[package]]\nname = "pi"\nversion = "9.9.9"\n`,
		);
		// Staged mode refuses a relevant input whose worktree state no longer
		// matches the index; the mutation is complete before the call, so this
		// refusal is deterministic. Assert the refusal names the mutated path.
		expect(() => captureSnapshot(root, out, "staged")).toThrow(/Cargo\.lock/);
		expect(readFileSync(out, "utf8")).toBe("old-output");
		for (const name of readdirSync(resolve(root, "out"))) {
			expect(name.startsWith(".sbom-snapshot-")).toBe(false);
		}
	});
});

describe("SBOM vendor provenance", () => {
	test("a patched path crate is classified as source path, not registry", () => {
		const { root, registryCrate } = makeVendoredRoot();
		const content = captureContent(root);
		const foreign = content.rust.packages.find((p) => p.name === "foreign");
		if (foreign === undefined) throw new Error("fixture graph must contain the vendored foreign crate");
		expect(foreign.source).toBe("path");
		expect(foreign.direct).toBe(true);
		// The vendored package must not be misreported as a workspace member.
		expect(content.rust.packages.some((p) => p.name === "foreign" && p.source === "workspace")).toBe(false);
		// Registry contrast from an installed, cached crate, plus the workspace
		// member itself: one graph must separate all three source kinds.
		const cached = content.rust.packages.find((p) => p.name === registryCrate.name);
		if (cached === undefined) throw new Error(`fixture graph must contain cached crate ${registryCrate.name}`);
		expect(cached.source).toBe("registry");
		expect(content.rust.packages.find((p) => p.name === "pi")?.source).toBe("workspace");
	});

	test("vendored pin inventory covers nested source and provenance files, not residue", () => {
		const { root } = makeVendoredRoot();
		const paths = readVendorSourcePins(root).map((pin) => pin.path);
		expect(paths).toContain("vendor/foreign-0.1.0/Cargo.toml");
		expect(paths).toContain("vendor/foreign-0.1.0/src/lib.rs");
		expect(paths).toContain("vendor/foreign-0.1.0/LICENSE");
		expect(paths).toContain("vendor/foreign-0.1.0/VENDORED.txt");
		// Untracked build residue inside the vendored tree has no indexed
		// provenance and must never enter the pin set.
		mkdirSync(resolve(root, "vendor/foreign-0.1.0/target"), { recursive: true });
		writeFileSync(resolve(root, "vendor/foreign-0.1.0/target/residue.o"), "build residue\n");
		expect(readVendorSourcePins(root).map((pin) => pin.path)).toEqual(paths);
	});

	test("mutating a tracked vendored source file fails SBOM verify and staged recapture", () => {
		const { root } = makeVendoredRoot();
		const out = resolve(root, "out", "sbom.json");
		mkdirSync(dirname(out), { recursive: true });
		const pre = captureSnapshot(root, out, "staged");

		// One vendored source byte edit must invalidate the digest chain...
		writeFileSync(resolve(root, "vendor/foreign-0.1.0/src/lib.rs"), "pub fn changed() {}\n");
		const drift = verifySnapshot(pre, captureContent(root));
		expect(drift.length).toBeGreaterThan(0);
		expect(drift.some((d) => d.includes("content digest drift"))).toBe(true);

		// ...and the staged re-capture must refuse the same edit while it is
		// unstaged, then accept it only after staging pins the new bytes.
		expect(() => captureSnapshot(root, out, "staged")).toThrow(/unstaged worktree changes/);
		git(root, "add", "vendor/foreign-0.1.0/src/lib.rs");
		const post = captureSnapshot(root, out, "staged");
		expect(post.contentSha256).not.toBe(pre.contentSha256);
		const pinFor = (snapshot: typeof post): string | undefined =>
			snapshot.content.rust.vendorPins?.find((pin) => pin.path === "vendor/foreign-0.1.0/src/lib.rs")?.sha256;
		expect(pinFor(post)).toBeDefined();
		expect(pinFor(post)).not.toBe(pinFor(pre));
	});
});
