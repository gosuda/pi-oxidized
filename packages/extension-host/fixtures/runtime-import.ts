import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import {
	decodeFrameLine,
	encodeFrameString,
	PROTOCOL_VERSION,
	type Frame,
} from "@earendil-works/pi-tui-protocol";
import { COMPATIBILITY_VERSION } from "../src/version.ts";

const PROBE_TIMEOUT_MS = 20_000;
const TERMINATION_GRACE_MS = 2_000;

class ProbeError extends Error {
	constructor(
		readonly exitCode: number,
		message: string,
	) {
		super(message);
		this.name = "ProbeError";
	}
}
interface LoadPayload {
	readonly commands: unknown;
	readonly errors: unknown;
	readonly extensions: unknown;
	readonly flags: unknown;
	readonly handlers: unknown;
	readonly renderers: unknown;
	readonly shortcuts: unknown;
	readonly tools: unknown;
}



function namedEntries(value: unknown): string[] {
	if (!Array.isArray(value)) return [];
	const names: string[] = [];
	for (const entry of value) {
		if (typeof entry !== "object" || entry === null || Array.isArray(entry)) continue;
		if ("name" in entry && typeof entry.name === "string") names.push(entry.name);
	}
	return names;
}


function messageRendererNames(value: unknown): string[] {
	if (!Array.isArray(value)) return [];
	const names: string[] = [];
	for (const entry of value) {
		if (typeof entry !== "object" || entry === null || Array.isArray(entry)) continue;
		if (
			"type" in entry &&
			entry.type === "message" &&
			"name" in entry &&
			typeof entry.name === "string"
		) {
			names.push(entry.name);
		}
	}
	return names;
}


function delay(milliseconds: number): Promise<void> {
	const { promise, resolve } = Promise.withResolvers<void>();
	setTimeout(resolve, milliseconds);
	return promise;
}

async function terminate(
	child: ChildProcessWithoutNullStreams,
	closed: Promise<void>,
): Promise<void> {
	child.stdin.end();
	if (child.exitCode !== null || child.signalCode !== null) return;
	child.kill("SIGTERM");
	await Promise.race([closed, delay(TERMINATION_GRACE_MS)]);
	if (child.exitCode !== null || child.signalCode !== null) return;
	child.kill("SIGKILL");
	await Promise.race([closed, delay(TERMINATION_GRACE_MS)]);
}

async function loadExtension(
	hostPath: string,
	extensionPath: string,
	cwd: string,
): Promise<LoadPayload> {
	const child = spawn(hostPath, ["--cwd", cwd], {
		cwd,
		stdio: ["pipe", "pipe", "pipe"],
	});
	const { promise: closed, resolve: resolveClosed } = Promise.withResolvers<void>();
	child.once("close", () => resolveClosed());

	const { promise, resolve, reject } = Promise.withResolvers<LoadPayload>();
	let stdout = "";
	let stderr = "";
	let settled = false;
	let loadSent = false;
	const timeout = setTimeout(() => {
		finish(new ProbeError(1, `compiled host probe timed out after ${PROBE_TIMEOUT_MS}ms`));
	}, PROBE_TIMEOUT_MS);

	function finish(error?: Error, payload?: LoadPayload): void {
		if (settled) return;
		settled = true;
		clearTimeout(timeout);
		if (error !== undefined) {
			reject(error);
		} else if (payload !== undefined) {
			resolve(payload);
		} else {
			reject(new Error("compiled host probe completed without a payload"));
		}
	}

	function handleFrame(frame: Frame): void {
		if (frame.id === 1) {
			const payload = frame.payload;
			if (
				frame.kind !== "res" ||
				frame.method !== "hello" ||
				typeof payload !== "object" ||
				payload === null ||
				!("protocolVersion" in payload) ||
				payload.protocolVersion !== PROTOCOL_VERSION ||
				!("compatibilityVersion" in payload) ||
				payload.compatibilityVersion !== COMPATIBILITY_VERSION
			) {
				finish(new ProbeError(3, "compiled host rejected the hello contract"));
				return;
			}
			if (!loadSent) {
				loadSent = true;
				child.stdin.write(
					encodeFrameString({
						id: 2,
						kind: "req",
						method: "extensions.load",
						payload: { extensionPaths: [extensionPath], cwd, projectTrusted: true },
					}),
				);
			}
			return;
		}
		if (frame.id !== 2) return;
		if (
			frame.kind !== "res" ||
			frame.method !== "extensions.load" ||
			typeof frame.payload !== "object" ||
			frame.payload === null ||
			Array.isArray(frame.payload)
		) {
			finish(new ProbeError(4, "compiled host returned an invalid extensions.load response"));
			return;
		}
		finish(undefined, {
			commands: "commands" in frame.payload ? frame.payload.commands : undefined,
			errors: "errors" in frame.payload ? frame.payload.errors : undefined,
			extensions: "extensions" in frame.payload ? frame.payload.extensions : undefined,
			flags: "flags" in frame.payload ? frame.payload.flags : undefined,
			handlers: "handlers" in frame.payload ? frame.payload.handlers : undefined,
			renderers: "renderers" in frame.payload ? frame.payload.renderers : undefined,
			shortcuts: "shortcuts" in frame.payload ? frame.payload.shortcuts : undefined,
			tools: "tools" in frame.payload ? frame.payload.tools : undefined,
		});
	}

	child.stdout.on("data", (chunk: Buffer) => {
		stdout += chunk.toString();
		for (;;) {
			const newline = stdout.indexOf("\n");
			if (newline < 0) return;
			const line = stdout.slice(0, newline);
			stdout = stdout.slice(newline + 1);
			let frame: Frame;
			try {
				frame = decodeFrameLine(line);
			} catch (error) {
				finish(error instanceof Error ? error : new Error(String(error)));
				return;
			}
			handleFrame(frame);
		}
	});
	child.stderr.on("data", (chunk: Buffer) => {
		stderr += chunk.toString();
	});
	child.once("error", (error) => finish(error));
	child.once("exit", (code, signal) => {
		if (settled) return;
		finish(
			new ProbeError(
				1,
				`compiled host exited before extensions.load (code=${String(code)}, signal=${String(signal)}): ${stderr.slice(-1000)}`,
			),
		);
	});

	child.stdin.write(
		encodeFrameString({
			id: 1,
			kind: "req",
			method: "hello",
			payload: {
				protocolVersion: PROTOCOL_VERSION,
				compatibilityVersion: COMPATIBILITY_VERSION,
			},
		}),
	);

	try {
		return await promise;
	} finally {
		await terminate(child, closed);
	}
}

async function main(): Promise<void> {
	const hostPath = process.argv[2];
	const extensionPath = process.argv[3];
	if (!hostPath || !extensionPath) {
		throw new ProbeError(
			2,
			"usage: runtime-import.ts <compiled-sidecar-path> <extension-path>",
		);
	}
	const payload = await loadExtension(hostPath, extensionPath, process.cwd());
	const errors = payload.errors;
	const tools = namedEntries(payload.tools);
	if (
		payload.extensions !== 1 ||
		!Array.isArray(errors) ||
		errors.length !== 0 ||
		!tools.includes("echo")
	) {
		throw new ProbeError(
			4,
			`extension load failed: ${JSON.stringify({ extensions: payload.extensions, errors, tools })}`,
		);
	}
	process.stdout.write(
		`${JSON.stringify({
			path: extensionPath,
			tools,
			handlers: Array.isArray(payload.handlers)
				? payload.handlers.filter(
						(entry): entry is string => typeof entry === "string",
					)
				: [],
			commands: namedEntries(payload.commands),
			flags: namedEntries(payload.flags),
			shortcuts: Array.isArray(payload.shortcuts)
				? payload.shortcuts.flatMap((entry) => {
						if (
							typeof entry !== "object" ||
							entry === null ||
							Array.isArray(entry) ||
							!("key" in entry) ||
							typeof entry.key !== "string"
						) {
							return [];
						}
						return [entry.key];
					})
				: [],
			messageRenderers: messageRendererNames(payload.renderers),
		})}\n`,
	);
}

main().catch((error) => {
	const failure = error instanceof Error ? error : new Error(String(error));
	console.error("runtime-import probe failed:", failure.message);
	process.exit(error instanceof ProbeError ? error.exitCode : 1);
});
