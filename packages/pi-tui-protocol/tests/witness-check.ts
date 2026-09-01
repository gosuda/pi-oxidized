/**
 * Pure lockstep checks for the generated cross-language protocol witness
 * (ARC11). `verifyWitness` consumes the `frames.jsonl` text and the parsed
 * `witness-manifest.json` and returns one violation string per broken
 * manifest rule; an empty list means the fixture and manifest agree.
 *
 * Dependency-free by design: pi-tui-protocol has zero package dependencies,
 * and the manifest — not any extension-host import — is the sole carrier of
 * the cross-language contract.
 */

/** One witnessed modifier-combo key event (`uiEvent` req with a key payload). */
interface ModifierComboKeyEvent {
	readonly code: string;
	readonly modifiers: Readonly<Record<string, boolean>>;
	readonly kind: string;
}

/** Parsed `witness-manifest.json` schema (ARC11 plan Decision 3). */
export interface WitnessManifest {
	/** Non-blank, non-`#` frame line count. */
	readonly totalLines: number;
	/** Sorted bijective set of witnessed `(method, kind)` pairs. */
	readonly methodKindPairs: readonly (readonly [string, string])[];
	/**
	 * Lifecycle event discriminants in ALL_EVENT_TYPES order. Optional only
	 * for the pre-generator manifests that predate ARC11; the generator
	 * always emits it.
	 */
	readonly lifecycleDiscriminants?: readonly string[];
	/** Witnessed modifier-combo key events in fixture order. */
	readonly modifierComboKeyEvents: readonly ModifierComboKeyEvent[];
	/** sha256 hex of the fixture text. */
	readonly fixtureSha256: string;
}

/** Wire envelope emitted by the generator (protocol.rs FrameId/FrameKind). */
interface WitnessFrame {
	readonly kind: unknown;
	readonly method: unknown;
	readonly payload: unknown;
}

/** Canonical, modifier-key-order-insensitive rendering of one key event. */
function modifierEventSignature(event: ModifierComboKeyEvent): string {
	const modifiers = Object.keys(event.modifiers)
		.sort()
		.map((key) => `${key}:${String(event.modifiers[key])}`)
		.join(",");
	return `${event.code}|${modifiers}|${event.kind}`;
}

/** Return one violation per broken manifest rule; pure over both inputs. */
export function verifyWitness(fixturesText: string, manifest: WitnessManifest): string[] {
	const violations: string[] = [];
	const frames: WitnessFrame[] = [];
	let totalLines = 0;

	for (const [lineIndex, line] of fixturesText.split("\n").entries()) {
		const trimmed = line.trim();
		if (trimmed === "" || trimmed.startsWith("#")) continue;
		totalLines += 1;
		let parsed: unknown;
		try {
			parsed = JSON.parse(trimmed);
		} catch (error) {
			violations.push(`fixture line ${lineIndex + 1} is invalid JSON: ${String(error)}`);
			continue;
		}
		if (parsed === null) {
			violations.push(`fixture line ${lineIndex + 1} is not a frame object`);
			continue;
		}
		frames.push(parsed as WitnessFrame);
	}

	if (totalLines !== manifest.totalLines) {
		violations.push(`totalLines mismatch: fixture has ${totalLines}, manifest declares ${manifest.totalLines}`);
	}

	const manifestPairs = new Set<string>();
	for (const entry of manifest.methodKindPairs) {
		if (
			!Array.isArray(entry) ||
			typeof entry[0] !== "string" ||
			typeof entry[1] !== "string" ||
			!["req", "res", "event", "error"].includes(entry[1])
		) {
			violations.push(`manifest pair entry is malformed: ${JSON.stringify(entry)}`);
			continue;
		}
		const key = `${entry[0]}:${entry[1]}`;
		if (manifestPairs.has(key)) {
			violations.push(`manifest declares duplicate pair: ${key}`);
		}
		manifestPairs.add(key);
	}
	const observedPairs = new Set<string>();
	for (const frame of frames) {
		if (typeof frame.method !== "string" || typeof frame.kind !== "string") {
			violations.push("fixture frame has non-string method or kind");
			continue;
		}
		observedPairs.add(`${frame.method}:${frame.kind}`);
	}
	for (const pair of manifestPairs) {
		if (!observedPairs.has(pair)) violations.push(`missing pair ${pair}`);
	}
	for (const pair of observedPairs) {
		if (!manifestPairs.has(pair)) violations.push(`untracked pair not in manifest: ${pair}`);
	}

	const lifecycle = manifest.lifecycleDiscriminants ?? [];
	const observedLifecycle: string[] = [];
	for (const frame of frames) {
		if (frame.kind !== "req" || typeof frame.method !== "string") continue;
		// Lifecycle hooks share the open-method namespace with dialogs (the
		// wire authority dispatches dialog methods first, then the handler
		// allowlist), so a lifecycle witness frame is identified by its
		// payload naming its own event type — the fixture convention this
		// check pins. Dialog frames like `input` carry dialog payloads.
		const payload = frame.payload as Record<string, unknown> | null | undefined;
		const payloadType =
			typeof payload === "object" && payload !== null ? payload["type"] : undefined;
		if (lifecycle.includes(frame.method) && payloadType === frame.method) {
			observedLifecycle.push(frame.method);
		}
	}
	for (let index = 0; index < Math.max(observedLifecycle.length, lifecycle.length); index += 1) {
		const observed = observedLifecycle[index];
		const expected = lifecycle[index];
		if (observed !== expected) {
			violations.push(
				`lifecycle discriminant mismatch at index ${index}: fixture has ${String(observed)}, manifest has ${String(expected)}`,
			);
			break;
		}
	}

	const keyEvents: ModifierComboKeyEvent[] = [];
	for (const frame of frames) {
		if (frame.kind !== "req" || frame.method !== "uiEvent") continue;
		const event = (
			frame.payload as
				| { event?: { type?: unknown; code?: unknown; modifiers?: Record<string, boolean>; kind?: unknown } }
				| null
				| undefined
		)?.event;
		if (event === undefined || event.type !== "key" || typeof event.code !== "string") continue;
		keyEvents.push({
			code: event.code,
			modifiers: event.modifiers ?? {},
			kind: typeof event.kind === "string" ? event.kind : "press",
		});
	}
	const manifestEvents = manifest.modifierComboKeyEvents;
	if (keyEvents.length !== manifestEvents.length) {
		violations.push(
			`modifierComboKeyEvents length mismatch: fixture has ${keyEvents.length}, manifest declares ${manifestEvents.length}`,
		);
	} else {
		for (const [index, observed] of keyEvents.entries()) {
			const expected = manifestEvents[index];
			if (expected === undefined || modifierEventSignature(observed) !== modifierEventSignature(expected)) {
				violations.push(
					`modifierComboKeyEvents mismatch at index ${index}: fixture ${modifierEventSignature(observed)}, manifest ${
						expected === undefined ? "none" : modifierEventSignature(expected)
					}`,
				);
				break;
			}
		}
	}

	// Payload-byte pin: the manifest digest names the exact fixture text, so
	// any single flipped byte is rejected even when every envelope rule holds.
	const digest = new Bun.CryptoHasher("sha256").update(fixturesText).digest("hex");
	if (digest !== manifest.fixtureSha256) {
		violations.push(
			`fixtureSha256 mismatch: fixture hashes to ${digest}, manifest declares ${manifest.fixtureSha256}`,
		);
	}

	return violations;
}
