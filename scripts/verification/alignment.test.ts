import { describe, expect, test } from "bun:test";
import { REQUIRED_TOOL_NAMES, selectPortableToolParameters } from "../generate-tool-schemas.ts";
import {
	CANONICAL_REFERENCE_SHA,
	PIN_LITERAL_PATHS,
	REPO_ROOT,
	STALE_REFERENCE_SHA,
	loadAlignmentInputs,
	runAlignmentWitnesses,
	verifyPinLiterals,
	verifyPortableToolSelection,
	verifyReferenceCheckout,
} from "./alignment.ts";

function parametersFor(names: readonly string[]): Record<string, unknown> {
	const definitions: Record<string, unknown> = {};
	for (const name of names) {
		definitions[name] = { parameters: { type: "object", properties: { [name]: { type: "string" } } } };
	}
	return definitions;
}

describe("VER-ALIGN reference pin", () => {
	test("canonical SHA is the settled baseline literal", () => {
		expect(CANONICAL_REFERENCE_SHA).toBe("8fa7eebd235355522c8104166b4f1f959b4e2f10");
		expect(STALE_REFERENCE_SHA).toBe("4488ad55c18f07ae89a489096c90de8667b3adfb");
	});

	test("owned pin files carry the canonical SHA twice in workflow and once in reconstruct", () => {
		const inputs = loadAlignmentInputs(REPO_ROOT);
		expect(verifyPinLiterals(inputs.files)).toEqual([]);
		for (const path of PIN_LITERAL_PATHS) {
			expect(inputs.files[path]).toContain(CANONICAL_REFERENCE_SHA);
			expect(inputs.files[path]).not.toContain(STALE_REFERENCE_SHA);
		}
	});

	test("pin witness fails on a stale workflow literal", () => {
		const files = {
			".github/workflows/release-verification.yml": `ref: ${STALE_REFERENCE_SHA}\nassert ${STALE_REFERENCE_SHA}\n`,
			"scripts/reconstruct-provider-data.ts": `// pinned reference ${CANONICAL_REFERENCE_SHA}\n`,
		};
		expect(verifyPinLiterals(files).some((problem) => problem.includes(STALE_REFERENCE_SHA))).toBe(true);
	});

	test("reference checkout witness accepts only the canonical HEAD", () => {
		expect(verifyReferenceCheckout(CANONICAL_REFERENCE_SHA)).toEqual([]);
		expect(verifyReferenceCheckout(STALE_REFERENCE_SHA)).toEqual([
			`.references/pi HEAD is ${STALE_REFERENCE_SHA}, expected ${CANONICAL_REFERENCE_SHA}`,
		]);
	});
});

describe("VER-ALIGN portable tool selection", () => {
	test("selects the seven portable tools and drops powershell", () => {
		const selected = selectPortableToolParameters(
			parametersFor([...REQUIRED_TOOL_NAMES, "powershell"]),
		);
		expect(Object.keys(selected).sort()).toEqual([...REQUIRED_TOOL_NAMES].sort());
		expect(selected).not.toHaveProperty("powershell");
	});

	test("fails when any required portable tool is absent", () => {
		const incomplete = parametersFor(REQUIRED_TOOL_NAMES.filter((name) => name !== "bash"));
		expect(() => selectPortableToolParameters(incomplete)).toThrow(/lacks required tools: bash/);
	});

	test("fails when a required tool has no parameters schema", () => {
		const definitions = parametersFor(REQUIRED_TOOL_NAMES);
		definitions.edit = { description: "missing parameters" };
		expect(() => selectPortableToolParameters(definitions)).toThrow(
			/reference tool "edit" has no parameters schema/,
		);
	});

	test("portable selection witness tolerates reference-only platform tools", () => {
		const registryTools = parametersFor([...REQUIRED_TOOL_NAMES, "powershell"]);
		expect(verifyPortableToolSelection(registryTools)).toEqual([]);
	});

	test("portable selection witness fails when a required tool is missing", () => {
		const registryTools = parametersFor(REQUIRED_TOOL_NAMES.filter((name) => name !== "ls"));
		expect(
			verifyPortableToolSelection(registryTools).some((problem) => problem.includes("ls")),
		).toBe(true);
	});
});

describe("VER-ALIGN acceptance path", () => {
	test("runAlignmentWitnesses is green against the repository", () => {
		expect(runAlignmentWitnesses(loadAlignmentInputs(REPO_ROOT))).toEqual([]);
	});
});
