#!/usr/bin/env bun
/**
 * DEPS-R2 shipped-exposure classifier.
 *
 * Classifies each proposed dependency remediation as Class E (complete E1–E4
 * pass bundle) or Class S (shipped-exposed / undecidable). Fail-closed: every
 * loader/runner/parser failure maps the affected check to undecidable and the
 * input to Class S. Emits pi.deps.exposure.v1 evidence without short-circuit.
 */

import { createHash } from "node:crypto";
import { existsSync } from "node:fs";
import {
	appendFile,
	mkdir,
	mkdtemp,
	readFile,
	readdir,
	rm,
	symlink,
	writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { basename, dirname, join, relative, resolve, sep } from "node:path";

import { HOST_PACKAGE_DIR, hostBundleCommands } from "../release/host.ts";
import type { CommandRunner, Fs, RunResult } from "../release/runner.ts";
import { SpawnRunner, realFs } from "../release/runner.ts";
import type { AssembleInputs } from "../release/stage.ts";
import { stagedInputs } from "../release/stage.ts";
import { TARGET_PLANS } from "../release/targets.ts";

export const SCHEMA = "pi.deps.exposure.v1" as const;
export const SENTINEL_OK = "DEPENDENCY_EXPOSURE_OK";
export const SENTINEL_FAILED = "DEPENDENCY_EXPOSURE_FAILED_CLOSED";
export const REPO_ROOT = resolve(import.meta.dirname, "../..");

export const SURFACES = [
	"package.json",
	"packages/extension-host/package.json",
	"packages/pi-tui-protocol/package.json",
] as const;

const BUN_LOCKS = ["bun.lock", "packages/extension-host/bun.lock"] as const;
const DEP_FIELDS = [
	"dependencies",
	"devDependencies",
	"peerDependencies",
	"optionalDependencies",
] as const;
const NON_EXEMPT_TOOLS = new Set(["rust-toolchain", "bun-runtime", "bun-bundler"]);
const EXCLUDED_DIRS = new Set([".git", ".references", "node_modules", "target", "dist"]);
const KNOWN_COMPILE_SITES = new Set([
	"scripts/release/host.ts",
	"packages/extension-host/package.json",
	"packages/extension-host/tests/acceptance.test.ts",
	"packages/extension-host/tests/bundled-modules.test.ts",
	".github/workflows/release-verification.yml",
	"scripts/tests/host.test.ts",
	"scripts/verification/compat-matrix.json",
	".outline/sdd/workflowz.json",
]);
const E3_FIXED_FILES = ["scripts/package-release.ts", "scripts/build-extension-host.ts"] as const;
const TOOL_FILES: Readonly<Record<string, readonly string[]>> = {
	"rust-toolchain": ["rust-toolchain.toml", "Cargo.toml"],
	"bun-runtime": ["scripts/release/runtime.ts"],
	"bun-bundler": [".github/workflows/release-verification.yml"],
};
const CHECK_NAMES = ["E1", "E2", "E3", "E4"] as const;
const BUILD_TIMEOUT_MS = 10 * 60_000;
const COMMAND_TIMEOUT_MS = 5 * 60_000;

type CheckName = (typeof CHECK_NAMES)[number];
export type CheckStatus = "pass" | "fail" | "undecidable";
export type ExposureClass = "S" | "E";
export type Overall = ExposureClass | "none";

export interface InputSpec {
	readonly kind: "npm" | "cargo" | "tool";
	readonly name: string;
	readonly raw: string;
}

export interface CheckResult {
	readonly status: CheckStatus;
	readonly detail: string;
}

export interface Verdict {
	readonly input: string;
	readonly class: ExposureClass;
	readonly checks: Readonly<Record<CheckName, CheckResult>>;
}

export interface ExposureReport {
	readonly schema: typeof SCHEMA;
	readonly decidedAt: string;
	readonly base: string;
	readonly head: "worktree";
	readonly verdicts: readonly Verdict[];
	readonly overall: Overall;
}

export interface ClassifyOptions {
	readonly base: string;
	readonly inputs: readonly string[];
	readonly repoRoot?: string;
	readonly runner?: CommandRunner;
	readonly now?: () => Date;
	readonly identity?: boolean;
	readonly fs?: Fs;
}

interface SideData {
	readonly root: string;
	readonly npmSurfaces: readonly NpmSurface[];
	readonly cargo?: CargoGraph;
	readonly corpus: readonly CorpusFile[];
	readonly packageScripts: ReadonlyMap<string, string>;
}

export interface NpmSurface {
	readonly path: string;
	readonly directory: string;
	readonly fields: Readonly<Record<string, Readonly<Record<string, string>>>>;
}

export interface CargoGraph {
	readonly packages: readonly CargoPackage[];
	readonly resolve: readonly CargoNode[];
}

interface CargoPackage {
	readonly id: string;
	readonly name: string;
	readonly manifestPath: string;
}

interface CargoNode {
	readonly deps: readonly CargoDep[];
}

interface CargoDep {
	readonly pkg: string;
	readonly kinds: readonly (string | null)[];
}

export interface CorpusFile {
	readonly path: string;
	readonly text: string;
}

export interface BundleEntryEvidence {
	readonly side: "base" | "head";
	readonly name: string;
	readonly mode: "compiled" | "graph-only" | "bundle";
	readonly inputs: readonly string[];
}

export interface BundleEvidence {
	readonly entries?: readonly BundleEntryEvidence[];
	readonly error?: string;
	readonly drift?: string;
}

export interface PackageLocation {
	readonly packageJson: string;
	readonly root: string;
	readonly prefixes: readonly string[];
	readonly bins: readonly string[];
}

interface MaterializedBase {
	readonly root: string;
	cleanup(): Promise<void>;
}

interface CliArgs {
	readonly mode: "self-test" | "classify";
	readonly base?: string;
	readonly inputs: readonly string[];
	readonly jsonPath?: string;
	readonly recordPath?: string;
}

function pass(detail: string): CheckResult {
	return { status: "pass", detail };
}

function fail(detail: string): CheckResult {
	return { status: "fail", detail };
}

function undecidable(detail: string): CheckResult {
	return { status: "undecidable", detail };
}

function errorText(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}

function asTable(value: unknown): Record<string, unknown> | undefined {
	return typeof value === "object" && value !== null && !Array.isArray(value)
		? (value as Record<string, unknown>)
		: undefined;
}

function asStringTable(value: unknown): Record<string, string> {
	const table = asTable(value);
	if (table === undefined) return {};
	const result: Record<string, string> = {};
	for (const [key, item] of Object.entries(table)) {
		if (typeof item !== "string") throw new Error(`dependency ${key} has non-string value`);
		result[key] = item;
	}
	return result;
}

export function normalizePath(path: string): string {
	return path.split("\\").join("/").replace(/^\.\//, "");
}

function pathIntersects(left: string, right: string): boolean {
	const a = resolve(left);
	const b = resolve(right);
	return a === b || a.startsWith(`${b}${sep}`) || b.startsWith(`${a}${sep}`);
}

export function parseInputSpec(raw: string): InputSpec {
	const separator = raw.indexOf(":");
	if (separator < 1 || separator === raw.length - 1) {
		throw new Error(`invalid input "${raw}"; expected npm:<name>, cargo:<name>, or tool:<id>`);
	}
	const kind = raw.slice(0, separator);
	const name = raw.slice(separator + 1);
	if (kind !== "npm" && kind !== "cargo" && kind !== "tool") {
		throw new Error(`invalid input kind "${kind}" in "${raw}"`);
	}
	if (name.trim() !== name || name.length === 0) throw new Error(`invalid empty or padded input "${raw}"`);
	return { kind, name, raw: `${kind}:${name}` };
}

export function verdictFromChecks(input: string, checks: Verdict["checks"]): Verdict {
	const exempt = CHECK_NAMES.every((name) => checks[name].status === "pass");
	return { input, class: exempt ? "E" : "S", checks };
}

function allUndecidable(input: string, detail: string): Verdict {
	return verdictFromChecks(input, {
		E1: undecidable(detail),
		E2: undecidable(detail),
		E3: undecidable(detail),
		E4: undecidable(detail),
	});
}

function reportOf(base: string, verdicts: readonly Verdict[], now: () => Date): ExposureReport {
	const overall: Overall = verdicts.length === 0
		? "none"
		: verdicts.some((verdict) => verdict.class === "S")
			? "S"
			: "E";
	return {
		schema: SCHEMA,
		decidedAt: now().toISOString(),
		base,
		head: "worktree",
		verdicts,
		overall,
	};
}

export function parseNpmSurface(path: string, text: string): NpmSurface {
	const root = asTable(JSON.parse(text) as unknown);
	if (root === undefined) throw new Error(`${path} is not a JSON object`);
	const fields: Record<string, Record<string, string>> = {};
	for (const field of DEP_FIELDS) fields[field] = asStringTable(root[field]);
	return { path, directory: dirname(path), fields };
}

async function loadNpmSurfaces(root: string): Promise<readonly NpmSurface[]> {
	return Promise.all(
		SURFACES.map(async (path) => parseNpmSurface(path, await readFile(join(root, path), "utf8"))),
	);
}

export function evaluateNpmMembership(
	name: string,
	base: readonly NpmSurface[],
	head: readonly NpmSurface[],
): CheckResult {
	const memberships: string[] = [];
	for (const side of [base, head]) {
		for (const surface of side) {
			for (const field of DEP_FIELDS) {
				if (Object.hasOwn(surface.fields[field] ?? {}, name)) {
					memberships.push(`${surface.path}:${field}`);
				}
			}
		}
	}
	if (memberships.length === 0) {
		return undecidable(`${name} absent from every npm surface on both sides`);
	}
	const forbidden = memberships.filter((entry) => !entry.endsWith(":devDependencies"));
	return forbidden.length === 0
		? pass(`union membership is devDependencies-only: ${memberships.join(", ")}`)
		: fail(`non-dev npm membership on either side: ${forbidden.join(", ")}`);
}

export function parseCargoMetadata(text: string): CargoGraph {
	const root = asTable(JSON.parse(text) as unknown);
	const packagesRaw = root?.packages;
	const resolveRaw = asTable(root?.resolve)?.nodes;
	if (!Array.isArray(packagesRaw) || !Array.isArray(resolveRaw)) {
		throw new Error("cargo metadata missing packages/resolve.nodes");
	}
	const packages: CargoPackage[] = packagesRaw.map((item, index) => {
		const table = asTable(item);
		if (
			typeof table?.id !== "string" ||
			typeof table.name !== "string" ||
			typeof table.manifest_path !== "string"
		) {
			throw new Error(`cargo metadata packages[${index}] malformed`);
		}
		return { id: table.id, name: table.name, manifestPath: table.manifest_path };
	});
	const resolve: CargoNode[] = resolveRaw.map((item, nodeIndex) => {
		const table = asTable(item);
		if (!Array.isArray(table?.deps)) {
			throw new Error(`cargo metadata resolve.nodes[${nodeIndex}].deps malformed`);
		}
		const deps: CargoDep[] = table.deps.map((dep, depIndex) => {
			const depTable = asTable(dep);
			if (typeof depTable?.pkg !== "string" || !Array.isArray(depTable.dep_kinds)) {
				throw new Error(`cargo metadata dep ${nodeIndex}:${depIndex} malformed`);
			}
			const kinds = depTable.dep_kinds.map((kind, kindIndex) => {
				const kindTable = asTable(kind);
				if (kindTable === undefined || !(kindTable.kind === null || typeof kindTable.kind === "string")) {
					throw new Error(`cargo metadata dep_kind ${nodeIndex}:${depIndex}:${kindIndex} malformed`);
				}
				return kindTable.kind;
			});
			return { pkg: depTable.pkg, kinds };
		});
		return { deps };
	});
	return { packages, resolve };
}

async function loadCargoGraph(root: string, runner: CommandRunner): Promise<CargoGraph> {
	const result = await runner.run("cargo", ["metadata", "--format-version", "1", "--locked"], {
		cwd: root,
		rejectOnError: false,
		timeoutMs: COMMAND_TIMEOUT_MS,
	});
	if (result.exitCode !== 0) throw new Error(`cargo metadata failed: ${result.stderr.slice(0, 500)}`);
	return parseCargoMetadata(result.stdout);
}

function cargoKinds(name: string, graph: CargoGraph): readonly (string | null)[] {
	const ids = new Set(graph.packages.filter((pkg) => pkg.name === name).map((pkg) => pkg.id));
	return graph.resolve.flatMap((node) =>
		node.deps.filter((dep) => ids.has(dep.pkg)).flatMap((dep) => dep.kinds),
	);
}

export function evaluateCargoKinds(name: string, base: CargoGraph, head: CargoGraph): CheckResult {
	const kinds = [...cargoKinds(name, base), ...cargoKinds(name, head)];
	if (kinds.length === 0) {
		return undecidable(`${name} has no dependency edge in either cargo graph`);
	}
	const shipped = kinds.filter((kind) => kind !== "dev");
	return shipped.length === 0
		? pass(`all ${kinds.length} dependency edges are dev`)
		: fail(`normal/build cargo edge present: ${shipped.map((kind) => kind ?? "normal").join(", ")}`);
}

async function walkFiles(root: string, relativeDir = ""): Promise<string[]> {
	const directory = join(root, relativeDir);
	const entries = await readdir(directory, { withFileTypes: true });
	const files: string[] = [];
	for (const entry of entries) {
		const rel = normalizePath(join(relativeDir, entry.name));
		if (entry.isDirectory()) {
			if (!EXCLUDED_DIRS.has(entry.name)) files.push(...await walkFiles(root, rel));
			continue;
		}
		if (entry.isFile()) files.push(rel);
	}
	return files;
}

async function loadCorpus(root: string): Promise<readonly CorpusFile[]> {
	const releaseDir = join(root, "scripts/release");
	const releaseFiles = (await readdir(releaseDir, { withFileTypes: true }))
		.filter((entry) => entry.isFile() && entry.name.endsWith(".ts"))
		.map((entry) => `scripts/release/${entry.name}`);
	const workflowDir = join(root, ".github/workflows");
	const workflowFiles = (await readdir(workflowDir, { withFileTypes: true }))
		.filter((entry) => entry.isFile() && /\.ya?ml$/.test(entry.name))
		.map((entry) => `.github/workflows/${entry.name}`);
	const paths = [...releaseFiles, ...E3_FIXED_FILES, ...workflowFiles];
	return Promise.all(
		paths.map(async (path) => ({ path, text: await readFile(join(root, path), "utf8") })),
	);
}

async function loadPackageScripts(root: string): Promise<ReadonlyMap<string, string>> {
	const scripts = new Map<string, string>();
	for (const path of SURFACES) {
		const parsed = asTable(JSON.parse(await readFile(join(root, path), "utf8")) as unknown);
		const table = asTable(parsed?.scripts);
		for (const [name, body] of Object.entries(table ?? {})) {
			if (typeof body !== "string") throw new Error(`${path} script ${name} is not a string`);
			scripts.set(`${path}#${name}`, body);
			if (!scripts.has(name)) scripts.set(name, body);
		}
	}
	return scripts;
}

function extractInvokedScripts(text: string): string[] {
	const names = new Set<string>();
	for (const match of text.matchAll(/\bbun\s+run\s+([\w:.-]+)/g)) {
		if (match[1] !== undefined) names.add(match[1]);
	}
	for (const match of text.matchAll(/["']run["']\s*,\s*["']([\w:.-]+)["']/g)) {
		if (match[1] !== undefined) names.add(match[1]);
	}
	return [...names];
}

export function expandScripts(
	corpus: readonly CorpusFile[],
	scripts: ReadonlyMap<string, string>,
): CorpusFile[] {
	const expanded = [...corpus];
	const queue = corpus.flatMap((file) => extractInvokedScripts(file.text));
	const visited = new Set<string>();
	while (queue.length > 0) {
		const name = queue.shift();
		if (name === undefined || visited.has(name)) continue;
		visited.add(name);
		if (visited.size > 64) throw new Error("package script expansion exceeded 64 entries");
		const body = scripts.get(name);
		if (body === undefined) continue;
		expanded.push({ path: `package.json#scripts.${name}`, text: body });
		queue.push(...extractInvokedScripts(body));
	}
	return expanded;
}

function escapeRegex(value: string): string {
	return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

export function scanCorpusText(
	name: string,
	bins: readonly string[],
	files: readonly CorpusFile[],
): CheckResult {
	const escaped = escapeRegex(name);
	const quoted = new RegExp(String.raw`["'\x60]` + escaped + String.raw`(?:[/"'\x60]|$)`);
	for (const file of files) {
		if (quoted.test(file.text)) {
			return fail(`${name} referenced by shipped-byte producer ${file.path}`);
		}
		for (const bin of bins) {
			const token = new RegExp(
				`(^|[^\\w@./-])${escapeRegex(bin)}(?=$|[^\\w@./-])`,
			);
			if (token.test(file.text)) {
				return fail(`bin ${bin} invoked by shipped-byte producer ${file.path}`);
			}
		}
	}
	return pass(
		`no package import, quoted name, or bin token in ${files.length} release/CI corpus files`,
	);
}

export async function findUnknownCompileSites(root: string): Promise<string[]> {
	const files = await walkFiles(root);
	const witnesses: string[] = [];
	for (const path of files) {
		if (
			path === "scripts/verification/dependency-exposure.ts" ||
			path === "scripts/verification/dependency-exposure.test.ts"
		) {
			continue;
		}
		const extension = path.slice(path.lastIndexOf("."));
		if (![".ts", ".json", ".yml", ".yaml"].includes(extension)) continue;
		const text = await readFile(join(root, path), "utf8");
		if ((text.includes("--compile") || text.includes("bun build")) && !KNOWN_COMPILE_SITES.has(path)) {
			witnesses.push(path);
		}
	}
	return witnesses;
}

async function shippingBuildCommands(
	root: string,
	temp: string,
): Promise<readonly { name: string; args: readonly string[] }[]> {
	const commands: { name: string; args: readonly string[] }[] = TARGET_PLANS.map((plan) => ({
		name: `compiled:${plan.rustTarget}`,
		args: hostBundleCommands(plan, temp).compiled,
	}));
	const firstPlan = TARGET_PLANS[0];
	if (firstPlan === undefined) throw new Error("TARGET_PLANS is empty");
	commands.push({
		name: "runtime-bundle",
		args: hostBundleCommands(firstPlan, temp).runtimeBundle,
	});
	const packageJson = asTable(
		JSON.parse(await readFile(join(root, HOST_PACKAGE_DIR, "package.json"), "utf8")) as unknown,
	);
	const build = asTable(packageJson?.scripts)?.build;
	if (typeof build !== "string") throw new Error("extension-host package.json has no build script");
	const tokens = build.trim().split(/\s+/);
	if (tokens[0] !== "bun" || tokens[1] !== "build") {
		throw new Error("extension-host build script is not a bun build command");
	}
	commands.push({ name: "package-script:build", args: tokens.slice(1) });
	return commands;
}

function replaceOutputArgs(args: readonly string[], output: string, metafile: string): string[] {
	const result: string[] = [];
	for (let index = 0; index < args.length; index++) {
		const arg = args[index];
		if (arg === undefined) continue;
		if (arg === "--outfile" || arg === "--outdir") {
			index++;
			continue;
		}
		if (arg.startsWith("--outfile=") || arg.startsWith("--outdir=")) continue;
		result.push(arg);
	}
	result.push("--outfile", output, `--metafile=${metafile}`);
	return result;
}

export function parseMetafile(text: string): readonly string[] {
	const root = asTable(JSON.parse(text) as unknown);
	const inputs = asTable(root?.inputs);
	if (inputs === undefined) throw new Error("Bun metafile missing top-level inputs object");
	return Object.keys(inputs).map(normalizePath);
}

async function runBuildGraph(
	root: string,
	runner: CommandRunner,
	name: string,
	args: readonly string[],
	temp: string,
): Promise<Omit<BundleEntryEvidence, "side">> {
	const slug = createHash("sha256").update(name).digest("hex").slice(0, 12);
	const output = join(temp, `${slug}.out`);
	const metafile = join(temp, `${slug}.json`);
	const primary = replaceOutputArgs(args, output, metafile);
	let mode: BundleEntryEvidence["mode"] = args.includes("--compile") ? "compiled" : "bundle";
	let result = await runner.run("bun", primary, {
		cwd: join(root, HOST_PACKAGE_DIR),
		rejectOnError: false,
		timeoutMs: BUILD_TIMEOUT_MS,
	});
	const primaryOk = result.exitCode === 0 && existsSync(metafile);
	if (!primaryOk && args.includes("--compile")) {
		mode = "graph-only";
		const fallback = primary.filter((arg) => arg !== "--compile");
		result = await runner.run("bun", fallback, {
			cwd: join(root, HOST_PACKAGE_DIR),
			rejectOnError: false,
			timeoutMs: BUILD_TIMEOUT_MS,
		});
	}
	if (result.exitCode !== 0) {
		throw new Error(`${name} metafile build failed: ${result.stderr.slice(0, 500)}`);
	}
	if (!existsSync(metafile)) {
		throw new Error(`${name} metafile missing after successful bun build`);
	}
	return { name, mode, inputs: parseMetafile(await readFile(metafile, "utf8")) };
}

async function installHost(root: string, runner: CommandRunner): Promise<void> {
	const result = await runner.run("bun", ["install", "--frozen-lockfile"], {
		cwd: join(root, HOST_PACKAGE_DIR),
		rejectOnError: false,
		timeoutMs: COMMAND_TIMEOUT_MS,
	});
	if (result.exitCode !== 0) {
		throw new Error(`host install failed: ${result.stderr.slice(0, 500)}`);
	}
}

async function buildSideEvidence(
	side: "base" | "head",
	root: string,
	runner: CommandRunner,
): Promise<readonly BundleEntryEvidence[]> {
	const scratchRoot = join(root, "target", "deps-r2-meta");
	await mkdir(scratchRoot, { recursive: true });
	const temp = await mkdtemp(join(scratchRoot, `${side}-`));
	try {
		await installHost(root, runner);
		const commands = await shippingBuildCommands(root, temp);
		const entries: BundleEntryEvidence[] = [];
		for (const command of commands) {
			entries.push({ side, ...await runBuildGraph(root, runner, command.name, command.args, temp) });
		}
		return entries;
	} finally {
		await rm(temp, { recursive: true, force: true });
	}
}

export async function buildBundleEvidence(
	baseRoot: string,
	headRoot: string,
	runner: CommandRunner,
): Promise<BundleEvidence> {
	try {
		const baseDrift = await findUnknownCompileSites(baseRoot);
		const headDrift = baseRoot === headRoot ? baseDrift : await findUnknownCompileSites(headRoot);
		const drift = [...new Set([...baseDrift, ...headDrift])];
		if (drift.length > 0) return { drift: `unknown bun compile/build site(s): ${drift.join(", ")}` };
		const base = await buildSideEvidence("base", baseRoot, runner);
		const head = baseRoot === headRoot
			? base.map((entry) => ({ ...entry, side: "head" as const }))
			: await buildSideEvidence("head", headRoot, runner);
		return { entries: [...base, ...head] };
	} catch (error) {
		return { error: errorText(error) };
	}
}

async function locatePackage(root: string, name: string): Promise<PackageLocation | undefined> {
	const candidates = [
		join(root, "node_modules", name),
		join(root, HOST_PACKAGE_DIR, "node_modules", name),
	];
	for (const surface of await loadNpmSurfaces(root)) {
		for (const field of DEP_FIELDS) {
			const value = surface.fields[field]?.[name];
			if (value?.startsWith("file:")) {
				candidates.push(resolve(root, surface.directory, value.slice("file:".length)));
			}
		}
	}
	for (const candidate of candidates) {
		const packageJson = join(candidate, "package.json");
		if (!existsSync(packageJson)) continue;
		const parsed = asTable(JSON.parse(await readFile(packageJson, "utf8")) as unknown);
		const bin = parsed?.bin;
		const bins = typeof bin === "string"
			? [typeof parsed?.name === "string" ? basename(parsed.name) : basename(name)]
			: Object.keys(asTable(bin) ?? {});
		const rel = normalizePath(relative(join(root, HOST_PACKAGE_DIR), candidate));
		return {
			packageJson,
			root: candidate,
			prefixes: [`node_modules/${name}/`, `${rel}/`].map(normalizePath),
			bins,
		};
	}
	return undefined;
}

export function inputMatchesMetafile(path: string, prefixes: readonly string[]): boolean {
	const normalized = normalizePath(path);
	return prefixes.some((prefix) => {
		if (normalized === prefix.slice(0, -1) || normalized.startsWith(prefix)) return true;
		if (!prefix.startsWith("node_modules/")) return false;
		return normalized.includes(`/${prefix}`);
	});
}

export function evaluateMetafileReachability(
	name: string,
	prefixes: readonly string[],
	entries: readonly BundleEntryEvidence[],
): CheckResult {
	const hits = entries.filter((entry) =>
		entry.inputs.some((path) => inputMatchesMetafile(path, prefixes)),
	);
	if (hits.length > 0) {
		return fail(
			`${name} reachable in shipping Bun graph(s): ${hits.map((hit) => `${hit.side}:${hit.name}`).join(", ")}`,
		);
	}
	return pass(
		`${name} absent from ${entries.length} authority-derived before/after shipping Bun graphs`,
	);
}

export function evaluateSideAwareReachability(
	name: string,
	locations: { readonly base?: PackageLocation; readonly head?: PackageLocation },
	entries: readonly BundleEntryEvidence[],
): CheckResult {
	const baseLocation = locations.base;
	const headLocation = locations.head;
	if (baseLocation === undefined || headLocation === undefined) {
		return undecidable(`${name} package root/prefix cannot be located independently on both sides`);
	}
	const hits = entries.filter((entry) => {
		const location = entry.side === "base" ? baseLocation : headLocation;
		return entry.inputs.some((path) => inputMatchesMetafile(path, location.prefixes));
	});
	if (hits.length > 0) {
		return fail(
			`${name} reachable in shipping Bun graph(s): ${hits.map((hit) => `${hit.side}:${hit.name}`).join(", ")}`,
		);
	}
	return pass(
		`${name} absent from ${entries.length} authority-derived before/after shipping Bun graphs`,
	);
}

function checkE2(
	spec: InputSpec,
	locations: { readonly base?: PackageLocation; readonly head?: PackageLocation },
	bundles: BundleEvidence,
): CheckResult {
	if (bundles.drift !== undefined) return undecidable(bundles.drift);
	if (bundles.error !== undefined || bundles.entries === undefined) {
		return undecidable(`shipping Bun graph probe failed: ${bundles.error ?? "missing evidence"}`);
	}
	if (spec.kind !== "npm") {
		return pass(`${spec.kind} input cannot enter Bun package graph; all ${bundles.entries.length} graphs loaded`);
	}
	const result = evaluateSideAwareReachability(spec.name, locations, bundles.entries);
	if (
		bundles.entries.some((entry) => entry.mode === "graph-only") &&
		spec.name === "typebox" &&
		result.status !== "fail"
	) {
		return undecidable("compile+metafile fallback lost the typebox reachability sanity witness");
	}
	return result;
}

function checkE3(
	spec: InputSpec,
	locations: { readonly base?: PackageLocation; readonly head?: PackageLocation },
	base: SideData,
	head: SideData,
): CheckResult {
	if (spec.kind === "tool") {
		return NON_EXEMPT_TOOLS.has(spec.name)
			? fail(
				`${spec.name} is unconditionally non-exempt: toolchain/runtime/bundler changes produce shipped bytes`,
			)
			: undecidable(`unknown tool id ${spec.name}`);
	}
	if (spec.kind === "npm" && locations.base === undefined && locations.head === undefined) {
		return undecidable(`${spec.name} package metadata/bin mapping unavailable on both sides`);
	}
	const bins = [...new Set([...(locations.base?.bins ?? []), ...(locations.head?.bins ?? [])])];
	try {
		const baseFiles = expandScripts(base.corpus, base.packageScripts);
		const headFiles = expandScripts(head.corpus, head.packageScripts);
		return scanCorpusText(spec.name, bins, [...baseFiles, ...headFiles]);
	} catch (error) {
		return undecidable(`release/CI corpus scan failed: ${errorText(error)}`);
	}
}

function stagedSources(root: string, fs: Fs): readonly string[] {
	const sources: string[] = [];
	for (const plan of TARGET_PLANS) {
		const common = {
			plan,
			version: "0.0.0",
			piBinaryPath: join(root, "target", plan.rustTarget, "release", plan.piBinaryName),
			repoRoot: root,
			fs,
			sourceDateEpoch: 0,
			compatibilityVersion: "0",
			protocolVersion: 0,
			createdAt: "1970-01-01T00:00:00.000Z",
			docsSource: join(root, "crates/pi/docs"),
			examplesSource: join(root, ".references/pi/packages/coding-agent/examples"),
			assetsSource: join(root, "crates/pi/assets"),
		} satisfies Omit<AssembleInputs, "host" | "bunRuntimePath">;
		const compiled: AssembleInputs = {
			...common,
			host: { kind: "compiled", binaryPath: join(root, "target/host", plan.hostBinaryName) },
		};
		const runtime: AssembleInputs = {
			...common,
			host: {
				kind: "runtime-bundle",
				runtimePath: join(root, "target/host", plan.bunRuntimeName),
				scriptPath: join(root, "target/host", plan.hostBundleName),
			},
			bunRuntimePath: join(root, "target/runtime", plan.bunRuntimeName),
		};
		for (const input of [...stagedInputs(compiled), ...stagedInputs(runtime)]) {
			if (input.source.length > 0 && !input.source.startsWith("generated:")) {
				sources.push(input.source);
			}
		}
	}
	return sources;
}

export function evaluateStaging(paths: readonly string[], staged: readonly string[]): CheckResult {
	const intersections = staged.filter((source) => paths.some((path) => pathIntersects(path, source)));
	return intersections.length === 0
		? pass(`no source path intersects ${staged.length} authority-derived staging inputs`)
		: fail(`source intersects staged input(s): ${intersections.join(", ")}`);
}

function cargoManifestPaths(name: string, graphs: readonly CargoGraph[]): string[] {
	return graphs.flatMap((graph) =>
		graph.packages.filter((pkg) => pkg.name === name).map((pkg) => dirname(pkg.manifestPath)),
	);
}

function checkE4(
	spec: InputSpec,
	locations: { readonly base?: PackageLocation; readonly head?: PackageLocation },
	base: SideData,
	head: SideData,
	e1: CheckResult,
	fs: Fs,
): CheckResult {
	try {
		const staged = [...stagedSources(base.root, fs), ...stagedSources(head.root, fs)];
		if (spec.kind === "tool") {
			return spec.name === "bun-runtime"
				? fail("Bun runtime is an authority-derived runtime-bundle staging input")
				: pass(`${spec.name} has no direct archive staging exemption; E3 remains non-exempt`);
		}
		if (spec.kind === "npm") {
			if (locations.base === undefined || locations.head === undefined) {
				return undecidable(`${spec.name} package path cannot be mapped independently on both sides for staging`);
			}
			return evaluateStaging([locations.base.root, locations.head.root], staged);
		}
		if (e1.status === "fail") return fail("normal/build cargo edge ships inside the pi binary");
		if (base.cargo === undefined || head.cargo === undefined) {
			return undecidable("cargo metadata unavailable for staging mapping");
		}
		const manifests = cargoManifestPaths(spec.name, [base.cargo, head.cargo]);
		if (manifests.length === 0) {
			return undecidable(`${spec.name} manifest directory cannot be mapped`);
		}
		return evaluateStaging(manifests, staged);
	} catch (error) {
		return undecidable(`staging authority probe failed: ${errorText(error)}`);
	}
}

async function loadSide(root: string, runner: CommandRunner, needCargo: boolean): Promise<SideData> {
	const [npmSurfaces, corpus, packageScripts] = await Promise.all([
		loadNpmSurfaces(root),
		loadCorpus(root),
		loadPackageScripts(root),
	]);
	const cargo = needCargo ? await loadCargoGraph(root, runner) : undefined;
	return { root, npmSurfaces, cargo, corpus, packageScripts };
}

function checkE1(spec: InputSpec, base: SideData, head: SideData): CheckResult {
	if (spec.kind === "npm") return evaluateNpmMembership(spec.name, base.npmSurfaces, head.npmSurfaces);
	if (spec.kind === "tool") {
		return NON_EXEMPT_TOOLS.has(spec.name)
			? fail(`${spec.name} is a tool input, not a dev-only package edge`)
			: undecidable(`unknown tool id ${spec.name}`);
	}
	if (base.cargo === undefined || head.cargo === undefined) {
		return undecidable("cargo metadata unavailable");
	}
	return evaluateCargoKinds(spec.name, base.cargo, head.cargo);
}

async function classifyLoaded(
	specs: readonly InputSpec[],
	base: SideData,
	head: SideData,
	bundles: BundleEvidence,
	fs: Fs,
): Promise<readonly Verdict[]> {
	const verdicts: Verdict[] = [];
	for (const spec of specs) {
		let locations: { base?: PackageLocation; head?: PackageLocation } = {};
		try {
			if (spec.kind === "npm") {
				locations = {
					base: await locatePackage(base.root, spec.name),
					head: await locatePackage(head.root, spec.name),
				};
			}
		} catch (error) {
			const detail = `package location probe failed: ${errorText(error)}`;
			verdicts.push(verdictFromChecks(spec.raw, {
				E1: checkE1(spec, base, head),
				E2: undecidable(detail),
				E3: undecidable(detail),
				E4: undecidable(detail),
			}));
			continue;
		}
		const e1 = checkE1(spec, base, head);
		verdicts.push(verdictFromChecks(spec.raw, {
			E1: e1,
			E2: checkE2(spec, locations, bundles),
			E3: checkE3(spec, locations, base, head),
			E4: checkE4(spec, locations, base, head, e1, fs),
		}));
	}
	return verdicts;
}

async function commandOk(
	runner: CommandRunner,
	command: string,
	args: readonly string[],
	cwd: string,
): Promise<RunResult> {
	const result = await runner.run(command, args, {
		cwd,
		rejectOnError: false,
		timeoutMs: COMMAND_TIMEOUT_MS,
	});
	if (result.exitCode !== 0) {
		throw new Error(`${command} ${args.join(" ")} failed: ${result.stderr.slice(0, 500)}`);
	}
	return result;
}

export async function materializeBase(
	repoRoot: string,
	base: string,
	runner: CommandRunner,
): Promise<MaterializedBase> {
	await commandOk(runner, "git", ["rev-parse", "--verify", `${base}^{commit}`], repoRoot);
	const parent = await mkdtemp(join(tmpdir(), "pi-deps-r2-base-"));
	const root = join(parent, "tree");
	let added = false;
	try {
		await commandOk(runner, "git", ["worktree", "add", "--detach", root, base], repoRoot);
		added = true;
		const reference = join(repoRoot, ".references/pi");
		if (existsSync(reference) && !existsSync(join(root, ".references/pi"))) {
			await mkdir(join(root, ".references"), { recursive: true });
			await symlink(reference, join(root, ".references/pi"), "dir");
		}
		return {
			root,
			cleanup: async () => {
				let removeError: Error | undefined;
				try {
					const remove = await runner.run("git", ["worktree", "remove", "--force", root], {
						cwd: repoRoot,
						rejectOnError: false,
						timeoutMs: COMMAND_TIMEOUT_MS,
					});
					if (remove.exitCode !== 0) {
						removeError = new Error(
							`git worktree remove --force failed: ${remove.stderr.slice(0, 500)}`,
						);
					}
				} catch (error) {
					removeError = error instanceof Error ? error : new Error(errorText(error));
				}
				await rm(parent, { recursive: true, force: true });
				if (removeError !== undefined) throw removeError;
			},
		};
	} catch (error) {
		if (added) {
			await runner.run("git", ["worktree", "remove", "--force", root], {
				cwd: repoRoot,
				rejectOnError: false,
				timeoutMs: COMMAND_TIMEOUT_MS,
			});
		}
		await rm(parent, { recursive: true, force: true });
		throw error;
	}
}

function parseBunLock(text: string): Record<string, string> {
	const root = asTable(Bun.JSONC.parse(text) as unknown);
	const packages = asTable(root?.packages);
	if (packages === undefined) throw new Error("bun.lock missing packages table");
	const result: Record<string, string> = {};
	for (const [name, value] of Object.entries(packages)) {
		result[name] = JSON.stringify(value);
	}
	return result;
}

function parseCargoLockVersions(text: string): Record<string, string> {
	const root = asTable(Bun.TOML.parse(text) as unknown);
	if (!Array.isArray(root?.package)) throw new Error("Cargo.lock missing package entries");
	const result: Record<string, string[]> = {};
	for (const [index, item] of root.package.entries()) {
		const table = asTable(item);
		if (typeof table?.name !== "string" || typeof table.version !== "string") {
			throw new Error(`Cargo.lock package[${index}] malformed`);
		}
		(result[table.name] ??= []).push(table.version);
	}
	return Object.fromEntries(
		Object.entries(result).map(([name, versions]) => [name, [...versions].sort().join(",")]),
	);
}

function changedKeys(
	before: Readonly<Record<string, string>>,
	after: Readonly<Record<string, string>>,
): string[] {
	return [...new Set([...Object.keys(before), ...Object.keys(after)])]
		.filter((key) => before[key] !== after[key]);
}

export function deriveAutoFromTexts(inputs: {
	readonly npmBefore: readonly string[];
	readonly npmAfter: readonly string[];
	readonly bunBefore: readonly string[];
	readonly bunAfter: readonly string[];
	readonly cargoBefore: string;
	readonly cargoAfter: string;
	readonly toolBefore: Readonly<Record<string, readonly string[]>>;
	readonly toolAfter: Readonly<Record<string, readonly string[]>>;
}): readonly string[] {
	const specs = new Set<string>();
	for (let index = 0; index < SURFACES.length; index++) {
		const path = SURFACES[index] ?? `surface-${index}`;
		const before = parseNpmSurface(path, inputs.npmBefore[index] ?? "{}");
		const after = parseNpmSurface(path, inputs.npmAfter[index] ?? "{}");
		for (const field of DEP_FIELDS) {
			for (const name of changedKeys(before.fields[field] ?? {}, after.fields[field] ?? {})) {
				specs.add(`npm:${name}`);
			}
		}
	}
	for (let index = 0; index < inputs.bunBefore.length; index++) {
		for (const name of changedKeys(
			parseBunLock(inputs.bunBefore[index] ?? '{"packages":{}}'),
			parseBunLock(inputs.bunAfter[index] ?? '{"packages":{}}'),
		)) {
			specs.add(`npm:${name}`);
		}
	}
	for (const name of changedKeys(
		parseCargoLockVersions(inputs.cargoBefore),
		parseCargoLockVersions(inputs.cargoAfter),
	)) {
		specs.add(`cargo:${name}`);
	}
	for (const tool of Object.keys(TOOL_FILES)) {
		if (JSON.stringify(inputs.toolBefore[tool] ?? []) !== JSON.stringify(inputs.toolAfter[tool] ?? [])) {
			specs.add(`tool:${tool}`);
		}
	}
	return [...specs].sort();
}

async function deriveAutoInputs(baseRoot: string, headRoot: string): Promise<readonly string[]> {
	const npmBefore = await Promise.all(SURFACES.map((path) => readFile(join(baseRoot, path), "utf8")));
	const npmAfter = await Promise.all(SURFACES.map((path) => readFile(join(headRoot, path), "utf8")));
	const bunBefore = await Promise.all(BUN_LOCKS.map((path) => readFile(join(baseRoot, path), "utf8")));
	const bunAfter = await Promise.all(BUN_LOCKS.map((path) => readFile(join(headRoot, path), "utf8")));
	const cargoBefore = await readFile(join(baseRoot, "Cargo.lock"), "utf8");
	const cargoAfter = await readFile(join(headRoot, "Cargo.lock"), "utf8");
	const toolBefore: Record<string, string[]> = {};
	const toolAfter: Record<string, string[]> = {};
	for (const [tool, files] of Object.entries(TOOL_FILES)) {
		toolBefore[tool] = await Promise.all(files.map((path) => readFile(join(baseRoot, path), "utf8")));
		toolAfter[tool] = await Promise.all(files.map((path) => readFile(join(headRoot, path), "utf8")));
	}
	return deriveAutoFromTexts({
		npmBefore,
		npmAfter,
		bunBefore,
		bunAfter,
		cargoBefore,
		cargoAfter,
		toolBefore,
		toolAfter,
	});
}

function requestedForCatastrophe(inputs: readonly string[]): readonly string[] {
	return inputs.length === 0 ? ["auto"] : inputs;
}

export async function classify(
	options: ClassifyOptions,
): Promise<{ report: ExposureReport; failedClosed: boolean }> {
	const repoRoot = resolve(options.repoRoot ?? REPO_ROOT);
	const runner = options.runner ?? new SpawnRunner();
	const now = options.now ?? (() => new Date());
	const fs = options.fs ?? realFs;
	let materialized: MaterializedBase | undefined;
	let outcome: { report: ExposureReport; failedClosed: boolean } | undefined;
	try {
		const baseRoot = options.identity
			? repoRoot
			: (materialized = await materializeBase(repoRoot, options.base, runner)).root;
		const rawInputs = options.inputs.includes("auto")
			? [...options.inputs.filter((input) => input !== "auto"), ...await deriveAutoInputs(baseRoot, repoRoot)]
			: [...options.inputs];
		const specs = [...new Set(rawInputs)].sort().map(parseInputSpec);
		if (specs.length === 0) {
			outcome = { report: reportOf(options.base, [], now), failedClosed: false };
		} else {
			const needCargo = specs.some((spec) => spec.kind === "cargo");
			let base: SideData;
			let head: SideData;
			try {
				base = await loadSide(baseRoot, runner, needCargo);
				head = baseRoot === repoRoot ? base : await loadSide(repoRoot, runner, needCargo);
				const bundles = await buildBundleEvidence(baseRoot, repoRoot, runner);
				const verdicts = await classifyLoaded(specs, base, head, bundles, fs);
				const failedClosed = verdicts.some((verdict) =>
					CHECK_NAMES.some((name) => verdict.checks[name].status === "undecidable"),
				);
				outcome = { report: reportOf(options.base, verdicts, now), failedClosed };
			} catch (error) {
				const report = reportOf(
					options.base,
					specs.map((spec) => allUndecidable(spec.raw, `loader failure: ${errorText(error)}`)),
					now,
				);
				outcome = { report, failedClosed: true };
			}
		}
	} catch (error) {
		const detail = `classification catastrophe: ${errorText(error)}`;
		const verdicts = requestedForCatastrophe(options.inputs).map((input) => allUndecidable(input, detail));
		outcome = { report: reportOf(options.base, verdicts, now), failedClosed: true };
	} finally {
		if (materialized !== undefined) {
			try {
				await materialized.cleanup();
			} catch (cleanupError) {
				const detail = `worktree cleanup failure: ${errorText(cleanupError)}`;
				const inputs = outcome?.report.verdicts.map((verdict) => verdict.input)
					?? requestedForCatastrophe(options.inputs);
				outcome = {
					report: reportOf(
						options.base,
						inputs.map((input) => allUndecidable(input, detail)),
						now,
					),
					failedClosed: true,
				};
			}
		}
	}
	return outcome ?? { report: reportOf(options.base, [], now), failedClosed: true };
}

function stableReportEvidence(verdict: Verdict): string {
	return JSON.stringify({ input: verdict.input, class: verdict.class, checks: verdict.checks });
}

export async function appendLedgerRows(path: string, report: ExposureReport): Promise<void> {
	if (report.verdicts.some((verdict) =>
		CHECK_NAMES.some((name) => verdict.checks[name].status === "undecidable")
	)) {
		throw new Error("refusing to record a failed-closed or incomplete classification report");
	}
	const date = report.decidedAt.slice(0, 10);
	const rows = [...report.verdicts]
		.sort((a, b) => a.input.localeCompare(b.input))
		.map((verdict) => {
			const hash = createHash("sha256").update(stableReportEvidence(verdict)).digest("hex");
			const statuses = CHECK_NAMES.map((name) => verdict.checks[name].status);
			return `| ${date} | dependency-exposure | ${report.base}→worktree | ${verdict.input} | ${verdict.class} | ${statuses.join(" | ")} | ${hash} |\n`;
		})
		.join("");
	await appendFile(path, rows, "utf8");
}

class CrashingBuildRunner implements CommandRunner {
	constructor(private readonly delegate: CommandRunner) {}

	async run(
		command: string,
		args: readonly string[],
		options?: Parameters<CommandRunner["run"]>[2],
	): Promise<RunResult> {
		if (command === "bun" && args[0] === "build" && args.some((arg) => arg === "--metafile" || arg.startsWith("--metafile="))) {
			throw new Error("rigged metafile crash");
		}
		return this.delegate.run(command, args, options);
	}
}

export async function selfTest(
	repoRoot = REPO_ROOT,
	options: { readonly runner?: CommandRunner } = {},
): Promise<readonly string[]> {
	const normalRunner = options.runner ?? new SpawnRunner();
	// Never bun install/build in the live workspace: both sides run inside an isolated checkout.
	const isolated = await materializeBase(repoRoot, "HEAD", normalRunner);
	try {
		const normal = await classify({
			base: "HEAD",
			inputs: ["npm:typebox", "npm:@types/bun", "npm:@deps-r2/nonexistent", "tool:bun-runtime"],
			repoRoot: isolated.root,
			runner: normalRunner,
			identity: true,
			now: () => new Date("2026-08-26T00:00:00.000Z"),
		});
		const expected: Readonly<Record<string, ExposureClass>> = {
			"npm:typebox": "S",
			"npm:@types/bun": "E",
			"npm:@deps-r2/nonexistent": "S",
			"tool:bun-runtime": "S",
		};
		const violations: string[] = [];
		for (const [input, expectedClass] of Object.entries(expected)) {
			const verdict = normal.report.verdicts.find((item) => item.input === input);
			if (verdict?.class !== expectedClass) {
				violations.push(`${input}: expected ${expectedClass}, found ${verdict?.class ?? "missing"}`);
			}
			if (verdict?.class === "E" && !CHECK_NAMES.every((name) => verdict.checks[name].status === "pass")) {
				violations.push(`${input}: Class E lacks four passes`);
			}
		}
		const crashed = await classify({
			base: "HEAD",
			inputs: ["npm:@types/bun"],
			repoRoot: isolated.root,
			runner: new CrashingBuildRunner(normalRunner),
			identity: true,
			now: () => new Date("2026-08-26T00:00:00.000Z"),
		});
		const crashVerdict = crashed.report.verdicts[0];
		if (
			crashVerdict?.class !== "S" ||
			crashVerdict.checks.E2.status !== "undecidable" ||
			!crashed.failedClosed
		) {
			violations.push("rigged metafile crash did not turn @types/bun from E to failed-closed S");
		}
		return violations;
	} finally {
		await isolated.cleanup();
	}
}

function parseCli(argv: readonly string[]): CliArgs {
	if (argv.length === 0 || (argv.length === 1 && argv[0] === "--self-test")) {
		return { mode: "self-test", inputs: [] };
	}
	if (argv[0] !== "classify") {
		throw new Error(
			"usage: dependency-exposure.ts [--self-test] | classify --base <ref> --input <spec|auto> [--json <path>] [--record <path>]",
		);
	}
	let base: string | undefined;
	let jsonPath: string | undefined;
	let recordPath: string | undefined;
	const inputs: string[] = [];
	for (let index = 1; index < argv.length; index++) {
		const flag = argv[index];
		const value = argv[index + 1];
		if (!["--base", "--input", "--json", "--record"].includes(flag ?? "") || value === undefined) {
			throw new Error(`invalid or incomplete argument ${flag ?? "<missing>"}`);
		}
		if (flag === "--base") base = value;
		if (flag === "--input") inputs.push(value);
		if (flag === "--json") jsonPath = value;
		if (flag === "--record") recordPath = value;
		index++;
	}
	if (base === undefined) throw new Error("classify requires --base <ref>");
	if (inputs.length === 0) throw new Error("classify requires at least one --input <spec|auto>");
	return { mode: "classify", base, inputs, jsonPath, recordPath };
}

async function main(): Promise<number> {
	let cli: CliArgs;
	try {
		cli = parseCli(Bun.argv.slice(2));
	} catch (error) {
		console.error(errorText(error));
		console.error(SENTINEL_FAILED);
		return 1;
	}
	if (cli.mode === "self-test") {
		const violations = await selfTest();
		if (violations.length === 0) {
			console.log(SENTINEL_OK);
			return 0;
		}
		for (const violation of violations) console.error(violation);
		console.error(SENTINEL_FAILED);
		return 1;
	}
	const result = await classify({ base: cli.base ?? "", inputs: cli.inputs });
	const json = `${JSON.stringify(result.report, null, 2)}\n`;
	if (cli.jsonPath !== undefined) {
		await mkdir(dirname(resolve(cli.jsonPath)), { recursive: true });
		await writeFile(cli.jsonPath, json, "utf8");
	}
	console.log(json.trimEnd());
	let failedClosed = result.failedClosed;
	if (cli.recordPath !== undefined) {
		try {
			await appendLedgerRows(cli.recordPath, result.report);
		} catch (error) {
			console.error(`record failed: ${errorText(error)}`);
			failedClosed = true;
		}
	}
	if (failedClosed) console.error(SENTINEL_FAILED);
	return failedClosed ? 1 : 0;
}

if (import.meta.main) {
	process.exitCode = await main();
}
