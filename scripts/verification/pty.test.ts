import { afterAll, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { PTY_KEYS, spawnPty } from "./pty.ts";

const isWindows = process.platform === "win32";


const temporaryPaths: string[] = [];

afterAll(() => {
	for (const path of temporaryPaths) rmSync(path, { recursive: true, force: true });
});

function temporaryDirectory(prefix: string): string {
	const path = mkdtempSync(join(tmpdir(), prefix));
	temporaryPaths.push(path);
	return path;
}

describe.skipIf(isWindows)("PTY driver", () => {
	test("preserves hostile argv, separates terminal echo, timestamps chunks, and exits cleanly", async () => {
		const root = temporaryDirectory("pi pty ' $() ");
		const process = spawnPty({
			argv: ["/bin/sh", "-c", "stty echo; printf APP_READY; IFS= read -r line; printf '\\nAPP:%s\\n' \"$line\""],
			cwd: root,
		});
		try {
			await process.waitFor(/APP_READY/, { deadlineMs: 5_000, source: "raw" });
			process.writeKeys("ECHO_SENT", PTY_KEYS.enter);
			const snapshot = await process.waitFor(/APP:ECHO_SENT/, { deadlineMs: 5_000 });
			expect(snapshot.echoText).toContain("ECHO_SENT");
			expect(snapshot.applicationText).toContain("APP:ECHO_SENT");
			expect(snapshot.chunks.length).toBeGreaterThan(0);
			expect(snapshot.chunks.every((chunk) => chunk.elapsedMs >= 0 && chunk.unixMs > 0)).toBe(true);
			expect(await process.waitForExit(5_000)).toBe(0);
		} finally {
			await process.terminate();
		}
	}, 15_000);

	test("terminates the complete PTY process group", async () => {
		const process = spawnPty({
			argv: ["/bin/sh", "-c", "sleep 300 & printf 'CHILD:%s\\n' \"$!\"; wait"],
			cwd: temporaryDirectory("pi-verification-tree-"),
		});
		try {
			const snapshot = await process.waitFor(/CHILD:(\d+)/, { deadlineMs: 5_000, source: "raw" });
			const match = /CHILD:(\d+)/.exec(snapshot.rawText);
			if (!match?.[1]) throw new Error("child pid was not reported");
			const childPid = Number(match[1]);
			await process.terminate();
			expect(() => globalThis.process.kill(childPid, 0)).toThrow();
		} finally {
			await process.terminate();
		}
	}, 15_000);
});
