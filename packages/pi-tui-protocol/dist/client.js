import { encodeFrame, FrameDecoder, } from "./codec.js";
/**
 * Request/correlation client over injected readable/writable streams.
 *
 * No terminal access: callers own the duplex byte transport (stdio pipes,
 * sockets, in-memory streams, etc.).
 */
export class ProtocolClient {
    writable;
    onFrame;
    pending = new Map();
    nextId = 1;
    writeChain = Promise.resolve();
    readerTask;
    closed = false;
    decoder = new FrameDecoder();
    /**
     * @param writable - Sink for outbound frames (ordered writes).
     * @param options.onFrame - Optional handler for unsolicited events/errors.
     */
    constructor(writable, options) {
        this.writable = writable;
        this.onFrame = options?.onFrame;
    }
    /**
     * Start consuming `readable` until it ends or {@link dispose} is called.
     *
     * Safe to call once. Subsequent calls are no-ops while a reader is active.
     */
    start(readable) {
        if (this.readerTask !== undefined) {
            return;
        }
        this.readerTask = this.readLoop(readable);
    }
    /** Allocate the next nonzero request id. */
    allocateId() {
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
    async send(frame) {
        this.ensureOpen();
        const bytes = encodeFrame(frame);
        await this.enqueueWrite(bytes);
    }
    /**
     * Send a request and wait for a correlated `res` or `error` frame.
     *
     * @throws when timed out, aborted, disposed, or when an error frame arrives.
     */
    async request(method, payload = {}, options) {
        this.ensureOpen();
        const id = this.allocateId();
        const frame = { id, kind: "req", method, payload };
        return await this.requestWithFrame(frame, options);
    }
    /**
     * Send a pre-built request frame and wait for correlation.
     */
    async requestWithFrame(frame, options) {
        this.ensureOpen();
        if (frame.kind !== "req" || frame.id === 0) {
            throw new Error("requestWithFrame requires kind=req and nonzero id");
        }
        const id = frame.id;
        if (this.pending.has(id)) {
            throw new Error(`duplicate pending request id ${id}`);
        }
        const { promise, resolve, reject } = Promise.withResolvers();
        const pending = {
            resolve,
            reject,
            timer: undefined,
            onAbort: undefined,
            signal: options?.signal,
        };
        // Register before signal/timeout hooks so abort/timeout always settle a live entry.
        this.pending.set(id, pending);
        const settleReject = (error) => {
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
        }
        catch (error) {
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
    async respond(id, method, payload = {}) {
        await this.send({ id, kind: "res", method, payload });
    }
    /** Convenience: send a correlated error frame. */
    async respondError(id, method, error) {
        const errorPayload = {
            code: error.code,
            message: error.message,
            retryable: error.retryable ?? false,
        };
        if (error.data !== undefined) {
            errorPayload.data = error.data;
        }
        await this.send({ id, kind: "error", method, payload: errorPayload });
    }
    /**
     * Inject a decoded inbound frame (tests / custom pumps).
     *
     * Correlated `res`/`error` settle pending requests. Orphan responses and
     * all events are delivered to `onFrame` when registered.
     */
    handleFrame(frame) {
        if (frame.kind === "res" || frame.kind === "error") {
            const pending = this.pending.get(frame.id);
            if (pending !== undefined) {
                this.pending.delete(frame.id);
                clearTimeout(pending.timer);
                cleanupAbort(pending);
                if (frame.kind === "error") {
                    pending.reject(new Error(`protocol error frame for id ${frame.id}: ${JSON.stringify(frame.payload)}`));
                }
                else {
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
    dispose(reason = "protocol client disposed") {
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
    get isDisposed() {
        return this.closed;
    }
    /** Number of in-flight correlated requests. */
    get pendingCount() {
        return this.pending.size;
    }
    /** Wait for the background reader (if any) to finish. */
    async join() {
        if (this.readerTask !== undefined) {
            await this.readerTask;
        }
    }
    ensureOpen() {
        if (this.closed) {
            throw new Error("protocol client is disposed");
        }
    }
    enqueueWrite(bytes) {
        const run = async () => {
            await this.writable.write(bytes);
        };
        this.writeChain = this.writeChain.then(run, run);
        return this.writeChain;
    }
    async readLoop(readable) {
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
        }
        catch (error) {
            this.dispose(error instanceof Error ? error.message : String(error));
        }
        finally {
            if (!this.closed) {
                this.dispose("protocol stream ended");
            }
        }
    }
}
function cleanupAbort(pending) {
    if (pending.signal !== undefined && pending.onAbort !== undefined) {
        pending.signal.removeEventListener("abort", pending.onAbort);
    }
}
async function* asAsyncIterable(readable) {
    if (isAsyncIterable(readable)) {
        yield* readable;
        return;
    }
    const stream = readable;
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
    }
    finally {
        reader.releaseLock();
    }
}
function isAsyncIterable(value) {
    if (typeof value !== "object" || value === null) {
        return false;
    }
    const maybe = value;
    return typeof maybe[Symbol.asyncIterator] === "function";
}
//# sourceMappingURL=client.js.map