#!/usr/bin/env bun
/**
 * Symmetric install-footprint accounting (PERF-T7, issue #91).
 *
 * Measures the installed-size footprint of the Rust release distribution and
 * the pinned upstream Bun/Node reference under one contract so the two sides
 * are semantically equal before any footprint number is quoted. The contract
 * (classes C1 launcher / C2 runtime payload / C3 shipped dependencies /
 * C4 external interpreter prerequisite, measurement unit, exclusions, and
 * authorities) is defined in docs/PERF-T7-install-footprint-accounting.md;
 * this runner is its mechanical enforcement.
 *
 * Rust side: the real release tree assembled by the production assembler
 * (`assembleRelease` from scripts/release/stage.ts, the same authority
 * package-release.ts uses), built from `cargo build -p pi --release --locked`
 * and the production extension-host build.
 *
 * Upstream side: `npm pack --dry-run --json` file lists for the pinned
 * reference package and its first-party workspace dependencies, plus the
 * production install closure from the upstream installer's own
 * `install-lock/package-lock.json`, measured as installed node_modules
 * directories. The interpreter (Node/Bun) is recorded as context and never
 * summed.
 *
 * Accounting only: no size threshold, gate, or target is defined or applied.
 * `pass` means "accounting complete and distributions quiet", never "small".
 * Output: target/bench/install-footprint.json.
 */

import { createHash } from "node:crypto";
import {
	existsSync,
	lstatSync,
	mkdirSync,
	readdirSync,
	readFileSync,
	readlinkSync,
	realpathSync,
	rmSync,
	writeFileSync,
} from "node:fs";
import { arch, platform } from "node:os";
import { dirname, join, resolve } from "node:path";
import {
	HOST_COMPATIBILITY_VERSION,
	HOST_PROTOCOL_VERSION,
	type HostArtifact,
} from "../release/host.ts";
import { realFs } from "../release/runner.ts";
import { provisionBunRuntime } from "../release/runtime.ts";
import { assembleRelease } from "../release/stage.ts";
import { planFor, type TargetPlan } from "../release/targets.ts";
import {
	formatNoiseRejection,
	NoiseRejection,
	type NoisyDistribution,
	requireQuiet,
} from "../statistics.ts";
import { localRustTriple } from "./dependency-exposure.ts";
import { distribution, HarnessFailure } from "./performance.ts";

const REPOSITORY_ROOT = resolve(import.meta.dirname, "../..");
const ARTIFACT_PATH = resolve(
	REPOSITORY_ROOT,
	"target/bench/install-footprint.json",
);
const REFERENCE_ROOT = resolve(REPOSITORY_ROOT, ".references/pi");
const CODING_AGENT_DIR = join(REFERENCE_ROOT, "packages/coding-agent");
const FOOTPRINT_BUILD_ROOT = resolve(
	REPOSITORY_ROOT,
	"target/bench/footprint-build",
);
const FOOTPRINT_STAGING_ROOT = resolve(
	REPOSITORY_ROOT,
	"target/bench/footprint-staging",
);
const RUNTIME_CACHE = resolve(REPOSITORY_ROOT, "target/bench/runtime-cache");
const RUST_BINARY = resolve(REPOSITORY_ROOT, "target/release/pi");

/** Full-accounting recomputations per run; static byte scans are expected degenerate. */
export const FOOTPRINT_SCAN_SAMPLES = 5;
/** npm-packaged entrypoint of the reference package (its declared `bin.pi`). */
export const UPSTREAM_NPM_LAUNCHER = "dist/bundle/cli.js";
/** Compiled launcher file produced only by upstream `build:binary` (never npm-published). */
export const UPSTREAM_COMPILED_LAUNCHER = "dist/pi";
/** First-party workspace scope inside the install-lock closure. */
export const FIRST_PARTY_SCOPE = "@earendil-works/pi-";
/** The primary payload package itself; excluded from C3 by the double-counting ban. */
export const PRIMARY_PAYLOAD_PACKAGE = "@earendil-works/pi-coding-agent";
/** Upstream reference pin the accounting contract is defined against. */
export const REFERENCE_PIN = "8fa7eebd235355522c8104166b4f1f959b4e2f10";

export const FOOTPRINT_SCHEMA = "pi.footprint.v1" as const;

export interface CommandRecord {
	readonly label: string;
	readonly cwd: string;
	readonly argv: readonly string[];
}

interface AuthorityRecord {
	readonly label: string;
	readonly path: string;
	readonly sha256: string;
	/** Present when the authority is a captured command document, not a file. */
	readonly capturedFrom?: string;
}

function listingAuthorityRecord(pack: PackOutput): AuthorityRecord {
	return {
		label: `npm pack payload listing (${pack.label})`,
		path: pack.cwd,
		sha256: pack.listingSha256,
		capturedFrom: "npm pack --dry-run --json (stdout)",
	};
}

export interface PackFile {
	readonly path: string;
	readonly size: number;
}

export interface WalkTotal {
	readonly bytes: number;
	readonly files: number;
	readonly symlinks: number;
}

export interface ClosureEntry {
	/** Lockfile key, e.g. `node_modules/chalk` (relative to the reference root). */
	readonly lockPath: string;
	/** Bare package name without the `node_modules/` prefix. */
	readonly name: string;
	readonly version: string;
	readonly optional: boolean;
	readonly os: readonly string[] | undefined;
	readonly cpu: readonly string[] | undefined;
	readonly firstParty: boolean;
}

export interface ClosurePlan {
	/** Third-party entries whose installed directories must be walked (C3). */
	readonly measure: readonly ClosureEntry[];
	/** Workspace-linked first-party entries measured via their own pack lists (C3). */
	readonly workspaceLinks: readonly ClosureEntry[];
	/** The primary payload package's own closure entry (counted as C1+C2, never C3). */
	readonly primaryPayload: ClosureEntry | undefined;
	/** Entries npm itself would not install on this platform (os/cpu filter). */
	readonly foreignOptional: readonly ClosureEntry[];
}

export interface PackClassification {
	/** C1: the npm launcher file. */
	readonly launcher: WalkTotal;
	/** C2: every remaining shipped byte. */
	readonly runtimePayload: WalkTotal;
	/** build:binary-only compiled launcher; excluded from the npm-variant total. */
	readonly compiledLauncherVariant: WalkTotal;
}

export interface ExternalPrerequisite {
	readonly name: string;
	readonly constraint: string | undefined;
	readonly onMachineVersion: string | undefined;
	readonly onMachineBytes: number | undefined;
	readonly treatment: "context-only, excluded from every total";
}

interface SideScan {
	readonly launcher: number;
	readonly runtimePayload: number;
	readonly shippedDependencies: number;
}

/** Distribution shape mirroring the check-9 runner's (performance.ts). */
export interface Distribution {
	readonly count: number;
	readonly median: number;
	readonly p95: number;
	readonly p99: number;
	readonly min: number;
	readonly max: number;
	readonly stddev: number;
	readonly relativeSpread: number | null;
}

interface ClassRecord {
	readonly bytes: Distribution;
	readonly files: number;
	readonly emptyReason?: string;
}

interface SideRecord {
	readonly implementation: "rust" | "upstream";
	readonly commands: readonly CommandRecord[];
	readonly authorities: readonly AuthorityRecord[];
	readonly classes: {
		launcher: ClassRecord;
		"runtime-payload": ClassRecord;
		"shipped-dependencies": ClassRecord;
	};
	readonly total: { readonly bytes: Distribution };
	readonly symlinks: number;
	readonly externalPrerequisites: readonly ExternalPrerequisite[];
	readonly excluded: readonly {
		readonly label: string;
		readonly reason: string;
	}[];
}

interface ClassSeries {
	readonly launcher: readonly number[];
	readonly runtimePayload: readonly number[];
	readonly shippedDependencies: readonly number[];
}

interface SideMeasurement {
	readonly record: SideRecord;
	/** Raw per-scan totals feeding the D4 noise gate. */
	readonly scanTotals: readonly number[];
	/** Raw per-scan class totals; invariant 3 gates every one of these. */
	readonly classSeries: ClassSeries;
	readonly launcherBytes: number;
	readonly totalBytes: number;
	readonly compiledLauncherBytes: number;
}

function classSeriesOf(scans: readonly SideScan[]): ClassSeries {
	return {
		launcher: scans.map((scan) => scan.launcher),
		runtimePayload: scans.map((scan) => scan.runtimePayload),
		shippedDependencies: scans.map((scan) => scan.shippedDependencies),
	};
}

interface FootprintArtifact {
	readonly schema: typeof FOOTPRINT_SCHEMA;
	generatedAt: string;
	pass: boolean;
	blockers: string[];
	readonly accounting: {
		readonly contract: string;
		readonly unit: string;
		readonly scanSamples: number;
		readonly noiseGate: string;
		readonly thresholds: string;
		readonly primaryComparison: string;
		readonly launcherPaths: {
			readonly rust: string;
			readonly upstreamNpm: string;
			readonly upstreamCompiled: string;
		};
	};
	machine: Record<string, string>;
	sides: { rust: SideRecord; upstream: SideRecord };
	comparison: {
		readonly npmVariant: {
			readonly rustTotalBytes: number;
			readonly upstreamTotalBytes: number;
			readonly upstreamOverRust: number;
		};
		readonly launcherOnlyContext: {
			readonly rustLauncherBytes: number;
			readonly upstreamCompiledLauncherBytes: number;
			readonly note: string;
		};
	};
	failure?: { readonly stage: string; readonly message: string };
}

function emptySideRecord(implementation: "rust" | "upstream"): SideRecord {
	return {
		implementation,
		commands: [],
		authorities: [],
		classes: {
			launcher: { bytes: distribution([0]), files: 0 },
			"runtime-payload": { bytes: distribution([0]), files: 0 },
			"shipped-dependencies": { bytes: distribution([0]), files: 0 },
		},
		total: { bytes: distribution([0]) },
		symlinks: 0,
		externalPrerequisites: [],
		excluded: [],
	};
}

const artifact: FootprintArtifact = {
	schema: FOOTPRINT_SCHEMA,
	generatedAt: new Date().toISOString(),
	pass: false,
	blockers: [],
	accounting: {
		contract: "docs/PERF-T7-install-footprint-accounting.md",
		unit: "apparent file bytes (lstat size); symlinks count zero bytes and are never followed",
		scanSamples: FOOTPRINT_SCAN_SAMPLES,
		noiseGate:
			"relative spread <= 0.2 of median per D4 (scripts/statistics.ts requireQuiet)",
		thresholds:
			"none; this lane defines accounting only and applies no size target",
		primaryComparison:
			"npm-variant installed footprint: launcher + runtime payload + shipped dependencies on both sides, external interpreter excluded",
		launcherPaths: {
			rust: "pi (plan.piBinaryName in the assembled release tree)",
			upstreamNpm: UPSTREAM_NPM_LAUNCHER,
			upstreamCompiled: UPSTREAM_COMPILED_LAUNCHER,
		},
	},
	machine: {},
	sides: {
		rust: emptySideRecord("rust"),
		upstream: emptySideRecord("upstream"),
	},
	comparison: {
		npmVariant: {
			rustTotalBytes: 0,
			upstreamTotalBytes: 0,
			upstreamOverRust: 0,
		},
		launcherOnlyContext: {
			rustLauncherBytes: 0,
			upstreamCompiledLauncherBytes: 0,
			note: "executable artifact size comparison (D7 naming); not an installed-footprint claim",
		},
	},
};

function status(message: string): void {
	process.stderr.write(`[footprint] ${message}\n`);
}

function sha256BytesOf(path: string): string {
	return createHash("sha256").update(readFileSync(path)).digest("hex");
}

function authorityRecord(label: string, path: string): AuthorityRecord {
	if (!existsSync(path)) {
		throw new HarnessFailure(
			"prerequisite",
			`expected authority file is missing: ${path}`,
		);
	}
	return { label, path, sha256: sha256BytesOf(path) };
}

function requiredExecutable(name: string): string {
	const path = Bun.which(name);
	if (!path) {
		throw new HarnessFailure(
			"prerequisite",
			`required executable not found on PATH: ${name}`,
		);
	}
	return path;
}

async function runCheckedCommand(
	record: CommandRecord,
	tracked: CommandRecord[],
): Promise<{ stdout: string; stderr: string }> {
	tracked.push(record);
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
			`footprint:${record.label}`,
			`${record.label} exited ${exitCode}\nstdout tail:\n${stdout.slice(-4_000)}\nstderr tail:\n${stderr.slice(-4_000)}`,
		);
	}
	return { stdout, stderr };
}

function readJsonFile(path: string): unknown {
	if (!existsSync(path)) {
		throw new HarnessFailure(
			"prerequisite",
			`expected JSON file is missing: ${path}`,
		);
	}
	return JSON.parse(readFileSync(path, "utf8"));
}

/**
 * Sum apparent bytes of every regular file under `root` (a directory).
 * Symlinks are never followed and count zero bytes; their count is reported.
 */
export function walkApparentBytes(root: string): WalkTotal {
	let bytes = 0;
	let files = 0;
	let symlinks = 0;
	const visit = (path: string): void => {
		for (const entry of readdirSync(path, { withFileTypes: true })) {
			const child = join(path, entry.name);
			if (entry.isSymbolicLink()) {
				symlinks += 1;
				continue;
			}
			if (entry.isDirectory()) {
				visit(child);
				continue;
			}
			if (entry.isFile()) {
				bytes += lstatSync(child).size;
				files += 1;
			}
		}
	};
	if (!statIsDirectory(root)) {
		throw new HarnessFailure(
			"footprint:walk",
			`walkApparentBytes requires a directory: ${root}`,
		);
	}
	visit(root);
	return { bytes, files, symlinks };
}

function statIsDirectory(path: string): boolean {
	const stat = lstatSync(path);
	if (stat.isSymbolicLink()) return false;
	return stat.isDirectory();
}

/**
 * npm `os`/`cpu` constraint semantics: an absent field matches anything;
 * entries match by equality or `!name` negation. Mirrors the filtering npm
 * itself applies when installing optional platform dependencies.
 */
export function npmPlatformMatches(
	constraints: readonly string[] | undefined,
	actual: string,
): boolean {
	if (constraints === undefined || constraints.length === 0) return true;
	let matched = false;
	for (const constraint of constraints) {
		if (constraint.startsWith("!")) {
			if (constraint.slice(1) === actual) return false;
		} else if (constraint === actual) {
			matched = true;
		}
	}
	// npm checkList semantics: match at least one non-negated entry when any
	// are present; a list of only negations matches everything it does not name.
	return (
		matched || constraints.every((constraint) => constraint.startsWith("!"))
	);
}

function closureEntryMatchesPlatform(
	entry: ClosureEntry,
	os: string,
	cpu: string,
): boolean {
	return npmPlatformMatches(entry.os, os) && npmPlatformMatches(entry.cpu, cpu);
}

/**
 * Split the install-lock closure into contract sets: third-party directories
 * to walk, first-party workspace links to measure via pack lists, the primary
 * payload entry (never C3), and foreign-platform optional entries npm itself
 * would not install on this platform.
 */
export function planUpstreamClosure(
	lock: { packages?: Record<string, Record<string, unknown>> },
	options: { platform: string; arch: string },
): ClosurePlan {
	const packages = lock.packages ?? {};
	const measure: ClosureEntry[] = [];
	const workspaceLinks: ClosureEntry[] = [];
	const foreignOptional: ClosureEntry[] = [];
	let primaryPayload: ClosureEntry | undefined;
	for (const lockPath of Object.keys(packages)) {
		if (!lockPath.startsWith("node_modules/")) continue;
		// Nested installs (node_modules/<parent>/node_modules/<child>) live
		// inside the parent's installed directory; walking them as separate
		// roots would count their bytes twice (double-counting ban).
		if (lockPath.includes("/node_modules/")) continue;
		const raw = packages[lockPath] ?? {};
		const name = lockPath.slice("node_modules/".length);
		const entry: ClosureEntry = {
			lockPath,
			name,
			version: typeof raw.version === "string" ? raw.version : "",
			optional: raw.optional === true,
			os: Array.isArray(raw.os) ? (raw.os as string[]) : undefined,
			cpu: Array.isArray(raw.cpu) ? (raw.cpu as string[]) : undefined,
			firstParty: name.startsWith(FIRST_PARTY_SCOPE),
		};
		if (name === PRIMARY_PAYLOAD_PACKAGE) {
			primaryPayload = entry;
			continue;
		}
		if (entry.firstParty) {
			workspaceLinks.push(entry);
			continue;
		}
		if (!closureEntryMatchesPlatform(entry, options.platform, options.arch)) {
			foreignOptional.push(entry);
			continue;
		}
		measure.push(entry);
	}
	// Deterministic iteration order for reproducible artifacts.
	measure.sort((left, right) => left.name.localeCompare(right.name));
	return { measure, workspaceLinks, primaryPayload, foreignOptional };
}

/**
 * Classify an `npm pack` file list into contract classes: the npm launcher
 * (C1), the remaining runtime payload (C2), and the compiled-launcher file
 * (build:binary-only, excluded from the npm-variant total).
 */
export function classifyPackFiles(
	files: readonly PackFile[],
): PackClassification {
	let launcherBytes = 0;
	let launcherFiles = 0;
	let payloadBytes = 0;
	let payloadFiles = 0;
	let compiledBytes = 0;
	let compiledFiles = 0;
	for (const file of files) {
		if (file.path === UPSTREAM_NPM_LAUNCHER) {
			launcherBytes += file.size;
			launcherFiles += 1;
		} else if (file.path === UPSTREAM_COMPILED_LAUNCHER) {
			compiledBytes += file.size;
			compiledFiles += 1;
		} else {
			payloadBytes += file.size;
			payloadFiles += 1;
		}
	}
	return {
		launcher: { bytes: launcherBytes, files: launcherFiles, symlinks: 0 },
		runtimePayload: { bytes: payloadBytes, files: payloadFiles, symlinks: 0 },
		compiledLauncherVariant: {
			bytes: compiledBytes,
			files: compiledFiles,
			symlinks: 0,
		},
	};
}

function sumWalkTotal(left: WalkTotal, right: WalkTotal): WalkTotal {
	return {
		bytes: left.bytes + right.bytes,
		files: left.files + right.files,
		symlinks: left.symlinks + right.symlinks,
	};
}

interface PackOutput {
	readonly label: string;
	readonly cwd: string;
	readonly files: readonly PackFile[];
	readonly listingSha256: string;
}

/**
 * Parse an `npm pack --dry-run --json` stdout document into a pack list.
 * The captured listing is the mechanical authority for shipped-package bytes;
 * per-scan distributions re-sum it rather than re-invoking npm.
 */
export function parsePackListing(
	label: string,
	cwd: string,
	stdout: string,
): PackOutput {
	// npm emits a top-level array (one manifest per pack invocation); accept
	// both the array form and a bare manifest object.
	const parsed = JSON.parse(stdout) as
		| { files?: { path?: unknown; size?: unknown }[] }[]
		| { files?: { path?: unknown; size?: unknown }[] };
	const manifest = Array.isArray(parsed) ? parsed[0] : parsed;
	const files: PackFile[] = [];
	for (const entry of manifest?.files ?? []) {
		if (typeof entry.path !== "string" || typeof entry.size !== "number") {
			throw new HarnessFailure(
				"footprint:npm-pack",
				`npm pack listing for ${label} has a malformed entry: ${JSON.stringify(entry)}`,
			);
		}
		files.push({ path: entry.path, size: entry.size });
	}
	if (files.length === 0) {
		throw new HarnessFailure(
			"footprint:npm-pack",
			`npm pack listing for ${label} is empty`,
		);
	}
	return {
		label,
		cwd,
		files,
		listingSha256: createHash("sha256").update(stdout).digest("hex"),
	};
}

function classRecord(
	values: readonly number[],
	files: number,
	emptyReason?: string,
): ClassRecord {
	return {
		bytes: distribution(values),
		files,
		...(emptyReason === undefined ? {} : { emptyReason }),
	};
}

interface ScanFileCounts {
	readonly launcherFiles: number;
	readonly runtimePayloadFiles: number;
	readonly shippedDependencyFiles: number;
}

function sideRecordFromScans(
	implementation: "rust" | "upstream",
	scanCommands: readonly CommandRecord[],
	scanAuthorities: readonly AuthorityRecord[],
	scans: readonly SideScan[],
	fileCounts: ScanFileCounts,
	symlinks: number,
	externalPrerequisites: readonly ExternalPrerequisite[],
	excluded: readonly { label: string; reason: string }[],
	emptyReasons: { "shipped-dependencies"?: string } = {},
): SideRecord {
	return {
		implementation,
		commands: scanCommands,
		authorities: scanAuthorities,
		classes: {
			launcher: classRecord(
				scans.map((scan) => scan.launcher),
				fileCounts.launcherFiles,
			),
			"runtime-payload": classRecord(
				scans.map((scan) => scan.runtimePayload),
				fileCounts.runtimePayloadFiles,
			),
			"shipped-dependencies": classRecord(
				scans.map((scan) => scan.shippedDependencies),
				fileCounts.shippedDependencyFiles,
				emptyReasons["shipped-dependencies"],
			),
		},
		total: {
			bytes: distribution(
				scans.map(
					(scan) =>
						scan.launcher + scan.runtimePayload + scan.shippedDependencies,
				),
			),
		},
		symlinks,
		externalPrerequisites,
		excluded,
	};
}

async function resolveHostArtifact(
	plan: TargetPlan,
	commands: CommandRecord[],
): Promise<HostArtifact> {
	const hostDir = join(
		FOOTPRINT_BUILD_ROOT,
		".staging-host/host",
		plan.rustTarget,
	);
	const compiled = join(hostDir, plan.hostBinaryName);
	if (existsSync(compiled)) {
		return { kind: "compiled", binaryPath: compiled };
	}
	const bundle = join(hostDir, plan.hostBundleName);
	if (!existsSync(bundle)) {
		throw new HarnessFailure(
			"build-artifact",
			`extension host build produced neither ${compiled} nor ${bundle}`,
		);
	}
	const runtimeDestination = join(
		FOOTPRINT_BUILD_ROOT,
		"bun-runtime",
		plan.bunRuntimeName,
	);
	status(
		"compiled sidecar unavailable; provisioning pinned Bun runtime for the fallback variant",
	);
	const runtimePath = await provisionBunRuntime({
		plan,
		destination: runtimeDestination,
		cacheDir: RUNTIME_CACHE,
		fs: realFs,
	});
	commands.push({
		label: "Pinned Bun runtime provisioning (fallback variant)",
		cwd: REPOSITORY_ROOT,
		argv: [
			"(release/runtime.ts provisionBunRuntime)",
			plan.bunTarget,
			RUNTIME_CACHE,
			runtimeDestination,
		],
	});
	return { kind: "runtime-bundle", runtimePath, scriptPath: bundle };
}

async function measureRustSide(): Promise<SideMeasurement> {
	const triple = localRustTriple();
	if (triple === undefined) {
		throw new HarnessFailure(
			"host-validation",
			`no release target for ${platform()} ${arch()}; footprint measurement requires a supported host triple`,
		);
	}
	const plan = planFor(triple);
	const cargo = requiredExecutable("cargo");
	const bun = requiredExecutable("bun");
	const commands: CommandRecord[] = [];
	await runCheckedCommand(
		{
			label: "Rust pi release build",
			cwd: REPOSITORY_ROOT,
			argv: [cargo, "build", "-p", "pi", "--release", "--locked"],
		},
		commands,
	);
	if (!existsSync(RUST_BINARY)) {
		throw new HarnessFailure(
			"build-artifact",
			`cargo build did not produce ${RUST_BINARY}`,
		);
	}
	await runCheckedCommand(
		{
			label: "Rust extension host release build",
			cwd: REPOSITORY_ROOT,
			argv: [
				bun,
				"run",
				"build:extension-host",
				"--target",
				plan.rustTarget,
				"--out",
				FOOTPRINT_BUILD_ROOT,
			],
		},
		commands,
	);
	const host = await resolveHostArtifact(plan, commands);
	const version = (
		readJsonFile(join(REPOSITORY_ROOT, "package.json")) as { version?: string }
	).version;
	if (typeof version !== "string") {
		throw new HarnessFailure(
			"prerequisite",
			"workspace package.json has no version string",
		);
	}
	rmSync(FOOTPRINT_STAGING_ROOT, { recursive: true, force: true });
	const sourceDateEpoch = Math.floor(Date.now() / 1000);
	const assembly = await assembleRelease(FOOTPRINT_STAGING_ROOT, {
		plan,
		version,
		piBinaryPath: RUST_BINARY,
		repoRoot: REPOSITORY_ROOT,
		host,
		fs: realFs,
		sourceDateEpoch,
		compatibilityVersion: HOST_COMPATIBILITY_VERSION,
		protocolVersion: HOST_PROTOCOL_VERSION,
		createdAt: new Date(sourceDateEpoch * 1000).toISOString(),
		docsSource: join(REPOSITORY_ROOT, "docs"),
		assetsSource: join(REPOSITORY_ROOT, "crates", "pi", "assets"),
	});
	const manifestPath = join(assembly.stagingDir, "release.json");
	const authorities: AuthorityRecord[] = [
		{
			label: "assembled release manifest (authoritative file list)",
			path: manifestPath,
			sha256: sha256BytesOf(manifestPath),
		},
	];
	const scans: SideScan[] = [];
	let payloadFileCount = 0;
	let symlinks = 0;
	for (let index = 0; index < FOOTPRINT_SCAN_SAMPLES; index += 1) {
		let launcherBytes = 0;
		let payloadBytes = 0;
		let payloadFiles = 0;
		let scanSymlinks = 0;
		for (const entry of readdirSync(assembly.stagingDir, {
			withFileTypes: true,
		})) {
			const child = join(assembly.stagingDir, entry.name);
			if (entry.isSymbolicLink()) {
				scanSymlinks += 1;
				continue;
			}
			const isLauncher = entry.isFile() && entry.name === plan.piBinaryName;
			if (entry.isFile()) {
				const size = lstatSync(child).size;
				if (isLauncher) launcherBytes += size;
				else {
					payloadBytes += size;
					payloadFiles += 1;
				}
				continue;
			}
			const walked = walkApparentBytes(child);
			scanSymlinks += walked.symlinks;
			if (isLauncher) launcherBytes += walked.bytes;
			else {
				payloadBytes += walked.bytes;
				payloadFiles += walked.files;
			}
		}
		symlinks = scanSymlinks;
		payloadFileCount = payloadFiles;
		scans.push({
			launcher: launcherBytes,
			runtimePayload: payloadBytes,
			shippedDependencies: 0,
		});
	}
	const record = sideRecordFromScans(
		"rust",
		commands,
		authorities,
		scans,
		{
			launcherFiles: 1,
			runtimePayloadFiles: payloadFileCount,
			shippedDependencyFiles: 0,
		},
		symlinks,
		[],
		[],
		{
			"shipped-dependencies":
				"empty by construction: dependencies are statically linked into the launcher; the extension-host sidecar ships inside the runtime payload",
		},
	);
	const last = scans.at(-1);
	if (last === undefined)
		throw new HarnessFailure("statistics", "rust scan set is empty");
	rmSync(FOOTPRINT_STAGING_ROOT, { recursive: true, force: true });
	return {
		record,
		scanTotals: scans.map((scan) => scan.launcher + scan.runtimePayload),
		classSeries: classSeriesOf(scans),
		launcherBytes: last.launcher,
		totalBytes: last.launcher + last.runtimePayload,
		compiledLauncherBytes: 0,
	};
}

async function measureUpstreamSide(): Promise<SideMeasurement> {
	if (!existsSync(CODING_AGENT_DIR)) {
		throw new HarnessFailure(
			"prerequisite",
			`pinned reference checkout is missing: ${CODING_AGENT_DIR} (expected .references/pi at ${REFERENCE_PIN})`,
		);
	}
	const npm = requiredExecutable("npm");
	const commands: CommandRecord[] = [];
	const codingAgentManifest = readJsonFile(
		join(CODING_AGENT_DIR, "package.json"),
	) as {
		name?: string;
		bin?: Record<string, string>;
	};
	if (codingAgentManifest.name !== PRIMARY_PAYLOAD_PACKAGE) {
		throw new HarnessFailure(
			"prerequisite",
			`reference package name is ${String(codingAgentManifest.name)}, expected ${PRIMARY_PAYLOAD_PACKAGE}`,
		);
	}
	const declaredLauncher = codingAgentManifest.bin?.pi;
	if (declaredLauncher !== UPSTREAM_NPM_LAUNCHER) {
		throw new HarnessFailure(
			"prerequisite",
			`reference package bin.pi is ${String(declaredLauncher)}, contract expects ${UPSTREAM_NPM_LAUNCHER}`,
		);
	}
	const authorities: AuthorityRecord[] = [
		authorityRecord(
			"reference coding-agent package manifest",
			join(CODING_AGENT_DIR, "package.json"),
		),
		authorityRecord(
			"upstream installer install lock (dependency closure authority)",
			join(CODING_AGENT_DIR, "install-lock/package-lock.json"),
		),
		authorityRecord(
			"upstream installer lock root (engines authority)",
			join(CODING_AGENT_DIR, "install-lock/package.json"),
		),
	];
	const primaryPack = parsePackListing(
		"reference coding-agent pack list",
		CODING_AGENT_DIR,
		(
			await runCheckedCommand(
				{
					label:
						"TypeScript reference package payload listing (npm pack --dry-run --json)",
					cwd: CODING_AGENT_DIR,
					argv: [npm, "pack", "--dry-run", "--json"],
				},
				commands,
			)
		).stdout,
	);
	authorities.push(listingAuthorityRecord(primaryPack));
	const lock = readJsonFile(
		join(CODING_AGENT_DIR, "install-lock/package-lock.json"),
	) as {
		packages?: Record<string, Record<string, unknown>>;
	};
	const closure = planUpstreamClosure(lock, {
		platform: platform(),
		arch: arch(),
	});
	if (closure.primaryPayload === undefined) {
		throw new HarnessFailure(
			"prerequisite",
			`install-lock closure does not contain ${PRIMARY_PAYLOAD_PACKAGE}`,
		);
	}
	// First-party workspace links: resolve through the on-disk symlink (the
	// workspace mapping), verify name@version against the lock, then measure
	// each package's published payload via its own pack list.
	const firstPartyPacks: PackOutput[] = [];
	for (const entry of closure.workspaceLinks) {
		const linkPath = join(REFERENCE_ROOT, entry.lockPath);
		if (!lstatSync(linkPath).isSymbolicLink()) {
			throw new HarnessFailure(
				"prerequisite",
				`first-party closure entry ${entry.name} is not a workspace symlink at ${linkPath}; a clean install-lock install would fetch it from the registry`,
			);
		}
		const workspaceDir = resolve(dirname(linkPath), readlinkSync(linkPath));
		const manifest = readJsonFile(join(workspaceDir, "package.json")) as {
			name?: string;
			version?: string;
		};
		if (manifest.name !== entry.name || manifest.version !== entry.version) {
			throw new HarnessFailure(
				"prerequisite",
				`workspace package at ${workspaceDir} is ${String(manifest.name)}@${String(manifest.version)}, lock pins ${entry.name}@${entry.version}`,
			);
		}
		const pack = parsePackListing(
			entry.name,
			workspaceDir,
			(
				await runCheckedCommand(
					{
						label: `TypeScript first-party dependency payload listing (${entry.name})`,
						cwd: workspaceDir,
						argv: [npm, "pack", "--dry-run", "--json"],
					},
					commands,
				)
			).stdout,
		);
		authorities.push(listingAuthorityRecord(pack));
		firstPartyPacks.push(pack);
	}
	// Third-party closure: installed directories walked per scan. A missing
	// directory that npm would skip on another platform is excluded with its
	// name; any other missing directory is a hard failure.
	const missingForeign: ClosureEntry[] = [];
	for (const entry of closure.measure) {
		const dir = join(REFERENCE_ROOT, entry.lockPath);
		if (existsSync(dir)) continue;
		if (
			entry.optional &&
			!closureEntryMatchesPlatform(entry, platform(), arch())
		) {
			missingForeign.push(entry);
		} else {
			throw new HarnessFailure(
				"prerequisite",
				`closure package ${entry.name}@${entry.version} is not installed at ${dir}; run npm ci in .references/pi first`,
			);
		}
	}
	const foreignExcluded = [...closure.foreignOptional, ...missingForeign].map(
		(entry) => `${entry.name}@${entry.version}`,
	);
	// Interpreter prerequisite: recorded as context, never summed.
	const installLockRoot = readJsonFile(
		join(CODING_AGENT_DIR, "install-lock/package.json"),
	) as {
		engines?: { node?: string };
	};
	const externalPrerequisites: ExternalPrerequisite[] = [
		await nodePrerequisite(installLockRoot?.engines?.node),
	];
	const scans: SideScan[] = [];
	let launcherFileCount = 0;
	let payloadFileCount = 0;
	let dependencyFileCount = 0;
	let symlinks = 0;
	for (let index = 0; index < FOOTPRINT_SCAN_SAMPLES; index += 1) {
		const primary = classifyPackFiles(primaryPack.files);
		let dependencies: WalkTotal = { bytes: 0, files: 0, symlinks: 0 };
		// Dependency packs carry no launcher: every listed byte is shipped
		// dependency payload, so sum the pack listing plainly rather than
		// applying the primary package's launcher carve-out.
		for (const pack of firstPartyPacks) {
			dependencies = sumWalkTotal(dependencies, {
				bytes: pack.files.reduce((total, file) => total + file.size, 0),
				files: pack.files.length,
				symlinks: 0,
			});
		}
		for (const entry of closure.measure) {
			const dir = join(REFERENCE_ROOT, entry.lockPath);
			if (!existsSync(dir)) continue;
			dependencies = sumWalkTotal(dependencies, walkApparentBytes(dir));
		}
		launcherFileCount = primary.launcher.files;
		payloadFileCount = primary.runtimePayload.files;
		dependencyFileCount = dependencies.files;
		symlinks = dependencies.symlinks;
		scans.push({
			launcher: primary.launcher.bytes,
			runtimePayload: primary.runtimePayload.bytes,
			shippedDependencies: dependencies.bytes,
		});
	}
	// Invariant 5: the workspace mapping symlinks themselves are counted and
	// reported (zero apparent bytes, never followed). Every first-party link
	// was verified to be a symlink by the prerequisite checks above.
	symlinks += closure.workspaceLinks.length;
	const excluded: { label: string; reason: string }[] = [
		{
			label: `compiled launcher file (${UPSTREAM_COMPILED_LAUNCHER})`,
			reason:
				"produced only by upstream build:binary; the npm publish flow (prepublishOnly -> build) never ships it; reported as the compiled-launcher variant under D7 naming",
		},
		{
			label: `${PRIMARY_PAYLOAD_PACKAGE} closure entry`,
			reason:
				"double-counting ban: the primary payload is measured exactly once as launcher + runtime payload",
		},
		{
			label: `foreign-platform optional dependencies (${foreignExcluded.length}: ${foreignExcluded.slice(0, 8).join(", ")}${foreignExcluded.length > 8 ? ", …" : ""})`,
			reason:
				"npm os/cpu filtering skips these on this platform; they are never installed here",
		},
		{
			label:
				"installer-generated metadata (npm .package-lock.json, .bin links)",
			reason:
				"generated by the installer, not shipped by the distribution; the Rust release.json IS counted because the release archive ships it",
		},
	];
	const record = sideRecordFromScans(
		"upstream",
		commands,
		authorities,
		scans,
		{
			launcherFiles: launcherFileCount,
			runtimePayloadFiles: payloadFileCount,
			shippedDependencyFiles: dependencyFileCount,
		},
		symlinks,
		externalPrerequisites,
		excluded,
	);
	const last = scans.at(-1);
	if (last === undefined)
		throw new HarnessFailure("statistics", "upstream scan set is empty");
	return {
		record,
		scanTotals: scans.map(
			(scan) => scan.launcher + scan.runtimePayload + scan.shippedDependencies,
		),
		classSeries: classSeriesOf(scans),
		launcherBytes: last.launcher,
		totalBytes: last.launcher + last.runtimePayload + last.shippedDependencies,
		compiledLauncherBytes: classifyPackFiles(primaryPack.files)
			.compiledLauncherVariant.bytes,
	};
}

async function nodePrerequisite(
	constraint: string | undefined,
): Promise<ExternalPrerequisite> {
	const nodePath = Bun.which("node");
	if (nodePath === null) {
		return {
			name: "node",
			constraint,
			onMachineVersion: undefined,
			onMachineBytes: undefined,
			treatment: "context-only, excluded from every total",
		};
	}
	const versionResult = Bun.spawnSync([nodePath, "--version"], {
		stdout: "pipe",
		stderr: "pipe",
	});
	const onMachineVersion =
		versionResult.exitCode === 0
			? new TextDecoder().decode(versionResult.stdout).trim()
			: undefined;
	let onMachineBytes: number | undefined;
	try {
		onMachineBytes = lstatSync(realpathSync(nodePath)).size;
	} catch {
		onMachineBytes = undefined;
	}
	return {
		name: "node",
		constraint,
		onMachineVersion,
		onMachineBytes,
		treatment: "context-only, excluded from every total",
	};
}

function noiseDistribution(
	label: string,
	values: readonly number[],
): NoisyDistribution {
	const stats = distribution(values);
	return {
		label,
		count: stats.count,
		median: stats.median,
		stddev: stats.stddev,
		relativeSpread: stats.relativeSpread,
	};
}

function writeArtifact(): void {
	mkdirSync(dirname(ARTIFACT_PATH), { recursive: true });
	artifact.generatedAt = new Date().toISOString();
	writeFileSync(
		ARTIFACT_PATH,
		`${JSON.stringify(artifact, null, 2)}\n`,
		"utf8",
	);
}

async function main(): Promise<void> {
	status("measuring Rust distribution footprint");
	const rust = await measureRustSide();
	artifact.sides.rust = rust.record;
	status("measuring upstream reference footprint");
	const upstream = await measureUpstreamSide();
	artifact.sides.upstream = upstream.record;
	artifact.machine = {
		os: platform(),
		arch: arch(),
		bun: Bun.version,
		referencePin: REFERENCE_PIN,
	};
	const rustTotal = rust.record.total.bytes.median;
	const upstreamTotal = upstream.record.total.bytes.median;
	artifact.comparison = {
		npmVariant: {
			rustTotalBytes: rustTotal,
			upstreamTotalBytes: upstreamTotal,
			upstreamOverRust: rustTotal === 0 ? 0 : upstreamTotal / rustTotal,
		},
		launcherOnlyContext: {
			rustLauncherBytes: rust.launcherBytes,
			upstreamCompiledLauncherBytes: upstream.compiledLauncherBytes,
			note: "executable artifact size comparison (D7 naming); not an installed-footprint claim",
		},
	};
	requireQuiet([
		noiseDistribution("rust installed-footprint total", rust.scanTotals),
		noiseDistribution("rust launcher", rust.classSeries.launcher),
		noiseDistribution("rust runtime payload", rust.classSeries.runtimePayload),
		noiseDistribution(
			"rust shipped dependencies",
			rust.classSeries.shippedDependencies,
		),
		noiseDistribution(
			"upstream npm-variant installed-footprint total",
			upstream.scanTotals,
		),
		noiseDistribution("upstream npm launcher", upstream.classSeries.launcher),
		noiseDistribution(
			"upstream runtime payload",
			upstream.classSeries.runtimePayload,
		),
		noiseDistribution(
			"upstream shipped dependencies",
			upstream.classSeries.shippedDependencies,
		),
	]);
	artifact.pass = artifact.blockers.length === 0;
	writeArtifact();
	process.stdout.write(
		`footprint accounting complete; artifact: ${ARTIFACT_PATH}\n` +
			`  rust total:     ${rustTotal} bytes\n` +
			`  upstream total: ${upstreamTotal} bytes (npm variant, interpreter excluded)\n` +
			`  upstream/rust:  ${rustTotal === 0 ? "n/a" : `${(upstreamTotal / rustTotal).toFixed(2)}x`}\n`,
	);
}

if (import.meta.main) {
	try {
		await main();
	} catch (error) {
		const failure = error instanceof Error ? error : new Error(String(error));
		if (failure instanceof NoiseRejection) {
			artifact.pass = false;
			writeArtifact();
			process.stderr.write(
				`footprint run rejected as noise:\n${formatNoiseRejection(failure.noisy)}\nartifact: ${ARTIFACT_PATH}\n`,
			);
			process.exitCode = 2;
		} else {
			const stage =
				failure instanceof HarnessFailure ? failure.stage : "unexpected";
			artifact.pass = false;
			artifact.blockers = [
				...artifact.blockers,
				`${stage}: ${failure.message}`,
			];
			artifact.failure = { stage, message: failure.message };
			writeArtifact();
			process.stderr.write(
				`footprint run failed:\n${failure.message}\nartifact: ${ARTIFACT_PATH}\n`,
			);
			process.exitCode = 1;
		}
	}
}
