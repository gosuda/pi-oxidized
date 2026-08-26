import { afterAll, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import {
	FIXED_WORKSPACE_MEMBERS,
	PINNED_AGENT_LOOP_CONFIG_SITES,
	REPO_ROOT,
	enumerateAgentLoopConfigSites,
	expectedCapabilityIds,
	parseInternalDependencyNames,
	parseWorkspaceMembers,
	runParityWitnesses,
	verifyAgentLoopConfigSites,
	verifyCapabilityLedger,
	verifyCrateBoundaries,
	verifyGraduatedTicketDag,
	verifyWorkspaceTopology,
} from "./parity.ts";

const LEDGER_TEXT = readFileSync(join(REPO_ROOT, "docs/PARITY_LEDGER.md"), "utf8");

const temporaryPaths: string[] = [];

afterAll(() => {
	for (const path of temporaryPaths) rmSync(path, { recursive: true, force: true });
});

function temporaryDirectory(prefix: string): string {
	const path = mkdtempSync(join(tmpdir(), prefix));
	temporaryPaths.push(path);
	return path;
}

// A fixture repository that mirrors every pinned boundary: the fixed
// workspace and edge set, the five pinned literal stubs, and the real ledger.
// Mutations below prove each witness fails on one specific drift.

const CRATE_DEPENDENCIES: Readonly<Record<string, readonly string[]>> = {
	pi: ["pi-agent", "pi-ai", "pi-ext", "pi-tui"],
	"pi-agent": ["pi-ai"],
	"pi-ai": [],
	"pi-ext": ["pi-agent", "pi-ai", "pi-tui"],
	"pi-tui": [],
};

interface FixtureOptions {
	readonly extraWorkspaceMember?: string;
	readonly piAgentExtraDependency?: string;
	readonly piAgentProbeSource?: string;
	readonly piExtProbeSource?: string;
	readonly agentLiteralLine?: number;
	readonly extraLiteralPath?: string;
	readonly ledgerText?: string;
}

function literalStub(padLines: number): string {
	const filler = Array.from({ length: padLines }, () => "// fixture filler").join("\n");
	const prefix = filler === "" ? "" : `${filler}\n`;
	return `${prefix}    AgentLoopConfig {\n        model: fixture_model(),\n    }\n`;
}

function writeFixtureRepo(directory: string, options: FixtureOptions = {}): string {
	const members = FIXED_WORKSPACE_MEMBERS.map((crate) => `crates/${crate}`);
	if (options.extraWorkspaceMember !== undefined) members.push(options.extraWorkspaceMember);
	writeFileSync(
		join(directory, "Cargo.toml"),
		`[workspace]\nmembers = [\n${members.map((member) => `\t"${member}",`).join("\n")}\n]\n`,
	);
	for (const crate of FIXED_WORKSPACE_MEMBERS) {
		const dependencies = [...(CRATE_DEPENDENCIES[crate] ?? [])];
		if (crate === "pi-agent" && options.piAgentExtraDependency !== undefined) {
			dependencies.push(options.piAgentExtraDependency);
		}
		const manifest = ["[package]", `name = "${crate}"`, "", "[dependencies]"];
		for (const dependency of dependencies) manifest.push(`${dependency} = { path = "../${dependency}" }`);
		const manifestPath = join(directory, "crates", crate, "Cargo.toml");
		mkdirSync(dirname(manifestPath), { recursive: true });
		writeFileSync(manifestPath, `${manifest.join("\n")}\n`);
		const libPath = join(directory, "crates", crate, "src", "lib.rs");
		mkdirSync(dirname(libPath), { recursive: true });
		writeFileSync(libPath, "// fixture crate root\n");
	}
	for (const pin of PINNED_AGENT_LOOP_CONFIG_SITES) {
		const pad =
			pin.path.endsWith("agent.rs") && options.agentLiteralLine !== undefined
				? options.agentLiteralLine - 1
				: pin.start - 1;
		const stubPath = join(directory, pin.path);
		mkdirSync(dirname(stubPath), { recursive: true });
		writeFileSync(stubPath, literalStub(pad));
	}
	if (options.piAgentProbeSource !== undefined) {
		writeFileSync(join(directory, "crates/pi-agent/src/fixture_probe.rs"), options.piAgentProbeSource);
	}
	if (options.piExtProbeSource !== undefined) {
		writeFileSync(join(directory, "crates/pi-ext/src/host_probe.rs"), options.piExtProbeSource);
	}
	if (options.extraLiteralPath !== undefined) {
		const extraPath = join(directory, options.extraLiteralPath);
		mkdirSync(dirname(extraPath), { recursive: true });
		writeFileSync(extraPath, literalStub(0));
	}
	const ledgerPath = join(directory, "docs/PARITY_LEDGER.md");
	mkdirSync(dirname(ledgerPath), { recursive: true });
	writeFileSync(ledgerPath, options.ledgerText ?? LEDGER_TEXT);
	return directory;
}

function hasViolation(violations: readonly string[], fragment: string): boolean {
	return violations.some((violation) => violation.includes(fragment));
}

describe("parity witness suite", () => {
	test("real repository passes every witness", () => {
		expect(runParityWitnesses(REPO_ROOT)).toEqual([]);
	});

	test("fixture replica of the pinned contract passes every witness", () => {
		const directory = writeFixtureRepo(temporaryDirectory("parity-fixture-"));
		expect(runParityWitnesses(directory)).toEqual([]);
	});

	test("expected capability IDs cover exactly the 57 family members", () => {
		const ids = expectedCapabilityIds();
		expect(ids.length).toBe(57);
		expect(new Set(ids).size).toBe(57);
		for (const id of ["A1", "A11", "G11", "T9", "E4", "C18", "R4"]) {
			expect(ids.includes(id)).toBe(true);
		}
	});

	test("manifest parsers extract members and internal dependencies only", () => {
		expect(parseWorkspaceMembers('[workspace]\nmembers = [\n  "crates/pi",\n  "crates/pi-ai",\n]\n')).toEqual([
			"pi",
			"pi-ai",
		]);
		const manifest = [
			"[dependencies]",
			'pi-ai = { path = "../pi-ai" }',
			'futures = { version = "1" }',
			"[dev-dependencies]",
			'tokio = "1"',
			"[[bin]]",
			'name = "fixture_bin"',
			'path = "src/bin/fixture.rs"',
			"[target.'cfg(unix)'.dependencies]",
			'nix = "1"',
		].join("\n");
		expect(parseInternalDependencyNames(manifest, FIXED_WORKSPACE_MEMBERS)).toEqual(["pi-ai"]);
	});

	test("topology witness reports missing members and edges", () => {
		const violations = verifyWorkspaceTopology({ members: ["pi"], edges: [], problems: [] });
		expect(hasViolation(violations, "missing workspace member pi-agent")).toBe(true);
		expect(hasViolation(violations, "missing internal dependency edge pi-agent -> pi-ai")).toBe(true);
	});

	test("extra workspace member fails the topology witness", () => {
		const directory = writeFixtureRepo(temporaryDirectory("parity-extra-member-"), {
			extraWorkspaceMember: "crates/pi-extra",
		});
		expect(hasViolation(runParityWitnesses(directory), "unexpected workspace member pi-extra")).toBe(true);
	});

	test("extra internal edge fails the topology witness", () => {
		const directory = writeFixtureRepo(temporaryDirectory("parity-extra-edge-"), {
			piAgentExtraDependency: "pi-tui",
		});
		expect(hasViolation(runParityWitnesses(directory), "unexpected internal dependency edge pi-agent -> pi-tui")).toBe(
			true,
		);
	});

	test("pi_tui reference in pi-agent fails the crate boundary witness", () => {
		const directory = writeFixtureRepo(temporaryDirectory("parity-pi-tui-"), {
			piAgentProbeSource: "use pi_tui::layout::SizeValue;\n",
		});
		expect(hasViolation(runParityWitnesses(directory), "references forbidden crate pi_tui")).toBe(true);
	});

	test("pi reference in pi-ext fails the crate boundary witness", () => {
		const directory = writeFixtureRepo(temporaryDirectory("parity-pi-ref-"), {
			piExtProbeSource: "use pi::cli::build;\n",
		});
		expect(hasViolation(runParityWitnesses(directory), "references forbidden crate pi::")).toBe(true);
	});

	test("comments and literals do not create forbidden crate imports", () => {
		expect(
			verifyCrateBoundaries([
				{
					crate: "pi-agent",
					path: "crates/pi-agent/src/probe.rs",
					content:
						'// use pi_tui::layout;\n/*\nuse pi_tui::layout;\n*/\nconst NOTE: &str = "pi_ext::server";\nconst RAW: &str = r#"\nuse pi_ext::server;\n"#;\n',
				},
			]),
		).toEqual([]);
	});

	test("boundary witness reports the referencing file and line", () => {
		const violations = verifyCrateBoundaries([
			{ crate: "pi-agent", path: "crates/pi-agent/src/probe.rs", content: "fn f() {}\nuse pi_ext::x;\n" },
		]);
		expect(violations).toEqual(["crates/pi-agent/src/probe.rs:2 references forbidden crate pi_ext"]);
	});

	test("duplicate capability ID fails the ledger witness", () => {
		const duplicated = LEDGER_TEXT.replace("\n| A2 |", "\n| A1 | Duplicate row | pi | m | s | landed | dup |\n| A2 |");
		const violations = verifyCapabilityLedger(duplicated);
		expect(hasViolation(violations, "duplicate capability ID A1")).toBe(true);
		expect(hasViolation(violations, "capability ledger has 58 rows, expected 57")).toBe(true);
	});

	test("missing capability ID fails the ledger witness", () => {
		const withoutC18 = LEDGER_TEXT.split("\n")
			.filter((line) => !line.startsWith("| C18 |"))
			.join("\n");
		const violations = verifyCapabilityLedger(withoutC18);
		expect(hasViolation(violations, "missing capability ID C18")).toBe(true);
		expect(hasViolation(violations, "capability ledger has 56 rows, expected 57")).toBe(true);
	});

	test("A8 status drift fails the ledger witness", () => {
		const drifted = LEDGER_TEXT.replace("compatibility audit | parity-blocked", "compatibility audit | landed");
		expect(hasViolation(verifyCapabilityLedger(drifted), 'capability A8 status must be "parity-blocked"')).toBe(true);
	});

	test("A8 evidence checklist truncation fails the ledger witness", () => {
		const truncated = LEDGER_TEXT.replace("executable negative witnesses", "negative witnesses");
		expect(hasViolation(verifyCapabilityLedger(truncated), "capability A8 evidence checklist is missing")).toBe(true);
	});

	test("A9 surface rewrite fails the ledger witness", () => {
		const rewritten = LEDGER_TEXT.replaceAll("OAuth-CLI", "OAuth CLI");
		expect(hasViolation(verifyCapabilityLedger(rewritten), 'capability A9 is missing pinned contract token "OAuth-CLI"')).toBe(
			true,
		);
	});

	test("E1 status drift fails the ledger witness", () => {
		const drifted = LEDGER_TEXT.replace("| extension-plan-owned | The mirror", "| landed | The mirror");
		expect(hasViolation(verifyCapabilityLedger(drifted), "capability E1 status must be")).toBe(true);
	});

	test("R3 platform contract drift fails the ledger witness", () => {
		const drifted = LEDGER_TEXT.replace("EndpointSpecError::UnsupportedOnPlatform", "a typed error");
		expect(hasViolation(verifyCapabilityLedger(drifted), "capability R3 is missing pinned contract token")).toBe(true);
	});

	test("unknown blocker fails the graduated DAG witness", () => {
		const drifted = LEDGER_TEXT.replace("| PAR-TEL | task | PAR-LEDGER |", "| PAR-TEL | task | PAR-NOPE |");
		expect(hasViolation(verifyGraduatedTicketDag(drifted), 'blocked_by "PAR-NOPE" has no row (missing node)')).toBe(
			true,
		);
	});

	test("cycle fails the graduated DAG witness", () => {
		const cyclic = LEDGER_TEXT.replace("| PAR-LEDGER | task | — |", "| PAR-LEDGER | task | PAR-CLOSE |");
		expect(hasViolation(verifyGraduatedTicketDag(cyclic), "dependency cycle involving")).toBe(true);
	});

	test("lowercase ticket ID fails the graduated DAG witness", () => {
		const drifted = LEDGER_TEXT.replace("| PAR-TEL | task | PAR-LEDGER |", "| par-tel | task | PAR-LEDGER |");
		expect(hasViolation(verifyGraduatedTicketDag(drifted), 'malformed stable ID "par-tel"')).toBe(true);
	});

	test("duplicate ticket stable ID fails the graduated DAG witness", () => {
		const drifted = LEDGER_TEXT.replace("| PAR-WIRE | research | PAR-LEDGER |", "| PAR-TEL | research | PAR-LEDGER |");
		expect(hasViolation(verifyGraduatedTicketDag(drifted), "duplicate ticket stable ID PAR-TEL")).toBe(true);
	});

	test("deleted graduated ticket fails the DAG witness", () => {
		const withoutClose = LEDGER_TEXT.split("\n")
			.filter((line) => !line.startsWith("| PAR-CLOSE |"))
			.join("\n");
		expect(hasViolation(verifyGraduatedTicketDag(withoutClose), "missing graduated ticket PAR-CLOSE")).toBe(true);
	});

	test("blocker edge rewrite fails the DAG witness", () => {
		const drifted = LEDGER_TEXT.replace(
			"| PAR-CLOSE | task | PAR-FOLD, PAR-CLIENT, PAR-SERVER, PAR-COMPAT-AUDIT, PAR-COMPAT-DISPO, PAR-PTY-GRILL, XC-2 |",
			"| PAR-CLOSE | task | PAR-FOLD, PAR-CLIENT, PAR-SERVER, XC-2 |",
		);
		expect(hasViolation(verifyGraduatedTicketDag(drifted), "ticket PAR-CLOSE blocked_by mismatch")).toBe(true);
	});

	test("aliased workspace dependency is resolved by its package rename", () => {
		const manifest = '[dependencies]\ntui_alias = { package = "pi-tui", path = "../pi-tui" }\n';
		expect(parseInternalDependencyNames(manifest, FIXED_WORKSPACE_MEMBERS)).toEqual(["pi-tui"]);
	});

	test("expanded Cargo dependency subtable resolves its package rename", () => {
		const manifest = '[dependencies.ui_alias]\npackage = "pi-tui"\npath = "../pi-tui"\n';
		expect(parseInternalDependencyNames(manifest, FIXED_WORKSPACE_MEMBERS)).toEqual(["pi-tui"]);
	});

	test("single-quoted Cargo package aliases resolve in both forms", () => {
		const inline = "[dependencies]\ntui_alias = { package = 'pi-tui', path = '../pi-tui' }\n";
		const expanded = "[dependencies.ui_alias]\npackage = 'pi-tui'\npath = '../pi-tui'\n";
		expect(parseInternalDependencyNames(inline, FIXED_WORKSPACE_MEMBERS)).toEqual(["pi-tui"]);
		expect(parseInternalDependencyNames(expanded, FIXED_WORKSPACE_MEMBERS)).toEqual(["pi-tui"]);
	});

	test("quoted dependency keys and subtables resolve workspace crates", () => {
		const direct = '[dependencies]\n"pi-tui" = { path = "../pi-tui" }\n';
		const aliased = "[dependencies.'ui_alias']\npackage = 'pi-tui'\npath = '../pi-tui'\n";
		expect(parseInternalDependencyNames(direct, FIXED_WORKSPACE_MEMBERS)).toEqual(["pi-tui"]);
		expect(parseInternalDependencyNames(aliased, FIXED_WORKSPACE_MEMBERS)).toEqual(["pi-tui"]);
	});

	test("commented Cargo aliases do not create workspace edges", () => {
		const manifest =
			'[dependencies]\n# old = { package = "pi-tui" }\n[dependencies.old]\n# package = "pi-tui"\n';
		expect(parseInternalDependencyNames(manifest, FIXED_WORKSPACE_MEMBERS)).toEqual([]);
	});

	test("comments and multiline literals are not AgentLoopConfig sites", () => {
		const sites = enumerateAgentLoopConfigSites({
			"crates/pi-agent/src/decls.rs":
				'/*\nAgentLoopConfig {\n*/\nconst NOTE: &str = r#"\nAgentLoopConfig {\n"#;\n// AgentLoopConfig {\n',
		});
		expect(sites).toEqual([]);
	});

	test("declarations and signatures are not AgentLoopConfig literal sites", () => {
		const sites = enumerateAgentLoopConfigSites({
			"crates/pi-agent/src/decls.rs":
				"pub struct AgentLoopConfig {\nimpl AgentLoopConfig {\n    fn f() -> AgentLoopConfig {\n        AgentLoopConfig {\n        }\n    }\n}\n",
		});
		expect(sites).toEqual([{ path: "crates/pi-agent/src/decls.rs", line: 4 }]);
	});

	test("drifted literal line fails the AgentLoopConfig site witness", () => {
		const directory = writeFixtureRepo(temporaryDirectory("parity-site-drift-"), { agentLiteralLine: 40 });
		const violations = runParityWitnesses(directory);
		expect(hasViolation(violations, "AgentLoopConfig literal at crates/pi-agent/src/agent.rs:40 matches no pinned site")).toBe(
			true,
		);
		expect(hasViolation(violations, "missing pinned AgentLoopConfig literal site crates/pi-agent/src/agent.rs:62-88")).toBe(
			true,
		);
	});

	test("a sixth literal site fails the AgentLoopConfig site witness", () => {
		const directory = writeFixtureRepo(temporaryDirectory("parity-sixth-site-"), {
			extraLiteralPath: "crates/pi/src/main.rs",
		});
		const violations = runParityWitnesses(directory);
		expect(hasViolation(violations, "expected exactly 5 AgentLoopConfig literal sites, found 6")).toBe(true);
		expect(hasViolation(violations, "crates/pi/src/main.rs:1 matches no pinned site")).toBe(true);
	});

	test("empty site list fails the AgentLoopConfig site witness", () => {
		const violations = verifyAgentLoopConfigSites([]);
		expect(hasViolation(violations, "expected exactly 5 AgentLoopConfig literal sites, found 0")).toBe(true);
		expect(hasViolation(violations, "missing pinned AgentLoopConfig literal site")).toBe(true);
	});
});
