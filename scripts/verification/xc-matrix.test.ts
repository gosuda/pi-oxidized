import { describe, expect, test } from "bun:test";

import {
	loadXcWitnessInputs,
	REPO_ROOT,
	runXcWitnesses,
	verifyCommandSuffixDisambiguation,
	verifyReservedShortcutGuard,
	verifyToolFirstWins,
} from "./xc-matrix.ts";

const INPUTS = loadXcWitnessInputs(REPO_ROOT);

describe("XC-5 registration conflict matrix witnesses", () => {
	test("real repository passes every XC witness", () => {
		expect(runXcWitnesses(INPUTS)).toEqual([]);
	});

	// M4: tool first-wins
	test("M4: tool first-wins guard is present in getAllRegisteredTools", () => {
		expect(verifyToolFirstWins(INPUTS.runnerSource)).toEqual([]);
	});

	test("M4 mutation: removing the has-guard fails the witness", () => {
		const mutated = INPUTS.runnerSource.replace(
			/if\s*\(!toolsByName\.has\(tool\.definition\.name\)\)/,
			"/* removed guard */ if (false)",
		);
		expect(verifyToolFirstWins(mutated)).not.toEqual([]);
	});

	// M5: command suffix disambiguation
	test("M5: command suffix disambiguation is present in resolveRegisteredCommands", () => {
		expect(verifyCommandSuffixDisambiguation(INPUTS.runnerSource)).toEqual([]);
	});

	test("M5 mutation: dropping the suffix template fails the witness", () => {
		const mutated = INPUTS.runnerSource.replace(
			/`\$\{command\.name\}:\$\{occurrence\}`/,
			"command.name",
		);
		expect(verifyCommandSuffixDisambiguation(mutated)).not.toEqual([]);
	});

	test("M5 mutation: removing the occurrence counter fails the witness", () => {
		const mutated = INPUTS.runnerSource.replace(
			/counts\.set\(command\.name[^]*?\+\s*1\);/,
			"/* counter removed */",
		);
		expect(verifyCommandSuffixDisambiguation(mutated)).not.toEqual([]);
	});

	// M6: reserved-shortcut guard
	test("M6: reserved-shortcut guard is present in getShortcuts", () => {
		expect(verifyReservedShortcutGuard(INPUTS.runnerSource)).toEqual([]);
	});

	test("M6 mutation: removing the restrictOverride check fails the witness", () => {
		const mutated = INPUTS.runnerSource.replace(
			/builtInKeybinding\s*\?\.\s*restrictOverride\s*===\s*true/,
			"false /* guard removed */",
		);
		expect(verifyReservedShortcutGuard(mutated)).not.toEqual([]);
	});

	test("M6 mutation: emptying the reserved list fails the witness", () => {
		const mutated = INPUTS.runnerSource.replace(
			/RESERVED_KEYBINDINGS_FOR_EXTENSION_CONFLICTS\s*=\s*\[([^\]]*)\]/,
			"RESERVED_KEYBINDINGS_FOR_EXTENSION_CONFLICTS = []",
		);
		expect(verifyReservedShortcutGuard(mutated)).not.toEqual([]);
	});
	test("M6 mutation: removing the continue after guard fails the witness", () => {
		const lines = INPUTS.runnerSource.split("\n");
		const guardIdx = lines.findIndex((l) => /restrictOverride\s*===\s*true/.test(l));
		const continueIdx = lines.findIndex(
			(l, i) => i > guardIdx && /^\s*continue\b/.test(l),
		);
		const mutated = lines
			.map((l, i) => (i === continueIdx ? l.replace("continue", "/* continue removed */") : l))
			.join("\n");
		expect(verifyReservedShortcutGuard(mutated)).not.toEqual([]);
	});
});
