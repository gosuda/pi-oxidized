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
	VERIFICATION_CUSTOM_UI_COMMAND,
	VERIFICATION_DIALOG_COMMAND,
	VERIFICATION_FLAG_COMMAND,
	VERIFICATION_SESSION_REPLACEMENT_COMMAND,
	VERIFICATION_MODEL,
	VERIFICATION_PROFILE_FLAG,
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
const RUST_VERIFICATION_PROFILE = "rust-compatibility-profile";
const TYPESCRIPT_VERIFICATION_PROFILE = "typescript-compatibility-profile";
const KITTY_CTRL_SHIFT_X = "\x1b[120;6u";
const DIALOG_INPUT_VALUE = "dialog-input-value";
const DIALOG_EDITOR_VALUE = "dialog-editor-value";

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

interface CompatibilityMarker {
	readonly stage: string;
	readonly instance: string;
	readonly sequence: number;
	readonly value: JsonValue;
}

interface WorkflowState {
	readonly runRoot: string;
	readonly homeDir: string;
	readonly agentDir: string;
	readonly sessionDir: string;
	readonly workDir: string;
	readonly loadCountPath: string;
	readonly compatibilityPath: string;
	readonly toolPath: string;
	readonly steps: StepEvidence[];
	readonly processes: NamedProcess[];
	originalSessionPath?: string;
	forkSessionPath?: string;
	loadGeneration?: number;
	rustInitialCompatibilityInstance?: string;
	rustReloadCompatibilityInstance?: string;
	typescriptCompatibilityInstance?: string;
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

function ensureRustPrerequisites(): void {
	for (const required of [RUST_BINARY, EXTENSION_HOST, EXTENSION_PATH]) {
		if (!existsSync(required)) {
			fail(
				`product prerequisite missing: ${required}${
					required === RUST_BINARY ? "; run cargo build -p pi --release --locked" : ""
				}`,
			);
		}
	}
}

function ensurePrerequisites(): string {
	ensureRustPrerequisites();
	for (const required of [TYPESCRIPT_CLI]) {
		if (!existsSync(required)) {
			fail(`product prerequisite missing: ${required}`);
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
		compatibilityPath: join(runRoot, "compatibility.jsonl"),
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
		PI_VERIFICATION_COMPATIBILITY_PATH: state.compatibilityPath,
		PI_VERIFICATION_TOOL_PATH: TOOL_FILE,
	};
}

function commonArguments(profile = RUST_VERIFICATION_PROFILE): string[] {
	return [
		"--provider",
		VERIFICATION_PROVIDER,
		"--model",
		VERIFICATION_MODEL,
		"--api-key",
		"verification-key",
		"--extension",
		EXTENSION_PATH,
		`--${VERIFICATION_PROFILE_FLAG}`,
		profile,
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

function readCompatibilityMarkers(state: WorkflowState): CompatibilityMarker[] {
	if (!existsSync(state.compatibilityPath)) return [];
	const text = readFileSync(state.compatibilityPath, "utf8");
	const markers: CompatibilityMarker[] = [];
	const nextSequenceByInstance = new Map<string, number>();
	for (const [index, rawLine] of text.split(/\r?\n/).entries()) {
		const line = rawLine.trim();
		if (line.length === 0) continue;
		let parsed: JsonValue;
		try {
			parsed = JSON.parse(line) as JsonValue;
		} catch (error) {
			fail(`invalid compatibility JSONL ${state.compatibilityPath}:${index + 1}: ${errorText(error)}`);
		}
		const object = asObject(parsed, `${state.compatibilityPath}:${index + 1}`);
		const keys = Object.keys(object).sort();
		assert(
			keys.join(",") === "instance,sequence,stage,value",
			`compatibility marker ${index + 1} has invalid keys: ${keys.join(",")}`,
		);
		const stage = asString(object.stage, `compatibility marker ${index + 1} stage`);
		const instance = asString(object.instance, `compatibility marker ${index + 1} instance`);
		assert(stage.length > 0, `compatibility marker ${index + 1} stage must not be empty`);
		assert(instance.length > 0, `compatibility marker ${index + 1} instance must not be empty`);
		const sequence = object.sequence;
		assert(
			typeof sequence === "number" && Number.isSafeInteger(sequence) && sequence > 0,
			`compatibility marker ${index + 1} sequence must be a positive safe integer`,
		);
		const expectedSequence = nextSequenceByInstance.get(instance) ?? 1;
		assert(
			sequence === expectedSequence,
			`compatibility marker ${index + 1} sequence for ${instance} must be ${expectedSequence}, got ${sequence}`,
		);
		nextSequenceByInstance.set(instance, sequence + 1);
		assert(Object.hasOwn(object, "value"), `compatibility marker ${index + 1} is missing value`);
		markers.push({ stage, instance, sequence, value: object.value ?? null });
	}
	return markers;
}

function markerValue(marker: CompatibilityMarker, label = marker.stage): JsonObject {
	return asObject(marker.value, `${label} value`);
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

async function waitForCompatibilityMarker(
	state: WorkflowState,
	label: string,
	process: PtyProcess,
	startIndex: number,
	stage: string,
	predicate: (marker: CompatibilityMarker) => boolean = () => true,
): Promise<{ marker: CompatibilityMarker; index: number }> {
	return waitUntil(
		label,
		COMMAND_DEADLINE_MS,
		() => {
			const markers = readCompatibilityMarkers(state);
			const relativeIndex = markers.slice(startIndex).findIndex((marker) => marker.stage === stage && predicate(marker));
			if (relativeIndex < 0) return undefined;
			const index = startIndex + relativeIndex;
			const marker = markers[index];
			return marker === undefined ? undefined : { marker, index };
		},
		process,
	);
}

async function waitForScreen(
	process: PtyProcess,
	label: string,
	text: string,
	startOffset = 0,
): Promise<void> {
	try {
		await process.waitFor((snapshot) => snapshot.rawText.slice(startOffset).includes(text), {
			deadlineMs: COMMAND_DEADLINE_MS,
			source: "raw",
		});
	} catch (error) {
		throw processBlocker(label, process, error);
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

async function assertInitialFlagCompatibility(
	state: WorkflowState,
	rust: PtyProcess,
	startIndex: number,
): Promise<void> {
	const step = beginStep(state, "rust-extension-flag-session-start");
	const { marker, index } = await waitForCompatibilityMarker(
		state,
		"Rust extension session_start flag marker",
		rust,
		startIndex,
		"session_start.after",
		(candidate) => markerValue(candidate).value === RUST_VERIFICATION_PROFILE,
	);
	state.rustInitialCompatibilityInstance = marker.instance;
	finishStep(step, {
		profile: RUST_VERIFICATION_PROFILE,
		instance: marker.instance,
		markerIndex: index,
	});
}

async function assertShortcutCompatibility(state: WorkflowState, rust: PtyProcess): Promise<void> {
	const step = beginStep(state, "rust-extension-shortcut-dispatch");
	const startIndex = readCompatibilityMarkers(state).length;
	rust.writeKeys(KITTY_CTRL_SHIFT_X);
	const { marker, index } = await waitForCompatibilityMarker(
		state,
		"canonical Kitty ctrl+shift+x shortcut dispatch",
		rust,
		startIndex,
		"shortcut.after",
		(candidate) => {
			const value = markerValue(candidate);
			return value.shortcut === "ctrl+shift+x" && value.dispatched === true;
		},
	);
	assert(
		marker.instance === state.rustInitialCompatibilityInstance,
		"shortcut dispatched in a different extension instance than session_start",
	);
	finishStep(step, { sequence: "CSI 120;6u", instance: marker.instance, markerIndex: index });
}

async function assertDialogCompatibility(state: WorkflowState, rust: PtyProcess): Promise<void> {
	const step = beginStep(state, "rust-extension-dialogs");
	const startIndex = readCompatibilityMarkers(state).length;
	sendLine(rust, `/${VERIFICATION_DIALOG_COMMAND}`);

	await waitForCompatibilityMarker(state, "select before marker", rust, startIndex, "dialogs.select.before");
	await waitForScreen(rust, "real select prompt", "Verification select prompt");
	rust.writeKeys(PTY_KEYS.down, PTY_KEYS.enter);
	await waitForCompatibilityMarker(
		state, "select result marker", rust, startIndex, "dialogs.select.after",
		(marker) => markerValue(marker).value === "beta",
	);

	await waitForCompatibilityMarker(state, "confirm before marker", rust, startIndex, "dialogs.confirm.before");
	await waitForScreen(rust, "real confirm prompt", "confirm prompt");
	rust.writeKeys(PTY_KEYS.enter);
	await waitForCompatibilityMarker(
		state, "confirm result marker", rust, startIndex, "dialogs.confirm.after",
		(marker) => markerValue(marker).value === true,
	);

	await waitForCompatibilityMarker(state, "input before marker", rust, startIndex, "dialogs.input.before");
	await waitForScreen(rust, "real input prompt", "input prompt");
	rust.writeKeys(DIALOG_INPUT_VALUE, PTY_KEYS.enter);
	await waitForCompatibilityMarker(
		state, "input result marker", rust, startIndex, "dialogs.input.after",
		(marker) => markerValue(marker).value === DIALOG_INPUT_VALUE,
	);

	await waitForCompatibilityMarker(state, "editor before marker", rust, startIndex, "dialogs.editor.before");
	await waitForScreen(rust, "real editor prompt", "editor prompt");
	rust.writeKeys(DIALOG_EDITOR_VALUE, PTY_KEYS.enter);
	const { marker: results, index } = await waitForCompatibilityMarker(
		state,
		"exact correlated dialog results",
		rust,
		startIndex,
		"dialogs.results",
		(marker) => {
			const value = markerValue(marker);
			return value.operationId === "verification-dialogs-v1"
				&& value.select === "beta"
				&& value.confirm === true
				&& value.input === DIALOG_INPUT_VALUE
				&& value.editor === DIALOG_EDITOR_VALUE;
		},
	);
	await waitForCompatibilityMarker(state, "dialog command after marker", rust, startIndex, "dialogs.command.after");
	finishStep(step, { instance: results.instance, markerIndex: index, operationId: "verification-dialogs-v1" });
}

async function assertCustomUiCompatibility(state: WorkflowState, rust: PtyProcess): Promise<void> {
	const step = beginStep(state, "rust-extension-custom-ui");
	const startIndex = readCompatibilityMarkers(state).length;
	sendLine(rust, `/${VERIFICATION_CUSTOM_UI_COMMAND}`);
	await waitForCompatibilityMarker(state, "custom initial render marker", rust, startIndex, "custom.render.initial");
	await waitForScreen(rust, "custom UI initial render", "state=initial");
	// Ratatui updates only the changed `initial` cells, so the raw PTY receives
	// `updated` rather than a repeated full line. Markers are file-only; this
	// post-input offset proves that the native terminal emitted the rerender.
	const updateOutputOffset = rust.snapshot().rawText.length;
	rust.writeKeys("x");
	await waitForCompatibilityMarker(
		state, "custom input mutation marker", rust, startIndex, "custom.input.after",
		(marker) => {
			const value = markerValue(marker);
			return value.input === "x" && value.state === "updated";
		},
	);
	await waitForCompatibilityMarker(state, "custom updated render marker", rust, startIndex, "custom.render.updated");
	await waitForScreen(rust, "custom UI updated rerender", "updated", updateOutputOffset);
	const { marker: completed, index } = await waitForCompatibilityMarker(
		state, "custom command completion after rerender", rust, startIndex, "custom.command.after",
		(marker) => markerValue(marker).state === "updated",
	);
	const disposed = await waitForCompatibilityMarker(
		state, "custom component disposal after rerender", rust, startIndex, "custom.dispose",
		(marker) => markerValue(marker).state === "updated",
	);
	assert(disposed.index < index, "custom command completed before the updated component was disposed");
	finishStep(step, { instance: completed.instance, markerIndex: index, state: "updated" });
}

async function runExtensionCompatibility(
	state: WorkflowState,
	rust: PtyProcess,
	startIndex: number,
): Promise<void> {
	await assertInitialFlagCompatibility(state, rust, startIndex);
	await assertShortcutCompatibility(state, rust);
	await assertDialogCompatibility(state, rust);
	await assertCustomUiCompatibility(state, rust);
}

async function runInitialSession(state: WorkflowState): Promise<SessionDocument> {
	const step = beginStep(state, "rust-interactive-tools-steering-compaction");
	const compatibilityStartIndex = readCompatibilityMarkers(state).length;
	const rust = launch(state, "rust-initial", [
		RUST_BINARY,
		...commonArguments(),
		"--session-dir",
		state.sessionDir,
	]);
	await waitForReady("Rust initial interactive startup", rust);
	state.loadGeneration = await waitForLoadGeneration(state, 1, rust);
	await runExtensionCompatibility(state, rust, compatibilityStartIndex);
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
	const compatibilityStartupIndex = readCompatibilityMarkers(state).length;
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
	const resumedStartup = await waitForCompatibilityMarker(
		state,
		"resumed Rust session_start flag marker",
		rust,
		compatibilityStartupIndex,
		"session_start.after",
		(marker) => markerValue(marker).value === RUST_VERIFICATION_PROFILE,
	);
	assert(
		Buffer.compare(Buffer.from(beforeResume), readFileSync(fork.path)) === 0,
		"resuming the Rust session rewrote historical JSONL before any action",
	);

	const reloadStep = beginStep(state, "rust-extension-reload-flag-preservation");
	const beforeReload = state.loadGeneration;
	const compatibilityReloadIndex = readCompatibilityMarkers(state).length;
	// Exercise the public slash-command path, then require the replacement host's
	// session_start hook to observe the original dynamic flag before any command.
	sendLine(rust, "/reload");
	state.loadGeneration = await waitForLoadGeneration(state, beforeReload + 1, rust);
	const replacementStart = await waitForCompatibilityMarker(
		state,
		"replacement host session_start preserved flag",
		rust,
		compatibilityReloadIndex,
		"session_start.after",
		(marker) => markerValue(marker).value === RUST_VERIFICATION_PROFILE,
	);
	assert(
		replacementStart.marker.instance !== resumedStartup.marker.instance,
		"/reload reused the prior extension instance instead of starting a replacement host registration",
	);
	state.rustReloadCompatibilityInstance = replacementStart.marker.instance;
	const observationStartIndex = readCompatibilityMarkers(state).length;
	sendLine(rust, `/${VERIFICATION_FLAG_COMMAND}`);
	const observationBefore = await waitForCompatibilityMarker(
		state, "replacement flag observation before marker", rust, observationStartIndex, "flag_observation.before",
	);
	const observationAfter = await waitForCompatibilityMarker(
		state,
		"replacement flag observation after marker",
		rust,
		observationStartIndex,
		"flag_observation.after",
		(marker) => markerValue(marker).value === RUST_VERIFICATION_PROFILE,
	);
	assert(
		observationBefore.marker.instance === replacementStart.marker.instance
			&& observationAfter.marker.instance === replacementStart.marker.instance,
		"flag observation command did not execute in the replacement extension instance",
	);
	assert(
		replacementStart.index < observationBefore.index && observationBefore.index < observationAfter.index,
		"replacement session_start did not observe the flag before command dispatch",
	);
	finishStep(reloadStep, {
		profile: RUST_VERIFICATION_PROFILE,
		previousInstance: resumedStartup.marker.instance,
		replacementInstance: replacementStart.marker.instance,
		sessionStartMarkerIndex: replacementStart.index,
		observationMarkerIndex: observationAfter.index,
	});
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

async function runSessionReplacement(state: WorkflowState): Promise<SessionDocument> {
	const step = beginStep(state, "rust-session-replacement");
	const expectedLoad = (state.loadGeneration ?? 0) + 1;
	const startIndex = readCompatibilityMarkers(state).length;
	const rust = launch(state, "rust-replacement", [
		RUST_BINARY,
		...commonArguments(),
		"--session-dir",
		state.sessionDir,
	]);
	try {
		await waitForReady("Rust replacement interactive startup", rust);
		state.loadGeneration = await waitForLoadGeneration(state, expectedLoad, rust);
		const priorFiles = new Set(sessionFiles(state));
		sendLine(rust, `/${VERIFICATION_SESSION_REPLACEMENT_COMMAND}`);

		const before = await waitForCompatibilityMarker(
			state, "replacement before marker", rust, startIndex, "replacement.before",
		);
		const instance = before.marker.instance;
		const setup = await waitForCompatibilityMarker(
			state, "replacement setup marker", rust, before.index + 1, "replacement.setup",
			(marker) => marker.instance === instance,
		);
		const withSessionBefore = await waitForCompatibilityMarker(
			state, "replacement withSession before marker", rust, setup.index + 1, "replacement.withSession.before",
			(marker) => marker.instance === instance,
		);
		const withSessionAfter = await waitForCompatibilityMarker(
			state, "replacement withSession after marker", rust, withSessionBefore.index + 1, "replacement.withSession.after",
			(marker) => marker.instance === instance,
		);
		const after = await waitForCompatibilityMarker(
			state, "replacement after marker", rust, withSessionAfter.index + 1, "replacement.after",
			(marker) => marker.instance === instance,
		);
		assert(
			before.index < setup.index && setup.index < withSessionBefore.index
				&& withSessionBefore.index < withSessionAfter.index && withSessionAfter.index < after.index,
			"replacement compatibility markers were not recorded in the required stage order",
		);
		const cancelled = markerValue(after.marker).cancelled;
		assert(cancelled === false, "session replacement was cancelled instead of completing");
		const rebound = await waitForCompatibilityMarker(
			state,
			"replacement session rebind marker",
			rust,
			withSessionAfter.index + 1,
			"session_start.after",
			(marker) => marker.instance !== instance,
		);

		// The replacement session is not persisted until its first assistant
		// response. Drive that turn through the live TUI after the rebind.
		sendLine(rust, "verification replacement turn");
		await waitForScreen(rust, "replacement assistant response", FINAL_MARKER);
		const sessionPath = await waitUntil(
			"replacement Rust session file",
			COMMAND_DEADLINE_MS,
			() => sessionFiles(state).find((path) => !priorFiles.has(path)),
			rust,
		);
		const session = readSession(sessionPath);
		const setupEntry = session.lines.find(
			(entry) => entryType(entry) === "custom" && entry.customType === "verification-replacement-setup",
		);
		assert(setupEntry !== undefined, "replacement session JSONL did not persist the setup custom entry");
		assert(
			typeof setupEntry.data === "object" && setupEntry.data !== null
				&& asObject(setupEntry.data, "setup custom entry data").source === "setup",
			"replacement setup custom entry did not persist data.source === \"setup\"",
		);
		const withSessionEntry = session.lines.find(
			(entry) =>
				entryType(entry) === "custom_message"
				&& entry.customType === "verification-replacement-with-session",
		);
		assert(withSessionEntry !== undefined, "replacement session JSONL did not persist the withSession custom_message");
		assert(
			withSessionEntry.content === "verification replacement withSession",
			"replacement withSession custom_message did not persist the exact content string",
		);
		assert(withSessionEntry.display === false, "replacement withSession custom_message did not persist display === false");
		const userEntry = session.lines.find(
			(entry) => messageRole(entry) === "user" && messageText(entry) === "verification replacement turn",
		);
		assert(userEntry !== undefined, "replacement session JSONL did not persist the user turn");
		const assistantEntry = session.lines.find(
			(entry) => messageRole(entry) === "assistant" && messageText(entry).includes(FINAL_MARKER),
		);
		assert(assistantEntry !== undefined, "replacement session JSONL did not persist the assistant response");
		sendLine(rust, `/${VERIFICATION_FLAG_COMMAND} post-replacement`);
		const post = await waitForCompatibilityMarker(
			state, "post-replacement command marker", rust, after.index + 1, "replacement.post",
			(marker) => marker.instance === rebound.marker.instance,
		);
		const postSession = await waitUntil(
			"post-replacement session command persistence",
			COMMAND_DEADLINE_MS,
			() => {
				const current = readSession(sessionPath);
				return current.lines.some(
					(entry) =>
						entryType(entry) === "custom_message"
						&& entry.customType === "verification-post-replacement"
						&& entry.content === "verification post replacement"
						&& entry.display === false,
				)
					? current
					: undefined;
			},
			rust,
		);
		await cleanQuit("Rust replacement session", rust);
		finishStep(step, {
			session: relative(state.runRoot, postSession.path),
			instance,
			replacementInstance: rebound.marker.instance,
			markerIndex: post.index,
			entries: postSession.lines.length,
			sha256: sha256(postSession.bytes),
		});
		return postSession;
	} catch (error) {
		const cause = error instanceof Error ? error : new Error(String(error));
		const snapshot = rust.snapshot();
		const ptyTail = snapshot.rawText.slice(-8_000);
		const markerCount = readCompatibilityMarkers(state).length;
		const sessions = sessionFiles(state);
		throw new Error(
			`${cause.message}\n\n[replacement-smoke context]\n`
			+ `compatibilityPath: ${state.compatibilityPath} (markers: ${markerCount})\n`
			+ `sessionDir: ${state.sessionDir} (files: ${sessions.length})\n`
			+ `sessionPaths: ${sessions.join(", ") || "(none)"}\n`
			+ `rustExitCode: ${String(snapshot.exitCode)}\n`
			+ `PTY tail:\n${ptyTail}`,
			{ cause },
		);
	}
}

async function reopenWithTypescript(
	state: WorkflowState,
	bun: string,
	rustSession: SessionDocument,
): Promise<void> {
	const step = beginStep(state, "typescript-real-session-reopen");
	const flagStep = beginStep(state, "typescript-extension-flag-session-start");
	const compatibilityStartIndex = readCompatibilityMarkers(state).length;
	const before = readFileSync(rustSession.path);
	copyFileSync(rustSession.path, join(state.runRoot, "session-before-typescript.jsonl"));
	const expectedLoad = (state.loadGeneration ?? 0) + 1;
	const typescript = launch(state, "typescript-reopen", [
		bun,
		TYPESCRIPT_CLI,
		...commonArguments(TYPESCRIPT_VERIFICATION_PROFILE),
		"--session-dir",
		state.sessionDir,
		"--session",
		rustSession.path,
	]);
	await waitForReady("TypeScript pi real --session reopen", typescript);
	state.loadGeneration = await waitForLoadGeneration(state, expectedLoad, typescript);
	const typescriptStart = await waitForCompatibilityMarker(
		state,
		"TypeScript extension session_start distinct flag marker",
		typescript,
		compatibilityStartIndex,
		"session_start.after",
		(marker) => markerValue(marker).value === TYPESCRIPT_VERIFICATION_PROFILE,
	);
	state.typescriptCompatibilityInstance = typescriptStart.marker.instance;
	finishStep(flagStep, {
		profile: TYPESCRIPT_VERIFICATION_PROFILE,
		instance: typescriptStart.marker.instance,
		markerIndex: typescriptStart.index,
	});
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

async function runReplacementOnly(): Promise<void> {
	ensureRustPrerequisites();
	const state = createState();
	let failure: Error | undefined;
	try {
		await runSessionReplacement(state);
	} catch (error) {
		failure = error instanceof Error ? error : new Error(String(error));
	} finally {
		await terminateAll(state.processes);
		for (const named of state.processes) capturePty(state, named);
		const result = {
			check: "replacement",
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
			},
			loadGeneration: state.loadGeneration ?? loadGeneration(state.loadCountPath),
			compatibility: {
				path: relative(state.runRoot, state.compatibilityPath),
				markerCount: readCompatibilityMarkers(state).length,
				rustProfile: RUST_VERIFICATION_PROFILE,
			},
			steps: state.steps,
			sessions: captureSessions(state),
			failure: failure?.message ?? null,
		};
		writeFileSync(join(state.runRoot, "result.json"), `${JSON.stringify(result, null, 2)}\n`, "utf8");
		writeFileSync(join(EVIDENCE_ROOT, "latest-run.txt"), `${basename(state.runRoot)}\n`, "utf8");
	}
	if (failure !== undefined) throw failure;
	process.stdout.write(`replacement smoke passed; evidence: ${state.runRoot}\n`);
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
			compatibility: {
				path: relative(state.runRoot, state.compatibilityPath),
				markerCount: readCompatibilityMarkers(state).length,
				rustProfile: RUST_VERIFICATION_PROFILE,
				typescriptProfile: TYPESCRIPT_VERIFICATION_PROFILE,
				rustInitialInstance: state.rustInitialCompatibilityInstance ?? null,
				rustReloadInstance: state.rustReloadCompatibilityInstance ?? null,
				typescriptInstance: state.typescriptCompatibilityInstance ?? null,
			},
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

const entrypoint = process.argv.includes("--replacement-only") ? runReplacementOnly : main;
await entrypoint().catch((error: unknown) => {
	console.error(errorText(error));
	process.exitCode = 1;
});
