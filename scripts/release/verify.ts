/**
 * Release archive integrity verifier — the single proof owner.
 *
 * Every release leg (seven targets, both double-package passes) invokes this
 * module on its unpacked archive. It asserts:
 *   1. `release.json` exists and parses as a `pi.release.v1` manifest.
 *   2. The on-disk file set (excluding `release.json`) equals the manifest
 *      entry set — both directions (no missing files, no extra files).
 *   3. Every manifest-listed file has a matching SHA-256 digest and size.
 *
 * The Python heredoc that previously duplicated this logic on the two musl
 * legs has been deleted; this module replaces it uniformly.
 */

import { createHash } from "node:crypto";
import { readdir, readFile, stat } from "node:fs/promises";
import { join, relative } from "node:path";

import { RELEASE_MANIFEST_SCHEMA, type ReleaseManifest } from "./stage.ts";

/** Result of a successful verification. */
export interface VerifyOk {
	readonly ok: true;
	readonly fileCount: number;
}

/** Result of a failed verification. */
export interface VerifyFail {
	readonly ok: false;
	readonly errors: readonly string[];
}

/** Union result of {@link verifyUnpackedArchive}. */
export type VerifyResult = VerifyOk | VerifyFail;

/** Normalize a path to POSIX-style forward slashes, no leading slash. */
function posixRel(root: string, absPath: string): string {
	return relative(root, absPath).split("\\").join("/");
}

/** Recursively collect all regular files under `dir`, returning POSIX-rel paths. */
async function collectFiles(dir: string): Promise<Set<string>> {
	const result = new Set<string>();
	async function walk(d: string): Promise<void> {
		const entries = await readdir(d, { withFileTypes: true });
		for (const entry of entries) {
			const full = join(d, entry.name);
			if (entry.isDirectory()) {
				await walk(full);
			} else if (entry.isFile()) {
				result.add(posixRel(dir, full));
			}
		}
	}
	await walk(dir);
	return result;
}

/**
 * Verify an unpacked release archive directory against its `release.json`.
 *
 * @param dir — absolute path to the archive root (containing `release.json`,
 *   `pi`, etc.). This is the single directory inside the extracted archive.
 * @returns {@link VerifyResult} — never throws on integrity failures; all
 *   failures are collected into `errors` so the caller sees every problem
 *   in one pass.
 */
export async function verifyUnpackedArchive(dir: string): Promise<VerifyResult> {
	const errors: string[] = [];

	// 1. Load and parse release.json.
	const manifestPath = join(dir, "release.json");
	let manifestBytes: Uint8Array;
	try {
		manifestBytes = await readFile(manifestPath);
	} catch {
		return { ok: false, errors: ["release.json: missing or unreadable"] };
	}

	let manifest: ReleaseManifest;
	try {
		manifest = JSON.parse(new TextDecoder().decode(manifestBytes)) as ReleaseManifest;
	} catch (err) {
		return {
			ok: false,
			errors: [`release.json: invalid JSON (${errMessage(err)})`],
		};
	}

	if (manifest === null || typeof manifest !== "object") {
		errors.push("release.json: manifest is not an object");
		return { ok: false, errors };
	}

	if (manifest.schema !== RELEASE_MANIFEST_SCHEMA) {
		errors.push(
			`release.json: unexpected schema "${manifest.schema}", expected "${RELEASE_MANIFEST_SCHEMA}"`,
		);
	}

	if (!Array.isArray(manifest.files)) {
		errors.push("release.json: \"files\" is not an array");
		return { ok: false, errors };
	}

	// Every entry must be well-formed before its fields are trusted; a
	// malformed manifest is an integrity failure, not a crash.
	for (const [index, entry] of manifest.files.entries()) {
		if (entry === null || typeof entry !== "object") {
			errors.push(`release.json: files[${index}] is not an object`);
		} else if (
			typeof entry.path !== "string" ||
			entry.path.length === 0 ||
			typeof entry.size !== "number" ||
			typeof entry.sha256 !== "string" ||
			typeof entry.executable !== "boolean"
		) {
			errors.push(`release.json: files[${index}] has invalid field types`);
		}
	}
	if (errors.length > 0) {
		return { ok: false, errors };
	}

	// 2. Manifest set equality (both directions), excluding release.json.
	const manifestFiles = new Set(manifest.files.map((f) => f.path));

	let diskFiles: Set<string>;
	try {
		diskFiles = await collectFiles(dir);
	} catch (err) {
		return { ok: false, errors: [`failed to enumerate files: ${errMessage(err)}`] };
	}
	diskFiles.delete("release.json");

	const onlyOnDisk = [...diskFiles].filter((f) => !manifestFiles.has(f)).sort();
	const onlyInManifest = [...manifestFiles].filter((f) => !diskFiles.has(f)).sort();

	if (onlyOnDisk.length > 0) {
		errors.push(`files on disk but not in manifest: ${onlyOnDisk.join(", ")}`);
	}
	if (onlyInManifest.length > 0) {
		errors.push(`files in manifest but not on disk: ${onlyInManifest.join(", ")}`);
	}

	// 3. Per-file SHA-256 + size verification.
	for (const entry of manifest.files) {
		const filePath = join(dir, ...entry.path.split("/"));
		let data: Uint8Array;
		try {
			data = await readFile(filePath);
		} catch {
			errors.push(`${entry.path}: missing or unreadable`);
			continue;
		}
		const digest = createHash("sha256").update(data).digest("hex");
		if (digest !== entry.sha256) {
			errors.push(
				`${entry.path}: sha256 mismatch (disk=${digest}, manifest=${entry.sha256})`,
			);
		}
		if (data.length !== entry.size) {
			errors.push(
				`${entry.path}: size mismatch (disk=${data.length}, manifest=${entry.size})`,
			);
		}
	}

	if (errors.length > 0) {
		return { ok: false, errors };
	}
	return { ok: true, fileCount: manifest.files.length };
}

/** Render an unknown error as a short string for diagnostic messages. */
function errMessage(err: unknown): string {
	if (err instanceof Error) return err.message;
	return String(err);
}

// ─────────────────────────────────────────────────────────────────────────────
// CLI entry: `bun run scripts/release/verify.ts <unpacked-dir>`
// ─────────────────────────────────────────────────────────────────────────────

if (import.meta.main) {
	const dir = process.argv[2];
	if (!dir) {
		process.stderr.write("usage: bun run scripts/release/verify.ts <unpacked-dir>\n");
		process.exit(2);
	}
	verifyUnpackedArchive(dir)
		.then((result) => {
			if (result.ok) {
				process.stdout.write(
					`archive-integrity: ${result.fileCount} members verified against release.json\n`,
				);
				process.exit(0);
			}
			for (const err of result.errors) {
				process.stderr.write(`archive-integrity FAIL: ${err}\n`);
			}
			process.exit(1);
		})
		.catch((err: unknown) => {
			process.stderr.write(`archive-integrity ERROR: ${errMessage(err)}\n`);
			process.exit(1);
		});
}
