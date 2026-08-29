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

const TERMINAL_QUERY_REPLIES = [["\x1b[c", "\x1b[?62;1;2;6;7;8;9c"]] as const;
const MAX_TERMINAL_QUERY_LENGTH = Math.max(
	"\x1b[6n".length,
	...TERMINAL_QUERY_REPLIES.map(([query]) => query.length),
);


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

interface InputWrite {
	readonly text: string;
	readonly outputOffset: number;
}

export interface KeyWriteReceipt {
	/** Character offset into rawText at the moment immediately before the first write. */
	readonly outputOffset: number;
	/** Process-relative elapsed (ms) captured immediately before the first FileSink.write. */
	readonly startedElapsedMs: number;
}

function shellQuote(value: string): string {
	if (value.includes("\0")) throw new Error("PTY argv cannot contain NUL bytes");
	return `'${value.replaceAll("'", `'"'"'`)}'`;
}

function commandString(
	argv: readonly [string, ...string[]],
	size: { readonly columns: number; readonly rows: number },
): string {
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
	return `stty cols ${size.columns} rows ${size.rows}; exec ${argv.map(shellQuote).join(" ")}`;
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

export class PtyProcess {
	readonly #startedAt = performance.now();
	readonly #chunks: PtyChunk[] = [];
	readonly #writes: InputWrite[] = [];
	readonly #process: Bun.Subprocess<"pipe", "pipe", "pipe">;
	readonly #completed: Promise<number>;
	readonly #stdin: Bun.FileSink;
	readonly #cursorPosition: { readonly row: number; readonly column: number } | false;
	#rawText = "";
	#queryScanOffset = 0;
	#exitCode: number | null = null;
	#version = 0;
	#listeners = new Set<() => void>();

	constructor(command: PtyCommand) {
		this.#cursorPosition = command.cursorPosition ?? { row: 1, column: 1 };
		this.#process = Bun.spawn(
			[
				"setsid",
				"--wait",
				"script",
				"--quiet",
				"--flush",
				"--echo",
				"always",
				"--return",
				"--command",
				commandString(command.argv, command.size ?? { columns: 80, rows: 24 }),
				"/dev/null",
			],
			{
				cwd: command.cwd,
				env: mergedEnvironment(command.env),
				stdin: "pipe",
				stdout: "pipe",
				stderr: "pipe",
			},
		);
		this.#stdin = this.#process.stdin;
		const ptyDone = this.#consume(this.#process.stdout, "pty");
		const driverDone = this.#consume(this.#process.stderr, "driver");
		this.#completed = Promise.all([this.#process.exited, ptyDone, driverDone]).then(([code]) => {
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
	writeKeys(...keys: readonly (string | Uint8Array)[]): KeyWriteReceipt {
		if (this.exited) throw new Error(`PTY process ${this.pid} has exited`);
		const encoded: Uint8Array[] = [];
		let text = "";
		for (const key of keys) {
			const bytes = typeof key === "string" ? new TextEncoder().encode(key) : key;
			text += new TextDecoder().decode(bytes);
			encoded.push(bytes);
		}
		// The receipt is the latency start boundary: captured after encoding,
		// immediately before the first sink write, so it excludes prior chunks
		// and all snapshot()/echo-scan cost.
		const receipt: KeyWriteReceipt = {
			outputOffset: this.#rawText.length,
			startedElapsedMs: performance.now() - this.#startedAt,
		};
		for (const bytes of encoded) this.#stdin.write(bytes);
		this.#writes.push({ text, outputOffset: receipt.outputOffset });
		this.#stdin.flush();
		return receipt;
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
		if (this.#exitCode !== null) return this.#exitCode;
		const deadline = Promise.withResolvers<null>();
		const deadlineTimer = setTimeout(() => deadline.resolve(null), deadlineMs);
		const code = await Promise.race([this.#completed, deadline.promise]);
		clearTimeout(deadlineTimer);
		if (code === null) throw new Error(`PTY process did not exit within ${deadlineMs}ms`);
		return code;
	}

	async terminate(graceMs = 1_000): Promise<number> {
		if (this.#exitCode !== null) return this.#exitCode;
		this.#signalTree("SIGTERM");
		try {
			return await this.waitForExit(graceMs);
		} catch {
			this.#signalTree("SIGKILL");
			return await this.waitForExit(graceMs);
		}
	}

	async #consume(stream: ReadableStream<Uint8Array>, source: PtyChunk["stream"]): Promise<void> {
		const decoder = new TextDecoder();
		for await (const bytes of stream) {
			// Chunk-arrival timestamp is captured before any copy/decode work so
			// it prices transport arrival, not harness processing.
			const arrivalElapsedMs = performance.now() - this.#startedAt;
			const copy = Uint8Array.from(bytes);
			const text = decoder.decode(copy, { stream: true });
			if (source === "pty") {
				this.#rawText += text;
				this.#answerTerminalQueries();
			}
			this.#chunks.push({
				stream: source,
				text,
				bytes: copy,
				elapsedMs: arrivalElapsedMs,
				unixMs: Date.now(),
			});
			this.#notify();
		}
		const arrivalElapsedMs = performance.now() - this.#startedAt;
		const tail = decoder.decode();
		if (tail) {
			if (source === "pty") {
				this.#rawText += tail;
				this.#answerTerminalQueries();
			}
			this.#chunks.push({
				stream: source,
				text: tail,
				bytes: new Uint8Array(),
				elapsedMs: arrivalElapsedMs,
				unixMs: Date.now(),
			});
			this.#notify();
		}
	}

	#answerTerminalQueries(): void {
		while (true) {
			let nextQuery: { index: number; query: string; reply: string } | undefined;
			const consider = (query: string, reply: string): void => {
				const index = this.#rawText.indexOf(query, this.#queryScanOffset);
				if (index >= 0 && (nextQuery === undefined || index < nextQuery.index)) {
					nextQuery = { index, query, reply };
				}
			};
			for (const [query, reply] of TERMINAL_QUERY_REPLIES) consider(query, reply);
			if (this.#cursorPosition !== false) {
				consider(
					"\x1b[6n",
					`\x1b[${this.#cursorPosition.row};${this.#cursorPosition.column}R`,
				);
			}
			if (nextQuery === undefined) break;
			this.#stdin.write(nextQuery.reply);
			this.#stdin.flush();
			this.#queryScanOffset = nextQuery.index + nextQuery.query.length;
		}
		this.#queryScanOffset = Math.max(
			this.#queryScanOffset,
			this.#rawText.length - (MAX_TERMINAL_QUERY_LENGTH - 1),
		);
	}

	#notify(): void {
		this.#version += 1;
		const listeners = [...this.#listeners];
		this.#listeners.clear();
		for (const listener of listeners) listener();
	}

	#signalTree(signal: ProcessSignal): void {
		try {
			process.kill(-this.#process.pid, signal);
		} catch (error) {
			if (!(error instanceof Error && "code" in error && error.code === "ESRCH")) throw error;
		}
	}
}

export function spawnPty(command: PtyCommand): PtyProcess {
	return new PtyProcess(command);
}
