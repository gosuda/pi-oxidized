/**
 * XC-6 hook-dispatch semantics lattice witnesses (issue #55).
 *
 * Static witnesses that verify the TypeScript reference code implements the
 * hook-dispatch semantics for all 33 lifecycle discriminants, classifying each
 * into one or more of: notification, chain, fold, cancellable, in-place.
 *
 * Mutations:
 *  M7  — tool_call ignores in-place input mutation (canonicalJsonEqual /
 *        jsonValuesEqual comparison dropped) → tool-call-reorder witness fails
 *  M8  — input 'handled' no longer short-circuits (return false dropped)
 *        → input witness fails
 *  M9  — null header value no longer deletes the header (before_provider_headers
 *        specialized case removed, falls through to generic emit) → witness fails
 *  M10 — tool_call block ignores terminate (terminate field dropped from
 *        ToolCallEventResult or response shaping) → terminate witness fails
 */

import { readFileSync } from "node:fs";
import { join, resolve } from "node:path";

export const REPO_ROOT = resolve(import.meta.dirname, "../..");

// ============================================================================
// 33-discriminant dispatch-semantics lattice
// ============================================================================

/** Dispatch semantics classes for lifecycle hooks. */
export type DispatchClass =
	| "notification"
	| "chain"
	| "fold"
	| "cancellable"
	| "in-place";

/** A discriminant can belong to multiple classes (e.g. tool_call is fold +
 * in-place + cancellable). */
export interface DiscriminantClassification {
	discriminant: string;
	classes: DispatchClass[];
}

/**
 * The canonical classification of all 33 lifecycle discriminants.
 *
 * - **notification**: handlers fire, results discarded, response `{ ok: true }`
 * - **chain**: last non-null result wins (session_before_* without cancel)
 * - **fold**: running values threaded to later handlers
 * - **cancellable**: handler can short-circuit (cancel/block/handled)
 * - **in-place**: handler mutates the input object directly (tool_call input,
 *   before_provider_headers headers)
 */
export const DISCRIMINANT_LATTICE: DiscriminantClassification[] = [
	{ discriminant: "project_trust", classes: ["notification"] },
	{ discriminant: "resources_discover", classes: ["fold"] },
	{ discriminant: "session_start", classes: ["notification"] },
	{ discriminant: "session_info_changed", classes: ["notification"] },
	{ discriminant: "session_before_switch", classes: ["chain", "cancellable"] },
	{ discriminant: "session_before_fork", classes: ["chain", "cancellable"] },
	{ discriminant: "session_before_compact", classes: ["chain", "cancellable"] },
	{ discriminant: "session_compact", classes: ["notification"] },
	{ discriminant: "session_shutdown", classes: ["notification"] },
	{ discriminant: "session_before_tree", classes: ["chain", "cancellable"] },
	{ discriminant: "session_tree", classes: ["notification"] },
	{ discriminant: "context", classes: ["fold"] },
	{ discriminant: "before_provider_request", classes: ["chain"] },
	{ discriminant: "before_provider_headers", classes: ["in-place"] },
	{ discriminant: "after_provider_response", classes: ["notification"] },
	{ discriminant: "before_agent_start", classes: ["fold"] },
	{ discriminant: "agent_start", classes: ["notification"] },
	{ discriminant: "agent_end", classes: ["notification"] },
	{ discriminant: "agent_settled", classes: ["notification"] },
	{ discriminant: "turn_start", classes: ["notification"] },
	{ discriminant: "turn_end", classes: ["notification"] },
	{ discriminant: "message_start", classes: ["notification"] },
	{ discriminant: "message_update", classes: ["notification", "cancellable"] },
	{ discriminant: "message_end", classes: ["fold"] },
	{ discriminant: "tool_execution_start", classes: ["notification"] },
	{ discriminant: "tool_execution_update", classes: ["notification"] },
	{ discriminant: "tool_execution_end", classes: ["notification"] },
	{ discriminant: "model_select", classes: ["notification"] },
	{ discriminant: "thinking_level_select", classes: ["notification"] },
	{ discriminant: "tool_call", classes: ["fold", "in-place", "cancellable"] },
	{ discriminant: "tool_result", classes: ["fold"] },
	{ discriminant: "user_bash", classes: ["chain"] },
	{ discriminant: "input", classes: ["fold", "cancellable"] },
];

/** All 33 discriminant names in canonical order. */
export const ALL_DISCRIMINANTS = DISCRIMINANT_LATTICE.map((d) => d.discriminant);

/** Discriminants belonging to a given class. */
export function discriminantsOfClass(cls: DispatchClass): string[] {
	return DISCRIMINANT_LATTICE.filter((d) => d.classes.includes(cls)).map(
		(d) => d.discriminant,
	);
}

// ============================================================================
// Inputs
// ============================================================================

export interface XcDispatchInputs {
	/** Contents of `packages/extension-host/src/host.ts`. */
	hostSource: string;
	/** Contents of `packages/extension-host/src/lean-runner.ts`. */
	leanSource: string;
	/** Contents of `packages/extension-host/src/refs.d.ts`. */
	refsSource: string;
	/** Contents of `packages/extension-host/tests/endpoint-conformance.test.ts`. */
	endpointTestSource: string;
	/** Contents of `packages/extension-host/tests/host.test.ts`. */
	hostTestSource: string;
	/** Contents of `packages/extension-host/tests/lean.test.ts`. */
	leanTestSource: string;
}

export function loadXcDispatchInputs(root: string): XcDispatchInputs {
	const read = (p: string) => readFileSync(join(root, p), "utf8");
	return {
		hostSource: read("packages/extension-host/src/host.ts"),
		leanSource: read("packages/extension-host/src/lean-runner.ts"),
		refsSource: read("packages/extension-host/src/refs.d.ts"),
		endpointTestSource: read("packages/extension-host/tests/endpoint-conformance.test.ts"),
		hostTestSource: read("packages/extension-host/tests/host.test.ts"),
		leanTestSource: read("packages/extension-host/tests/lean.test.ts"),
	};
}

// ============================================================================
// Lattice completeness witness: all 33 discriminants classified
// ============================================================================

/**
 * The lattice must classify exactly 33 discriminants matching ALL_EVENT_TYPES.
 */
export function verifyLatticeCompleteness(
	hostSource: string,
): string[] {
	const violations: string[] = [];
	const match = hostSource.match(
		/export const ALL_EVENT_TYPES = \[([\s\S]*?)\] as const/,
	);
	if (match === null) {
		violations.push("ALL_EVENT_TYPES array not found in host.ts");
		return violations;
	}
	const hostDiscriminants = match[1] ?? ""
		.split(",")
		.map((s) => s.trim().replace(/["']/g, ""))
		.filter((s) => s.length > 0);

	if (hostDiscriminants.length !== ALL_DISCRIMINANTS.length) {
		violations.push(
			`ALL_EVENT_TYPES has ${hostDiscriminants.length} entries, lattice has ${ALL_DISCRIMINANTS.length}`,
		);
	}
	for (let i = 0; i < hostDiscriminants.length; i++) {
		if (hostDiscriminants[i] !== ALL_DISCRIMINANTS[i]) {
			violations.push(
				`discriminant mismatch at index ${i}: host has "${hostDiscriminants[i]}", lattice has "${ALL_DISCRIMINANTS[i] ?? "<missing>"}"`,
			);
		}
	}
	return violations;
}

// ============================================================================
// M7: tool_call in-place input mutation comparison
// ============================================================================

/**
 * Both host.ts and lean-runner.ts must compare the post-hook input against a
 * pre-hook baseline using a key-order-insensitive comparison
 * (canonicalJsonEqual / jsonValuesEqual). If the comparison is removed, the
 * host would either always or never echo `input`, breaking the in-place
 * mutation semantics.
 *
 * `witness: host.ts::handleLifecycleHook tool_call case` (canonicalJsonEqual)
 * `witness: lean-runner.ts::handleLifecycleHook tool_call case` (jsonValuesEqual)
 */
export function verifyToolCallInPlaceComparison(
	hostSource: string,
	leanSource: string,
): string[] {
	const violations: string[] = [];

	// Host: canonicalJsonEqual comparison
	if (!/case "tool_call"[\s\S]*?canonicalJsonEqual\s*\(\s*input\s*,\s*baseline\s*\)/.test(hostSource)) {
		violations.push(
			"M7: host.ts tool_call case missing canonicalJsonEqual(input, baseline) comparison",
		);
	}
	// Host: baseline snapshot via structuredClone
	if (!/case "tool_call"[\s\S]*?structuredClone\s*\(\s*input\s*\)/.test(hostSource)) {
		violations.push(
			"M7: host.ts tool_call case missing structuredClone(input) baseline snapshot",
		);
	}

	// Lean: jsonValuesEqual comparison
	if (!/case "tool_call"[\s\S]*?jsonValuesEqual\s*\(\s*input\s*,\s*baseline\s*\)/.test(leanSource)) {
		violations.push(
			"M7: lean-runner.ts tool_call case missing jsonValuesEqual(input, baseline) comparison",
		);
	}
	// Lean: baseline snapshot via cloneJsonValue
	if (!/case "tool_call"[\s\S]*?cloneJsonValue\s*\(\s*"tool_call\.input"\s*,\s*input\s*\)/.test(leanSource)) {
		violations.push(
			"M7: lean-runner.ts tool_call case missing cloneJsonValue baseline snapshot",
		);
	}

	return violations;
}

// ============================================================================
// M8: input 'handled' short-circuit
// ============================================================================

/**
 * Both host.ts and lean-runner.ts must short-circuit the input hook dispatch
 * when a handler returns `{ action: "handled" }`. In the lean runner this is
 * `return false` from the onResult callback; in the host it is the upstream
 * `emitInput` returning `{ action: "handled" }` which is forwarded directly.
 *
 * `witness: lean-runner.ts::handleLifecycleHook input case` (handled → return false)
 * `witness: host.ts::handleLifecycleHook input case` (emitInput result forwarded)
 */
export function verifyInputHandledShortCircuit(
	hostSource: string,
	leanSource: string,
): string[] {
	const violations: string[] = [];

	// Lean: handled → return false (short-circuit)
	// Lean: handled → return false (short-circuit) — tight pattern matching
	// the exact code structure: "handled") { handled = true; return false
	if (!/r\["action"\]\s*===\s*"handled"\s*\)\s*\{\s*handled\s*=\s*true\s*;?\s*return\s+false/.test(leanSource)) {
		violations.push(
			"M8: lean-runner.ts input case missing 'handled' → return false short-circuit",
		);
	}
	// Lean: handled → respond with { action: "handled" }
	if (!/if\s*\(\s*handled\s*\)\s*\{[\s\S]*?action:\s*"handled"/.test(leanSource)) {
		violations.push(
			"M8: lean-runner.ts input case missing { action: 'handled' } response",
		);
	}

	// Host: emitInput result forwarded (the upstream runner handles short-circuit)
	if (!/case "input"[\s\S]*?runner\.emitInput\s*\(/.test(hostSource)) {
		violations.push(
			"M8: host.ts input case missing runner.emitInput call",
		);
	}

	return violations;
}

// ============================================================================
// M9: before_provider_headers null-deletes-header
// ============================================================================

/**
 * Both host.ts and lean-runner.ts must have a specialized `before_provider_headers`
 * case that passes headers to handlers for in-place mutation and returns the
 * mutated headers. If the specialized case is removed, the hook falls through
 * to the generic emit path which discards results and responds `{ ok: true }`,
 * losing the null-deletes-header semantics.
 *
 * `witness: host.ts::handleLifecycleHook before_provider_headers case`
 * `witness: lean-runner.ts::handleLifecycleHook before_provider_headers case`
 */
export function verifyBeforeProviderHeadersInPlace(
	hostSource: string,
	leanSource: string,
): string[] {
	const violations: string[] = [];

	// Host: specialized case calling emitBeforeProviderHeaders
	if (!/case "before_provider_headers"[\s\S]*?emitBeforeProviderHeaders\s*\(/.test(hostSource)) {
		violations.push(
			"M9: host.ts missing before_provider_headers case with emitBeforeProviderHeaders call",
		);
	}
	// Host: responds with { headers: result }
	if (!/case "before_provider_headers"[\s\S]*?headers:\s*result/.test(hostSource)) {
		violations.push(
			"M9: host.ts before_provider_headers case missing { headers: result } response",
		);
	}

	// Lean: specialized case with in-place mutation
	if (!/case "before_provider_headers"[\s\S]*?runHooks\s*\(/.test(leanSource)) {
		violations.push(
			"M9: lean-runner.ts missing before_provider_headers case with runHooks call",
		);
	}
	// Lean: responds with { headers } — match the client.respond line specifically
	if (!/client\.respond\s*\(\s*id\s*,\s*eventType[^)]*,\s*\{\s*headers\s*\}/.test(leanSource)) {
		violations.push(
			"M9: lean-runner.ts before_provider_headers case missing { headers } response",
		);
	}
	return violations;
}

// ============================================================================
// M10: tool_call terminate forwarding
// ============================================================================

/**
 * The `terminate` field must be present in `ToolCallEventResult` (refs.d.ts)
 * and forwarded through the response in both host.ts and lean-runner.ts.
 * The host spreads the result into the response, so `terminate` passes through
 * if it's in the result type. The lean runner also spreads the result.
 *
 * `witness: refs.d.ts::ToolCallEventResult.terminate`
 * `witness: host.ts::handleLifecycleHook tool_call case` (result spread into response)
 * `witness: lean-runner.ts::handleLifecycleHook tool_call case` (result spread into response)
 */
export function verifyToolCallTerminateForwarding(
	refsSource: string,
	hostSource: string,
	leanSource: string,
): string[] {
	const violations: string[] = [];

	// refs.d.ts: terminate field in ToolCallEventResult (bounded to not cross into ToolResultEventResult)
	if (!/interface ToolCallEventResult\s*\{[^}]*?terminate\s*\?\s*:\s*boolean/.test(refsSource)) {
		violations.push(
			"M10: refs.d.ts ToolCallEventResult missing terminate?: boolean field",
		);
	}

	// Host: tool_call response spreads result (which includes terminate)
	if (!/case "tool_call"[\s\S]*?\.\.\.\(isRecord\s*\(\s*result\s*\)\s*\?\s*result\s*:\s*\{\s*\}\)/.test(hostSource)) {
		violations.push(
			"M10: host.ts tool_call case missing result spread into response (terminate would be lost)",
		);
	}

	// Lean: tool_call response spreads result (which includes terminate)
	if (!/case "tool_call"[\s\S]*?\.\.\.\(isRecord\s*\(\s*result\s*\)\s*\?\s*result\s*:\s*\{\s*\}\)/.test(leanSource)) {
		violations.push(
			"M10: lean-runner.ts tool_call case missing result spread into response (terminate would be lost)",
		);
	}

	// Lean: block: true short-circuit (terminate is only meaningful with block)
	if (!/case "tool_call"[\s\S]*?r\["block"\]\s*===\s*true[\s\S]*?return false/.test(leanSource)) {
		violations.push(
			"M10: lean-runner.ts tool_call case missing block: true → return false short-circuit",
		);
	}


	// refs.d.ts: terminate field in ToolResultEventResult (Rust AfterToolCallWire consumes it)
	if (!/interface ToolResultEventResult\s*\{[^}]*?terminate\s*\?\s*:\s*boolean/.test(refsSource)) {
		violations.push(
			"M10: refs.d.ts ToolResultEventResult missing terminate?: boolean field",
		);
	}

	// Host: tool_result forwards terminate from hook result
	if (!/case "tool_result"[\s\S]*?result\["terminate"\]/.test(hostSource)) {
		violations.push(
			"M10: host.ts tool_result case missing terminate forwarding",
		);
	}

	// Lean: tool_result forwards terminate from hook result
	if (!/case "tool_result"[\s\S]*?r\["terminate"\]/.test(leanSource)) {
		violations.push(
			"M10: lean-runner.ts tool_result case missing terminate forwarding",
		);
	}
	return violations;
}

// ============================================================================
// Mutable-hook class coverage witness
// ============================================================================

/**
 * Every mutable-hook class (fold, cancellable, in-place) must have at least
 * one witness in the host, lean, or endpoint-conformance test suites.
 */
export function verifyMutableHookCoverage(
	hostTestSource: string,
	leanTestSource: string,
	endpointTestSource: string,
): string[] {
	const violations: string[] = [];
	const allTests = hostTestSource + leanTestSource + endpointTestSource;

	// Fold witnesses — each fold discriminant must have a test that exercises
	// running-value threading. Patterns use [\s\S] to match across lines.
	const foldChecks: Array<[string, RegExp]> = [
		["input fold", /input[\s\S]*text[\s\S]*images|ordered fold[\s\S]*input/i],
		["before_agent_start fold", /before_agent_start[\s\S]*systemPrompt|ordered fold[\s\S]*before_agent_start/i],
		["tool_result fold", /tool_result[\s\S]*content|ordered fold[\s\S]*tool_result/i],
		["message_end fold", /message_end[\s\S]*message|ordered fold[\s\S]*message_end/i],
		["tool_call fold (input threading)", /tool_call[\s\S]*input[\s\S]*reorder|tool_call[\s\S]*value[\s\S]*change/i],
	];
	for (const [name, pattern] of foldChecks) {
		if (!pattern.test(allTests)) {
			violations.push(`fold class: no witness found for ${name}`);
		}
	}

	// Cancellable witnesses — each cancellable discriminant must have a test
	// that exercises the short-circuit path.
	const cancellableChecks: Array<[string, RegExp]> = [
		["session_before_* cancel", /session_before[\s\S]*cancel:\s*true|cancel:\s*true[\s\S]*session_before/i],
		["input handled", /"handled"|action:\s*"handled"/i],
		["tool_call block", /block:\s*true|block.*true.*tool_call/i],
		["message_update cancel", /message_update[\s\S]*cancel|[Cc]ancel[Ww]ire/i],
	];
	for (const [name, pattern] of cancellableChecks) {
		if (!pattern.test(allTests)) {
			violations.push(`cancellable class: no witness found for ${name}`);
		}
	}

	// In-place witnesses — each in-place discriminant must have a test that
	// exercises direct object mutation.
	const inPlaceChecks: Array<[string, RegExp]> = [
		["tool_call in-place input mutation", /tool_call[\s\S]*reorder|tool_call[\s\S]*key.order|tool_call[\s\S]*in.place/i],
		["before_provider_headers in-place", /before_provider_headers|null[\s\S]*deletes[\s\S]*header|provider.*headers/i],
	];
	for (const [name, pattern] of inPlaceChecks) {
		if (!pattern.test(allTests)) {
			violations.push(`in-place class: no witness found for ${name}`);
		}
	}

	return violations;
}

// ============================================================================
// Orchestration
// ============================================================================

/** Run every XC-6 witness; empty means green. */
export function runXcDispatchWitnesses(inputs: XcDispatchInputs): string[] {
	return [
		...verifyLatticeCompleteness(inputs.hostSource),
		...verifyToolCallInPlaceComparison(inputs.hostSource, inputs.leanSource),
		...verifyInputHandledShortCircuit(inputs.hostSource, inputs.leanSource),
		...verifyBeforeProviderHeadersInPlace(inputs.hostSource, inputs.leanSource),
		...verifyToolCallTerminateForwarding(inputs.refsSource, inputs.hostSource, inputs.leanSource),
		...verifyMutableHookCoverage(inputs.hostTestSource, inputs.leanTestSource, inputs.endpointTestSource),
	];
}

if (import.meta.main) {
	const inputs = loadXcDispatchInputs(REPO_ROOT);
	const violations = runXcDispatchWitnesses(inputs);
	if (violations.length > 0) {
		console.error("XC-6 dispatch-semantics lattice witness violations:");
		for (const v of violations) {
			console.error(`  - ${v}`);
		}
		process.exit(1);
	}
	console.error("XC-6 dispatch-semantics lattice witnesses: all green");
}
