/**
 * XC-4 handshake-asymmetry mutation witnesses (issue #42).
 *
 * Static witnesses that verify the three-mode handshake asymmetry from
 * docs/extension-compatibility-contract.md section 3:
 *
 * - Mode 1 (host.ts): validates BOTH protocolVersion AND compatibilityVersion.
 * - Mode 2 (lean-runner.ts): validates protocolVersion ONLY, ignores compat.
 * - Mode 3 (server.rs): validates protocolVersion ONLY, ignores compat.
 *
 * Each witness targets one mutation (M1–M3): if the referenced guard is added
 * or removed, the witness reports a violation.
 */

import { readFileSync } from "node:fs";
import { join, resolve } from "node:path";

export const REPO_ROOT = resolve(import.meta.dirname, "../..");

export interface HandshakeWitnessInputs {
	/** Contents of `packages/extension-host/src/host.ts`. */
	hostSource: string;
	/** Contents of `packages/extension-host/src/lean-runner.ts`. */
	leanSource: string;
	/** Contents of `crates/pi-ext/src/server.rs`. */
	serverSource: string;
}

export function loadHandshakeWitnessInputs(root: string): HandshakeWitnessInputs {
	const hostSource = readFileSync(
		join(root, "packages/extension-host/src/host.ts"),
		"utf8",
	);
	const leanSource = readFileSync(
		join(root, "packages/extension-host/src/lean-runner.ts"),
		"utf8",
	);
	const serverSource = readFileSync(
		join(root, "crates/pi-ext/src/server.rs"),
		"utf8",
	);
	return { hostSource, leanSource, serverSource };
}

// ============================================================================
// M3 witness: Mode 1 host MUST check compatibilityVersion
// ============================================================================

/**
 * Mode 1 (`host.ts::handleHelloFrame`) validates BOTH `protocolVersion` and
 * `compatibilityVersion`. If the compatibilityVersion check is dropped, a
 * foreign-compat endpoint would be accepted in Mode 1, violating the
 * handshake asymmetry contract.
 *
 * `witness: host.ts::handleHelloFrame` —
 * `if (typeof remoteCompat !== "string" || remoteCompat !== COMPATIBILITY_VERSION)`
 */
export function verifyHostCompatCheck(source: string): string[] {
	const violations: string[] = [];

	// The host must read compatibilityVersion from the payload.
	const readsCompat = /remoteCompat\s*=\s*payload\[.compatibilityVersion.\]/;
	if (!readsCompat.test(source)) {
		violations.push(
			"host handleHelloFrame must read `compatibilityVersion` from the " +
				"hello payload (Mode 1 validates both versions)",
		);
	}

	// The host must reject on compatibilityVersion mismatch.
	const checksCompat = /remoteCompat\s*!==\s*COMPATIBILITY_VERSION/;
	if (!checksCompat.test(source)) {
		violations.push(
			"host handleHelloFrame must reject when `compatibilityVersion` does " +
				"not match COMPATIBILITY_VERSION (Mode 1 compat-mismatch guard)",
		);
	}

	// The host must terminate on compat mismatch (not just log).
	// Line-based: find the compat check line, then verify `this.terminate`
	// appears before the enclosing block closes (dedent or blank-end).
	const hostLines = source.split("\n");
	const compatLineIdx = hostLines.findIndex((l) =>
		/remoteCompat\s*!==\s*COMPATIBILITY_VERSION/.test(l),
	);
	if (compatLineIdx === -1) {
		// Already reported by checksCompat above.
	} else {
		const compatIndent = hostLines[compatLineIdx].search(/\S/);
		let foundTerminate = false;
		for (let i = compatLineIdx + 1; i < hostLines.length; i++) {
			const indent = hostLines[i].search(/\S/);
			if (indent >= 0 && indent <= compatIndent && hostLines[i].trim() !== "") {
				break;
			}
			if (/this\.terminate/.test(hostLines[i])) {
				foundTerminate = true;
				break;
			}
		}
		if (!foundTerminate) {
			violations.push(
				"host handleHelloFrame must call `this.terminate` on " +
					"compatibilityVersion mismatch, not merely log it",
			);
		}
	}

	return violations;
}

// ============================================================================
// M1 witness: Mode 2 lean MUST NOT check compatibilityVersion
// ============================================================================

/**
 * Mode 2 (`lean-runner.ts::handleHelloFrame`) validates `protocolVersion` ONLY
 * and must NOT require `compatibilityVersion` to match. If a compat check is
 * added, valid foreign-compat endpoints would be rejected, violating the
 * handshake asymmetry contract.
 *
 * `witness: lean-runner.ts::handleHelloFrame` — protocol-only validation,
 * no `compatibilityVersion` comparison.
 */
export function verifyLeanProtocolOnly(source: string): string[] {
	const violations: string[] = [];

	// The lean runner must check protocolVersion.
	const checksProtocol = /remoteProtocol\s*!==\s*PROTOCOL_VERSION/;
	if (!checksProtocol.test(source)) {
		violations.push(
			"lean handleHelloFrame must validate `protocolVersion` against " +
				"PROTOCOL_VERSION",
		);
	}

	// The lean runner must NOT gate on compatibilityVersion. The ack
	// includes compatibilityVersion as a value but must never compare it.
	// Catch any `!== COMPATIBILITY_VERSION` pattern — the lean source has
	// no legitimate comparison to COMPATIBILITY_VERSION (only a value in
	// the ack response).
	const compatGate = /!==\s*COMPATIBILITY_VERSION/;
	if (compatGate.test(source)) {
		violations.push(
			"lean handleHelloFrame must NOT gate on `compatibilityVersion` — " +
				"Mode 2 validates protocolVersion only; adding a compat check " +
				"would reject valid foreign-compat endpoints",
		);
	}

	return violations;
}

// ============================================================================
// M2 witness: Mode 3 native server MUST NOT check compatibility_version
// ============================================================================

/**
 * Mode 3 (`server.rs::validate_hello`) validates `protocol_version` ONLY and
 * must NOT require `compatibility_version` to match. If a compat check is
 * added, valid foreign-compat endpoints would be rejected, violating the
 * handshake asymmetry contract.
 *
 * `witness: server.rs::validate_hello` — protocol-only validation,
 * no `compatibility_version` comparison.
 */
export function verifyServerProtocolOnly(source: string): string[] {
	const violations: string[] = [];

	// The server must check protocol_version.
	const checksProtocol = /hello\.protocol_version\s*!=\s*PROTOCOL_VERSION/;
	if (!checksProtocol.test(source)) {
		violations.push(
			"server validate_hello must validate `hello.protocol_version` " +
				"against PROTOCOL_VERSION",
		);
	}

	// The server must NOT compare compatibility_version against
	// COMPATIBILITY_VERSION inside validate_hello.
	const compatGate = /compatibility_version\s*!=\s*COMPATIBILITY_VERSION/;
	if (compatGate.test(source)) {
		violations.push(
			"server validate_hello must NOT gate on `compatibility_version` " +
				"— Mode 3 validates protocol_version only; adding a compat " +
				"check would reject valid foreign-compat endpoints",
		);
	}

	// The deliberate-ignore comment must be present.
	const ignoreComment = /compatibilityVersion is deliberately ignored/;
	if (!ignoreComment.test(source)) {
		violations.push(
			"server validate_hello must carry the `compatibilityVersion is " +
				"deliberately ignored` comment documenting the asymmetry",
		);
	}

	return violations;
}

// ============================================================================
// Orchestration
// ============================================================================

/** Run every handshake witness; empty means green. */
export function runHandshakeWitnesses(
	inputs: HandshakeWitnessInputs,
): string[] {
	return [
		...verifyHostCompatCheck(inputs.hostSource),
		...verifyLeanProtocolOnly(inputs.leanSource),
		...verifyServerProtocolOnly(inputs.serverSource),
	];
}

if (import.meta.main) {
	const inputs = loadHandshakeWitnessInputs(REPO_ROOT);
	const violations = runHandshakeWitnesses(inputs);
	if (violations.length > 0) {
		process.stderr.write("XC handshake witness violations:\n");
		for (const v of violations) {
			process.stderr.write(`  - ${v}\n`);
		}
		process.exit(1);
	}
	process.stdout.write("XC handshake witnesses: all green\n");
}
