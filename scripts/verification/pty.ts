export const PTY_KEYS = {
	enter: "\r",
	escape: "\x1b",
	ctrlC: "\x03",
	ctrlD: "\x04",
	up: "\x1b[A",
	down: "\x1b[B",
	right: "\x1b[C",
	left: "\x1b[D",
} as const;


export interface PtyCommand {
	argv: readonly [string, ...string[]];
	cwd: string;
	env?: Readonly<Record<string, string | undefined>>;
	stdin?: string | Uint8Array;
	cursorPosition?: { readonly row: number; readonly column: number } | false;
	size?: { readonly columns: number; readonly rows: number };
}

export interface PtyChunk {
	readonly stream: "pty" | "driver";
	readonly text: string;
	readonly bytes: Uint8Array;
	readonly elapsedMs: number;
	readonly unixMs: number;
}

export interface PtySnapshot {
	readonly rawText: string;
	readonly applicationText: string;
	readonly echoText: string;
	readonly chunks: readonly PtyChunk[];
	readonly elapsedMs: number;
	readonly exited: boolean;
	readonly exitCode: number | null;
}

export interface WaitForOptions {
	deadlineMs: number;
	source?: "application" | "raw" | "echo";
}

type SnapshotPredicate = (snapshot: PtySnapshot) => boolean;
type ProcessSignal = "SIGINT" | "SIGTERM" | "SIGKILL";

const CURSOR_POSITION_QUERY = "\x1b[6n";
const TERMINAL_QUERY_RESPONSES: Readonly<Record<string, string>> = {
	"\x1b[?u": "\x1b[?0u",
	"\x1b[c": "\x1b[?1;2c",
	"\x1b[16t": "\x1b[6;16;8t",
	"\x1b]11;?\x07": "\x1b]11;rgb:0000/0000/0000\x1b\\",
	"\x1b[?996n": "\x1b[?997;1n",
};
export const TERMINAL_QUERY_SEQUENCES = [
	...Object.keys(TERMINAL_QUERY_RESPONSES),
	CURSOR_POSITION_QUERY,
] as const;
export const MAX_TERMINAL_QUERY_LENGTH = Math.max(...TERMINAL_QUERY_SEQUENCES.map((query) => query.length));

interface InputWrite {
	readonly text: string;
	readonly outputOffset: number;
}

function mergedEnvironment(overrides: PtyCommand["env"]): Record<string, string> {
	const result: Record<string, string> = {};
	for (const [name, value] of Object.entries(process.env)) {
		if (value !== undefined) result[name] = value;
	}
	for (const [name, value] of Object.entries(overrides ?? {})) {
		if (value === undefined) delete result[name];
		else result[name] = value;
	}
	result.TERM ??= "xterm-256color";
	return result;
}

/**
 * Write to a Bun terminal without leaking close-race failures. Real write
 * failures are reported through `onError`; teardown races are ignored.
 */
export function writeTerminalSafe(
	terminal: { readonly closed: boolean; write(data: string | Uint8Array): number | Promise<number> },
	data: string | Uint8Array,
	onError: (error: unknown) => void = () => {},
): void {
	if (terminal.closed || data.length === 0) return;
	try {
		// Bun queues the complete argument even when it reports zero
		// synchronous progress. Retrying would duplicate accepted bytes.
		const result = terminal.write(data);
		if (result instanceof Promise) {
			void result.catch((error: unknown) => {
				if (!terminal.closed) onError(error);
			});
		}
	} catch (error) {
		if (!terminal.closed) onError(error);
	}
}

export class PtyProcess {
	readonly #startedAt = performance.now();
	readonly #chunks: PtyChunk[] = [];
	readonly #writes: InputWrite[] = [];
	readonly #process: Bun.Subprocess;
	readonly #completed: Promise<number>;
	readonly #decoder = new TextDecoder();
	readonly #cursorPosition: { readonly row: number; readonly column: number } | false;
	#terminalQueryScanOffset = 0;
	#rawText = "";
	#exitCode: number | null = null;
	#version = 0;
	#listeners = new Set<() => void>();
	readonly #terminalWriteFailure = Promise.withResolvers<void>();
	#terminalWriteError: { readonly cause: unknown } | undefined;

	constructor(command: PtyCommand) {
		if (process.platform === "win32") {
			throw new Error("Bun terminal spawn is unavailable on Windows; PTY tests must be skipped");
		}
		this.#cursorPosition = command.cursorPosition ?? { row: 1, column: 1 };
		const size = command.size ?? { columns: 80, rows: 24 };
		if (
			!Number.isSafeInteger(size.columns) ||
			size.columns < 1 ||
			size.columns > 10_000 ||
			!Number.isSafeInteger(size.rows) ||
			size.rows < 1 ||
			size.rows > 10_000
		) {
			throw new Error("PTY size must use integer columns and rows between 1 and 10000");
		}
		for (const argument of command.argv) {
			if (argument.includes("\0")) throw new Error("PTY argv cannot contain NUL bytes");
		}
		const ptyEnd = Promise.withResolvers<void>();
		this.#process = Bun.spawn([...command.argv], {
			cwd: command.cwd,
			env: mergedEnvironment(command.env),
			// Bun's POSIX terminal spawn gives the child its own session and
			// controlling terminal (setsid + TIOCSCTTY), replacing the
			// util-linux setsid(1)/script(1) wrapper that exists only on Linux.
			terminal: {
				cols: size.columns,
				rows: size.rows,
				data: (_terminal, bytes) => this.#receive(bytes),
				exit: () => {
					this.#flushTail();
					ptyEnd.resolve();
				},
			},
		});
		this.#completed = this.#process.exited.then(async (code) => {
			// Closing the master releases the descriptor and SIGHUPs orphaned group
			// members, matching script(1) teardown when its child exits. Do this as
			// soon as the spawned child exits — waiting for terminal EOF first can
			// hang forever when a background descendant still holds the slave open.
			try {
				const terminal = this.#process.terminal;
				if (terminal && !terminal.closed) terminal.close();
			} catch {
				// Teardown is best-effort; the exit code is already known.
			}
			await ptyEnd.promise;
			this.#exitCode = code;
			this.#notify();
			return code;
		});
		if (command.stdin !== undefined) this.writeKeys(command.stdin);
	}

	get pid(): number {
		return this.#process.pid;
	}

	get exited(): boolean {
		return this.#exitCode !== null;
	}

	writeKeys(...keys: readonly (string | Uint8Array)[]): void {
		if (this.exited) throw new Error(`PTY process ${this.pid} has exited`);
		this.#throwIfTerminalWriteFailed();
		const outputOffset = this.#rawText.length;
		let text = "";
		for (const key of keys) {
			// Clone caller-provided bytes at the ownership boundary so queued
			// writes never alias storage the caller can still mutate.
			const bytes = typeof key === "string" ? new TextEncoder().encode(key) : Uint8Array.from(key);
			text += new TextDecoder().decode(bytes);
			this.#writeTerminal(bytes);
			this.#throwIfTerminalWriteFailed();
		}
		this.#writes.push({ text, outputOffset });
	}


	snapshot(): PtySnapshot {
		const echoRanges: Array<readonly [number, number]> = [];
		let searchOffset = 0;
		for (const write of this.#writes) {
			if (!/[\r\n]/.test(write.text) || !/[^\x00-\x1f\x7f]/.test(write.text)) continue;
			const candidates = [write.text, write.text.replaceAll("\r", "\r\n"), write.text.replaceAll("\n", "\r\n")];
			let bestStart = -1;
			let bestText = "";
			for (const candidate of candidates) {
				const start = this.#rawText.indexOf(candidate, Math.max(searchOffset, write.outputOffset));
				if (start >= 0 && (bestStart < 0 || start < bestStart || (start === bestStart && candidate.length > bestText.length))) {
					bestStart = start;
					bestText = candidate;
				}
			}
			if (bestStart >= 0) {
				echoRanges.push([bestStart, bestStart + bestText.length]);
				searchOffset = bestStart + bestText.length;
			}
		}
		let applicationText = "";
		let echoText = "";
		let offset = 0;
		for (const [start, end] of echoRanges) {
			applicationText += this.#rawText.slice(offset, start);
			echoText += this.#rawText.slice(start, end);
			offset = end;
		}
		applicationText += this.#rawText.slice(offset);
		return {
			rawText: this.#rawText,
			applicationText,
			echoText,
			chunks: [...this.#chunks],
			elapsedMs: performance.now() - this.#startedAt,
			exited: this.exited,
			exitCode: this.#exitCode,
		};
	}

	async waitFor(pattern: RegExp | SnapshotPredicate, options: WaitForOptions): Promise<PtySnapshot> {
		if (!(options.deadlineMs > 0)) throw new Error("PTY wait deadline must be positive");
		const deadline = performance.now() + options.deadlineMs;
		for (;;) {
			this.#throwIfTerminalWriteFailed();
			const snapshot = this.snapshot();
			let matched: boolean;
			if (pattern instanceof RegExp) {
				pattern.lastIndex = 0;
				const text = options.source === "raw" ? snapshot.rawText : options.source === "echo" ? snapshot.echoText : snapshot.applicationText;
				matched = pattern.test(text);
			} else {
				matched = pattern(snapshot);
			}
			if (matched) return snapshot;
			if (snapshot.exited) throw new Error(`PTY process exited with code ${snapshot.exitCode} before expected output`);
			const remaining = deadline - performance.now();
			if (remaining <= 0) throw new Error(`PTY output deadline exceeded after ${options.deadlineMs}ms`);
			const observedVersion = this.#version;
			const changed = Promise.withResolvers<void>();
			const listener = () => changed.resolve();
			this.#listeners.add(listener);
			if (this.#version !== observedVersion) listener();
			const deadlineTimer = setTimeout(changed.resolve, remaining);
			await changed.promise;
			clearTimeout(deadlineTimer);
			this.#listeners.delete(listener);
		}
	}

	async waitForExit(deadlineMs: number): Promise<number> {
		this.#throwIfTerminalWriteFailed();
		if (this.#exitCode !== null) return this.#exitCode;
		const deadline = Promise.withResolvers<null>();
		const deadlineTimer = setTimeout(() => deadline.resolve(null), deadlineMs);
		const code = await Promise.race([
			this.#completed,
			deadline.promise,
			this.#terminalWriteFailure.promise.then(() => null),
		]);
		clearTimeout(deadlineTimer);
		this.#throwIfTerminalWriteFailed();
		if (code === null) throw new Error(`PTY process did not exit within ${deadlineMs}ms`);
		return code;
	}

	async terminate(graceMs = 1_000): Promise<number> {
		if (this.#exitCode !== null) return this.#exitCode;
		this.#signalTree("SIGTERM");
		try {
			return await this.#waitForProcessExit(graceMs);
		} catch {
			this.#signalTree("SIGKILL");
			return await this.#waitForProcessExit(graceMs);
		}
	}

	async #waitForProcessExit(deadlineMs: number): Promise<number> {
		if (this.#exitCode !== null) return this.#exitCode;
		const deadline = Promise.withResolvers<null>();
		const deadlineTimer = setTimeout(() => deadline.resolve(null), deadlineMs);
		const code = await Promise.race([this.#completed, deadline.promise]);
		clearTimeout(deadlineTimer);
		if (code === null) throw new Error(`PTY process did not exit within ${deadlineMs}ms`);
		return code;
	}

	#receive(bytes: Uint8Array): void {
		const copy = Uint8Array.from(bytes);
		const text = this.#decoder.decode(copy, { stream: true });
		this.#rawText += text;
		this.#answerTerminalQueries();
		this.#chunks.push({
			stream: "pty",
			text,
			bytes: copy,
			elapsedMs: performance.now() - this.#startedAt,
			unixMs: Date.now(),
		});
		this.#notify();
	}

	#flushTail(): void {
		const tail = this.#decoder.decode();
		if (!tail) return;
		this.#rawText += tail;
		this.#answerTerminalQueries();
		this.#chunks.push({
			stream: "pty",
			text: tail,
			bytes: new Uint8Array(),
			elapsedMs: performance.now() - this.#startedAt,
			unixMs: Date.now(),
		});
		this.#notify();
	}

	#writeTerminal(data: string | Uint8Array): void {
		const terminal = this.#process.terminal;
		if (!terminal) return;
		writeTerminalSafe(terminal, data, (error) => {
			if (this.#terminalWriteError !== undefined) return;
			this.#terminalWriteError = { cause: error };
			this.#terminalWriteFailure.resolve();
			this.#notify();
		});
	}

	#throwIfTerminalWriteFailed(): void {
		if (this.#terminalWriteError === undefined) return;
		const { cause } = this.#terminalWriteError;
		const detail = cause instanceof Error ? cause.message : String(cause);
		throw new Error(`PTY terminal write failed: ${detail}`, { cause });
	}

	#answerTerminalQueries(): void {
		while (this.#terminalQueryScanOffset < this.#rawText.length) {
			let matched: string | undefined;
			for (const query of TERMINAL_QUERY_SEQUENCES) {
				if (this.#rawText.startsWith(query, this.#terminalQueryScanOffset)) {
					matched = query;
					break;
				}
			}
			if (matched !== undefined) {
				const response =
					matched === CURSOR_POSITION_QUERY
						? this.#cursorPosition === false
							? undefined
							: `\x1b[${this.#cursorPosition.row};${this.#cursorPosition.column}R`
						: TERMINAL_QUERY_RESPONSES[matched];
				if (response !== undefined) this.#writeTerminal(response);
				this.#terminalQueryScanOffset += matched.length;
				continue;
			}

			const remaining = this.#rawText.length - this.#terminalQueryScanOffset;
			if (remaining < MAX_TERMINAL_QUERY_LENGTH) {
				const tail = this.#rawText.slice(this.#terminalQueryScanOffset);
				if (TERMINAL_QUERY_SEQUENCES.some((query) => query.startsWith(tail))) return;
			}
			this.#terminalQueryScanOffset += 1;
		}
	}

	#notify(): void {
		this.#version += 1;
		const listeners = [...this.#listeners];
		this.#listeners.clear();
		for (const listener of listeners) listener();
	}

	#signalTree(signal: ProcessSignal): void {
		// Bun terminal spawn is POSIX-only and makes the child a process-group
		// leader, so signal the complete group.
		try {
			process.kill(-this.#process.pid, signal);
		} catch (error) {
			if (!(error instanceof Error && "code" in error && error.code === "ESRCH")) throw error;
			// No such group: the leader is gone or never became a group leader.
			try {
				this.#process.kill(signal);
			} catch {
				// The leader already exited.
			}
		}
	}
}

export function spawnPty(command: PtyCommand): PtyProcess {
	return new PtyProcess(command);
}
