import { describe, expect, test } from "bun:test";
import {
	NOISE_EXIT_CODE,
	NOISE_RELATIVE_SPREAD_LIMIT,
	NoiseRejection,
	REMEDIATION_LADDER,
	formatNoiseRejection,
	requireQuiet,
	spreadStats,
	type NoisyDistribution,
} from "../statistics.ts";

function labeled(
	label: string,
	values: readonly number[],
	median: number,
): NoisyDistribution {
	const spread = spreadStats(values, median);
	return {
		label,
		count: values.length,
		median,
		stddev: spread.stddev,
		relativeSpread: spread.relativeSpread,
	};
}

describe("spreadStats", () => {
	test("empty array is degenerate quiet zeros", () => {
		expect(spreadStats([], 0)).toEqual({ stddev: 0, relativeSpread: 0 });
	});

	test("population stddev divides by n not n-1", () => {
		const result = spreadStats([1, 2, 3, 4, 5], 3);
		expect(result.stddev).toBe(Math.sqrt(2));
		expect(result.relativeSpread).toBe(Math.sqrt(2) / 3);
	});

	test("relative spread divides by median not mean", () => {
		const values = [1, 1, 1, 1, 100] as const;
		const median = 1;
		const mean = 20.8;
		const result = spreadStats(values, median);
		const expectedStddev = Math.sqrt(
			values.reduce((sum, value) => sum + (value - mean) ** 2, 0) / values.length,
		);
		expect(result.stddev).toBe(expectedStddev);
		expect(result.relativeSpread).toBe(expectedStddev / median);
		expect(result.relativeSpread).not.toBe(expectedStddev / mean);
	});

	test("constant zero is quiet", () => {
		expect(spreadStats([0, 0, 0], 0)).toEqual({ stddev: 0, relativeSpread: 0 });
	});

	test("nonconstant median zero is undefined and noisy", () => {
		const result = spreadStats([0, 0, 1], 0);
		expect(result.stddev).toBeGreaterThan(0);
		expect(result.relativeSpread).toBeNull();
	});

	test("single element is quiet", () => {
		expect(spreadStats([7], 7)).toEqual({ stddev: 0, relativeSpread: 0 });
	});
});

describe("requireQuiet boundary and class semantics", () => {
	test("relative spread exactly 0.20 is quiet", () => {
		expect(() =>
			requireQuiet([
				{
					label: "boundary",
					count: 2,
					median: 10,
					stddev: 2,
					relativeSpread: NOISE_RELATIVE_SPREAD_LIMIT,
				},
			]),
		).not.toThrow();
	});

	test("relative spread above 0.20 is noisy", () => {
		expect(() =>
			requireQuiet([
				{
					label: "noisy",
					count: 2,
					median: 10,
					stddev: 2.0000001,
					relativeSpread: NOISE_RELATIVE_SPREAD_LIMIT + Number.EPSILON,
				},
			]),
		).toThrow(NoiseRejection);
	});

	test("median-zero nonconstant distribution is noisy", () => {
		expect(() => requireQuiet([labeled("zero-median", [0, 0, 1], 0)])).toThrow(NoiseRejection);
	});

	test("collects every noisy distribution in one rejection", () => {
		try {
			requireQuiet([
				labeled("a", [1, 100], 1),
				{
					label: "b",
					count: 2,
					median: 10,
					stddev: 1,
					relativeSpread: 0.1,
				},
				labeled("c", [0, 0, 1], 0),
			]);
			throw new Error("expected NoiseRejection");
		} catch (error) {
			expect(error).toBeInstanceOf(NoiseRejection);
			expect((error as NoiseRejection).noisy.map((entry) => entry.label)).toEqual(["a", "c"]);
		}
	});

	test("NoiseRejection is distinct from Error subclasses used by harness", () => {
		const rejection = new NoiseRejection([
			{
				label: "x",
				count: 1,
				median: 1,
				stddev: 1,
				relativeSpread: 1,
			},
		]);
		expect(rejection.name).toBe("NoiseRejection");
		expect(rejection).toBeInstanceOf(Error);
		expect(rejection).not.toHaveProperty("stage");
		expect(rejection).not.toHaveProperty("failures");
		expect(NOISE_EXIT_CODE).toBe(2);
	});
});

describe("formatNoiseRejection", () => {
	test("emits every remediation step in D4 order with governor pin first", () => {
		const text = formatNoiseRejection([
			{
				label: "stream",
				count: 3,
				median: 0,
				stddev: 1,
				relativeSpread: null,
			},
		]);
		expect(text).toContain("stream:");
		expect(text).toContain("undefined (median zero)");
		expect(REMEDIATION_LADDER).toEqual([
			"pin CPU frequency/governor",
			"isolate the process",
			"widen sample counts",
			"enlarge the input",
		]);
		const positions = REMEDIATION_LADDER.map((step) => text.indexOf(step));
		expect(positions.every((position) => position >= 0)).toBe(true);
		expect(positions).toEqual([...positions].sort((left, right) => left - right));
		expect(text.indexOf("pin CPU frequency/governor")).toBeLessThan(text.indexOf("isolate the process"));
	});
});
