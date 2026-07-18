/**
 * Release-script argv parsing, isolated from target modeling and from I/O.
 *
 * Environment access is limited to reading the `SOURCE_DATE_EPOCH` default,
 * which is CLI behavior; no other ambient state is read.
 */

import { resolve } from "node:path";

import type { TargetPlan } from "./targets.ts";
import { planFor, RUST_TARGETS } from "./targets.ts";

/**
 * Parsed release-script argv: every flag accepted by `package-release.ts`
 * surfaces here so callers (and tests) can inspect the resolved plan without
 * re-running `process.argv`.
 */
export interface ReleaseArgs {
	/** Resolved target plan (always set when parsing succeeds). */
	readonly plan: TargetPlan;
	/** Output directory (absolute). Defaults to `<cwd>/dist/release`. */
	readonly outDir: string;
	/** When true, skip cargo + host compile and assemble with stubs. */
	readonly dryRun: boolean;
	/** When true, skip cargo build only (still compile host). */
	readonly noCargo: boolean;
	/** When true, skip host unit tests (still typecheck + compile). */
	readonly skipHostTests: boolean;
	/** When true, run the host `hello` handshake against the sidecar. */
	readonly handshake: boolean;
	/** Override the SOURCE_DATE_EPOCH timestamp used for archive mtimes. */
	readonly sourceDateEpoch: string;
}

/** Sentinel thrown by {@link parseReleaseArgs} when `--help` is requested. */
export class ArgvHelpRequested extends Error {
	constructor() {
		super("help requested");
		this.name = "ArgvHelpRequested";
	}
}

/** Thrown by {@link parseReleaseArgs} when `--target` is missing. */
export class MissingTargetError extends Error {
	constructor() {
		super(`Missing required --target <triple>. One of: ${RUST_TARGETS.join(", ")}`);
		this.name = "MissingTargetError";
	}
}

/** Thrown by {@link parseReleaseArgs} on an unknown flag. */
export class UnknownArgError extends Error {
	readonly arg: string;
	constructor(arg: string) {
		super(`Unknown argument: ${arg}`);
		this.name = "UnknownArgError";
		this.arg = arg;
	}
}

/** Thrown when `--source-date-epoch` is not a base-10 integer. */
export class InvalidSourceDateEpochError extends Error {
	readonly value: string;
	constructor(value: string) {
		super(`Invalid --source-date-epoch (must be base-10 integer seconds): ${value}`);
		this.name = "InvalidSourceDateEpochError";
		this.value = value;
	}
}

/** Thrown when a value-taking flag has no following non-flag value. */
export class MissingArgValueError extends Error {
	readonly flag: string;
	constructor(flag: string) {
		super(`Flag ${flag} requires a value (next argument missing or starts with '-').`);
		this.name = "MissingArgValueError";
		this.flag = flag;
	}
}

/** Non-negative base-10 integer seconds regex, used for SOURCE_DATE_EPOCH. */
const DECIMAL_INTEGER_SECONDS = /^\d+$/;

/**
 * Consume the value following a value-taking flag. Throws if the value is
 * missing or itself looks like another flag (starts with `-`).
 */
function requireFlagValue(argv: readonly string[], i: number, flag: string): string {
	const next = argv[i + 1];
	if (next === undefined || next.length === 0 || next.startsWith("-")) {
		throw new MissingArgValueError(flag);
	}
	return next;
}

/**
 * Parse the release script argv array.
 *
 * @throws {@link MissingTargetError} when `--target` is absent.
 * @throws {@link MissingArgValueError} when a value-taking flag has no value.
 * @throws {@link InvalidTargetError} via {@link planFor} for unsupported triples.
 * @throws {@link InvalidSourceDateEpochError} for non-decimal SOURCE_DATE_EPOCH.
 * @throws {@link UnknownArgError} on unrecognised flags.
 * @throws {@link ArgvHelpRequested} when `--help` / `-h` is present.
 */
export function parseReleaseArgs(
	argv: readonly string[],
	cwd: string = process.cwd(),
	sourceDateEpochEnv: string | undefined = process.env.SOURCE_DATE_EPOCH,
): ReleaseArgs {
	let target: string | undefined;
	let outDir: string | undefined;
	let dryRun = false;
	let noCargo = false;
	let skipHostTests = false;
	let handshake = true;
	let sourceDateEpoch = sourceDateEpochEnv ?? "0";

	for (let i = 0; i < argv.length; i++) {
		const arg = argv[i];
		if (arg === undefined) continue;
		switch (arg) {
			case "--target": {
				target = requireFlagValue(argv, i, "--target");
				i += 1;
				break;
			}
			case "--out":
			case "--out-dir": {
				outDir = requireFlagValue(argv, i, arg);
				i += 1;
				break;
			}
			case "--source-date-epoch": {
				sourceDateEpoch = requireFlagValue(argv, i, arg);
				i += 1;
				break;
			}
			case "--dry-run": {
				dryRun = true;
				break;
			}
			case "--no-cargo": {
				noCargo = true;
				break;
			}
			case "--skip-host-tests": {
				skipHostTests = true;
				break;
			}
			case "--no-handshake": {
				handshake = false;
				break;
			}
			case "--help":
			case "-h": {
				throw new ArgvHelpRequested();
			}
			default: {
				throw new UnknownArgError(arg);
			}
		}
	}

	if (target === undefined) {
		throw new MissingTargetError();
	}
	if (!DECIMAL_INTEGER_SECONDS.test(sourceDateEpoch)) {
		throw new InvalidSourceDateEpochError(sourceDateEpoch);
	}
	return {
		plan: planFor(target),
		outDir: resolve(cwd, outDir ?? "dist/release"),
		dryRun,
		noCargo,
		skipHostTests,
		handshake,
		sourceDateEpoch,
	};
}
