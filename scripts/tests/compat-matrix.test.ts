import { readFileSync } from "node:fs";
import { join } from "node:path";

import { describe, expect, test } from "bun:test";

import {
	runCommand,
	runMatrix,
	selectRows,
	uniqueCommands,
	validateMatrix,
	type Matrix,
} from "../verification/compat-matrix.ts";

function validCommand() {
	return {
		cwd: ".",
		argv: ["bun", "-e", "process.exit(0)"],
		timeoutMs: 10_000,
	};
}

function failingCommand() {
	return {
		cwd: ".",
		argv: ["bun", "-e", "process.exit(2)"],
		timeoutMs: 10_000,
	};
}

function minimalMatrix(): Matrix {
	return {
		version: "0.1.0",
		rows: [
			{
				id: "r1",
				surface: "handshake",
				tier: "host",
				required: true,
				commands: [validCommand()],
				evidence: "test",
			},
		],
	};
}

describe("validateMatrix", () => {
	test("accepts a minimal valid matrix", () => {
		expect(() => validateMatrix(minimalMatrix())).not.toThrow();
	});

	test("rejects a non-object", () => {
		expect(() => validateMatrix(null)).toThrow("Matrix must be an object");
	});

	test("rejects missing rows", () => {
		expect(() => validateMatrix({})).toThrow("rows must be an array");
	});

	test("rejects duplicate ids", () => {
		const matrix = {
			rows: [
				{ id: "r1", surface: "a", tier: "unit", required: true, commands: [validCommand()], evidence: "e" },
				{ id: "r1", surface: "b", tier: "unit", required: true, commands: [validCommand()], evidence: "e" },
			],
		};
		expect(() => validateMatrix(matrix)).toThrow("duplicate row id: r1");
	});

	test("rejects an invalid tier", () => {
		const matrix = {
			rows: [
				{ id: "r1", surface: "a", tier: "warp", required: true, commands: [validCommand()], evidence: "e" },
			],
		};
		expect(() => validateMatrix(matrix)).toThrow("tier must be one of");
	});

	test("rejects empty evidence", () => {
		const matrix = {
			rows: [
				{ id: "r1", surface: "a", tier: "unit", required: true, commands: [validCommand()], evidence: "" },
			],
		};
		expect(() => validateMatrix(matrix)).toThrow("evidence must be a non-empty string");
	});

	test("rejects an excluded row that still has commands", () => {
		const matrix = {
			rows: [
				{
					id: "r1",
					surface: "a",
					tier: "unit",
					required: false,
					excluded: true,
					commands: [validCommand()],
					evidence: "e",
				},
			],
		};
		expect(() => validateMatrix(matrix)).toThrow("excluded and must not have commands");
	});
});

describe("selectRows", () => {
	const matrix: Matrix = {
		version: "0.1.0",
		rows: [
			{ id: "r1", surface: "a", tier: "unit", required: true, commands: [validCommand()], evidence: "e" },
			{ id: "r2", surface: "b", tier: "unit", required: false, commands: [validCommand()], evidence: "e" },
			{ id: "r3", surface: "c", tier: "host", required: true, excluded: true, commands: [], evidence: "e" },
			{ id: "r4", surface: "d", tier: "host", required: false, commands: [validCommand()], evidence: "e" },
		],
	};

	test("default selection returns required, non-excluded rows", () => {
		const selected = selectRows(matrix, {});
		expect(selected.map((row) => row.id)).toEqual(["r1"]);
	});

	test("--row selects a specific row regardless of required/excluded", () => {
		const selected = selectRows(matrix, { rows: ["r3"] });
		expect(selected.map((row) => row.id)).toEqual(["r3"]);
	});

	test("--tier selects all rows in that tier", () => {
		const selected = selectRows(matrix, { tier: "host" });
		expect([...selected.map((row) => row.id)].sort()).toEqual(["r3", "r4"]);
	});

	test("--row and --tier are combined as a union", () => {
		const selected = selectRows(matrix, { rows: ["r2"], tier: "host" });
		expect([...selected.map((row) => row.id)].sort()).toEqual(["r2", "r3", "r4"]);
	});
});

describe("uniqueCommands", () => {
	const commandA = validCommand();
	const commandB = { ...validCommand(), argv: ["bun", "-e", "process.exit(1)"] };

	test("deduplicates identical commands and collects row ids", () => {
		const matrix: Matrix = {
			rows: [
				{ id: "r1", surface: "a", tier: "unit", required: true, commands: [commandA], evidence: "e" },
				{ id: "r2", surface: "b", tier: "unit", required: true, commands: [commandA], evidence: "e" },
				{ id: "r3", surface: "c", tier: "unit", required: true, commands: [commandB], evidence: "e" },
			],
		};
		const groups = uniqueCommands(matrix.rows);
		expect(groups).toHaveLength(2);
		const keys = groups.map((group) => [...group.rowIds].sort());
		expect(keys).toContainEqual(["r1", "r2"]);
		expect(keys).toContainEqual(["r3"]);
	});

	test("ignores excluded and empty-command rows", () => {
		const matrix: Matrix = {
			rows: [
				{ id: "r1", surface: "a", tier: "unit", required: true, commands: [validCommand()], evidence: "e" },
				{ id: "r2", surface: "b", tier: "unit", required: false, excluded: true, commands: [], evidence: "e" },
			],
		};
		const groups = uniqueCommands(matrix.rows);
		expect(groups).toHaveLength(1);
		expect(groups[0]?.rowIds).toEqual(["r1"]);
	});
});

describe("runMatrix", () => {
	const repoRoot = process.cwd();

	test("dry-run records every selected row as dry-run/skipped", async () => {
		const matrix: Matrix = {
			version: "0.1.0",
			rows: [
				{ id: "r1", surface: "a", tier: "unit", required: true, commands: [validCommand()], evidence: "e" },
				{ id: "r2", surface: "b", tier: "unit", required: false, excluded: true, commands: [], evidence: "e", rationale: "x" },
			],
		};
		const result = await runMatrix({
			matrix,
			matrixPath: "scripts/verification/compat-matrix.json",
			repoRoot,
			request: { rows: ["r1", "r2"] },
			dryRun: true,
		});
		const r1 = result.rowResults.find((row) => row.rowId === "r1");
		const r2 = result.rowResults.find((row) => row.rowId === "r2");
		expect(r1?.status).toBe("dry-run");
		expect(r2?.status).toBe("skipped");
		expect(result.commandResults).toHaveLength(0);
		expect(result.summary.requiredFailed).toHaveLength(0);
	});

	test("records a passing required row", async () => {
		const matrix = minimalMatrix();
		const result = await runMatrix({
			matrix,
			matrixPath: "scripts/verification/compat-matrix.json",
			repoRoot,
			request: {},
			dryRun: false,
		});
		expect(result.rowResults[0]?.status).toBe("passed");
		expect(result.summary.requiredFailed).toHaveLength(0);
	});

	test("records a failing required row and populates requiredFailed", async () => {
		const matrix: Matrix = {
			version: "0.1.0",
			rows: [
				{
					id: "r1",
					surface: "a",
					tier: "unit",
					required: true,
					commands: [failingCommand()],
					evidence: "e",
				},
			],
		};
		const result = await runMatrix({
			matrix,
			matrixPath: "scripts/verification/compat-matrix.json",
			repoRoot,
			request: {},
			dryRun: false,
		});
		expect(result.rowResults[0]?.status).toBe("failed");
		expect(result.rowResults[0]?.exitCode).toBe(2);
		expect(result.summary.requiredFailed).toEqual(["r1"]);
	});

	test("does not list an optional failing row in requiredFailed", async () => {
		const matrix: Matrix = {
			version: "0.1.0",
			rows: [
				{
					id: "r1",
					surface: "a",
					tier: "unit",
					required: false,
					commands: [failingCommand()],
					evidence: "e",
				},
			],
		};
		const result = await runMatrix({
			matrix,
			matrixPath: "scripts/verification/compat-matrix.json",
			repoRoot,
			request: { rows: ["r1"] },
			dryRun: false,
		});
		expect(result.rowResults[0]?.status).toBe("failed");
		expect(result.summary.requiredFailed).toHaveLength(0);
	});

	test("returns a failed required row when the binary is missing", async () => {
		const matrix: Matrix = {
			version: "0.1.0",
			rows: [
				{
					id: "r1",
					surface: "a",
					tier: "unit",
					required: true,
					commands: [
						{
							cwd: ".",
							argv: ["this-binary-should-not-exist-12345"],
							timeoutMs: 10_000,
						},
					],
					evidence: "e",
				},
			],
		};
		const result = await runMatrix({
			matrix,
			matrixPath: "scripts/verification/compat-matrix.json",
			repoRoot,
			request: { rows: ["r1"] },
			dryRun: false,
		});
		expect(result.rowResults[0]?.status).toBe("failed");
		expect(result.rowResults[0]?.exitCode).toBeNull();
		expect(result.rowResults[0]?.error).toMatch(/launch\/read error/);
		expect(result.summary.requiredFailed).toEqual(["r1"]);
	});

	test("fails a required row with a named prerequisite error when requires is missing", async () => {
		const matrix: Matrix = {
			version: "0.2.0",
			rows: [
				{
					id: "r1",
					surface: "a",
					tier: "unit",
					required: true,
					requires: ["/nonexistent-musl-prerequisite-path"],
					commands: [validCommand()],
					evidence: "e",
				},
			],
		};
		const result = await runMatrix({
			matrix,
			matrixPath: "scripts/verification/compat-matrix.json",
			repoRoot,
			request: {},
			dryRun: false,
		});
		expect(result.rowResults[0]?.status).toBe("failed");
		expect(result.rowResults[0]?.error).toBe(
			"missing prerequisite: /nonexistent-musl-prerequisite-path not found under " + repoRoot,
		);
		expect(result.summary.requiredFailed).toEqual(["r1"]);
		expect(result.commandResults).toHaveLength(0);
	});
});

describe("runCommand", () => {
	test("captures exit code and output", async () => {
		const result = await runCommand(
			{
				cwd: ".",
				argv: ["bun", "-e", "process.stdout.write('ok'); process.exit(0)"],
				timeoutMs: 10_000,
			},
			process.cwd(),
		);
		expect(result.exitCode).toBe(0);
		expect(result.stdout).toContain("ok");
		expect(result.timedOut).toBe(false);
	});

	test("captures a non-zero exit", async () => {
		const result = await runCommand(
			{
				cwd: ".",
				argv: ["bun", "-e", "process.stderr.write('err'); process.exit(3)"],
				timeoutMs: 10_000,
			},
			process.cwd(),
		);
		expect(result.exitCode).toBe(3);
		expect(result.stderr).toContain("err");
	});
});

describe("release tier matrix 0.2.0 (REL-T5)", () => {
	const repoRoot = join(import.meta.dir, "../..");
	const matrixPath = join(repoRoot, "scripts/verification/compat-matrix.json");
	const raw = readFileSync(matrixPath, "utf8");
	const matrix = validateMatrix(JSON.parse(raw));
	const releaseRows = selectRows(matrix, { tier: "release" });
	const muslRows = releaseRows.filter((row) => row.id.endsWith("-musl"));
	const tierNRows = releaseRows.filter(
		(row) =>
			/(?<!Not a )Tier N terminal-conformance row/.test(row.evidence) ||
			(row.rationale !== undefined &&
				/(?<!Not a )Tier N terminal-conformance row/.test(row.rationale)),
	);
	const excludedReleaseRows = releaseRows.filter((row) => row.excluded === true);
	const locallyRequiredReleaseRows = releaseRows.filter(
		(row) => row.required && row.excluded !== true,
	);

	/** Verbatim absence line owned by the musl transcript lane. */
	function canonicalAbsenceLine(): string {
		const source = readFileSync(
			join(repoRoot, "crates/pi-tui/tests/transcript_musl_smoke.rs"),
			"utf8",
		);
		const match = source.match(/const ABSENCE_LINE: &str = "([^"]+)";/);
		if (match === null || match[1] === undefined) {
			throw new Error("ABSENCE_LINE not found in transcript_musl_smoke.rs");
		}
		return match[1];
	}

	test("validateMatrix passes on the committed matrix at version 0.2.0", () => {
		expect(matrix.version).toBe("0.2.0");
	});

	test("selectRows tier:release returns exactly the seven release target rows", () => {
		expect(releaseRows.map((row) => row.id)).toEqual([
			"release-x86_64-linux",
			"release-aarch64-linux",
			"release-x86_64-darwin",
			"release-aarch64-darwin",
			"release-x86_64-windows",
			"release-x86_64-linux-musl",
			"release-aarch64-linux-musl",
		]);
	});

	test("exactly five release rows carry the Tier N terminal-conformance claim", () => {
		expect(tierNRows).toHaveLength(5);
		expect(tierNRows.map((row) => row.id).sort()).toEqual([
			"release-aarch64-darwin",
			"release-aarch64-linux",
			"release-x86_64-darwin",
			"release-x86_64-linux",
			"release-x86_64-windows",
		]);
	});

	test("exactly two release rows are locally required and five are excluded", () => {
		expect(locallyRequiredReleaseRows.map((row) => row.id)).toEqual([
			"release-x86_64-linux",
			"release-x86_64-linux-musl",
		]);
		expect(excludedReleaseRows.map((row) => row.id).sort()).toEqual([
			"release-aarch64-darwin",
			"release-aarch64-linux",
			"release-aarch64-linux-musl",
			"release-x86_64-darwin",
			"release-x86_64-windows",
		]);
	});

	test("every excluded release row names its actual CI witness runner", () => {
		const witnesses: Record<string, string> = {
			"release-aarch64-linux": "ubuntu-24.04-arm",
			"release-aarch64-linux-musl": "ubuntu-24.04-arm",
			"release-aarch64-darwin": "macos-15",
			"release-x86_64-darwin": "macos-15-intel",
			"release-x86_64-windows": "windows-2025",
		};
		for (const row of excludedReleaseRows) {
			const text = `${row.evidence} ${row.rationale ?? ""} ${row.citation ?? ""}`;
			const witness = witnesses[row.id];
			if (witness === undefined) throw new Error(`no CI witness pinned for ${row.id}`);
			expect(text).toContain(witness);
		}
	});

	test("both musl rows carry the absence line byte-identically to the transcript lane constant", () => {
		expect(muslRows.map((row) => row.id).sort()).toEqual([
			"release-aarch64-linux-musl",
			"release-x86_64-linux-musl",
		]);
		const absence = canonicalAbsenceLine();
		for (const row of muslRows) {
			expect(row.evidence.includes(absence)).toBe(true);
		}
		// The absence line appears exactly twice in the whole matrix — once per
		// musl row — and never on a Tier N row.
		expect(raw.split(absence).length - 1).toBe(2);
		for (const row of tierNRows) {
			expect(
				row.evidence.includes(absence) || (row.rationale?.includes(absence) ?? false),
			).toBe(false);
		}
	});

	test("local musl row names the musl userland prerequisite paths", () => {
		const row = releaseRows.find((r) => r.id === "release-x86_64-linux-musl");
		expect(row?.requires).toEqual(["/lib/ld-musl-x86_64.so.1", "/etc/ld-musl-x86_64.path"]);
		expect(row?.rationale).toContain("musl-gcc");
	});

	test("aarch64 rationales claim native ubuntu-24.04-arm execution, never the cross-compile fallback", () => {
		for (const id of ["release-aarch64-linux", "release-aarch64-linux-musl"]) {
			const row = matrix.rows.find((r) => r.id === id);
			expect(row?.rationale).toContain("ubuntu-24.04-arm");
			expect(row?.rationale).toContain("no cross-compilation");
			expect(row?.rationale).toContain("no QEMU");
			expect(row?.rationale ?? "").not.toContain("Cross-compiling aarch64");
		}
	});
});
