import { createHash } from "node:crypto";
import {
	existsSync,
	mkdirSync,
	mkdtempSync,
	readFileSync,
	readdirSync,
	rmSync,
	statSync,
	writeFileSync,
} from "node:fs";
import { arch, platform, release } from "node:os";
import { dirname, join, relative, resolve } from "node:path";
import { PTY_KEYS, type PtyProcess, type PtySnapshot, spawnPty } from "./pty.ts";

const REPOSITORY_ROOT = resolve(import.meta.dirname, "../..");
const ARTIFACT_PATH = resolve(REPOSITORY_ROOT, "target/bench/performance-comparison.json");
const RUST_BINARY = resolve(REPOSITORY_ROOT, "target/release/pi");
const TYPESCRIPT_BINARY = resolve(REPOSITORY_ROOT, ".references/pi/packages/coding-agent/dist/pi");
const HOST_BUILD_ROOT = resolve(REPOSITORY_ROOT, "target/bench/performance-extension-host");
const EXTENSION_HOST = resolve(
	HOST_BUILD_ROOT,
	".staging-host/host/x86_64-unknown-linux-gnu/pi-extension-host",
);
const VERIFICATION_EXTENSION = resolve(import.meta.dirname, "extension.ts");

const RUST_SOURCE_ROOTS = [
	"Cargo.toml",
	"Cargo.lock",
	"rust-toolchain.toml",
	"rustfmt.toml",
	"deny.toml",
	"crates/pi-ai",
	"crates/pi-agent",
	"crates/pi-ext",
	"crates/pi-tui",
	"crates/pi",
	"package.json",
	"bun.lock",
	"packages/extension-host",
	"scripts/build-extension-host.ts",
	"scripts/release",
] as const;
const TYPESCRIPT_SOURCE_ROOTS = [
	".references/pi/package.json",
	".references/pi/package-lock.json",
	".references/pi/packages/ai",
	".references/pi/packages/agent",
	".references/pi/packages/tui",
	".references/pi/packages/coding-agent",
] as const;
const SOURCE_IGNORED_DIRECTORIES: Record<string, true> = {
	".git": true,
	coverage: true,
	dist: true,
	node_modules: true,
};

const SYNC_BEGIN = "\x1b[?2026h";
const SYNC_END = "\x1b[?2026l";
const PTY_TERM = "xterm-256color";
const VERSION_COLD_SAMPLES = 20;
const VERSION_WARMUPS = 10;
const VERSION_WARM_SAMPLES = 50;
const FIRST_FRAME_COLD_SAMPLES = 20;
const FIRST_FRAME_WARMUPS = 5;
const FIRST_FRAME_WARM_SAMPLES = 30;
const STREAM_PROCESS_WARMUPS = 3;
const STREAM_PROCESS_SAMPLES = 20;
const STREAM_CHUNKS = 256;
const STREAM_CHUNK_DELAY_MS = 2;
const KEY_WARMUPS = 20;
const KEY_SAMPLES = 200;
const PROC_SAMPLE_INTERVAL_MS = 1;
const VERSION_SPEEDUP_TARGET = 3;
const FIRST_FRAME_SPEEDUP_TARGET = 3;
const STREAM_CPU_SPEEDUP_TARGET = 2;
const KEYPRESS_P99_TARGET_MS = 5;

const implementationNames = ["rust", "typescript"] as const;
type Implementation = (typeof implementationNames)[number];
type SampleKind = "cold" | "warm";

interface Distribution {
	readonly count: number;
	readonly median: number;
	readonly p95: number;
	readonly p99: number;
	readonly min: number;
	readonly max: number;
}

interface ProcCpuSnapshot {
	readonly maxOwnTicks: ReadonlyMap<string, number>;
	readonly procSamples: number;
	readonly observedProcesses: number;
}

interface ProcessObservation {
	readonly pid: number;
	readonly startTime: string;
	readonly ownTicks: number;
	readonly children: readonly number[];
}

interface VersionSample {
	readonly kind: SampleKind;
	readonly wallMs: number;
	readonly processTreeCpuMs: number;
	readonly procSamples: number;
	readonly observedProcesses: number;
	readonly ptyRootPid: number;
	readonly output: string;
}

interface FirstFrameSample {
	readonly kind: SampleKind;
	readonly wallMs: number;
	readonly processTreeCpuMs: number;
	readonly procSamplesAtFrame: number;
	readonly observedProcessesAtFrame: number;
	readonly ptyRootPid: number;
	readonly frameBytes: number;
	readonly detection: "synchronized-output" | "row-local-fallback";
}

interface StreamTurnSample {
	readonly sampleId: string;
	readonly processTreeCpuMs: number;
	readonly cpuMsPerProviderFrame: number;
	readonly streamWallMs: number;
	readonly providerFrameCount: number;
	readonly paintedSynchronizedFrames: number;
	readonly firstObservedChunk: number;
	readonly highestFullChunkTokenInPty: number;
	readonly assistantPaintBeforeFinal: boolean;
	readonly firstAssistantPaintElapsedMs: number;
	readonly rawStreamOutput: string;
	readonly rawStreamSha256: string;
	readonly procSamplesBefore: number;
	readonly procSamplesAfter: number;
	readonly observedProcesses: number;
	readonly ptyRootPid: number;
	readonly persistedProviderFrames: number;
	readonly sessionJsonlFiles: readonly string[];
	readonly sessionSha256: string;
}

type StreamTurnMeasurement = Omit<
	StreamTurnSample,
	"persistedProviderFrames" | "sessionJsonlFiles" | "sessionSha256"
>;

interface KeypressSample {
	readonly latencyMs: number;
	readonly synchronizedFramesObserved: number;
}

interface CommandRecord {
	readonly label: string;
	readonly cwd: string;
	readonly argv: readonly string[];
}

interface FileRecord {
	readonly path: string;
	readonly sha256: string;
	readonly bytes: number;
}

interface SourceFingerprint {
	readonly roots: readonly string[];
	readonly files: number;
	readonly sha256: string;
}

interface ImplementationMeasurements<T> {
	readonly rust: readonly T[];
	readonly typescript: readonly T[];
}

interface PerformanceArtifact {
	check: 9;
	generatedAt: string;
	pass: boolean;
	blockers: string[];
	machine: Record<string, string | readonly string[]>;
	build: {
		commands: CommandRecord[];
		artifacts?: Record<string, FileRecord>;
		sourceFingerprints?: {
			before: { rust: SourceFingerprint; typescript: SourceFingerprint };
			after?: { rust: SourceFingerprint; typescript: SourceFingerprint };
			stable?: boolean;
		};
	};
	harness: Record<string, string | number | boolean | readonly string[] | Record<string, number>>;
	measurements: Record<string, object>;
	failure?: {
		stage: string;
		message: string;
	};
}

class HarnessFailure extends Error {
	constructor(
		readonly stage: string,
		message: string,
	) {
		super(message);
		this.name = "HarnessFailure";
	}
}

class ThresholdFailure extends Error {
	constructor(readonly failures: readonly string[]) {
		super(failures.join("\n"));
		this.name = "ThresholdFailure";
	}
}

const temporaryDirectories: string[] = [];
const buildCommands: CommandRecord[] = [];
const artifact: PerformanceArtifact = {
	check: 9,
	generatedAt: new Date().toISOString(),
	pass: false,
	blockers: [],
	machine: {},
	build: { commands: buildCommands },
	harness: {},
	measurements: {},
};

function status(message: string): void {
	process.stderr.write(`[check 9] ${message}\n`);
}

function errorMessage(error: Error | string): string {
	return typeof error === "string" ? error : error.message;
}

function requiredExecutable(name: string): string {
	const path = Bun.which(name);
	if (!path) throw new HarnessFailure("prerequisite", `required executable not found on PATH: ${name}`);
	return path;
}

function temporaryDirectory(label: string): string {
	const path = mkdtempSync(join(Bun.env.TMPDIR ?? "/tmp", `pi-check9-${label}-`));
	temporaryDirectories.push(path);
	return path;
}

function tail(text: string, maximum = 12_000): string {
	return text.length <= maximum ? text : text.slice(-maximum);
}

async function runCheckedCommand(record: CommandRecord): Promise<void> {
	buildCommands.push(record);
	status(`running ${record.label}`);
	const child = Bun.spawn([...record.argv], {
		cwd: record.cwd,
		env: process.env,
		stdin: "ignore",
		stdout: "pipe",
		stderr: "pipe",
	});
	const [stdout, stderr, exitCode] = await Promise.all([
		new Response(child.stdout).text(),
		new Response(child.stderr).text(),
		child.exited,
	]);
	if (exitCode !== 0) {
		throw new HarnessFailure(
			`build:${record.label}`,
			`${record.label} exited ${exitCode}\nstdout:\n${tail(stdout)}\nstderr:\n${tail(stderr)}`,
		);
	}
}

function fileRecord(path: string): FileRecord {
	if (!existsSync(path)) throw new HarnessFailure("build-artifact", `expected build artifact is missing: ${path}`);
	const bytes = readFileSync(path);
	return {
		path,
		sha256: createHash("sha256").update(bytes).digest("hex"),
		bytes: statSync(path).size,
	};
}

function sourceFingerprint(roots: readonly string[]): SourceFingerprint {
	const files: string[] = [];
	const visit = (path: string): void => {
		const metadata = statSync(path);
		if (metadata.isDirectory()) {
			const entries = readdirSync(path, { withFileTypes: true }).sort((left, right) =>
				left.name.localeCompare(right.name),
			);
			for (const entry of entries) {
				if (entry.isDirectory() && SOURCE_IGNORED_DIRECTORIES[entry.name] === true) continue;
				if (entry.isFile() && /^pi-session-.*\.html$/.test(entry.name)) continue;
				visit(join(path, entry.name));
			}
		} else if (metadata.isFile()) {
			files.push(path);
		}
	};
	for (const root of roots) visit(resolve(REPOSITORY_ROOT, root));
	files.sort();
	const hash = createHash("sha256");
	for (const path of files) {
		const name = relative(REPOSITORY_ROOT, path);
		const bytes = readFileSync(path);
		hash.update(`${name.length}:${name}:${bytes.byteLength}:`);
		hash.update(bytes);
	}
	return { roots, files: files.length, sha256: hash.digest("hex") };
}

function readOptional(path: string): string | undefined {
	try {
		return readFileSync(path, "utf8").trim();
	} catch (error) {
		if (
			error instanceof Error &&
			"code" in error &&
			(error.code === "ENOENT" || error.code === "EACCES" || error.code === "ESRCH")
		) {
			return undefined;
		}
		throw error;
	}
}

function cpuModel(): string {
	const cpuInfo = readFileSync("/proc/cpuinfo", "utf8");
	const line = cpuInfo.split("\n").find((candidate) => candidate.startsWith("model name"));
	return line?.split(":").slice(1).join(":").trim() || "unknown";
}

function cpuGovernors(): readonly string[] {
	const root = "/sys/devices/system/cpu";
	const governors = new Set<string>();
	try {
		for (const entry of readdirSync(root)) {
			if (!/^cpu\d+$/.test(entry)) continue;
			const governor = readOptional(join(root, entry, "cpufreq/scaling_governor"));
			if (governor) governors.add(governor);
		}
	} catch (error) {
		if (!(error instanceof Error && "code" in error && error.code === "ENOENT")) throw error;
	}
	return governors.size > 0 ? [...governors].sort() : ["unavailable"];
}

function machineMetadata(): Record<string, string | readonly string[]> {
	if (platform() !== "linux" || arch() !== "x64") {
		throw new HarnessFailure(
			"host-validation",
			`check 9 requires Linux x86_64 /proc sampling, found ${platform()} ${arch()}`,
		);
	}
	return {
		os: platform(),
		arch: arch(),
		cpuModel: cpuModel(),
		kernel: release(),
		kernelBuild: readOptional("/proc/version") ?? "unknown",
		governor: cpuGovernors(),
		terminal: process.env.TERM_PROGRAM ?? process.env.TERM ?? "unknown",
		term: process.env.TERM ?? "unset",
		termProgram: process.env.TERM_PROGRAM ?? "unset",
		colorTerm: process.env.COLORTERM ?? "unset",
	};
}

function clockTicksPerSecond(): number {
	const getconf = requiredExecutable("getconf");
	const result = Bun.spawnSync([getconf, "CLK_TCK"], { stdout: "pipe", stderr: "pipe" });
	if (result.exitCode !== 0) {
		throw new HarnessFailure(
			"host-validation",
			`getconf CLK_TCK exited ${result.exitCode}: ${new TextDecoder().decode(result.stderr).trim()}`,
		);
	}
	const value = Number.parseInt(new TextDecoder().decode(result.stdout).trim(), 10);
	if (!Number.isSafeInteger(value) || value <= 0) {
		throw new HarnessFailure("host-validation", `getconf CLK_TCK returned invalid value: ${value}`);
	}
	return value;
}

function parseProcStat(pid: number): Omit<ProcessObservation, "children"> | undefined {
	const raw = readOptional(`/proc/${pid}/stat`);
	if (!raw) return undefined;
	const close = raw.lastIndexOf(")");
	if (close < 0) return undefined;
	const fields = raw.slice(close + 2).split(" ");
	const userTicks = Number.parseInt(fields[11] ?? "", 10);
	const systemTicks = Number.parseInt(fields[12] ?? "", 10);
	const startTime = fields[19];
	if (!Number.isSafeInteger(userTicks) || !Number.isSafeInteger(systemTicks) || !startTime) return undefined;
	return { pid, startTime, ownTicks: userTicks + systemTicks };
}

function processChildren(pid: number): readonly number[] {
	const result = new Set<number>();
	let tasks: string[];
	try {
		tasks = readdirSync(`/proc/${pid}/task`);
	} catch (error) {
		if (
			error instanceof Error &&
			"code" in error &&
			(error.code === "ENOENT" || error.code === "EACCES" || error.code === "ESRCH")
		)
			return [];
		throw error;
	}
	for (const task of tasks) {
		if (!/^\d+$/.test(task)) continue;
		const raw = readOptional(`/proc/${pid}/task/${task}/children`);
		if (!raw) continue;
		for (const child of raw.split(/\s+/)) {
			if (!child) continue;
			const value = Number.parseInt(child, 10);
			if (Number.isSafeInteger(value) && value > 0) result.add(value);
		}
	}
	return [...result];
}

function observeProcessTree(rootPid: number): readonly ProcessObservation[] {
	const pending = [rootPid];
	const visited = new Set<number>();
	const result: ProcessObservation[] = [];
	while (pending.length > 0) {
		const pid = pending.pop();
		if (pid === undefined || visited.has(pid)) continue;
		visited.add(pid);
		const stat = parseProcStat(pid);
		if (!stat) continue;
		const children = processChildren(pid);
		result.push({ ...stat, children });
		for (const child of children) pending.push(child);
	}
	return result;
}

class ProcTreeSampler {
	readonly #maximumOwnTicks = new Map<string, number>();
	readonly #observedIdentities = new Set<string>();
	#procSamples = 0;
	#running = true;
	readonly #completed: Promise<void>;

	constructor(
		readonly rootPid: number,
		readonly intervalMs: number,
	) {
		this.#sample();
		this.#completed = this.#sampleLoop();
	}

	snapshot(): ProcCpuSnapshot {
		this.#sample();
		return {
			maxOwnTicks: new Map(this.#maximumOwnTicks),
			procSamples: this.#procSamples,
			observedProcesses: this.#observedIdentities.size,
		};
	}

	async stop(): Promise<ProcCpuSnapshot> {
		this.#running = false;
		await this.#completed;
		return this.snapshot();
	}

	async #sampleLoop(): Promise<void> {
		while (this.#running) {
			await Bun.sleep(this.intervalMs);
			if (this.#running) this.#sample();
		}
	}

	#sample(): void {
		this.#procSamples += 1;
		for (const process of observeProcessTree(this.rootPid)) {
			const identity = `${process.pid}:${process.startTime}`;
			this.#observedIdentities.add(identity);
			const previous = this.#maximumOwnTicks.get(identity) ?? 0;
			if (process.ownTicks > previous) this.#maximumOwnTicks.set(identity, process.ownTicks);
		}
	}
}

function totalTicks(snapshot: ProcCpuSnapshot): number {
	let ticks = 0;
	for (const value of snapshot.maxOwnTicks.values()) ticks += value;
	return ticks;
}

function cpuMillisecondsBetween(before: ProcCpuSnapshot, after: ProcCpuSnapshot, ticksPerSecond: number): number {
	let delta = 0;
	for (const [identity, afterTicks] of after.maxOwnTicks) {
		delta += Math.max(0, afterTicks - (before.maxOwnTicks.get(identity) ?? 0));
	}
	return (delta * 1_000) / ticksPerSecond;
}

function cpuMilliseconds(snapshot: ProcCpuSnapshot, ticksPerSecond: number): number {
	return (totalTicks(snapshot) * 1_000) / ticksPerSecond;
}

function stripTerminalSequences(text: string): string {
	return text
		.replace(/\x1b\][^\x07]*(?:\x07|\x1b\\)/g, "")
		.replace(/\x1bP[\s\S]*?\x1b\\/g, "")
		.replace(/\x1b\[[0-?]*[ -/]*[@-~]/g, "")
		.replace(/\x1b[@-_]/g, "")
		.replace(/[\x00-\x1f\x7f]/g, "");
}

interface FrameObservation {
	readonly elapsedMs: number;
	readonly bytes: number;
	readonly detection: FirstFrameSample["detection"];
}

function frameObservation(snapshot: PtySnapshot, chunkOffset = 0): FrameObservation | undefined {
	let raw = "";
	let bytes = 0;
	for (const chunk of snapshot.chunks.slice(chunkOffset)) {
		if (chunk.stream !== "pty") continue;
		raw += chunk.text;
		bytes += chunk.bytes.byteLength;
		const begin = raw.indexOf(SYNC_BEGIN);
		if (begin >= 0) {
			if (raw.indexOf(SYNC_END, begin + SYNC_BEGIN.length) >= 0) {
				return { elapsedMs: chunk.elapsedMs, bytes, detection: "synchronized-output" };
			}
			continue;
		}
		if (/\x1b\[[0-?]*[ -/]*[@-~]/.test(raw) && stripTerminalSequences(raw).trim().length > 0) {
			return { elapsedMs: chunk.elapsedMs, bytes, detection: "row-local-fallback" };
		}
	}
	return undefined;
}

function countOccurrences(text: string, needle: string): number {
	let count = 0;
	let offset = 0;
	for (;;) {
		const found = text.indexOf(needle, offset);
		if (found < 0) return count;
		count += 1;
		offset = found + needle.length;
	}
}

function maximumChunkIndex(text: string): number {
	let maximum = 0;
	for (const match of text.matchAll(/verification-chunk-(\d{4})/g)) {
		const value = Number.parseInt(match[1] ?? "", 10);
		if (Number.isSafeInteger(value) && value > maximum) maximum = value;
	}
	return maximum;
}

interface AssistantPaintObservation {
	readonly highestChunkIndex: number;
	readonly elapsedMs: number;
	readonly beforeFinal: boolean;
}

function firstAssistantPaint(
	snapshot: PtySnapshot,
	chunkOffset: number,
	finalMarker: string,
): AssistantPaintObservation | undefined {
	let raw = "";
	let scanOffset = 0;
	for (const chunk of snapshot.chunks.slice(chunkOffset)) {
		if (chunk.stream !== "pty") continue;
		raw += chunk.text;
		for (;;) {
			const begin = raw.indexOf(SYNC_BEGIN, scanOffset);
			if (begin < 0) break;
			const end = raw.indexOf(SYNC_END, begin + SYNC_BEGIN.length);
			if (end < 0) break;
			const frame = raw.slice(begin, end + SYNC_END.length);
			const highestChunkIndex = maximumChunkIndex(frame);
			if (highestChunkIndex > 0) {
				return {
					highestChunkIndex,
					elapsedMs: chunk.elapsedMs,
					beforeFinal: !frame.includes(finalMarker),
				};
			}
			scanOffset = end + SYNC_END.length;
		}
	}
	return undefined;
}

interface StreamingSessionEvidence {
	readonly persistedProviderFrames: number;
	readonly sessionJsonlFiles: readonly string[];
	readonly sessionSha256: string;
}

function streamingSessionEvidence(
	sessionDirectory: string,
	finalMarker: string,
): StreamingSessionEvidence {
	const paths: string[] = [];
	const visit = (path: string): void => {
		const metadata = statSync(path);
		if (metadata.isDirectory()) {
			for (const entry of readdirSync(path, { withFileTypes: true })) {
				visit(join(path, entry.name));
			}
		} else if (metadata.isFile() && path.endsWith(".jsonl")) {
			paths.push(path);
		}
	};
	visit(sessionDirectory);
	paths.sort();
	if (paths.length === 0) throw new HarnessFailure("stream-session", "streaming run wrote no session JSONL");

	const providerFrames = new Set<number>();
	const hash = createHash("sha256");
	let foundFinalMarker = false;
	for (const path of paths) {
		const name = relative(sessionDirectory, path);
		const bytes = readFileSync(path);
		const text = bytes.toString("utf8");
		hash.update(`${name.length}:${name}:${bytes.byteLength}:`);
		hash.update(bytes);
		foundFinalMarker ||= text.includes(finalMarker);
		for (const match of text.matchAll(/verification-chunk-(\d{4})/g)) {
			const index = Number.parseInt(match[1] ?? "", 10);
			if (index >= 1 && index <= STREAM_CHUNKS) providerFrames.add(index);
		}
	}
	if (!foundFinalMarker) {
		throw new HarnessFailure("stream-session", `persisted assistant response omitted ${finalMarker}`);
	}
	return {
		persistedProviderFrames: providerFrames.size,
		sessionJsonlFiles: paths.map((path) => relative(sessionDirectory, path)),
		sessionSha256: hash.digest("hex"),
	};
}

function distribution(values: readonly number[]): Distribution {
	if (values.length === 0 || values.some((value) => !Number.isFinite(value) || value < 0)) {
		throw new HarnessFailure("statistics", "distribution requires finite non-negative samples");
	}
	const sorted = [...values].sort((left, right) => left - right);
	const quantile = (probability: number): number => {
		if (sorted.length === 1) return sorted[0] ?? 0;
		const position = (sorted.length - 1) * probability;
		const lower = Math.floor(position);
		const upper = Math.ceil(position);
		const lowerValue = sorted[lower];
		const upperValue = sorted[upper];
		if (lowerValue === undefined || upperValue === undefined) throw new HarnessFailure("statistics", "quantile index escaped sample range");
		return lowerValue + (upperValue - lowerValue) * (position - lower);
	};
	return {
		count: sorted.length,
		median: quantile(0.5),
		p95: quantile(0.95),
		p99: quantile(0.99),
		min: sorted[0] ?? 0,
		max: sorted.at(-1) ?? 0,
	};
}

function speedup(rust: Distribution, typescript: Distribution): number {
	if (rust.median <= 0) throw new HarnessFailure("statistics", "Rust median must be positive for a speedup ratio");
	return typescript.median / rust.median;
}

function implementationOrder(index: number): readonly Implementation[] {
	return index % 2 === 0 ? implementationNames : ["typescript", "rust"];
}

function benchmarkEnvironment(sandbox: string): Record<string, string | undefined> {
	const agentDirectory = join(sandbox, "agent");
	const sessionDirectory = join(sandbox, "sessions");
	mkdirSync(agentDirectory, { recursive: true });
	mkdirSync(sessionDirectory, { recursive: true });
	return {
		HOME: join(sandbox, "home"),
		PI_CODING_AGENT_DIR: agentDirectory,
		PI_CODING_AGENT_SESSION_DIR: sessionDirectory,
		PI_EXTENSION_HOST: EXTENSION_HOST,
		PI_OFFLINE: "1",
		PI_SKIP_VERSION_CHECK: "1",
		TERM: PTY_TERM,
		TERM_PROGRAM: process.env.TERM_PROGRAM ?? "WarpTerminal",
		COLORTERM: process.env.COLORTERM ?? "truecolor",
	};
}

function binaryFor(implementation: Implementation): string {
	return implementation === "rust" ? RUST_BINARY : TYPESCRIPT_BINARY;
}

const extensionFreeArgs = [
	"--provider",
	"anthropic",
	"--model",
	"claude-sonnet-4-5",
	"--api-key",
	"verification-no-network",
	"--no-extensions",
	"--no-session",
	"--offline",
	"--no-context-files",
	"--no-skills",
	"--no-prompt-templates",
	"--no-themes",
	"--approve",
] as const;

const streamingArgs = [
	"--provider",
	"verification",
	"--model",
	"model",
	"--api-key",
	"verification-key",
	"--extension",
	VERIFICATION_EXTENSION,

	"--offline",
	"--no-context-files",
	"--no-skills",
	"--no-prompt-templates",
	"--no-themes",
	"--approve",
] as const;


async function requireCleanExitIfSettled(
	pty: PtyProcess,
	label: string,
): Promise<boolean> {
	if (!pty.exited) return false;
	const code = await pty.waitForExit(1);
	if (code !== 0) throw new HarnessFailure(label, `${label} exited ${code}\nPTY tail:\n${tail(pty.snapshot().rawText, 4_000)}`);
	return true;
}

async function terminateAndRequireCleanExit(pty: PtyProcess, label: string): Promise<void> {
	if (await requireCleanExitIfSettled(pty, label)) return;
	pty.writeKeys("/quit", PTY_KEYS.enter);
	let code: number;
	try {
		code = await pty.waitForExit(10_000);
	} catch (error) {
		throw new HarnessFailure(
			label,
			`${label} did not exit through /quit: ${errorMessage(error instanceof Error ? error : String(error))}\nPTY tail:\n${tail(pty.snapshot().rawText, 4_000)}`,
		);
	}
	if (code !== 0) throw new HarnessFailure(label, `${label} /quit exited ${code}\nPTY tail:\n${tail(pty.snapshot().rawText, 4_000)}`);
}

async function runVersionSample(
	implementation: Implementation,
	kind: SampleKind,
	ticksPerSecond: number,
): Promise<VersionSample> {
	const sandbox = temporaryDirectory(`version-${implementation}`);
	mkdirSync(join(sandbox, "home"), { recursive: true });
	const pty = spawnPty({
		argv: [binaryFor(implementation), "--version"],
		cwd: sandbox,
		env: benchmarkEnvironment(sandbox),
		size: { columns: 80, rows: 24 },
	});
	const sampler = new ProcTreeSampler(pty.pid, PROC_SAMPLE_INTERVAL_MS);
	try {
		const exitCode = await pty.waitForExit(10_000);
		const finalCpu = await sampler.stop();
		const snapshot = pty.snapshot();
		if (exitCode !== 0) {
			throw new HarnessFailure(
				`version:${implementation}`,
				`${implementation} --version exited ${exitCode}\nPTY output:\n${tail(snapshot.rawText, 4_000)}`,
			);
		}
		const output = stripTerminalSequences(snapshot.applicationText).trim();
		if (!output) throw new HarnessFailure(`version:${implementation}`, `${implementation} --version produced no output`);
		return {
			kind,
			wallMs: snapshot.elapsedMs,
			processTreeCpuMs: cpuMilliseconds(finalCpu, ticksPerSecond),
			procSamples: finalCpu.procSamples,
			observedProcesses: finalCpu.observedProcesses,
			ptyRootPid: pty.pid,
			output,
		};
	} finally {
		await sampler.stop();
		await pty.terminate();
	}
}

async function runFirstFrameSample(
	implementation: Implementation,
	kind: SampleKind,
	ticksPerSecond: number,
): Promise<FirstFrameSample> {
	const sandbox = temporaryDirectory(`frame-${implementation}`);
	mkdirSync(join(sandbox, "home"), { recursive: true });
	const pty = spawnPty({
		argv: [binaryFor(implementation), ...extensionFreeArgs],
		cwd: sandbox,
		env: benchmarkEnvironment(sandbox),
		size: { columns: 100, rows: 32 },
	});
	const sampler = new ProcTreeSampler(pty.pid, PROC_SAMPLE_INTERVAL_MS);
	try {
		const snapshot = await pty.waitFor((candidate) => frameObservation(candidate) !== undefined, {
			deadlineMs: 20_000,
			source: "raw",
		});
		const frame = frameObservation(snapshot);
		if (!frame) throw new HarnessFailure(`first-frame:${implementation}`, "first-frame predicate returned without a frame");
		const frameCpu = sampler.snapshot();
		// First-frame sampling ends before input readiness. Product smoke and
		// streaming checks own `/quit`; finally always reaps this process.
		await requireCleanExitIfSettled(pty, `first-frame:${implementation}`);
		return {
			kind,
			wallMs: frame.elapsedMs,
			processTreeCpuMs: cpuMilliseconds(frameCpu, ticksPerSecond),
			procSamplesAtFrame: frameCpu.procSamples,
			observedProcessesAtFrame: frameCpu.observedProcesses,
			ptyRootPid: pty.pid,
			frameBytes: frame.bytes,
			detection: frame.detection,
		};
	} catch (error) {
		throw new HarnessFailure(
			`first-frame:${implementation}`,
			`${errorMessage(error instanceof Error ? error : String(error))}\nPTY tail:\n${tail(pty.snapshot().rawText, 4_000)}`,
		);
	} finally {
		await sampler.stop();
		await pty.terminate();
	}
}

async function runStreamTurn(
	pty: PtyProcess,
	sampler: ProcTreeSampler,
	sampleId: string,
	label: string,
	finalMarker: string,
	ticksPerSecond: number,
): Promise<StreamTurnMeasurement> {
	const promptOutputOffset = pty.snapshot().rawText.length;
	const prompt = `check 9 ${label}`;
	pty.writeKeys("\x1b[200~", prompt, "\x1b[201~");
	await pty.waitFor(
		(snapshot) => {
			const promptOutput = snapshot.rawText.slice(promptOutputOffset);
			return countOccurrences(promptOutput, SYNC_BEGIN) > 0 && stripTerminalSequences(promptOutput).includes(label);
		},
		{ deadlineMs: 5_000, source: "raw" },
	);
	const beforeCpu = sampler.snapshot();
	const beforeOutput = pty.snapshot();
	const streamOutputOffset = beforeOutput.rawText.length;
	const streamChunkOffset = beforeOutput.chunks.length;
	pty.writeKeys(PTY_KEYS.enter);

	const completed = await pty.waitFor(
		(snapshot) => snapshot.rawText.slice(streamOutputOffset).includes(finalMarker),
		{ deadlineMs: 30_000, source: "raw" },
	);
	const afterCpu = sampler.snapshot();
	const processTreeCpuMs = cpuMillisecondsBetween(beforeCpu, afterCpu, ticksPerSecond);
	const streamOutput = completed.rawText.slice(streamOutputOffset);
	const assistantPaint = firstAssistantPaint(completed, streamChunkOffset, finalMarker);
	const paintedSynchronizedFrames = countOccurrences(streamOutput, SYNC_BEGIN);
	const firstObservedChunk = assistantPaint?.highestChunkIndex ?? 0;
	const highestFullChunkTokenInPty = maximumChunkIndex(streamOutput);
	if (processTreeCpuMs <= 0) {
		throw new HarnessFailure(
			`stream:${label}`,
			`/proc sampling observed zero process-tree CPU across ${STREAM_CHUNKS} provider frames`,
		);
	}
	if (paintedSynchronizedFrames <= 0 || !assistantPaint) {
		throw new HarnessFailure(
			`stream:${label}`,
			`stream produced ${paintedSynchronizedFrames} painted frames and no observable assistant chunk`,
		);
	}
	return {
		sampleId,
		processTreeCpuMs,
		cpuMsPerProviderFrame: processTreeCpuMs / STREAM_CHUNKS,
		streamWallMs: completed.elapsedMs - beforeOutput.elapsedMs,
		providerFrameCount: STREAM_CHUNKS,
		paintedSynchronizedFrames,
		firstObservedChunk,
		highestFullChunkTokenInPty,
		assistantPaintBeforeFinal: assistantPaint.beforeFinal,
		firstAssistantPaintElapsedMs: assistantPaint.elapsedMs,
		rawStreamOutput: streamOutput,
		rawStreamSha256: createHash("sha256").update(streamOutput).digest("hex"),
		procSamplesBefore: beforeCpu.procSamples,
		procSamplesAfter: afterCpu.procSamples,
		observedProcesses: afterCpu.observedProcesses,
		ptyRootPid: pty.pid,
	};
}

async function runStreamProcess(
	implementation: Implementation,
	ticksPerSecond: number,
	sampleId: string,
): Promise<StreamTurnSample> {
	const sandbox = temporaryDirectory(`stream-${implementation}-${sampleId}`);
	mkdirSync(join(sandbox, "home"), { recursive: true });
	const finalMarker = `PI_CHECK9_STREAM_${implementation}_${sampleId.replaceAll("-", "_")}`;
	const pty = spawnPty({
		argv: [binaryFor(implementation), ...streamingArgs],
		cwd: sandbox,
		env: {
			...benchmarkEnvironment(sandbox),
			PI_VERIFICATION_MODE: "text",
			PI_VERIFICATION_CHUNK_COUNT: String(STREAM_CHUNKS),
			PI_VERIFICATION_CHUNK_DELAY_MS: String(STREAM_CHUNK_DELAY_MS),
			PI_VERIFICATION_FINAL_MARKER: finalMarker,
		},
		size: { columns: 100, rows: 40 },
	});
	const sampler = new ProcTreeSampler(pty.pid, PROC_SAMPLE_INTERVAL_MS);
	try {
		await pty.waitFor((snapshot) => frameObservation(snapshot) !== undefined, { deadlineMs: 20_000, source: "raw" });
		const sample = await runStreamTurn(
			pty,
			sampler,
			sampleId,
			`${implementation}-${sampleId}`,
			finalMarker,
			ticksPerSecond,
		);
		await Bun.sleep(100);
		await terminateAndRequireCleanExit(pty, `stream:${implementation}:${sampleId}`);
		const sessionEvidence = streamingSessionEvidence(join(sandbox, "sessions"), finalMarker);
		if (sessionEvidence.persistedProviderFrames !== STREAM_CHUNKS) {
			throw new HarnessFailure(
				`stream:${implementation}:${sampleId}`,
				`persisted assistant response contained ${sessionEvidence.persistedProviderFrames}/${STREAM_CHUNKS} provider frames`,
			);
		}
		return { ...sample, ...sessionEvidence };
	} catch (error) {
		throw new HarnessFailure(
			`stream:${implementation}:${sampleId}`,
			`${errorMessage(error instanceof Error ? error : String(error))}\nPTY tail:\n${tail(pty.snapshot().rawText, 8_000)}`,
		);
	} finally {
		await sampler.stop();
		await pty.terminate();
	}
}

async function runKeypressBenchmark(ticksPerSecond: number): Promise<{
	readonly samples: readonly KeypressSample[];
	readonly warmupCount: number;
	readonly ptyRootPid: number;
	readonly procSamples: number;
	readonly observedProcesses: number;
	readonly processTreeCpuMs: number;
}> {
	const sandbox = temporaryDirectory("keypress-rust");
	mkdirSync(join(sandbox, "home"), { recursive: true });
	const pty = spawnPty({
		argv: [RUST_BINARY, ...extensionFreeArgs],
		cwd: sandbox,
		env: benchmarkEnvironment(sandbox),
		size: { columns: 100, rows: 32 },
	});
	const sampler = new ProcTreeSampler(pty.pid, PROC_SAMPLE_INTERVAL_MS);
	try {
		await pty.waitFor((snapshot) => frameObservation(snapshot) !== undefined, { deadlineMs: 20_000, source: "raw" });
		await Bun.sleep(30);
		const measured: KeypressSample[] = [];
		for (let index = 0; index < KEY_WARMUPS + KEY_SAMPLES; index += 1) {
			const before = pty.snapshot();
			const chunkOffset = before.chunks.length;
			const key = String.fromCharCode(97 + (index % 26));
			pty.writeKeys(key);
			const painted = await pty.waitFor((snapshot) => frameObservation(snapshot, chunkOffset) !== undefined, {
				deadlineMs: 1_000,
				source: "raw",
			});
			const frame = frameObservation(painted, chunkOffset);
			if (!frame) throw new HarnessFailure("keypress:rust", "keypress paint predicate returned without a frame");
			const latencyMs = frame.elapsedMs - before.elapsedMs;
			if (latencyMs < 0) throw new HarnessFailure("keypress:rust", `negative keypress latency ${latencyMs}`);
			if (index >= KEY_WARMUPS) {
				const frameText = painted.chunks
					.slice(chunkOffset)
					.filter((chunk) => chunk.stream === "pty")
					.map((chunk) => chunk.text)
					.join("");
				measured.push({
					latencyMs,
					synchronizedFramesObserved: countOccurrences(frameText, SYNC_BEGIN),
				});
			}
		}
		const clearChunkOffset = pty.snapshot().chunks.length;
		pty.writeKeys("\x15");
		await pty.waitFor((snapshot) => frameObservation(snapshot, clearChunkOffset) !== undefined, {
			deadlineMs: 1_000,
			source: "raw",
		});
		await terminateAndRequireCleanExit(pty, "keypress:rust");
		const finalCpu = await sampler.stop();
		return {
			samples: measured,
			warmupCount: KEY_WARMUPS,
			ptyRootPid: pty.pid,
			procSamples: finalCpu.procSamples,
			observedProcesses: finalCpu.observedProcesses,
			processTreeCpuMs: cpuMilliseconds(finalCpu, ticksPerSecond),
		};
	} catch (error) {
		throw new HarnessFailure(
			"keypress:rust",
			`${errorMessage(error instanceof Error ? error : String(error))}\nPTY tail:\n${tail(pty.snapshot().rawText, 6_000)}`,
		);
	} finally {
		await sampler.stop();
		await pty.terminate();
	}
}

function runCacheDrop(python: string, path: string): void {
	const code = [
		"import os, sys",
		"with open(sys.argv[1], 'rb') as artifact:",
		"    os.posix_fadvise(artifact.fileno(), 0, 0, os.POSIX_FADV_DONTNEED)",
	].join("\n");
	const result = Bun.spawnSync([python, "-c", code, path], { stdout: "pipe", stderr: "pipe" });
	if (result.exitCode !== 0) {
		throw new HarnessFailure(
			"cold-cache",
			`posix_fadvise(DONTNEED) failed for ${path}: ${new TextDecoder().decode(result.stderr).trim()}`,
		);
	}
}

function syncFileSystems(): void {
	const sync = requiredExecutable("sync");
	const result = Bun.spawnSync([sync], { stdout: "pipe", stderr: "pipe" });
	if (result.exitCode !== 0) {
		throw new HarnessFailure("cold-cache", `sync exited ${result.exitCode}: ${new TextDecoder().decode(result.stderr).trim()}`);
	}
}

function summarizeWallSamples<T extends { readonly kind: SampleKind; readonly wallMs: number }>(samples: readonly T[]) {
	const cold = samples.filter((sample) => sample.kind === "cold");
	const warm = samples.filter((sample) => sample.kind === "warm");
	return {
		cold: distribution(cold.map((sample) => sample.wallMs)),
		warm: distribution(warm.map((sample) => sample.wallMs)),
	};
}


async function collectVersionSamples(
	python: string,
	ticksPerSecond: number,
): Promise<ImplementationMeasurements<VersionSample>> {
	const result: Record<Implementation, VersionSample[]> = { rust: [], typescript: [] };
	status("collecting cold --version samples");
	syncFileSystems();
	for (let sample = 0; sample < VERSION_COLD_SAMPLES; sample += 1) {
		for (const implementation of implementationOrder(sample)) {
			runCacheDrop(python, binaryFor(implementation));
			result[implementation].push(await runVersionSample(implementation, "cold", ticksPerSecond));
		}
	}
	status("warming --version artifacts");
	for (let sample = 0; sample < VERSION_WARMUPS; sample += 1) {
		for (const implementation of implementationOrder(sample)) await runVersionSample(implementation, "warm", ticksPerSecond);
	}
	status("collecting warm --version samples");
	for (let sample = 0; sample < VERSION_WARM_SAMPLES; sample += 1) {
		for (const implementation of implementationOrder(sample)) {
			result[implementation].push(await runVersionSample(implementation, "warm", ticksPerSecond));
		}
	}
	return result;
}

async function collectFirstFrameSamples(
	python: string,
	ticksPerSecond: number,
): Promise<ImplementationMeasurements<FirstFrameSample>> {
	const result: Record<Implementation, FirstFrameSample[]> = { rust: [], typescript: [] };
	status("collecting cold extension-free first-frame samples");
	syncFileSystems();
	for (let sample = 0; sample < FIRST_FRAME_COLD_SAMPLES; sample += 1) {
		for (const implementation of implementationOrder(sample)) {
			runCacheDrop(python, binaryFor(implementation));
			result[implementation].push(await runFirstFrameSample(implementation, "cold", ticksPerSecond));
		}
	}
	status("warming extension-free first-frame artifacts");
	for (let sample = 0; sample < FIRST_FRAME_WARMUPS; sample += 1) {
		for (const implementation of implementationOrder(sample)) await runFirstFrameSample(implementation, "warm", ticksPerSecond);
	}
	status("collecting warm extension-free first-frame samples");
	for (let sample = 0; sample < FIRST_FRAME_WARM_SAMPLES; sample += 1) {
		for (const implementation of implementationOrder(sample)) {
			result[implementation].push(await runFirstFrameSample(implementation, "warm", ticksPerSecond));
		}
	}
	return result;
}

async function collectStreamSamples(ticksPerSecond: number): Promise<ImplementationMeasurements<StreamTurnSample>> {
	const result: Record<Implementation, StreamTurnSample[]> = { rust: [], typescript: [] };
	status("warming identical shared-extension streaming fixture");
	for (let sample = 0; sample < STREAM_PROCESS_WARMUPS; sample += 1) {
		for (const implementation of implementationOrder(sample)) {
			await runStreamProcess(implementation, ticksPerSecond, `warmup-${sample + 1}`);
		}
	}
	status("collecting streaming-tail process-tree CPU samples");
	for (let sample = 0; sample < STREAM_PROCESS_SAMPLES; sample += 1) {
		for (const implementation of implementationOrder(sample)) {
			const sampleId = `sample-${String(sample + 1).padStart(3, "0")}`;
			result[implementation].push(await runStreamProcess(implementation, ticksPerSecond, sampleId));
		}
	}
	return result;
}

function exactBlocker(label: string, actual: number, target: number, evidence: string): string {
	return `${label}: ${actual.toFixed(3)}x < required ${target.toFixed(3)}x (${evidence})`;
}

function writeArtifact(): void {
	mkdirSync(dirname(ARTIFACT_PATH), { recursive: true });
	artifact.generatedAt = new Date().toISOString();
	writeFileSync(ARTIFACT_PATH, `${JSON.stringify(artifact, null, 2)}\n`, "utf8");
}

async function buildProducts(): Promise<void> {
	const cargo = requiredExecutable("cargo");
	const npm = requiredExecutable("npm");
	const bun = requiredExecutable("bun");
	await runCheckedCommand({
		label: "Rust pi release build",
		cwd: REPOSITORY_ROOT,
		argv: [cargo, "build", "-p", "pi", "--release", "--locked"],
	});
	await runCheckedCommand({
		label: "Rust extension host release build",
		cwd: REPOSITORY_ROOT,
		argv: [
			bun,
			"run",
			"build:extension-host",
			"--target",
			"x86_64-unknown-linux-gnu",
			"--out",
			HOST_BUILD_ROOT,
		],
	});
	await runCheckedCommand({
		label: "TypeScript pi locked dependency install",
		cwd: resolve(REPOSITORY_ROOT, ".references/pi"),
		argv: [npm, "ci", "--ignore-scripts"],
	});
	// Offline replacement for the upstream `build:binary` chain: the ai
	// package's build starts with `npm run generate-models`, which fetches
	// live provider catalogs (forbidden here) and mutates the reference tree.
	// main() already provisioned the gitignored provider data (with the
	// inversion proof) before fingerprinting; run the same compile steps the
	// upstream chain would, minus the generator.
	const refRoot = resolve(REPOSITORY_ROOT, ".references/pi");
	const tsgo = resolve(refRoot, "node_modules/.bin/tsgo");
	const shx = resolve(refRoot, "node_modules/.bin/shx");
	await runCheckedCommand({
		label: "TypeScript pi tui build",
		cwd: resolve(refRoot, "packages/tui"),
		argv: [tsgo, "-p", "tsconfig.build.json"],
	});
	await runCheckedCommand({
		label: "TypeScript pi ai build (generate-models skipped)",
		cwd: resolve(refRoot, "packages/ai"),
		argv: [tsgo, "-p", "tsconfig.build.json"],
	});
	await runCheckedCommand({
		label: "TypeScript pi ai data staging",
		cwd: resolve(refRoot, "packages/ai"),
		argv: [shx, "rm", "-rf", "dist/providers/data"],
	});
	await runCheckedCommand({
		label: "TypeScript pi ai data copy",
		cwd: resolve(refRoot, "packages/ai"),
		argv: [shx, "cp", "-r", "src/providers/data", "dist/providers/data"],
	});
	await runCheckedCommand({
		label: "TypeScript pi agent build",
		cwd: resolve(refRoot, "packages/agent"),
		argv: [tsgo, "-p", "tsconfig.build.json"],
	});
	await runCheckedCommand({
		label: "TypeScript pi coding-agent build",
		cwd: resolve(refRoot, "packages/coding-agent"),
		argv: [npm, "run", "build"],
	});
	await runCheckedCommand({
		label: "TypeScript pi binary compile",
		cwd: resolve(refRoot, "packages/coding-agent"),
		argv: [
			bun,
			"build",
			"--compile",
			"./dist/bun/cli.js",
			"./src/utils/image-resize-worker.ts",
			"--outfile",
			"dist/pi",
		],
	});
	await runCheckedCommand({
		label: "TypeScript pi binary assets",
		cwd: resolve(refRoot, "packages/coding-agent"),
		argv: [npm, "run", "copy-binary-assets"],
	});
	artifact.build.artifacts = {
		rustPi: fileRecord(RUST_BINARY),
		typescriptPi: fileRecord(TYPESCRIPT_BINARY),
		rustExtensionHost: fileRecord(EXTENSION_HOST),
		sharedVerificationExtension: fileRecord(VERIFICATION_EXTENSION),
		performanceHarness: fileRecord(resolve(import.meta.dirname, "performance.ts")),
	};
}

async function main(): Promise<void> {
	artifact.machine = machineMetadata();
	const ticksPerSecond = clockTicksPerSecond();
	const python = requiredExecutable("python3");
	// Provision the gitignored reference provider data BEFORE fingerprinting:
	// reconstruction is deterministic, and the fingerprint must cover the
	// exact data the benchmark builds against (a fresh clone has none).
	await runCheckedCommand({
		label: "Provider data reconstruction (offline, proof-checked)",
		cwd: REPOSITORY_ROOT,
		argv: [requiredExecutable("bun"), "run", "scripts/reconstruct-provider-data.ts"],
	});
	const sourceBefore = {
		rust: sourceFingerprint(RUST_SOURCE_ROOTS),
		typescript: sourceFingerprint(TYPESCRIPT_SOURCE_ROOTS),
	};
	artifact.build.sourceFingerprints = { before: sourceBefore };
	artifact.harness = {
		ptyDriver: "scripts/verification/pty.ts PtyProcess",
		processTreeCpuSource: "/proc/<pid>/stat plus /proc/<pid>/task/*/children rooted at PtyProcess.pid",
		processTreeAccounting: "1ms sampling; maximum observed own utime+stime per (pid,starttime); interval delta across all observed identities",
		procSampleIntervalMs: PROC_SAMPLE_INTERVAL_MS,
		clockTicksPerSecond: ticksPerSecond,
		quantileMethod: "R-7 linear interpolation",
		coldCacheMethod: "sync once per cold group, then posix_fadvise(POSIX_FADV_DONTNEED) on the implementation executable before every cold sample",
		firstFrameDefinition: "first complete DEC synchronized-output transaction; row-local printable CSI transaction is the recorded fallback",
		streamCpuDefinition: "whole-process-tree CPU immediately before submit Enter through final marker, divided by the fixed 256 deterministic provider text-delta frames; painted frame/coalescing counts recorded separately",
		keypressDefinition: "PTY key write to first complete synchronized output paint, sequential with no artificial/background-coalescer delay",
		ptyTerm: PTY_TERM,
		inputPaintBypassesBackgroundCoalescer: true,
		versionSamples: { cold: VERSION_COLD_SAMPLES, warmups: VERSION_WARMUPS, warm: VERSION_WARM_SAMPLES },
		firstFrameSamples: { cold: FIRST_FRAME_COLD_SAMPLES, warmups: FIRST_FRAME_WARMUPS, warm: FIRST_FRAME_WARM_SAMPLES },
		streamSamples: { processWarmups: STREAM_PROCESS_WARMUPS, measuredPerImplementation: STREAM_PROCESS_SAMPLES },
		keypressSamples: { warmups: KEY_WARMUPS, warm: KEY_SAMPLES },
		streamChunks: STREAM_CHUNKS,
		streamChunkDelayMs: STREAM_CHUNK_DELAY_MS,
	};

	await buildProducts();

	const versionSamples = await collectVersionSamples(python, ticksPerSecond);
	const versionSummary = {
		rust: summarizeWallSamples(versionSamples.rust),
		typescript: summarizeWallSamples(versionSamples.typescript),
	};
	const versionSpeedups = {
		cold: speedup(versionSummary.rust.cold, versionSummary.typescript.cold),
		warm: speedup(versionSummary.rust.warm, versionSummary.typescript.warm),
	};
	artifact.measurements.version = {
		unit: "milliseconds wall time",
		commands: {
			rust: [RUST_BINARY, "--version"],
			typescript: [TYPESCRIPT_BINARY, "--version"],
		},
		summary: versionSummary,
		speedupTypescriptOverRust: versionSpeedups,
		rawSamples: versionSamples,
	};
	writeArtifact();

	const firstFrameSamples = await collectFirstFrameSamples(python, ticksPerSecond);
	const firstFrameSummary = {
		rust: summarizeWallSamples(firstFrameSamples.rust),
		typescript: summarizeWallSamples(firstFrameSamples.typescript),
	};
	const firstFrameSpeedups = {
		cold: speedup(firstFrameSummary.rust.cold, firstFrameSummary.typescript.cold),
		warm: speedup(firstFrameSummary.rust.warm, firstFrameSummary.typescript.warm),
	};
	artifact.measurements.extensionFreeFirstFrame = {
		unit: "milliseconds wall time",
		commands: {
			rust: [RUST_BINARY, ...extensionFreeArgs],
			typescript: [TYPESCRIPT_BINARY, ...extensionFreeArgs],
		},
		summary: firstFrameSummary,
		speedupTypescriptOverRust: firstFrameSpeedups,
		rawSamples: firstFrameSamples,
	};
	writeArtifact();

	const streamSamples = await collectStreamSamples(ticksPerSecond);
	const streamSummary = {
		rust: distribution(streamSamples.rust.map((sample) => sample.cpuMsPerProviderFrame)),
		typescript: distribution(streamSamples.typescript.map((sample) => sample.cpuMsPerProviderFrame)),
	};
	const streamSpeedup = speedup(streamSummary.rust, streamSummary.typescript);
	const streamingStarvation = {
		rust: streamSamples.rust.filter((sample) => !sample.assistantPaintBeforeFinal).length,
		typescript: streamSamples.typescript.filter((sample) => !sample.assistantPaintBeforeFinal).length,
	};
	const streamThresholdValid = streamingStarvation.rust === 0 && streamingStarvation.typescript === 0;
	artifact.measurements.streamingTailFrameCpu = {
		unit: "process-tree CPU milliseconds per deterministic provider frame",
		commands: {
			rust: [RUST_BINARY, ...streamingArgs],
			typescript: [TYPESCRIPT_BINARY, ...streamingArgs],
		},
		fixture: {
			extension: VERIFICATION_EXTENSION,
			chunks: STREAM_CHUNKS,
			chunkDelayMs: STREAM_CHUNK_DELAY_MS,
		},
		summary: streamSummary,
		speedupTypescriptOverRust: streamSpeedup,
		thresholdValid: streamThresholdValid,
		visibleStreamingStarvationSamples: streamingStarvation,
		rawSamples: streamSamples,
	};
	writeArtifact();

	status("collecting Rust native keypress-to-paint samples");
	const keypress = await runKeypressBenchmark(ticksPerSecond);
	const keypressSummary = distribution(keypress.samples.map((sample) => sample.latencyMs));
	artifact.measurements.rustNativeKeypressToPaint = {
		unit: "milliseconds wall time",
		summary: keypressSummary,
		thresholdMs: KEYPRESS_P99_TARGET_MS,
		warmupCount: keypress.warmupCount,
		ptyRootPid: keypress.ptyRootPid,
		procSamples: keypress.procSamples,
		observedProcesses: keypress.observedProcesses,
		processTreeCpuMs: keypress.processTreeCpuMs,
		rawSamples: keypress.samples,
	};

	const sourceAfter = {
		rust: sourceFingerprint(RUST_SOURCE_ROOTS),
		typescript: sourceFingerprint(TYPESCRIPT_SOURCE_ROOTS),
	};
	const sourceStable =
		sourceBefore.rust.sha256 === sourceAfter.rust.sha256 &&
		sourceBefore.typescript.sha256 === sourceAfter.typescript.sha256;
	artifact.build.sourceFingerprints = {
		before: sourceBefore,
		after: sourceAfter,
		stable: sourceStable,
	};

	const blockers: string[] = [];
	if (!sourceStable) {
		blockers.push(
			`source changed during benchmark session: Rust ${sourceBefore.rust.sha256} -> ${sourceAfter.rust.sha256}; ` +
				`TypeScript ${sourceBefore.typescript.sha256} -> ${sourceAfter.typescript.sha256}`,
		);
	}
	if (versionSpeedups.cold < VERSION_SPEEDUP_TARGET) {
		blockers.push(
			exactBlocker(
				"cold pi --version speedup",
				versionSpeedups.cold,
				VERSION_SPEEDUP_TARGET,
				`TypeScript median ${versionSummary.typescript.cold.median.toFixed(3)} ms / Rust median ${versionSummary.rust.cold.median.toFixed(3)} ms`,
			),
		);
	}
	if (versionSpeedups.warm < VERSION_SPEEDUP_TARGET) {
		blockers.push(
			exactBlocker(
				"warm pi --version speedup",
				versionSpeedups.warm,
				VERSION_SPEEDUP_TARGET,
				`TypeScript median ${versionSummary.typescript.warm.median.toFixed(3)} ms / Rust median ${versionSummary.rust.warm.median.toFixed(3)} ms`,
			),
		);
	}
	if (firstFrameSpeedups.cold < FIRST_FRAME_SPEEDUP_TARGET) {
		blockers.push(
			exactBlocker(
				"cold extension-free first-frame speedup",
				firstFrameSpeedups.cold,
				FIRST_FRAME_SPEEDUP_TARGET,
				`TypeScript median ${firstFrameSummary.typescript.cold.median.toFixed(3)} ms / Rust median ${firstFrameSummary.rust.cold.median.toFixed(3)} ms`,
			),
		);
	}
	if (firstFrameSpeedups.warm < FIRST_FRAME_SPEEDUP_TARGET) {
		blockers.push(
			exactBlocker(
				"warm extension-free first-frame speedup",
				firstFrameSpeedups.warm,
				FIRST_FRAME_SPEEDUP_TARGET,
				`TypeScript median ${firstFrameSummary.typescript.warm.median.toFixed(3)} ms / Rust median ${firstFrameSummary.rust.warm.median.toFixed(3)} ms`,
			),
		);
	}
	if (!streamThresholdValid) {
		blockers.push(
			`streaming-tail frame CPU threshold invalid: assistant content was not painted before the final marker in ` +
				`${streamingStarvation.rust}/${streamSamples.rust.length} Rust samples and ` +
				`${streamingStarvation.typescript}/${streamSamples.typescript.length} TypeScript samples ` +
				`(256 chunks × 2 ms; raw PTY output and hashes are recorded per sample)`,
		);
	} else if (streamSpeedup < STREAM_CPU_SPEEDUP_TARGET) {
		blockers.push(
			exactBlocker(
				"streaming-tail provider-frame CPU speedup",
				streamSpeedup,
				STREAM_CPU_SPEEDUP_TARGET,
				`TypeScript median ${streamSummary.typescript.median.toFixed(6)} CPU ms/frame / Rust median ${streamSummary.rust.median.toFixed(6)} CPU ms/frame`,
			),
		);
	}
	if (keypressSummary.p99 >= KEYPRESS_P99_TARGET_MS) {
		blockers.push(
			`Rust native keypress-to-paint p99: ${keypressSummary.p99.toFixed(3)} ms >= required ${KEYPRESS_P99_TARGET_MS.toFixed(3)} ms ` +
				`(median ${keypressSummary.median.toFixed(3)} ms, p95 ${keypressSummary.p95.toFixed(3)} ms, ${keypressSummary.count} samples)`,
		);
	}

	artifact.blockers = blockers;
	artifact.pass = blockers.length === 0;
	writeArtifact();
	if (blockers.length > 0) throw new ThresholdFailure(blockers);
	process.stdout.write(`check 9 passed; artifact: ${ARTIFACT_PATH}\n`);
}

try {
	await main();
} catch (error) {
	const failure = error instanceof Error ? error : new Error(String(error));
	if (!(failure instanceof ThresholdFailure)) {
		const stage = failure instanceof HarnessFailure ? failure.stage : "unexpected";
		artifact.pass = false;
		artifact.blockers = [`${stage}: ${failure.message}`];
		artifact.failure = { stage, message: failure.message };
		writeArtifact();
	}
	process.stderr.write(`check 9 failed:\n${failure.message}\nartifact: ${ARTIFACT_PATH}\n`);
	process.exitCode = 1;
} finally {
	for (const path of temporaryDirectories) rmSync(path, { recursive: true, force: true });
}
