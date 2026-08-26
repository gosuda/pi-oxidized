import { dirname } from "node:path";

import { decodeZipArchive, sha256Bytes } from "./archive.ts";
import { realFs, safeJoinPath, type Fs } from "./runner.ts";
import type { RustTarget, TargetPlan } from "./targets.ts";

/** Bun version embedded in runtime-bundle release archives. */
export const BUN_RUNTIME_VERSION = "1.3.14";

/** A pinned official Bun release asset for one Rust release target. */
export interface BunRuntimeAsset {
	readonly bunTarget: string;
	readonly fileName: string;
	readonly sha256: string;
	readonly runtimeMember: string;
	readonly url: string;
}

interface AssetPin {
	readonly bunTarget: string;
	readonly fileName: string;
	readonly sha256: string;
}

const ASSET_PINS: Readonly<Record<RustTarget, AssetPin>> = {
	"x86_64-unknown-linux-gnu": {
		bunTarget: "bun-linux-x64-baseline",
		fileName: "bun-linux-x64-baseline.zip",
		sha256: "a063908ae08b7852ca10939bbdc6ceed3ddabce8fb9402dce83d65d73b36e6c7",
	},
	"x86_64-unknown-linux-musl": {
		bunTarget: "bun-linux-x64-musl-baseline",
		fileName: "bun-linux-x64-musl-baseline.zip",
		sha256: "56a7d6806cf155536c0178f0ea5fbd098e684fa509ebdb4fc0a7e19fb65382dc",
	},
	"aarch64-unknown-linux-gnu": {
		bunTarget: "bun-linux-arm64",
		fileName: "bun-linux-aarch64.zip",
		sha256: "a27ffb63a8310375836e0d6f668ae17fa8d8d18b88c37c821c65331973a19a3b",
	},
	"aarch64-unknown-linux-musl": {
		bunTarget: "bun-linux-arm64-musl",
		fileName: "bun-linux-aarch64-musl.zip",
		sha256: "b98e0ad3625c5c00d1d5b5ff55605c7adddbfae151861e68ade57b2d3b8703bb",
	},
	"x86_64-apple-darwin": {
		bunTarget: "bun-darwin-x64-baseline",
		fileName: "bun-darwin-x64-baseline.zip",
		sha256: "3e35ad6f53971a9834bf9e6786e2adf72b5f1921cc9a9c5fde073d2972944076",
	},
	"aarch64-apple-darwin": {
		bunTarget: "bun-darwin-arm64",
		fileName: "bun-darwin-aarch64.zip",
		sha256: "d8b96221828ad6f97ac7ac0ab7e95872341af763001e8803e8267652c2652620",
	},
	"x86_64-pc-windows-msvc": {
		bunTarget: "bun-windows-x64-baseline",
		fileName: "bun-windows-x64-baseline.zip",
		sha256: "538f9c846355d9e847b2671bc00c47da4229a0befb24df3282b739770f3b475f",
	},
};

/** Resolve and validate the official runtime asset matching a target plan. */
export function bunRuntimeAsset(plan: TargetPlan): BunRuntimeAsset {
	const pin = ASSET_PINS[plan.rustTarget];
	if (pin === undefined) {
		throw new BunRuntimeProvisionError(`no Bun runtime pin for ${plan.rustTarget}`);
	}
	if (pin.bunTarget !== plan.bunTarget) {
		throw new BunRuntimeProvisionError(
			`Bun target mismatch for ${plan.rustTarget}: plan=${plan.bunTarget}, pin=${pin.bunTarget}`,
		);
	}
	const stem = pin.fileName.slice(0, -".zip".length);
	return {
		...pin,
		runtimeMember: `${stem}/${plan.bunRuntimeName}`,
		url: `https://github.com/oven-sh/bun/releases/download/bun-v${BUN_RUNTIME_VERSION}/${pin.fileName}`,
	};
}

/** Minimal fetch response seam used by focused tests. */
export interface RuntimeFetchResponse {
	readonly ok: boolean;
	readonly status: number;
	arrayBuffer(): Promise<ArrayBuffer>;
}

/** Fetch seam for the pinned runtime archive. */
export type RuntimeFetcher = (url: string) => Promise<RuntimeFetchResponse>;

export interface ProvisionBunRuntimeOptions {
	readonly plan: TargetPlan;
	readonly destination: string;
	/**
	 * Offline pre-cache directory consulted BEFORE any fetch: the pinned
	 * archive is read from `safeJoinPath(cacheDir, asset.fileName)`, and an
	 * absent or unreadable entry falls through to the network. When set, any
	 * fetch failure (non-OK response or a throwing fetcher) is wrapped into a
	 * {@link BunRuntimeProvisionError} naming both the expected cache path and
	 * the asset filename. Undefined keeps the online path unchanged.
	 *
	 * Accepted Windows cache contract (narrow; recorded in
	 * https://github.com/metaphorics/pi-oxidized/issues/110#issuecomment-5426692845):
	 * every cache byte is checksum-verified against the pinned sha256 before
	 * extraction, so a path rebind can only yield the pinned bytes or the
	 * identical `checksum mismatch` rejection; no native no-reparse handle is
	 * taken.
	 */
	readonly cacheDir?: string;
	/**
	 * Checksum seam for focused tests; defaults to {@link sha256Bytes}. Needed
	 * because the pinned official archive bytes are unforgeable offline.
	 */
	readonly digest?: (bytes: Uint8Array) => string;
	readonly fs?: Fs;
	readonly fetcher?: RuntimeFetcher;
}

/**
 * Verify, extract, and write one runtime archive. Cache hits and downloads
 * share this path so corrupted bytes fail byte-identically at either source.
 */
async function installRuntimeArchive(
	archive: Uint8Array,
	asset: BunRuntimeAsset,
	options: ProvisionBunRuntimeOptions,
	fs: Fs,
	digest: (bytes: Uint8Array) => string,
): Promise<string> {
	const actualSha256 = digest(archive);
	if (actualSha256 !== asset.sha256) {
		throw new BunRuntimeProvisionError(
			`checksum mismatch for ${asset.fileName}: expected ${asset.sha256}, got ${actualSha256}`,
		);
	}
	const runtime = decodeZipArchive(archive).find(
		(entry) => entry.path === asset.runtimeMember,
	);
	if (runtime === undefined || runtime.data.length === 0) {
		throw new BunRuntimeProvisionError(
			`official archive ${asset.fileName} is missing ${asset.runtimeMember}`,
		);
	}

	await fs.mkdir(dirname(options.destination), { recursive: true });
	await fs.writeFile(options.destination, runtime.data);
	if (!options.plan.windows) await fs.chmod(options.destination, 0o755);
	return options.destination;
}

/** Download, checksum, and extract the target-matching Bun executable. */
export async function provisionBunRuntime(
	options: ProvisionBunRuntimeOptions,
): Promise<string> {
	const asset = bunRuntimeAsset(options.plan);
	const fetcher = options.fetcher ?? ((url: string) => fetch(url));
	const digest = options.digest ?? sha256Bytes;
	const fs = options.fs ?? realFs;

	if (options.cacheDir !== undefined) {
		const cachePath = safeJoinPath(options.cacheDir, asset.fileName);
		let cached: Uint8Array | undefined;
		try {
			cached = await fs.readFile(cachePath);
		} catch {
			// Absent or unreadable cache entry: fall through to the fetch.
		}
		if (cached !== undefined) {
			return await installRuntimeArchive(cached, asset, options, fs, digest);
		}
	}

	let archive: Uint8Array;
	try {
		const response = await fetcher(asset.url);
		if (!response.ok) {
			throw new BunRuntimeProvisionError(
				`failed to download ${asset.url}: HTTP ${response.status}`,
			);
		}
		archive = new Uint8Array(await response.arrayBuffer());
	} catch (error) {
		if (options.cacheDir === undefined) throw error;
		const cachePath = safeJoinPath(options.cacheDir, asset.fileName);
		const reason = error instanceof Error ? error.message : String(error);
		throw new BunRuntimeProvisionError(
			`failed to fetch ${asset.fileName}: ${reason}; no usable offline cache entry at ${cachePath}`,
		);
	}
	return await installRuntimeArchive(archive, asset, options, fs, digest);
}

/** Failure while selecting, downloading, or verifying a bundled Bun runtime. */
export class BunRuntimeProvisionError extends Error {
	constructor(message: string) {
		super(message);
		this.name = "BunRuntimeProvisionError";
	}
}
