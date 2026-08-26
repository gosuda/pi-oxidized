import { describe, expect, test } from "bun:test";
import { existsSync, readFileSync } from "node:fs";
import { resolve } from "node:path";
import { stats } from "../bench-extension-scaling.ts";

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
