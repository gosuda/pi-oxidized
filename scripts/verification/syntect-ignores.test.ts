import { describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
	PINNED_SYNTECT,
	REPO_ROOT,
	TRIPWIRE,
	loadWatchStateFromRoot,
	parseWatchState,
	verifyWatchState,
	watchVerdict,
} from "./syntect-ignores.ts";

const denyTomlText = readFileSync(join(REPO_ROOT, "deny.toml"), "utf8");
const lockText = readFileSync(join(REPO_ROOT, "Cargo.lock"), "utf8");

function ignoreLine(id: string): string {
	const line = denyTomlText.split("\n").find((candidate) => candidate.includes(`id = "${id}"`));
	expect(line, `deny.toml fixture should contain ignore for ${id}`).toBeDefined();
	return line as string;
}

function removeIgnoreLine(text: string, id: string): string {
	return text.replace(ignoreLine(id), "");
}

function replaceIgnoreId(text: string, oldId: string, newId: string): string {
	return text.replace(`id = "${oldId}"`, `id = "${newId}"`);
}

function setTripwire(text: string, value: string): string {
	return text.replace(`unused-ignored-advisory = "deny"`, `unused-ignored-advisory = "${value}"`);
}

function setLockVersion(text: string, crate: string, version: string): string {
	const block = new RegExp(`(?<=name = "${crate}"\\nversion = ")[^"]+`);
	const replaced = text.replace(block, version);
	expect(replaced).not.toBe(text);
	return replaced;
}

function removeSyntectDependency(text: string, dependency: string): string {
	const syntectBlock = /(\[\[package\]\]\nname = "syntect"\n[\s\S]*?\ndependencies = \[\n)([\s\S]*?)(\n\]\n)/;
	const match = text.match(syntectBlock);
	if (match === null) throw new Error("Cargo.lock fixture has no syntect dependency block");
	const dependencies = match[2];
	if (dependencies === undefined) throw new Error("Cargo.lock fixture has no syntect dependency list");
	const replacedDependencies = dependencies.replace(` "${dependency}",\n`, "");
	expect(replacedDependencies).not.toBe(dependencies);
	return text.replace(syntectBlock, `$1${replacedDependencies}$3`);
}

function violate(deny: string, lock: string): string[] {
	return verifyWatchState(parseWatchState(deny, lock));
}

describe("syntect-ignores watch (DEPS-R3)", () => {
	test("real tree passes: both ignores load-bearing against the locked chain", () => {
		const state = loadWatchStateFromRoot(REPO_ROOT);
		expect(verifyWatchState(state)).toEqual([]);
	});

	test("locked chain still pins syntect 5.3.0 -> bincode 1.3.3 / yaml-rust 0.4.5", () => {
		const state = parseWatchState(denyTomlText, lockText);
		expect(state.syntect).toBe(PINNED_SYNTECT);
		expect(state.bincode).toBe("1.3.3");
		expect(state.yamlRust).toBe("0.4.5");
		expect(state.syntectDependencies).toContain("bincode");
		expect(state.syntectDependencies).toContain("yaml-rust");
	});

	test("verdict names the qualifying-release trigger, not a retirement", () => {
		const verdict = watchVerdict();
		expect(verdict).toContain("ships the watch");
		expect(verdict).toContain("Qualifying-release trigger");
		expect(verdict).toContain("no qualifying upstream syntect release exists");
	});

	describe("failures on drift", () => {
		test("removing one ignore entry fails", () => {
			const violations = violate(removeIgnoreLine(denyTomlText, "RUSTSEC-2024-0320"), lockText);
			expect(violations.length).toBeGreaterThan(0);
			expect(violations.join("\n")).toContain("RUSTSEC-2024-0320");
		});

		test("changing an advisory id fails", () => {
			const violations = violate(replaceIgnoreId(denyTomlText, "RUSTSEC-2025-0141", "RUSTSEC-9999-9999"), lockText);
			expect(violations.join("\n")).toContain("RUSTSEC-2025-0141");
			expect(violations).not.toEqual([]);
		});

		test("relaxing the unused-ignored-advisory tripwire fails", () => {
			const violations = violate(setTripwire(denyTomlText, "warn"), lockText);
			expect(violations.join("\n")).toContain(TRIPWIRE);
			expect(violations).not.toEqual([]);
		});

		test("bumping locked bincode fails", () => {
			const violations = violate(denyTomlText, setLockVersion(lockText, "bincode", "1.4.0"));
			expect(violations.join("\n")).toContain("bincode");
		});

		test("bumping locked yaml-rust fails", () => {
			const violations = violate(denyTomlText, setLockVersion(lockText, "yaml-rust", "0.4.6"));
			expect(violations.join("\n")).toContain("yaml-rust");
		});

		test("bumping locked syntect fails and names the retirement trigger", () => {
			const violations = violate(denyTomlText, setLockVersion(lockText, "syntect", "5.4.0"));
			const joined = violations.join("\n");
			expect(joined).toContain("syntect");
			expect(joined).toContain("retire the ignores per issue #126");
		});

		test("dropping a transitive from syntect fails while the package remains locked", () => {
			const withoutDependency = removeSyntectDependency(lockText, "bincode");
			const violations = violate(denyTomlText, withoutDependency);
			expect(violations.join("\n")).toContain("syntect no longer depends on bincode");
		});
	});

	describe("fail closed on malformed / missing inputs", () => {
		test("malformed deny.toml throws", () => {
			expect(() => parseWatchState("not [ a valid toml", lockText)).toThrow();
		});

		test("malformed Cargo.lock throws", () => {
			expect(() => parseWatchState(denyTomlText, "<<< not toml")).toThrow();
		});

		test("deny.toml missing [advisories] table throws", () => {
			expect(() => parseWatchState("[graph]\ntargets = []\n", lockText)).toThrow();
		});

		test("locking a package missing from Cargo.lock throws", () => {
			const truncated = lockText.replace(/name = "bincode"\nversion = "1\.3\.3"\n/g, "");
			expect(() => parseWatchState(denyTomlText, truncated)).toThrow();
		});

		test("missing policy files on disk throw", () => {
			const directory = mkdtempSync(join(tmpdir(), "syntect-ignores-"));
			try {
				expect(() => loadWatchStateFromRoot(directory)).toThrow();
			} finally {
				rmSync(directory, { recursive: true, force: true });
			}
		});
	});
});