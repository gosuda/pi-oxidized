import { describe, expect, test } from "bun:test";
import { encodeFrame, requestFrame, responseFrame } from "../src/codec.ts";
import { ProtocolClient } from "../src/client.ts";
import { localHello } from "../src/types.ts";

class MemoryWritable {
	readonly chunks: Uint8Array[] = [];
	private readonly waiters: Array<() => void> = [];

	write(chunk: Uint8Array): void {
		this.chunks.push(chunk.slice());
		const waiters = this.waiters.splice(0, this.waiters.length);
		for (const resolve of waiters) {
			resolve();
		}
	}

	text(): string {
		return new TextDecoder().decode(concat(this.chunks));
	}

	/** Resolve when at least `count` chunks have been written. */
	whenChunks(count: number): Promise<void> {
		if (this.chunks.length >= count) {
			return Promise.resolve();
		}
		const { promise, resolve } = Promise.withResolvers<void>();
		const check = () => {
			if (this.chunks.length >= count) {
				resolve();
			} else {
				this.waiters.push(check);
			}
		};
		this.waiters.push(check);
		return promise;
	}
}

function concat(chunks: Uint8Array[]): Uint8Array {
	const total = chunks.reduce((n, c) => n + c.byteLength, 0);
	const out = new Uint8Array(total);
	let offset = 0;
	for (const chunk of chunks) {
		out.set(chunk, offset);
		offset += chunk.byteLength;
	}
	return out;
}

describe("ProtocolClient", () => {
	test("concurrent correlation", async () => {
		const writable = new MemoryWritable();
		const client = new ProtocolClient(writable);

		const p1 = client.request("select", { title: "A", options: ["1"] });
		const p2 = client.request("confirm", { title: "B", message: "m" });
		expect(client.pendingCount).toBe(2);

		await writable.whenChunks(2);
		const out = writable.text().trimEnd().split("\n");
		expect(out.length).toBe(2);
		const first = JSON.parse(out[0] ?? "{}") as { id: number; method: string };
		const second = JSON.parse(out[1] ?? "{}") as { id: number; method: string };
		expect(first.id).toBe(1);
		expect(first.method).toBe("select");
		expect(second.id).toBe(2);
		expect(second.method).toBe("confirm");

		// Resolve out of order.
		client.handleFrame(responseFrame(2, "confirm", { confirmed: false }));
		client.handleFrame(responseFrame(1, "select", { value: "1" }));

		const [r1, r2] = await Promise.all([p1, p2]);
		expect(r1.payload).toEqual({ value: "1" });
		expect(r2.payload).toEqual({ confirmed: false });
		expect(client.pendingCount).toBe(0);
		client.dispose();
	});

	test("timeout", async () => {
		const client = new ProtocolClient(new MemoryWritable());
		// timeoutMs: 0 fires on the next timer turn without a long wall wait.
		await expect(client.request("hello", localHello(), { timeoutMs: 0 })).rejects.toThrow(
			/timed out/,
		);
		expect(client.pendingCount).toBe(0);
		client.dispose();
	});

	test("abort", async () => {
		const client = new ProtocolClient(new MemoryWritable());
		const ac = new AbortController();
		const p = client.request("hello", localHello(), { signal: ac.signal });
		ac.abort();
		await expect(p).rejects.toThrow(/aborted/);
		expect(client.pendingCount).toBe(0);
		client.dispose();
	});

	test("orphan response delivered to onFrame", () => {
		const orphans: unknown[] = [];
		const client = new ProtocolClient(new MemoryWritable(), {
			onFrame: (frame) => {
				orphans.push(frame);
			},
		});
		client.handleFrame(responseFrame(99, "hello", localHello()));
		expect(orphans).toHaveLength(1);
		const first = orphans[0] as { id: number };
		expect(first.id).toBe(99);
		client.dispose();
	});

	test("ordered writes under concurrency", async () => {
		const order: number[] = [];
		const gates: Array<PromiseWithResolvers<void>> = [
			Promise.withResolvers<void>(),
			Promise.withResolvers<void>(),
			Promise.withResolvers<void>(),
		];
		// Release writes in reverse readiness so only the write queue enforces order.
		const writable = {
			async write(chunk: Uint8Array): Promise<void> {
				const line = new TextDecoder().decode(chunk).trimEnd();
				const frame = JSON.parse(line) as { id: number };
				const gate = gates[frame.id - 1];
				if (gate === undefined) {
					throw new Error(`missing gate for id ${frame.id}`);
				}
				await gate.promise;
				order.push(frame.id);
			},
		};
		const client = new ProtocolClient(writable);
		const sends = Promise.all([
			client.send(requestFrame(1, "notify", { message: "a" })),
			client.send(requestFrame(2, "notify", { message: "b" })),
			client.send(requestFrame(3, "notify", { message: "c" })),
		]);
		// Unblock last-first; ordered queue must still write 1→2→3.
		gates[2]?.resolve();
		gates[1]?.resolve();
		gates[0]?.resolve();
		await sends;
		expect(order).toEqual([1, 2, 3]);
		client.dispose();
	});

	test("dispose rejects pending", async () => {
		const client = new ProtocolClient(new MemoryWritable());
		const p = client.request("hello", localHello());
		client.dispose("gone");
		await expect(p).rejects.toThrow(/gone/);
		expect(client.isDisposed).toBe(true);
	});

	test("reader loop correlates response bytes", async () => {
		const writable = new MemoryWritable();
		const client = new ProtocolClient(writable);
		async function* inbound() {
			await writable.whenChunks(1);
			const reqLine = new TextDecoder()
				.decode(writable.chunks[0] ?? new Uint8Array())
				.trimEnd();
			const req = JSON.parse(reqLine) as { id: number };
			yield encodeFrame(responseFrame(req.id, "hello", localHello()));
		}
		client.start(inbound());
		const res = await client.request("hello", localHello());
		expect(res.kind).toBe("res");
		expect(res.method).toBe("hello");
		await client.join();
	});
});
