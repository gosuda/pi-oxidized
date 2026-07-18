import {
	encodeFrame,
	errorFrame,
	FrameDecoder,
	requestFrame,
	responseFrame,
} from "./codec.js";
import type { Frame, FrameId, Method } from "./types.js";

/** Options for a correlated request. */
export interface RequestOptions {
	/** Timeout in milliseconds. */
	timeoutMs?: number;
	/** AbortSignal for cancellation. */
	signal?: AbortSignal;
}

/** Inbound frame handler. */
export type FrameHandler = (frame: Frame) => void;

/** Writable sink for encoded frame bytes. */
export interface ByteWritable {
	write(chunk: Uint8Array): void | Promise<void>;
}

/** Readable source of encoded frame bytes. */
export type ByteReadable = AsyncIterable<Uint8Array> | ReadableStream<Uint8Array>;

interface Pending {
	resolve: (frame: Frame) => void;
	reject: (error: Error) => void;
	timer: ReturnType<typeof setTimeout> | undefined;
	onAbort: (() => void) | undefined;
	signal: AbortSignal | undefined;
}

/**
 * Request/correlation client over injected readable/writable streams.
 *
 * No terminal access: callers own the duplex byte transport (stdio pipes,
 * sockets, in-memory streams, etc.).
 */
export class ProtocolClient {
	private readonly writable: ByteWritable;
	private readonly onFrame: FrameHandler | undefined;
	private readonly pending = new Map<FrameId, Pending>();
	private nextId: FrameId = 1;
	private writeChain: Promise<void> = Promise.resolve();
	private readerTask: Promise<void> | undefined;
	private closed = false;
	private readonly decoder = new FrameDecoder();

	/**
	 * @param writable - Sink for outbound frames (ordered writes).
	 * @param options.onFrame - Optional handler for unsolicited events/errors.
	 */
	constructor(
		writable: ByteWritable,
		options?: {
			onFrame?: FrameHandler;
		},
	) {
		this.writable = writable;
		this.onFrame = options?.onFrame;
	}

	/**
	 * Start consuming `readable` until it ends or {@link dispose} is called.
	 *
	 * Safe to call once. Subsequent calls are no-ops while a reader is active.
	 */
	start(readable: ByteReadable): void {
		if (this.readerTask !== undefined) {
			return;
		}
		this.readerTask = this.readLoop(readable);
	}

	/** Allocate the next nonzero request id. */
	allocateId(): FrameId {
		const id = this.nextId;
		this.nextId += 1;
		if (this.nextId > Number.MAX_SAFE_INTEGER) {
			this.nextId = 1;
		}
		return id;
	}

	/**
	 * Write a frame without waiting for a correlated response.
	 * Writes are strictly ordered through an internal queue.
	 */
	async send(frame: Frame): Promise<void> {
		this.ensureOpen();
		const bytes = encodeFrame(frame);
		await this.enqueueWrite(bytes);
	}

	/**
	 * Send a request and wait for a correlated `res` or `error` frame.
	 *
	 * @throws when timed out, aborted, disposed, or when an error frame arrives.
	 */
	async request(
		method: Method,
		payload: unknown = {},
		options?: RequestOptions,
	): Promise<Frame> {
		this.ensureOpen();
		const id = this.allocateId();
		const frame = requestFrame(id, method, payload);
		return await this.requestWithFrame(frame, options);
	}

	/**
	 * Send a pre-built request frame and wait for correlation.
	 */
	async requestWithFrame(frame: Frame, options?: RequestOptions): Promise<Frame> {
		this.ensureOpen();
		if (frame.kind !== "req" || frame.id === 0) {
			throw new Error("requestWithFrame requires kind=req and nonzero id");
		}
		const id = frame.id;
		if (this.pending.has(id)) {
			throw new Error(`duplicate pending request id ${id}`);
		}

		const { promise, resolve, reject } = Promise.withResolvers<Frame>();
		const pending: Pending = {
			resolve,
			reject,
			timer: undefined,
			onAbort: undefined,
			signal: options?.signal,
		};
		// Register before signal/timeout hooks so abort/timeout always settle a live entry.
		this.pending.set(id, pending);

		const settleReject = (error: Error) => {
			const current = this.pending.get(id);
			if (current !== pending) {
				return;
			}
			this.pending.delete(id);
			clearTimeout(pending.timer);
			cleanupAbort(pending);
			reject(error);
		};

		if (options?.timeoutMs !== undefined) {
			const timeoutMs = options.timeoutMs;
			pending.timer = setTimeout(() => {
				settleReject(new Error(`protocol request ${id} timed out after ${timeoutMs}ms`));
			}, timeoutMs);
		}
		if (options?.signal !== undefined) {
			const onAbort = () => {
				settleReject(new Error(`protocol request ${id} aborted`));
			};
			pending.onAbort = onAbort;
			if (options.signal.aborted) {
				onAbort();
				return await promise;
			}
			options.signal.addEventListener("abort", onAbort, { once: true });
		}

		if (!this.pending.has(id)) {
			// Already settled by a synchronous abort above.
			return await promise;
		}

		try {
			await this.send(frame);
		} catch (error) {
			const current = this.pending.get(id);
			if (current === pending) {
				this.pending.delete(id);
				clearTimeout(pending.timer);
				cleanupAbort(pending);
			}
			throw error;
		}

		return await promise;
	}

	/** Convenience: respond to a request. */
	async respond(id: FrameId, method: Method, payload: unknown = {}): Promise<void> {
		await this.send(responseFrame(id, method, payload));
	}

	/** Convenience: send a correlated error frame. */
	async respondError(
		id: FrameId,
		method: Method,
		error: { code: string; message: string; retryable?: boolean; data?: unknown },
	): Promise<void> {
		await this.send(errorFrame(id, method, error));
	}

	/**
	 * Inject a decoded inbound frame (tests / custom pumps).
	 *
	 * Correlated `res`/`error` settle pending requests. Orphan responses and
	 * all events are delivered to `onFrame` when registered.
	 */
	handleFrame(frame: Frame): void {
		if (frame.kind === "res" || frame.kind === "error") {
			const pending = this.pending.get(frame.id);
			if (pending !== undefined) {
				this.pending.delete(frame.id);
				clearTimeout(pending.timer);
				cleanupAbort(pending);
				if (frame.kind === "error") {
					pending.reject(
						new Error(
							`protocol error frame for id ${frame.id}: ${JSON.stringify(frame.payload)}`,
						),
					);
				} else {
					pending.resolve(frame);
				}
				return;
			}
			// Orphan response/error.
			this.onFrame?.(frame);
			return;
		}
		this.onFrame?.(frame);
	}

	/**
	 * Cancel all pending requests and stop accepting new work.
	 */
	dispose(reason = "protocol client disposed"): void {
		if (this.closed) {
			return;
		}
		this.closed = true;
		for (const [id, pending] of this.pending) {
			this.pending.delete(id);
			clearTimeout(pending.timer);
			cleanupAbort(pending);
			pending.reject(new Error(reason));
		}
	}

	/** Whether {@link dispose} has been called. */
	get isDisposed(): boolean {
		return this.closed;
	}

	/** Number of in-flight correlated requests. */
	get pendingCount(): number {
		return this.pending.size;
	}

	/** Wait for the background reader (if any) to finish. */
	async join(): Promise<void> {
		if (this.readerTask !== undefined) {
			await this.readerTask;
		}
	}

	private ensureOpen(): void {
		if (this.closed) {
			throw new Error("protocol client is disposed");
		}
	}

	private enqueueWrite(bytes: Uint8Array): Promise<void> {
		const run = async () => {
			await this.writable.write(bytes);
		};
		this.writeChain = this.writeChain.then(run, run);
		return this.writeChain;
	}

	private async readLoop(readable: ByteReadable): Promise<void> {
		try {
			for await (const chunk of asAsyncIterable(readable)) {
				if (this.closed) {
					break;
				}
				const frames = this.decoder.push(chunk);
				for (const frame of frames) {
					this.handleFrame(frame);
				}
			}
			if (!this.closed) {
				const finalFrame = this.decoder.finishWithFinalLine();
				if (finalFrame !== undefined) {
					this.handleFrame(finalFrame);
				}
			}
		} catch (error) {
			this.dispose(error instanceof Error ? error.message : String(error));
		} finally {
			if (!this.closed) {
				this.dispose("protocol stream ended");
			}
		}
	}
}

function cleanupAbort(pending: Pending): void {
	if (pending.signal !== undefined && pending.onAbort !== undefined) {
		pending.signal.removeEventListener("abort", pending.onAbort);
	}
}

async function* asAsyncIterable(readable: ByteReadable): AsyncIterable<Uint8Array> {
	if (isAsyncIterable(readable)) {
		yield* readable;
		return;
	}
	const stream: ReadableStream<Uint8Array> = readable;
	const reader = stream.getReader();
	try {
		while (true) {
			const result = await reader.read();
			if (result.done) {
				break;
			}
			if (result.value !== undefined) {
				yield result.value;
			}
		}
	} finally {
		reader.releaseLock();
	}
}

function isAsyncIterable(value: unknown): value is AsyncIterable<Uint8Array> {
	if (typeof value !== "object" || value === null) {
		return false;
	}
	const maybe = value as Record<PropertyKey, unknown>;
	return typeof maybe[Symbol.asyncIterator] === "function";
}
