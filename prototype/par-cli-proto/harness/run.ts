#!/usr/bin/env bun
/**
 * PAR-CLI-PROTO harness runner.
 *
 * Drives the upstream dist/cli.js for each OAuth provider in the matrix,
 * using the fetch-shim preload to intercept all OAuth endpoint calls.
 * Stdin is scripted per-provider.  stdout/stderr/exit-code/auth.json are
 * captured as goldens.  The runner is self-verifying: it re-runs each
 * provider and diffs against the golden to confirm replay stability.
 *
 * Usage:
 *   bun harness/run.ts                    # run all providers, generate goldens
 *   bun harness/run.ts <provider-id>      # run one provider
 *   bun harness/run.ts --verify           # re-run all, diff against goldens
 *   bun harness/run.ts --regenerate       # overwrite goldens from upstream
 */

import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { $ } from "bun";

const ROOT = import.meta.dir;
const PROTO_DIR = dirname(ROOT); // prototype/par-cli-proto
const FIXTURES_DIR = join(PROTO_DIR, "fixtures");
const STDIN_DIR = join(PROTO_DIR, "stdin");
const GOLDENS_DIR = join(PROTO_DIR, "goldens");
const SHIM_PATH = join(ROOT, "fetch-shim.ts");
const MANIFEST_PATH = join(PROTO_DIR, "matrix-manifest.json");

const UPSTREAM_CLI = join(
	import.meta.dir,
	"..", "..", "..",
	".references", "pi", "packages", "ai", "dist", "cli.js",
);

type ManifestProvider = {
	id: string;
	name: string;
	flow_type: string;
	stdin_script: string;
	fixture: string;
	golden_dir: string;
};

type Manifest = {
	providers: ManifestProvider[];
};

type RunResult = {
	provider: string;
	stdout: string;
	stderr: string;
	exitCode: number;
	authJson: string | null;
};

function loadManifest(): Manifest {
	return JSON.parse(readFileSync(MANIFEST_PATH, "utf-8")) as Manifest;
}

async function runProvider(provider: ManifestProvider): Promise<RunResult> {
	const fixturePath = join(PROTO_DIR, provider.fixture);
	const stdinPath = join(PROTO_DIR, provider.stdin_script);
	const stdinContent = readFileSync(stdinPath, "utf-8");

	const tmpDir = `${PROTO_DIR}/.tmp-${provider.id}`;
	if (existsSync(tmpDir)) rmSync(tmpDir, { recursive: true });
	mkdirSync(tmpDir, { recursive: true });

	const env: Record<string, string> = {
		...process.env,
		MOCK_PROVIDER: provider.id,
		MOCK_FIXTURE_PATH: fixturePath,
		MOCK_SHIM_DEBUG: "0",
	};

	// Use a pipe for stdin. Delay writing the scripted input so the child
	// process has time to reach the readline question prompt. If we write
	// too early, readline consumes the data before rl.question() is called
	// and the line event is lost. After writing, close stdin to signal EOF.
	const proc = Bun.spawn({
		cmd: ["bun", "--preload", SHIM_PATH, UPSTREAM_CLI, "login", provider.id],
		cwd: tmpDir,
		env,
		stdin: "pipe",
		stdout: "pipe",
		stderr: "pipe",
	});

	// Wait for the child to reach its prompt, then write stdin and close.
	// Device-code flows reach their prompt quickly; browser-PKCE flows
	// need time for callback server setup before the manual_code prompt.
	Bun.sleep(2000).then(() => {
		proc.stdin.write(stdinContent);
		proc.stdin.flush();
		proc.stdin.end();
	});

	const [stdout, stderr, exitCode] = await Promise.all([
		new Response(proc.stdout).text(),
		new Response(proc.stderr).text(),
		proc.exited,
	]);

	let authJson: string | null = null;
	const authPath = join(tmpDir, "auth.json");
	if (existsSync(authPath)) {
		authJson = readFileSync(authPath, "utf-8");
	}

	rmSync(tmpDir, { recursive: true, force: true });

	return { provider: provider.id, stdout, stderr, exitCode, authJson };
}

function saveGoldens(provider: ManifestProvider, result: RunResult): void {
	const goldenDir = join(GOLDENS_DIR, provider.id);
	if (existsSync(goldenDir)) rmSync(goldenDir, { recursive: true });
	mkdirSync(goldenDir, { recursive: true });

	writeFileSync(join(goldenDir, "stdout.txt"), result.stdout);
	writeFileSync(join(goldenDir, "stderr.txt"), result.stderr);
	writeFileSync(join(goldenDir, "exit-code.txt"), String(result.exitCode));
	if (result.authJson) {
		writeFileSync(join(goldenDir, "auth.json"), result.authJson);
	}
}

function loadGoldens(provider: ManifestProvider): RunResult {
	const goldenDir = join(GOLDENS_DIR, provider.id);
	return {
		provider: provider.id,
		stdout: readFileSync(join(goldenDir, "stdout.txt"), "utf-8"),
		stderr: readFileSync(join(goldenDir, "stderr.txt"), "utf-8"),
		exitCode: Number.parseInt(readFileSync(join(goldenDir, "exit-code.txt"), "utf-8"), 10),
		authJson: existsSync(join(goldenDir, "auth.json"))
			? readFileSync(join(goldenDir, "auth.json"), "utf-8")
			: null,
	};
}

function normalizeAuthJson(json: string): string {
	try {
		const parsed = JSON.parse(json) as Record<string, Record<string, unknown>>;
		for (const provider of Object.keys(parsed)) {
			const cred = parsed[provider];
			if (typeof cred.expires === "number") {
				cred.expires = "<TIMESTAMP>";
			}
		}
		return JSON.stringify(parsed, null, 2);
	} catch {
		return json;
	}
}

function normalizeStdout(text: string): string {
	return text
		// Replace PKCE code_challenge values (43-char base64url)
		.replace(/code_challenge=[A-Za-z0-9_-]{43}/g, "code_challenge=<PKCE>")
		// Replace state values (43-char base64url)
		.replace(/state=[A-Za-z0-9_-]{43}/g, "state=<STATE>")
		// Replace OpenRouter callback UUID paths (both raw and URL-encoded)
		.replace(/\/oauth\/callback\/[0-9a-f-]{36}/g, "/oauth/callback/<UUID>")
		.replace(/%2Foauth%2Fcallback%2F[0-9a-f-]{36}/g, "%2Foauth%2Fcallback%2F<UUID>")
		// Replace ports in callback URLs (ephemeral ports) — both raw and URL-encoded
		.replace(/(?:localhost|127\.0\.0\.1):\d+/g, "$1:<PORT>")
		.replace(/%3A\d+/g, "%3A<PORT>");
}

function diffResults(golden: RunResult, actual: RunResult): string[] {
	const diffs: string[] = [];
	if (golden.exitCode !== actual.exitCode) {
		diffs.push(`exit-code: golden=${golden.exitCode} actual=${actual.exitCode}`);
	}
	const goldenStdout = normalizeStdout(golden.stdout);
	const actualStdout = normalizeStdout(actual.stdout);
	if (goldenStdout !== actualStdout) {
		diffs.push(`stdout differs (normalized)`);
	}
	if (golden.stderr !== actual.stderr) {
		diffs.push(`stderr differs`);
	}
	const goldenAuth = golden.authJson ? normalizeAuthJson(golden.authJson) : null;
	const actualAuth = actual.authJson ? normalizeAuthJson(actual.authJson) : null;
	if (goldenAuth !== actualAuth) {
		diffs.push(`auth.json differs (normalized)`);
	}
	return diffs;
}

async function main(): Promise<void> {
	const args = process.argv.slice(2);
	const mode = args[0] === "--verify" ? "verify" : args[0] === "--regenerate" ? "regenerate" : "generate";
	const filterProvider = args.find((a) => !a.startsWith("--"));

	const manifest = loadManifest();
	const providers = filterProvider
		? manifest.providers.filter((p) => p.id === filterProvider)
		: manifest.providers;

	if (providers.length === 0) {
		process.stderr.write(`No providers matched filter: ${filterProvider ?? "(none)"}\n`);
		process.exit(1);
	}

	if (mode === "generate" || mode === "regenerate") {
		for (const provider of providers) {
			process.stderr.write(`[generate] ${provider.id}... `);
			const result = await runProvider(provider);
		saveGoldens(provider, result);
		process.stderr.write(`exit=${result.exitCode} auth=${result.authJson ? "yes" : "no"}\n`);
		// Delay between providers to avoid callback port conflicts (TIME_WAIT)
		if (providers.length > 1) await Bun.sleep(1000);
		}
		process.stderr.write(`\nGoldens saved to ${GOLDENS_DIR}\n`);
		return;
	}

	// verify mode
	let allPass = true;
	for (const provider of providers) {
		process.stderr.write(`[verify] ${provider.id}... `);
		const golden = loadGoldens(provider);
		const actual = await runProvider(provider);
		const diffs = diffResults(golden, actual);
		if (diffs.length === 0) {
			process.stderr.write("PASS\n");
		} else {
			allPass = false;
			process.stderr.write(`FAIL\n`);
		for (const d of diffs) process.stderr.write(`  - ${d}\n`);
		}
		// Delay between providers to avoid callback port conflicts (TIME_WAIT)
		if (providers.length > 1) await Bun.sleep(1000);
	}
	process.stderr.write(`\n${allPass ? "ALL PASS" : "FAILURES"}\n`);
	process.exit(allPass ? 0 : 1);
}

main().catch((error: unknown) => {
	process.stderr.write(`Fatal: ${error instanceof Error ? error.message : String(error)}\n`);
	process.exit(1);
});
