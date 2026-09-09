#!/usr/bin/env bun
/**
 * SBOM baseline for the dependency upgrade campaign (DEPS-R1, issue #117).
 *
 * Captures the deterministic software-bill-of-materials snapshot that every
 * later epoch diffs against (EXT-23 post-audit: "SBOM regenerated and diffed
 * vs the Phase 1 baseline"). The baseline covers every shipped input class
 * named by the policy.
 *
 * `capture` refuses a tree whose inputs are dirty unless `--staged-inputs` is
 * given; in the staged mode the snapshot records an honest indexed-provenance
 * claim against the real base commit, and every input is rechecked immediately
 * after capture and again before the atomic file replacement. `verify`
 * recomputes the content from the live tree and fails closed on any drift.
 *
 * CLI:
 *   deps-sbom.ts capture [--staged-inputs] [--out <file>]
 *   deps-sbom.ts verify [--snapshot <file>]
 */

import { spawnSync } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { closeSync, openSync, readFileSync, readdirSync, renameSync, rmSync, statSync, writeSync } from "node:fs";
import { dirname, join, posix, resolve } from "node:path";

import {
	beginStagedInputCapture,
	parseStagedInputProvenance,
	assertCaptureOutputPathsDisjoint,
} from "./capture-inputs.ts";
import type { StagedInputProvenance, StagedInputCapture } from "./capture-inputs.ts";
import { assertCompiledVendorCoverage, vendorInputScopes, readVendorSourcePins } from "./vendor-provenance.ts";
import type { VendorSourcePin } from "./vendor-provenance.ts";

export const REPO_ROOT = resolve(import.meta.dirname, "../..");

/** Snapshot schema discriminator. */
export const SBOM_SCHEMA = "pi.deps.sbom.v1";

/** Checked-in Phase 1 baseline (the per-epoch diff anchor). */
export const BASELINE_PATH = "scripts/verification/fixtures/deps-r1-sbom-baseline.json";

/** Green sentinel emitted by `verify`. */
export const SBOM_OK = "DEPENDENCY_SBOM_OK";

const CARGO_METADATA_ARGV = [
	"metadata",
	"--format-version",
	"1",
	"--locked",
	"--offline",
	"--all-features",
] as const;

/** Every tracked file the snapshot content is derived from. */
function sbomInputPaths(root: string): readonly string[] {
	return [
	".gitattributes",
	".cargo/config",
	".cargo/config.toml",
	"Cargo.toml",
	"Cargo.lock",
	...["pi", "pi-agent", "pi-ai", "pi-ext", "pi-tui"].map((c) => `crates/${c}/Cargo.toml`),
	"rust-toolchain.toml",
	"package.json",
	"bun.lock",
	"packages/extension-host/package.json",
	"packages/extension-host/bun.lock",
	"packages/pi-tui-protocol/package.json",
	"scripts/release/runtime.ts",
	"scripts/release/targets.ts",
	"scripts/verification/capture-inputs.ts",
	"scripts/verification/deps-sbom.ts",
	"scripts/verification/vendor-provenance.ts",
	...vendorInputScopes(root),
	".github/workflows/release-verification.yml",
	];
}

/** One resolved Rust crate (or workspace member) in the locked graph. */
export interface RustPackageRecord {
	readonly name: string;
	readonly version: string;
	readonly license: string;
	/**
	 * `workspace` for workspace members, `path` for non-member local-path
	 * packages (e.g. approved vendor crates), `registry` for all other
	 * external sources. Determined by cargo metadata `source` field, not
	 * merely workspace membership, so a patched path crate is never
	 * misreported as registry.
	 */
	readonly source: "registry" | "workspace" | "path";
	readonly direct: boolean;
	readonly devOnly: boolean;
}

/** Dependency fields of one package.json surface, as written. */
export interface NpmSurfaceRecord {
	readonly path: string;
	readonly dependencies: Record<string, string>;
	readonly devDependencies: Record<string, string>;
	readonly optionalDependencies: Record<string, string>;
	readonly peerDependencies: Record<string, string>;
}

/** One resolved `name@version` entry of a lockfile of record. */
export interface LockPackageRecord {
	readonly name: string;
	readonly version: string;
}

/** One lockfile of record with its full resolution list. */
export interface LockfileRecord {
	readonly path: string;
	readonly lockfileVersion: number;
	readonly packages: readonly LockPackageRecord[];
}

/** One sha256-pinned Bun release asset staged into archives. */
export interface AssetPinRecord {
	readonly rustTarget: string;
	readonly bunTarget: string;
	readonly sha256: string;
}

/** The deterministic content — identical for identical trees. */
export interface SbomContent {
	readonly rust: {
		readonly toolchainChannel: string;
		readonly ciRustToolchain: string;
		readonly packages: readonly RustPackageRecord[];
		/** Tracked vendored build source / manifest / provenance / license pins. */
		readonly vendorPins?: readonly VendorSourcePin[];
	};
	readonly npm: {
		readonly surfaces: readonly NpmSurfaceRecord[];
		readonly lockfiles: readonly LockfileRecord[];
	};
	readonly tools: {
		readonly bunRuntimeVersion: string;
		readonly ciBunVersion: string;
		readonly bunAssetPins: readonly AssetPinRecord[];
		readonly releaseTargets: readonly string[];
	};
}

/** The stored snapshot: content plus provenance and its digest. */
export interface SbomSnapshot {
	readonly schema: string;
	readonly capturedAt: string;
	readonly captureHead: string;
	readonly contentSha256: string;
	readonly content: SbomContent;
	readonly stagedInputProvenance?: StagedInputProvenance;
}

function isRecord(value: unknown): value is Record<string, unknown> {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}

function asString(value: unknown, what: string): string {
	if (typeof value !== "string") throw new Error(`${what}: expected string`);
	return value;
}

function stringRecord(value: unknown, what: string): Record<string, string> {
	if (!isRecord(value)) throw new Error(`${what}: expected record`);
	const out: Record<string, string> = {};
	for (const [k, v] of Object.entries(value)) out[k] = asString(v, `${what}[${k}]`);
	return out;
}

/** Deterministic JSON: sorted keys, no whitespace, undefined dropped. */
export function canonicalJson(value: unknown): string {
	if (value === null || typeof value !== "object") return JSON.stringify(value) ?? "null";
	if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
	const entries = Object.entries(value)
		.filter((entry) => entry[1] !== undefined)
		.sort((a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0))
		.map(([k, v]) => `${JSON.stringify(k)}:${canonicalJson(v)}`);
	return `{${entries.join(",")}}`;
}

/** sha256 over the canonical content serialization — the drift anchor. */
export function contentDigest(content: SbomContent): string {
	return createHash("sha256").update(canonicalJson(content)).digest("hex");
}

interface CargoMetadata {
	packages: Array<{
		name: string;
		version: string;
		license: string | null;
		id: string;
		manifest_path: string;
		/** `null` for path/workspace packages; `registry+...`/`git+...` otherwise. */
		source: string | null;
	}>;
	workspace_members: string[];
	resolve: {
		nodes: Array<{
			id: string;
			deps: Array<{ pkg: string; dep_kinds: Array<{ kind: string | null }> }>;
		}>;
	};
}

function cargoMetadata(root: string): CargoMetadata {
	const result = spawnSync("cargo", [...CARGO_METADATA_ARGV], {
		cwd: root,
		encoding: "utf8",
		timeout: 10 * 60_000,
		maxBuffer: 64 * 1024 * 1024,
	});
	if (result.status !== 0) {
		throw new Error(
			`cargo metadata failed (${result.status}): ${(result.stderr ?? "").slice(0, 400)}`,
		);
	}
	const parsed: unknown = JSON.parse(result.stdout);
	if (!isRecord(parsed)) throw new Error("cargo metadata: expected object");
	const packages = parsed["packages"];
	const workspaceMembers = parsed["workspace_members"];
	const resolveNode = parsed["resolve"];
	if (!Array.isArray(packages) || !Array.isArray(workspaceMembers)) {
		throw new Error("cargo metadata: malformed packages/workspace_members");
	}
	const nodes = isRecord(resolveNode) && Array.isArray(resolveNode["nodes"])
		? resolveNode["nodes"]
		: [];
	return {
	packages: packages.map((p) => {
		if (!isRecord(p)) throw new Error("cargo metadata: malformed package");
		return {
			name: asString(p["name"], "package.name"),
			version: asString(p["version"], "package.version"),
			license: typeof p["license"] === "string" ? p["license"] : null,
			id: asString(p["id"], "package.id"),
			manifest_path: asString(p["manifest_path"], "package.manifest_path"),
			source: typeof p["source"] === "string" ? p["source"] : null,
		};
	}),
	workspace_members: workspaceMembers.map((m) => asString(m, "workspace_member")),
		resolve: { nodes: nodes.map((n) => {
			if (!isRecord(n)) throw new Error("cargo metadata: malformed resolve node");
			const deps = Array.isArray(n["deps"]) ? n["deps"] : [];
			return {
				id: asString(n["id"], "node.id"),
				deps: deps.map((d) => {
					if (!isRecord(d)) throw new Error("cargo metadata: malformed dep");
					const kinds = Array.isArray(d["dep_kinds"]) ? d["dep_kinds"] : [];
					return {
						pkg: asString(d["pkg"], "dep.pkg"),
						dep_kinds: kinds.map((k) => {
							if (!isRecord(k)) throw new Error("cargo metadata: malformed dep_kind");
							return { kind: typeof k["kind"] === "string" ? k["kind"] : null };
						}),
					};
				}),
			};
		}) },
	};
}

function rustPackages(root: string, vendorPins: readonly VendorSourcePin[]): readonly RustPackageRecord[] {
	const meta = cargoMetadata(root);
	assertCompiledVendorCoverage(root, meta.packages, vendorPins);
	const members = new Set(meta.workspace_members);
	const byId = new Map(meta.packages.map((p) => [p.id, p]));
	const directEdges = new Map<string, Set<string>>();
	for (const memberId of meta.workspace_members) {
		const node = meta.resolve.nodes.find((n) => n.id === memberId);
		if (node === undefined) continue;
		for (const dep of node.deps) {
			const pkg = byId.get(dep.pkg);
			if (pkg === undefined || members.has(pkg.id)) continue;
			const kinds = dep.dep_kinds.length
				? dep.dep_kinds.map((k) => k.kind ?? "normal")
				: ["normal"];
			const entry = directEdges.get(pkg.id) ?? new Set<string>();
			for (const kind of kinds) entry.add(kind);
			directEdges.set(pkg.id, entry);
		}
	}
	const records = meta.packages.map((p) => {
		const kinds = directEdges.get(p.id);
		let source: RustPackageRecord["source"];
		if (members.has(p.id)) {
			source = "workspace";
		} else if (p.source === null) {
			source = "path";
		} else {
			source = "registry";
		}
		return {
			name: p.name,
			version: p.version,
			license: p.license ?? "",
			source,
			direct: kinds !== undefined,
			devOnly: kinds !== undefined && [...kinds].every((k) => k === "dev"),
		};
	});
	return [...records].sort(
		(a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : a.version < b.version ? -1 : 1),
	);
}

const NPM_SURFACES: readonly string[] = [
	"package.json",
	"packages/extension-host/package.json",
	"packages/pi-tui-protocol/package.json",
];

const LOCKFILES: readonly string[] = ["bun.lock", "packages/extension-host/bun.lock"];

function npmSurfaces(root: string): readonly NpmSurfaceRecord[] {
	return NPM_SURFACES.map((rel) => {
		const parsed: unknown = JSON.parse(readFileSync(resolve(root, rel), "utf8"));
		if (!isRecord(parsed)) throw new Error(`${rel}: expected object`);
		return {
			path: rel,
			dependencies: isRecord(parsed["dependencies"])
				? stringRecord(parsed["dependencies"], `${rel} dependencies`)
				: {},
			devDependencies: isRecord(parsed["devDependencies"])
				? stringRecord(parsed["devDependencies"], `${rel} devDependencies`)
				: {},
			optionalDependencies: isRecord(parsed["optionalDependencies"])
				? stringRecord(parsed["optionalDependencies"], `${rel} optionalDependencies`)
				: {},
			peerDependencies: isRecord(parsed["peerDependencies"])
				? stringRecord(parsed["peerDependencies"], `${rel} peerDependencies`)
				: {},
		};
	});
}

function splitPackageKey(key: string): { name: string; version: string } {
	if (key.startsWith("@")) {
		const at = key.indexOf("@", 1);
		if (at > 1) return { name: key.slice(0, at), version: key.slice(at + 1) };
	} else {
		const at = key.indexOf("@");
		if (at > 0) return { name: key.slice(0, at), version: key.slice(at + 1) };
	}
	throw new Error(`lockfile package key is not name@version: ${key}`);
}

function lockfileRecords(root: string): readonly LockfileRecord[] {
	return LOCKFILES.map((rel) => {
		const text = readFileSync(resolve(root, rel), "utf8");
		const parsed: unknown = JSON.parse(text.replace(/,(\s*[}\]])/g, "$1"));
		if (!isRecord(parsed) || !isRecord(parsed["packages"])) {
			throw new Error(`${rel}: expected bun.lock with packages record`);
		}
		const packages = Object.entries(parsed["packages"])
			.map(([, value]) => {
				const id = Array.isArray(value) && typeof value[0] === "string" ? value[0] : null;
				if (id === null) throw new Error(`${rel}: package entry without name@version id`);
				return splitPackageKey(id);
			})
			.sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : a.version < b.version ? -1 : 1))
			.map((nv) => ({ name: nv.name, version: nv.version }));
		return {
			path: rel,
			lockfileVersion: typeof parsed["lockfileVersion"] === "number"
				? parsed["lockfileVersion"]
				: 0,
			packages,
		};
	});
}

const ASSET_PIN_PATTERN =
	/"(?<triple>[a-z0-9_-]+)":\s*\{\s*bunTarget:\s*"(?<bunTarget>[^"]+)",\s*fileName:\s*"[^"]+",\s*sha256:\s*"(?<sha256>[0-9a-f]{64})",/g;

const BUN_RUNTIME_VERSION_PATTERN =
	/export\s+const\s+BUN_RUNTIME_VERSION\s*=\s*"([^"]+)"\s*;/g;

const RUST_TARGETS_PATTERN =
	/export\s+const\s+RUST_TARGETS\s*=\s*\[([\s\S]*?)\]\s*as\s+const\s*;/g;

function extractBunRuntimeVersion(text: string, what: string): string {
	const present = text.includes("export const BUN_RUNTIME_VERSION");
	const matches = [...text.matchAll(BUN_RUNTIME_VERSION_PATTERN)];
	if (matches.length === 0) {
		const problem = present ? "non-literal or malformed" : "missing";
		throw new Error(`${what}: ${problem} BUN_RUNTIME_VERSION declaration`);
	}
	if (matches.length > 1) throw new Error(`${what}: duplicate BUN_RUNTIME_VERSION declaration`);
	const declaration = matches[0];
	if (declaration === undefined) {
		throw new Error(`${what}: BUN_RUNTIME_VERSION declaration did not match`);
	}
	const version = declaration[1];
	if (version === undefined || version.length === 0) {
		throw new Error(`${what}: BUN_RUNTIME_VERSION is empty`);
	}
	return version;
}

function extractBunAssetPins(
	text: string,
	releaseTargets: readonly string[],
	what: string,
): readonly AssetPinRecord[] {
	const pins: AssetPinRecord[] = [];
	for (const match of text.matchAll(ASSET_PIN_PATTERN)) {
		const groups = match.groups;
		if (groups === undefined) {
			throw new Error(`${what}: asset pin declaration did not capture its groups`);
		}
		const rustTarget = groups["triple"];
		const bunTarget = groups["bunTarget"];
		const sha256 = groups["sha256"];
		if (rustTarget === undefined || bunTarget === undefined || sha256 === undefined) {
			throw new Error(`${what}: asset pin declaration is missing a capture value`);
		}
		pins.push({ rustTarget, bunTarget, sha256 });
	}
	const pinned = new Set(pins.map((p) => p.rustTarget));
	if (pins.length !== releaseTargets.length || pinned.size !== releaseTargets.length) {
		throw new Error(
			`${what}: asset pin extraction mismatch: ${pins.length} pins for ${releaseTargets.length} targets`,
		);
	}
	for (const target of releaseTargets) {
		if (!pinned.has(target)) throw new Error(`${what}: asset pin extraction lost target: ${target}`);
	}
	return pins.sort((a, b) => (a.rustTarget < b.rustTarget ? -1 : 1));
}

function readRustTargets(root: string): readonly string[] {
	const text = readFileSync(resolve(root, "scripts/release/targets.ts"), "utf8");
	const present = text.includes("export const RUST_TARGETS");
	const matches = [...text.matchAll(RUST_TARGETS_PATTERN)];
	if (matches.length === 0) {
		const problem = present ? "non-literal or malformed" : "missing";
		throw new Error(`scripts/release/targets.ts: ${problem} RUST_TARGETS declaration`);
	}
	if (matches.length > 1) throw new Error("scripts/release/targets.ts: duplicate RUST_TARGETS declaration");
	const declaration = matches[0];
	if (declaration === undefined) {
		throw new Error("scripts/release/targets.ts: RUST_TARGETS declaration did not match");
	}
	const arrayText = declaration[1];
	if (arrayText === undefined) {
		throw new Error("scripts/release/targets.ts: RUST_TARGETS declaration did not capture its array body");
	}
	const targets: string[] = [];
	for (const segment of arrayText.split(",")) {
		const literal = segment.match(/^\s*"([^"]*)"\s*$/);
		if (literal === null) {
			if (segment.trim().length === 0) continue;
			throw new Error(`scripts/release/targets.ts: non-literal target in RUST_TARGETS: ${segment.trim()}`);
		}
		const value = literal[1];
		if (value === undefined || value.length === 0) {
			throw new Error("scripts/release/targets.ts: empty target in RUST_TARGETS");
		}
		if (targets.includes(value)) {
			throw new Error(`scripts/release/targets.ts: duplicate target in RUST_TARGETS: ${value}`);
		}
		if (!/^[a-z0-9_-]+$/.test(value) || !value.includes("-")) {
			throw new Error(`scripts/release/targets.ts: invalid target in RUST_TARGETS: ${value}`);
		}
		targets.push(value);
	}
	if (targets.length === 0) throw new Error("scripts/release/targets.ts: RUST_TARGETS is empty");
	return targets;
}

function workflowPins(root: string): { rust: string; bun: string } {
	const text = readFileSync(
		resolve(root, ".github/workflows/release-verification.yml"),
		"utf8",
	);
	const rust = text.match(/toolchain:\s*(\S+)/);
	const bun = text.match(/bun-version:\s*(\S+)/);
	if (rust === null || bun === null) {
		throw new Error("release-verification.yml: toolchain/bun-version pins not found");
	}
	const rustPin = rust[1];
	const bunPin = bun[1];
	if (rustPin === undefined || bunPin === undefined) {
		throw new Error("release-verification.yml: toolchain/bun-version pins did not capture a value");
	}
	return { rust: rustPin, bun: bunPin };
}

export function toolchainChannel(root: string): string {
	const parsed = Bun.TOML.parse(readFileSync(resolve(root, "rust-toolchain.toml"), "utf8"));
	if (!isRecord(parsed) || !isRecord(parsed["toolchain"])) {
		throw new Error("rust-toolchain.toml: expected [toolchain]");
	}
	return asString(parsed["toolchain"]["channel"], "toolchain.channel");
}

/** Recompute the SBOM content from the tree at `root` (offline, tracked files only). */
export function captureContent(root: string): SbomContent {
	const vendorPins = readVendorSourcePins(root);
	const runtimeText = readFileSync(resolve(root, "scripts/release/runtime.ts"), "utf8");
	const bunRuntimeVersion = extractBunRuntimeVersion(runtimeText, "scripts/release/runtime.ts");
	const releaseTargets = readRustTargets(root);
	const bunAssetPins = extractBunAssetPins(runtimeText, releaseTargets, "scripts/release/runtime.ts");
	const pins = workflowPins(root);
	return {
		rust: {
			toolchainChannel: toolchainChannel(root),
			ciRustToolchain: pins.rust,
			packages: rustPackages(root, vendorPins),
			vendorPins: vendorPins.length > 0 ? vendorPins : undefined,
		},
		npm: { surfaces: npmSurfaces(root), lockfiles: lockfileRecords(root) },
		tools: {
			bunRuntimeVersion,
			ciBunVersion: pins.bun,
			bunAssetPins,
			releaseTargets: [...releaseTargets],
		},
	};
}

function gitDirtyInputs(root: string): string[] {
	const status = spawnSync("git", ["status", "--porcelain", "--", ...sbomInputPaths(root)], {
		cwd: root,
		encoding: "utf8",
	});
	if (status.status !== 0) throw new Error("git status failed");
	return (status.stdout ?? "")
		.split("\n")
		.map((line) => line.trim())
		.filter((line) => line.length > 0);
}

/** Validate and load a snapshot document (schema + digest chain). */
export function loadSnapshot(text: string): SbomSnapshot {
	const parsed: unknown = JSON.parse(text);
	if (!isRecord(parsed)) throw new Error("snapshot: expected object");
	if (parsed["schema"] !== SBOM_SCHEMA) {
		throw new Error(`snapshot: wrong schema ${String(parsed["schema"])}`);
	}
	const rawContent: unknown = parsed["content"];
	if (!isRecord(rawContent)) throw new Error("snapshot: missing content");
	const expectedSha = asString(parsed["contentSha256"], "snapshot.contentSha256");
	if (createHash("sha256").update(canonicalJson(rawContent)).digest("hex") !== expectedSha) {
		throw new Error("snapshot: content does not match contentSha256");
	}
	const captureHead = asString(parsed["captureHead"], "snapshot.captureHead");
	let stagedInputProvenance: StagedInputProvenance | undefined;
	const rawProvenance = parsed["stagedInputProvenance"];
	if (rawProvenance !== undefined) {
		stagedInputProvenance = parseStagedInputProvenance(rawProvenance, "snapshot.stagedInputProvenance");
		if (stagedInputProvenance.baseHead !== captureHead) {
			throw new Error("snapshot: stagedInputProvenance.baseHead does not match captureHead");
		}
	}
	return {
		schema: SBOM_SCHEMA,
		capturedAt: asString(parsed["capturedAt"], "snapshot.capturedAt"),
		captureHead,
		contentSha256: expectedSha,
		content: parsed["content"] as SbomContent,
		stagedInputProvenance,
	};
}

/** Pure drift check; empty array means the baseline still describes the tree. */
export function verifySnapshot(snapshot: SbomSnapshot, live: SbomContent): string[] {
	const drift: string[] = [];
	const expected = contentDigest(snapshot.content);
	const actual = contentDigest(live);
	if (expected !== actual) {
		drift.push(
			`content digest drift: baseline ${snapshot.contentSha256} (captured ${snapshot.capturedAt} at ${snapshot.captureHead}) != live ${actual}`,
		);
	}
	const baselineRust = new Map(snapshot.content.rust.packages.map((p) => [`${p.name}@${p.version}`, p]));
	const liveRust = new Map(live.rust.packages.map((p) => [`${p.name}@${p.version}`, p]));
	for (const [key, record] of baselineRust) {
		if (!liveRust.has(key)) drift.push(`rust: ${key} left the locked graph`);
	}
	for (const key of liveRust.keys()) {
		if (!baselineRust.has(key)) drift.push(`rust: ${key} entered the locked graph`);
	}
	if (snapshot.content.rust.toolchainChannel !== live.rust.toolchainChannel) {
		drift.push(
			`rust: toolchain channel ${snapshot.content.rust.toolchainChannel} -> ${live.rust.toolchainChannel}`,
		);
	}
	for (const lockfile of live.npm.lockfiles) {
		const baselineLockfile = snapshot.content.npm.lockfiles.find((l) => l.path === lockfile.path);
		if (baselineLockfile === undefined) {
			drift.push(`npm: lockfile ${lockfile.path} missing from baseline`);
			continue;
		}
		const baselinePackages = new Set(baselineLockfile.packages.map((p) => `${p.name}@${p.version}`));
		for (const pkg of lockfile.packages) {
			if (!baselinePackages.has(`${pkg.name}@${pkg.version}`)) {
				drift.push(`npm: ${lockfile.path} resolution ${pkg.name}@${pkg.version} not in baseline`);
			}
		}
	}
	if (snapshot.content.tools.bunRuntimeVersion !== live.tools.bunRuntimeVersion) {
		drift.push(
			`tools: bundled Bun runtime ${snapshot.content.tools.bunRuntimeVersion} -> ${live.tools.bunRuntimeVersion}`,
		);
	}
	const baselinePins = new Map(snapshot.content.tools.bunAssetPins.map((p) => [p.rustTarget, p.sha256]));
	for (const pin of live.tools.bunAssetPins) {
		if (baselinePins.get(pin.rustTarget) !== pin.sha256) {
			drift.push(`tools: Bun asset pin ${pin.rustTarget} drifted`);
		}
	}
	return drift;
}

/**
 * Capture a full SBOM snapshot, optionally recording staged-input provenance,
 * and publish it to `outPath` through an atomic single-file replacement.
 */
export function captureSnapshot(
	root: string,
	outPath: string,
	inputMode: "committed" | "staged" = "committed",
): SbomSnapshot {
	const absoluteOut = resolve(outPath);
	const inputPaths = sbomInputPaths(root);
	readVendorSourcePins(root);

	// Guard output overlap before any producer work or Git reads.
	assertCaptureOutputPathsDisjoint(root, inputPaths, [absoluteOut]);

	if (inputMode === "committed") {
		const dirty = gitDirtyInputs(root);
		if (dirty.length > 0) {
			throw new Error(
				`SBOM capture refused: inputs dirty (commit or stash first):\n${dirty.join("\n")}\n`,
			);
		}
	}
	const handle = beginStagedInputCapture(root, inputPaths);
	const captureHead = handle.provenance.baseHead;
	const stagedInputProvenance = inputMode === "staged" ? handle.provenance : undefined;

	const content = captureContent(root);

	// Immediate post-capture recheck: inputs must still match the captured index.
	handle.assertUnchanged();

	const snapshot: SbomSnapshot = {
		schema: SBOM_SCHEMA,
		capturedAt: new Date().toISOString().slice(0, 10),
		captureHead,
		contentSha256: contentDigest(content),
		content,
		stagedInputProvenance,
	};

	const text = `${JSON.stringify(snapshot, null, "\t")}\n`;

	// Validate the serialized document (digest chain and metadata) before writing.
	loadSnapshot(text);

	const outDir = dirname(absoluteOut);
	const tmpName = `.sbom-snapshot-${process.pid}-${randomUUID()}.tmp.json`;
	const tmpPath = resolve(outDir, tmpName);
	let written = false;
	try {
		const fd = openSync(tmpPath, "wx");
		written = true;
		try {
			writeSync(fd, Buffer.from(text, "utf8"));
		} finally {
			closeSync(fd);
		}
		// Final pre-rename recheck: no input may have changed while the temp was written.
		handle.assertUnchanged();
		renameSync(tmpPath, absoluteOut);
	} catch (err) {
		if (written) {
			rmSync(tmpPath, { force: true });
		}
		throw err;
	}

	return snapshot;
}

class UsageError extends Error {}

function parseCaptureArgs(args: string[]): { out: string; inputMode: "committed" | "staged" } {
	let out: string | undefined;
	let inputMode: "committed" | "staged" = "committed";
	for (let i = 0; i < args.length; i++) {
		const a = args[i];
		if (a === "--staged-inputs") {
			if (inputMode === "staged") throw new UsageError("duplicate --staged-inputs");
			inputMode = "staged";
		} else if (a === "--out") {
			if (out !== undefined) throw new UsageError("duplicate --out");
			const v = args[i + 1];
			if (v === undefined || v.startsWith("--")) throw new UsageError("--out requires a value");
			out = v;
			i++;
		} else {
			throw new UsageError(`unknown capture flag ${a}`);
		}
	}
	return { out: out ?? BASELINE_PATH, inputMode };
}

function parseVerifyArgs(args: string[]): { snapshot: string } {
	let snapshot: string | undefined;
	for (let i = 0; i < args.length; i++) {
		const a = args[i];
		if (a === "--staged-inputs") {
			throw new UsageError("--staged-inputs is not valid for verify");
		}
		if (a === "--snapshot") {
			if (snapshot !== undefined) throw new UsageError("duplicate --snapshot");
			const v = args[i + 1];
			if (v === undefined || v.startsWith("--")) throw new UsageError("--snapshot requires a value");
			snapshot = v;
			i++;
		} else {
			throw new UsageError(`unknown verify flag ${a}`);
		}
	}
	return { snapshot: snapshot ?? BASELINE_PATH };
}

function main(): void {
	const args = process.argv.slice(2);
	const mode = args[0];
	const rest = args.slice(1);
	try {
		if (mode === "capture") {
			const { out, inputMode } = parseCaptureArgs(rest);
			const outPath = resolve(REPO_ROOT, out);
			const snapshot = captureSnapshot(REPO_ROOT, outPath, inputMode);
			if (snapshot.stagedInputProvenance !== undefined) {
				process.stdout.write(
					`captured indexed SBOM baseline at ${out} (based on commit ${snapshot.captureHead.slice(0, 8)}, digest ${snapshot.contentSha256.slice(0, 12)})\n`,
				);
			} else {
				process.stdout.write(
					`captured SBOM baseline at ${out} (head ${snapshot.captureHead.slice(0, 8)}, digest ${snapshot.contentSha256.slice(0, 12)})\n`,
				);
			}
			return;
		}
		if (mode === "verify") {
			const { snapshot } = parseVerifyArgs(rest);
			const doc = loadSnapshot(readFileSync(resolve(REPO_ROOT, snapshot), "utf8"));
			const drift = verifySnapshot(doc, captureContent(REPO_ROOT));
			if (drift.length > 0) {
				for (const line of drift) process.stdout.write(`FAIL ${line}\n`);
				process.stderr.write("DEPENDENCY_SBOM_DRIFT\n");
				process.exit(1);
			}
			process.stdout.write(
				`${SBOM_OK} baseline ${doc.captureHead.slice(0, 8)} (${doc.capturedAt}) still describes the tree\n`,
			);
			return;
		}
		throw new UsageError("expected capture or verify");
	} catch (err) {
		if (err instanceof UsageError) {
			process.stderr.write(`${err.message}\n`);
			process.stderr.write(
				"usage: deps-sbom.ts capture [--staged-inputs] [--out <file>] | verify [--snapshot <file>]\n",
			);
			process.exit(2);
		}
		const message = err instanceof Error ? err.message : String(err);
		process.stderr.write(`${message}\n`);
		process.exit(1);
	}
}

if (import.meta.main) main();
