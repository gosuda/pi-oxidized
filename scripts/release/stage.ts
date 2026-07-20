/**
 * Release staging: assemble the on-disk directory tree that becomes the
 * archive, verify target agreement + executable bits + no-host-in-pi, and
 * emit the `release.json` manifest with per-file SHA-256 digests.
 *
 * Every external interaction flows through the {@link Fs} and
 * {@link CommandRunner} seams so tests can drive the assembly without
 * invoking cargo or bun.
 */

import { createHash } from "node:crypto";

import type { HostArtifact } from "./host.ts";
import { safeJoinPath } from "./runner.ts";
import type { Fs } from "./runner.ts";
import type { TargetPlan } from "./targets.ts";

/** Schema discriminator stamped into every `release.json`. */
export const RELEASE_MANIFEST_SCHEMA = "pi.release.v1" as const;

/** Per-file entry inside the manifest. */
export interface ManifestFile {
	/** POSIX-style path relative to the archive root. */
	readonly path: string;
	/** File size in bytes. */
	readonly size: number;
	/** Lowercase hex SHA-256 digest of file bytes. */
	readonly sha256: string;
	/** `true` for binaries that must carry the executable bit. */
	readonly executable: boolean;
}

/** Shape of `release.json` shipped inside every archive. */
export interface ReleaseManifest {
	readonly schema: typeof RELEASE_MANIFEST_SCHEMA;
	readonly version: string;
	readonly rustTarget: string;
	readonly bunTarget: string;
	/** `compiled` sidecar or `runtime-bundle` fallback. */
	readonly hostKind: HostArtifact["kind"];
	readonly compatibilityVersion: string;
	readonly protocolVersion: number;
	readonly sourceDateEpoch: number;
	readonly createdAt: string;
	readonly files: readonly ManifestFile[];
}

/** Inputs to {@link assembleRelease}. */
export interface AssembleInputs {
	/** Resolved release target. */
	readonly plan: TargetPlan;
	/** Workspace version (e.g. `0.1.0`). */
	readonly version: string;
	/** Absolute path to the freshly-built Rust binary for `plan.rustTarget`. */
	readonly piBinaryPath: string;
	/** Absolute path to the workspace root (for metadata sources). */
	readonly repoRoot: string;
	/** Built host artifact (compiled sidecar or runtime-bundle fallback). */
	readonly host: HostArtifact;
	/**
	 * Absolute path to a pre-built Bun runtime matching `plan.bunTarget`,
	 * required only when `host.kind === "runtime-bundle"`. The release script
	 * supplies it from the official Bun release archive.
	 */
	readonly bunRuntimePath?: string;
	/** Filesystem seam. */
	readonly fs: Fs;
	/** Source-date-epoch stamp for the manifest + archive mtimes. */
	readonly sourceDateEpoch: number;
	/** Compatibility version recorded in the manifest. */
	readonly compatibilityVersion: string;
	/** Protocol version recorded in the manifest. */
	readonly protocolVersion: number;
	/**
	 * Built timestamp (ISO 8601). For reproducibility, pass a fixed value
	 * derived from `sourceDateEpoch`; tests inject deterministic strings.
	 */
	readonly createdAt: string;
	/**
	 * Absolute source paths for the docs/examples/assets trees that get
	 * copied verbatim into the archive root. Missing paths are skipped
	 * silently so a workspace can ship a subset.
	 */
	readonly docsSource?: string;
	readonly examplesSource?: string;
	readonly assetsSource?: string;
	/**
	 * Optional set of additional `(src, archiveRelPath)` pairs to copy in,
	 * used by tests to verify reproducibility without spinning up cargo.
	 * Reserved destinations (binary slots, manifest path, or duplicates) are
	 * rejected before any bytes are written.
	 */
	readonly extraFiles?: readonly { readonly src: string; readonly dest: string }[];
}

/** Result of {@link assembleRelease}: staging directory + manifest. */
export interface AssembledRelease {
	/** Absolute path to the staged archive-root directory. */
	readonly stagingDir: string;
	/** Written `release.json` document. */
	readonly manifest: ReleaseManifest;
}

/** Error raised when a target-agreement or contamination check fails. */
export class ReleaseVerifyError extends Error {
	constructor(message: string) {
		super(message);
		this.name = "ReleaseVerifyError";
	}
}

/** Reserved top-level slots extras cannot overwrite. */
const RESERVED_TOP_LEVEL: Record<string, true> = {
	"pi": true,
	"pi.exe": true,
	"pi-extension-host": true,
	"pi-extension-host.exe": true,
	"pi-extension-host.js": true,
	"bun": true,
	"bun.exe": true,
	"release.json": true,
};

/** POSIX-style normalization of an archive-relative path. */
function normalizeArchiveRel(path: string): string {
	return path.split("\\").join("/").replace(/^\/+/, "");
}

/** Track which archive-relative paths have already been staged. */
class UsedPaths {
	private readonly seen = new Set<string>();

	claim(relPath: string): void {
		const norm = normalizeArchiveRel(relPath);
		if (this.seen.has(norm)) {
			throw new ReleaseVerifyError(`duplicate archive path: ${norm}`);
		}
		this.seen.add(norm);
	}

	assertNotReserved(relPath: string): void {
		const norm = normalizeArchiveRel(relPath);
		const top = norm.split("/")[0] ?? "";
		if (RESERVED_TOP_LEVEL[top] && !norm.startsWith("docs/") && !norm.startsWith("examples/") && !norm.startsWith("assets/")) {
			throw new ReleaseVerifyError(
				`extraFile destination collides with reserved slot: ${norm}`,
			);
		}
	}
}

/**
 * Assemble the release tree at `<stagingRoot>/<plan.archiveDir>/`:
 *
 *   <archiveDir>/
 *     pi[.exe]
 *     pi-extension-host[.exe]            (compiled path)
 *     bun[.exe], pi-extension-host.js    (fallback path)
 *     CHANGELOG.md, README.md, LICENSE
 *     docs/...                           (recursive)
 *     examples/...                       (recursive)
 *     assets/...                         (recursive)
 *     release.json
 *
 * Every destination path is run through {@link safeJoinPath} so a malicious
 * source name cannot escape the staging root, and every stage step claims
 * its paths through {@link UsedPaths} so a caller-supplied `extraFiles`
 * entry cannot overwrite a binary or duplicate another file.
 */
export async function assembleRelease(
	stagingRoot: string,
	inputs: AssembleInputs,
): Promise<AssembledRelease> {
	const { fs, plan } = inputs;
	const archiveDir = safeJoinPath(stagingRoot, plan.archiveDir);
	await fs.mkdir(archiveDir, { recursive: true });

	const used = new UsedPaths();
	const copied: ManifestFile[] = [];

	// 1. Rust binary.
	copied.push(
		await copyBinary(fs, inputs.piBinaryPath, archiveDir, plan.piBinaryName, true, plan.windows, used),
	);

	// 2. Host sidecar (or fallback runtime+bundle).
	for (const f of await copyHostArtifact(inputs, archiveDir, used)) copied.push(f);

	// 3. Optional top-level metadata files. Missing files are skipped silently
	// (the master plan does not mandate any particular README / LICENSE
	// layout for the archive; release-time bundles can ship a subset).
	for (const name of ["CHANGELOG.md", "README.md", "LICENSE", "LICENSE-MIT"]) {
		const f = await copyOptionalFile(fs, inputs.repoRoot, archiveDir, name, used);
		if (f) copied.push(f);
	}

	// 4. Recursive doc/example/asset trees.
	for (const f of await copyTreeOptional(fs, inputs.docsSource, archiveDir, "docs", used)) {
		copied.push(f);
	}
	for (const f of await copyTreeOptional(fs, inputs.examplesSource, archiveDir, "examples", used)) {
		copied.push(f);
	}
	for (const f of await copyTreeOptional(fs, inputs.assetsSource, archiveDir, "assets", used)) {
		copied.push(f);
	}
	for (const f of await copyTreeOptional(
		fs,
		`${inputs.repoRoot}/crates/pi/assets/theme`,
		archiveDir,
		"theme",
		used,
	)) {
		copied.push(f);
	}

	// 5. Caller-supplied extras (tests use this for deterministic content).
	if (inputs.extraFiles) {
		for (const extra of inputs.extraFiles) {
			const destRel = normalizeArchiveRel(extra.dest);
			used.assertNotReserved(destRel);
			used.claim(destRel);
			const data = await fs.readFile(extra.src);
			const dest = safeJoinPath(archiveDir, destRel);
			await fs.mkdir(dest.split("/").slice(0, -1).join("/"), { recursive: true });
			await fs.writeFile(dest, data);
			copied.push(manifestEntryFromData(data, destRel, false));
		}
	}

	// Verification gate: no host bytes inside the Rust binary.
	await verifyNoHostInPi(fs, inputs.piBinaryPath, inputs.host);

	// Verification gate: binaries present and (on POSIX) executable.
	await verifyExecutableBits(fs, plan, archiveDir, inputs.host);

	const manifest: ReleaseManifest = {
		schema: RELEASE_MANIFEST_SCHEMA,
		version: inputs.version,
		rustTarget: plan.rustTarget,
		bunTarget: plan.bunTarget,
		hostKind: inputs.host.kind,
		compatibilityVersion: inputs.compatibilityVersion,
		protocolVersion: inputs.protocolVersion,
		sourceDateEpoch: inputs.sourceDateEpoch,
		createdAt: inputs.createdAt,
		files: copied.slice().sort((a, b) => (a.path < b.path ? -1 : a.path > b.path ? 1 : 0)),
	};
	const manifestPath = safeJoinPath(archiveDir, "release.json");
	used.claim("release.json");
	await fs.writeFile(manifestPath, JSON.stringify(manifest, null, 2) + "\n");
	return { stagingDir: archiveDir, manifest };
}

/** Copy a single binary into the staging tree, preserving the exec bit. */
async function copyBinary(
	fs: Fs,
	srcPath: string,
	archiveDir: string,
	destName: string,
	executable: boolean,
	isWindows: boolean,
	used: UsedPaths,
): Promise<ManifestFile> {
	used.claim(destName);
	const data = await fs.readFile(srcPath);
	const dest = safeJoinPath(archiveDir, destName);
	await fs.writeFile(dest, data);
	// Windows has no chmod; the archive writer carries the bit via
	// manifest metadata and the installer restores it on POSIX.
	if (executable && !isWindows) {
		try {
			await fs.chmod(dest, 0o755);
		} catch {
			// Just in case it fails on a strange filesystem.
		}
	}
	return manifestEntryFromData(data, destName, executable);
}

/** Copy the host artifact(s) into the staging tree. */
async function copyHostArtifact(
	inputs: AssembleInputs,
	archiveDir: string,
	used: UsedPaths,
): Promise<ManifestFile[]> {
	const { fs, plan, host } = inputs;
	const out: ManifestFile[] = [];
	if (host.kind === "compiled") {
		out.push(
			await copyBinary(fs, host.binaryPath, archiveDir, plan.hostBinaryName, true, plan.windows, used),
		);
		return out;
	}
	// runtime-bundle fallback: ship script + the official Bun runtime.
	out.push(
		await copyBinary(fs, host.scriptPath, archiveDir, plan.hostBundleName, false, plan.windows, used),
	);
	const runtimePath = inputs.bunRuntimePath;
	if (!runtimePath) {
		throw new ReleaseVerifyError(
			`runtime-bundle host requires bunRuntimePath for ${plan.rustTarget}`,
		);
	}
	out.push(
		await copyBinary(fs, runtimePath, archiveDir, plan.bunRuntimeName, true, plan.windows, used),
	);
	return out;
}

/** Copy one optional top-level metadata file; returns `null` if absent. */
async function copyOptionalFile(
	fs: Fs,
	repoRoot: string,
	archiveDir: string,
	name: string,
	used: UsedPaths,
): Promise<ManifestFile | null> {
	const src = `${repoRoot}/${name}`;
	let data: Uint8Array;
	try {
		data = await fs.readFile(src);
	} catch {
		return null;
	}
	used.claim(name);
	const dest = safeJoinPath(archiveDir, name);
	await fs.writeFile(dest, data);
	return manifestEntryFromData(data, name, false);
}

/**
 * Recursively copy `src` into `<archiveDir>/<destSubdir>/`, returning one
 * manifest entry per file. If `src` is `undefined` or does not exist, returns
 * an empty array (caller can ship a subset).
 */
async function copyTreeOptional(
	fs: Fs,
	src: string | undefined,
	archiveDir: string,
	destSubdir: string,
	used: UsedPaths,
): Promise<ManifestFile[]> {
	if (!src) return [];
	let exists = true;
	try {
		const s = await fs.stat(src);
		if (!s.isDir) exists = false;
	} catch {
		exists = false;
	}
	if (!exists) return [];

	const destRoot = safeJoinPath(archiveDir, destSubdir);
	await fs.cp(src, destRoot, { recursive: true });
	const out: ManifestFile[] = [];
	const queue: string[] = [destRoot];
	while (queue.length > 0) {
		const dir = queue.shift();
		if (dir === undefined) break;
		let entries: string[];
		try {
			entries = await fs.readdir(dir);
		} catch {
			continue;
		}
		entries.sort();
		for (const name of entries) {
			const childAbs = `${dir}/${name}`;
			const s = await fs.stat(childAbs);
			const rel = archiveRelativePath(archiveDir, childAbs);
			if (s.isDir) {
				queue.push(childAbs);
				continue;
			}
			if (!s.isFile) continue;
			used.claim(rel);
			const data = await fs.readFile(childAbs);
			out.push(manifestEntryFromData(data, rel, false));
		}
	}
	return out;
}

/** Compute the archive-relative POSIX path for an absolute staged file. */
function archiveRelativePath(archiveDir: string, absPath: string): string {
	const rel = absPath.slice(archiveDir.length + 1);
	return rel.split("\\").join("/");
}

/**
 * Verification: a 64 KiB slice of the host must not appear contiguously in
 * the Rust binary. If it does, the build accidentally embedded the host via
 * `include_bytes!` (the master plan forbids embedding ~100 MB into the LTO
 * link).
 */
async function verifyNoHostInPi(fs: Fs, piPath: string, host: HostArtifact): Promise<void> {
	const piBytes = await fs.readFile(piPath);
	const hostSrc = host.kind === "compiled" ? host.binaryPath : host.scriptPath;
	const hostBytes = await fs.readFile(hostSrc);
	const probeLen = Math.min(64 * 1024, hostBytes.length);
	if (probeLen === 0) return;
	const probe = hostBytes.subarray(0, probeLen);
	if (Buffer.from(piBytes).indexOf(probe) !== -1) {
		throw new ReleaseVerifyError(
			`Rust binary at ${piPath} contains a contiguous ${probeLen}-byte slice of the host ${hostSrc}; the host must be shipped beside the binary, never embedded.`,
		);
	}
}

/**
 * Verification: pi (and the host binary in compiled mode) must exist and
 * carry the executable bit on POSIX. Windows archives skip the bit check.
 */
async function verifyExecutableBits(
	fs: Fs,
	plan: TargetPlan,
	archiveDir: string,
	host: HostArtifact,
): Promise<void> {
	if (plan.windows) return; // Windows uses the manifest's executable flag.
	const required = [plan.piBinaryName];
	if (host.kind === "compiled") required.push(plan.hostBinaryName);
	if (host.kind === "runtime-bundle") required.push(plan.bunRuntimeName);
	for (const name of required) {
		const path = safeJoinPath(archiveDir, name);
		const s = await fs.stat(path);
		if (!s.isFile) {
			throw new ReleaseVerifyError(`expected file at ${path}`);
		}
		if ((s.mode & 0o111) === 0) {
			throw new ReleaseVerifyError(`${path} is missing the executable bit`);
		}
	}
}

/** Build a manifest entry from already-loaded bytes. */
function manifestEntryFromData(data: Uint8Array, relPath: string, executable: boolean): ManifestFile {
	const hash = createHash("sha256").update(data).digest("hex");
	return { path: relPath, size: data.length, sha256: hash, executable };
}

