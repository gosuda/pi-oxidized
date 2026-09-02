import { describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { sha256Bytes } from "../release/archive.ts";
import { RELEASE_MANIFEST_SCHEMA } from "../release/stage.ts";
import { verifyUnpackedArchive } from "../release/verify.ts";

/** Manifest entry shape matching `ManifestFile` in stage.ts. */
interface Entry {
	readonly path: string;
	readonly size: number;
	readonly sha256: string;
	readonly executable: boolean;
}

/** Build a valid release.json manifest for the given file entries. */
function manifestJson(files: readonly Entry[]): string {
	return (
		JSON.stringify(
			{
				schema: RELEASE_MANIFEST_SCHEMA,
				version: "0.1.0",
				rustTarget: "x86_64-unknown-linux-gnu",
				bunTarget: "bun-linux-x64-baseline",
				hostKind: "compiled",
				compatibilityVersion: "0.80.10",
				protocolVersion: 1,
				sourceDateEpoch: 1735689600,
				createdAt: "2025-01-01T00:00:00.000Z",
				files,
			},
			null,
			2,
		) + "\n"
	);
}

/** Create a temp archive root with the given files and a release.json. */
function makeArchiveRoot(
	files: Readonly<Record<string, Uint8Array | string>>,
	manifestFiles?: readonly Entry[],
): string {
	const dir = mkdtempSync(join(tmpdir(), "arc21-verify-"));
	for (const [rel, content] of Object.entries(files)) {
		const full = join(dir, ...rel.split("/"));
		mkdirSync(join(full, ".."), { recursive: true });
		writeFileSync(full, content);
	}
	const entries =
		manifestFiles ??
		Object.entries(files).map(([rel, content]) => {
			const bytes =
				typeof content === "string" ? new TextEncoder().encode(content) : content;
			return {
				path: rel,
				size: bytes.length,
				sha256: sha256Bytes(bytes),
				executable: false,
			} satisfies Entry;
		});
	writeFileSync(join(dir, "release.json"), manifestJson(entries));
	return dir;
}

/** Helper: encode string to Uint8Array. */
function bytes(s: string): Uint8Array {
	return new TextEncoder().encode(s);
}

describe("verifyUnpackedArchive", () => {
	test("clean pass — all files present, digests match", async () => {
		const dir = makeArchiveRoot({
			"pi": bytes("binary content"),
			"docs/README.md": "hello world\n",
			"host/pi-extension-host": bytes("host binary"),
		});
		try {
			const result = await verifyUnpackedArchive(dir);
			expect(result.ok).toBe(true);
			if (result.ok) {
				expect(result.fileCount).toBe(3);
			}
		} finally {
			rmSync(dir, { recursive: true, force: true });
		}
	});

	test("missing file — manifest references a file not on disk", async () => {
		const dir = makeArchiveRoot({
			"pi": bytes("binary content"),
			"docs/README.md": "hello world\n",
		});
		// Override manifest to include a file that doesn't exist on disk
		const entries: Entry[] = [
			{ path: "pi", size: 15, sha256: sha256Bytes(bytes("binary content")), executable: true },
			{
				path: "docs/README.md",
				size: 12,
				sha256: sha256Bytes(bytes("hello world\n")),
				executable: false,
			},
			{
				path: "host/pi-extension-host",
				size: 11,
				sha256: sha256Bytes(bytes("host binary")),
				executable: true,
			},
		];
		writeFileSync(join(dir, "release.json"), manifestJson(entries));
		try {
			const result = await verifyUnpackedArchive(dir);
			expect(result.ok).toBe(false);
			if (!result.ok) {
				expect(result.errors.some((e) => e.includes("host/pi-extension-host"))).toBe(true);
				expect(result.errors.some((e) => e.includes("not on disk"))).toBe(true);
			}
		} finally {
			rmSync(dir, { recursive: true, force: true });
		}
	});

	test("extra file — disk has a file not in manifest", async () => {
		const dir = makeArchiveRoot({
			"pi": bytes("binary content"),
			"docs/README.md": "hello world\n",
			"host/pi-extension-host": bytes("host binary"),
			"extra/unexpected.txt": "surprise\n",
		});
		// Manifest only lists 3 files; the 4th is extra
		const entries: Entry[] = [
			{ path: "pi", size: 15, sha256: sha256Bytes(bytes("binary content")), executable: true },
			{
				path: "docs/README.md",
				size: 12,
				sha256: sha256Bytes(bytes("hello world\n")),
				executable: false,
			},
			{
				path: "host/pi-extension-host",
				size: 11,
				sha256: sha256Bytes(bytes("host binary")),
				executable: true,
			},
		];
		writeFileSync(join(dir, "release.json"), manifestJson(entries));
		try {
			const result = await verifyUnpackedArchive(dir);
			expect(result.ok).toBe(false);
			if (!result.ok) {
				expect(result.errors.some((e) => e.includes("extra/unexpected.txt"))).toBe(true);
				expect(result.errors.some((e) => e.includes("on disk but not in manifest"))).toBe(
					true,
				);
			}
		} finally {
			rmSync(dir, { recursive: true, force: true });
		}
	});

	test("digest mismatch — file content differs from manifest sha256", async () => {
		const dir = makeArchiveRoot({
			"pi": bytes("binary content"),
			"docs/README.md": "hello world\n",
		});
		// Manifest has wrong sha256 for pi
		const entries: Entry[] = [
			{
				path: "pi",
				size: 15,
				sha256: sha256Bytes(bytes("DIFFERENT CONTENT")),
				executable: true,
			},
			{
				path: "docs/README.md",
				size: 12,
				sha256: sha256Bytes(bytes("hello world\n")),
				executable: false,
			},
		];
		writeFileSync(join(dir, "release.json"), manifestJson(entries));
		try {
			const result = await verifyUnpackedArchive(dir);
			expect(result.ok).toBe(false);
			if (!result.ok) {
				expect(result.errors.some((e) => e.startsWith("pi: sha256 mismatch"))).toBe(true);
			}
		} finally {
			rmSync(dir, { recursive: true, force: true });
		}
	});

	test("missing release.json — fails with clear error", async () => {
		const dir = mkdtempSync(join(tmpdir(), "arc21-verify-"));
		writeFileSync(join(dir, "pi"), "stub");
		try {
			const result = await verifyUnpackedArchive(dir);
			expect(result.ok).toBe(false);
			if (!result.ok) {
				expect(result.errors.some((e) => e.includes("release.json"))).toBe(true);
			}
		} finally {
			rmSync(dir, { recursive: true, force: true });
		}
	});

	test("size mismatch — file size differs from manifest", async () => {
		const dir = makeArchiveRoot({
			"pi": bytes("binary content"),
		});
		const entries: Entry[] = [
			{ path: "pi", size: 999, sha256: sha256Bytes(bytes("binary content")), executable: true },
		];
		writeFileSync(join(dir, "release.json"), manifestJson(entries));
		try {
			const result = await verifyUnpackedArchive(dir);
			expect(result.ok).toBe(false);
			if (!result.ok) {
				expect(result.errors.some((e) => e.startsWith("pi: size mismatch"))).toBe(true);
			}
		} finally {
			rmSync(dir, { recursive: true, force: true });
		}
	});
});

test("verifier collects null manifest entries instead of crashing", () => {
	const dir = makeArchiveRoot({}, [null as unknown as Entry]);
	return verifyUnpackedArchive(dir).then((result) => {
		expect(result.ok).toBe(false);
		expect(result.errors?.some((e) => e.includes("files[0] is not an object"))).toBe(true);
	});
});

test("verifier collects invalid entry field types", () => {
	const dir = makeArchiveRoot({}, [
		{ path: 42, size: "x", sha256: true, executable: "no" } as unknown as Entry,
	]);
	return verifyUnpackedArchive(dir).then((result) => {
		expect(result.ok).toBe(false);
		expect(result.errors?.some((e) => e.includes("invalid field types"))).toBe(true);
	});
});

test("verifier collects a null manifest instead of crashing", () => {
	const dir = makeArchiveRoot({}, []);
	writeFileSync(join(dir, "release.json"), "null");
	return verifyUnpackedArchive(dir).then((result) => {
		expect(result.ok).toBe(false);
		expect(result.errors?.some((e) => e.includes("manifest is not an object"))).toBe(true);
	});
});
