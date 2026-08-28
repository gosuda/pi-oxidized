#!/usr/bin/env bun
/**
 * Dedicated keypress-dispatch benchmark runner (PERF-T11 unit keypress-dispatch).
 *
 * Thin CLI over the shared collector in scripts/verification/performance.ts:
 *
 *   single arm:  bun run scripts/bench-keypress-dispatch.ts --binary target/release/pi \
 *                  --rounds 27 --output target/bench/keypress-dispatch.json
 *   paired A/B:  bun run scripts/bench-keypress-dispatch.ts --baseline BASE --design DES \
 *                  --pairs 9 --output target/bench/keypress-ab.json
 * One round = one fresh PTY process with 20 discarded warmup key-clear pairs and
 * 200 measured key-clear pairs. Trust gates (single arm and each paired arm):
 * population stddev/median of the round medians <= 0.20 and collection wall >= 1 s.
 * Pooled raw p99 < 5 ms stays a separate behavior gate, never a trust substitute.
 */

import { dirname, resolve } from "node:path";
import { mkdirSync, writeFileSync } from "node:fs";
import {
	aggregateKeypressRounds,
	distribution,
	exitCodeForFailure,
	runKeypressBenchmark,
	runKeypressRound,
	KEYPRESS_MEASURED_ROUNDS,
	type Distribution,
	type KeypressBenchmarkResult,
	type KeypressRoundRecord,
} from "./verification/performance.ts";
import { NOISE_RELATIVE_SPREAD_LIMIT, NOISE_EXIT_CODE } from "./statistics.ts";

interface RunnerArgs {
	readonly binary?: string;
	readonly baseline?: string;
	readonly design?: string;
	readonly rounds: number;
	readonly pairs: number;
	readonly output: string;
}

function usage(): never {
	console.error(
		"usage: bench-keypress-dispatch.ts (--binary PATH [--rounds N] | --baseline PATH --design PATH [--pairs N]) --output PATH",
	);
	process.exit(2);
}

function parseArgs(argv: readonly string[]): RunnerArgs {
	const args: {
		binary?: string;
		baseline?: string;
		design?: string;
		rounds?: number;
		pairs?: number;
		output?: string;
	} = {};
	for (let index = 0; index < argv.length; index += 1) {
		const flag = argv[index];
		const value = argv[index + 1];
		if (value === undefined) usage();
		switch (flag) {
			case "--binary": args.binary = value; break;
			case "--baseline": args.baseline = value; break;
			case "--design": args.design = value; break;
			case "--rounds": args.rounds = Number(value); break;
			case "--pairs": args.pairs = Number(value); break;
			case "--output": args.output = value; break;
			default: usage();
		}
		index += 1;
	}
	if (!args.output) usage();
	if (args.binary && (args.baseline || args.design)) usage();
	if (!args.binary && !(args.baseline && args.design)) usage();
	if (args.rounds !== undefined && (!Number.isFinite(args.rounds) || args.rounds < 1)) usage();
	if (args.pairs !== undefined && (!Number.isFinite(args.pairs) || args.pairs < 1)) usage();
	return {
		binary: args.binary,
		baseline: args.baseline,
		design: args.design,
		rounds: args.rounds ?? KEYPRESS_MEASURED_ROUNDS,
		pairs: args.pairs ?? 9,
		output: args.output,
	};
}

interface ArmSummary {
	readonly binary: string;
	readonly roundMedians: readonly number[];
	readonly roundSummary: Distribution;
	readonly pooled: Distribution;
	readonly collectionWallMs: number;
}

function trustBlockers(label: string, arm: ArmSummary): readonly string[] {
	const blockers: string[] = [];
	if (arm.roundSummary.relativeSpread === null || arm.roundSummary.relativeSpread > NOISE_RELATIVE_SPREAD_LIMIT) {
		blockers.push(
			`${label}: round-median relative spread ${String(arm.roundSummary.relativeSpread)} > ${NOISE_RELATIVE_SPREAD_LIMIT}`,
		);
	}
	if (arm.collectionWallMs < 1_000) {
		blockers.push(`${label}: collection wall ${arm.collectionWallMs.toFixed(0)} ms < 1000 ms`);
	}
	return blockers;
}

function armFromResult(result: KeypressBenchmarkResult): ArmSummary {
	return {
		binary: result.binary.sha256,
		roundMedians: result.roundMedians,
		roundSummary: result.roundSummary,
		pooled: result.pooled,
		collectionWallMs: result.collectionWallMs,
	};
}

function armFromRounds(binarySha: string, rounds: readonly KeypressRoundRecord[], collectionWallMs: number): ArmSummary {
	const aggregated = aggregateKeypressRounds(rounds);
	return {
		binary: binarySha,
		roundMedians: aggregated.roundMedians,
		roundSummary: aggregated.roundSummary,
		pooled: aggregated.pooled,
		collectionWallMs,
	};
}

async function runSingleArm(args: RunnerArgs): Promise<number> {
	if (!args.binary) usage();
	const started = performance.now();
	const result = await runKeypressBenchmark(args.binary, { rounds: args.rounds });
	const arm = armFromResult(result);
	const trust = trustBlockers(`binary ${args.binary}`, arm);
	const behavior = result.pooled.p99 >= 5 ? [`pooled raw p99 ${result.pooled.p99.toFixed(3)} ms >= 5 ms`] : [];
	const artifact = {
		mode: "single-arm",
		binaryPath: args.binary,
		rounds: args.rounds,
		processWarmups: result.processWarmups,
		trustGates: { roundMedianRelativeSpreadLimit: NOISE_RELATIVE_SPREAD_LIMIT, collectionWallMinimumMs: 1_000 },
		trustBlockers: trust,
		behaviorBlockers: behavior,
		trusted: trust.length === 0,
		behaviorPass: behavior.length === 0,
		arm: { ...arm, roundRecords: result.rounds, scheduling: result.scheduling, invalidFrameCount: result.invalidFrameCount },
	};
	mkdirSync(dirname(resolve(args.output)), { recursive: true });
	writeFileSync(args.output, `${JSON.stringify(artifact, null, 2)}\n`);
	console.error(
		`rounds=${result.rounds.length} samples=${result.pooled.count} ` +
			`median=${arm.pooled.median.toFixed(4)}ms roundMedian=${arm.roundSummary.median.toFixed(4)}ms ` +
			`rs=${((arm.roundSummary.relativeSpread ?? Number.NaN) * 100).toFixed(2)}% ` +
			`p99=${arm.pooled.p99.toFixed(3)}ms wall=${result.collectionWallMs.toFixed(0)}ms ` +
			`trusted=${trust.length === 0}`,
	);
	return trust.length > 0 ? NOISE_EXIT_CODE : behavior.length > 0 ? 1 : 0;
}

async function runPaired(args: RunnerArgs): Promise<number> {
	if (!args.baseline || !args.design) usage();
	const baselineRounds: KeypressRoundRecord[] = [];
	const designRounds: KeypressRoundRecord[] = [];
	// Per-arm collection wall: only the time spent collecting that arm's own
	// rounds counts toward its >= 1 s trust gate; the other arm's wall is
	// excluded so two short arms cannot pass on their combined duration.
	let baselineWallMs = 0;
	let designWallMs = 0;
	for (let pair = 0; pair < args.pairs; pair += 1) {
		// Alternate arm order every pair so position effects cannot alias with arm.
		const baselineFirst = pair % 2 === 0;
		const first = baselineFirst ? args.baseline : args.design;
		const second = baselineFirst ? args.design : args.baseline;
		const roundStart = performance.now();
		const firstRound = await runKeypressRound(first, pair);
		const firstWall = performance.now() - roundStart;
		const secondStart = performance.now();
		const secondRound = await runKeypressRound(second, pair + 1000);
		const secondWall = performance.now() - secondStart;
		if (baselineFirst) {
			baselineWallMs += firstWall;
			designWallMs += secondWall;
			baselineRounds.push(firstRound);
			designRounds.push(secondRound);
		} else {
			designWallMs += firstWall;
			baselineWallMs += secondWall;
			designRounds.push(firstRound);
			baselineRounds.push(secondRound);
		}
	}
	const baselineArm = armFromRounds(args.baseline, baselineRounds, baselineWallMs);
	const designArm = armFromRounds(args.design, designRounds, designWallMs);
	const pairedSpeedups = baselineRounds.map((baselineRound, index) => {
		const designRound = designRounds[index];
		if (!designRound || designRound.medianMs <= 0) {
			throw new Error(`paired round ${index} has non-positive design median`);
		}
		return baselineRound.medianMs / designRound.medianMs;
	});
	const medianPairedSpeedup = distribution(pairedSpeedups).median;
	const trust = [...trustBlockers("baseline", baselineArm), ...trustBlockers("design", designArm)];
	const artifact = {
		mode: "paired",
		baselinePath: args.baseline,
		designPath: args.design,
		pairs: args.pairs,
		trustGates: { roundMedianRelativeSpreadLimit: NOISE_RELATIVE_SPREAD_LIMIT, collectionWallMinimumMs: 1_000 },
		trustBlockers: trust,
		trusted: trust.length === 0,
		baselineCollectionWallMs: baselineWallMs,
		designCollectionWallMs: designWallMs,
		baseline: { ...baselineArm, roundRecords: baselineRounds },
		design: { ...designArm, roundRecords: designRounds },
		pairedSpeedups,
		medianPairedSpeedup,
	};
	writeFileSync(args.output, `${JSON.stringify(artifact, null, 2)}\n`);
	console.error(
		`pairs=${args.pairs} baselineRoundMedian=${baselineArm.roundSummary.median.toFixed(4)}ms ` +
			`designRoundMedian=${designArm.roundSummary.median.toFixed(4)}ms ` +
			`medianPairedSpeedup=${medianPairedSpeedup.toFixed(4)}x ` +
			`baselineRs=${((baselineArm.roundSummary.relativeSpread ?? Number.NaN) * 100).toFixed(2)}% ` +
			`designRs=${((designArm.roundSummary.relativeSpread ?? Number.NaN) * 100).toFixed(2)}% ` +
			`trusted=${trust.length === 0}`,
	);
	return trust.length > 0 ? NOISE_EXIT_CODE : 0;
}

try {
	const args = parseArgs(process.argv.slice(2));
	process.exitCode = args.binary ? await runSingleArm(args) : await runPaired(args);
} catch (error) {
	console.error(error instanceof Error ? error.stack ?? error.message : String(error));
	process.exitCode = exitCodeForFailure(error);
}
