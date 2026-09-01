import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import {
	buildFrames,
	buildManifest,
	diffArtifact,
	loadLifecycle,
} from "./gen-witness.ts";

const HOST_SOURCE_PATH =
	"crates/pi/src/core/extension_host.rs";
const FIXTURES_PATH =
	"packages/pi-tui-protocol/tests/fixtures/frames.jsonl";
const MANIFEST_PATH =
	"packages/pi-tui-protocol/tests/fixtures/witness-manifest.json";

const lifecycle = loadLifecycle(readFileSync(HOST_SOURCE_PATH, "utf8"));

describe("gen-witness determinism", () => {
	test("two runs build byte-identical artifacts", () => {
		expect(buildFrames(lifecycle)).toBe(buildFrames(lifecycle));
		expect(buildManifest(lifecycle).text).toBe(buildManifest(lifecycle).text);
	});

	test("committed artifacts equal the pure builders' output (verify:witness --check)", () => {
		const framesText = readFileSync(FIXTURES_PATH, "utf8");
		const { text: manifestText } = buildManifest(lifecycle);
		expect(framesText).toBe(buildFrames(lifecycle));
		expect(readFileSync(MANIFEST_PATH, "utf8")).toBe(manifestText);
		// The --check path is diffArtifact over exactly these two comparisons.
		expect([
			...diffArtifact("frames.jsonl", framesText, buildFrames(lifecycle)),
			...diffArtifact("witness-manifest.json", readFileSync(MANIFEST_PATH, "utf8"), manifestText),
		]).toEqual([]);
	});

	test("--check rejects a mutated committed artifact with the first differing line", () => {
		const mutated = buildFrames(lifecycle).replace('"title"', '"titel"');
		expect(mutated).not.toBe(buildFrames(lifecycle));
		const violations = diffArtifact("frames.jsonl", mutated, buildFrames(lifecycle));
		expect(violations).toHaveLength(1);
		expect(violations[0]).toContain("first differing line");
	});

	test("lifecycle parses to exactly 35 ordered discriminants", () => {
		expect(lifecycle).toHaveLength(35);
		expect(lifecycle[0]).toBe("project_trust");
		expect(lifecycle[34]).toBe("input");
	});
});
