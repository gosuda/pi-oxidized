import {
	copyFileSync,
	existsSync,
	mkdirSync,
	mkdtempSync,
	readdirSync,
	readFileSync,
	statSync,
	writeFileSync,
} from "node:fs";
import { createHash } from "node:crypto";
import { basename, join, relative, resolve } from "node:path";
import {
	DEFAULT_FINAL_MARKER,
	VERIFICATION_MODEL,
	VERIFICATION_PROVIDER,
} from "./extension.ts";
import { PTY_KEYS, type PtyProcess, spawnPty } from "./pty.ts";

const REPO_ROOT = resolve(import.meta.dirname, "../..");
const EVIDENCE_ROOT = resolve(REPO_ROOT, "target/verification/e2e");
const RUST_BINARY = resolve(REPO_ROOT, "target/release/pi");
const EXTENSION_HOST = resolve(REPO_ROOT, "packages/extension-host/dist/pi-extension-host");
const EXTENSION_PATH = resolve(import.meta.dirname, "extension.ts");
const TYPESCRIPT_CLI = resolve(
	REPO_ROOT,
	".references/pi/packages/coding-agent/src/cli.ts",
);
const FINAL_MARKER = `${DEFAULT_FINAL_MARKER}_E2E`;
const STEERING_MARKER = "verification-steering-next-turn";
const TOOL_FILE = "verification-e2e.txt";
const TOOL_FILE_BEFORE = "verification-before\n";
const TOOL_FILE_AFTER = "verification-after\n";
const READY_DEADLINE_MS = 30_000;
const TURN_DEADLINE_MS = 180_000;
const COMMAND_DEADLINE_MS = 60_000;
const EXIT_DEADLINE_MS = 15_000;
const POLL_INTERVAL_MS = 25;

interface JsonObject {
	[key: string]: JsonValue;
}
type JsonValue = null | boolean | number | string | JsonValue[] | JsonObject;

interface SessionDocument {
	readonly path: string;
	readonly bytes: Uint8Array;
	readonly lines: readonly JsonObject[];
}

interface StepEvidence {
	name: string;
	startedAt: string;
	finishedAt?: string;
	detail?: JsonValue;
}

interface NamedProcess {
	readonly name: string;
	readonly process: PtyProcess;
}

interface WorkflowState {
	readonly runRoot: string;
	readonly homeDir: string;
	readonly agentDir: string;
	readonly sessionDir: string;
	readonly workDir: string;
	readonly loadCountPath: string;
	readonly toolPath: string;
	readonly steps: StepEvidence[];
	readonly processes: NamedProcess[];
	originalSessionPath?: string;
	forkSessionPath?: string;
	loadGeneration?: number;
}

function errorText(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}

function fail(message: string): never {
	throw new Error(message);
}

function assert(condition: boolean, message: string): asserts condition {
	if (!condition) fail(message);
}

function asObject(value: JsonValue | undefined, label: string): JsonObject {
	if (value === null || value === undefined || Array.isArray(value) || typeof value !== "object") {
		fail(`${label} must be an object`);
	}
	return value;
}

function asString(value: JsonValue | undefined, label: string): string {
	if (typeof value !== "string") fail(`${label} must be a string`);
	return value;
}

function sha256(bytes: Uint8Array): string {
	return createHash("sha256").update(bytes).digest("hex");
}

function isoNow(): string {
	return new Date().toISOString();
}

function beginStep(state: WorkflowState, name: string): StepEvidence {
	const step: StepEvidence = { name, startedAt: isoNow() };
	state.steps.push(step);
	return step;
}

function finishStep(step: StepEvidence, detail?: JsonValue): void {
	step.finishedAt = isoNow();
	if (detail !== undefined) step.detail = detail;
}

function ensurePrerequisites(): string {
	for (const required of [RUST_BINARY, EXTENSION_HOST, EXTENSION_PATH, TYPESCRIPT_CLI]) {
		if (!existsSync(required)) {
			fail(
				`product prerequisite missing: ${required}${
					required === RUST_BINARY ? "; run cargo build -p pi --release --locked" : ""
				}`,
			);
		}
	}
	const bun = Bun.which("bun");
	if (!bun) fail("product prerequisite missing: bun executable is required to reopen the Rust session with TypeScript pi");
	return bun;
}

function createState(): WorkflowState {
	mkdirSync(EVIDENCE_ROOT, { recursive: true });
	const runRoot = mkdtempSync(join(EVIDENCE_ROOT, "run-"));
	const state: WorkflowState = {
		runRoot,
		homeDir: join(runRoot, "home"),
		agentDir: join(runRoot, "agent"),
		sessionDir: join(runRoot, "sessions"),
		workDir: join(runRoot, "work"),
		loadCountPath: join(runRoot, "extension-load-generation.txt"),
		toolPath: join(runRoot, "work", TOOL_FILE),
		steps: [],
		processes: [],
	};
	for (const directory of [state.homeDir, state.agentDir, state.sessionDir, state.workDir]) {
		mkdirSync(directory, { recursive: true });
	}
	writeFileSync(state.toolPath, TOOL_FILE_BEFORE, "utf8");
	return state;
}

function processEnvironment(state: WorkflowState): Record<string, string> {
	return {
		HOME: state.homeDir,
		PI_CODING_AGENT_DIR: state.agentDir,
		PI_CODING_AGENT_SESSION_DIR: state.sessionDir,
		PI_EXTENSION_HOST: EXTENSION_HOST,
		PI_OFFLINE: "1",
		PI_VERIFICATION_MODE: "auto",
		// Two large deterministic turns place real history outside the 20k-token
		// keep window, so manual compaction has something observable to summarize.
		PI_VERIFICATION_CHUNK_COUNT: "5000",
		PI_VERIFICATION_CHUNK_DELAY_MS: "1",
		PI_VERIFICATION_FINAL_MARKER: FINAL_MARKER,
		PI_VERIFICATION_LOAD_COUNT_PATH: state.loadCountPath,
		PI_VERIFICATION_TOOL_PATH: TOOL_FILE,
	};
}

function commonArguments(): string[] {
	return [
		"--provider",
		VERIFICATION_PROVIDER,
		"--model",
		VERIFICATION_MODEL,
		"--api-key",
		"verification-key",
		"--extension",
		EXTENSION_PATH,
		"--offline",
		"--no-context-files",
		"--no-skills",
		"--no-prompt-templates",
		"--no-themes",
		"--approve",
	];
}

function launch(
	state: WorkflowState,
	name: string,
	argv: readonly [string, ...string[]],
): PtyProcess {
	const process = spawnPty({
		argv,
		cwd: state.workDir,
		env: processEnvironment(state),
		size: { columns: 120, rows: 48 },
	});
	state.processes.push({ name, process });
	return process;
}

async function waitForReady(name: string, process: PtyProcess): Promise<void> {
	try {
		await process.waitFor(
			(snapshot) =>
				snapshot.rawText.length > 100 &&
				(snapshot.rawText.includes("type") ||
					snapshot.rawText.includes("No messages") ||
					snapshot.rawText.includes(FINAL_MARKER) ||
					snapshot.rawText.includes("Compacted context")),
			{ deadlineMs: READY_DEADLINE_MS, source: "raw" },
		);
	} catch (error) {
		throw processBlocker(name, process, error);
	}
}

function processBlocker(name: string, process: PtyProcess, cause: unknown): Error {
	const snapshot = process.snapshot();
	const tail = snapshot.rawText.slice(-8_000);
	return new Error(
		`product blocker during ${name}: ${errorText(cause)}; exit=${String(snapshot.exitCode)}\nPTY tail:\n${tail}`,
	);
}

function sendLine(process: PtyProcess, text: string): void {
	process.writeKeys(text, PTY_KEYS.enter);
}

async function cleanQuit(name: string, process: PtyProcess): Promise<void> {
	if (process.exited) throw processBlocker(`${name} /quit`, process, "process exited before /quit");
	sendLine(process, "/quit");
	try {
		const exitCode = await process.waitForExit(EXIT_DEADLINE_MS);
		assert(exitCode === 0, `product blocker during ${name} /quit: expected exit 0, got ${exitCode}`);
	} catch (error) {
		throw processBlocker(`${name} /quit`, process, error);
	}
}

function listFiles(root: string): string[] {
	if (!existsSync(root)) return [];
	const files: string[] = [];
	const pending = [root];
	while (pending.length > 0) {
		const directory = pending.pop();
		if (directory === undefined) break;
		for (const entry of readdirSync(directory, { withFileTypes: true })) {
			const path = join(directory, entry.name);
			if (entry.isDirectory()) pending.push(path);
			else if (entry.isFile()) files.push(path);
		}
	}
	return files.sort();
}

function sessionFiles(state: WorkflowState): string[] {
	return listFiles(state.sessionDir).filter((path) => path.endsWith(".jsonl"));
}

function readSession(path: string): SessionDocument {
	const bytes = readFileSync(path);
	const text = bytes.toString("utf8");
	const lines = text
		.split(/\r?\n/)
		.map((line) => line.trim())
		.filter((line) => line.length > 0)
		.map((line, index) => {
			let parsed: JsonValue;
			try {
				parsed = JSON.parse(line) as JsonValue;
			} catch (error) {
				fail(`invalid session JSONL ${path}:${index + 1}: ${errorText(error)}`);
			}
			return asObject(parsed, `${path}:${index + 1}`);
		});
	return { path, bytes, lines };
}

function entryType(entry: JsonObject): string {
	return typeof entry.type === "string" ? entry.type : "";
}

function messageObject(entry: JsonObject): JsonObject | undefined {
	if (entryType(entry) !== "message") return undefined;
	const message = entry.message;
	if (message === null || Array.isArray(message) || typeof message !== "object") return undefined;
	return message;
}

function contentText(value: JsonValue | undefined): string {
	if (typeof value === "string") return value;
	if (!Array.isArray(value)) return "";
	return value
		.map((block) => {
			if (typeof block === "string") return block;
			if (block === null || Array.isArray(block) || typeof block !== "object") return "";
			return typeof block.text === "string" ? block.text : "";
		})
		.join("");
}

function messageText(entry: JsonObject): string {
	return contentText(messageObject(entry)?.content);
}

function messageRole(entry: JsonObject): string {
	const role = messageObject(entry)?.role;
	return typeof role === "string" ? role : "";
}

function toolName(entry: JsonObject): string {
	const name = messageObject(entry)?.toolName;
	return typeof name === "string" ? name : "";
}

function toolResult(session: SessionDocument, name: string): JsonObject | undefined {
	return session.lines.find(
		(entry) => messageRole(entry) === "toolResult" && toolName(entry) === name,
	);
}

function compactionEntry(session: SessionDocument): JsonObject | undefined {
	return session.lines.find((entry) => entryType(entry) === "compaction");
}

function loadGeneration(path: string): number {
	if (!existsSync(path)) return 0;
	const value = Number.parseInt(readFileSync(path, "utf8").trim(), 10);
	return Number.isSafeInteger(value) && value >= 0 ? value : 0;
}

async function waitUntil<T>(
	label: string,
	deadlineMs: number,
	check: () => T | undefined,
	process?: PtyProcess,
): Promise<T> {
	const deadline = performance.now() + deadlineMs;
	for (;;) {
		const result = check();
		if (result !== undefined) return result;
		if (process?.exited === true) throw processBlocker(label, process, "process exited while awaiting state");
		if (performance.now() >= deadline) fail(`product blocker: ${label} exceeded ${deadlineMs}ms`);
		await Bun.sleep(POLL_INTERVAL_MS);
	}
}

async function waitForSession(
	state: WorkflowState,
	label: string,
	process: PtyProcess,
	predicate: (session: SessionDocument) => boolean,
	deadlineMs = TURN_DEADLINE_MS,
): Promise<SessionDocument> {
	return waitUntil(
		label,
		deadlineMs,
		() => {
			for (const path of sessionFiles(state)) {
				const session = readSession(path);
				if (predicate(session)) return session;
			}
			return undefined;
		},
		process,
	);
}

async function waitForLoadGeneration(
	state: WorkflowState,
	minimum: number,
	process: PtyProcess,
): Promise<number> {
	return waitUntil(
		`extension load generation ${minimum}`,
		COMMAND_DEADLINE_MS,
		() => {
			const generation = loadGeneration(state.loadCountPath);
			return generation >= minimum ? generation : undefined;
		},
		process,
	);
}

function assertToolResults(session: SessionDocument, state: WorkflowState): void {
	const read = toolResult(session, "read");
	const edit = toolResult(session, "edit");
	const bash = toolResult(session, "bash");
	assert(read !== undefined, "read tool result was not persisted");
	assert(edit !== undefined, "edit tool result was not persisted");
	assert(bash !== undefined, "bash tool result was not persisted");
	for (const [name, entry] of [["read", read], ["edit", edit], ["bash", bash]] as const) {
		const message = asObject(entry.message, `${name} tool result message`);
		assert(message.isError === false, `${name} built-in tool returned an error: ${messageText(entry)}`);
		assert(messageText(entry).length > 0, `${name} built-in tool result was empty`);
	}
	assert(messageText(read).includes("verification-before"), "read tool result did not contain the original file content");
	assert(messageText(bash).includes("verification-bash"), "bash tool result did not contain real command output");
	assert(readFileSync(state.toolPath, "utf8") === TOOL_FILE_AFTER, "edit tool did not mutate the fixture file exactly");
}

function assertSteeringTurn(session: SessionDocument): void {
	const originalUser = session.lines.findIndex(
		(entry) => messageRole(entry) === "user" && messageText(entry).includes("verification:tools"),
	);
	const steer = session.lines.findIndex(
		(entry) => messageRole(entry) === "user" && messageText(entry) === STEERING_MARKER,
	);
	assert(originalUser >= 0, "initial tool prompt was not persisted");
	assert(steer > originalUser, "steering text was not persisted after the original prompt");
	const assistantBefore = session.lines.findLastIndex(
		(entry, index) => index < steer && messageRole(entry) === "assistant" && messageText(entry).includes(FINAL_MARKER),
	);
	const assistantAfter = session.lines.findIndex(
		(entry, index) => index > steer && messageRole(entry) === "assistant" && messageText(entry).includes(FINAL_MARKER),
	);
	assert(assistantBefore > originalUser, "streaming turn did not finish before queued steering was applied");
	assert(assistantAfter > steer, "steering did not cause and persist the next assistant turn");
	const steerEntry = session.lines[steer];
	const nextAssistant = session.lines[assistantAfter];
	assert(steerEntry !== undefined && typeof steerEntry.id === "string", "steering entry has no tree id");
	assert(nextAssistant !== undefined, "next assistant entry missing");
	assert(
		nextAssistant.parentId === steerEntry.id,
		"next assistant entry is not parented to the steering entry, so steering did not affect the next turn",
	);
}

function assertCompaction(entry: JsonObject): void {
	assert(
		typeof entry.summary === "string" && entry.summary.includes("Deterministic verification compaction"),
		"manual compaction summary did not come from the shared deterministic provider",
	);
	assert(
		typeof entry.firstKeptEntryId === "string" && entry.firstKeptEntryId.length > 0,
		"manual compaction entry is missing firstKeptEntryId",
	);
	assert(typeof entry.tokensBefore === "number", "manual compaction entry is missing tokensBefore");
}

async function runInitialSession(state: WorkflowState): Promise<SessionDocument> {
	const step = beginStep(state, "rust-interactive-tools-steering-compaction");
	const rust = launch(state, "rust-initial", [
		RUST_BINARY,
		...commonArguments(),
		"--session-dir",
		state.sessionDir,
	]);
	await waitForReady("Rust initial interactive startup", rust);
	state.loadGeneration = await waitForLoadGeneration(state, 1, rust);
	sendLine(rust, "verification:tools execute real read edit bash stages");

	const toolsSession = await waitForSession(
		state,
		"real read/edit/bash tool results",
		rust,
		(session) => ["read", "edit", "bash"].every((name) => toolResult(session, name) !== undefined),
	);
	assertToolResults(toolsSession, state);

	try {
		await rust.waitFor(/verification-chunk-\d{4}/, {
			deadlineMs: TURN_DEADLINE_MS,
			source: "raw",
		});
	} catch (error) {
		throw processBlocker("multi-chunk streaming before steering", rust, error);
	}
	sendLine(rust, STEERING_MARKER);

	const steeredSession = await waitForSession(
		state,
		"steering persistence and next turn",
		rust,
		(session) => {
			const steerIndex = session.lines.findIndex(
				(entry) => messageRole(entry) === "user" && messageText(entry) === STEERING_MARKER,
			);
			return steerIndex >= 0 && session.lines.some(
				(entry, index) => index > steerIndex && messageRole(entry) === "assistant" && messageText(entry).includes(FINAL_MARKER),
			);
		},
	);
	assertSteeringTurn(steeredSession);

	// This must be the public interactive command path. If the runtime incorrectly
	// forwards `/compact` to the model, the user entry below identifies the exact
	// product dispatch blocker instead of accepting a fake summary.
	sendLine(rust, "/compact");
	const compactedSession = await waitUntil(
		"interactive /compact dispatch and compaction entry",
		TURN_DEADLINE_MS,
		() => {
			const path = state.originalSessionPath ?? sessionFiles(state)[0];
			if (!path) return undefined;
			const session = readSession(path);
			const compacted = compactionEntry(session);
			if (compacted !== undefined) return session;
			if (
				session.lines.some(
					(entry) => messageRole(entry) === "user" && messageText(entry).trim() === "/compact",
				)
			) {
				fail(
					"product blocker: interactive `/compact` was persisted as a model user message instead of dispatching ViewAction::Compact",
				);
			}
			return undefined;
		},
		rust,
	);
	const compacted = compactionEntry(compactedSession);
	assert(compacted !== undefined, "manual compaction entry missing");
	assertCompaction(compacted);
	state.originalSessionPath = compactedSession.path;
	await cleanQuit("Rust initial session", rust);
	finishStep(step, {
		session: relative(state.runRoot, compactedSession.path),
		entries: compactedSession.lines.length,
		sha256: sha256(compactedSession.bytes),
	});
	return readSession(compactedSession.path);
}

async function runFork(state: WorkflowState, original: SessionDocument): Promise<SessionDocument> {
	const step = beginStep(state, "rust-fork");
	const priorFiles = new Set(sessionFiles(state));
	const expectedLoad = (state.loadGeneration ?? 0) + 1;
	const rust = launch(state, "rust-fork", [
		RUST_BINARY,
		...commonArguments(),
		"--session-dir",
		state.sessionDir,
		"--fork",
		original.path,
	]);
	await waitForReady("Rust fork interactive startup", rust);
	state.loadGeneration = await waitForLoadGeneration(state, expectedLoad, rust);
	const forkPath = await waitUntil(
		"forked Rust session file",
		COMMAND_DEADLINE_MS,
		() => sessionFiles(state).find((path) => !priorFiles.has(path)),
		rust,
	);
	await cleanQuit("Rust fork session", rust);
	const fork = readSession(forkPath);
	const header = fork.lines[0];
	assert(header !== undefined && entryType(header) === "session", "forked session has no v3 session header");
	assert(header.parentSession === original.path, "forked session header does not reference the source session");
	assert(compactionEntry(fork) !== undefined, "forked session did not preserve the compaction marker");
	assert(
		fork.lines.some((entry) => typeof entry.id === "string" && typeof entry.parentId === "string"),
		"forked session did not preserve transcript tree ids",
	);
	state.forkSessionPath = forkPath;
	finishStep(step, {
		session: relative(state.runRoot, fork.path),
		parentSession: original.path,
		entries: fork.lines.length,
		sha256: sha256(fork.bytes),
	});
	return fork;
}

async function runResumeAndReload(state: WorkflowState, fork: SessionDocument): Promise<SessionDocument> {
	const step = beginStep(state, "rust-resume-reload");
	const beforeResume = fork.bytes;
	const expectedStartupLoad = (state.loadGeneration ?? 0) + 1;
	const rust = launch(state, "rust-resume", [
		RUST_BINARY,
		...commonArguments(),
		"--session-dir",
		state.sessionDir,
		"--session",
		fork.path,
	]);
	await waitForReady("Rust resumed interactive startup", rust);
	state.loadGeneration = await waitForLoadGeneration(state, expectedStartupLoad, rust);
	assert(
		Buffer.compare(Buffer.from(beforeResume), readFileSync(fork.path)) === 0,
		"resuming the Rust session rewrote historical JSONL before any action",
	);

	const beforeReload = state.loadGeneration;
	// Ctrl+R is the public interactive reload action and cannot be mistaken for a
	// provider prompt. The load-generation file is written by the shared source
	// extension factory each time a fresh host registration pass occurs.
	rust.writeKeys("\x12");
	state.loadGeneration = await waitForLoadGeneration(state, beforeReload + 1, rust);
	await cleanQuit("Rust resumed session", rust);
	const resumed = readSession(fork.path);
	assert(
		resumed.lines.some((entry) => entryType(entry) === "compaction"),
		"resumed session lost its compaction entry",
	);
	assertSteeringTurn(resumed);
	finishStep(step, {
		session: relative(state.runRoot, resumed.path),
		loadGenerationBeforeReload: beforeReload,
		loadGenerationAfterReload: state.loadGeneration,
		sha256: sha256(resumed.bytes),
	});
	return resumed;
}

async function reopenWithTypescript(
	state: WorkflowState,
	bun: string,
	rustSession: SessionDocument,
): Promise<void> {
	const step = beginStep(state, "typescript-real-session-reopen");
	const before = readFileSync(rustSession.path);
	copyFileSync(rustSession.path, join(state.runRoot, "session-before-typescript.jsonl"));
	const expectedLoad = (state.loadGeneration ?? 0) + 1;
	const typescript = launch(state, "typescript-reopen", [
		bun,
		TYPESCRIPT_CLI,
		...commonArguments(),
		"--session-dir",
		state.sessionDir,
		"--session",
		rustSession.path,
	]);
	await waitForReady("TypeScript pi real --session reopen", typescript);
	state.loadGeneration = await waitForLoadGeneration(state, expectedLoad, typescript);
	await cleanQuit("TypeScript reopened session", typescript);
	const after = readFileSync(rustSession.path);
	copyFileSync(rustSession.path, join(state.runRoot, "session-after-typescript.jsonl"));
	assert(
		after.byteLength >= before.byteLength &&
			Buffer.compare(before, after.subarray(0, before.byteLength)) === 0,
		"TypeScript pi rewrote bytes in the Rust session's historical JSONL prefix",
	);
	const reopened = readSession(rustSession.path);
	const header = reopened.lines[0];
	assert(header !== undefined && header.version === 3, "TypeScript reopen did not preserve the Rust v3 header");
	assert(compactionEntry(reopened) !== undefined, "TypeScript reopen lost the compaction transcript marker");
	assertSteeringTurn(reopened);
	assert(
		reopened.lines.some((entry) => typeof entry.id === "string" && typeof entry.parentId === "string"),
		"TypeScript reopen lost session tree markers",
	);
	finishStep(step, {
		sessionFlag: rustSession.path,
		bytes: after.byteLength,
		sha256Before: sha256(before),
		sha256After: sha256(after),
		loadGeneration: state.loadGeneration,
	});
}

async function terminateAll(processes: readonly NamedProcess[]): Promise<void> {
	for (const named of processes) {
		try {
			await named.process.terminate(2_000);
		} catch (error) {
			console.warn(`failed to terminate ${named.name} process group ${named.process.pid}: ${errorText(error)}`);
		}
	}
}

function capturePty(state: WorkflowState, named: NamedProcess): void {
	const snapshot = named.process.snapshot();
	const ptyChunks = snapshot.chunks.filter((chunk) => chunk.stream === "pty");
	const driverChunks = snapshot.chunks.filter((chunk) => chunk.stream === "driver");
	writeFileSync(
		join(state.runRoot, `${named.name}.raw`),
		Buffer.concat(ptyChunks.map((chunk) => Buffer.from(chunk.bytes))),
	);
	writeFileSync(
		join(state.runRoot, `${named.name}.driver.log`),
		driverChunks.map((chunk) => chunk.text).join(""),
		"utf8",
	);
	writeFileSync(
		join(state.runRoot, `${named.name}.pty.json`),
		`${JSON.stringify(
			{
				pid: named.process.pid,
				exitCode: snapshot.exitCode,
				elapsedMs: snapshot.elapsedMs,
				applicationText: snapshot.applicationText,
				echoText: snapshot.echoText,
				chunks: snapshot.chunks.map((chunk) => ({
					stream: chunk.stream,
					elapsedMs: chunk.elapsedMs,
					unixMs: chunk.unixMs,
					byteLength: chunk.bytes.byteLength,
				})),
			},
			null,
			2,
		)}\n`,
		"utf8",
	);
}

function captureSessions(state: WorkflowState): JsonValue {
	const sessions = sessionFiles(state).map((path) => {
		const session = readSession(path);
		return {
			path: relative(state.runRoot, path),
			bytes: session.bytes.byteLength,
			sha256: sha256(session.bytes),
			entries: session.lines.length,
			types: session.lines.map(entryType),
		};
	});
	return sessions;
}

async function main(): Promise<void> {
	const bun = ensurePrerequisites();
	const state = createState();
	let failure: Error | undefined;
	try {
		const original = await runInitialSession(state);
		const fork = await runFork(state, original);
		const resumed = await runResumeAndReload(state, fork);
		await reopenWithTypescript(state, bun, resumed);
	} catch (error) {
		failure = error instanceof Error ? error : new Error(String(error));
	} finally {
		await terminateAll(state.processes);
		for (const named of state.processes) capturePty(state, named);
		const result = {
			check: 11,
			status: failure === undefined ? "pass" : "fail",
			startedAt: state.steps[0]?.startedAt ?? isoNow(),
			finishedAt: isoNow(),
			runRoot: state.runRoot,
			machine: {
				platform: process.platform,
				arch: process.arch,
				bunVersion: Bun.version,
			},
			paths: {
				rustBinary: RUST_BINARY,
				extensionHost: EXTENSION_HOST,
				extension: EXTENSION_PATH,
				typescriptCli: TYPESCRIPT_CLI,
				originalSession: state.originalSessionPath ?? null,
				forkSession: state.forkSessionPath ?? null,
			},
			loadGeneration: state.loadGeneration ?? loadGeneration(state.loadCountPath),
			steps: state.steps,
			sessions: captureSessions(state),
			failure: failure?.message ?? null,
		};
		writeFileSync(join(state.runRoot, "result.json"), `${JSON.stringify(result, null, 2)}\n`, "utf8");
		writeFileSync(join(EVIDENCE_ROOT, "latest-run.txt"), `${basename(state.runRoot)}\n`, "utf8");
	}
	if (failure !== undefined) throw failure;
	process.stdout.write(`check 11 passed; evidence: ${state.runRoot}\n`);
}

await main().catch((error: unknown) => {
	console.error(errorText(error));
	process.exitCode = 1;
});
