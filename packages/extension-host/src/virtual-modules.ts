/**
 * Virtual module resolution for extension loading via jiti.
 *
 * Mirrors the reference loader's `getAliases()` / `VIRTUAL_MODULES` so every
 * specifier an extension can import resolves to the same reference package
 * source. Legacy `@mariozechner/*` names alias to their `@earendil-works/*`
 * counterparts. `@sinclair/typebox*` aliases to modern `typebox*`.
 *
 * No package registry is contacted at runtime — all resolution is local file
 * paths to the pinned reference source.
 */

import { createJiti, type Jiti } from "jiti/static";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";

const __dirname = dirname(fileURLToPath(import.meta.url));

/**
 * Reference root resolution.
 *
 * Source-mode (`bun test` / `bun run fixtures/...`): resolve relative to this
 * file's directory — `../../../.references/pi/packages/`.
 *
 * Compiled-binary mode (`./dist/pi-extension-host`): `import.meta.url` points
 * to a virtual `/$bunfs/root` filesystem, so module-relative paths fail.
 * Fall back to `process.cwd()` — the shipped topology places the binary in
 * `packages/extension-host/dist/` and the reference at `../../.references/`.
 * When invoked from that directory the cwd-based path resolves correctly.
 */
const REF_ROOT = (() => {
	const fromModule = resolve(__dirname, "..", "..", "..", ".references", "pi", "packages");
	if (__dirname.startsWith("/$bunfs") || __dirname === "/") {
		return resolve(process.cwd(), "..", "..", ".references", "pi", "packages");
	}
	return fromModule;
})();

/** Map every extension-importable specifier to reference source. */
export function getExtensionAliases(): Record<string, string> {
	const codingAgent = `${REF_ROOT}/coding-agent/src/index.ts`;
	const agent = `${REF_ROOT}/agent/src/index.ts`;
	const tui = `${REF_ROOT}/tui/src/index.ts`;
	const aiCompat = `${REF_ROOT}/ai/src/compat.ts`;
	const aiOauth = `${REF_ROOT}/ai/src/oauth.ts`;
	const aiProviders = `${REF_ROOT}/ai/src/providers/all.ts`;
	// The reference loader resolves typebox via require.resolve from the
	// coding-agent package; bun hoists that copy to the workspace root, so
	// mirror the hoisted path and share one typebox instance with the
	// reference extension machinery.
	const typeboxRoot = `${REF_ROOT}/../node_modules/typebox/build`;
	const typebox = `${typeboxRoot}/index.mjs`;
	const typeboxCompile = `${typeboxRoot}/compile/index.mjs`;
	const typeboxValue = `${typeboxRoot}/value/index.mjs`;

	return {
		// Modern names → reference source (extensions subsystem, not full pkg).
		"@earendil-works/pi-coding-agent": codingAgent,
		"@earendil-works/pi-agent-core": agent,
		"@earendil-works/pi-tui": tui,
		"@earendil-works/pi-ai": aiCompat,
		"@earendil-works/pi-ai/compat": aiCompat,
		"@earendil-works/pi-ai/oauth": aiOauth,
		"@earendil-works/pi-ai/providers/all": aiProviders,

		// Legacy names → same sources.
		"@mariozechner/pi-coding-agent": codingAgent,
		"@mariozechner/pi-agent-core": agent,
		"@mariozechner/pi-tui": tui,
		"@mariozechner/pi-ai": aiCompat,
		"@mariozechner/pi-ai/compat": aiCompat,
		"@mariozechner/pi-ai/oauth": aiOauth,
		"@mariozechner/pi-ai/providers/all": aiProviders,

		// Typebox (mirrors reference loader VIRTUAL_MODULES / getAliases).
		typebox,
		"typebox/compile": typeboxCompile,
		"typebox/value": typeboxValue,
		"@sinclair/typebox": typebox,
		"@sinclair/typebox/compile": typeboxCompile,
		"@sinclair/typebox/value": typeboxValue,
	};
}

/**
 * Create a jiti instance configured for loading TypeScript extensions.
 *
 * Extensions import `@earendil-works/*` / `@mariozechner/*` specifiers which
 * are aliased to the reference source. `moduleCache: false` ensures each load
 * gets a fresh module evaluation (matching the reference loader).
 */
export function createExtensionJiti(): Jiti {
	return createJiti(__dirname, {
		moduleCache: false,
		alias: getExtensionAliases(),
	});
}