import { describe, expect, test } from "bun:test";

import { assembleRelease, stagedInputs } from "../release/stage.ts";
import { planFor } from "../release/targets.ts";
import type { Fs, FsStat } from "../release/runner.ts";

/** In-memory filesystem for testing the assembly stage. */
class MemoryFs implements Fs {
	readonly files = new Map<string, Uint8Array>();
	readonly modes = new Map<string, number>();
	readonly chmodCalls: { path: string; mode: number }[] = [];

	async mkdir(_path: string, _opts?: { recursive?: boolean }): Promise<void> {}
	async rm(_path: string, _opts?: { recursive?: boolean; force?: boolean }): Promise<void> {}

	async writeFile(path: string, data: Uint8Array | string): Promise<void> {
		const bytes = typeof data === "string" ? new TextEncoder().encode(data) : data;
		this.files.set(path, new Uint8Array(bytes));
	}

	async readFile(path: string): Promise<Uint8Array> {
		const data = this.files.get(path);
		if (!data) throw new Error(`ENOENT: ${path}`);
		return data;
	}

	async copyFile(src: string, dest: string): Promise<void> {
		const data = await this.readFile(src);
		this.files.set(dest, data);
		const mode = this.modes.get(src);
		if (mode !== undefined) this.modes.set(dest, mode);
	}

	async cp(_src: string, _dest: string, _opts?: { recursive?: boolean }): Promise<void> {
		// Mock cp for directories: just copy whatever starts with src.
		// For unit tests, we pre-populate the dest directly if needed.
	}

	async chmod(path: string, mode: number): Promise<void> {
		this.chmodCalls.push({ path, mode });
		this.modes.set(path, mode);
	}

	async stat(path: string): Promise<FsStat> {
		const data = this.files.get(path);
		if (data) {
			const mode = this.modes.get(path) ?? 0o644;
			return { isFile: true, isDir: false, size: data.length, mode };
		}
		// Treat anything else as a dir for testing tree traversal.
		return { isFile: false, isDir: true, size: 0, mode: 0o755 };
	}

	async readdir(path: string): Promise<string[]> {
		const entries = new Set<string>();
		for (const k of this.files.keys()) {
			if (k.startsWith(path + "/")) {
				const rest = k.slice(path.length + 1);
				entries.add(rest.split("/")[0] ?? "");
			}
		}
		return [...entries];
	}
}

describe("assembleRelease", () => {
	test("assembles a deterministic manifest and verifies file order", async () => {
		const fs = new MemoryFs();
		const plan = planFor("aarch64-unknown-linux-gnu");

		// Setup mock source files.
		fs.files.set("/workspace/target/aarch64-unknown-linux-gnu/release/pi", new Uint8Array([1]));
		fs.modes.set("/workspace/target/aarch64-unknown-linux-gnu/release/pi", 0o755);
		fs.files.set("/staging/pi-extension-host", new Uint8Array([2]));
		fs.modes.set("/staging/pi-extension-host", 0o755);

		// Required metadata + optional files.
		fs.files.set("/workspace/CHANGELOG.md", new Uint8Array([5]));
		fs.files.set("/workspace/LICENSE", new Uint8Array([3]));
		fs.files.set("/workspace/README.md", new Uint8Array([4]));
		fs.files.set("/workspace/docs/intro.md", new Uint8Array([6]));

		const inputs = {
			plan,
			version: "1.0.0",
			piBinaryPath: "/workspace/target/aarch64-unknown-linux-gnu/release/pi",
			repoRoot: "/workspace",
			host: { kind: "compiled" as const, binaryPath: "/staging/pi-extension-host" },
			bunRuntimePath: undefined,
			fs,
			sourceDateEpoch: 1000,
			compatibilityVersion: "0.8",
			protocolVersion: 1,
			createdAt: "2024-01-01T00:00:00Z",
			docsSource: "/workspace/docs",
			assetsSource: "/workspace/assets",
		};

		const assembly1 = await assembleRelease("/staging", inputs);
		const m1Bytes = new Uint8Array(await fs.readFile("/staging/pi-linux-arm64/release.json"));
		
		const assembly2 = await assembleRelease("/staging", inputs);
		const m2Bytes = new Uint8Array(await fs.readFile("/staging/pi-linux-arm64/release.json"));
		
		expect(assembly1.stagingDir).toBe("/staging/pi-linux-arm64");
		// Byte-for-byte identical manifest JSON.
		expect(m1Bytes).toEqual(m2Bytes);

		// Manifest structure.
		const m1 = assembly1.manifest;
		expect(m1.schema).toBe("pi.release.v1");
		expect(m1.version).toBe("1.0.0");
		expect(m1.rustTarget).toBe("aarch64-unknown-linux-gnu");
		expect(m1.bunTarget).toBe("bun-linux-arm64");

		// Canonical file order (sorted by path).
		const paths = m1.files.map((f) => f.path);
		const sortedPaths = [...paths].sort();
		expect(paths).toEqual(sortedPaths);

		// Includes the expected files.
		expect(paths).toContain("pi");
		expect(paths).toContain("pi-extension-host");
		expect(paths).toContain("CHANGELOG.md");
		expect(paths).toContain("LICENSE");
		expect(paths).toContain("README.md");
		// Executable bits.
		const piEntry = m1.files.find((f) => f.path === "pi");
		expect(piEntry?.executable).toBe(true);
		const readmeEntry = m1.files.find((f) => f.path === "README.md");
		expect(readmeEntry?.executable).toBe(false);
	});

	test("skips chmod on Windows targets", async () => {
		const fs = new MemoryFs();
		const plan = planFor("x86_64-pc-windows-msvc");

		fs.files.set("/workspace/target/x86_64-pc-windows-msvc/release/pi.exe", new Uint8Array([1]));
		fs.files.set("/staging/pi-extension-host.exe", new Uint8Array([2]));
		fs.files.set("/workspace/CHANGELOG.md", new Uint8Array([5]));
		fs.files.set("/workspace/README.md", new Uint8Array([4]));

		const inputs = {
			plan,
			version: "1.0.0",
			piBinaryPath: "/workspace/target/x86_64-pc-windows-msvc/release/pi.exe",
			repoRoot: "/workspace",
			host: { kind: "compiled" as const, binaryPath: "/staging/pi-extension-host.exe" },
			bunRuntimePath: undefined,
			fs,
			sourceDateEpoch: 1000,
			compatibilityVersion: "0.8",
			protocolVersion: 1,
			createdAt: "2024-01-01T00:00:00Z",
			docsSource: "/workspace/docs",
			assetsSource: "/workspace/assets",
		};

		await assembleRelease("/staging", inputs);

		// Ensure no chmod calls were made because it's Windows.
		expect(fs.chmodCalls).toHaveLength(0);
	});

	test("assembles the provisioned Bun runtime and JavaScript fallback", async () => {
		const fs = new MemoryFs();
		const plan = planFor("x86_64-unknown-linux-gnu");
		const piPath = "/workspace/target/x86_64-unknown-linux-gnu/release/pi";
		const scriptPath = "/staging/host/pi-extension-host.js";
		const runtimePath = "/staging/host/bun";
		fs.files.set(piPath, new Uint8Array([1]));
		fs.files.set(scriptPath, new Uint8Array([2]));
		fs.files.set(runtimePath, new Uint8Array([3]));
		fs.modes.set(piPath, 0o755);
		fs.files.set("/workspace/CHANGELOG.md", new Uint8Array([5]));
		fs.files.set("/workspace/README.md", new Uint8Array([4]));
		fs.modes.set(runtimePath, 0o755);

		const assembly = await assembleRelease("/staging", {
			plan,
			version: "1.0.0",
			piBinaryPath: piPath,
			repoRoot: "/workspace",
			host: { kind: "runtime-bundle", runtimePath, scriptPath },
			bunRuntimePath: runtimePath,
			fs,
			sourceDateEpoch: 1000,
			compatibilityVersion: "0.80.10",
			protocolVersion: 1,
			createdAt: "2024-01-01T00:00:00Z",
			docsSource: "/workspace/docs",
		});

		expect(assembly.manifest.hostKind).toBe("runtime-bundle");
		expect(assembly.manifest.files.map((file) => file.path)).toEqual([
			"CHANGELOG.md",
			"README.md",
			"bun",
			"pi",
			"pi-extension-host.js",
		]);
		expect(assembly.manifest.files.find((file) => file.path === "bun")?.executable).toBe(
			true,
		);
		expect(
			assembly.manifest.files.find((file) => file.path === "pi-extension-host.js")
				?.executable,
		).toBe(false);
	});

	test("stages the compiled sidecar and the musl fallback beside it", () => {
		const plan = planFor("x86_64-unknown-linux-musl");
		const base = {
			plan,
			version: "1.0.0",
			piBinaryPath: "/workspace/target/x86_64-unknown-linux-musl/release/pi",
			repoRoot: "/workspace",
			fs: new MemoryFs(),
			sourceDateEpoch: 1000,
			compatibilityVersion: "0.80.10",
			protocolVersion: 1,
			createdAt: "2024-01-01T00:00:00Z",
			docsSource: "/workspace/docs",
			assetsSource: "/workspace/assets",
		};

		expect(
			stagedInputs({
				...base,
				host: { kind: "compiled", binaryPath: "/staging/pi-extension-host" },
				fallbackBundle: {
					scriptPath: "/staging/host/pi-extension-host.js",
					bunRuntimePath: "/staging/host/bun",
				},
			}).map((entry) => [entry.kind, entry.source, entry.destRel, entry.optional]),
		).toEqual([
			["rust-binary", base.piBinaryPath, "pi", false],
			["host-binary", "/staging/pi-extension-host", "pi-extension-host", false],
			["host-bundle", "/staging/host/pi-extension-host.js", "pi-extension-host.js", false],
			["bun-runtime", "/staging/host/bun", "bun", false],
			["metadata-file", "/workspace/CHANGELOG.md", "CHANGELOG.md", false],
			["metadata-file", "/workspace/README.md", "README.md", false],
			["metadata-file", "/workspace/LICENSE", "LICENSE", true],
			["metadata-file", "/workspace/LICENSE-MIT", "LICENSE-MIT", true],
			["tree", "/workspace/docs", "docs", false],
			["tree", "/workspace/assets", "assets", true],
			["tree", "/workspace/crates/pi/assets/theme", "theme", true],
			["manifest", "generated:release.json", "release.json", false],
		]);
	});
	test("stages both musl host execution paths into one manifest", async () => {
		const fs = new MemoryFs();
		const plan = planFor("x86_64-unknown-linux-musl");
		const piPath = "/workspace/target/x86_64-unknown-linux-musl/release/pi";
		fs.files.set(piPath, new Uint8Array([1]));
		fs.modes.set(piPath, 0o755);
		fs.files.set("/staging/pi-extension-host", new Uint8Array([2]));
		fs.modes.set("/staging/pi-extension-host", 0o755);
		fs.files.set("/staging/host/pi-extension-host.js", new Uint8Array([3]));
		fs.files.set("/staging/host/bun", new Uint8Array([4]));
		fs.files.set("/workspace/CHANGELOG.md", new Uint8Array([5]));
		fs.files.set("/workspace/README.md", new Uint8Array([4]));
		fs.modes.set("/staging/host/bun", 0o755);

		const assembly = await assembleRelease("/staging", {
			plan,
			version: "1.0.0",
			piBinaryPath: piPath,
			repoRoot: "/workspace",
			host: { kind: "compiled" as const, binaryPath: "/staging/pi-extension-host" },
			fallbackBundle: {
				scriptPath: "/staging/host/pi-extension-host.js",
				bunRuntimePath: "/staging/host/bun",
			},
			fs,
			sourceDateEpoch: 1000,
			compatibilityVersion: "0.80.10",
			protocolVersion: 1,
			createdAt: "2024-01-01T00:00:00Z",
			docsSource: "/workspace/docs",
		});

		expect(assembly.manifest.hostKind).toBe("compiled");
		expect(assembly.manifest.files.map((file) => file.path)).toEqual([
			"CHANGELOG.md",
			"README.md",
			"bun",
			"pi",
			"pi-extension-host",
			"pi-extension-host.js",
		]);
		expect(assembly.manifest.files.find((file) => file.path === "bun")?.executable).toBe(
			true,
		);
	});

	test("rejects a runtime bundle without a provisioned Bun path", async () => {
		const fs = new MemoryFs();
		const plan = planFor("x86_64-unknown-linux-gnu");
		const piPath = "/workspace/target/x86_64-unknown-linux-gnu/release/pi";
		const scriptPath = "/staging/host/pi-extension-host.js";
		fs.files.set(piPath, new Uint8Array([1]));
		fs.files.set(scriptPath, new Uint8Array([2]));
		fs.modes.set(piPath, 0o755);

		const inputs = {
			plan,
			version: "1.0.0",
			piBinaryPath: piPath,
			repoRoot: "/workspace",
			host: { kind: "runtime-bundle" as const, runtimePath: "/missing/bun", scriptPath },
			fs,
			sourceDateEpoch: 1000,
			compatibilityVersion: "0.80.10",
			protocolVersion: 1,
			createdAt: "2024-01-01T00:00:00Z",
			docsSource: "/workspace/docs",
		};
		await expect(
			assembleRelease("/staging", { ...inputs, bunRuntimePath: undefined }),
		).rejects.toThrow("requires bunRuntimePath");
		await expect(
			assembleRelease("/staging", { ...inputs, bunRuntimePath: "/missing/bun" }),
		).rejects.toThrow("ENOENT: /missing/bun");
	});
});

describe("stagedInputs", () => {
	test("exposes the ordered staging authority for compiled and fallback hosts", () => {
		const plan = planFor("aarch64-unknown-linux-gnu");
		const base = {
			plan,
			version: "1.0.0",
			piBinaryPath: "/workspace/target/aarch64-unknown-linux-gnu/release/pi",
			repoRoot: "/workspace",
			fs: new MemoryFs(),
			sourceDateEpoch: 1000,
			compatibilityVersion: "0.8",
			protocolVersion: 1,
			createdAt: "2024-01-01T00:00:00Z",
			docsSource: "/workspace/docs",
			assetsSource: "/workspace/assets",
			extraFiles: [{ src: "/tmp/extra.txt", dest: "notes/extra.txt" }],
		};

		expect(
			stagedInputs({
				...base,
				host: { kind: "compiled", binaryPath: "/staging/pi-extension-host" },
			}).map((entry) => [entry.kind, entry.source, entry.destRel, entry.optional]),
		).toEqual([
			["rust-binary", base.piBinaryPath, "pi", false],
			["host-binary", "/staging/pi-extension-host", "pi-extension-host", false],
			["metadata-file", "/workspace/CHANGELOG.md", "CHANGELOG.md", false],
			["metadata-file", "/workspace/README.md", "README.md", false],
			["metadata-file", "/workspace/LICENSE", "LICENSE", true],
			["metadata-file", "/workspace/LICENSE-MIT", "LICENSE-MIT", true],
			["tree", "/workspace/docs", "docs", false],
			["tree", "/workspace/assets", "assets", true],
			["tree", "/workspace/crates/pi/assets/theme", "theme", true],
			["extra", "/tmp/extra.txt", "notes/extra.txt", false],
			["manifest", "generated:release.json", "release.json", false],
		]);

		expect(
			stagedInputs({
				...base,
				host: {
					kind: "runtime-bundle",
					runtimePath: "/staging/bun",
					scriptPath: "/staging/pi-extension-host.js",
				},
				bunRuntimePath: "/official/bun",
			}).map((entry) => [entry.kind, entry.source, entry.destRel, entry.optional]),
		).toEqual([
			["rust-binary", base.piBinaryPath, "pi", false],
			["host-bundle", "/staging/pi-extension-host.js", "pi-extension-host.js", false],
			["bun-runtime", "/official/bun", "bun", false],
			["metadata-file", "/workspace/CHANGELOG.md", "CHANGELOG.md", false],
			["metadata-file", "/workspace/README.md", "README.md", false],
			["metadata-file", "/workspace/LICENSE", "LICENSE", true],
			["metadata-file", "/workspace/LICENSE-MIT", "LICENSE-MIT", true],
			["tree", "/workspace/docs", "docs", false],
			["tree", "/workspace/assets", "assets", true],
			["tree", "/workspace/crates/pi/assets/theme", "theme", true],
			["extra", "/tmp/extra.txt", "notes/extra.txt", false],
			["manifest", "generated:release.json", "release.json", false],
		]);
	});
});
