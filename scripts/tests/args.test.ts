import { describe, expect, test } from "bun:test";

import {
	ArgvHelpRequested,
	InvalidSourceDateEpochError,
	MissingArgValueError,
	MissingTargetError,
	parseReleaseArgs,
	UnknownArgError,
} from "../release/args.ts";
import { InvalidTargetError, RUST_TARGETS } from "../release/targets.ts";

describe("args", () => {
	test("resolves the minimal happy path with defaults", () => {
		const args = parseReleaseArgs(["--target", "x86_64-unknown-linux-gnu"]);
		expect(args.plan.rustTarget).toBe("x86_64-unknown-linux-gnu");
		expect(args.dryRun).toBe(false);
		expect(args.noCargo).toBe(false);
		expect(args.skipHostTests).toBe(false);
		expect(args.handshake).toBe(true);
		expect(args.sourceDateEpoch).toBe("0");
	});

	test("honors --dry-run and --no-cargo independently", () => {
		const dryArgs = parseReleaseArgs([
			"--target",
			"x86_64-apple-darwin",
			"--dry-run",
		]);
		expect(dryArgs.dryRun).toBe(true);
		expect(dryArgs.noCargo).toBe(false);

		const noCargoArgs = parseReleaseArgs([
			"--target",
			"x86_64-apple-darwin",
			"--no-cargo",
		]);
		expect(noCargoArgs.dryRun).toBe(false);
		expect(noCargoArgs.noCargo).toBe(true);
	});

	test("expands --out to an absolute path", () => {
		const abs = parseReleaseArgs(
			["--target", "x86_64-apple-darwin", "--out", "/tmp/release"],
			"/cwd",
		);
		expect(abs.outDir).toBe("/tmp/release");

		const relative = parseReleaseArgs(
			["--target", "x86_64-apple-darwin", "--out", "artifacts"],
			"/cwd",
		);
		expect(relative.outDir).toBe("/cwd/artifacts");

		const defaultArgs = parseReleaseArgs(
			["--target", "x86_64-apple-darwin"],
			"/cwd",
		);
		expect(defaultArgs.outDir).toBe("/cwd/dist/release");
	});

	test("accepts the --out-dir alias", () => {
		const args = parseReleaseArgs(
			["--target", "x86_64-apple-darwin", "--out-dir", "/tmp/r2"],
			"/cwd",
		);
		expect(args.outDir).toBe("/tmp/r2");
	});

	test("rejects missing --target with MissingTargetError", () => {
		expect(() => parseReleaseArgs([])).toThrow(MissingTargetError);
	});

	test("rejects unsupported targets via planFor", () => {
		expect(() => parseReleaseArgs(["--target", "riscv64gc-unknown-linux-gnu"])).toThrow(
			InvalidTargetError,
		);
	});

	test("rejects unknown flags", () => {
		expect(() =>
			parseReleaseArgs(["--target", "x86_64-apple-darwin", "--bogus"]),
		).toThrow(UnknownArgError);
	});

	test("rejects missing values for value-taking flags", () => {
		expect(() => parseReleaseArgs(["--target"])).toThrow(MissingArgValueError);
		expect(() =>
			parseReleaseArgs(["--target", "x86_64-apple-darwin", "--out"]),
		).toThrow(MissingArgValueError);
		expect(() =>
			parseReleaseArgs(["--target", "x86_64-apple-darwin", "--out", "-x"]),
		).toThrow(MissingArgValueError);
	});

	test("rejects non-decimal --source-date-epoch", () => {
		expect(() =>
			parseReleaseArgs([
				"--target",
				"x86_64-apple-darwin",
				"--source-date-epoch",
				"1.5",
			]),
		).toThrow(InvalidSourceDateEpochError);
		expect(() =>
			parseReleaseArgs([
				"--target",
				"x86_64-apple-darwin",
				"--source-date-epoch",
				"abc",
			]),
		).toThrow(InvalidSourceDateEpochError);
		expect(() =>
			parseReleaseArgs(
				["--target", "x86_64-apple-darwin"],
				"/cwd",
				"-1",
			),
		).toThrow(InvalidSourceDateEpochError);
	});

	test("throws ArgvHelpRequested on --help / -h", () => {
		expect(() => parseReleaseArgs(["--help"])).toThrow(ArgvHelpRequested);
		expect(() => parseReleaseArgs(["-h"])).toThrow(ArgvHelpRequested);
	});

	test("forwards SOURCE_DATE_EPOCH env when --source-date-epoch is absent", () => {
		const args = parseReleaseArgs(["--target", "x86_64-apple-darwin"], "/cwd", "1700000000");
		expect(args.sourceDateEpoch).toBe("1700000000");
	});

	test("explicit --source-date-epoch overrides env", () => {
		const args = parseReleaseArgs(
			["--target", "x86_64-apple-darwin", "--source-date-epoch", "42"],
			"/cwd",
			"1700000000",
		);
		expect(args.sourceDateEpoch).toBe("42");
	});

	test("--no-handshake disables handshake", () => {
		const args = parseReleaseArgs(["--target", "x86_64-apple-darwin", "--no-handshake"]);
		expect(args.handshake).toBe(false);
	});

	test("--skip-host-tests disables host test step", () => {
		const args = parseReleaseArgs(["--target", "x86_64-apple-darwin", "--skip-host-tests"]);
		expect(args.skipHostTests).toBe(true);
	});

	test("iterates over every supported triple without throwing", () => {
		for (const triple of RUST_TARGETS) {
			const args = parseReleaseArgs(["--target", triple]);
			expect(args.plan.rustTarget).toBe(triple);
		}
	});
});
