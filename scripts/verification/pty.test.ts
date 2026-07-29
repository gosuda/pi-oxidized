import { expect, test } from "bun:test";
import { writeTerminalSafe } from "./pty.ts";

test("writeTerminalSafe reports synchronous write failures", () => {
	const errors: unknown[] = [];
	const terminal = {
		closed: false,
		write: () => {
			throw new Error("sync write failure");
		},
	};
	writeTerminalSafe(terminal, "data", (error) => errors.push(error));
	expect(errors).toHaveLength(1);
	expect(String(errors[0])).toContain("sync write failure");
});

test("writeTerminalSafe swallows async close-race rejections without unhandled rejection", async () => {
	const unhandled: unknown[] = [];
	const writeErrors: unknown[] = [];
	const handler = (reason: unknown): void => {
		unhandled.push(reason);
	};
	process.on("unhandledRejection", handler);
	try {
		const { promise, reject } = Promise.withResolvers<number>();
		const terminal = {
			closed: false,
			write: () => promise,
		};
		writeTerminalSafe(terminal, "data", (error) => writeErrors.push(error));
		terminal.closed = true;
		reject(new Error("write after close"));
		await new Promise<void>((resolve) => setImmediate(resolve));
		expect(unhandled).toHaveLength(0);
		expect(writeErrors).toHaveLength(0);
	} finally {
		process.off("unhandledRejection", handler);
	}
});


test("writeTerminalSafe reports async failures while the terminal is open", async () => {
	const errors: unknown[] = [];
	const { promise, reject } = Promise.withResolvers<number>();
	const terminal = {
		closed: false,
		write: () => promise,
	};
	writeTerminalSafe(terminal, "data", (error) => errors.push(error));
	reject(new Error("disk full"));
	await new Promise<void>((resolve) => setImmediate(resolve));
	expect(errors).toHaveLength(1);
	expect(String(errors[0])).toContain("disk full");
});

test("writeTerminalSafe is a no-op for empty data or closed terminal", () => {
	let writeCount = 0;
	const openTerminal = {
		closed: false,
		write: () => {
			writeCount++;
			return 0;
		},
	};
	writeTerminalSafe(openTerminal, "");
	expect(writeCount).toBe(0);

	const closedTerminal = {
		closed: true,
		write: () => {
			writeCount++;
			return 0;
		},
	};
	writeTerminalSafe(closedTerminal, "data");
	expect(writeCount).toBe(0);
});
