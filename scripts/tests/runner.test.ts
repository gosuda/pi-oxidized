import { describe, expect, test } from "bun:test";

import {
	CommandFailedError,
	CommandOutputLimitError,
	CommandTimeoutError,
	OK_RUN,
	PathTraversalError,
	pathExists,
	RecordingRunner,
	RunResult,
	safeJoinPath,
	SpawnRunner,
	type Fs,
} from "../release/runner.ts";

describe("RecordingRunner", () => {
	test("records every call and returns the responder's result", async () => {
		const reply: RunResult = { exitCode: 0, stdout: "ok", stderr: "" };
		const runner = new RecordingRunner((call) => {
			if (call.command === "cargo" && call.args[0] === "metadata") return reply;
			return OK_RUN;
		});
		const a = await runner.run("cargo", ["metadata"]);
		const b = await runner.run("bun", ["build", "main.ts"]);
		expect(a).toEqual(reply);
		expect(b).toEqual(OK_RUN);
		expect(runner.calls).toHaveLength(2);
		expect(runner.calls[0]?.command).toBe("cargo");
		expect(runner.calls[0]?.args).toEqual(["metadata"]);
		expect(runner.calls[1]?.command).toBe("bun");
		expect(runner.calls[1]?.args).toEqual(["build", "main.ts"]);
	});

	test("falls back to OK_RUN when responder returns undefined", async () => {
		const runner = new RecordingRunner(() => undefined);
		const res = await runner.run("echo", ["hi"]);
		expect(res).toEqual(OK_RUN);
	});

	test("preserves the options object on the recorded call", async () => {
		const runner = new RecordingRunner(() => OK_RUN);
		await runner.run("bun", ["test"], { cwd: "/x", env: { A: "1" }, stdin: "in" });
		expect(runner.calls[0]?.options).toEqual({
			cwd: "/x",
			env: { A: "1" },
			stdin: "in",
		});
	});
});

describe("SpawnRunner integration", () => {
	test("captures stdout + exit code for a successful command", async () => {
		const runner = new SpawnRunner();
		const res = await runner.run(
			"sh",
			["-c", "printf hi; printf err 1>&2; exit 7"],
			{ rejectOnError: false },
		);
		expect(res.exitCode).toBe(7);
		expect(res.stdout).toBe("hi");
		expect(res.stderr).toBe("err");
	});

	test("throws CommandFailedError on nonzero when rejectOnError is set", async () => {
		const runner = new SpawnRunner();
		let caught: unknown;
		try {
			await runner.run("sh", ["-c", "exit 3"], { rejectOnError: true });
		} catch (err) {
			caught = err;
		}
		expect(caught).toBeInstanceOf(CommandFailedError);
		if (caught instanceof CommandFailedError) {
			expect(caught.command).toBe("sh");
			expect(caught.exitCode).toBe(3);
		}
	});

	test("forwards stdin to the child", async () => {
		const runner = new SpawnRunner();
		const res = await runner.run("cat", [], { stdin: "from-stdin" });
		expect(res.stdout).toBe("from-stdin");
	});

	test("terminates a hung child at the configured deadline", async () => {
		const runner = new SpawnRunner();
		const started = Date.now();
		let caught: unknown;
		try {
			await runner.run("sh", ["-c", "printf started; sleep 30"], { timeoutMs: 100 });
		} catch (err) {
			caught = err;
		}
		expect(caught).toBeInstanceOf(CommandTimeoutError);
		if (caught instanceof CommandTimeoutError) {
			expect(caught.command).toBe("sh");
			expect(caught.timeoutMs).toBe(100);
			expect(caught.stdout).toBe("started");
		}
		expect(Date.now() - started).toBeLessThan(5_000);
	});

	test("kills a child once combined output exceeds maxOutputBytes", async () => {
		const runner = new SpawnRunner();
		let caught: unknown;
		try {
			await runner.run(
				"sh",
				["-c", "while true; do printf 'xxxxxxxx'; done"],
				{ maxOutputBytes: 64, rejectOnError: false },
			);
		} catch (err) {
			caught = err;
		}
		expect(caught).toBeInstanceOf(CommandOutputLimitError);
		if (caught instanceof CommandOutputLimitError) {
			expect(caught.command).toBe("sh");
			expect(caught.maxOutputBytes).toBe(64);
			const retained = Buffer.byteLength(caught.stdout) + Buffer.byteLength(caught.stderr);
			expect(retained).toBeLessThanOrEqual(64);
			expect(retained).toBeGreaterThan(0);
		}
	});

	test("SEC-005: retention ceiling throws without awaiting a detached pipe holder", async () => {
		const runner = new SpawnRunner();
		const started = Date.now();
		let caught: unknown;
		try {
			// Fifo writer keeps the pipe open after the process is killed unless readers are destroyed.
			await runner.run(
				"sh",
				["-c", "yes xxxxxxxxx | dd bs=1 status=none"],
				{ maxOutputBytes: 32, rejectOnError: false },
			);
		} catch (err) {
			caught = err;
		}
		expect(caught).toBeInstanceOf(CommandOutputLimitError);
		if (caught instanceof CommandOutputLimitError) {
			const retained = Buffer.byteLength(caught.stdout) + Buffer.byteLength(caught.stderr);
			expect(retained).toBeLessThanOrEqual(32);
		}
		expect(Date.now() - started).toBeLessThan(5_000);
	});

	test("rejects non-positive deadlines before spawning", async () => {
		const runner = new SpawnRunner();
		await expect(
			runner.run("sh", ["-c", "exit 0"], { timeoutMs: 0 }),
		).rejects.toThrow(RangeError);
	});
});

describe("safeJoinPath", () => {
	test("accepts POSIX paths contained by the base", () => {
		expect(safeJoinPath("/staging", "sub/file.txt")).toBe("/staging/sub/file.txt");
		expect(safeJoinPath("/staging", ".")).toBe("/staging");
	});

	test("rejects POSIX parent and absolute-target escapes", () => {
		expect(() => safeJoinPath("/staging", "../escape")).toThrow(PathTraversalError);
		expect(() => safeJoinPath("/staging", "/etc/passwd")).toThrow(PathTraversalError);
		expect(() => safeJoinPath("/staging", "/staging-evil/file")).toThrow(PathTraversalError);
	});

	test("accepts win32 paths contained by the base", () => {
		expect(safeJoinPath("C:\\staging", "sub\\file.txt")).toBe(
			"C:\\staging\\sub\\file.txt",
		);
		expect(safeJoinPath("C:\\staging", ".")).toBe("C:\\staging");
	});

	test("rejects win32 parent and cross-drive escapes", () => {
		expect(() => safeJoinPath("C:\\staging", "..\\escape")).toThrow(
			PathTraversalError,
		);
		expect(() => safeJoinPath("C:\\staging", "D:escape")).toThrow(
			PathTraversalError,
		);
	});

	test("rejects embedded null bytes and POSIX backslashes", () => {
		expect(() => safeJoinPath("/staging", "x\0y")).toThrow(PathTraversalError);
		expect(() => safeJoinPath("/staging", "win\\path")).toThrow(PathTraversalError);
	});
});

describe("pathExists", () => {
	test("returns true when stat resolves", async () => {
		const fs: Fs = {
			async mkdir() {},
			async rm() {},
			async writeFile() {},
			async readFile() {
				return new Uint8Array();
			},
			async copyFile() {},
			async cp() {},
			async chmod() {},
			async stat(p) {
				if (p === "/a") return { isFile: true, isDir: false, size: 1, mode: 0 };
				throw new Error(`ENOENT: ${p}`);
			},
			async readdir() {
				return [];
			},
		};
		expect(await pathExists(fs, "/a")).toBe(true);
	});

	test("returns false when stat throws", async () => {
		const fs: Fs = {
			async mkdir() {},
			async rm() {},
			async writeFile() {},
			async readFile() {
				return new Uint8Array();
			},
			async copyFile() {},
			async cp() {},
			async chmod() {},
			async stat() {
				throw new Error("ENOENT");
			},
			async readdir() {
				return [];
			},
		};
		expect(await pathExists(fs, "/missing")).toBe(false);
	});
});
