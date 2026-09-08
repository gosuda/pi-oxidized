#!/usr/bin/env bun
/**
 * Tests for the shared staged-input capture primitive
 * (scripts/verification/capture-inputs.ts).
 *
 * Every Git interaction runs against a real, disposable repository under
 * target/capture-inputs-tests/ (mkdtempSync per case, local throwaway commit
 * identity, real git/file operations — never mocked Git output). Synthetic
 * histories stay confined to those owned repositories; the project's own
 * index, config and history are never touched. Owned scratch is removed in
 * afterAll.
 */

import { spawnSync } from "node:child_process";
import { chmodSync, lstatSync, mkdirSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { afterAll, describe, expect, test } from "bun:test";

import {
	STAGED_INPUT_PROVENANCE_SCHEMA,
	assertCaptureOutputPathsDisjoint,
	beginRelevantInputObservation,
	beginStagedInputCapture,
	parseStagedInputProvenance,
} from "./capture-inputs.ts";
import type { IndexedCaptureInput, StagedInputProvenance } from "./capture-inputs.ts";

const TESTS_ROOT = resolve(import.meta.dirname, "../../target/capture-inputs-tests");
const OWNED_DIRECTORIES: string[] = [];
const TEST_TIMEOUT_MS = 120_000;

/** One-time symlink capability probe; platform capabilities are stated, not faked. */
function symlinksSupported(): boolean {
	try {
		mkdirSync(TESTS_ROOT, { recursive: true });
		const target = join(TESTS_ROOT, `probe-target-${process.pid}`);
		const link = join(TESTS_ROOT, `probe-link-${process.pid}`);
		writeFileSync(target, "probe\n");
		symlinkSync(target, link);
		rmSync(link);
		rmSync(target);
		return true;
	} catch {
		return false;
	}
}
const SYMLINKS_SUPPORTED = symlinksSupported();

/** One-time case-insensitivity probe; platform capabilities are stated, not faked. */
function caseInsensitiveFilesystem(): boolean {
	try {
		mkdirSync(TESTS_ROOT, { recursive: true });
		const target = join(TESTS_ROOT, `CaseProbe-${process.pid}.tmp`);
		writeFileSync(target, "probe\n");
		const variant = join(TESTS_ROOT, `caseprobe-${process.pid}.tmp`);
		const aliased = lstatSync(variant).isFile();
		rmSync(target);
		return aliased;
	} catch {
		return false;
	}
}
const CASE_INSENSITIVE_FS = caseInsensitiveFilesystem();

const TEST_IDENTITY = {
	GIT_AUTHOR_NAME: "Capture Input Tests",
	GIT_AUTHOR_EMAIL: "capture-inputs-tests@example.invalid",
	GIT_COMMITTER_NAME: "Capture Input Tests",
	GIT_COMMITTER_EMAIL: "capture-inputs-tests@example.invalid",
} as const;

/**
 * Fixture git subprocesses stay isolated from the harness environment:
 * redirected-repository variables never reach a fixture subprocess (the
 * production helper still refuses such environments, and the deliberate
 * redirection test below still exercises that refusal), and user hooks or
 * commit signing are disabled per invocation without touching real or global
 * Git configuration.
 */
const REDIRECTED_REPOSITORY_VARS = [
	"GIT_DIR",
	"GIT_WORK_TREE",
	"GIT_INDEX_FILE",
	"GIT_COMMON_DIR",
	"GIT_OBJECT_DIRECTORY",
	"GIT_ALTERNATE_OBJECT_DIRECTORIES",
] as const;

const FIXTURE_GIT_CONFIG = ["-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null"] as const;

function git(repo: string, args: readonly string[], options?: { readonly allowFailure?: boolean }): string {
	const env: Record<string, string | undefined> = {
		...process.env,
		...TEST_IDENTITY,
		GIT_OPTIONAL_LOCKS: "0",
	};
	for (const name of REDIRECTED_REPOSITORY_VARS) delete env[name];
	const result = spawnSync("git", [...FIXTURE_GIT_CONFIG, ...args], {
		cwd: repo,
		encoding: "buffer",
		maxBuffer: 64 * 1024 * 1024,
		env,
	});
	const stdout = result.stdout === null ? "" : result.stdout.toString("utf8");
	if (result.status !== 0 && options?.allowFailure !== true) {
		const stderr = result.stderr === null ? "" : result.stderr.toString("utf8");
		throw new Error(`git ${args.join(" ")} failed in ${repo}: ${stderr}`);
	}
	return stdout;
}

function writeFixture(repo: string, relPath: string, content: string): void {
	const absolute = join(repo, relPath);
	mkdirSync(dirname(absolute), { recursive: true });
	writeFileSync(absolute, content);
}

function newRepo(label: string, format: "sha1" | "sha256" = "sha1"): string {
	mkdirSync(TESTS_ROOT, { recursive: true });
	const repo = mkdtempSync(join(TESTS_ROOT, `${label}-`));
	OWNED_DIRECTORIES.push(repo);
	const initArgs =
		format === "sha256" ? ["init", "-q", "-b", "main", "--object-format=sha256"] : ["init", "-q", "-b", "main"];
	git(repo, initArgs);
	writeFixture(repo, "seed.txt", "seed\n");
	writeFixture(repo, "data/a.txt", "a\n");
	writeFixture(repo, "data/sub/b.txt", "b\n");
	writeFixture(repo, "other/other.txt", "other\n");
	writeFixture(repo, ".gitignore", "data/ignored-*.txt\n");
	git(repo, ["add", "."]);
	git(repo, ["commit", "-q", "-m", "init"]);
	return repo;
}

/** Independent parse of `git ls-files --stage -z` for membership equality checks. */
function indexedEntries(repo: string, scopes: readonly string[]): IndexedCaptureInput[] {
	const output = git(repo, ["ls-files", "--stage", "-z", "--", ...scopes]);
	const entries: IndexedCaptureInput[] = [];
	for (const token of output.split("\0")) {
		if (token.length === 0) continue;
		const tab = token.indexOf("\t");
		if (tab <= 0) throw new Error(`unexpected ls-files record: ${JSON.stringify(token)}`);
		const meta = token.slice(0, tab).split(" ");
		const path = token.slice(tab + 1);
		entries.push({
			path,
			mode: meta[0] as IndexedCaptureInput["mode"],
			blob: meta[1] ?? "",
		});
	}
	entries.sort((a, b) => (a.path < b.path ? -1 : a.path > b.path ? 1 : 0));
	return entries;
}

function entryFor(path: string, overrides: Record<string, unknown> = {}): Record<string, unknown> {
	return { path, mode: "100644", blob: "b".repeat(40), ...overrides };
}

function validProvenance(overrides: Record<string, unknown> = {}): Record<string, unknown> {
	return {
		schema: STAGED_INPUT_PROVENANCE_SCHEMA,
		mode: "staged",
		baseHead: "a".repeat(40),
		objectFormat: "sha1",
		pathspecs: ["data"],
		entries: [entryFor("data/a.txt")],
		...overrides,
	};
}

afterAll(() => {
	for (const directory of OWNED_DIRECTORIES.splice(0)) {
		rmSync(directory, { recursive: true, force: true });
	}
	rmSync(TESTS_ROOT, { recursive: true, force: true });
});

describe("staged-input capture (real Git repositories)", () => {
	test(
		"case 1: staged relevant modification/addition with equal worktree captures exactly the index",
		() => {
			const repo = newRepo("case1");
			writeFixture(repo, "data/a.txt", "a-modified\n");
			writeFixture(repo, "data/new.txt", "new\n");
			git(repo, ["add", "data/a.txt", "data/new.txt"]);

			const handle = beginStagedInputCapture(repo, ["data"]);
			expect(handle.provenance.schema).toBe(STAGED_INPUT_PROVENANCE_SCHEMA);
			expect(handle.provenance.mode).toBe("staged");
			expect(handle.provenance.baseHead).toBe(git(repo, ["rev-parse", "HEAD"]).trim());
			expect(handle.provenance.pathspecs).toEqual(["data"]);
			expect([...handle.provenance.entries]).toEqual(indexedEntries(repo, ["data"]));
			handle.assertUnchanged();

			expect(Object.isFrozen(handle.provenance)).toBe(true);
			expect(Object.isFrozen(handle.provenance.entries)).toBe(true);
			expect(Object.isFrozen(handle.provenance.pathspecs)).toBe(true);
			for (const entry of handle.provenance.entries) {
				expect(Object.isFrozen(entry)).toBe(true);
			}
		},
		TEST_TIMEOUT_MS,
	);

	test(
		"case 1b: clean explicit staged mode succeeds without manufacturing a staged diff",
		() => {
			const repo = newRepo("case1b");
			const handle = beginStagedInputCapture(repo, ["data"]);
			expect(handle.provenance.entries.map((entry) => entry.path)).toEqual(["data/a.txt", "data/sub/b.txt"]);
			expect(handle.provenance.entries.map((entry) => entry.blob)).toEqual(
				indexedEntries(repo, ["data"]).map((entry) => entry.blob),
			);
			handle.assertUnchanged();
		},
		TEST_TIMEOUT_MS,
	);

	test(
		"case 2: unrelated staged, unstaged and untracked changes neither invalidate nor enlarge the snapshot",
		() => {
			const repo = newRepo("case2");
			writeFixture(repo, "other/other.txt", "staged-unrelated\n");
			git(repo, ["add", "other/other.txt"]);
			writeFixture(repo, "seed.txt", "unstaged-unrelated\n");
			writeFixture(repo, "other/untracked.txt", "untracked-unrelated\n");

			const handle = beginStagedInputCapture(repo, ["data"]);
			expect(handle.provenance.entries.map((entry) => entry.path)).toEqual(["data/a.txt", "data/sub/b.txt"]);
			handle.assertUnchanged();
		},
		TEST_TIMEOUT_MS,
	);

	test(
		"case 3a: relevant unstaged content change refuses",
		() => {
			const repo = newRepo("case3a");
			writeFixture(repo, "data/a.txt", "unstaged edit\n");
			expect(() => beginStagedInputCapture(repo, ["data"])).toThrow(/\(unstaged-input\)/);
		},
		TEST_TIMEOUT_MS,
	);

	test.skipIf(process.platform === "win32")(
		"case 3b: relevant unstaged mode change refuses (POSIX executable-bit class)",
		() => {
			const repo = newRepo("case3b");
			chmodSync(join(repo, "data/a.txt"), 0o755);
			expect(() => beginStagedInputCapture(repo, ["data"])).toThrow(/\(unstaged-input\)/);
		},
		TEST_TIMEOUT_MS,
	);

	test(
		"case 3c: untracked relevant file refuses",
		() => {
			const repo = newRepo("case3c");
			writeFixture(repo, "data/untracked.txt", "untracked\n");
			expect(() => beginStagedInputCapture(repo, ["data"])).toThrow(/\(unstaged-input\)/);
		},
		TEST_TIMEOUT_MS,
	);

	test(
		"case 3d: ignored relevant untracked file refuses (others read without exclude-standard)",
		() => {
			const repo = newRepo("case3d");
			writeFixture(repo, "data/ignored-secret.txt", "ignored\n");
			expect(() => beginStagedInputCapture(repo, ["data"])).toThrow(/\(unstaged-input\)/);
		},
		TEST_TIMEOUT_MS,
	);

	test(
		"case 3e: empty intent-to-add entry refuses",
		() => {
			const repo = newRepo("case3e");
			writeFixture(repo, "data/ita.txt", "");
			git(repo, ["add", "-N", "data/ita.txt"]);
			expect(() => beginStagedInputCapture(repo, ["data"])).toThrow(/\(unsupported-state\)/);
		},
		TEST_TIMEOUT_MS,
	);

	test(
		"case 4a: actual conflicting merge refuses the unmerged index",
		() => {
			const repo = newRepo("case4a");
			git(repo, ["checkout", "-q", "-b", "side"]);
			writeFixture(repo, "data/a.txt", "side\n");
			git(repo, ["commit", "-qam", "side"]);
			git(repo, ["checkout", "-q", "main"]);
			writeFixture(repo, "data/a.txt", "main\n");
			git(repo, ["commit", "-qam", "main"]);
			git(repo, ["merge", "side"], { allowFailure: true });

			expect(() => beginStagedInputCapture(repo, ["data"])).toThrow(/\(unmerged-input\)/);
			expect(() => beginRelevantInputObservation(repo, ["data"])).toThrow(/\(unmerged-input\)/);
		},
		TEST_TIMEOUT_MS,
	);

	test.skipIf(!SYMLINKS_SUPPORTED)(
		"case 4b: indexed symlink state refuses",
		() => {
			const repo = newRepo("case4b");
			rmSync(join(repo, "data/a.txt"));
			symlinkSync("seed.txt", join(repo, "data/a.txt"));
			git(repo, ["add", "data/a.txt"]);
			expect(() => beginStagedInputCapture(repo, ["data"])).toThrow(/\(unsupported-state\)/);
		},
		TEST_TIMEOUT_MS,
	);

	test.skipIf(!SYMLINKS_SUPPORTED)(
		"case 4c: worktree symlink replacing an indexed regular file refuses",
		() => {
			const repo = newRepo("case4c");
			rmSync(join(repo, "data/a.txt"));
			symlinkSync("seed.txt", join(repo, "data/a.txt"));
			// The worktree typechange is both an unstaged change and an
			// unsupported symlink state; the refusal must name the relevant
			// file, not pin which invalid state production reports first.
			expect(() => beginStagedInputCapture(repo, ["data"])).toThrow(/data\/a\.txt/);
		},
		TEST_TIMEOUT_MS,
	);

	test(
		"case 4d: gitlink (submodule) index entry refuses",
		() => {
			const repo = newRepo("case4d");
			// A real commit OID from a throwaway seed repository: gitlink
			// entries store commit IDs, and the path must not collide with an
			// ordinary file already in the index or worktree.
			const seed = newRepo("case4d-seed");
			const commit = git(seed, ["rev-parse", "HEAD"]).trim();
			git(repo, ["update-index", "--add", "--cacheinfo", `160000,${commit},data/submodule`]);
			expect(() => beginStagedInputCapture(repo, ["data"])).toThrow(/\(unsupported-state\)/);
		},
		TEST_TIMEOUT_MS,
	);

	test(
		"case 4e: skip-worktree and assume-unchanged flags refuse",
		() => {
			const skipRepo = newRepo("case4e-skip");
			git(skipRepo, ["update-index", "--skip-worktree", "data/a.txt"]);
			expect(() => beginStagedInputCapture(skipRepo, ["data"])).toThrow(/\(unsupported-state\)/);
			expect(() => beginRelevantInputObservation(skipRepo, ["data"])).toThrow(/\(unsupported-state\)/);

			const assumeRepo = newRepo("case4e-assume");
			git(assumeRepo, ["update-index", "--assume-unchanged", "data/a.txt"]);
			expect(() => beginStagedInputCapture(assumeRepo, ["data"])).toThrow(/\(unsupported-state\)/);
		},
		TEST_TIMEOUT_MS,
	);

	test(
		"case 5: staged rename and deletion change membership accurately; recreated deletion refuses",
		() => {
			const repo = newRepo("case5");
			git(repo, ["mv", "data/a.txt", "data/moved.txt"]);
			git(repo, ["rm", "-q", "data/sub/b.txt"]);

			const handle = beginStagedInputCapture(repo, ["data"]);
			expect(handle.provenance.entries.map((entry) => entry.path)).toEqual(["data/moved.txt"]);
			expect(handle.provenance.baseHead).toBe(git(repo, ["rev-parse", "HEAD"]).trim());

			writeFixture(repo, "data/sub/b.txt", "recreated untracked\n");
			expect(() => beginStagedInputCapture(repo, ["data"])).toThrow(/\(unstaged-input\)/);
		},
		TEST_TIMEOUT_MS,
	);

	test(
		"case 6a: assertUnchanged refuses unstaged byte mutation after begin",
		() => {
			const repo = newRepo("case6a");
			const handle = beginStagedInputCapture(repo, ["data"]);
			writeFixture(repo, "data/a.txt", "mutated after capture\n");
			expect(() => handle.assertUnchanged()).toThrow(/\(worktree-race\)/);
		},
		TEST_TIMEOUT_MS,
	);

	test(
		"case 6b: assertUnchanged refuses file replacement after begin",
		() => {
			const repo = newRepo("case6b");
			const handle = beginStagedInputCapture(repo, ["data"]);
			rmSync(join(repo, "data/a.txt"));
			writeFixture(repo, "data/a.txt", "replacement bytes\n");
			expect(() => handle.assertUnchanged()).toThrow(/\(worktree-race\)/);
		},
		TEST_TIMEOUT_MS,
	);

	test(
		"case 6c: assertUnchanged refuses restaging different bytes after begin",
		() => {
			const repo = newRepo("case6c");
			const handle = beginStagedInputCapture(repo, ["data"]);
			writeFixture(repo, "data/a.txt", "restaged bytes\n");
			git(repo, ["add", "data/a.txt"]);
			expect(() => handle.assertUnchanged()).toThrow(/\(index-race\)/);
		},
		TEST_TIMEOUT_MS,
	);

	test(
		"case 6d: assertUnchanged refuses relevant entry addition after begin",
		() => {
			const repo = newRepo("case6d");
			const handle = beginStagedInputCapture(repo, ["data"]);
			writeFixture(repo, "data/extra.txt", "extra\n");
			git(repo, ["add", "data/extra.txt"]);
			expect(() => handle.assertUnchanged()).toThrow(/\(index-race\)/);
		},
		TEST_TIMEOUT_MS,
	);

	test(
		"case 6e: assertUnchanged refuses relevant entry deletion after begin",
		() => {
			const repo = newRepo("case6e");
			const handle = beginStagedInputCapture(repo, ["data"]);
			git(repo, ["rm", "-q", "data/a.txt"]);
			expect(() => handle.assertUnchanged()).toThrow(/\(index-race\)/);
		},
		TEST_TIMEOUT_MS,
	);

	test(
		"case 6f: assertUnchanged refuses HEAD movement with identical relevant files",
		() => {
			const repo = newRepo("case6f");
			const handle = beginStagedInputCapture(repo, ["data"]);
			git(repo, ["commit", "-q", "--allow-empty", "-m", "head move"]);
			expect(() => handle.assertUnchanged()).toThrow(/\(head-race\)/);
		},
		TEST_TIMEOUT_MS,
	);

	test(
		"case 6g: assertUnchanged refuses edit-restore cycles and keeps the immutable baseline",
		() => {
			const repo = newRepo("case6g");
			const original = readFileSync(join(repo, "data/a.txt"));
			const handle = beginStagedInputCapture(repo, ["data"]);
			writeFixture(repo, "data/a.txt", "temporary edit\n");
			writeFileSync(join(repo, "data/a.txt"), original);
			expect(() => handle.assertUnchanged()).toThrow(/\(worktree-race\)/);
			// The baseline cannot be reset by repairing the tree afterwards.
			expect(() => handle.assertUnchanged()).toThrow(/\(worktree-race\)/);
		},
		TEST_TIMEOUT_MS,
	);

	test(
		"case 6h: an untouched handle keeps passing",
		() => {
			const repo = newRepo("case6h");
			const handle = beginStagedInputCapture(repo, ["data"]);
			handle.assertUnchanged();
			handle.assertUnchanged();
		},
		TEST_TIMEOUT_MS,
	);

	test(
		"case 7: spaces, TAB, newline and Unicode paths survive NUL parsing; sha1 and sha256 formats work",
		() => {
			const repo = newRepo("case7");
			const names = [
				"data/sp ace.txt",
				"data/tab\tx.txt",
				"data/nl\nx.txt",
				"data/ünïcode-λ-世界.txt",
			];
			for (const name of names) writeFixture(repo, name, `content of ${name}\n`);
			git(repo, ["add", "--", ...names]);

			const handle = beginStagedInputCapture(repo, ["data"]);
			const captured = handle.provenance.entries.map((entry) => entry.path);
			for (const name of names) expect(captured).toContain(name);
			expect([...handle.provenance.entries]).toEqual(indexedEntries(repo, ["data"]));
			handle.assertUnchanged();
			expect(handle.provenance.objectFormat).toBe("sha1");
			expect(handle.provenance.baseHead).toMatch(/^[0-9a-f]{40}$/);
		},
		TEST_TIMEOUT_MS,
	);

	test(
		"case 7b: sha256 repositories produce 64-hex provenance",
		() => {
			const repo = newRepo("case7b", "sha256");
			writeFixture(repo, "data/a.txt", "sha256 change\n");
			git(repo, ["add", "data/a.txt"]);

			const handle = beginStagedInputCapture(repo, ["data"]);
			expect(handle.provenance.objectFormat).toBe("sha256");
			expect(handle.provenance.baseHead).toMatch(/^[0-9a-f]{64}$/);
			expect(handle.provenance.baseHead).toBe(git(repo, ["rev-parse", "HEAD"]).trim());
			for (const entry of handle.provenance.entries) {
				expect(entry.blob).toMatch(/^[0-9a-f]{64}$/);
			}
		},
		TEST_TIMEOUT_MS,
	);

	test(
		"case 8: disjointness refuses relevant, glob, ancestor, symlink-alias and Git-admin destinations",
		() => {
			const repo = newRepo("case8");
			const scopes = ["data", "packages/*/x.json"];
			const disjoint = (outputPaths: readonly string[]) =>
				assertCaptureOutputPathsDisjoint(repo, scopes, outputPaths);

			expect(() => disjoint([join(repo, "data/a.txt")])).toThrow(/\(output-overlap\)/);
			expect(() => disjoint([join(repo, "data/new-projection.json")])).toThrow(/\(output-overlap\)/);
			expect(() => disjoint([join(repo, "packages/a/deep/x.json")])).toThrow(/\(output-overlap\)/);
			expect(() => disjoint([join(repo, "data/sub")])).toThrow(/\(output-overlap\)/);
			expect(() => disjoint([join(repo, "data")])).toThrow(/\(output-overlap\)/);
			expect(() => disjoint([join(repo, ".git/hooks/projection.json")])).toThrow(/\(output-overlap\)/);

			// Glob scope misses and ordinary non-relevant destinations pass.
			expect(() => disjoint([join(repo, "packages/a/y.json")])).not.toThrow();
			expect(() => disjoint([join(repo, "target/proj/projection.json")])).not.toThrow();
			const outside = mkdtempSync(join(TESTS_ROOT, "outside-"));
			OWNED_DIRECTORIES.push(outside);
			expect(() => disjoint([join(outside, "projection.json")])).not.toThrow();
		},
		TEST_TIMEOUT_MS,
	);

	test.skipIf(!SYMLINKS_SUPPORTED)(
		"case 8b: symlinked output aliases into a relevant scope refuse",
		() => {
			const repo = newRepo("case8b");
			symlinkSync(join(repo, "data"), join(repo, "data-alias"));
			expect(() =>
				assertCaptureOutputPathsDisjoint(repo, ["data"], [join(repo, "data-alias/projection.json")]),
			).toThrow(/\(output-overlap\)/);
		},
		TEST_TIMEOUT_MS,
	);

	test.skipIf(!SYMLINKS_SUPPORTED)(
		"case 8c: outside-root outputs resolving through symlinks into scopes or Git admin dirs refuse",
		() => {
			const repo = newRepo("case8c");
			const outside = mkdtempSync(join(TESTS_ROOT, "outside-alias-"));
			OWNED_DIRECTORIES.push(outside);
			const disjoint = (outputPaths: readonly string[]) =>
				assertCaptureOutputPathsDisjoint(repo, ["data"], outputPaths);

			// A symlink outside the root that points into a relevant scope or
			// into the real Git admin directory must be caught by real-path
			// resolution, not skipped because the lexical path escapes.
			symlinkSync(join(repo, "data"), join(outside, "scope-alias"));
			expect(() => disjoint([join(outside, "scope-alias/projection.json")])).toThrow(/\(output-overlap\)/);
			symlinkSync(join(repo, ".git"), join(outside, "git-alias"));
			expect(() => disjoint([join(outside, "git-alias/hooks/projection.json")])).toThrow(/\(output-overlap\)/);

			// An outside-root alias into a non-relevant part of the tree and an
			// ordinary not-yet-created outside destination remain legal.
			symlinkSync(join(repo, "other"), join(outside, "other-alias"));
			expect(() => disjoint([join(outside, "other-alias/projection.json")])).not.toThrow();
			expect(() => disjoint([join(outside, "fresh/dir/projection.json")])).not.toThrow();
		},
		TEST_TIMEOUT_MS,
	);

	test(
		"case 8d: the standalone output guard refuses redirected repository/index environments",
		() => {
			const repo = newRepo("case8d");
			for (const name of REDIRECTED_REPOSITORY_VARS) {
				const previous = process.env[name];
				process.env[name] = join(repo, ".git");
				try {
					expect(() =>
						assertCaptureOutputPathsDisjoint(repo, ["data"], [join(repo, "target/projection.json")]),
					).toThrow(/\(unsupported-state\)/);
				} finally {
					if (previous === undefined) delete process.env[name];
					else process.env[name] = previous;
				}
			}
		},
		TEST_TIMEOUT_MS,
	);

	test.skipIf(!CASE_INSENSITIVE_FS)(
		"case 8e: an output differing only in case from a relevant input refuses on case-insensitive filesystems",
		() => {
			const repo = newRepo("case8e");
			// data/a.txt is indexed; on a case-insensitive filesystem
			// DATA/A.TXT resolves to the same file, and an atomic rename onto it
			// would destroy the input's bytes.
			expect(() =>
				assertCaptureOutputPathsDisjoint(repo, ["data"], [join(repo, "DATA/A.TXT")]),
			).toThrow(/\(output-overlap\)/);
		},
		TEST_TIMEOUT_MS,
	);

	test.skipIf(CASE_INSENSITIVE_FS)(
		"case 8f: a case-variant output names a genuinely different file on case-sensitive filesystems",
		() => {
			const repo = newRepo("case8f");
			// The refusal above must come from real-path resolution, not a
			// lexical blanket: where DATA/A.TXT is truly a different path it is
			// an ordinary destination.
			expect(() =>
				assertCaptureOutputPathsDisjoint(repo, ["data"], [join(repo, "DATA/A.TXT")]),
			).not.toThrow();
		},
		TEST_TIMEOUT_MS,
	);

	test(
		"case 9: produced provenance parses, round-trips, and malformed shapes refuse",
		() => {
			const repo = newRepo("case9");
			writeFixture(repo, "data/a.txt", "case 9\n");
			git(repo, ["add", "data/a.txt"]);
			const handle = beginStagedInputCapture(repo, ["data"]);

			const roundTripped: StagedInputProvenance = parseStagedInputProvenance(
				JSON.parse(JSON.stringify(handle.provenance)),
				"round-trip",
			);
			expect(roundTripped).toEqual(handle.provenance);
			expect(Object.isFrozen(roundTripped)).toBe(true);

			const parsed = parseStagedInputProvenance(validProvenance(), "valid");
			expect(parsed.entries[0]?.path).toBe("data/a.txt");
			const sha256Provenance = validProvenance({
				objectFormat: "sha256",
				baseHead: "a".repeat(64),
				entries: [entryFor("data/a.txt", { blob: "b".repeat(64) })],
			});
			expect(parseStagedInputProvenance(sha256Provenance, "valid-sha256").objectFormat).toBe("sha256");

			const rejects: ReadonlyArray<readonly [string, unknown]> = [
				["null", null],
				["number", 42],
				["array", []],
				["missing key", { schema: STAGED_INPUT_PROVENANCE_SCHEMA, mode: "staged", baseHead: "a".repeat(40), objectFormat: "sha1", pathspecs: ["data"] }],
				["extra key", { ...validProvenance(), unexpected: true }],
				["wrong schema", validProvenance({ schema: "pi.deps.sbom.v1" })],
				["wrong mode", validProvenance({ mode: "committed" })],
				["wrong objectFormat", validProvenance({ objectFormat: "sha3" })],
				["short baseHead", validProvenance({ baseHead: "a".repeat(39) })],
				["long baseHead", validProvenance({ baseHead: "a".repeat(64) })],
				["uppercase baseHead", validProvenance({ baseHead: "A".repeat(40) })],
				["absolute pathspec", validProvenance({ pathspecs: ["/abs"] })],
				["traversal pathspec", validProvenance({ pathspecs: ["data/../escape"] })],
				["empty pathspecs", validProvenance({ pathspecs: [] })],
				["duplicate pathspecs", validProvenance({ pathspecs: ["data", "data"] })],
				["unsorted pathspecs", validProvenance({ pathspecs: ["z", "a"] })],
				["backslash pathspec", validProvenance({ pathspecs: ["data\\x"] })],
				["wildcard pathspec", validProvenance({ pathspecs: ["data/?.txt"] })],
				["empty entries", validProvenance({ entries: [] })],
				["unsorted entries", validProvenance({ entries: [entryFor("data/z.txt"), entryFor("data/a.txt")] })],
				["duplicate entries", validProvenance({ entries: [entryFor("data/a.txt"), entryFor("data/a.txt")] })],
				["entry outside scope", validProvenance({ entries: [entryFor("other/other.txt")] })],
				["entry missing key", validProvenance({ entries: [{ path: "data/a.txt", mode: "100644" }] })],
				[
					"entry extra key",
					validProvenance({ entries: [{ path: "data/a.txt", mode: "100644", blob: "b".repeat(40), extra: 1 }] }),
				],
				["entry bad mode", validProvenance({ entries: [entryFor("data/a.txt", { mode: "120000" })] })],
				["entry uppercase blob", validProvenance({ entries: [entryFor("data/a.txt", { blob: "B".repeat(40) })] })],
				["entry zero blob", validProvenance({ entries: [entryFor("data/a.txt", { blob: "0".repeat(40) })] })],
				["entry traversal path", validProvenance({ entries: [entryFor("../escape.txt")] })],
				["entry NUL path", validProvenance({ entries: [entryFor("data/a\u0000b")] })],
				["entry nonstring path", validProvenance({ entries: [{ path: 7, mode: "100644", blob: "b".repeat(40) }] })],
			];
			for (const [label, value] of rejects) {
				let threw = false;
				try {
					parseStagedInputProvenance(value, "reject-case");
				} catch {
					threw = true;
				}
				if (threw === false) throw new Error(`expected provenance rejection for: ${label}`);
			}
		},
		TEST_TIMEOUT_MS,
	);

	test(
		"case 19: dep-free observation tolerates porcelain M but refuses bytes changing during capture",
		() => {
			const repo = newRepo("case19");
			writeFixture(repo, "data/a.txt", "dep-free local change\n");
			const observation = beginRelevantInputObservation(repo, ["data"]);
			expect("provenance" in observation).toBe(false);
			observation.assertUnchanged();

			writeFixture(repo, "data/a.txt", "changed again during capture\n");
			expect(() => observation.assertUnchanged()).toThrow(/\(worktree-race\)/);
			expect(() => observation.assertUnchanged()).toThrow(/\(worktree-race\)/);
		},
		TEST_TIMEOUT_MS,
	);

	test(
		"case 19b: observation tracks untracked relevant regular files and their replacement",
		() => {
			const repo = newRepo("case19b");
			writeFixture(repo, "data/untracked.txt", "untracked\n");
			const observation = beginRelevantInputObservation(repo, ["data"]);
			observation.assertUnchanged();

			writeFixture(repo, "data/untracked.txt", "replaced\n");
			expect(() => observation.assertUnchanged()).toThrow(/\(worktree-race\)/);
		},
		TEST_TIMEOUT_MS,
	);

	test(
		"case 19c: observation refuses HEAD movement and staged membership changes",
		() => {
			const headRepo = newRepo("case19c-head");
			const headObservation = beginRelevantInputObservation(headRepo, ["data"]);
			git(headRepo, ["commit", "-q", "--allow-empty", "-m", "head move"]);
			expect(() => headObservation.assertUnchanged()).toThrow(/\(head-race\)/);

			const indexRepo = newRepo("case19c-index");
			const indexObservation = beginRelevantInputObservation(indexRepo, ["data"]);
			writeFixture(indexRepo, "data/extra.txt", "extra\n");
			git(indexRepo, ["add", "data/extra.txt"]);
			expect(() => indexObservation.assertUnchanged()).toThrow(/\(index-race\)/);
		},
		TEST_TIMEOUT_MS,
	);

	test(
		"case 19d: observation records absence and refuses reappearance",
		() => {
			const repo = newRepo("case19d");
			rmSync(join(repo, "data/a.txt"));
			const observation = beginRelevantInputObservation(repo, ["data"]);
			observation.assertUnchanged();

			writeFixture(repo, "data/a.txt", "back again\n");
			expect(() => observation.assertUnchanged()).toThrow(/\(worktree-race\)/);
		},
		TEST_TIMEOUT_MS,
	);

	test(
		"case 19e: observation records absence when a relevant input's parent directory is deleted",
		() => {
			const repo = newRepo("case19e");
			rmSync(join(repo, "data"), { recursive: true });

			// Staged mode still refuses the dirty tree; observation records the
			// actual absence the dep-field-free exception is built to tolerate.
			expect(() => beginStagedInputCapture(repo, ["data"])).toThrow(/\(unstaged-input\)/);
			const observation = beginRelevantInputObservation(repo, ["data"]);
			observation.assertUnchanged();

			// Recreating the parent without the file is still absence.
			mkdirSync(join(repo, "data"));
			observation.assertUnchanged();

			// The file reappearing is a real change and must be caught.
			writeFixture(repo, "data/a.txt", "back again\n");
			expect(() => observation.assertUnchanged()).toThrow(/\(worktree-race\)/);
		},
		TEST_TIMEOUT_MS,
	);

	test(
		"scope grammar refuses non-conforming scopes",
		() => {
			const repo = newRepo("grammar");
			const badScopes: ReadonlyArray<readonly string[]> = [
				[],
				[""],
				["/abs/path"],
				["../escape"],
				["data/../escape"],
				["a\\b"],
				["data\u0000x"],
				["[abc].txt"],
				["data/?.txt"],
				[":(literal)data"],
				["!negated"],
				["^negated"],
				["data/"],
				["data//x"],
				["./data"],
			];
			for (const scopes of badScopes) {
				let threw = false;
				try {
					beginStagedInputCapture(repo, scopes);
				} catch {
					threw = true;
				}
				if (threw === false) throw new Error(`expected scope rejection for: ${JSON.stringify(scopes)}`);
			}
		},
		TEST_TIMEOUT_MS,
	);

	test(
		"explicit recursive scope captures nested files",
		() => {
			const repo = newRepo("vendor-glob");
			writeFixture(repo, "vendor/foreign-0.1.0/src/lib.rs", "pub fn x() {}\n");
			git(repo, ["add", "vendor/foreign-0.1.0/src/lib.rs"]);
			const handle = beginStagedInputCapture(repo, ["vendor/*/src/**"]);
			expect(handle.provenance.pathspecs).toEqual(["vendor/*/src/**"]);
			expect(handle.provenance.entries.map((entry) => entry.path)).toEqual([
				"vendor/foreign-0.1.0/src/lib.rs",
			]);
			handle.assertUnchanged();
		},
		TEST_TIMEOUT_MS,
	);

	test(
		"a directory-named glob without explicit recursion captures nothing",
		() => {
			// Git pathspec semantics, on purpose: `vendor/*/src` matches the
			// directory entry itself, not the files inside it, so a staged
			// capture over it has no indexed entries and must fail loudly
			// instead of silently under-capturing.
			const repo = newRepo("vendor-glob-nonrecursive");
			writeFixture(repo, "vendor/foreign-0.1.0/src/lib.rs", "pub fn x() {}\n");
			git(repo, ["add", "vendor/foreign-0.1.0/src/lib.rs"]);
			expect(() => beginStagedInputCapture(repo, ["vendor/*/src"])).toThrow(
				/no relevant indexed files match the declared pathspecs/,
			);
		},
		TEST_TIMEOUT_MS,
	);

	test(
		"repository and index redirection environment variables refuse",
		() => {
			const repo = newRepo("redirection");
			for (const name of REDIRECTED_REPOSITORY_VARS) {
				const previous = process.env[name];
				process.env[name] = join(repo, ".git");
				try {
					expect(() => beginStagedInputCapture(repo, ["data"])).toThrow(/\(unsupported-state\)/);
				} finally {
					if (previous === undefined) delete process.env[name];
					else process.env[name] = previous;
				}
			}
		},
		TEST_TIMEOUT_MS,
	);

	test(
		"nonrepository and unborn roots refuse",
		() => {
			mkdirSync(TESTS_ROOT, { recursive: true });
			const plain = mkdtempSync(join(TESTS_ROOT, "norepo-"));
			OWNED_DIRECTORIES.push(plain);
			// The scratch root lives inside the enclosing repository's
			// worktree, so the refusal surfaces as unsupported-state ("not the
			// worktree top-level"); under a true nonrepository parent it would
			// surface as git-failure. Either way the root must be refused with
			// the offending root named — do not pin which precedence fires.
			expect(() => beginStagedInputCapture(plain, ["data"])).toThrow(plain);

			const unborn = mkdtempSync(join(TESTS_ROOT, "unborn-"));
			OWNED_DIRECTORIES.push(unborn);
			git(unborn, ["init", "-q", "-b", "main"]);
			expect(() => beginStagedInputCapture(unborn, ["data"])).toThrow(/\(git-failure\)/);
		},
		TEST_TIMEOUT_MS,
	);
});
