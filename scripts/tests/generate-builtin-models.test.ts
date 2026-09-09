/**
 * OFFLINE prep mode tests for scripts/generate-builtin-models.ts.
 *
 * Every scenario here spawns the real CLI (`bun run generate-builtin-models.ts
 * --offline-capture <dir>`) against real temp-dir fixtures - a real git
 * checkout for the source-SHA gate and real JSON files for the capture and
 * provenance - rather than mocking any validator. Default/`--check` behavior
 * (no `--offline-capture`) is exercised only for branch isolation; it must
 * never see offline-mode error text regardless of canonical-checkout state.
 */
import { describe, expect, test } from "bun:test";
import { execFileSync, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

import {
	EXPECTED_PROVIDER_IDS,
	buildSortedCatalog,
	encodeCatalog,
	offlineOutputPaths,
} from "../generate-builtin-models.ts";

const REPO_ROOT = resolve(import.meta.dirname, "../..");
const GENERATOR_PATH = join(REPO_ROOT, "scripts/generate-builtin-models.ts");
const CANONICAL_CATALOG_PATH = join(REPO_ROOT, "crates/pi-ai/data/builtin-models.json");

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function runGenerator(args: string[]): { status: number; stdout: string; stderr: string } {
	const proc = spawnSync("bun", ["run", GENERATOR_PATH, ...args], {
		cwd: REPO_ROOT,
		encoding: "utf8",
		timeout: 30_000,
	});
	return { status: proc.status ?? -1, stdout: proc.stdout ?? "", stderr: proc.stderr ?? "" };
}

/** One trivial model per expected provider - enough to satisfy provider-set validation. */
function syntheticCatalog(): Record<string, Record<string, unknown>> {
	const catalog: Record<string, Record<string, unknown>> = {};
	for (const providerId of EXPECTED_PROVIDER_IDS) {
		catalog[providerId] = {
			[`${providerId}-test-model`]: {
				id: `${providerId}-test-model`,
				name: "Test Model",
				provider: providerId,
			},
		};
	}
	return catalog;
}

/** Real, disposable git checkout standing in for a captured reference source root. */
function makeSourceRepo(): { root: string; sha: string } {
	const root = mkdtempSync(join(tmpdir(), "pi-offline-source-"));
	execFileSync("git", ["init", "--quiet"], { cwd: root });
	execFileSync("git", ["config", "user.email", "offline-prep-test@example.invalid"], { cwd: root });
	execFileSync("git", ["config", "user.name", "Offline Prep Test"], { cwd: root });
	writeFileSync(join(root, "marker.txt"), "offline-prep-fixture\n", "utf8");
	execFileSync("git", ["add", "."], { cwd: root });
	execFileSync("git", ["commit", "--quiet", "-m", "fixture"], { cwd: root });
	const sha = execFileSync("git", ["rev-parse", "HEAD"], { cwd: root, encoding: "utf8" }).trim();
	return { root, sha };
}

/** Flips the trailing hex digit; stays a valid 40-char SHA shape, never equal to the input. */
function flipHex(sha: string): string {
	const last = sha.at(-1);
	return `${sha.slice(0, -1)}${last === "0" ? "1" : "0"}`;
}

interface CaptureRootOptions {
	sourceRoot: string;
	sourceSha: string;
	catalog?: Record<string, Record<string, unknown>>;
	provenanceOverrides?: Record<string, unknown>;
	omitCatalogFile?: boolean;
	omitProvenanceFile?: boolean;
	corruptProvenanceJson?: boolean;
}

/** Builds a real `--offline-capture <dir>` fixture on disk; returns its root. */
function makeCaptureRoot(options: CaptureRootOptions): string {
	const captureRoot = mkdtempSync(join(tmpdir(), "pi-offline-capture-"));
	const catalog = options.catalog ?? syntheticCatalog();
	const catalogBytes = `${JSON.stringify(catalog, null, 2)}\n`;

	if (!options.omitCatalogFile) {
		const captureDir = join(captureRoot, "public-catalog-capture");
		mkdirSync(captureDir, { recursive: true });
		writeFileSync(join(captureDir, "models.json"), catalogBytes, "utf8");
	}

	if (!options.omitProvenanceFile) {
		const provenancePath = join(captureRoot, "public-catalog-provenance.json");
		if (options.corruptProvenanceJson) {
			writeFileSync(provenancePath, "{not valid json", "utf8");
		} else {
			const providerCount = Object.keys(catalog).length;
			let modelCount = 0;
			for (const models of Object.values(catalog)) {
				modelCount += Object.keys(models).length;
			}
			const provenance = {
				schemaVersion: 1,
				sourceRoot: options.sourceRoot,
				sourceSha: options.sourceSha,
				generator: "packages/ai/scripts/generate-models.ts",
				mode: "--strict --json-only",
				capturedAtUnixSeconds: 1700000000,
				sources: ["https://example.invalid/models.json"],
				catalogSha256: createHash("sha256").update(catalogBytes, "utf8").digest("hex"),
				providers: providerCount,
				models: modelCount,
				classification: "test fixture",
				...options.provenanceOverrides,
			};
			writeFileSync(provenancePath, `${JSON.stringify(provenance, null, 2)}\n`, "utf8");
		}
	}

	return captureRoot;
}

function cleanup(...paths: string[]): void {
	for (const path of paths) {
		rmSync(path, { recursive: true, force: true });
	}
}

// ---------------------------------------------------------------------------
// Missing prerequisites
// ---------------------------------------------------------------------------

describe("offline prep: missing prerequisites", () => {
	test("absent provenance file fails closed with the checked path", () => {
		const emptyRoot = mkdtempSync(join(tmpdir(), "pi-offline-missing-"));
		try {
			const result = runGenerator(["--offline-capture", emptyRoot]);
			expect(result.status).not.toBe(0);
			expect(result.stderr).toContain("offline capture provenance not found or unreadable");
			expect(result.stderr).toContain(join(emptyRoot, "public-catalog-provenance.json"));
		} finally {
			cleanup(emptyRoot);
		}
	});

	test("absent catalog file fails closed with the checked path", () => {
		const { root: sourceRoot, sha } = makeSourceRepo();
		const captureRoot = makeCaptureRoot({ sourceRoot, sourceSha: sha, omitCatalogFile: true });
		try {
			const result = runGenerator(["--offline-capture", captureRoot]);
			expect(result.status).not.toBe(0);
			expect(result.stderr).toContain("offline capture catalog not found or unreadable");
			expect(result.stderr).toContain(join(captureRoot, "public-catalog-capture", "models.json"));
		} finally {
			cleanup(sourceRoot, captureRoot);
		}
	});
});

// ---------------------------------------------------------------------------
// Provenance mismatch
// ---------------------------------------------------------------------------

describe("offline prep: provenance mismatch", () => {
	test("sourceSha diverged from the live source HEAD fails with both SHAs", () => {
		const { root: sourceRoot, sha } = makeSourceRepo();
		const wrongSha = flipHex(sha);
		const captureRoot = makeCaptureRoot({ sourceRoot, sourceSha: wrongSha });
		try {
			const result = runGenerator(["--offline-capture", captureRoot]);
			expect(result.status).not.toBe(0);
			expect(result.stderr).toContain("offline capture source mismatch");
			expect(result.stderr).toContain(wrongSha);
			expect(result.stderr).toContain(sha);
		} finally {
			cleanup(sourceRoot, captureRoot);
		}
	});

	test("catalog bytes diverged from the pinned catalogSha256 fails with both hashes", () => {
		const { root: sourceRoot, sha } = makeSourceRepo();
		const captureRoot = makeCaptureRoot({ sourceRoot, sourceSha: sha });
		const catalogPath = join(captureRoot, "public-catalog-capture", "models.json");
		const provenancePath = join(captureRoot, "public-catalog-provenance.json");
		const provenance = JSON.parse(readFileSync(provenancePath, "utf8")) as { catalogSha256: string };
		const pinnedHash = provenance.catalogSha256;
		const original = readFileSync(catalogPath, "utf8");
		writeFileSync(catalogPath, original.replace("Test Model", "Tampered Model"), "utf8");
		try {
			const result = runGenerator(["--offline-capture", captureRoot]);
			expect(result.status).not.toBe(0);
			expect(result.stderr).toContain("offline capture catalog hash mismatch");
			expect(result.stderr).toContain(pinnedHash);
		} finally {
			cleanup(sourceRoot, captureRoot);
		}
	});

	test("provenance provider count diverged from the catalog fails with both counts", () => {
		const { root: sourceRoot, sha } = makeSourceRepo();
		const captureRoot = makeCaptureRoot({
			sourceRoot,
			sourceSha: sha,
			provenanceOverrides: { providers: 999 },
		});
		try {
			const result = runGenerator(["--offline-capture", captureRoot]);
			expect(result.status).not.toBe(0);
			expect(result.stderr).toContain("offline capture provider count mismatch");
			expect(result.stderr).toContain("999");
			expect(result.stderr).toContain(String(EXPECTED_PROVIDER_IDS.length));
		} finally {
			cleanup(sourceRoot, captureRoot);
		}
	});

	test("provenance model count diverged from the catalog fails with both counts", () => {
		const { root: sourceRoot, sha } = makeSourceRepo();
		const captureRoot = makeCaptureRoot({
			sourceRoot,
			sourceSha: sha,
			provenanceOverrides: { models: 1 },
		});
		try {
			const result = runGenerator(["--offline-capture", captureRoot]);
			expect(result.status).not.toBe(0);
			expect(result.stderr).toContain("offline capture model count mismatch");
			expect(result.stderr).toContain(String(EXPECTED_PROVIDER_IDS.length));
		} finally {
			cleanup(sourceRoot, captureRoot);
		}
	});
});

// ---------------------------------------------------------------------------
// Corrupt input
// ---------------------------------------------------------------------------

describe("offline prep: corrupt input", () => {
	test("provenance file with invalid JSON syntax fails with parse context", () => {
		const { root: sourceRoot, sha } = makeSourceRepo();
		const captureRoot = makeCaptureRoot({ sourceRoot, sourceSha: sha, corruptProvenanceJson: true });
		try {
			const result = runGenerator(["--offline-capture", captureRoot]);
			expect(result.status).not.toBe(0);
			expect(result.stderr).toContain("is not valid JSON");
		} finally {
			cleanup(sourceRoot, captureRoot);
		}
	});

	test("provenance file missing catalogSha256 fails closed on that field", () => {
		const { root: sourceRoot, sha } = makeSourceRepo();
		const captureRoot = makeCaptureRoot({ sourceRoot, sourceSha: sha });
		const provenancePath = join(captureRoot, "public-catalog-provenance.json");
		const provenance = JSON.parse(readFileSync(provenancePath, "utf8")) as Record<string, unknown>;
		delete provenance.catalogSha256;
		writeFileSync(provenancePath, `${JSON.stringify(provenance, null, 2)}\n`, "utf8");
		try {
			const result = runGenerator(["--offline-capture", captureRoot]);
			expect(result.status).not.toBe(0);
			expect(result.stderr).toContain('field "catalogSha256"');
		} finally {
			cleanup(sourceRoot, captureRoot);
		}
	});

	test("catalog missing a required provider fails via provider-set validation", () => {
		const { root: sourceRoot, sha } = makeSourceRepo();
		const catalog = syntheticCatalog();
		delete catalog.anthropic;
		const captureRoot = makeCaptureRoot({ sourceRoot, sourceSha: sha, catalog });
		try {
			const result = runGenerator(["--offline-capture", captureRoot]);
			expect(result.status).not.toBe(0);
			expect(result.stderr).toContain("missing providers: anthropic");
		} finally {
			cleanup(sourceRoot, captureRoot);
		}
	});
});

// ---------------------------------------------------------------------------
// Output fence and determinism
// ---------------------------------------------------------------------------

describe("offline prep: output fence and determinism", () => {
	test("canonical builtin-models.json is untouched and the prep artifact lands outside it", () => {
		const before = readFileSync(CANONICAL_CATALOG_PATH);
		const { root: sourceRoot, sha } = makeSourceRepo();
		const captureRoot = makeCaptureRoot({ sourceRoot, sourceSha: sha });
		const { dir, artifactPath, manifestPath } = offlineOutputPaths(sha);
		try {
			const result = runGenerator(["--offline-capture", captureRoot]);
			expect(result.status).toBe(0);
			expect(resolve(artifactPath)).not.toBe(resolve(CANONICAL_CATALOG_PATH));
			expect(existsSync(artifactPath)).toBe(true);
			expect(existsSync(manifestPath)).toBe(true);
			const after = readFileSync(CANONICAL_CATALOG_PATH);
			expect(after.equals(before)).toBe(true);
		} finally {
			cleanup(sourceRoot, captureRoot, dir);
		}
	});

	test("the same capture rerun produces byte-identical artifact and manifest", () => {
		const { root: sourceRoot, sha } = makeSourceRepo();
		const captureRoot = makeCaptureRoot({ sourceRoot, sourceSha: sha });
		const { dir, artifactPath, manifestPath } = offlineOutputPaths(sha);
		try {
			const first = runGenerator(["--offline-capture", captureRoot]);
			expect(first.status).toBe(0);
			const firstArtifact = readFileSync(artifactPath);
			const firstManifest = readFileSync(manifestPath);

			const second = runGenerator(["--offline-capture", captureRoot]);
			expect(second.status).toBe(0);
			const secondArtifact = readFileSync(artifactPath);
			const secondManifest = readFileSync(manifestPath);

			expect(secondArtifact.equals(firstArtifact)).toBe(true);
			expect(secondManifest.equals(firstManifest)).toBe(true);

			const expectedEncoded = encodeCatalog(buildSortedCatalog(syntheticCatalog()));
			expect(firstArtifact.toString("utf8")).toBe(expectedEncoded);

			const manifest = JSON.parse(firstManifest.toString("utf8")) as Record<string, unknown>;
			expect(manifest.sourceSha).toBe(sha);
			expect(manifest.sourceRoot).toBe(sourceRoot);
			expect(manifest.outputSha256).toBe(
				createHash("sha256").update(firstArtifact).digest("hex"),
			);
		} finally {
			cleanup(sourceRoot, captureRoot, dir);
		}
	});
});

// ---------------------------------------------------------------------------
// CLI dispatch and branch isolation
// ---------------------------------------------------------------------------

describe("offline prep: CLI dispatch", () => {
	test("--offline-capture and --check together are rejected before any I/O", () => {
		const { root: sourceRoot, sha } = makeSourceRepo();
		const captureRoot = makeCaptureRoot({ sourceRoot, sourceSha: sha });
		try {
			const result = runGenerator(["--offline-capture", captureRoot, "--check"]);
			expect(result.status).not.toBe(0);
			expect(result.stderr).toContain("mutually exclusive");
		} finally {
			cleanup(sourceRoot, captureRoot);
		}
	});

	test("--offline-capture without a directory argument fails with a usage error", () => {
		const result = runGenerator(["--offline-capture"]);
		expect(result.status).not.toBe(0);
		expect(result.stderr).toContain("--offline-capture requires a directory argument");
	});

	test("default/--check dispatch never emits offline-mode error text", () => {
		const result = runGenerator(["--check"]);
		expect(result.stderr).not.toContain("offline capture");
		expect(result.stderr).not.toContain("--offline-capture");
	});
});
