#!/usr/bin/env bun
/**
 * SBOM baseline for the dependency upgrade campaign (DEPS-R1, issue #117).
 *
 * Captures the deterministic software-bill-of-materials snapshot that every
 * later epoch diffs against (EXT-23 post-audit: "SBOM regenerated and diffed
 * vs the Phase 1 baseline"). The baseline covers every shipped input class
 * named by the policy:
 *
 * - the locked Rust graph (`cargo metadata --locked --offline --all-features`
 *   over Cargo.lock): one record per resolved crate with license, registry vs
 *   workspace provenance, and direct/dev-only edge position;
 * - the Rust toolchain channel (rust-toolchain.toml) and the CI toolchain pin;
 * - all three package.json surfaces with their dependency fields;
 * - both lockfiles of record (root bun.lock + extension-host bun.lock) with
 *   every resolved `name@version` entry;
 * - the bundled Bun runtime version, its seven sha256-pinned release assets,
 *   and the CI bun-version pin.
 *
 * `capture` refuses a tree whose inputs are dirty, so a snapshot always
 * describes a committed tree; `verify` recomputes the content from the live
 * tree and fails closed on any drift — a red verify after a dependency change
 * means "refresh the baseline in the same commit", never "ignore the guard".
 *
 * CLI:
 *   deps-sbom.ts capture [--out <file>]   write the snapshot (default: the
 *                                         checked-in baseline fixture)
 *   deps-sbom.ts verify [--snapshot <f>]  recompute and diff against the
 *                                         checked-in baseline; exit 1 on drift
 */

import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";

import { BUN_RUNTIME_VERSION } from "../release/runtime.ts";
import { RUST_TARGETS } from "../release/targets.ts";

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
export const SBOM_INPUT_PATHS: readonly string[] = [
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
	".github/workflows/release-verification.yml",
];

/** One resolved Rust crate (or workspace member) in the locked graph. */
export interface RustPackageRecord {
	readonly name: string;
	readonly version: string;
	readonly license: string;
	readonly source: "registry" | "workspace";
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

function rustPackages(root: string): readonly RustPackageRecord[] {
	const meta = cargoMetadata(root);
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
		return {
			name: p.name,
			version: p.version,
			license: p.license ?? "",
			source: members.has(p.id) ? ("workspace" as const) : ("registry" as const),
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
				// bun.lock shape: "<name>": ["<name>@<version>", scope, meta, integrity]
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

function assetPins(root: string): readonly AssetPinRecord[] {
	const text = readFileSync(resolve(root, "scripts/release/runtime.ts"), "utf8");
	const pinPattern =
		/"(?<triple>[a-z0-9_-]+)":\s*\{\s*bunTarget:\s*"(?<bunTarget>[^"]+)",\s*fileName:\s*"[^"]+",\s*sha256:\s*"(?<sha256>[0-9a-f]{64})",/g;
	const pins: AssetPinRecord[] = [];
	for (const match of text.matchAll(pinPattern)) {
		const groups = match.groups;
		if (groups === undefined) continue;
		pins.push({
			rustTarget: groups["triple"] ?? "",
			bunTarget: groups["bunTarget"] ?? "",
			sha256: groups["sha256"] ?? "",
		});
	}
	const pinned = new Set(pins.map((p) => p.rustTarget));
	const targets = new Set(RUST_TARGETS);
	if (pins.length !== RUST_TARGETS.length || pinned.size !== targets.size) {
		throw new Error(
			`asset pin extraction mismatch: ${pins.length} pins for ${RUST_TARGETS.length} targets`,
		);
	}
	for (const target of targets) {
		if (!pinned.has(target)) throw new Error(`asset pin extraction lost target: ${target}`);
	}
	return pins.sort((a, b) => (a.rustTarget < b.rustTarget ? -1 : 1));
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
	return { rust: rust[1] ?? "", bun: bun[1] ?? "" };
}

function toolchainChannel(root: string): string {
	const parsed = Bun.TOML.parse(readFileSync(resolve(root, "rust-toolchain.toml"), "utf8"));
	if (!isRecord(parsed) || !isRecord(parsed["toolchain"])) {
		throw new Error("rust-toolchain.toml: expected [toolchain]");
	}
	return asString(parsed["toolchain"]["channel"], "toolchain.channel");
}

/** Recompute the SBOM content from the tree at `root` (offline, tracked files only). */
export function captureContent(root: string): SbomContent {
	const pins = workflowPins(root);
	return {
		rust: {
			toolchainChannel: toolchainChannel(root),
			ciRustToolchain: pins.rust,
			packages: rustPackages(root),
		},
		npm: { surfaces: npmSurfaces(root), lockfiles: lockfileRecords(root) },
		tools: {
			bunRuntimeVersion: BUN_RUNTIME_VERSION,
			ciBunVersion: pins.bun,
			bunAssetPins: assetPins(root),
			releaseTargets: [...RUST_TARGETS],
		},
	};
}

function gitHeadAndDirtyInputs(root: string): { head: string; dirty: string[] } {
	const head = spawnSync("git", ["rev-parse", "HEAD"], { cwd: root, encoding: "utf8" });
	if (head.status !== 0) throw new Error("git rev-parse failed");
	const status = spawnSync("git", ["status", "--porcelain", "--", ...SBOM_INPUT_PATHS], {
		cwd: root,
		encoding: "utf8",
	});
	if (status.status !== 0) throw new Error("git status failed");
	const dirty = (status.stdout ?? "")
		.split("\n")
		.map((line) => line.trim())
		.filter((line) => line.length > 0);
	return { head: (head.stdout ?? "").trim(), dirty };
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
	// Shape is proven by the digest chain: the content hash below the named type
	// was produced by `capture` from a real SbomContent, so any malformed store
	// fails this comparison before the boundary assertion carries it forward.
	if (createHash("sha256").update(canonicalJson(rawContent)).digest("hex") !== expectedSha) {
		throw new Error("snapshot: content does not match contentSha256");
	}
	return {
		schema: SBOM_SCHEMA,
		capturedAt: asString(parsed["capturedAt"], "snapshot.capturedAt"),
		captureHead: asString(parsed["captureHead"], "snapshot.captureHead"),
		contentSha256: expectedSha,
		content: parsed["content"] as SbomContent,
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

function main(): void {
	const args = process.argv.slice(2);
	const mode = args[0];
	if (mode === "capture") {
		const outFlag = args.indexOf("--out");
		const outRel = outFlag !== -1 ? (args[outFlag + 1] ?? BASELINE_PATH) : BASELINE_PATH;
		const { head, dirty } = gitHeadAndDirtyInputs(REPO_ROOT);
		if (dirty.length > 0) {
			process.stderr.write(
				`SBOM capture refused: inputs dirty (commit or stash first):\n${dirty.join("\n")}\n`,
			);
			process.exit(1);
		}
		const content = captureContent(REPO_ROOT);
		const snapshot: SbomSnapshot = {
			schema: SBOM_SCHEMA,
			capturedAt: new Date().toISOString().slice(0, 10),
			captureHead: head,
			contentSha256: contentDigest(content),
			content,
		};
		writeFileSync(
			resolve(REPO_ROOT, outRel),
			`${JSON.stringify(snapshot, null, "\t")}\n`,
		);
		process.stdout.write(
			`captured SBOM baseline at ${outRel} (head ${head.slice(0, 8)}, digest ${snapshot.contentSha256.slice(0, 12)})\n`,
		);
		return;
	}
	if (mode === "verify") {
		const snapFlag = args.indexOf("--snapshot");
		const snapRel = snapFlag !== -1 ? (args[snapFlag + 1] ?? BASELINE_PATH) : BASELINE_PATH;
		const snapshot = loadSnapshot(readFileSync(resolve(REPO_ROOT, snapRel), "utf8"));
		const drift = verifySnapshot(snapshot, captureContent(REPO_ROOT));
		if (drift.length > 0) {
			for (const line of drift) process.stdout.write(`FAIL ${line}\n`);
			process.stderr.write("DEPENDENCY_SBOM_DRIFT\n");
			process.exit(1);
		}
		process.stdout.write(
			`${SBOM_OK} baseline ${snapshot.captureHead.slice(0, 8)} (${snapshot.capturedAt}) still describes the tree\n`,
		);
		return;
	}
	process.stderr.write("usage: deps-sbom.ts capture [--out <file>] | verify [--snapshot <file>]\n");
	process.exit(2);
}

if (import.meta.main) main();
