/**
 * XC extension-compatibility mutation witnesses (XC-5, issue #38).
 *
 * Static witnesses that verify the TypeScript reference code implements the
 * registration conflict matrix rules from docs/extension-compatibility-contract.md
 * section 6. Each witness targets one mutation: if the referenced guard or
 * logic is removed from the reference source, the witness reports a violation.
 */

import { readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { assertCanonicalReference, canonicalReferenceRoot } from "../reference-identity.ts";

export const REPO_ROOT = resolve(import.meta.dirname, "../..");

export interface XcWitnessInputs {
	/** Contents of `.references/pi-2.0/packages/coding-agent/src/core/extensions/runner.ts`. */
	runnerSource: string;
}

export function loadXcWitnessInputs(root: string): XcWitnessInputs {
	assertCanonicalReference(root);
	const runnerSource = readFileSync(
		join(canonicalReferenceRoot(root), "packages/coding-agent/src/core/extensions/runner.ts"),
		"utf8",
	);
	return { runnerSource };
}

// ============================================================================
// M6 witness: reserved-shortcut guard present in getShortcuts
// ============================================================================

/**
 * The `restrictOverride === true` skip guard in `getShortcuts` prevents
 * extensions from overriding reserved built-in shortcuts. If this guard is
 * removed, a reserved shortcut can be replaced by an extension, violating
 * Rule 4 of the registration conflict matrix.
 *
 * `witness: runner.ts::getShortcuts` line 511 —
 * `if (builtInKeybinding?.restrictOverride === true) { ... continue; }`
 */
export function verifyReservedShortcutGuard(source: string): string[] {
	const violations: string[] = [];

	// The guard checks restrictOverride === true and skips (continue).
	const guardPattern =
		/builtInKeybinding\s*\?\.\s*restrictOverride\s*===\s*true/;
	if (!guardPattern.test(source)) {
		violations.push(
			"reserved-shortcut guard missing: getShortcuts must check " +
				"`builtInKeybinding?.restrictOverride === true` and skip " +
				"(continue) to prevent extension override of reserved built-ins",
		);
	}

	// The RESERVED_KEYBINDINGS_FOR_EXTENSION_CONFLICTS list must exist and be
	// non-empty — it defines which built-in actions are reserved.
	const reservedListMatch = source.match(
		/RESERVED_KEYBINDINGS_FOR_EXTENSION_CONFLICTS\s*=\s*\[([^\]]*)\]/,
	);
	if (!reservedListMatch) {
		violations.push(
			"RESERVED_KEYBINDINGS_FOR_EXTENSION_CONFLICTS list is missing " +
				"from runner.ts — the reserved-shortcut guard has no source of " +
				"reserved action names",
		);
	} else {
		const entries = reservedListMatch[1] ?? ""
			.split(",")
			.map((s) => s.trim().replace(/["'`]/g, ""))
			.filter((s) => s.length > 0);
		if (entries.length === 0) {
			violations.push(
				"RESERVED_KEYBINDINGS_FOR_EXTENSION_CONFLICTS list is empty " +
					"— no built-in shortcuts are reserved, so the guard is inert",
			);
		}
	}

	// The `continue` statement must follow the guard check within the same
	// block. We use a line-based scan: find the line with
	// `restrictOverride === true`, then look for `continue` before the
	// matching closing brace (dedented to the same or lesser depth).
	const lines = source.split("\n");
	const guardLineIdx = lines.findIndex((line) =>
		/restrictOverride\s*===\s*true/.test(line),
	);
	if (guardLineIdx === -1) {
		// Already reported by guardPattern above.
	} else {
		const guardIndent = (lines[guardLineIdx] ?? "").search(/\S/);
		let foundContinue = false;
		for (let i = guardLineIdx + 1; i < lines.length; i++) {
			const indent = (lines[i] ?? "").search(/\S/);
			if (indent >= 0 && indent <= guardIndent && (lines[i] ?? "").trim() !== "") {
				// Reached a line at the same or lesser indent — block ended.
				break;
			}
			if (/^\s*continue\b/.test(lines[i] ?? "")) {
				foundContinue = true;
				break;
			}
		}
		if (!foundContinue) {
			violations.push(
				"reserved-shortcut guard does not skip (continue) after " +
					"detecting restrictOverride === true — the shortcut would " +
					"not be blocked",
			);
		}
	}

	return violations;
}

// ============================================================================
// M5 witness: command suffix disambiguation logic present
// ============================================================================

/**
 * The `resolveRegisteredCommands` method assigns unique invocation names
 * (`name:1`, `name:2`, …) to duplicate command names. If this logic is
 * dropped, duplicates would collide and the Rust first-wins dedup would
 * silently keep only the first.
 *
 * `witness: runner.ts::resolveRegisteredCommands` lines 603-636.
 */
export function verifyCommandSuffixDisambiguation(source: string): string[] {
	const violations: string[] = [];

	// The invocation name must use a `:` suffix pattern for duplicates.
	const suffixPattern =
		/invocationName\s*=\s*.*`\$\{command\.name\}:\$\{occurrence\}`/;
	if (!suffixPattern.test(source)) {
		violations.push(
			"command suffix disambiguation missing: resolveRegisteredCommands " +
				"must assign `${command.name}:${occurrence}` as the invocation " +
				"name for duplicate commands",
		);
	}

	// The occurrence counter must track per-name counts.
	const countPattern = /counts\.set\(command\.name/;
	if (!countPattern.test(source)) {
		violations.push(
			"command occurrence counter missing: resolveRegisteredCommands " +
				"must count per-name occurrences to decide whether to suffix",
		);
	}

	return violations;
}

// ============================================================================
// M4 witness: tool first-wins logic present in reference
// ============================================================================

/**
 * The `getAllRegisteredTools` method uses a Map with `if (!toolsByName.has(...))
 * to implement first-registration-wins. If this guard is removed, later
 * registrations would overwrite earlier ones.
 *
 * `witness: runner.ts::getAllRegisteredTools` lines 451-461.
 */
export function verifyToolFirstWins(source: string): string[] {
	const violations: string[] = [];

	const firstWinsPattern =
		/getAllRegisteredTools[\s\S]*?if\s*\(!toolsByName\.has\(tool\.definition\.name\)/;
	if (!firstWinsPattern.test(source)) {
		violations.push(
			"tool first-wins guard missing: getAllRegisteredTools must check " +
				"`!toolsByName.has(tool.definition.name)` before inserting to " +
				"preserve first registration",
		);
	}

	return violations;
}

// ============================================================================
// Orchestration
// ============================================================================

/** Run every XC witness; empty means green. */
export function runXcWitnesses(inputs: XcWitnessInputs): string[] {
	return [
		...verifyReservedShortcutGuard(inputs.runnerSource),
		...verifyCommandSuffixDisambiguation(inputs.runnerSource),
		...verifyToolFirstWins(inputs.runnerSource),
	];
}

if (import.meta.main) {
	const inputs = loadXcWitnessInputs(REPO_ROOT);
	const violations = runXcWitnesses(inputs);
	if (violations.length > 0) {
		console.error("XC matrix witness violations:");
		for (const v of violations) {
			console.error(`  - ${v}`);
		}
		process.exit(1);
	}
	console.log("XC matrix witnesses: all green");
}
