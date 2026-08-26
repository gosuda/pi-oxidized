/**
 * Injectable command-runner and filesystem seams used by the release script.
 *
 * The release script never calls `Bun.spawn` or `node:fs` directly: every
 * external interaction flows through these interfaces so unit tests can swap
 * in fakes without forking subprocesses or touching the real filesystem.
 */

import { spawn } from "node:child_process";
import {
	chmod,
	copyFile,
	cp,
	mkdir,
	readFile,
	readdir,
	rm,
	stat,
	writeFile,
} from "node:fs/promises";
import { posix, win32 } from "node:path";

/** Result of running one command via the runner. */
export interface RunResult {
	/** Exit code. `0` indicates success. */
	readonly exitCode: number;
	/** Combined UTF-8 stdout. */
	readonly stdout: string;
	/** Combined UTF-8 stderr. */
	readonly stderr: string;
}

/** Options accepted by {@link CommandRunner.run}. */
export interface RunOptions {
	/** Working directory. */
	readonly cwd?: string;
	/** Environment variables layered on top of the current process env. */
	readonly env?: Readonly<Record<string, string>>;
	/** Stdio passed to the child's stdin. Defaults to ignoring stdin. */
	readonly stdin?: string;
	/** Throw on nonzero exit instead of returning. */
	readonly rejectOnError?: boolean;
	/** Positive deadline in milliseconds. On expiry the process tree is killed. */
	readonly timeoutMs?: number;
	/**
	 * Hard combined retention ceiling for stdout+stderr. Retention stops at the
	 * cap, readers are destroyed, the process tree is terminated, and the runner
	 * throws without awaiting an escaped pipe holder.
	 */
	readonly maxOutputBytes?: number;
}

/**
 * Command runner seam. The default implementation (`SpawnRunner`) shells out
 * via `node:child_process`; tests inject a fake that records the call and
 * returns a canned result without spawning anything.
 */
export interface CommandRunner {
	run(command: string, args: readonly string[], options?: RunOptions): Promise<RunResult>;
}

/** Convert a non-zero exit into a thrown error when `rejectOnError` is set. */
export class CommandFailedError extends Error {
	readonly command: string;
	readonly args: readonly string[];
	readonly exitCode: number;
	readonly stderr: string;
	constructor(res: RunResult, command: string, args: readonly string[]) {
		super(
			`Command "${command}" failed with exit code ${res.exitCode}: ${res.stderr.slice(0, 2000)}`,
		);
		this.name = "CommandFailedError";
		this.command = command;
		this.args = args;
		this.exitCode = res.exitCode;
		this.stderr = res.stderr;
	}
}

/** Raised after combined stdout+stderr exceeds {@link RunOptions.maxOutputBytes}. */
export class CommandOutputLimitError extends Error {
	readonly command: string;
	readonly args: readonly string[];
	readonly maxOutputBytes: number;
	readonly stdout: string;
	readonly stderr: string;

	constructor(
		command: string,
		args: readonly string[],
		maxOutputBytes: number,
		stdout: string,
		stderr: string,
	) {
		super(`Command "${command}" exceeded maxOutputBytes=${maxOutputBytes}`);
		this.name = "CommandOutputLimitError";
		this.command = command;
		this.args = args;
		this.maxOutputBytes = maxOutputBytes;
		this.stdout = stdout;
		this.stderr = stderr;
	}
}

/** Raised after a command exceeds its configured deadline and is terminated. */
export class CommandTimeoutError extends Error {
	readonly command: string;
	readonly args: readonly string[];
	readonly timeoutMs: number;
	readonly stdout: string;
	readonly stderr: string;

	constructor(
		command: string,
		args: readonly string[],
		timeoutMs: number,
		stdout: string,
		stderr: string,
	) {
		super(`Command "${command}" timed out after ${timeoutMs}ms`);
		this.name = "CommandTimeoutError";
		this.command = command;
		this.args = args;
		this.timeoutMs = timeoutMs;
		this.stdout = stdout;
		this.stderr = stderr;
	}
}

/** Kill a spawned process and its descendants without invoking a shell. */
async function terminateProcessTree(
	child: ReturnType<typeof spawn>,
): Promise<void> {
	const pid = child.pid;
	if (pid === undefined) {
		child.kill("SIGKILL");
		return;
	}
	if (process.platform !== "win32") {
		try {
			process.kill(-pid, "SIGKILL");
		} catch {
			child.kill("SIGKILL");
		}
		return;
	}

	await new Promise<void>((resolve) => {
		const killer = spawn("taskkill", ["/pid", String(pid), "/t", "/f"], {
			stdio: "ignore",
		});
		killer.once("error", () => {
			child.kill("SIGKILL");
			resolve();
		});
		killer.once("close", () => resolve());
	});
}

/**
 * Default `CommandRunner`: spawn `command` with `args` and capture combined
 * stdout/stderr. Honors `options.stdin`, `options.cwd`, `options.env`.
 */
export class SpawnRunner implements CommandRunner {
	async run(
		command: string,
		args: readonly string[],
		options: RunOptions = {},
	): Promise<RunResult> {
		if (
			options.timeoutMs !== undefined &&
			(!Number.isFinite(options.timeoutMs) || options.timeoutMs <= 0)
		) {
			throw new RangeError(`timeoutMs must be positive, got ${options.timeoutMs}`);
		}
		if (
			options.maxOutputBytes !== undefined &&
			(!Number.isFinite(options.maxOutputBytes) || options.maxOutputBytes <= 0)
		) {
			throw new RangeError(`maxOutputBytes must be positive, got ${options.maxOutputBytes}`);
		}
		const env = options.env ? { ...process.env, ...options.env } : { ...process.env };
		const child = spawn(command, [...args], {
			cwd: options.cwd,
			env,
			detached: process.platform !== "win32",
			stdio: [options.stdin !== undefined ? "pipe" : "ignore", "pipe", "pipe"],
		});
		// Attach close/error listeners BEFORE consuming stdio so a spawn
		// failure cannot race past us.
		const exit = Promise.withResolvers<number>();
		child.on("close", (code: number | null) => exit.resolve(code ?? -1));
		child.on("error", () => exit.resolve(-1));
		// Write stdin first so children that block on it can make progress.
		if (options.stdin !== undefined && child.stdin) {
			child.stdin.end(options.stdin);
		}
		// Drain stdout and stderr concurrently so a child that fills one
		// pipe while we are still reading the other cannot deadlock.
		let stdout = "";
		let stderr = "";
		let retainedBytes = 0;
		let outputLimited = false;
		const limit = options.maxOutputBytes;
		const limited = Promise.withResolvers<void>();
		const timedOutGate = Promise.withResolvers<void>();
		const destroyReaders = (): void => {
			try { child.stdout?.destroy(); } catch { /* ignore */ }
			try { child.stderr?.destroy(); } catch { /* ignore */ }
		};
		const tripLimit = (): void => {
			if (outputLimited || limit === undefined) return;
			outputLimited = true;
			destroyReaders();
			void terminateProcessTree(child);
			limited.resolve();
		};
		const retainChunk = (target: "stdout" | "stderr", chunk: Buffer | string): void => {
			if (outputLimited) return;
			const raw = typeof chunk === "string" ? Buffer.from(chunk, "utf8") : chunk;
			if (limit === undefined) {
				const text = raw.toString("utf8");
				if (target === "stdout") stdout += text;
				else stderr += text;
				retainedBytes += raw.byteLength;
				return;
			}
			const remaining = limit - retainedBytes;
			if (remaining <= 0) {
				tripLimit();
				return;
			}
			const keep = raw.subarray(0, Math.min(raw.byteLength, remaining));
			const text = keep.toString("utf8");
			if (target === "stdout") stdout += text;
			else stderr += text;
			retainedBytes += keep.byteLength;
			if (raw.byteLength > keep.byteLength) tripLimit();
		};
		const drainStdout = (async () => {
			if (!child.stdout) return;
			try {
				for await (const chunk of child.stdout) {
					retainChunk("stdout", chunk);
					if (outputLimited) return;
				}
			} catch {
				// Reader may be destroyed at the retention ceiling.
			}
		})();
		const drainStderr = (async () => {
			if (!child.stderr) return;
			try {
				for await (const chunk of child.stderr) {
					retainChunk("stderr", chunk);
					if (outputLimited) return;
				}
			} catch {
				// Reader may be destroyed at the retention ceiling.
			}
		})();
		let timedOut = false;
		const timeout = options.timeoutMs === undefined
			? undefined
			: setTimeout(() => {
				timedOut = true;
				destroyReaders();
				void terminateProcessTree(child);
				timedOutGate.resolve();
			}, options.timeoutMs);
		// Keep the timeout handle referenced so an otherwise-idle wait cannot drop it.
		const winner = await Promise.race([
			exit.promise.then((code) => ({ kind: "exit" as const, code })),
			limited.promise.then(() => ({ kind: "limit" as const })),
			timedOutGate.promise.then(() => ({ kind: "timeout" as const })),
		]);
		if (timeout !== undefined) clearTimeout(timeout);
		if (winner.kind === "limit" || outputLimited) {
			if (limit === undefined) throw new Error("output limited without maxOutputBytes");
			// Do not await drains/exit: an escaped pipe holder must not block throw.
			void Promise.allSettled([drainStdout, drainStderr, exit.promise]);
			if (retainedBytes > limit) {
				throw new Error(`internal retention invariant broken: retained=${retainedBytes} > cap=${limit}`);
			}
			throw new CommandOutputLimitError(command, args, limit, stdout, stderr);
		}
		if (winner.kind === "timeout" || timedOut) {
			await Promise.allSettled([drainStdout, drainStderr, exit.promise]);
			if (options.timeoutMs !== undefined) {
				throw new CommandTimeoutError(command, args, options.timeoutMs, stdout, stderr);
			}
		}
		await Promise.all([drainStdout, drainStderr]);
		const exitCode = winner.kind === "exit" ? winner.code : await exit.promise;
		const result: RunResult = { exitCode, stdout, stderr };
		if (options.rejectOnError && exitCode !== 0) {
			throw new CommandFailedError(result, command, args);
		}
		return result;
	}
}

/**
 * Recorded call from a {@link RecordingRunner}. Tests assert on these to
 * verify the release script issues the right cargo / bun / cargo metadata
 * invocations in the right order with the right argv.
 */
export interface RecordedCall {
	readonly command: string;
	readonly args: readonly string[];
	readonly options: RunOptions | undefined;
}

/**
 * Test fake: records every call into {@link RecordedCall} and returns the
 * responder's `RunResult` (or a default success when the responder returns
 * `undefined`). Responders typically pattern-match on `command` / `args[0]`
 * to simulate cargo / bun behavior.
 */
export class RecordingRunner implements CommandRunner {
	readonly calls: RecordedCall[] = [];
	private readonly responder: (call: RecordedCall) => Promise<RunResult | undefined> | RunResult | undefined;

	constructor(
		responder: (call: RecordedCall) => Promise<RunResult | undefined> | RunResult | undefined,
	) {
		this.responder = responder;
	}

	async run(
		command: string,
		args: readonly string[],
		options?: RunOptions,
	): Promise<RunResult> {
		const call: RecordedCall = { command, args, options };
		this.calls.push(call);
		const res = await Promise.resolve(this.responder(call));
		return res ?? OK_RUN;
	}
}

/** Default successful response: empty stdout/stderr, exit 0. */
export const OK_RUN: RunResult = { exitCode: 0, stdout: "", stderr: "" };

/** Convenience for tests: a canned `cargo metadata` reply. */
export function cargoMetadataReply(targetDir: string): RunResult {
	return {
		exitCode: 0,
		stdout: JSON.stringify({ target_directory: targetDir }),
		stderr: "",
	};
}

// ─────────────────────────────────────────────────────────────────────────────
// Filesystem seam
// ─────────────────────────────────────────────────────────────────────────────

/** `stat` result surfaced by the {@link Fs} seam. */
export interface FsStat {
	readonly isFile: boolean;
	readonly isDir: boolean;
	readonly size: number;
	readonly mode: number;
}

/**
 * Minimal filesystem surface used by the release script. The default
 * implementation (`realFs`) proxies to `node:fs/promises`; tests inject a fake
 * that owns an in-memory tree.
 */
export interface Fs {
	mkdir(path: string, opts?: { recursive?: boolean }): Promise<void>;
	rm(path: string, opts?: { recursive?: boolean; force?: boolean }): Promise<void>;
	writeFile(path: string, data: Uint8Array | string): Promise<void>;
	readFile(path: string): Promise<Uint8Array>;
	copyFile(src: string, dest: string): Promise<void>;
	cp(src: string, dest: string, opts?: { recursive?: boolean }): Promise<void>;
	chmod(path: string, mode: number): Promise<void>;
	stat(path: string): Promise<FsStat>;
	readdir(path: string): Promise<string[]>;
}

/** Default `Fs` implementation: thin wrapper over `node:fs/promises`. */
export const realFs: Fs = {
	async mkdir(path, opts) {
		await mkdir(path, opts);
	},
	async rm(path, opts) {
		await rm(path, opts);
	},
	async writeFile(path, data) {
		await writeFile(path, data);
	},
	async readFile(path) {
		const buf = await readFile(path);
		return new Uint8Array(buf.buffer, buf.byteOffset, buf.byteLength);
	},
	async copyFile(src, dest) {
		await copyFile(src, dest);
	},
	async cp(src, dest, opts) {
		await cp(src, dest, opts);
	},
	async chmod(path, mode) {
		await chmod(path, mode);
	},
	async stat(path) {
		const s = await stat(path);
		return { isFile: s.isFile(), isDir: s.isDirectory(), size: s.size, mode: s.mode };
	},
	async readdir(path) {
		return readdir(path);
	},
};

// ─────────────────────────────────────────────────────────────────────────────
// Path-safety helpers
// ─────────────────────────────────────────────────────────────────────────────

/** Error raised by {@link safeJoinPath} and {@link pathExists} boundaries. */
export class PathTraversalError extends Error {
	constructor(message: string) {
		super(message);
		this.name = "PathTraversalError";
	}
}

/**
 * Join `base` and `target`, then verify the result is still inside `base`.
 * Uses the base path's POSIX or win32 semantics and rejects absolute targets,
 * `..` escapes, null bytes, and POSIX backslashes.
 *
 * @throws {@link PathTraversalError} on any violation.
 */
export function safeJoinPath(base: string, target: string): string {
	if (target.includes("\0")) {
		throw new PathTraversalError(`null byte in path: ${target}`);
	}
	const path = win32.isAbsolute(base) && !posix.isAbsolute(base) ? win32 : posix;
	if (path.isAbsolute(target)) {
		throw new PathTraversalError(`absolute target path: ${target}`);
	}
	if (path === posix && target.includes("\\")) {
		throw new PathTraversalError(`backslash in path: ${target}`);
	}

	const normalizedBase = path.resolve(base);
	const resolved = path.resolve(normalizedBase, target);
	const relativePath = path.relative(normalizedBase, resolved);
	if (
		path.isAbsolute(relativePath) ||
		relativePath === ".." ||
		relativePath.startsWith(`..${path.sep}`)
	) {
		throw new PathTraversalError(`path escapes base: ${target} (base=${base})`);
	}
	return resolved;
}

/**
 * Return `true` if `path` exists according to `fs.stat`. Used at multiple
 * preflight points in the release orchestrator.
 */
export async function pathExists(fs: Fs, path: string): Promise<boolean> {
	try {
		await fs.stat(path);
		return true;
	} catch {
		return false;
	}
}

/**
 * Return `true` if `path` exists and is a directory. Distinct from
 * {@link pathExists} because the release script checks both forms at
 * different boundaries.
 */
export async function isDirectory(fs: Fs, path: string): Promise<boolean> {
	try {
		const s = await fs.stat(path);
		return s.isDir;
	} catch {
		return false;
	}
}
