/** Root Cargo patch authority and tracked vendored input inventory shared by capture tools. */
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { dirname, join, relative, resolve, sep } from "node:path";

interface RootPatch {
	readonly source: string;
	readonly name: string;
	readonly spec: string | Record<string, unknown>;
}

/** Parse once at the boundary; snippets and inventory consume the same patch semantics. */
function parseRootPatches(root: string): readonly RootPatch[] {
	const parsed = Bun.TOML.parse(readFileSync(join(resolve(root), "Cargo.toml"), "utf8"));
	const patch = "patch" in parsed ? parsed.patch : undefined;
	if (patch === undefined) return [];
	if (patch === null || typeof patch !== "object" || Array.isArray(patch)) throw new Error("root Cargo.toml: [patch] must be a table");
	const entries: RootPatch[] = [];
	for (const [source, table] of Object.entries(patch)) {
		if (table === null || typeof table !== "object" || Array.isArray(table) || table instanceof Date) throw new Error(`root Cargo.toml: [patch.${source}] must be a table`);
		for (const [name, spec] of Object.entries(table)) {
			if (typeof spec === "string") { entries.push({ source, name, spec }); continue; }
			if (spec === null || typeof spec !== "object" || Array.isArray(spec) || spec instanceof Date) throw new Error(`root Cargo.toml: patch entry "${name}" must be a table or string`);
			const fields: Record<string, unknown> = { ...spec };
			if (fields["path"] !== undefined) {
				const path = fields["path"];
				if (typeof path !== "string" || path.length === 0) throw new Error(`root Cargo.toml: patch entry "${name}" has an invalid path`);
				fields["path"] = resolve(root, path);
			}
			entries.push({ source, name, spec: fields });
		}
	}
	return entries;
}

/** Serialize one TOML scalar for the generated lane manifest; fails closed rather than mis-serializing. */
function tomlScalar(value: unknown): string {
	if (typeof value === "string") {
		if (value.includes("\u007F")) throw new Error("root Cargo.toml patch value contains U+007F, which TOML basic strings cannot carry");
		return JSON.stringify(value);
	}
	if (typeof value === "number" || typeof value === "boolean") return String(value);
	if (Array.isArray(value)) return `[${value.map(tomlScalar).join(", ")}]`;
	throw new Error(`root Cargo.toml patch value cannot be serialized to TOML: ${JSON.stringify(value)}`);
}

/** Rebase root patches for standalone snippet manifests without re-resolving registry crates. */
export function rootPatchTables(root: string): string[] {
	const tables = new Map<string, string[]>();
	for (const { source, name, spec } of parseRootPatches(root)) {
		const entries = tables.get(source) ?? [];
		const value = typeof spec === "string" ? JSON.stringify(spec)
			: `{ ${Object.entries(spec).map(([key, field]) => `${key} = ${tomlScalar(field)}`).join(", ")} }`;
		const key = /^[A-Za-z0-9_-]+$/.test(name) ? name : JSON.stringify(name);
		entries.push(`${key} = ${value}`);
		tables.set(source, entries);
	}
	return [...tables].flatMap(([source, entries]) => [`[patch.${/^[A-Za-z0-9_-]+$/.test(source) ? source : JSON.stringify(source)}]`, ...entries, ""]);
}

export interface RootPatchCrate {
	readonly name: string;
	readonly path: string;
}

/** Path patches outside the approved vendor tree cannot silently evade provenance. */
export function rootPatchCrates(root: string): readonly RootPatchCrate[] {
	const crates: RootPatchCrate[] = [];
	for (const { name, spec } of parseRootPatches(root)) {
		if (typeof spec === "string" || typeof spec["path"] !== "string") continue;
		const path = relative(resolve(root), spec["path"]).split(sep).join("/");
		if (!path.startsWith("vendor/") || path.includes("\\") || /[*?\[]/.test(path)) {
			throw new Error(`vendored patch ${name} at ${path}: path must be a literal crate directory under vendor/; refusing unpinned patch bytes`);
		}
		crates.push({ name, path });
	}
	return crates;
}

/** Fixed input classes, applied to each parsed declaration rather than a parallel directory glob. */
const VENDOR_INPUT_CLASSES = ["Cargo.lock", "Cargo.toml", "Cargo.toml.orig", "LICENSE", ".cargo_vcs_info.json", "VENDORED.txt", "src/**"] as const;

export function vendorInputScopes(root: string): readonly string[] {
	return [...new Set(rootPatchCrates(root).flatMap(({ path }) => VENDOR_INPUT_CLASSES.map((suffix) => `${path}/${suffix}`)))].sort();
}

/** Reconcile Cargo's authoritative local sources before discarding manifest paths in projections. */
export function assertCompiledVendorCoverage(root: string, packages: readonly { name: string; manifest_path: string; source: string | null }[], pins: readonly VendorSourcePin[]): void {
	for (const pkg of packages) {
		if (pkg.source !== null) continue;
		const path = relative(resolve(root), dirname(pkg.manifest_path)).split(sep).join("/");
		if (path.startsWith("vendor/") && !pins.some((pin) => pin.path === `${path}/Cargo.toml`)) {
			throw new Error(`compiled vendored crate ${pkg.name} at ${path}: no pinned manifest from a root Cargo.toml path patch`);
		}
	}
}

/** One sha256-pinned vendored source file; identical shape to a file pin. */
export interface VendorSourcePin {
	readonly path: string;
	readonly sha256: string;
}

/**
 * Run a non-shell git command and return its stdout, split at NUL bytes.
 * Fails closed on any error, timeout, or nonzero status.
 */
function runGitOrThrow(root: string, args: readonly string[], what: string): string {
	const result = spawnSync("git", args, {
		cwd: root,
		encoding: "utf8",
		timeout: 30_000,
		maxBuffer: 64 * 1024 * 1024,
	});
	if (result.status !== 0) {
		throw new Error(`${what} failed: ${(result.stderr ?? "").slice(0, 300)}`);
	}
	return result.stdout ?? "";
}

/** Split NUL-terminated records; paths are never split on whitespace/newline. */
function splitNulRecords(output: string): readonly string[] {
	return output.split("\0").filter((token) => token.length > 0);
}

/**
 * All tracked, ordinary files matching the vendored-crate scopes, sorted.
 * Uses `git ls-files --stage -z` and fails closed on malformed records,
 * conflict-stage entries, and non-regular entries (symlinks, submodules).
 * The paths are relative to the repository root.
 */
export function vendorCrateInputPaths(root: string): readonly string[] {
	const crates = rootPatchCrates(root);
	if (crates.length === 0) return [];
	const scopes = [...new Set(crates.flatMap(({ path }) => VENDOR_INPUT_CLASSES.map((suffix) => `${path}/${suffix}`)))];
	const output = runGitOrThrow(root, ["ls-files", "--stage", "-z", "--", ...scopes], "git ls-files vendor");
	const paths: string[] = [];
	const seen = new Set<string>();
	for (const token of splitNulRecords(output)) {
		const tab = token.indexOf("\t");
		if (tab <= 0) throw new Error("git ls-files vendor: record without a TAB path separator");
		const meta = token.slice(0, tab);
		const path = token.slice(tab + 1);
		if (path.length === 0) throw new Error("git ls-files vendor: record with an empty path");
		if (path.includes("\0") || path.includes("\\") || path.startsWith("/")) {
			throw new Error(`git ls-files vendor: unusable vendored path ${JSON.stringify(path)}`);
		}
		for (const segment of path.split("/")) {
			if (segment.length === 0 || segment === "." || segment === "..") {
				throw new Error(`git ls-files vendor: unusable vendored path ${JSON.stringify(path)}`);
			}
		}
		const [mode, , stage] = meta.split(" ");
		if (stage !== "0") {
			throw new Error(`vendored input ${path} sits at conflict stage ${stage}; resolve and restage`);
		}
		if (mode !== "100644" && mode !== "100755") {
			throw new Error(`vendored input ${path} has unsupported index mode ${mode} (symlink, gitlink or sparse entry)`);
		}
		if (seen.has(path)) throw new Error(`git ls-files vendor: duplicate record for ${path}`);
		seen.add(path);
		paths.push(path);
	}
	for (const crate of crates) {
		if (!seen.has(`${crate.path}/Cargo.toml`) || !paths.some((path) => path.startsWith(`${crate.path}/src/`))) {
			throw new Error(`vendored patch ${crate.name} at ${crate.path}: requires an indexed Cargo.toml and indexed src/** bytes; missing, untracked-only, or outside-scope inputs cannot be pinned`);
		}
	}
	paths.sort();
	return paths;
}

/**
 * Hash every tracked build-source + manifest + lock + provenance + license file
 * for every declared path patch. Empty only when no path patch is declared.
 */
export function readVendorSourcePins(root: string): readonly VendorSourcePin[] {
	const pins: VendorSourcePin[] = [];
	for (const relPath of vendorCrateInputPaths(root)) {
		pins.push({
			path: relPath,
			sha256: createHash("sha256").update(readFileSync(join(root, relPath))).digest("hex"),
		});
	}
	return pins;
}
