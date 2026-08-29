import { describe, expect, test } from "bun:test";
import { existsSync, readFileSync } from "node:fs";
import { resolve } from "node:path";
import {
	NOISE_ROUNDS,
	NOISE_ROUND_WARMUPS,
	RustSamplerError,
	SAMPLES_PER_ROUND,
	parseRustSamplerOutput,
	roundNoiseLane,
	roundSummary,
	runRustSampler,
	stats,
	validateRustSamplerReport,
} from "../bench-extension-scaling.ts";
import { NOISE_RELATIVE_SPREAD_LIMIT, requireQuiet } from "../statistics.ts";

const REPOSITORY_ROOT = resolve(import.meta.dirname, "../..");
const ARTIFACT_PATH = resolve(REPOSITORY_ROOT, "target/bench/extension-scaling.json");

describe("bench-extension-scaling stats regression", () => {
	test("preserves ceil-rank quantiles and mean while adding population spread", () => {
		const result = stats([1, 2, 3, 4, 5]);
		expect(result).toEqual({
			median: 3,
			p95: 5,
			p99: 5,
			mean: 3,
			n: 5,
			stddev: Math.sqrt(2),
			relativeSpread: Math.sqrt(2) / 3,
		});
	});

	test("empty samples stay zero including additive spread fields", () => {
		expect(stats([])).toEqual({
			median: 0,
			p95: 0,
			p99: 0,
			mean: 0,
			n: 0,
			stddev: 0,
			relativeSpread: 0,
		});
	});

	test("does not run the benchmark when the module is imported", () => {
		const before = existsSync(ARTIFACT_PATH) ? readFileSync(ARTIFACT_PATH) : undefined;
		expect(typeof stats).toBe("function");
		const after = existsSync(ARTIFACT_PATH) ? readFileSync(ARTIFACT_PATH) : undefined;
		expect(after).toEqual(before);
	});
});

describe("bench-extension-scaling round-median noise validity", () => {
	// Ten rounds of ten samples: nine samples at the base value plus one 10x
	// outlier per round. Every round median sits exactly at the base, while the
	// pooled raw distribution carries a >20% relative spread the unchanged
	// pooled gate would reject.
	const stableRounds = Array.from({ length: NOISE_ROUNDS }, () => [
		1, 1, 1, 1, 1, 1, 1, 1, 1, 10,
	]);

	test("pooled-tail noise passes only through stable round medians", () => {
		const pooled = stats(stableRounds.flat());
		expect(pooled.n).toBe(NOISE_ROUNDS * 10);
		expect(pooled.relativeSpread).not.toBeNull();
		expect(pooled.relativeSpread).toBeGreaterThan(NOISE_RELATIVE_SPREAD_LIMIT);

		const rounds = roundSummary(stableRounds);
		expect(rounds.roundMedians).toEqual(Array.from({ length: NOISE_ROUNDS }, () => 1));
		expect(rounds.roundRelativeSpread).not.toBeNull();
		expect(rounds.roundRelativeSpread).toBeLessThanOrEqual(NOISE_RELATIVE_SPREAD_LIMIT);

		// The exact lane object the benchmark feeds requireQuiet stays quiet.
		expect(() => requireQuiet([roundNoiseLane("pooled-tail lane", rounds)])).not.toThrow();
	});

	test("genuinely bimodal round medians fail the unchanged 20% limit", () => {
		const bimodalRounds = Array.from({ length: NOISE_ROUNDS }, (_, round) =>
			round % 2 === 0 ? Array.from({ length: 10 }, () => 1) : Array.from({ length: 10 }, () => 2),
		);
		const rounds = roundSummary(bimodalRounds);
		expect(rounds.roundMedians).toEqual(
			Array.from({ length: NOISE_ROUNDS }, (_, round) => (round % 2 === 0 ? 1 : 2)),
		);
		expect(rounds.roundRelativeSpread).not.toBeNull();
		// The alternating medians have enough variance to breach the unchanged
		// 20% limit regardless of whether the configured round count is odd.
		expect(rounds.roundMedian).toBe(1);
		expect(rounds.roundRelativeSpread).toBeGreaterThan(NOISE_RELATIVE_SPREAD_LIMIT);
		expect(() => requireQuiet([roundNoiseLane("bimodal lane", rounds)])).toThrow();
	});

	test("refuses to manufacture stability from zero rounds", () => {
		expect(() => roundSummary([])).toThrow(/at least one measured round/);
	});

	test("keeps the batching shape documented in the artifact", () => {
		expect(NOISE_ROUNDS).toBe(27);
		expect(NOISE_ROUND_WARMUPS).toBe(20);
		expect(SAMPLES_PER_ROUND).toBe(1_000);
	});
});

// ---------------------------------------------------------------------------
// Composite Rust producer contract
// ---------------------------------------------------------------------------

function measuredSamples(value = 0.5): number[] {
	return Array.from({ length: 9 }, (_, index) => value + index / 1_000);
}

function validSamplerReport() {
	return {
		schemaVersion: 1,
		provenance: {
			entrypoint: "pi_ext::server::serve_io",
			frameCodec: "pi_ext::protocol::{encode_frame,decode_frame_str}",
			protocol: {
				compiledProtocolVersion: 1,
				compiledCompatibilityVersion: "0.80.10",
				observedProtocolVersion: 1,
				observedCompatibilityVersion: "0.80.10",
			},
			corpus: {
				identity: "extension-scaling-terminal-input-v1",
				digestAlgorithm: "fnv1a64",
				digest: "1658b7155567cb02",
				measuredRounds: 9,
				warmupsPerScenario: 30,
				samplesPerScenario: 10000,
				fastStreamSamples: 10000,
			},
		},
		scenarios: [
			{
				scenario: "zero",
				extensionCount: 0,
				terminalInputMode: "passThrough",
				requestsPerSample: 10000,
				normalizedSamplesMs: measuredSamples(),
			},
			{
				scenario: "idle100",
				extensionCount: 100,
				terminalInputMode: "passThrough",
				requestsPerSample: 10000,
				normalizedSamplesMs: measuredSamples(),
			},
			{
				scenario: "active20",
				extensionCount: 20,
				terminalInputMode: "passThrough",
				requestsPerSample: 10000,
				normalizedSamplesMs: measuredSamples(),
			},
			{
				scenario: "fastTerminalInput",
				extensionCount: 1,
				terminalInputMode: "fast",
				requestsPerSample: 10000,
				normalizedSamplesMs: measuredSamples(),
			},
			{
				scenario: "slowTerminalInput",
				extensionCount: 2,
				terminalInputMode: "slowThenFast",
				requestsPerSample: 10000,
				normalizedSamplesMs: measuredSamples(),
				timeoutSamplesMs: measuredSamples(4.1),
				localitySamplesMs: measuredSamples(0.1),
			},
		],
	};
}

describe("bench-extension-scaling rust sampler contract", () => {
	test("accepts a well-formed production serve_io report", () => {
		const report = parseRustSamplerOutput(`${JSON.stringify(validSamplerReport())}\n`);
		expect(() => validateRustSamplerReport(report)).not.toThrow();
	});

	test("rejects missing, multi-line, or non-JSON sampler output", () => {
		expect(() => parseRustSamplerOutput("")).toThrow(RustSamplerError);
		expect(() => parseRustSamplerOutput("   \n")).toThrow(/exactly one JSONL/);
		expect(() =>
			parseRustSamplerOutput("cargo test prose\n{\"schemaVersion\":1}\n"),
		).toThrow(/exactly one JSONL/);
		expect(() => parseRustSamplerOutput("{not json}")).toThrow(/not valid JSON/);
		expect(() => parseRustSamplerOutput("[1,2]")).toThrow(/JSON object/);
	});

	test("rejects a bypassed producer that does not name serve_io", () => {
		const bypassed = validSamplerReport();
		bypassed.provenance.entrypoint = "custom_bench_loop";
		expect(() => validateRustSamplerReport(bypassed)).toThrow(/entrypoint/);
	});

	test("rejects protocol provenance mismatches against the TS mirrors", () => {
		const wrongVersion = validSamplerReport();
		wrongVersion.provenance.protocol.observedProtocolVersion = 2;
		expect(() => validateRustSamplerReport(wrongVersion)).toThrow(/protocol provenance mismatch/);

		const wrongCompat = validSamplerReport();
		wrongCompat.provenance.protocol.observedCompatibilityVersion = "9.9.9";
		expect(() => validateRustSamplerReport(wrongCompat)).toThrow(/protocol provenance mismatch/);
	});

	test("rejects corpus digest and identity tampering", () => {
		const wrongIdentity = validSamplerReport();
		wrongIdentity.provenance.corpus.identity = "other-corpus";
		expect(() => validateRustSamplerReport(wrongIdentity)).toThrow(/corpus identity/);

		const wrongDigest = validSamplerReport();
		wrongDigest.provenance.corpus.digest = "zzzz";
		expect(() => validateRustSamplerReport(wrongDigest)).toThrow(/digest/);

		const wrongShape = validSamplerReport();
		wrongShape.provenance.corpus.measuredRounds = 1;
		expect(() => validateRustSamplerReport(wrongShape)).toThrow(/measurement shape/);
	});

	test("rejects missing, duplicated, or malformed scenarios", () => {
		const missing = validSamplerReport();
		missing.scenarios = missing.scenarios.filter((s) => s.scenario !== "slowTerminalInput");
		expect(() => validateRustSamplerReport(missing)).toThrow(/missing scenarios/);

		const duplicated = validSamplerReport();
		const firstScenario = duplicated.scenarios[0];
		if (!firstScenario) throw new Error("scenario fixture is empty");
		duplicated.scenarios = [...duplicated.scenarios, { ...firstScenario }];
		expect(() => validateRustSamplerReport(duplicated)).toThrow(/duplicate scenario/);

		const wrongLoad = validSamplerReport();
		const idleScenario = wrongLoad.scenarios[1];
		if (!idleScenario) throw new Error("idle scenario fixture is missing");
		idleScenario.extensionCount = 3;
		expect(() => validateRustSamplerReport(wrongLoad)).toThrow(/extensions/);

		const badSamples = validSamplerReport();
		const zeroScenario = badSamples.scenarios[0];
		if (!zeroScenario) throw new Error("zero scenario fixture is missing");
		zeroScenario.normalizedSamplesMs = [0.5, Number.NaN];

		const shortSamples = validSamplerReport();
		const shortZero = shortSamples.scenarios[0];
		if (!shortZero) throw new Error("zero scenario fixture is missing");
		shortZero.normalizedSamplesMs = [0.5];
		expect(() => validateRustSamplerReport(shortSamples)).toThrow(/sample count/);

		const wrongMode = validSamplerReport();
		const wrongModeZero = wrongMode.scenarios[0];
		if (!wrongModeZero) throw new Error("zero scenario fixture is missing");
		wrongModeZero.terminalInputMode = "fast";
		expect(() => validateRustSamplerReport(wrongMode)).toThrow(/mode/);

		const missingSlowEvidence = validSamplerReport();
		const slow = missingSlowEvidence.scenarios.find(
			(scenario) => scenario.scenario === "slowTerminalInput",
		);
		if (!slow) throw new Error("slow scenario fixture is missing");
		Reflect.deleteProperty(slow, "timeoutSamplesMs");
		expect(() => validateRustSamplerReport(missingSlowEvidence)).toThrow(/timeoutSamplesMs/);
		expect(() => validateRustSamplerReport(badSamples)).toThrow(/finite non-negative/);
	});

	test("missing or bypassed sampler binary prevents the artifact pass", () => {
		expect(() =>
			runRustSampler({ build: false, binaryPath: "/nonexistent/pi-extension-scaling" }),
		).toThrow(RustSamplerError);
		// A real binary that does not implement the sampler protocol also fails
		// closed: echo exits 0 but its stdout is not a sampler report.
		expect(() => runRustSampler({ build: false, binaryPath: "/bin/echo" })).toThrow(
			RustSamplerError,
		);
	});
});
