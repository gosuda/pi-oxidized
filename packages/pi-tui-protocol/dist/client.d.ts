import type { Frame, FrameId } from "./types.js";
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
/**
 * Request/correlation client over injected readable/writable streams.
 *
 * No terminal access: callers own the duplex byte transport (stdio pipes,
 * sockets, in-memory streams, etc.).
 */
export declare class ProtocolClient {
    private readonly writable;
    private readonly onFrame;
    private readonly pending;
    private nextId;
    private writeChain;
    private readerTask;
    private closed;
    private readonly decoder;
    /**
     * @param writable - Sink for outbound frames (ordered writes).
     * @param options.onFrame - Optional handler for unsolicited events/errors.
     */
    constructor(writable: ByteWritable, options?: {
        onFrame?: FrameHandler;
    });
    /**
     * Start consuming `readable` until it ends or {@link dispose} is called.
     *
     * Safe to call once. Subsequent calls are no-ops while a reader is active.
     */
    start(readable: ByteReadable): void;
    /** Allocate the next nonzero request id. */
    allocateId(): FrameId;
    /**
     * Write a frame without waiting for a correlated response.
     * Writes are strictly ordered through an internal queue.
     */
    send(frame: Frame): Promise<void>;
    /**
     * Send a request and wait for a correlated `res` or `error` frame.
     *
     * @throws when timed out, aborted, disposed, or when an error frame arrives.
     */
    request(method: string, payload?: unknown, options?: RequestOptions): Promise<Frame>;
    /**
     * Send a pre-built request frame and wait for correlation.
     */
    requestWithFrame(frame: Frame, options?: RequestOptions): Promise<Frame>;
    /** Convenience: respond to a request. */
    respond(id: FrameId, method: string, payload?: unknown): Promise<void>;
    /** Convenience: send a correlated error frame. */
    respondError(id: FrameId, method: string, error: {
        code: string;
        message: string;
        retryable?: boolean;
        data?: unknown;
    }): Promise<void>;
    /**
     * Inject a decoded inbound frame (tests / custom pumps).
     *
     * Correlated `res`/`error` settle pending requests. Orphan responses and
     * all events are delivered to `onFrame` when registered.
     */
    handleFrame(frame: Frame): void;
    /**
     * Cancel all pending requests and stop accepting new work.
     */
    dispose(reason?: string): void;
    /** Whether {@link dispose} has been called. */
    get isDisposed(): boolean;
    /** Number of in-flight correlated requests. */
    get pendingCount(): number;
    /** Wait for the background reader (if any) to finish. */
    join(): Promise<void>;
    private ensureOpen;
    private enqueueWrite;
    private readLoop;
}
//# sourceMappingURL=client.d.ts.map