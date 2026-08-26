#!/usr/bin/env bun
/**
 * syntect ignore retirement watch (DEPS-R3, issue #126).
 *
 * Offline witness over deny.toml + Cargo.lock that locks the CURRENT epoch's
 * syntect-transitive policy state and fails closed on any drift:
 *
 *   - both `[advisories].ignore` entries are still present with their pinned
 *     advisory IDs (RUSTSEC-2025-0141 for bincode 1.3.3, RUSTSEC-2024-0320
 *     for yaml-rust 0.4.5), and each reason still names its pinned crate;
 *   - `unused-ignored-advisory = "deny"` is still the tripwire, so an ignore
 *     left in place after its advisory stops applying fails the deny check;
 *   - the locked transitive chain still pins syntect 5.3.0 -> bincode 1.3.3
 *     and yaml-rust 0.4.5, so both ignores are still load-bearing.
 *
 * No qualifying released syntect drops either transitive this epoch (live
 * registry + issue #23 policy; upstream's yaml-rust2 migration and the
 * bincode replacement PR are both unreleased), so the witness SHIPS the watch
 * rather than the retirement. When a release does drop one of the transitives
 * (or a bump moves the locked chain), the failure text names the retirement
 * trigger so the controller performs the single atomic chore(deps-r3)
 * retirement commit per issue #126 instead of patching this watch.
 *
 * Fail closed: missing or malformed inputs and any of the above drifts exit
 * non-zero.
 */

import { existsSync, readFileSync } from "node:fs";
import { resolve } from "node:path";

export const REPO_ROOT = resolve(import.meta.dirname, "../..");

/** Advisory -> the pinned transitive crate and version it guards this epoch. */
export const PINNED_IGNORES: readonly { id: string; crate: string; version: string }[] = [
	{ id: "RUSTSEC-2025-0141", crate: "bincode", version: "1.3.3" },
	{ id: "RUSTSEC-2024-0320", crate: "yaml-rust", version: "0.4.5" },
];

/** `unused-ignored-advisory` must remain this value to trip on a stale ignore. */
export const TRIPWIRE = "deny";

/** Locked syntect version this epoch; a bump signals staleness / the qualifying release. */
export const PINNED_SYNTECT = "5.3.0";

/** Epoch of this watch verdict (issue #126 freshness re-scan date). */
export const WATCH_EPOCH = "2026-08-26";

export interface IgnoreEntry {
	readonly id: string;
	readonly reason: string;
}

export interface WatchState {
	readonly tripwire: string;
	readonly ignores: readonly IgnoreEntry[];
	readonly syntect: string;
	readonly syntectDependencies: readonly string[];
	readonly bincode: string;
	readonly yamlRust: string;
}

function asTable(value: unknown): Record<string, unknown> | undefined {
	return typeof value === "object" && value !== null && !Array.isArray(value)
		? (value as Record<string, unknown>)
		: undefined;
}

function asArray(value: unknown): unknown[] | undefined {
	return Array.isArray(value) ? value : undefined;
}

const parseToml = Bun.TOML.parse as (source: string) => unknown;

/** Parse the two policy inputs into a WatchState; throws on any malformed/missing part. */
export function parseWatchState(denyTomlText: string, lockText: string): WatchState {
	const deny = asTable(parseToml(denyTomlText));
	if (deny === undefined) throw new Error("deny.toml does not parse to a TOML table");

	const advisories = asTable(deny.advisories);
	if (advisories === undefined) throw new Error('deny.toml: missing "[advisories]" table');

	const tripwireRaw = advisories["unused-ignored-advisory"];
	if (typeof tripwireRaw !== "string") {
		throw new Error('deny.toml: [advisories] missing string key "unused-ignored-advisory"');
	}

	const ignoreRaw = asArray(advisories.ignore) ?? [];
	const ignores: IgnoreEntry[] = ignoreRaw.map((entry, index) => {
		const table = asTable(entry);
		if (table === undefined || typeof table.id !== "string" || typeof table.reason !== "string") {
			throw new Error(`deny.toml: [advisories].ignore[${index}] must be a table with string "id" and "reason"`);
		}
		return { id: table.id, reason: table.reason };
	});

	const lock = asTable(parseToml(lockText));
	if (lock === undefined) throw new Error("Cargo.lock does not parse to a TOML table");

	const packages = asArray(lock.package);
	if (packages === undefined) throw new Error("Cargo.lock: missing [[package]] entries");

	const packageOf = (name: string): Record<string, unknown> => {
		const matches = packages.flatMap((entry) => {
			const table = asTable(entry);
			return table?.name === name ? [table] : [];
		});
		if (matches.length !== 1) {
			throw new Error(`Cargo.lock: expected exactly one package named "${name}", found ${matches.length}`);
		}
		const [match] = matches;
		if (match === undefined) throw new Error(`Cargo.lock: package "${name}" disappeared during parsing`);
		return match;
	};
	const versionOf = (name: string, packageTable: Record<string, unknown>): string => {
		if (typeof packageTable.version !== "string") {
			throw new Error(`Cargo.lock: package "${name}" has no string version`);
		}
		return packageTable.version;
	};
	const syntect = packageOf("syntect");
	const rawDependencies = asArray(syntect.dependencies);
	if (rawDependencies === undefined) {
		throw new Error('Cargo.lock: package "syntect" has no dependency list');
	}
	const syntectDependencies = rawDependencies.map((dependency, index) => {
		if (typeof dependency !== "string") {
			throw new Error(`Cargo.lock: package "syntect" dependency[${index}] is not a string`);
		}
		return dependency;
	});
	const bincode = packageOf("bincode");
	const yamlRust = packageOf("yaml-rust");

	return {
		tripwire: tripwireRaw,
		ignores,
		syntect: versionOf("syntect", syntect),
		syntectDependencies,
		bincode: versionOf("bincode", bincode),
		yamlRust: versionOf("yaml-rust", yamlRust),
	};
}

/** Pure drift check over a parsed state; empty array means green. */
export function verifyWatchState(state: WatchState): string[] {
	const violations: string[] = [];
	const ignoreByCrate = new Map(state.ignores.map((entry) => [entry.id, entry]));

	for (const pin of PINNED_IGNORES) {
		const entry = ignoreByCrate.get(pin.id);
		if (entry === undefined) {
			violations.push(`syntect ignore ${pin.id} (${pin.crate}) absent from deny.toml [advisories].ignore`);
			continue;
		}
		if (!entry.reason.includes(pin.crate)) {
			violations.push(`syntect ignore ${pin.id} reason no longer names its pinned crate "${pin.crate}"`);
		}
	}
	for (const entry of state.ignores) {
		if (!PINNED_IGNORES.some((pin) => pin.id === entry.id)) {
			violations.push(`unexpected ignored advisory ${entry.id} in deny.toml [advisories].ignore`);
		}
	}

	if (state.tripwire !== TRIPWIRE) {
		violations.push(`unused-ignored-advisory must be "${TRIPWIRE}" to trip on a stale ignore; found "${state.tripwire}"`);
	}

	for (const pin of PINNED_IGNORES) {
		const hasDependency = state.syntectDependencies.some(
			(dependency) => dependency === pin.crate || dependency.startsWith(`${pin.crate} `),
		);
		if (!hasDependency) {
			violations.push(`locked syntect no longer depends on ${pin.crate}; re-evaluate ignore ${pin.id}`);
		}
		const actual = state[pin.crate === "bincode" ? "bincode" : "yamlRust"];
		if (actual !== pin.version) {
			violations.push(
				`locked ${pin.crate} is ${actual}, expected ${pin.version}; transitive moved — re-evaluate ignore ${pin.id}`,
			);
		}
	}
	if (state.syntect !== PINNED_SYNTECT) {
		violations.push(
			`locked syntect is ${state.syntect}, expected ${PINNED_SYNTECT}; if this release drops bincode/yaml-rust retire the ignores per issue #126 (DEPS-R3)`,
		);
	}

	return violations;
}

/** Fail-closed loader: throws when either policy input is missing or unreadable. */
export function loadWatchStateFromRoot(root: string): WatchState {
	const denyTomlPath = resolve(root, "deny.toml");
	const lockPath = resolve(root, "Cargo.lock");
	if (!existsSync(denyTomlPath)) throw new Error(`deny.toml not found at ${denyTomlPath}`);
	if (!existsSync(lockPath)) throw new Error(`Cargo.lock not found at ${lockPath}`);
	return parseWatchState(readFileSync(denyTomlPath, "utf8"), readFileSync(lockPath, "utf8"));
}

/** Current-epoch watch verdict: no qualifying syntect release, so ship the watch. */
export function watchVerdict(): string {
	return [
		`[syntect-ignores watch — DEPS-R3, epoch ${WATCH_EPOCH}]`,
		"Verdict: no qualifying upstream syntect release exists; this commit ships the watch, not the retirement.",
		"Both assume-ignored advisories are still load-bearing against the locked transitive chain",
		"(syntect 5.3.0 -> bincode 1.3.3 / yaml-rust 0.4.5); both deny.toml ignore entries and the",
		"`unused-ignored-advisory = \"deny\"` tripwire intact.",
		"Qualifying-release trigger: on the first released syntect that drops the bincode or",
		"yaml-rust/yaml-rust2 transitive (issue #126 DEPS-R3), retire both ignores in one atomic",
		"chore(deps-r3) commit that upgrades syntect, cites the release, and runs the Class S",
		"seven-target post-audit including both musl artifacts.",
		"SYNTECT_IGNORES_WATCH_OK",
	].join("\n");
}

function main(): void {
	let state: WatchState;
	try {
		state = loadWatchStateFromRoot(REPO_ROOT);
	} catch (error) {
		console.error(`syntect-ignores watch failed to load inputs: ${(error as Error).message}`);
		process.exit(1);
	}

	const violations = verifyWatchState(state);
	if (violations.length > 0) {
		console.error(`syntect-ignores watch FAILED CLOSED with ${violations.length} violation(s):`);
		for (const violation of violations) console.error(`  - ${violation}`);
		process.exit(1);
	}

	process.stdout.write(`${watchVerdict()}\n`);
}

if (import.meta.main) main();