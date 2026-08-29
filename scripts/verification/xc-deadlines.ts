/**
 * XC-8 deadline, cancellation, error-isolation, and stale-guard witnesses
 * (issue #44).
 *
 * Static witnesses that verify the local extension-host source implements the
 * deadlines, cancellation routing, error isolation, and stale-command-context
 * guards from docs/extension-compatibility-contract.md section 10.  Each
 * witness targets one mutation (M15–M18): if the referenced guard or logic is
 * removed from the source, the witness reports a violation.
 *
 * The witnesses read the **local** `packages/extension-host/src/` sources
 * (the Mode 1 bundled compat host and the Mode 2 lean runner), not the
 * `.references` tree, because the contract doc section 10 witnesses cite
 * those local files.
 */

import { readFileSync } from "node:fs";
import { join, resolve } from "node:path";

export const REPO_ROOT = resolve(import.meta.dirname, "../..");

export interface XcDeadlineInputs {
	/** Contents of `packages/extension-host/src/host.ts`. */
	hostSource: string;
	/** Contents of `packages/extension-host/src/lean-runner.ts`. */
	leanSource: string;
}

export function loadXcDeadlineInputs(root: string): XcDeadlineInputs {
	const hostSource = readFileSync(
		join(root, "packages/extension-host/src/host.ts"),
		"utf8",
	);
	const leanSource = readFileSync(
		join(root, "packages/extension-host/src/lean-runner.ts"),
		"utf8",
	);
	return { hostSource, leanSource };
}

// ============================================================================
// M15 witness: terminal-input 4 ms deadline and 64-capacity queue
// ============================================================================

/**
 * The terminal-input actor enforces a 4 ms per-handler deadline via a
 * `Promise.race` against `setTimeout(…, EXTENSION_INPUT_TIMEOUT_MS)` and a
 * 64-entry queue capacity via `EXTENSION_INPUT_QUEUE_CAPACITY`.  If either
 * constant is removed or the timeout race is dropped, slow handlers would
 * block the input pipeline and the scaling p99/deadline assertions would
 * fail on both sides.
 *
 * `witness: host.ts::EXTENSION_INPUT_TIMEOUT_MS = 4` (line 116),
 * `::EXTENSION_INPUT_QUEUE_CAPACITY = 64` (line 118),
 * `::invokeTerminalHandler` timeout race (line 1448).
 */
export function verifyTerminalInputDeadline(source: string): string[] {
	const violations: string[] = [];

	// EXTENSION_INPUT_TIMEOUT_MS must be exactly 4.
	const timeoutMatch = source.match(
		/EXTENSION_INPUT_TIMEOUT_MS\s*=\s*(\d+)/,
	);
	if (!timeoutMatch) {
		violations.push(
			"EXTENSION_INPUT_TIMEOUT_MS constant is missing from host.ts — " +
				"the terminal-input 4 ms deadline has no source pin",
		);
	} else if (Number(timeoutMatch[1]) !== 4) {
		violations.push(
			`EXTENSION_INPUT_TIMEOUT_MS is ${timeoutMatch[1]}, expected 4 — ` +
				"the terminal-input deadline has been weakened",
		);
	}

	// EXTENSION_INPUT_QUEUE_CAPACITY must be exactly 64.
	const capacityMatch = source.match(
		/EXTENSION_INPUT_QUEUE_CAPACITY\s*=\s*(\d+)/,
	);
	if (!capacityMatch) {
		violations.push(
			"EXTENSION_INPUT_QUEUE_CAPACITY constant is missing from host.ts — " +
				"the terminal-input 64-capacity queue bound has no source pin",
		);
	} else if (Number(capacityMatch[1]) !== 64) {
		violations.push(
			`EXTENSION_INPUT_QUEUE_CAPACITY is ${capacityMatch[1]}, expected 64 — ` +
				"the queue bound has been changed",
		);
	}

	// The timeout race: setTimeout(() => resolve({ kind: "timeout" }),
	// EXTENSION_INPUT_TIMEOUT_MS) must be present inside invokeTerminalHandler.
	const timeoutRacePattern =
		/setTimeout\s*\(\s*\(\s*\)\s*=>\s*resolve\s*\(\s*\{\s*kind:\s*"timeout"\s*\}\s*\)\s*,\s*EXTENSION_INPUT_TIMEOUT_MS\s*\)/;
	if (!timeoutRacePattern.test(source)) {
		violations.push(
			"terminal-input timeout race is missing from invokeTerminalHandler — " +
				"a slow handler would block the input pipeline instead of being " +
				"disabled after 4 ms",
		);
	}

	// The capacity guard: queue length checked against
	// EXTENSION_INPUT_QUEUE_CAPACITY before enqueue.
	const capacityGuardPattern =
		/terminalInputQueue\.length\s*>=\s*EXTENSION_INPUT_QUEUE_CAPACITY/;
	if (!capacityGuardPattern.test(source)) {
		violations.push(
			"terminal-input queue capacity guard is missing — " +
				"the 64-capacity bound is not enforced before enqueue",
		);
	}

	return violations;
}

// ============================================================================
// M16 witness: hook throw emits extensionError instead of crashing host
// ============================================================================

/**
 * A lifecycle hook or shortcut handler that throws must NOT crash the host.
 * Instead, the error is caught and emitted as a per-extension
 * `extensionError` notification (lean `runHooks`) or a correlated
 * non-retryable `extension_error` response (host `handleLifecycleHook`), and
 * the remaining handlers / correlated request continue.  Shortcut failures
 * are caught in a detached `.catch` that emits `extensionError` unless the
 * controller was already aborted or the host disposed.
 *
 * `witness: host.ts::handleLifecycleHook` catch → respondError
 * (lines 1052-1058); `host.ts::handleShortcutExecute` detached catch
 * (lines 902-910); `lean-runner.ts::runHooks` catch → emitExtensionError
 * (lines 1593-1598).
 */
export function verifyErrorIsolation(
	hostSource: string,
	leanSource: string,
): string[] {
	const violations: string[] = [];

	// host.ts handleLifecycleHook: catch block must emit extension_error
	// via respondError (not rethrow).
	const hostHookCatchPattern =
		/catch\s*\(\s*err\s*\)\s*\{[\s\S]*?respondError\s*\(\s*id\s*,\s*eventType[\s\S]*?extension_error[\s\S]*?\}/;
	if (!hostHookCatchPattern.test(hostSource)) {
		violations.push(
			"host.ts handleLifecycleHook error isolation is missing — " +
				"a hook throw must be caught and returned as a correlated " +
				"extension_error response, not rethrown to crash the host",
		);
	}

	// host.ts handleShortcutExecute: detached .catch must emit
	// extensionError (unless aborted/disposed).
	const shortcutCatchPattern =
		/\.catch\s*\(\s*\(\s*error\s*\)\s*=>\s*\{[\s\S]*?emitExtensionError[\s\S]*?shortcut\.execute[\s\S]*?\}/;
	if (!shortcutCatchPattern.test(hostSource)) {
		violations.push(
			"host.ts handleShortcutExecute detached error catch is missing — " +
				"a shortcut handler throw must emit extensionError, not crash " +
				"the host",
		);
	}

	// lean-runner.ts runHooks: catch block must emit extensionError.
	const leanHookCatchPattern =
		/catch\s*\(\s*err\s*\)\s*\{[\s\S]*?emitExtensionError\s*\(/;
	if (!leanHookCatchPattern.test(leanSource)) {
		violations.push(
			"lean-runner.ts runHooks error isolation is missing — " +
				"a hook throw must be caught and emitted as extensionError, " +
				"not rethrown to crash the lean runner",
		);
	}

	return violations;
}

// ============================================================================
// M17 witness: stale replacement token rejected via markStale + guardActive
// ============================================================================

/**
 * After a session replacement (newSession/fork/switchSession/reload), the
 * initiating command context is marked stale via `markStale?.()` inside
 * `captureReplacementToken` **before** any token-shaped early return.  The
 * `guardActive` closure then throws `STALE_COMMAND_CONTEXT_MESSAGE` when a
 * captured context is reused.  If `markStale` is dropped, a stale
 * replacement token would be accepted and the stale-token witness would
 * fail.
 *
 * `witness: host.ts::captureReplacementToken` markStale call (line 2817);
 * `host.ts::guardActive` STALE_COMMAND_CONTEXT_MESSAGE throw (line 2753);
 * `host.ts::createCommandContext` guard closure (lines 2968-2971).
 */
export function verifyStaleGuard(source: string): string[] {
	const violations: string[] = [];

	// STALE_COMMAND_CONTEXT_MESSAGE constant must exist.
	const staleMsgPattern = /STALE_COMMAND_CONTEXT_MESSAGE\s*=/;
	if (!staleMsgPattern.test(source)) {
		violations.push(
			"STALE_COMMAND_CONTEXT_MESSAGE constant is missing from host.ts — " +
				"the stale-command-context guard has no message to throw",
		);
	}

	// captureReplacementToken must call markStale?.() before returning.
	const markStalePattern = /markStale\s*\?\.\s*\(\s*\)/;
	if (!markStalePattern.test(source)) {
		violations.push(
			"markStale?.() call is missing from captureReplacementToken — " +
				"a replacement token would be accepted without marking the " +
				"initiating command context stale",
		);
	}

	// guardActive must throw STALE_COMMAND_CONTEXT_MESSAGE when the runner
	// has been replaced.
	const guardThrowPattern =
		/throw new Error\s*\(\s*STALE_COMMAND_CONTEXT_MESSAGE\s*\)/;
	if (!guardThrowPattern.test(source)) {
		violations.push(
			"guardActive STALE_COMMAND_CONTEXT_MESSAGE throw is missing — " +
				"a stale command context would be accepted after runner " +
				"replacement instead of throwing",
		);
	}

	// createCommandContext guard closure must check the `stale` flag.
	const staleFlagPattern = /if\s*\(\s*stale\s*\|\|\s*this\.runner\s*!==\s*runner\s*\)/;
	if (!staleFlagPattern.test(source)) {
		violations.push(
			"createCommandContext stale-flag guard is missing — " +
				"the per-command `stale` boolean is not checked before " +
				"allowing context access",
		);
	}

	return violations;
}

// ============================================================================
// M18 witness: cancel routing aborts in-flight controllers by request ID
// ============================================================================

/**
 * `tool.cancel` / `provider.cancel` control events must be routed to the
 * in-flight AbortController for the matching request ID in both the host and
 * the lean runner.  The controller's `signal.aborted` flag is then checked
 * in the tool/provider execution loop to stop accepting updates and return
 * a `cancelled` error frame.  If the method check or the abort call is
 * dropped, a cancel event would be silently ignored and the cancel race
// witness would fail.
 *
 * `witness: host.ts::handleControlEvent` method check + abort
 * (lines 2191-2198); `lean-runner.ts::handleControlEvent` method check +
 * abort (lines 1916-1924); `lean-runner.ts::handleToolExecute` signal-aborted
// update gate (line 1408).
 */
export function verifyCancelRouting(
	hostSource: string,
	leanSource: string,
): string[] {
	const violations: string[] = [];

	// Both host and lean must check for tool.cancel AND provider.cancel.
	for (const [label, src] of [["host", hostSource], ["lean", leanSource]] as const) {
		const methodCheckPattern =
			/frame\.method\s*!==\s*"tool\.cancel"\s*&&\s*frame\.method\s*!==\s*"provider\.cancel"/;
		if (!methodCheckPattern.test(src)) {
			violations.push(
				`${label} handleControlEvent tool.cancel/provider.cancel method ` +
					"check is missing — a cancel event would not be routed to " +
					"the in-flight controller",
			);
		}

		// Both must abort in-flight tools and providers by request ID.
		const toolAbortPattern = /inFlightTools\.get\s*\(\s*requestId\s*\)\s*\?\.\s*abort\s*\(\s*\)/;
		if (!toolAbortPattern.test(src)) {
			violations.push(
				`${label} handleControlEvent inFlightTools abort is missing — ` +
					"a tool.cancel event would not abort the in-flight tool " +
					"controller",
			);
		}

		const providerAbortPattern = /inFlightProviders\.get\s*\(\s*requestId\s*\)\s*\?\.\s*abort\s*\(\s*\)/;
		if (!providerAbortPattern.test(src)) {
			violations.push(
				`${label} handleControlEvent inFlightProviders abort is missing ` +
					"— a provider.cancel event would not abort the in-flight " +
					"provider controller",
			);
		}
	}

	// The lean runner must gate onUpdate on controller.signal.aborted so
	// cancelled tools stop accepting updates (the cancel race witness).
	const signalGatePattern = /controller\.signal\.aborted\s*\)/;
	if (!signalGatePattern.test(leanSource)) {
		violations.push(
			"lean-runner handleToolExecute signal-aborted update gate is " +
				"missing — a cancelled tool would continue accepting updates " +
				"instead of stopping, and the cancel race witness would fail",
		);
	}

	// The lean runner must check controller.signal.aborted after execution
	// to return a cancelled error frame.
	const postExecCheckPattern =
		/if\s*\(\s*controller\.signal\.aborted\s*\)\s*\{[\s\S]*?cancelled[\s\S]*?\}/;
	if (!postExecCheckPattern.test(leanSource)) {
		violations.push(
			"lean-runner handleToolExecute post-execution signal-aborted check " +
				"is missing — a cancelled tool would return a success response " +
				"instead of a cancelled error frame",
		);
	}

	return violations;
}

// ============================================================================
// Orchestration
// ============================================================================

/** Run every XC-8 witness; empty means green. */
export function runXcDeadlineWitnesses(inputs: XcDeadlineInputs): string[] {
	return [
		...verifyTerminalInputDeadline(inputs.hostSource),
		...verifyErrorIsolation(inputs.hostSource, inputs.leanSource),
		...verifyStaleGuard(inputs.hostSource),
		...verifyCancelRouting(inputs.hostSource, inputs.leanSource),
	];
}

if (import.meta.main) {
	const inputs = loadXcDeadlineInputs(REPO_ROOT);
	const violations = runXcDeadlineWitnesses(inputs);
	if (violations.length > 0) {
		console.error("XC-8 deadline/cancellation witness violations:");
		for (const v of violations) {
			console.error(`  - ${v}`);
		}
		process.exit(1);
	}
	console.log("XC-8 deadline/cancellation witnesses: all green");
}
