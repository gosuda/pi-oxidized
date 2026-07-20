/**
 * Reconstruct the reference tree's gitignored provider data JSONs
 * (`.references/pi/packages/ai/src/providers/data/*.json`) from the committed
 * Rust catalog (`crates/pi-ai/data/builtin-models.json`).
 *
 * Upstream produces these files with a network-fetching generator; this repo
 * forbids network generation, and the committed catalog is the proven-exact
 * offline inverse (regenerating `builtin-models.json` from the reconstructed
 * files yields a zero diff). Harnesses that build the reference TypeScript pi
 * run this first so `generate-models` never has to run.
 */
import { mkdir, readdir, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const REPO_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const CATALOG_PATH = join(REPO_ROOT, "crates/pi-ai/data/builtin-models.json");
const PROVIDERS_DIR = join(REPO_ROOT, ".references/pi/packages/ai/src/providers");
const DATA_DIR = join(PROVIDERS_DIR, "data");

function sortDeep(value: unknown): unknown {
	if (Array.isArray(value)) return value.map(sortDeep);
	if (value === null || typeof value !== "object") return value;
	const sorted: Record<string, unknown> = {};
	for (const key of Object.keys(value as Record<string, unknown>).sort()) {
		sorted[key] = sortDeep((value as Record<string, unknown>)[key]);
	}
	return sorted;
}

async function main(): Promise<void> {
	const catalog = (await Bun.file(CATALOG_PATH).json()) as Record<
		string,
		Record<string, unknown>
	>;
	const wrappers = (await readdir(PROVIDERS_DIR)).filter((name) =>
		name.endsWith(".models.ts"),
	);
	await mkdir(DATA_DIR, { recursive: true });

	let written = 0;
	const missing: string[] = [];
	for (const wrapper of wrappers) {
		const provider = wrapper.replace(/\.models\.ts$/, "");
		const models = catalog[provider];
		if (models === undefined) {
			missing.push(provider);
			continue;
		}
		const body = `${JSON.stringify(sortDeep(models), null, "\t")}\n`;
		await writeFile(join(DATA_DIR, `${provider}.json`), body, "utf8");
		written += 1;
	}

	if (missing.length > 0) {
		throw new Error(
			`catalog has no models for wrapper providers: ${missing.join(", ")}`,
		);
	}
	console.warn(`reconstructed ${written} provider data files from the catalog`);

	// Fail-fast inversion proof: regenerating the catalog from the
	// reconstructed files must reproduce the exact catalog bytes we read.
	// Snapshot/restore keeps the generator-owned artifact untouched on every
	// exit path, no matter how the regeneration ends.
	const before = await Bun.file(CATALOG_PATH).bytes();
	try {
		const regen = Bun.spawnSync(
			["bun", "run", join(REPO_ROOT, "scripts/generate-builtin-models.ts")],
			{ cwd: REPO_ROOT, stdout: "ignore", stderr: "pipe" },
		);
		if (regen.exitCode !== 0) {
			throw new Error(
				`inversion proof failed: generate-builtin-models exited ${regen.exitCode}: ${regen.stderr.toString().slice(0, 400)}`,
			);
		}
		const after = await Bun.file(CATALOG_PATH).bytes();
		if (Buffer.compare(Buffer.from(before), Buffer.from(after)) !== 0) {
			throw new Error(
				"inversion proof failed: regenerated builtin-models.json differs from the catalog the reconstruction used",
			);
		}
	} finally {
		await writeFile(CATALOG_PATH, before);
	}
	console.warn("inversion proof passed: catalog round-trips exactly");
}

await main();
