import { describe, expect, test } from "bun:test";

import {
	ALL_DISCRIMINANTS,
	DISCRIMINANT_LATTICE,
	loadXcDispatchInputs,
	REPO_ROOT,
	runXcDispatchWitnesses,
	verifyBeforeProviderHeadersInPlace,
	verifyInputHandledShortCircuit,
	verifyLatticeCompleteness,
	verifyMutableHookCoverage,
	verifyToolCallInPlaceComparison,
	verifyToolCallTerminateForwarding,
} from "./xc-dispatch.ts";

const INPUTS = loadXcDispatchInputs(REPO_ROOT);

describe("XC-6 hook-dispatch semantics lattice witnesses", () => {
	test("real repository passes every XC-6 witness", () => {
		expect(runXcDispatchWitnesses(INPUTS)).toEqual([]);
	});

	// --- Lattice completeness ---

	test("lattice classifies exactly 33 discriminants", () => {
		expect(ALL_DISCRIMINANTS).toHaveLength(33);
	});

	test("lattice discriminants match ALL_EVENT_TYPES in host.ts", () => {
		expect(verifyLatticeCompleteness(INPUTS.hostSource)).toEqual([]);
	});

	test("every discriminant has at least one class", () => {
		for (const entry of DISCRIMINANT_LATTICE) {
			expect(entry.classes.length).toBeGreaterThan(0);
		}
	});

	test("lattice mutation: removing a discriminant fails the witness", () => {
		const mutated = INPUTS.hostSource.replace(
			/"input",\n/,
			"",
		);
		expect(verifyLatticeCompleteness(mutated)).not.toEqual([]);
	});

	// --- M7: tool_call in-place input mutation comparison ---

	test("M7: tool_call in-place comparison is present", () => {
		expect(verifyToolCallInPlaceComparison(INPUTS.hostSource, INPUTS.leanSource)).toEqual([]);
	});

	test("M7 mutation: dropping canonicalJsonEqual from host fails the witness", () => {
		const mutated = INPUTS.hostSource.replace(
			/canonicalJsonEqual\s*\(\s*input\s*,\s*baseline\s*\)/,
			"true",
		);
		expect(verifyToolCallInPlaceComparison(mutated, INPUTS.leanSource)).not.toEqual([]);
	});

	test("M7 mutation: dropping jsonValuesEqual from lean fails the witness", () => {
		const mutated = INPUTS.leanSource.replace(
			/jsonValuesEqual\s*\(\s*input\s*,\s*baseline\s*\)/,
			"true",
		);
		expect(verifyToolCallInPlaceComparison(INPUTS.hostSource, mutated)).not.toEqual([]);
	});

	test("M7 mutation: dropping structuredClone baseline from host fails the witness", () => {
		const mutated = INPUTS.hostSource.replace(
			/structuredClone\s*\(\s*input\s*\)/,
			"{}",
		);
		expect(verifyToolCallInPlaceComparison(mutated, INPUTS.leanSource)).not.toEqual([]);
	});

	test("M7 mutation: dropping cloneJsonValue baseline from lean fails the witness", () => {
		const mutated = INPUTS.leanSource.replace(
			/cloneJsonValue\s*\(\s*"tool_call\.input"\s*,\s*input\s*\)/,
			"{}",
		);
		expect(verifyToolCallInPlaceComparison(INPUTS.hostSource, mutated)).not.toEqual([]);
	});

	// --- M8: input 'handled' short-circuit ---

	test("M8: input handled short-circuit is present", () => {
		expect(verifyInputHandledShortCircuit(INPUTS.hostSource, INPUTS.leanSource)).toEqual([]);
	});

	test("M8 mutation: dropping 'return false' from lean input handled fails the witness", () => {
		const mutated = INPUTS.leanSource.replace(
			/(r\["action"\]\s*===\s*"handled"\s*\)\s*\{\s*handled\s*=\s*true\s*;?\s*)return\s+false/,
			"$1return true",
		);
		expect(verifyInputHandledShortCircuit(INPUTS.hostSource, mutated)).not.toEqual([]);
	});

	test("M8 mutation: dropping emitInput from host fails the witness", () => {
		const mutated = INPUTS.hostSource.replace(
			/runner\.emitInput\s*\(/,
			"runner.emit(",
		);
		expect(verifyInputHandledShortCircuit(mutated, INPUTS.leanSource)).not.toEqual([]);
	});

	// --- M9: before_provider_headers null-deletes-header ---

	test("M9: before_provider_headers in-place handling is present", () => {
		expect(verifyBeforeProviderHeadersInPlace(INPUTS.hostSource, INPUTS.leanSource)).toEqual([]);
	});

	test("M9 mutation: removing before_provider_headers case from host fails the witness", () => {
		const mutated = INPUTS.hostSource.replace(
			/case "before_provider_headers"/,
			'case "before_provider_headers_removed"',
		);
		expect(verifyBeforeProviderHeadersInPlace(mutated, INPUTS.leanSource)).not.toEqual([]);
	});

	test("M9 mutation: removing before_provider_headers case from lean fails the witness", () => {
		const mutated = INPUTS.leanSource.replace(
			/case "before_provider_headers"/,
			'case "before_provider_headers_removed"',
		);
		expect(verifyBeforeProviderHeadersInPlace(INPUTS.hostSource, mutated)).not.toEqual([]);
	});

	// --- M10: tool_call terminate forwarding ---

	test("M10: tool_call terminate forwarding is present", () => {
		expect(verifyToolCallTerminateForwarding(INPUTS.refsSource, INPUTS.hostSource, INPUTS.leanSource)).toEqual([]);
	});

	test("M10 mutation: removing terminate from ToolCallEventResult fails the witness", () => {
		const mutated = INPUTS.refsSource.replace(
			/(interface ToolCallEventResult[\s\S]*?)terminate\s*\?\s*:\s*boolean/,
			"$1/* removed */",
		);
		expect(verifyToolCallTerminateForwarding(mutated, INPUTS.hostSource, INPUTS.leanSource)).not.toEqual([]);
	});

	test("M10 mutation: removing terminate from ToolResultEventResult fails the witness", () => {
		const mutated = INPUTS.refsSource.replace(
			/(interface ToolResultEventResult[\s\S]*?)terminate\s*\?\s*:\s*boolean/,
			"$1/* removed */",
		);
		expect(verifyToolCallTerminateForwarding(mutated, INPUTS.hostSource, INPUTS.leanSource)).not.toEqual([]);
	});

	test("M10 mutation: dropping result spread from host tool_call fails the witness", () => {
		const mutated = INPUTS.hostSource.replace(
			/\.\.\.\(isRecord\s*\(\s*result\s*\)\s*\?\s*result\s*:\s*\{\s*\}\)/,
			"{}",
		);
		expect(verifyToolCallTerminateForwarding(INPUTS.refsSource, mutated, INPUTS.leanSource)).not.toEqual([]);
	});

	test("M10 mutation: dropping result spread from lean tool_call fails the witness", () => {
		const mutated = INPUTS.leanSource.replace(
			/\.\.\.\(isRecord\s*\(\s*result\s*\)\s*\?\s*result\s*:\s*\{\s*\}\)/,
			"{}",
		);
		expect(verifyToolCallTerminateForwarding(INPUTS.refsSource, INPUTS.hostSource, mutated)).not.toEqual([]);
	});

	// --- Mutable-hook class coverage ---

	test("every mutable-hook class has ≥1 witness in test suites", () => {
		expect(verifyMutableHookCoverage(INPUTS.hostTestSource, INPUTS.leanTestSource, INPUTS.endpointTestSource)).toEqual([]);
	});
});
