/**
 * Cross-target release plan: maps one of the five supported Rust target
 * triples to its Bun host compile target, archive format, and the binary
 * names that the release script must assemble beside each other.
 *
 * Pure module — no I/O, no environment access. Every function is unit
 * testable in isolation.
 */

/**
 * The five Rust target triples the master plan supports for release.
 *
 * (Verification check 13: `x86_64-unknown-linux-gnu`,
 * `aarch64-unknown-linux-gnu`, `x86_64-apple-darwin`,
 * `aarch64-apple-darwin`, `x86_64-pc-windows-msvc`.)
 */
export const RUST_TARGETS = [
	"x86_64-unknown-linux-gnu",
	"aarch64-unknown-linux-gnu",
	"x86_64-apple-darwin",
	"aarch64-apple-darwin",
	"x86_64-pc-windows-msvc",
] as const;

/** Rust target triple. */
export type RustTarget = (typeof RUST_TARGETS)[number];

/** Archive container chosen per platform. */
export type ArchiveKind = "tar.gz" | "zip";

/** OS family derived from the Rust triple. */
export type OsFamily = "linux" | "darwin" | "windows";

/** CPU architecture derived from the Rust triple. */
export type Arch = "x86_64" | "aarch64";

/**
 * A fully-resolved release target: every downstream phase (cargo build, host
 * compile, archive, naming, manifest) reads from this object so there is a
 * single source of truth the tests can pin.
 */
export interface TargetPlan {
	/** Rust triple passed to `cargo build --target`. */
	readonly rustTarget: RustTarget;
	/**
	 * Bun compile target (`bun-<os>-<arch>[-baseline]`). x86_64 targets use
	 * `-baseline` to avoid Bun's AVX2 floor; arm64 targets are standard.
	 */
	readonly bunTarget: string;
	/** OS family. */
	readonly os: OsFamily;
	/** CPU architecture. */
	readonly arch: Arch;
	/** Archive container. Windows uses zip; everything else tar.gz. */
	readonly archive: ArchiveKind;
	/** `true` when the binaries carry an `.exe` suffix. */
	readonly windows: boolean;
	/** `true` when the triple targets Apple Darwin. */
	readonly darwin: boolean;
	/** Released Rust binary name (`pi` or `pi.exe`). */
	readonly piBinaryName: string;
	/** Released compiled-host binary name (`pi-extension-host` or `.exe`). */
	readonly hostBinaryName: string;
	/** Released Bun runtime name used by the fallback path. */
	readonly bunRuntimeName: string;
	/** Released host JavaScript bundle used by the fallback path. */
	readonly hostBundleName: string;
	/** Directory prefix inside the archive (`pi-<os>-<arch>[-base]`). */
	readonly archiveDir: string;
}

/** Suffix appended to x86_64 Bun targets to dodge Bun's AVX2 floor. */
const X64_BASELINE_SUFFIX = "baseline";

function buildPlan(triple: RustTarget): TargetPlan {
	const windows = triple.endsWith("-windows-msvc");
	const darwin = triple.includes("-apple-darwin");
	const os: OsFamily = windows ? "windows" : darwin ? "darwin" : "linux";
	const arch: Arch = triple.startsWith("aarch64") ? "aarch64" : "x86_64";
	const exe = windows ? ".exe" : "";
	const bunArch = arch === "aarch64" ? "arm64" : "x64";
	const bunOs = os === "darwin" ? "darwin" : os === "windows" ? "windows" : "linux";
	const baseline = arch === "x86_64" ? `-${X64_BASELINE_SUFFIX}` : "";
	const archiveDirArch = arch === "aarch64" ? "arm64" : "x64";
	const archiveDirBase = arch === "x86_64" ? "-base" : "";
	return {
		rustTarget: triple,
		bunTarget: `bun-${bunOs}-${bunArch}${baseline}`,
		os,
		arch,
		archive: windows ? "zip" : "tar.gz",
		windows,
		darwin,
		piBinaryName: `pi${exe}`,
		hostBinaryName: `pi-extension-host${exe}`,
		bunRuntimeName: `bun${exe}`,
		hostBundleName: "pi-extension-host.js",
		archiveDir: `pi-${os}-${archiveDirArch}${archiveDirBase}`,
	};
}

/** Map every supported triple to its plan, in declaration order. */
export const TARGET_PLANS: readonly TargetPlan[] = RUST_TARGETS.map(buildPlan);

/** Static lookup table backing {@link planFor} / {@link isSupportedTarget}. */
const PLAN_BY_TRIPLE: Record<RustTarget, TargetPlan> = Object.fromEntries(
	TARGET_PLANS.map((p) => [p.rustTarget, p]),
) as Record<RustTarget, TargetPlan>;

/**
 * Resolve a Rust triple to its `TargetPlan`.
 *
 * @throws {@link InvalidTargetError} if the triple is not supported.
 */
export function planFor(triple: string): TargetPlan {
	if (!Object.hasOwn(PLAN_BY_TRIPLE, triple as RustTarget)) {
		throw new InvalidTargetError(triple);
	}
	return PLAN_BY_TRIPLE[triple as RustTarget];
}

/** Error raised when a target triple is not supported. */
export class InvalidTargetError extends Error {
	readonly input: string;
	constructor(input: string) {
		super(
			`Unsupported release target: ${input}. Supported triples: ${RUST_TARGETS.join(", ")}`,
		);
		this.name = "InvalidTargetError";
		this.input = input;
	}
}

/** Type guard: true when `triple` is one of the supported release targets. */
export function isSupportedTarget(triple: string): triple is RustTarget {
	return Object.hasOwn(PLAN_BY_TRIPLE, triple as RustTarget);
}

/**
 * Deterministic archive base name: `pi-<version>-<archiveDir>.<ext>`.
 * Version is the published workspace version (no leading `v`).
 */
export function archiveName(version: string, plan: TargetPlan): string {
	return `pi-${version}-${plan.archiveDir}.${plan.archive === "zip" ? "zip" : "tar.gz"}`;
}

/** Deterministic checksum sidecar name: `<archive>.sha256`. */
export function checksumName(archiveBaseName: string): string {
	return `${archiveBaseName}.sha256`;
}
