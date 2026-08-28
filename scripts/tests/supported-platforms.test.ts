import { readFileSync } from "node:fs";
import { join } from "node:path";

import { describe, expect, test } from "bun:test";

/**
 * REL-T9 (#116) citation contract for docs/supported-platforms.md.
 *
 * The document must repeat the two byte-identity carriers — the musl absence
 * line and the five-row Tier N census — exactly as their owning sources spell
 * them, must keep every platform claim anchored to a code line, manifest
 * field, workflow step, or dated pin, and must stay in lockstep with the
 * seven-target release model.
 */
describe("supported-platforms document (REL-T9)", () => {
	const repoRoot = join(import.meta.dir, "../..");
	const doc = readFileSync(join(repoRoot, "docs/supported-platforms.md"), "utf8");
	const compatRaw = readFileSync(
		join(repoRoot, "scripts/verification/compat-matrix.json"),
		"utf8",
	);
	const transcriptMatrix = readFileSync(
		join(repoRoot, "docs/tui-transcript-schema-v1.md"),
		"utf8",
	);
	const muslSmokeSource = readFileSync(
		join(repoRoot, "crates/pi-tui/tests/transcript_musl_smoke.rs"),
		"utf8",
	);
	const censusTestSource = readFileSync(
		join(repoRoot, "scripts/tests/compat-matrix.test.ts"),
		"utf8",
	);

	const absence = muslSmokeSource.match(/const ABSENCE_LINE: &str = "([^"]+)";/)?.[1] ?? "";
	const census =
		censusTestSource.match(
			/test\("(exactly five release rows carry the Tier N terminal-conformance claim)"/,
		)?.[1] ?? "";

	test("absence line is extracted from the owning transcript-lane constant", () => {
		expect(absence).toBe("no PTY/render/synchronized-output/no-clear claims");
	});

	test("musl absence line is byte-identical across matrix, compat matrix, and document", () => {
		// The compat matrix carries the line exactly twice — once per musl row —
		// and never on a Tier N row (the REL-T5 census pins the same count).
		expect(compatRaw.split(absence).length - 1).toBe(2);
		// The transcript matrix's musl smoke-lane row spells the same line.
		expect(transcriptMatrix.includes(absence)).toBe(true);
		// This document repeats it verbatim.
		expect(doc.includes(absence)).toBe(true);
	});

	test("five-row Tier N statement is byte-identical across matrix, compat matrix, and document", () => {
		expect(census).toBe(
			"exactly five release rows carry the Tier N terminal-conformance claim",
		);
		// The census sentence, verbatim, in this document.
		expect(doc.includes(census)).toBe(true);
		// The compat-matrix row phrase survives byte-identically here: six
		// occurrences in the JSON — five positive row claims plus the one
		// "Not a Tier N terminal-conformance row" negation on the musl row.
		expect(compatRaw.split("Tier N terminal-conformance row").length - 1).toBe(6);
		expect(compatRaw.split("Not a Tier N terminal-conformance row").length - 1).toBe(1);
		expect(doc.includes("Tier N terminal-conformance row")).toBe(true);
		// All seven release row ids are named in the document.
		for (const id of [
			"release-x86_64-linux",
			"release-aarch64-linux",
			"release-x86_64-darwin",
			"release-aarch64-darwin",
			"release-x86_64-windows",
			"release-x86_64-linux-musl",
			"release-aarch64-linux-musl",
		]) {
			expect(doc.includes(id)).toBe(true);
		}
	});

	test("document maps each of the seven triples to its native-runner leg", () => {
		for (const triple of [
			"x86_64-unknown-linux-gnu",
			"x86_64-unknown-linux-musl",
			"aarch64-unknown-linux-gnu",
			"aarch64-unknown-linux-musl",
			"x86_64-apple-darwin",
			"aarch64-apple-darwin",
			"x86_64-pc-windows-msvc",
		]) {
			expect(doc.includes(`\`${triple}\``)).toBe(true);
		}
		for (const runner of [
			"ubuntu-latest",
			"ubuntu-24.04-arm",
			"macos-15-intel",
			"macos-15",
			"windows-2025",
		]) {
			expect(doc.includes(runner)).toBe(true);
		}
	});

	test("document records the dated pins it narrates", () => {
		for (const pin of [
			"1.3.14",
			"1.97.1",
			"pi.release.v1",
			"SOURCE_DATE_EPOCH=1735689600",
			"windows-2025",
			"15.2.0-r5",
			"1.2.4-2",
			"3.24.1",
			"4d101475d8b20a2381f78447822ac1eab6504dd8",
		]) {
			expect(doc.includes(pin)).toBe(true);
		}
	});

	test("every normative list item, table row, and statement carries a citation or dated pin", () => {
		const citation =
			/[\w./@~-]+\.(?:ts|rs|json|md|toml|yml)(?::[\d,-]+)?|workflow:\d+|accessed 2026-\d{2}-\d{2}|as of 2026-\d{2}-\d{2}|grounded 2026-\d{2}-\d{2}/;
		const lines = doc.split("\n");
		const uncited: string[] = [];
		let inFence = false;
		for (let index = 0; index < lines.length; index += 1) {
			const line = lines[index] ?? "";
			if (line.startsWith("```")) {
				inFence = !inFence;
				continue;
			}
			if (inFence) continue;
			// Table data rows: each must name its workflow matrix leg. A row whose
			// successor is the `---` separator is the header and is skipped.
			if (line.startsWith("|") && !line.includes("---")) {
				const successor = lines[index + 1] ?? "";
				if (!successor.includes("---") && !citation.test(line)) {
					uncited.push(`${index + 1}: ${line}`);
				}
				continue;
			}
			// A claim unit is a list/ordered item plus its indented continuation
			// lines; the citation may sit on any line of the unit.
			if (!/^\s*(?:-|\d+\.)\s/.test(line)) continue;
			let block = line;
			for (let next = index + 1; next < lines.length; next += 1) {
				const continuation = lines[next] ?? "";
				if (!/^\s{2,}\S/.test(continuation)) break;
				block += `\n${continuation}`;
				index = next;
			}
			if (!citation.test(block)) uncited.push(`${index + 1}: ${line}`);
		}
		expect(uncited).toEqual([]);
	});
});
