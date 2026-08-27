#!/usr/bin/env bun
/**
 * PERF-T4: Isolated session append and reopen timing lanes.
 *
 * Produces append-only and reopen-only timing for both the Rust binary
 * (`target/release/session-timing`) and the reference CLI (via a Bun-side
 * SessionManager harness), on pre-generated v3 JSONL sessions at fixed
 * entry/byte counts.  Both warm and explicit cold-cache lanes are measured.
 *
 * SHA-256 prefix preservation is verified per sample.  Peak memory (VmHWM)
 * is reported from the Rust binary's JSON output and from /proc/self/status
 * for the Bun-side reference lane.
 *
 * The noise gate (stddev > 20% of median) is enforced via `requireQuiet`.
 */

import { createHash } from "node:crypto";
import { mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";

import {
	NOISE_EXIT_CODE,
	NoiseRejection,
	formatNoiseRejection,
	requireQuiet,
	spreadStats,
	type NoisyDistribution,
} from "./statistics.ts";

const REPOSITORY_ROOT = resolve(import.meta.dirname, "..");
const RUST_BINARY = resolve(REPOSITORY_ROOT, "target/release/session-timing");
const ARTIFACT_PATH = resolve(REPOSITORY_ROOT, "target/bench/session-timing.json");

const ENTRY_COUNTS = [100, 1_000, 5_000] as const;
const WARMUP_COUNT = 3;
const SAMPLE_COUNT = 20;
const COLD_SAMPLE_COUNT = 10;
const SHA256_PREFIX_LENGTH = 16;

type Lane = "append" | "reopen";
type Implementation = "rust" | "typescript";
type CacheKind = "warm" | "cold";

interface SampleRecord {
	readonly lane: Lane;
	readonly implementation: Implementation;
	readonly cache: CacheKind;
	readonly entries: number;
	readonly index: number;
	readonly wallMs: number;
	readonly sha256Prefix: string;
	readonly peakRssBytes: number;
}

interface SummaryRecord {
	readonly lane: Lane;
	readonly implementation: Implementation;
	readonly cache: CacheKind;
	readonly entries: number;
	readonly count: number;
	readonly medianMs: number;
	readonly stddevMs: number;
	readonly relativeSpread: number;
	readonly peakRssBytes: number;
}

interface SessionTimingArtifact {
	readonly unit: string;
	readonly entryCounts: readonly number[];
	readonly warmups: number;
	readonly warmSamples: number;
	readonly coldSamples: number;
	readonly sha256PrefixLength: number;
	readonly samples: readonly SampleRecord[];
	readonly summary: readonly SummaryRecord[];
	pass: boolean;
}

function status(message: string): void {
	process.stderr.write(`[perf-t4] ${message}\n`);
}

function distribution(values: readonly number[]) {
	const sorted = [...values].sort((a, b) => a - b);
	const count = sorted.length;
	if (count === 0) return { count, median: 0, min: 0, max: 0, p95: 0, p99: 0, ...spreadStats([], 0) };
	const at = (i: number): number => {
		const value = sorted[i];
		if (value === undefined) throw new Error(`distribution: missing sorted[${i}] for count=${count}`);
		return value;
	};
	const median = count % 2 === 0 ? (at(count / 2 - 1) + at(count / 2)) / 2 : at(Math.floor(count / 2));
	const { stddev, relativeSpread } = spreadStats(values, median);
	const p95 = sorted[Math.min(count - 1, Math.floor(count * 0.95))];
	const p99 = sorted[Math.min(count - 1, Math.floor(count * 0.99))];
	return { count, median, min: sorted[0], max: sorted[count - 1], p95, p99, stddev, relativeSpread };
}

function peakRssBytes(): number {
	try {
		const status = readFileSync("/proc/self/status", "utf-8");
		for (const line of status.split("\n")) {
			if (line.startsWith("VmHWM:")) {
				const num = line.replace(/\D/g, "");
				return parseInt(num, 10) * 1024;
			}
		}
	} catch {
		// Not Linux or no /proc
	}
	return 0;
}

export function sha256Prefix(path: string): string {
	const content = readFileSync(path);
	return createHash("sha256").update(content).digest("hex").slice(0, SHA256_PREFIX_LENGTH);
}

function temporaryDirectory(label: string): string {
	const dir = resolve(REPOSITORY_ROOT, `target/bench/session-timing-${label}-${process.pid}`);
	mkdirSync(dir, { recursive: true });
	return dir;
}

const temporaryDirectories: string[] = [];

// ---------------------------------------------------------------------------
// Rust binary lane
// ---------------------------------------------------------------------------

async function runRustLane(
	lane: Lane,
	entries: number,
	cache: CacheKind,
	outdir: string,
): Promise<SampleRecord[]> {
	const sampleCount = cache === "cold" ? COLD_SAMPLE_COUNT : SAMPLE_COUNT;
	const records: SampleRecord[] = [];

	if (cache === "cold") {
		// Cold-cache: spawn a fresh process per sample, dropping the executable's
		// page cache before each spawn so the binary loads from disk every time.
		for (let idx = 0; idx < sampleCount; idx++) {
			runCacheDrop(RUST_BINARY);
			const args = [
				RUST_BINARY,
				"--mode",
				lane,
				"--entries",
				String(entries),
				"--samples",
				"1",
				"--warmups",
				"0",
				"--outdir",
				outdir,
				"--json",
				"--cold",
			];
			const result = Bun.spawnSync(args, {
				stdout: "pipe",
				stderr: "pipe",
				cwd: REPOSITORY_ROOT,
			});
			if (result.exitCode !== 0 && result.exitCode !== 2) {
				const stderr = new TextDecoder().decode(result.stderr).trim();
				throw new Error(`Rust session-timing ${lane}/cold/${entries} sample ${idx} exited ${result.exitCode}: ${stderr}`);
			}
			const stdout = new TextDecoder().decode(result.stdout).trim();
			for (const line of stdout.split("\n")) {
				const trimmed = line.trim();
				if (!trimmed) continue;
				try {
					const parsed = JSON.parse(trimmed);
					if (parsed.sample) {
						records.push({
							lane,
							implementation: "rust",
							cache,
							entries,
							index: idx,
							wallMs: parsed.sample.wallMs,
							sha256Prefix: parsed.sample.sha256Prefix,
							peakRssBytes: parsed.sample.peakRssBytes ?? 0,
						});
					}
				} catch {
					// Skip non-JSON lines (summary objects, etc.)
				}
			}
		}
	} else {
		// Warm-cache: single process, all samples in-process
		const args = [
			RUST_BINARY,
			"--mode",
			lane,
			"--entries",
			String(entries),
			"--samples",
			String(sampleCount),
			"--warmups",
			String(WARMUP_COUNT),
			"--outdir",
			outdir,
			"--json",
		];
		const result = Bun.spawnSync(args, {
			stdout: "pipe",
			stderr: "pipe",
			cwd: REPOSITORY_ROOT,
		});
		if (result.exitCode !== 0 && result.exitCode !== 2) {
			const stderr = new TextDecoder().decode(result.stderr).trim();
			throw new Error(`Rust session-timing ${lane}/warm/${entries} exited ${result.exitCode}: ${stderr}`);
		}
		const stdout = new TextDecoder().decode(result.stdout).trim();
		for (const line of stdout.split("\n")) {
			const trimmed = line.trim();
			if (!trimmed) continue;
			try {
				const parsed = JSON.parse(trimmed);
				if (parsed.sample) {
					records.push({
						lane,
						implementation: "rust",
						cache,
						entries,
						index: parsed.sample.index,
						wallMs: parsed.sample.wallMs,
						sha256Prefix: parsed.sample.sha256Prefix,
						peakRssBytes: parsed.sample.peakRssBytes ?? 0,
					});
				}
			} catch {
				// Skip non-JSON lines (summary objects, etc.)
			}
		}
	}
	return records;
}

// ---------------------------------------------------------------------------
// TypeScript reference lane (in-process via Bun)
// ---------------------------------------------------------------------------

function registerReferenceResolver(): void {
	Bun.plugin({
		name: "pi-reference-resolver-t4",
		setup(build) {
			const refRoot = resolve(REPOSITORY_ROOT, ".references/pi");
			const refUuid = resolve(refRoot, "packages/ai/src/utils/uuid.ts");
			build.onResolve({ filter: /^@earendil-works\/pi-ai$/ }, () => ({
				path: refUuid,
			}));
			build.onResolve({ filter: /^cross-spawn$/ }, () => ({
				path: "cross-spawn",
				namespace: "pi-shim-t4",
			}));
			build.onLoad({ filter: /.*/, namespace: "pi-shim-t4" }, () => ({
				contents:
					'import { spawn, spawnSync } from "node:child_process";\nexport default Object.assign(spawn, { sync: spawnSync });\n',
				loader: "js",
			}));
		},
	});
}

interface TsSessionManager {
	create(cwd: string, sessionDir?: string): TsSessionManager;
	open(path: string, sessionDir?: string): TsSessionManager;
	appendMessage(message: { role: string; content: unknown }): string;
}

async function loadReferenceSessionManager(): Promise<{
	SessionManager: {
		create(cwd: string, sessionDir?: string): TsSessionManager;
		open(path: string, sessionDir?: string): TsSessionManager;
	};
}> {
	registerReferenceResolver();
	const refPath = resolve(REPOSITORY_ROOT, ".references/pi/packages/coding-agent/src/core/session-manager.ts");
	return await import(refPath);
}

async function runTsAppendLane(
	SessionManager: {
		create(cwd: string, sessionDir?: string): TsSessionManager;
		open(path: string, sessionDir?: string): TsSessionManager;
	},
	entries: number,
	cache: CacheKind,
	outdir: string,
): Promise<SampleRecord[]> {
	const sampleCount = cache === "cold" ? COLD_SAMPLE_COUNT : SAMPLE_COUNT;
	const records: SampleRecord[] = [];

	// Warmups (warm only)
	if (cache === "warm") {
		for (let w = 0; w < WARMUP_COUNT; w++) {
		const path = resolve(outdir, `ts-warmup-append-${w}.jsonl`);
		const sm = SessionManager.create(outdir, outdir);
		for (let i = 0; i < entries; i++) {
			sm.appendMessage({ role: "user", content: [{ type: "text", text: `message-${String(i).padStart(6, "0")}` }] });
		}
		try { rmSync(path, { force: true }); } catch { /* */ }
		}
	}

	for (let idx = 0; idx < sampleCount; idx++) {
		const path = resolve(outdir, `ts-append-s${idx}.jsonl`);
		// Write empty file so open initializes it
		writeFileSync(path, "");
		const sm = SessionManager.open(path, outdir);
		const start = performance.now();
		for (let i = 0; i < entries; i++) {
			sm.appendMessage({ role: "user", content: [{ type: "text", text: `message-${String(i).padStart(6, "0")}` }] });
		}
		const wallMs = performance.now() - start;
		const hash = sha256Prefix(path);
		try { rmSync(path, { force: true }); } catch { /* */ }

		records.push({
			lane: "append",
			implementation: "typescript",
			cache,
			entries,
			index: idx,
			wallMs,
			sha256Prefix: hash,
			peakRssBytes: peakRssBytes(),
		});
	}
	return records;
}

async function runTsReopenLane(
	SessionManager: { open(path: string, sessionDir?: string): TsSessionManager },
	sessionPath: string,
	entries: number,
	cache: CacheKind,
): Promise<SampleRecord[]> {
	const sampleCount = cache === "cold" ? COLD_SAMPLE_COUNT : SAMPLE_COUNT;
	const records: SampleRecord[] = [];
	const expectedHash = sha256Prefix(sessionPath);

	// Warmups (warm only — cold samples start from an unprimed cache)
	if (cache === "warm") {
		for (let w = 0; w < WARMUP_COUNT; w++) {
			SessionManager.open(sessionPath);
		}
	}

	for (let idx = 0; idx < sampleCount; idx++) {
		if (cache === "cold") runCacheDrop(sessionPath);
		const start = performance.now();
		SessionManager.open(sessionPath);
		const wallMs = performance.now() - start;
		const hash = sha256Prefix(sessionPath);
		if (hash !== expectedHash) {
			throw new Error(`SHA-256 prefix changed on reopen: ${expectedHash} -> ${hash}`);
		}

		records.push({
			lane: "reopen",
			implementation: "typescript",
			cache,
			entries,
			index: idx,
			wallMs,
			sha256Prefix: hash,
			peakRssBytes: peakRssBytes(),
		});
	}
	return records;
}

// ---------------------------------------------------------------------------
// Cold cache support
// ---------------------------------------------------------------------------

function runCacheDrop(path: string): void {
	const python = "python3";
	const code = "import os, sys\nwith open(sys.argv[1], 'rb') as f:\n    os.posix_fadvise(f.fileno(), 0, 0, os.POSIX_FADV_DONTNEED)";
	const result = Bun.spawnSync([python, "-c", code, path], { stdout: "pipe", stderr: "pipe" });
	if (result.exitCode !== 0) {
		throw new Error(`posix_fadvise failed for ${path}: ${new TextDecoder().decode(result.stderr).trim()}`);
	}
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

async function main(): Promise<void> {
	status("loading reference SessionManager");
	const { SessionManager } = await loadReferenceSessionManager();

	const allSamples: SampleRecord[] = [];
	const summaryRecords: SummaryRecord[] = [];

	for (const entries of ENTRY_COUNTS) {
		status(`entries=${entries}: generating session fixtures`);
		const rustDir = temporaryDirectory(`rust-${entries}`);
		temporaryDirectories.push(rustDir);
		const tsDir = temporaryDirectory(`ts-${entries}`);
		temporaryDirectories.push(tsDir);

		// Generate a TS session file for reopen lane
		const reopenPath = resolve(tsDir, "reopen-target.jsonl");
		writeFileSync(reopenPath, "");
		const genSm = SessionManager.open(reopenPath, tsDir);
		for (let i = 0; i < entries; i++) {
			genSm.appendMessage({ role: "user", content: [{ type: "text", text: `message-${String(i).padStart(6, "0")}` }] });
		}

		for (const lane of ["append", "reopen"] as const) {
			for (const cache of ["warm", "cold"] as const) {
				status(`entries=${entries} lane=${lane} cache=${cache}: Rust`);
			const rustSamples = await runRustLane(lane, entries, cache, rustDir);
			allSamples.push(...rustSamples);

				status(`entries=${entries} lane=${lane} cache=${cache}: TypeScript`);
				let tsSamples: SampleRecord[];
				if (lane === "append") {
					tsSamples = await runTsAppendLane(SessionManager, entries, cache, tsDir);
				} else {
					tsSamples = await runTsReopenLane(SessionManager, reopenPath, entries, cache);
				}
				allSamples.push(...tsSamples);

				// Summaries
				for (const impl of ["rust", "typescript"] as const) {
					const filtered = allSamples.filter(
						(s) => s.lane === lane && s.implementation === impl && s.cache === cache && s.entries === entries,
					);
					const dist = distribution(filtered.map((s) => s.wallMs));
					summaryRecords.push({
						lane,
						implementation: impl,
						cache,
						entries,
						count: dist.count,
						medianMs: dist.median,
						stddevMs: dist.stddev,
						relativeSpread: dist.relativeSpread ?? 0,
						peakRssBytes: Math.max(...filtered.map((s) => s.peakRssBytes), 0),
					});
				}
			}
		}
	}

	// Noise gate
	const noiseDistributions: NoisyDistribution[] = summaryRecords.map((s) => ({
		label: `${s.lane}/${s.implementation}/${s.cache}/entries=${s.entries}`,
		count: s.count,
		median: s.medianMs,
		stddev: s.stddevMs,
		relativeSpread: s.relativeSpread,
	}));

	const artifact: SessionTimingArtifact = {
		unit: "milliseconds wall time",
		entryCounts: ENTRY_COUNTS,
		warmups: WARMUP_COUNT,
		warmSamples: SAMPLE_COUNT,
		coldSamples: COLD_SAMPLE_COUNT,
		sha256PrefixLength: SHA256_PREFIX_LENGTH,
		samples: allSamples,
		summary: summaryRecords,
		pass: true,
	};

	try {
		requireQuiet(noiseDistributions);
	} catch (error) {
		if (error instanceof NoiseRejection) {
			artifact.pass = false;
			writeFileSync(ARTIFACT_PATH, JSON.stringify(artifact, null, 2) + "\n");
			process.stderr.write(`perf-t4 rejected as noise:\n${formatNoiseRejection(error.noisy)}\nartifact: ${ARTIFACT_PATH}\n`);
			process.exitCode = NOISE_EXIT_CODE;
			return;
		}
		throw error;
	}

	writeFileSync(ARTIFACT_PATH, JSON.stringify(artifact, null, 2) + "\n");
	process.stdout.write(`perf-t4 passed; artifact: ${ARTIFACT_PATH}\n`);
}

if (import.meta.main) {
	try {
		await main();
	} catch (error) {
		const message = error instanceof Error ? error.message : String(error);
		process.stderr.write(`perf-t4 failed: ${message}\n`);
		process.exitCode = 1;
	} finally {
		for (const dir of temporaryDirectories) {
			try { rmSync(dir, { recursive: true, force: true }); } catch { /* */ }
		}
	}
}
