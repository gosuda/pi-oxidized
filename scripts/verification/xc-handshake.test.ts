import { describe, expect, test } from "bun:test";

import {
	loadHandshakeWitnessInputs,
	REPO_ROOT,
	runHandshakeWitnesses,
	verifyHostCompatCheck,
	verifyLeanProtocolOnly,
	verifyServerProtocolOnly,
} from "./xc-handshake.ts";

const INPUTS = loadHandshakeWitnessInputs(REPO_ROOT);

describe("XC-4 handshake asymmetry witnesses", () => {
	test("real repository passes every handshake witness", () => {
		expect(runHandshakeWitnesses(INPUTS)).toEqual([]);
	});

	// M3: Mode 1 host MUST check compatibilityVersion
	test("M3: host compat-mismatch guard is present in handleHelloFrame", () => {
		expect(verifyHostCompatCheck(INPUTS.hostSource)).toEqual([]);
	});

	test("M3 mutation: dropping the compat check fails the witness", () => {
		const mutated = INPUTS.hostSource.replace(
			/if\s*\(\s*typeof\s+remoteCompat\s*!==\s*"string"\s*\|\|\s*remoteCompat\s*!==\s*COMPATIBILITY_VERSION\s*\)/,
			"/* compat check dropped */ if (false)",
		);
		expect(verifyHostCompatCheck(mutated)).not.toEqual([]);
	});

	test("M3 mutation: removing the terminate-on-compat fails the witness", () => {
		// Replace the `this.terminate` inside the compat-mismatch block.
		// The `compatibility version mismatch` string is unique to that call.
		const mutated = INPUTS.hostSource.replace(
			/this\.terminate\(\s*`compatibility version mismatch[^`]*`/,
			"/* terminate dropped */ void 0",
		);
		expect(verifyHostCompatCheck(mutated)).not.toEqual([]);
	});

	// M1: Mode 2 lean MUST NOT check compatibilityVersion
	test("M1: lean protocol-only validation is present (no compat gate)", () => {
		expect(verifyLeanProtocolOnly(INPUTS.leanSource)).toEqual([]);
	});

	test("M1 mutation: adding a compat requirement fails the witness", () => {
		// Inject a compat gate after the protocol check in lean handleHelloFrame.
		const mutated = INPUTS.leanSource.replace(
			/(remoteProtocol\s*!==\s*PROTOCOL_VERSION[\s\S]*?return;\s*\n)/,
			"$1\t\tconst remoteCompat = payload[\"compatibilityVersion\"];\n" +
				"\t\tif (typeof remoteCompat !== \"string\" || remoteCompat !== COMPATIBILITY_VERSION) {\n" +
				"\t\t\tthis.terminate(`compatibility version mismatch`);\n" +
				"\t\t\treturn;\n\t\t}\n",
		);
		expect(verifyLeanProtocolOnly(mutated)).not.toEqual([]);
	});

	// M2: Mode 3 native server MUST NOT check compatibility_version
	test("M2: server protocol-only validation is present (no compat gate)", () => {
		expect(verifyServerProtocolOnly(INPUTS.serverSource)).toEqual([]);
	});

	test("M2 mutation: adding a compat requirement fails the witness", () => {
		// Inject a compat gate after the protocol check in validate_hello.
		const mutated = INPUTS.serverSource.replace(
			/(hello\.protocol_version\s*!=\s*PROTOCOL_VERSION[\s\S]*?\}\s*\n)/,
			"$1    if hello.compatibility_version != COMPATIBILITY_VERSION {\n" +
				"        return Err(ServerError::Handshake(format!(\n" +
				"            \"compatibility version mismatch: remote={} local={COMPATIBILITY_VERSION}\",\n" +
				"            hello.compatibility_version\n" +
				"        )));\n    }\n",
		);
		expect(verifyServerProtocolOnly(mutated)).not.toEqual([]);
	});

	test("M2 mutation: removing the deliberate-ignore comment fails the witness", () => {
		const mutated = INPUTS.serverSource.replace(
			/\/\/ Protocol-only: compatibilityVersion is deliberately ignored\./,
			"// comment removed",
		);
		expect(verifyServerProtocolOnly(mutated)).not.toEqual([]);
	});
});
