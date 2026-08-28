#!/usr/bin/env bun
/** Tests for the DEPS-R1 SBOM baseline (scripts/verification/deps-sbom.ts). */

import { readFileSync } from "node:fs";
import { join } from "node:path";

import { describe, expect, test } from "bun:test";

import {
	BASELINE_PATH,
	REPO_ROOT,
	SBOM_SCHEMA,
	canonicalJson,
	captureContent,
	contentDigest,
	loadSnapshot,
	verifySnapshot,
} from "./deps-sbom.ts";
import type { SbomContent } from "./deps-sbom.ts";

/** Recursively strips `readonly` so drift probes can mutate fixture copies. */
type DeepMutable<T> = {
	-readonly [K in keyof T]: T[K] extends readonly (infer U)[]
		? DeepMutable<U>[]
		: T[K] extends object
			? DeepMutable<T[K]>
			: T[K];
};

const SNAPSHOT_PATH = join(REPO_ROOT, BASELINE_PATH);

function baselineSnapshot() {
	return loadSnapshot(readFileSync(SNAPSHOT_PATH, "utf8"));
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
			// Re-anchored by the Bin P patch sweep (4e10a0a) and the Bin M
			// minor sweep (e331816) and the Bin X1 major bump: these are the from-versions every later
			// epoch diffs against.
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
			"serde-saphyr@0.0.29",
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

	test("direct registry pins carry licenses and the toolchain/bun pins agree", () => {
		const { content } = baselineSnapshot();
		const direct = content.rust.packages.filter((p) => p.direct && p.source === "registry");
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
		// JSON round-trip: the drift probe mutates a writable copy of the
		const mutable = () =>
			JSON.parse(JSON.stringify(snapshot.content)) as DeepMutable<SbomContent>;
		const live = mutable();
		const serde = live.rust.packages.find((p) => p.name === "serde");
		if (serde === undefined) throw new Error("fixture lost serde");
		serde.version = "1.0.230";
		expect(verifySnapshot(snapshot, live).some((d) => d.includes("serde"))).toBe(true);

		const toolchainMoved = mutable();
		toolchainMoved.rust.toolchainChannel = "1.98.0";
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
