/**
 * Deterministic archive writers and checksum helpers.
 *
 * Both tar.gz and zip archives are byte-for-byte reproducible:
 *   - entry order is lexicographic by archive-relative path,
 *   - timestamps are clamped to SOURCE_DATE_EPOCH (defaults to 0),
 *   - ownership is numeric uid=0 / gid=0 with empty uname/gname (tar) or
 *     zero-modtime fields with no extra fields (zip),
 *   - gzip / deflate compression is invoked with fixed options.
 *
 * No external dependencies; only `node:crypto`, `node:fs/promises`, and the
 * Bun built-ins (`Bun.gzipSync`, `Bun.deflateSync`) are used.
 */

import { createHash } from "node:crypto";
import { chmod, mkdir, writeFile, readFile } from "node:fs/promises";
import { dirname, join } from "node:path";

/** A single byte stream to pack into an archive, with its in-archive path. */
export interface ArchiveEntry {
	/**
	 * POSIX-style path relative to the archive root (use `/` separators).
	 * Must not start with `/` or contain `..` segments — callers validate this
	 * via {@link safeRelativePath} before reaching the writer.
	 */
	readonly path: string;
	/** File bytes. */
	readonly data: Uint8Array;
	/** Unix permission bits for tar entries (zip ignores this). */
	readonly mode: number;
}

/** Options accepted by both archive writers. */
export interface ArchiveOptions {
	/**
	 * Non-negative integer seconds since the Unix epoch used as the mtime for
	 * every entry. Defaults to 0 for reproducibility.
	 */
	readonly sourceDateEpoch: number;
	/**
	 * Gzip compression level (0–9). Defaults to 9.
	 */
	readonly gzipLevel?: number;
}

/** Default gzip compression level (best ratio; deterministic for fixed input). */
const DEFAULT_GZIP_LEVEL = 9;

/** Maximum USTAR mtime: 11 octal digits (plus NUL) = 0o77777777777. */
const USTAR_MAX_MTIME_SECONDS = 0o77777777777;

/**
 * Write a deterministic `.tar.gz` archive.
 *
 * @param entries  archive contents; the writer sorts a copy by path.
 * @param outPath  destination `.tar.gz` path. Parent directories are created.
 */
export async function writeTarGz(
	entries: readonly ArchiveEntry[],
	outPath: string,
	options: ArchiveOptions,
): Promise<void> {
	const sorted = sortByPath(entries);
	const tarBytes = encodeTarArchive(sorted, clampMtime(options.sourceDateEpoch));
	const gzBytes = gzipDeterministic(tarBytes, options.gzipLevel ?? DEFAULT_GZIP_LEVEL);
	await mkdir(dirname(outPath), { recursive: true });
	await writeFile(outPath, gzBytes);
}

/**
 * Write a deterministic `.zip` archive (deflate compression).
 *
 * @param entries  archive contents; the writer sorts a copy by path.
 * @param outPath  destination `.zip` path. Parent directories are created.
 */
export async function writeZip(
	entries: readonly ArchiveEntry[],
	outPath: string,
	options: ArchiveOptions,
): Promise<void> {
	const sorted = sortByPath(entries);
	const zipBytes = encodeZipArchive(sorted, clampMtime(options.sourceDateEpoch));
	await mkdir(dirname(outPath), { recursive: true });
	await writeFile(outPath, zipBytes);
}

/** Clamp mtime into the USTAR representable range; values above are
 * saturated (we want determinism, not a runtime error). */
function clampMtime(seconds: number): number {
	if (!Number.isFinite(seconds) || seconds < 0) return 0;
	if (seconds > USTAR_MAX_MTIME_SECONDS) return USTAR_MAX_MTIME_SECONDS;
	return Math.floor(seconds);
}

/** Sort entries by archive path. Returns a new array; input is untouched. */
function sortByPath(entries: readonly ArchiveEntry[]): ArchiveEntry[] {
	return [...entries].sort((a, b) =>
		a.path < b.path ? -1 : a.path > b.path ? 1 : 0,
	);
}

/**
 * Sanitize and validate an in-archive path. Rejects absolute paths, `..`
 * segments, backslashes (callers must pass POSIX-style), empty segments,
 * and trailing slashes.
 *
 * @throws {@link TraversalError} on any violation.
 */
export function safeRelativePath(input: string): string {
	if (input.length === 0) {
		throw new TraversalError("archive path is empty");
	}
	if (input.includes("\0")) {
		throw new TraversalError(`archive path contains a null byte: ${input}`);
	}
	if (input.includes("\\")) {
		throw new TraversalError(`archive path contains backslash: ${input}`);
	}
	if (input.startsWith("/")) {
		throw new TraversalError(`archive path is absolute: ${input}`);
	}
	const segments = input.split("/");
	for (const seg of segments) {
		if (seg.length === 0) {
			throw new TraversalError(`archive path has empty segment: ${input}`);
		}
		if (seg === "." || seg === "..") {
			throw new TraversalError(`archive path escapes root: ${input}`);
		}
		if (seg.includes(":")) {
			throw new TraversalError(`archive path contains a drive separator: ${input}`);
		}
	}
	return input;
}

/** Error raised by {@link safeRelativePath} on path-traversal violations. */
export class TraversalError extends Error {
	constructor(message: string) {
		super(message);
		this.name = "TraversalError";
	}
}

// ─────────────────────────────────────────────────────────────────────────────
// Tar (USTAR) writer
// ─────────────────────────────────────────────────────────────────────────────

/** Size of a tar header block, in bytes. */
const TAR_BLOCK_SIZE = 512;

/**
 * Encode a deterministic USTAR tar archive (no compression). Entries are
 * already sorted by path.
 */
function encodeTarArchive(entries: readonly ArchiveEntry[], mtimeSeconds: number): Uint8Array {
	const chunks: Uint8Array[] = [];
	for (const entry of entries) {
		chunks.push(encodeTarHeader(entry, mtimeSeconds));
		chunks.push(padToBlock(entry.data));
	}
	// Two zero blocks mark end of archive.
	chunks.push(new Uint8Array(TAR_BLOCK_SIZE * 2));
	return concatBytes(chunks);
}

/** Build the 512-byte USTAR header for one entry. */
function encodeTarHeader(entry: ArchiveEntry, mtimeSeconds: number): Uint8Array {
	const path = safeRelativePath(entry.path);
	const header = new Uint8Array(TAR_BLOCK_SIZE);
	const data = entry.data;
	// Split a too-long name across the USTAR `prefix`/`name` fields.
	let nameField = path;
	let prefixField = "";
	if (path.length > 100) {
		const split = path.indexOf("/", path.length - 100);
		if (split === -1 || split === 0) {
			throw new TraversalError(`tar entry name too long to encode: ${path}`);
		}
		prefixField = path.slice(0, split);
		nameField = path.slice(split + 1);
		if (nameField.length > 100 || prefixField.length > 155) {
			throw new TraversalError(`tar entry name too long to encode: ${path}`);
		}
	}
	writeStringField(header, 0, 100, nameField);
	writeOctal(header, 100, 8, 0o555 & entry.mode);
	writeOctal(header, 108, 8, 0); // uid
	writeOctal(header, 116, 8, 0); // gid
	writeOctal(header, 124, 12, BigInt(data.length));
	writeOctal(header, 136, 12, BigInt(mtimeSeconds));
	// Checksum placeholder: 8 spaces.
	for (let i = 148; i < 156; i++) header[i] = 0x20;
	header[156] = 0x30; // typeflag '0' = regular file
	writeStringField(header, 157, 100, ""); // linkname
	writeStringField(header, 257, 6, "ustar");
	header[263] = 0x30; // version "00"
	header[264] = 0x30;
	writeStringField(header, 265, 32, ""); // uname
	writeStringField(header, 297, 32, ""); // gname
	writeOctal(header, 329, 8, 0); // devmajor
	writeOctal(header, 337, 8, 0); // devminor
	writeStringField(header, 345, 155, prefixField);
	// Compute and write checksum.
	let checksum = 0;
	for (let i = 0; i < TAR_BLOCK_SIZE; i++) checksum += header[i] ?? 0;
	writeOctal(header, 148, 7, BigInt(checksum));
	header[155] = 0x20; // trailing space
	return header;
}

/** Pad a byte buffer up to the next 512-byte boundary. */
function padToBlock(data: Uint8Array): Uint8Array {
	const remainder = data.length % TAR_BLOCK_SIZE;
	if (remainder === 0) return data;
	const padded = new Uint8Array(data.length + (TAR_BLOCK_SIZE - remainder));
	padded.set(data, 0);
	return padded;
}

/** Write a fixed-length ASCII string field, NUL-padded. */
function writeStringField(buf: Uint8Array, offset: number, len: number, value: string): void {
	const bytes = Buffer.from(value, "ascii");
	const max = Math.min(bytes.length, len - 1);
	for (let i = 0; i < max; i++) buf[offset + i] = bytes[i] ?? 0;
	buf[offset + max] = 0;
}

/** Write an unsigned octal value with a trailing NUL. */
function writeOctal(buf: Uint8Array, offset: number, len: number, value: bigint | number): void {
	const big = typeof value === "bigint" ? value : BigInt(value);
	const octal = big.toString(8);
	const digitLen = octal.length;
	// Field layout: `len - 1` digits then a NUL terminator.
	if (digitLen > len - 1) {
		throw new Error(`octal field overflow: value=${octal} len=${len}`);
	}
	for (let i = 0; i < digitLen; i++) buf[offset + i] = octal.charCodeAt(i);
	buf[offset + digitLen] = 0;
}

/**
 * Gzip-encode bytes deterministically. Bun's `gzipSync` is deterministic given
 * the same input and options (level + memLevel + strategy). The header
 * timestamp is forced to 0 by passing `{ level }` only and then rewriting the
 * MTIME field at offset 4..8.
 */
function gzipDeterministic(bytes: Uint8Array, level: number): Uint8Array {
	const gz = Bun.gzipSync(Buffer.from(bytes), { level: level as 9 });
	// Force MTIME (bytes 4–7) to 0 for byte-stable output across runs.
	gz[4] = 0;
	gz[5] = 0;
	gz[6] = 0;
	gz[7] = 0;
	return new Uint8Array(gz.buffer, gz.byteOffset, gz.byteLength);
}

// ─────────────────────────────────────────────────────────────────────────────
// Zip writer
// ─────────────────────────────────────────────────────────────────────────────

/** Encode a deterministic zip archive. Entries are already sorted by path. */
function encodeZipArchive(entries: readonly ArchiveEntry[], mtimeSeconds: number): Uint8Array {
	const dos = dosDateTime(mtimeSeconds);
	const fileRecords: Uint8Array[] = [];
	const centralRecords: Uint8Array[] = [];
	let offset = 0;
	for (const entry of entries) {
		const path = safeRelativePath(entry.path);
		const nameBytes = Buffer.from(path, "utf8");
		const crc = crc32(entry.data);
		const compressedRaw = Bun.deflateSync(Buffer.from(entry.data), { level: 9 });
		const compressed = new Uint8Array(
			compressedRaw.buffer,
			compressedRaw.byteOffset,
			compressedRaw.byteLength,
		);
		// Use deflate only when it actually shrinks; otherwise store.
		const useDeflate = compressed.length < entry.data.length;
		const payload = useDeflate ? compressed : entry.data;
		const method = useDeflate ? 8 : 0;

		const localHeader = new Uint8Array(30 + nameBytes.length);
		const dv = new DataView(localHeader.buffer);
		dv.setUint32(0, 0x04034b50, true); // signature
		dv.setUint16(4, 20, true); // version needed
		dv.setUint16(6, 0, true); // general purpose flag
		dv.setUint16(8, method, true); // compression method
		dv.setUint16(10, dos.time, true); // mod time
		dv.setUint16(12, dos.date, true); // mod date
		dv.setUint32(14, crc >>> 0, true); // crc32
		dv.setUint32(18, payload.length, true); // compressed size
		dv.setUint32(22, entry.data.length, true); // uncompressed size
		dv.setUint16(26, nameBytes.length, true); // filename length
		dv.setUint16(28, 0, true); // extra field length (none, for determinism)
		localHeader.set(nameBytes, 30);

		const centralHeader = new Uint8Array(46 + nameBytes.length);
		const cv = new DataView(centralHeader.buffer);
		cv.setUint32(0, 0x02014b50, true); // central signature
		cv.setUint16(4, 20, true); // version made by
		cv.setUint16(6, 20, true); // version needed
		cv.setUint16(8, 0, true); // general purpose flag
		cv.setUint16(10, method, true); // compression method
		cv.setUint16(12, dos.time, true); // mod time
		cv.setUint16(14, dos.date, true); // mod date
		cv.setUint32(16, crc >>> 0, true); // crc32
		cv.setUint32(20, payload.length, true); // compressed size
		cv.setUint32(24, entry.data.length, true); // uncompressed size
		cv.setUint16(28, nameBytes.length, true); // filename length
		cv.setUint16(30, 0, true); // extra field length (none)
		cv.setUint16(32, 0, true); // comment length
		cv.setUint16(34, 0, true); // disk number
		cv.setUint16(36, 0, true); // internal attrs
		cv.setUint32(38, 0, true); // external attrs
		cv.setUint32(42, offset, true); // local header offset
		centralHeader.set(nameBytes, 46);

		fileRecords.push(localHeader, payload);
		centralRecords.push(centralHeader);
		offset += localHeader.length + payload.length;
	}

	const centralStart = offset;
	const centralBytes = concatBytes(centralRecords);

	const eocd = new Uint8Array(22);
	const ev = new DataView(eocd.buffer);
	ev.setUint32(0, 0x06054b50, true); // EOCD signature
	ev.setUint16(4, 0, true); // disk number
	ev.setUint16(6, 0, true); // disk with central dir
	ev.setUint16(8, entries.length, true); // entries on this disk
	ev.setUint16(10, entries.length, true); // total entries
	ev.setUint32(12, centralBytes.length, true); // central dir size
	ev.setUint32(16, centralStart, true); // central dir offset
	ev.setUint16(20, 0, true); // comment length

	return concatBytes([...fileRecords, centralBytes, eocd]);
}

const ZIP_LOCAL_SIGNATURE = 0x04034b50;
const ZIP_CENTRAL_SIGNATURE = 0x02014b50;
const ZIP_EOCD_SIGNATURE = 0x06054b50;
const ZIP_MAX_ENTRIES = 10_000;
const ZIP_MAX_UNCOMPRESSED_BYTES = 512 * 1024 * 1024;

/** Decode regular files from a standard, non-encrypted ZIP archive. */
export function decodeZipArchive(bytes: Uint8Array): ArchiveEntry[] {
	const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
	const eocdOffset = findZipEocd(view);
	const disk = view.getUint16(eocdOffset + 4, true);
	const centralDisk = view.getUint16(eocdOffset + 6, true);
	const entriesOnDisk = view.getUint16(eocdOffset + 8, true);
	const entryCount = view.getUint16(eocdOffset + 10, true);
	const centralSize = view.getUint32(eocdOffset + 12, true);
	const centralOffset = view.getUint32(eocdOffset + 16, true);
	if (disk !== 0 || centralDisk !== 0 || entriesOnDisk !== entryCount) {
		throw new Error("multi-disk ZIP archives are not supported");
	}
	if (entryCount === 0xffff || centralSize === 0xffffffff || centralOffset === 0xffffffff) {
		throw new Error("ZIP64 archives are not supported");
	}
	if (entryCount > ZIP_MAX_ENTRIES) {
		throw new Error(`ZIP archive has too many entries: ${entryCount}`);
	}
	requireZipRange(bytes, centralOffset, centralSize, "central directory");

	const decoded: ArchiveEntry[] = [];
	const seen = new Set<string>();
	let totalSize = 0;
	let offset = centralOffset;
	for (let index = 0; index < entryCount; index++) {
		requireZipRange(bytes, offset, 46, `central entry ${index}`);
		if (view.getUint32(offset, true) !== ZIP_CENTRAL_SIGNATURE) {
			throw new Error(`invalid ZIP central entry signature at ${offset}`);
		}
		const flags = view.getUint16(offset + 8, true);
		const method = view.getUint16(offset + 10, true);
		const expectedCrc = view.getUint32(offset + 16, true);
		const compressedSize = view.getUint32(offset + 20, true);
		const uncompressedSize = view.getUint32(offset + 24, true);
		const nameLength = view.getUint16(offset + 28, true);
		const extraLength = view.getUint16(offset + 30, true);
		const commentLength = view.getUint16(offset + 32, true);
		const externalAttrs = view.getUint32(offset + 38, true);
		const localOffset = view.getUint32(offset + 42, true);
		const recordLength = 46 + nameLength + extraLength + commentLength;
		requireZipRange(bytes, offset, recordLength, `central entry ${index}`);
		if ((flags & 1) !== 0) throw new Error("encrypted ZIP entries are not supported");
		if (method !== 0 && method !== 8) {
			throw new Error(`unsupported ZIP compression method ${method}`);
		}
		const rawName = new TextDecoder("utf-8", { fatal: true }).decode(
			bytes.subarray(offset + 46, offset + 46 + nameLength),
		);
		const isDirectory = rawName.endsWith("/");
		const path = safeRelativePath(isDirectory ? rawName.slice(0, -1) : rawName);
		if (seen.has(path)) throw new Error(`duplicate ZIP entry: ${path}`);
		seen.add(path);
		offset += recordLength;
		if (isDirectory) continue;

		totalSize += uncompressedSize;
		if (totalSize > ZIP_MAX_UNCOMPRESSED_BYTES) {
			throw new Error("ZIP archive exceeds the uncompressed size limit");
		}
		requireZipRange(bytes, localOffset, 30, `local entry ${path}`);
		if (view.getUint32(localOffset, true) !== ZIP_LOCAL_SIGNATURE) {
			throw new Error(`invalid ZIP local entry signature for ${path}`);
		}
		const localNameLength = view.getUint16(localOffset + 26, true);
		const localExtraLength = view.getUint16(localOffset + 28, true);
		const payloadOffset = localOffset + 30 + localNameLength + localExtraLength;
		requireZipRange(bytes, payloadOffset, compressedSize, `payload for ${path}`);
		const payload = bytes.subarray(payloadOffset, payloadOffset + compressedSize);
		const inflatedRaw = method === 0 ? payload : Bun.inflateSync(Buffer.from(payload));
		const inflated = new Uint8Array(
			inflatedRaw.buffer,
			inflatedRaw.byteOffset,
			inflatedRaw.byteLength,
		);
		if (inflated.length !== uncompressedSize) {
			throw new Error(`ZIP size mismatch for ${path}`);
		}
		if (crc32(inflated) !== expectedCrc) {
			throw new Error(`ZIP checksum mismatch for ${path}`);
		}
		const unixMode = (externalAttrs >>> 16) & 0xffff;
		decoded.push({ path, data: inflated, mode: unixMode === 0 ? 0o644 : unixMode & 0o777 });
	}
	if (offset !== centralOffset + centralSize) {
		throw new Error("ZIP central directory size mismatch");
	}
	return decoded;
}

/** Extract a ZIP archive without relying on a host `unzip` executable. */
export async function extractZip(archivePath: string, outDir: string): Promise<void> {
	const entries = decodeZipArchive(await readBytes(archivePath));
	for (const entry of entries) {
		const destination = join(outDir, ...entry.path.split("/"));
		await mkdir(dirname(destination), { recursive: true });
		await writeFile(destination, entry.data);
		if (process.platform !== "win32") await chmod(destination, entry.mode);
	}
}

/** List regular-file members from a gzip-compressed USTAR archive. */
export function listTarGzEntries(bytes: Uint8Array): string[] {
	const tarRaw = Bun.gunzipSync(Buffer.from(bytes));
	const tar = new Uint8Array(tarRaw.buffer, tarRaw.byteOffset, tarRaw.byteLength);
	const paths: string[] = [];
	let offset = 0;
	while (offset + TAR_BLOCK_SIZE <= tar.length) {
		const header = tar.subarray(offset, offset + TAR_BLOCK_SIZE);
		if (header.every((byte) => byte === 0)) break;
		const name = readTarString(header, 0, 100);
		const prefix = readTarString(header, 345, 155);
		const path = safeRelativePath(prefix.length === 0 ? name : `${prefix}/${name}`);
		const sizeRaw = readTarString(header, 124, 12).trim();
		const size = sizeRaw.length === 0 ? 0 : Number.parseInt(sizeRaw, 8);
		if (!Number.isSafeInteger(size) || size < 0) {
			throw new Error(`invalid tar size for ${path}`);
		}
		const payloadEnd = offset + TAR_BLOCK_SIZE + size;
		if (payloadEnd > tar.length) throw new Error(`truncated tar entry: ${path}`);
		const type = header[156] ?? 0;
		if (type === 0 || type === 0x30) paths.push(path);
		offset = offset + TAR_BLOCK_SIZE + Math.ceil(size / TAR_BLOCK_SIZE) * TAR_BLOCK_SIZE;
	}
	return paths;
}

function findZipEocd(view: DataView): number {
	const minimum = 22;
	if (view.byteLength < minimum) throw new Error("truncated ZIP archive");
	const lower = Math.max(0, view.byteLength - minimum - 0xffff);
	for (let offset = view.byteLength - minimum; offset >= lower; offset--) {
		if (
			view.getUint32(offset, true) === ZIP_EOCD_SIGNATURE &&
			offset + minimum + view.getUint16(offset + 20, true) === view.byteLength
		) {
			return offset;
		}
	}
	throw new Error("ZIP end-of-central-directory record not found");
}

function requireZipRange(
	bytes: Uint8Array,
	offset: number,
	length: number,
	label: string,
): void {
	if (offset < 0 || length < 0 || offset > bytes.length - length) {
		throw new Error(`truncated ZIP ${label}`);
	}
}

function readTarString(bytes: Uint8Array, offset: number, length: number): string {
	const field = bytes.subarray(offset, offset + length);
	const end = field.indexOf(0);
	return Buffer.from(end === -1 ? field : field.subarray(0, end)).toString("ascii");
}


/** DOS date/time encoding (seconds granularity, UTC).
 *
 * DOS dates are 7 bits for year (1980-2107 inclusive), so any epoch before
 * 1980 is clamped to the DOS epoch (1980-01-01 00:00:00). The zip DOS time
 * field only has two-second resolution.
 */
function dosDateTime(epochSeconds: number): { time: number; date: number } {
	const epoch = epochSeconds < 0 ? 0 : epochSeconds;
	const d = new Date(epoch * 1000);
	let year = d.getUTCFullYear() - 1980;
	if (year < 0) year = 0;
	if (year > 127) year = 127;
	const date = ((year << 9) | ((d.getUTCMonth() + 1) << 5) | d.getUTCDate()) & 0xffff;
	const time =
		((d.getUTCHours() << 11) | (d.getUTCMinutes() << 5) | (d.getUTCSeconds() >>> 1)) & 0xffff;
	return { time, date };
}

/** CRC32 of bytes, returned as an unsigned 32-bit integer. */
function crc32(data: Uint8Array): number {
	const table = CRC32_TABLE;
	let crc = 0xffffffff;
	for (let i = 0; i < data.length; i++) {
		const byte = data[i] ?? 0;
		const lookup = table[(crc ^ byte) & 0xff] ?? 0;
		crc = lookup ^ (crc >>> 8);
	}
	return (crc ^ 0xffffffff) >>> 0;
}

/** Pre-computed CRC32 polynomial table (0xedb88320). */
const CRC32_TABLE: ReadonlyArray<number> = (() => {
	const table = new Array<number>(256);
	for (let n = 0; n < 256; n++) {
		let c = n;
		for (let k = 0; k < 8; k++) {
			c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
		}
		table[n] = c >>> 0;
	}
	return table;
})();

// ─────────────────────────────────────────────────────────────────────────────
// Shared helpers
// ─────────────────────────────────────────────────────────────────────────────

/** Concatenate a list of byte chunks into one buffer. */
function concatBytes(chunks: readonly Uint8Array[]): Uint8Array {
	let total = 0;
	for (const c of chunks) total += c.length;
	const out = new Uint8Array(total);
	let pos = 0;
	for (const c of chunks) {
		out.set(c, pos);
		pos += c.length;
	}
	return out;
}

/** Read a file as bytes. */
export async function readBytes(path: string): Promise<Uint8Array> {
	const buf = await readFile(path);
	return new Uint8Array(buf.buffer, buf.byteOffset, buf.byteLength);
}

/**
 * Compute the SHA-256 hex digest of a file's bytes.
 * Used both for archive checksums and for per-file manifest entries.
 */
export async function sha256File(path: string): Promise<string> {
	const bytes = await readBytes(path);
	return sha256Bytes(bytes);
}

/** SHA-256 hex digest of in-memory bytes. */
export function sha256Bytes(bytes: Uint8Array): string {
	return createHash("sha256").update(bytes).digest("hex");
}

/**
 * Format a checksum sidecar line: `<hex>  <filename>\n` (two-space separator,
 * binary mode, matches `sha256sum` convention).
 */
export function checksumLine(hexDigest: string, fileName: string): string {
	return `${hexDigest}  ${fileName}\n`;
}
