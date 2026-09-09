#!/usr/bin/env bun
/**
 * Shared staged-input capture primitive for the verification capture commands.
 *
 * In explicit staged mode (`--staged-inputs`) the capture commands publish the
 * working tree only after proving that every declared relevant input is an
 * ordinary indexed file whose raw working bytes equal its indexed blob. The
 * real current commit stays `captureHead`; the relevant indexed state is
 * recorded separately as `stagedInputProvenance` so a snapshot can describe a
 * same-green-commit refresh without a temporary commit, a fake commit ID or a
 * broad dirty bypass.
 *
 * Guarantees, all enforced against real Git state:
 *
 * - the repository is a real worktree with an existing commit, reached without
 *   GIT_DIR/GIT_WORK_TREE/GIT_INDEX_FILE/GIT_COMMON_DIR/GIT_OBJECT_DIRECTORY/
 *   GIT_ALTERNATE_OBJECT_DIRECTORIES redirection;
 * - every relevant index entry is stage 0, mode 100644/100755, with a valid
 *   lowercase sha1/sha256 blob ID (no unmerged, symlink, gitlink or sparse
 *   entries; no skip-worktree or assume-unchanged flags; no intent-to-add);
 * - no relevant scope member is untracked or ignored (checked with
 *   `ls-files --others` WITHOUT exclude-standard, so ignored-but-untracked
 *   relevant files are refused too);
 * - staged worktree state is clean for every relevant path (porcelain v2
 *   Y='.'; staged X changes and X=D deletions are legal), and each working
 *   file hashes to exactly its indexed blob (`blob <len>\0` + raw bytes) with
 *   a POSIX executable-bit class matching the index mode;
 * - HEAD, the relevant index, the status set and the untracked set are
 *   re-read after the inventory and must agree, so a lasting index/HEAD
 *   transition cannot pass as one snapshot;
 * - `assertUnchanged()` repeats the whole inventory and compares HEAD,
 *   format, entry membership/path/mode/blob and private working-file
 *   observations (bytes + identity), so worktree edits, restaging, relevant
 *   additions/deletions/renames, HEAD movement and edit-restore cycles all
 *   fail while unrelated changes are intentionally ignored.
 *
 * A clean index is legal under explicit staged mode; no staged diff is
 * manufactured. A staged deletion is represented by absence from the
 * inventory plus the stored scope and real baseHead, never a zero blob.
 *
 * `beginRelevantInputObservation` is the weaker cousin used only by the
 * dep-field-free dirty exception: it records HEAD/index/status and actual
 * working bytes/identity/absence (including initially relevant untracked
 * regular files) without requiring worktree==index and returns no provenance.
 *
 * Git is invoked with non-shell spawnSync, cwd=root, a 30 second timeout, a
 * 64 MiB output buffer and GIT_OPTIONAL_LOCKS=0. Missing output, timeouts,
 * termination, overflow, malformed output and nonzero status fail closed;
 * empty output means an empty clean result. No Git state is ever written.
 */

import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { lstatSync, readFileSync, realpathSync } from "node:fs";
import type { BigIntStats } from "node:fs";
import { basename, dirname, isAbsolute, join, relative, resolve, sep } from "node:path";

/** Schema discriminator of the stored `stagedInputProvenance` object. */
export const STAGED_INPUT_PROVENANCE_SCHEMA = "pi.deps.staged-inputs.v1" as const;

/** One relevant indexed file as provenance records it. */
export interface IndexedCaptureInput {
	readonly path: string;
	readonly mode: "100644" | "100755";
	readonly blob: string;
}

/** Stored provenance of an explicit staged capture. Deep-frozen when produced. */
export interface StagedInputProvenance {
	readonly schema: typeof STAGED_INPUT_PROVENANCE_SCHEMA;
	readonly mode: "staged";
	readonly baseHead: string;
	readonly objectFormat: "sha1" | "sha256";
	readonly pathspecs: readonly string[];
	readonly entries: readonly IndexedCaptureInput[];
}

/** Live capture handle; `assertUnchanged` re-validates against the frozen baseline. */
export interface StagedInputCapture {
	readonly provenance: StagedInputProvenance;
	assertUnchanged(): void;
}

const GIT_TIMEOUT_MS = 30_000;
const GIT_MAX_BUFFER_BYTES = 64 * 1024 * 1024;
const STDERR_BOUND_CHARS = 400;
const ZERO_OID = /^0+$/;
const REDIRECTED_REPOSITORY_VARS = [
	"GIT_DIR",
	"GIT_WORK_TREE",
	"GIT_INDEX_FILE",
	"GIT_COMMON_DIR",
	"GIT_OBJECT_DIRECTORY",
	"GIT_ALTERNATE_OBJECT_DIRECTORIES",
] as const;
const PROVENANCE_KEYS = ["baseHead", "entries", "mode", "objectFormat", "pathspecs", "schema"] as const;
const ENTRY_KEYS = ["blob", "mode", "path"] as const;

type ObjectFormat = "sha1" | "sha256";
type InventoryMode = "staged" | "observation";

interface FileIdentity {
	readonly dev: bigint;
	readonly ino: bigint;
	readonly mode: bigint;
	readonly size: bigint;
	readonly mtimeMs: bigint;
	readonly ctimeMs: bigint;
}

type WorkingObservation =
	| { readonly kind: "absent" }
	| { readonly kind: "file"; readonly identity: FileIdentity; readonly sha256: string; readonly size: bigint };

interface GitInventoryState {
	readonly head: string;
	readonly objectFormat: ObjectFormat;
	readonly entries: readonly IndexedCaptureInput[];
	readonly statusRecords: readonly string[];
	readonly untracked: readonly string[];
}

interface Inventory extends GitInventoryState {
	readonly observations: ReadonlyMap<string, WorkingObservation>;
}

interface ScopeMatcher {
	readonly literals: readonly string[];
	readonly globs: readonly RegExp[];
}

/** Ordinary error with a context prefix and a human-readable category token. */
function fail(context: string, category: string, message: string): never {
	throw new Error(`${context} (${category}): ${message}`);
}

function boundedStderr(stderr: Buffer | string | undefined | null): string {
	if (stderr === undefined || stderr === null) return "no stderr";
	const text = (typeof stderr === "string" ? stderr : stderr.toString("utf8")).trim();
	if (text.length === 0) return "no stderr";
	return text.length > STDERR_BOUND_CHARS ? `${text.slice(0, STDERR_BOUND_CHARS)}…(truncated)` : text;
}

/** Deterministic explicit comparison; never localeCompare. */
function compareStrings(a: string, b: string): number {
	return a < b ? -1 : a > b ? 1 : 0;
}

function sameStringList(a: readonly string[], b: readonly string[]): boolean {
	return a.length === b.length && a.every((value, index) => value === b[index]);
}

/** True only for ENOENT-style "no such entry" filesystem errors. */
function isMissingEntryError(error: unknown): boolean {
	return (
		typeof error === "object" &&
		error !== null &&
		"code" in error &&
		(error as { code: unknown }).code === "ENOENT"
	);
}

function deepFreeze<T>(value: T): T {
	if (value !== null && typeof value === "object") {
		for (const key of Object.keys(value as Record<string, unknown>)) {
			deepFreeze((value as Record<string, unknown>)[key]);
		}
		Object.freeze(value);
	}
	return value;
}

/** Non-shell git invocation: bounded, timed, lock-free, fail-closed. */
function runGit(root: string, args: readonly string[], context: string): Buffer {
	const result = spawnSync("git", [...args], {
		cwd: root,
		timeout: GIT_TIMEOUT_MS,
		maxBuffer: GIT_MAX_BUFFER_BYTES,
		killSignal: "SIGKILL",
		env: { ...process.env, GIT_OPTIONAL_LOCKS: "0" },
	});
	if (result.error !== undefined && result.error !== null) {
		fail(context, "git-failure", `git ${args[0]}: could not run git: ${result.error.message}`);
	}
	if (result.signal !== null && result.signal !== undefined) {
		fail(context, "git-failure", `git ${args[0]}: terminated by ${String(result.signal)} (timeout or external signal)`);
	}
	if (result.status !== 0) {
		fail(context, "git-failure", `git ${args.join(" ")} exited ${String(result.status)}: ${boundedStderr(result.stderr)}`);
	}
	if (result.stdout === null) {
		fail(context, "git-failure", `git ${args.join(" ")} produced no output`);
	}
	return result.stdout;
}

const fatalUtf8 = new TextDecoder("utf-8", { fatal: true });

function decodeFatal(bytes: Buffer, context: string, what: string): string {
	try {
		return fatalUtf8.decode(bytes);
	} catch {
		fail(context, "malformed-git-data", `${what}: output is not valid UTF-8`);
	}
}

/** Split NUL-terminated records; paths are never split on whitespace/newline. */
function splitNulRecords(output: Buffer, context: string, what: string): readonly string[] {
	const tokens: string[] = [];
	let start = 0;
	while (start < output.length) {
		const nul = output.indexOf(0, start);
		if (nul === -1) fail(context, "malformed-git-data", `${what}: record is not NUL-terminated`);
		tokens.push(decodeFatal(output.subarray(start, nul), context, what));
		start = nul + 1;
	}
	return tokens;
}

function assertNoRedirection(context: string): void {
	for (const name of REDIRECTED_REPOSITORY_VARS) {
		if (process.env[name] !== undefined) {
			fail(context, "unsupported-state", `${name} is set; refusing to capture through a redirected repository or index`);
		}
	}
}

function openWorktreeRoot(root: string, context: string): string {
	let resolved: string;
	try {
		resolved = realpathSync(resolve(root));
	} catch {
		fail(context, "git-failure", `repository root ${root} does not exist or is unreadable`);
	}
	const toplevel = runGit(resolved, ["rev-parse", "--show-toplevel"], context).toString("utf8").trim();
	let canonicalToplevel: string;
	try {
		canonicalToplevel = realpathSync(toplevel);
	} catch {
		fail(context, "malformed-git-data", `git rev-parse --show-toplevel returned an unusable path: ${toplevel}`);
	}
	if (canonicalToplevel !== resolved) {
		fail(context, "unsupported-state", `root ${root} is not the worktree top-level (${toplevel}); pass the repository root`);
	}
	return resolved;
}

function readHeadCommit(root: string, context: string): string {
	const raw = runGit(root, ["rev-parse", "--verify", "HEAD^{commit}"], context).toString("utf8").trim();
	if (/^[0-9a-f]{40}$/.test(raw) === false && /^[0-9a-f]{64}$/.test(raw) === false) {
		fail(context, "malformed-git-data", `git rev-parse --verify HEAD^{{commit}} returned no usable commit ID (unborn or nonrepository root): ${boundedStderr(raw)}`);
	}
	return raw;
}

function readObjectFormat(root: string, context: string): ObjectFormat {
	const raw = runGit(root, ["rev-parse", "--show-object-format"], context).toString("utf8").trim();
	if (raw !== "sha1" && raw !== "sha256") {
		fail(context, "malformed-git-data", `unsupported object format ${JSON.stringify(raw)}`);
	}
	return raw;
}

function assertOid(
	value: unknown,
	format: ObjectFormat,
	context: string,
	what: string,
	category = "malformed-git-data",
): string {
	if (typeof value !== "string") fail(context, category, `${what}: object ID must be a string`);
	const width = format === "sha1" ? 40 : 64;
	if (value.length !== width || /^[0-9a-f]+$/.test(value) === false) {
		fail(context, category, `${what}: expected a lowercase ${width}-hex ${format} object ID, got ${JSON.stringify(value)}`);
	}
	if (ZERO_OID.test(value)) fail(context, category, `${what}: zero object ID is not a real blob`);
	return value;
}

/** Root-relative POSIX path with exact spelling; no absolute/traversal/dot/backslash/NUL forms. */
function validateEntryPath(raw: string, context: string, category: string, what: string): string {
	if (raw.length === 0) fail(context, category, `${what}: empty path`);
	if (raw.includes("\0")) fail(context, category, `${what}: path contains NUL`);
	if (raw.includes("\\")) fail(context, category, `${what}: path contains a backslash`);
	if (raw.startsWith("/")) fail(context, category, `${what}: absolute path ${JSON.stringify(raw)}`);
	for (const segment of raw.split("/")) {
		if (segment.length === 0) fail(context, category, `${what}: empty path segment in ${JSON.stringify(raw)}`);
		if (segment === "." || segment === "..") fail(context, category, `${what}: dot segment in ${JSON.stringify(raw)}`);
	}
	return raw;
}

/** Scope grammar: root-relative literal files/directory prefixes and `*` globs only. */
function validateScope(raw: unknown, context: string, category: string, what: string): string {
	if (typeof raw !== "string" || raw.length === 0) {
		fail(context, category, `${what}: every scope must be a nonempty string`);
	}
	if (raw.includes("\0")) fail(context, category, `${what}: scope ${JSON.stringify(raw)} contains NUL`);
	if (raw.includes("\\")) fail(context, category, `${what}: scope ${JSON.stringify(raw)} contains a backslash`);
	if (isAbsolute(raw) || raw.startsWith("/")) fail(context, category, `${what}: scope ${JSON.stringify(raw)} is absolute`);
	if (raw.startsWith(":")) fail(context, category, `${what}: scope ${JSON.stringify(raw)} uses Git pathspec magic`);
	if (raw.startsWith("!") || raw.startsWith("^")) fail(context, category, `${what}: scope ${JSON.stringify(raw)} is an exclusion`);
	if (raw.includes("[") || raw.includes("]")) fail(context, category, `${what}: scope ${JSON.stringify(raw)} uses a character class`);
	if (raw.includes("?")) fail(context, category, `${what}: scope ${JSON.stringify(raw)} uses the ? wildcard`);
	if (raw.endsWith("/")) fail(context, category, `${what}: scope ${JSON.stringify(raw)} has a trailing slash`);
	for (const segment of raw.split("/")) {
		if (segment.length === 0) fail(context, category, `${what}: scope ${JSON.stringify(raw)} has an empty segment`);
		if (segment === "." || segment === "..") fail(context, category, `${what}: scope ${JSON.stringify(raw)} has a dot segment`);
	}
	return raw;
}

function normalizeScopes(pathspecs: readonly unknown[], context: string): readonly string[] {
	if (pathspecs.length === 0) fail(context, "unsupported-state", "pathspecs must be a nonempty list of relevant scopes");
	const validated = pathspecs.map((raw) => validateScope(raw, context, "unsupported-state", "pathspec"));
	validated.sort(compareStrings);
	const deduped: string[] = [];
	for (const scope of validated) {
		if (deduped.length === 0 || deduped[deduped.length - 1] !== scope) deduped.push(scope);
	}
	return deduped;
}

/** Escape regex metacharacters; `*` becomes Git's ordinary `.*` (crosses `/`). */
function buildScopeMatcher(scopes: readonly string[], context: string): ScopeMatcher {
	const literals: string[] = [];
	const globs: RegExp[] = [];
	for (const scope of scopes) {
		if (scope.includes("*") === false) {
			literals.push(scope);
			continue;
		}
		const pattern = scope
			.split("*")
			.map((part) => part.replace(/[.+?^${}()|[\]\\]/g, "\\$&"))
			.join("[\\s\\S]*");
		try {
			// Full-path match only, mirroring Git's default pathspec
			// wildmatch: `*` crosses `/` but there is no implicit directory
			// recursion — recursive depth must be spelled out with `**` in the
			// scope itself.
			globs.push(new RegExp(`^${pattern}$`, "u"));
		} catch {
			fail(context, "unsupported-state", `scope ${JSON.stringify(scope)} cannot become a matcher`);
		}
	}
	return { literals, globs };
}

function matchesScope(matcher: ScopeMatcher, path: string): boolean {
	for (const literal of matcher.literals) {
		if (path === literal || path.startsWith(`${literal}/`)) return true;
	}
	for (const glob of matcher.globs) {
		if (glob.test(path)) return true;
	}
	return false;
}

function readIndexEntries(
	root: string,
	scopes: readonly string[],
	objectFormat: ObjectFormat,
	context: string,
): IndexedCaptureInput[] {
	const output = runGit(root, ["ls-files", "--stage", "-z", "--", ...scopes], context);
	const tokens = splitNulRecords(output, context, "git ls-files --stage");
	const entries: IndexedCaptureInput[] = [];
	const seen = new Set<string>();
	for (const token of tokens) {
		const tab = token.indexOf("\t");
		if (tab <= 0) fail(context, "malformed-git-data", "git ls-files --stage: record without a TAB path separator");
		const meta = token.slice(0, tab);
		const path = validateEntryPath(token.slice(tab + 1), context, "malformed-git-data", "indexed path");
		const metaFields = meta.split(" ");
		if (metaFields.length !== 3) {
			fail(context, "malformed-git-data", `git ls-files --stage: malformed record for ${path}`);
		}
		const [mode, oid, stage] = metaFields as [string, string, string];
		if (stage !== "0") {
			fail(context, "unmerged-input", `relevant input ${path} sits at conflict stage ${stage}; resolve and restage`);
		}
		if (mode !== "100644" && mode !== "100755") {
			fail(context, "unsupported-state", `relevant input ${path} has unsupported index mode ${mode} (symlink, gitlink or sparse directory entry)`);
		}
		assertOid(oid, objectFormat, context, `indexed blob for ${path}`);
		if (seen.has(path)) fail(context, "malformed-git-data", `git ls-files --stage: duplicate record for ${path}`);
		seen.add(path);
		entries.push({ path, mode: mode as IndexedCaptureInput["mode"], blob: oid });
	}
	entries.sort((a, b) => compareStrings(a.path, b.path));
	return entries;
}

/** Refuse skip-worktree (S), assume-unchanged (lowercase tags) and unmerged (M) flags. */
function assertSupportedIndexFlags(root: string, scopes: readonly string[], context: string): void {
	const output = runGit(root, ["ls-files", "-v", "-z", "--", ...scopes], context);
	const tokens = splitNulRecords(output, context, "git ls-files -v");
	for (const token of tokens) {
		if (token.length < 3 || token[1] !== " ") {
			fail(context, "malformed-git-data", "git ls-files -v: malformed tagged record");
		}
		const tag = token[0] as string;
		const path = token.slice(2);
		if (tag === "S") {
			fail(context, "unsupported-state", `relevant input ${path} is marked skip-worktree; unset it before capturing`);
		}
		if (tag >= "a" && tag <= "z") {
			fail(context, "unsupported-state", `relevant input ${path} is marked assume-unchanged; unset it before capturing`);
		}
		if (tag === "M") {
			fail(context, "unmerged-input", `relevant input ${path} is unmerged in the index`);
		}
	}
}

/**
 * Porcelain v2 records under -z (git-status(1)): an ordinary `1` record
 * carries its fixed fields and the path in ONE NUL-terminated record — fields
 * are space-separated and the path is the remainder after the 8th field, so
 * spaces, TABs and newlines inside paths survive; a `2` renamed/copied record
 * carries the target path the same way plus the original path as a second NUL
 * record; a `u` unmerged record carries its path the same way after ten fixed
 * fields.
 */
function readStatusRecords(
	root: string,
	scopes: readonly string[],
	mode: InventoryMode,
	context: string,
): readonly string[] {
	const output = runGit(
		root,
		["status", "--porcelain=v2", "-z", "--untracked-files=all", "--ignore-submodules=none", "--", ...scopes],
		context,
	);
	const tokens = splitNulRecords(output, context, "git status --porcelain=v2");
	const records: string[] = [];
	for (let i = 0; i < tokens.length; ) {
		const token = tokens[i] as string;
		if (token.length === 0) fail(context, "malformed-git-data", "git status: empty record");
		const kind = token[0] as string;
		if (kind === "?") {
			if (token[1] !== " " || token.length < 3) {
				fail(context, "malformed-git-data", "git status: malformed untracked record");
			}
			validateEntryPath(token.slice(2), context, "malformed-git-data", "untracked status path");
			records.push(token);
			i += 1;
			continue;
		}
		if (kind === "!") fail(context, "malformed-git-data", "git status: unexpected ignored record");
		if (kind === "u") {
			// u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>
			const fields = token.split(" ");
			const path = fields.length > 10 ? fields.slice(10).join(" ") : "?";
			fail(context, "unmerged-input", `relevant input ${path} is unmerged in the index; resolve and restage`);
		}
		if (kind !== "1" && kind !== "2") {
			fail(context, "malformed-git-data", `git status: unknown record kind ${JSON.stringify(kind)}`);
		}
		// 1: <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>
		// 2: <XY> <sub> <mH> <mI> <mW> <hH> <hI> <X><score> <path>
		const headerFields = kind === "1" ? 8 : 9;
		const fields = token.split(" ");
		if (fields.length <= headerFields) {
			fail(context, "malformed-git-data", "git status: truncated changed record");
		}
		const xy = fields[1] as string;
		if (xy.length !== 2) {
			fail(context, "malformed-git-data", `git status: malformed XY field ${JSON.stringify(xy)}`);
		}
		for (const field of [fields[3], fields[4], fields[5]]) {
			if (/^[0-7]{6}$/.test(field as string) === false) {
				fail(context, "malformed-git-data", `git status: malformed mode field ${JSON.stringify(field)}`);
			}
		}
		if (kind === "2" && /^[RC][1-9][0-9]{0,2}$/.test(fields[8] as string) === false) {
			fail(context, "malformed-git-data", `git status: malformed rename score field ${JSON.stringify(fields[8])}`);
		}
		const path = fields.slice(headerFields).join(" ");
		const origPath = kind === "2" ? tokens[i + 1] : undefined;
		if (kind === "2" && (origPath === undefined || origPath.length === 0)) {
			fail(context, "malformed-git-data", "git status: renamed record is missing its NUL-terminated original path");
		}
		const x = xy[0] as string;
		const y = xy[1] as string;
		const indexMode = fields[4] as string;
		validateEntryPath(path, context, "malformed-git-data", "status path");
		if (origPath !== undefined) validateEntryPath(origPath, context, "malformed-git-data", "rename origin path");
		// Additions legitimately carry zero HEAD fields and staged deletions a
		// zero index entry; any other zero index mode is an intent-to-add entry.
		if (indexMode === "000000" && x !== "D") {
			fail(context, "unsupported-state", `relevant input ${path} has a zero index mode (${xy}); intent-to-add entries are not supported`);
		}
		if (mode === "staged" && y !== ".") {
			fail(context, "unstaged-input", `relevant input ${path} has unstaged worktree changes (${xy}); stage or revert them before capturing`);
		}
		records.push(origPath !== undefined ? `${token}\0${origPath}` : token);
		i += kind === "2" ? 2 : 1;
	}
	return records;
}

/** Untracked AND ignored relevant files; ignored ones have no indexed provenance either. */
function readUntrackedOthers(
	root: string,
	scopes: readonly string[],
	mode: InventoryMode,
	context: string,
): readonly string[] {
	const output = runGit(root, ["ls-files", "--others", "-z", "--", ...scopes], context);
	const tokens = splitNulRecords(output, context, "git ls-files --others");
	const paths: string[] = [];
	const seen = new Set<string>();
	for (const token of tokens) {
		if (token.length === 0) fail(context, "malformed-git-data", "git ls-files --others: empty record");
		const path = validateEntryPath(token, context, "malformed-git-data", "untracked path");
		if (seen.has(path)) fail(context, "malformed-git-data", `git ls-files --others: duplicate record for ${path}`);
		seen.add(path);
		paths.push(path);
	}
	paths.sort(compareStrings);
	if (mode === "staged" && paths.length > 0) {
		const preview = paths.slice(0, 5).join(", ");
		fail(
			context,
			"unstaged-input",
			`relevant scopes contain ${paths.length} untracked or ignored file(s) with no indexed provenance: ${preview}${paths.length > 5 ? ", …" : ""}`,
		);
	}
	return paths;
}

function blobIdFor(bytes: Buffer, objectFormat: ObjectFormat): string {
	return createHash(objectFormat)
		.update(`blob ${bytes.length}\0`)
		.update(bytes)
		.digest("hex");
}

function observeIdentity(stats: BigIntStats): FileIdentity {
	return deepFreeze({
		dev: stats.dev,
		ino: stats.ino,
		mode: stats.mode,
		size: stats.size,
		mtimeMs: stats.mtimeMs,
		ctimeMs: stats.ctimeMs,
	});
}

function sameIdentity(a: FileIdentity, b: FileIdentity): boolean {
	return (
		a.dev === b.dev &&
		a.ino === b.ino &&
		a.mode === b.mode &&
		a.size === b.size &&
		a.mtimeMs === b.mtimeMs &&
		a.ctimeMs === b.ctimeMs
	);
}

function observeWorkingFile(
	root: string,
	relPath: string,
	mode: InventoryMode,
	context: string,
	indexMatch?: { readonly blob: string; readonly mode: IndexedCaptureInput["mode"]; readonly objectFormat: ObjectFormat },
): WorkingObservation {
	const segments = relPath.split("/");
	let probe = root;
	for (let i = 0; i < segments.length - 1; i += 1) {
		probe = join(probe, segments[i] as string);
		const walked = segments.slice(0, i + 1).join("/");
		let stats;
		try {
			stats = lstatSync(probe);
		} catch {
			if (mode === "staged") {
				fail(context, "unstaged-input", `relevant input ${relPath}: parent path ${walked} is missing from the worktree`);
			}
			// Observation mode records actual absence: a missing or unreadable
			// ancestor means the input cannot be a present regular file, so the
			// file-level lstat below settles it as absent (or as a real file if
			// the tree raced back into existence).
			break;
		}
		if (stats.isSymbolicLink()) {
			fail(context, "unsupported-state", `relevant input ${relPath}: traverses symlink ancestor ${walked}`);
		}
		if (stats.isDirectory() === false) {
			fail(context, "unsupported-state", `relevant input ${relPath}: parent ${walked} is not a directory`);
		}
	}
	const absolute = join(root, ...segments);
	let before: BigIntStats;
	try {
		before = lstatSync(absolute, { bigint: true });
	} catch {
		if (mode === "staged") {
			fail(context, "unstaged-input", `relevant input ${relPath} is missing from the worktree`);
		}
		return { kind: "absent" };
	}
	if (before.isFile() === false) {
		fail(context, "unsupported-state", `relevant input ${relPath} is not an ordinary file (symlink, fifo, device or directory)`);
	}
	let bytes: Buffer;
	try {
		bytes = readFileSync(absolute);
	} catch (error) {
		fail(context, "unsupported-state", `relevant input ${relPath} is unreadable: ${(error as Error).message}`);
	}
	let after: BigIntStats;
	try {
		after = lstatSync(absolute, { bigint: true });
	} catch {
		fail(context, "worktree-race", `relevant input ${relPath} disappeared while being read`);
	}
	if (sameIdentity(observeIdentity(before), observeIdentity(after)) === false) {
		fail(context, "worktree-race", `relevant input ${relPath} changed while being read`);
	}
	if (indexMatch !== undefined) {
		if (process.platform !== "win32") {
			const worktreeClass = (before.mode & 0o111n) !== 0n ? "100755" : "100644";
			if (worktreeClass !== indexMatch.mode) {
				fail(
					context,
					"unstaged-input",
					`relevant input ${relPath}: worktree permission class ${worktreeClass} does not match indexed mode ${indexMatch.mode}`,
				);
			}
		}
		const computed = blobIdFor(bytes, indexMatch.objectFormat);
		if (computed !== indexMatch.blob) {
			fail(
				context,
				"unstaged-input",
				`relevant input ${relPath}: working bytes hash ${computed} differs from indexed blob ${indexMatch.blob}; staged capture requires raw byte equality`,
			);
		}
	}
	return {
		kind: "file",
		identity: observeIdentity(before),
		sha256: createHash("sha256").update(bytes).digest("hex"),
		size: before.size,
	};
}

function readGitInventoryState(
	root: string,
	scopes: readonly string[],
	mode: InventoryMode,
	context: string,
): GitInventoryState {
	const head = readHeadCommit(root, context);
	const objectFormat = readObjectFormat(root, context);
	const expectedWidth = objectFormat === "sha1" ? 40 : 64;
	if (head.length !== expectedWidth) {
		fail(context, "malformed-git-data", `HEAD commit ID does not match the ${objectFormat} object format`);
	}
	const entries = readIndexEntries(root, scopes, objectFormat, context);
	assertSupportedIndexFlags(root, scopes, context);
	const statusRecords = readStatusRecords(root, scopes, mode, context);
	const untracked = readUntrackedOthers(root, scopes, mode, context);
	return { head, objectFormat, entries, statusRecords, untracked };
}

function statusXy(record: string): string {
	const header = record.split("\0", 1)[0] ?? "";
	const xy = header.split(" ")[1];
	return typeof xy === "string" ? xy : "??";
}

function statusRaceCategory(baseline: readonly string[], current: readonly string[]): string {
	if (baseline.length === current.length) {
		for (let i = 0; i < baseline.length; i += 1) {
			const before = baseline[i] as string;
			const after = current[i] as string;
			if (before === after) continue;
			const beforeXy = statusXy(before);
			const afterXy = statusXy(after);
			if (beforeXy[0] === afterXy[0] && beforeXy[1] !== afterXy[1]) return "worktree-race";
		}
	}
	return "index-race";
}

function truncateForMessage(text: string): string {
	return text.length > 120 ? `${text.slice(0, 117)}...` : text;
}

function recordDiffPreview(baseline: readonly string[], current: readonly string[]): string {
	for (let i = 0; i < Math.max(baseline.length, current.length); i += 1) {
		const before = baseline[i];
		const after = current[i];
		if (before !== after) {
			return `${truncateForMessage(before ?? "(missing)")} -> ${truncateForMessage(after ?? "(missing)")}`;
		}
	}
	return "no visible difference";
}

function compareGitState(
	baseline: GitInventoryState,
	current: GitInventoryState,
	context: string,
	when: string,
	untrackedRaceCategory: "worktree-race" | "index-race",
): void {
	if (current.head !== baseline.head) {
		fail(context, "head-race", `HEAD moved ${when}: ${baseline.head} -> ${current.head}`);
	}
	if (current.objectFormat !== baseline.objectFormat) {
		fail(context, "head-race", `object format changed ${when}: ${baseline.objectFormat} -> ${current.objectFormat}`);
	}
	const total = Math.max(baseline.entries.length, current.entries.length);
	for (let i = 0; i < total; i += 1) {
		const before = baseline.entries[i];
		const after = current.entries[i];
		if (before === undefined || after === undefined || before.path !== after.path) {
			const beforePath = before?.path ?? "(missing)";
			const afterPath = after?.path ?? "(missing)";
			fail(context, "index-race", `relevant index membership changed ${when}: ${beforePath} -> ${afterPath}`);
		}
		if (before.mode !== after.mode || before.blob !== after.blob) {
			fail(
				context,
				"index-race",
				`relevant index entry ${before.path} changed ${when}: ${before.mode} ${before.blob} -> ${after.mode} ${after.blob}`,
			);
		}
	}
	if (sameStringList(baseline.statusRecords, current.statusRecords) === false) {
		fail(
			context,
			statusRaceCategory(baseline.statusRecords, current.statusRecords),
			`relevant status changed ${when}: ${recordDiffPreview(baseline.statusRecords, current.statusRecords)}`,
		);
	}
	if (sameStringList(baseline.untracked, current.untracked) === false) {
		fail(context, untrackedRaceCategory, `relevant untracked set changed ${when}`);
	}
}

function compareInventories(baseline: Inventory, current: Inventory, context: string, when: string): void {
	compareGitState(baseline, current, context, when, "worktree-race");
	for (const [path, before] of baseline.observations) {
		const after = current.observations.get(path);
		if (after === undefined) {
			fail(context, "worktree-race", `${path} stopped being a relevant input ${when}`);
		}
		if (before.kind === "absent" && after.kind === "absent") continue;
		if (before.kind === "file" && after.kind === "file") {
			if (before.sha256 !== after.sha256 || before.size !== after.size) {
				fail(context, "worktree-race", `working bytes of ${path} changed ${when}`);
			}
			if (sameIdentity(before.identity, after.identity) === false) {
				fail(context, "worktree-race", `file identity of ${path} changed ${when} (edit/restore or replace cycle)`);
			}
			continue;
		}
		fail(context, "worktree-race", `presence of ${path} changed ${when}`);
	}
	for (const path of current.observations.keys()) {
		if (baseline.observations.has(path) === false) {
			fail(context, "worktree-race", `${path} became a relevant input ${when}`);
		}
	}
}


function captureInventory(root: string, scopes: readonly string[], mode: InventoryMode, context: string): Inventory {
	assertNoRedirection(context);
	const state = readGitInventoryState(root, scopes, mode, context);
	if (mode === "staged" && state.entries.length === 0) {
		fail(
			context,
			"unstaged-input",
			"no relevant indexed files match the declared pathspecs; staged provenance requires at least one indexed entry",
		);
	}
	const observations = new Map<string, WorkingObservation>();
	for (const entry of state.entries) {
		observations.set(
			entry.path,
			observeWorkingFile(
				root,
				entry.path,
				mode,
				context,
				mode === "staged"
					? { blob: entry.blob, mode: entry.mode, objectFormat: state.objectFormat }
					: undefined,
			),
		);
	}
	for (const path of state.untracked) {
		if (observations.has(path) === false) {
			observations.set(path, observeWorkingFile(root, path, "observation", context));
		}
	}
	// Confirmation pass: a lasting index/HEAD/status transition must not read as one snapshot.
	const confirmation = readGitInventoryState(root, scopes, mode, context);
	compareGitState(state, confirmation, context, "while capturing", "index-race");
	return { ...state, observations };
}

/**
 * Begin an explicit staged capture over `pathspecs`. Requires every relevant
 * input to be staged, unmodified in the worktree, and raw-byte identical to
 * its indexed blob. Returns deep-frozen provenance against the real current
 * commit plus an `assertUnchanged` handle that re-validates the whole
 * inventory (HEAD, format, entries, working bytes and identity).
 */
export function beginStagedInputCapture(root: string, pathspecs: readonly string[]): StagedInputCapture {
	const context = "staged capture";
	const canonicalRoot = openWorktreeRoot(root, context);
	const scopes = normalizeScopes(pathspecs, context);
	const baseline = captureInventory(canonicalRoot, scopes, "staged", context);
	const provenance = deepFreeze<StagedInputProvenance>({
		schema: STAGED_INPUT_PROVENANCE_SCHEMA,
		mode: "staged",
		baseHead: baseline.head,
		objectFormat: baseline.objectFormat,
		pathspecs: deepFreeze([...scopes]),
		entries: deepFreeze(baseline.entries.map((entry) => deepFreeze({ ...entry }))),
	});
	return {
		provenance,
		assertUnchanged(): void {
			assertRelevantInputsUnchanged(canonicalRoot, scopes, baseline, context);
		},
	};
}

function assertRelevantInputsUnchanged(
	root: string,
	scopes: readonly string[],
	baseline: Inventory,
	context: string,
): void {
	let current: Inventory;
	try {
		current = captureInventory(root, scopes, "staged", context);
	} catch (error) {
		fail(context, "worktree-race", `relevant inputs changed after the capture began: ${(error as Error).message}`);
	}
	compareInventories(baseline, current, context, "after the capture began");
}

/**
 * Record the actual relevant-tree state (HEAD, index, status, real working
 * bytes/identity/absence, including initially relevant untracked regular
 * files) WITHOUT requiring worktree==index. Used only by the unchanged
 * dep-field-free dirty exception after its existing guard; returns no staged
 * provenance.
 */
export function beginRelevantInputObservation(
	root: string,
	pathspecs: readonly string[],
): { assertUnchanged(): void } {
	const context = "staged capture";
	const canonicalRoot = openWorktreeRoot(root, context);
	const scopes = normalizeScopes(pathspecs, context);
	const baseline = captureInventory(canonicalRoot, scopes, "observation", context);
	return {
		assertUnchanged(): void {
			let current: Inventory;
			try {
				current = captureInventory(canonicalRoot, scopes, "observation", context);
			} catch (error) {
				fail(context, "worktree-race", `relevant inputs changed after the observation began: ${(error as Error).message}`);
			}
			compareInventories(baseline, current, context, "after the observation began");
		},
	};
}

function assertStrictlySorted(values: readonly string[], context: string, category: string, what: string): void {
	for (let i = 1; i < values.length; i += 1) {
		const previous = values[i - 1] as string;
		const current = values[i] as string;
		if (compareStrings(previous, current) >= 0) {
			fail(context, category, `${what} must be strictly sorted without duplicates: ${JSON.stringify(previous)} >= ${JSON.stringify(current)}`);
		}
	}
}

/**
 * Offline structural validation of stored provenance. Historical objects need
 * not exist locally and baseHead need not equal the current HEAD. Malformed
 * data is never reordered into validity.
 */
export function parseStagedInputProvenance(value: unknown, what: string): StagedInputProvenance {
	const context = what;
	if (typeof value !== "object" || value === null || Array.isArray(value)) {
		fail(context, "provenance-shape", "staged input provenance must be a JSON object");
	}
	const record = value as Record<string, unknown>;
	const keys = Object.keys(record).sort(compareStrings);
	if (keys.length !== PROVENANCE_KEYS.length || keys.some((key, index) => key !== PROVENANCE_KEYS[index])) {
		fail(context, "provenance-keys", `expected exactly {${PROVENANCE_KEYS.join(", ")}}, found {${keys.join(", ")}}`);
	}
	if (record["schema"] !== STAGED_INPUT_PROVENANCE_SCHEMA) {
		fail(context, "provenance-schema", `expected ${STAGED_INPUT_PROVENANCE_SCHEMA}, found ${JSON.stringify(record["schema"] ?? null)}`);
	}
	if (record["mode"] !== "staged") {
		fail(context, "provenance-mode", `expected "staged", found ${JSON.stringify(record["mode"] ?? null)}`);
	}
	const objectFormat = record["objectFormat"];
	if (objectFormat !== "sha1" && objectFormat !== "sha256") {
		fail(context, "provenance-object-format", `expected "sha1" or "sha256", found ${JSON.stringify(objectFormat ?? null)}`);
	}
	const baseHead = assertOid(record["baseHead"], objectFormat, context, "baseHead", "provenance-base-head");
	const rawPathspecs = record["pathspecs"];
	if (Array.isArray(rawPathspecs) === false || rawPathspecs.length === 0) {
		fail(context, "provenance-pathspecs", "pathspecs must be a nonempty array of scope strings");
	}
	const pathspecs = rawPathspecs.map((raw) => validateScope(raw, context, "provenance-pathspecs", "pathspec"));
	assertStrictlySorted(pathspecs, context, "provenance-pathspecs", "pathspecs");
	const rawEntries = record["entries"];
	if (Array.isArray(rawEntries) === false || rawEntries.length === 0) {
		fail(context, "provenance-entries", "entries must be a nonempty array of indexed input records");
	}
	const entries: IndexedCaptureInput[] = [];
	for (const raw of rawEntries) {
		if (typeof raw !== "object" || raw === null || Array.isArray(raw)) {
			fail(context, "provenance-entries", "each entry must be a JSON object");
		}
		const entry = raw as Record<string, unknown>;
		const entryKeys = Object.keys(entry).sort(compareStrings);
		if (entryKeys.length !== ENTRY_KEYS.length || entryKeys.some((key, index) => key !== ENTRY_KEYS[index])) {
			fail(context, "provenance-entries", `each entry must have exactly {${ENTRY_KEYS.join(", ")}}, found {${entryKeys.join(", ")}}`);
		}
		const pathValue = entry["path"];
		if (typeof pathValue !== "string") {
			fail(context, "provenance-entries", `entry.path: expected a string, found ${JSON.stringify(pathValue ?? null)}`);
		}
		const path = validateEntryPath(pathValue, context, "provenance-entries", "entry.path");
		const mode = entry["mode"];
		if (mode !== "100644" && mode !== "100755") {
			fail(context, "provenance-entries", `entry.mode for ${path} must be "100644" or "100755", found ${JSON.stringify(mode ?? null)}`);
		}
		const blob = assertOid(entry["blob"], objectFormat, context, `entry.blob for ${path}`, "provenance-entries");
		entries.push({ path, mode, blob });
	}
	assertStrictlySorted(
		entries.map((entry) => entry.path),
		context,
		"provenance-entries",
		"entries",
	);
	const matcher = buildScopeMatcher(pathspecs, context);
	for (const entry of entries) {
		if (matchesScope(matcher, entry.path) === false) {
			fail(context, "provenance-entries", `entry ${entry.path} is outside the declared pathspecs`);
		}
	}
	return deepFreeze<StagedInputProvenance>({
		schema: STAGED_INPUT_PROVENANCE_SCHEMA,
		mode: "staged",
		baseHead,
		objectFormat,
		pathspecs: deepFreeze([...pathspecs]),
		entries: deepFreeze(entries.map((entry) => deepFreeze({ ...entry }))),
	});
}

function toPosix(path: string): string {
	return sep === "/" ? path : path.split(sep).join("/");
}

function resolveAdminDir(root: string, args: readonly string[], context: string): string {
	const raw = runGit(root, args, context).toString("utf8").trim();
	if (raw.length === 0) fail(context, "malformed-git-data", `git ${args.join(" ")} returned an empty path`);
	try {
		return realpathSync(resolve(root, raw));
	} catch {
		fail(context, "malformed-git-data", `git ${args.join(" ")} returned an unusable path: ${raw}`);
	}
}

/**
 * Refuse outputs that are lexical or real-path aliases of relevant scopes, sit
 * at a relevant input's ancestor/descendant location, traverse existing
 * symlink ancestors below root, or fall inside the actual Git administrative
 * directories. Ordinary outside-root destinations are allowed. Call this with
 * every ACTUAL final projection filename after digest computation — never a
 * guessed placeholder — and never cure overlap by excluding outputs from the
 * relevant set.
 */
export function assertCaptureOutputPathsDisjoint(
	root: string,
	pathspecs: readonly string[],
	outputPaths: readonly string[],
): void {
	const context = "staged capture output";
	assertNoRedirection(context);
	const canonicalRoot = openWorktreeRoot(root, context);
	const scopes = normalizeScopes(pathspecs, context);
	const matcher = buildScopeMatcher(scopes, context);
	const objectFormat = readObjectFormat(canonicalRoot, context);
	const entryPaths = readIndexEntries(canonicalRoot, scopes, objectFormat, context).map((entry) => entry.path);
	const adminDirs = [
		resolveAdminDir(canonicalRoot, ["rev-parse", "--absolute-git-dir"], context),
		resolveAdminDir(canonicalRoot, ["rev-parse", "--git-common-dir"], context),
	];
	for (const output of outputPaths) {
		assertOutputDisjoint(canonicalRoot, matcher, entryPaths, adminDirs, output, context);
	}
}

function assertOutputDisjoint(
	root: string,
	matcher: ScopeMatcher,
	entryPaths: readonly string[],
	adminDirs: readonly string[],
	output: string,
	context: string,
): void {
	if (typeof output !== "string" || output.length === 0) {
		fail(context, "output-overlap", "each output path must be a nonempty absolute file path");
	}
	if (isAbsolute(output) === false) {
		fail(context, "output-overlap", `output path must be absolute: ${output}`);
	}
	const relLexical = toPosix(relative(root, output));
	if (relLexical.length === 0) {
		fail(context, "output-overlap", `output path must be a file, not the repository root: ${output}`);
	}
	// relative() cannot express a different Windows drive or UNC root and
	// returns the absolute target instead; treat that as outside the root too.
	const outsideRoot = relLexical === ".." || relLexical.startsWith("../") || isAbsolute(relLexical);
	const realOutput = outsideRoot
		? resolveRealOutputPath(output, context)
		: resolveInRootOutput(root, relLexical, output, context);
	const relReal = toPosix(relative(root, realOutput));
	if (matchesScope(matcher, relLexical) || matchesScope(matcher, relReal)) {
		const aliased = relReal !== relLexical ? ` (resolves to ${relReal})` : "";
		fail(context, "output-overlap", `output ${output} lands inside a relevant capture scope: ${relLexical}${aliased}; outputs must never overwrite relevant inputs`);
	}
	for (const input of entryPaths) {
		if (
			relLexical === input ||
			relReal === input ||
			input.startsWith(`${relLexical}/`) ||
			input.startsWith(`${relReal}/`) ||
			relLexical.startsWith(`${input}/`) ||
			relReal.startsWith(`${input}/`)
		) {
			fail(context, "output-overlap", `output ${output} is an ancestor, descendant or replacement of relevant input ${input}`);
		}
	}
	for (const admin of adminDirs) {
		const adminPosix = toPosix(admin);
		for (const candidate of [output, realOutput]) {
			const candidatePosix = toPosix(candidate);
			if (candidatePosix === adminPosix || candidatePosix.startsWith(`${adminPosix}/`)) {
				fail(context, "output-overlap", `output ${candidate} is inside the Git administrative directory ${admin}`);
			}
		}
	}
}

/**
 * Real path of an in-root output. Existing ancestors are canonicalized and
 * existing symlink components refused; an existing final component is
 * canonicalized as well so a case-insensitive filesystem cannot alias a
 * relevant input through letter case alone.
 */
function resolveInRootOutput(root: string, relLexical: string, output: string, context: string): string {
	const segments = relLexical.split("/");
	let probe = root;
	let consumed = 0;
	for (let i = 0; i < segments.length - 1; i += 1) {
		const next = join(probe, segments[i] as string);
		let stats;
		try {
			stats = lstatSync(next);
		} catch (error) {
			if (isMissingEntryError(error)) break;
			fail(context, "output-overlap", `output ${output}: unusable parent path ${next}: ${(error as Error).message}`);
		}
		if (stats.isSymbolicLink()) {
			fail(context, "output-overlap", `output ${output} traverses existing symlink ancestor ${next}; write real directories instead`);
		}
		if (stats.isDirectory() === false) {
			fail(context, "output-overlap", `output ${output}: parent ${next} is not a directory`);
		}
		consumed = i + 1;
		probe = next;
	}
	const realPrefix = realpathSync(probe);
	const remaining = segments.slice(consumed);
	if (remaining.length > 1) {
		// An ancestor is missing, so the final component cannot exist either.
		return join(realPrefix, ...remaining);
	}
	const finalPath = join(probe, remaining[0] as string);
	let finalStats;
	try {
		finalStats = lstatSync(finalPath);
	} catch (error) {
		if (isMissingEntryError(error) === false) {
			fail(context, "output-overlap", `output ${output}: unusable path ${finalPath}: ${(error as Error).message}`);
		}
		return join(realPrefix, remaining[0] as string);
	}
	if (finalStats.isSymbolicLink()) {
		fail(context, "output-overlap", `output ${output} traverses existing symlink ancestor ${finalPath}; write real directories instead`);
	}
	return realpathSync(finalPath);
}

/**
 * Real path of an outside-root output: resolve the deepest existing ancestor
 * and re-append the not-yet-existing tail. Symlink ancestors outside the root
 * are legitimate (system mounts, /tmp); resolving them is what lets the
 * scope/admin comparisons catch aliases back into the repository.
 */
function resolveRealOutputPath(output: string, context: string): string {
	let probe = resolve(output);
	const missing: string[] = [];
	for (;;) {
		try {
			lstatSync(probe);
		} catch (error) {
			if (isMissingEntryError(error) === false) {
				fail(context, "output-overlap", `output ${output}: unusable path ${probe}: ${(error as Error).message}`);
			}
			const parent = dirname(probe);
			if (parent === probe) {
				fail(context, "output-overlap", `output ${output}: no existing ancestor directory`);
			}
			missing.unshift(basename(probe));
			probe = parent;
			continue;
		}
		try {
			const real = realpathSync(probe);
			return missing.length === 0 ? real : join(real, ...missing);
		} catch (error) {
			fail(context, "output-overlap", `output ${output}: could not resolve the real path of ${probe}: ${(error as Error).message}`);
		}
	}
}
