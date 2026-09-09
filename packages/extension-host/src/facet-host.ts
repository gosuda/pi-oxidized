/**
 * Development-only Chord facet bridge.
 *
 * This module owns the real bundle/build/load/reload/dispose path. It never
 * evaluates source text with `eval`; Chord's VM bundle loader verifies the
 * content-addressed entry before executing it.
 */

import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import {
	createFacetHost,
	createRemoteServiceBinding,
	createRemoteServiceEndpoint,
	createServiceSubscribeCall,
	createServiceUnsubscribeCall,
	defineFacet,
	defineService,
	parseServiceCall,
	parseServiceCatalogue,
	parseServiceProviderUpdate,
	parseServiceSubscriptionSnapshot,
	type Context,
	type Facet,
	type FacetEnvironment,
	type FacetHost,
	type JsonValue,
	type LoadedFacets,
	type RemoteServiceEndpoint,
	type RemoteServiceSource,
	type Service,
	type ServiceCall,
	type ServiceCatalogueEntry,
	type ServiceMode,
	type ServiceProviderUpdate,
	type ServiceSubscription,
} from "@earendil-works/chord";
import { BACKGROUND_CONTEXT, withAbortSignal } from "@earendil-works/chord/context";
import {
	createFacetBundleArtifactLoader,
	createFacetBundleLoader,
	readFacetBundleArtifact,
	readFacetBundleManifest,
} from "@earendil-works/chord/node";
import { bundleFacetPackage } from "@earendil-works/chord/bundler";

export const PRESENTATION_FACET_BUNDLES_KEY = "presentationFacetBundles";
export const PI_PLUGIN_API = "@earendil-works/pi-coding-agent/experimental/plugin";

export const DEFAULT_PLUGIN_FACETS = Object.freeze({
	session: "src/session.ts",
	tui: "src/tui.ts",
});

export interface SlashCommandCompletionWire {
	readonly value: string;
	readonly label: string;
	readonly description?: string;
}

export interface SlashCommandContributionWire {
	readonly name: string;
	readonly description?: string;
	readonly argumentHint?: string;
}

interface SlashCommandContribution extends SlashCommandContributionWire {
	readonly run: (args: string, context: Context) => unknown | Promise<unknown>;
	readonly getArgumentCompletions?: (
		argumentPrefix: string,
	) => readonly SlashCommandCompletionWire[] | null | Promise<readonly SlashCommandCompletionWire[] | null>;
}

interface SlashCommands {
	register(command: SlashCommandContribution): () => void;
	replace(command: SlashCommandContribution): () => void;
	list(): readonly SlashCommandContribution[];
	subscribe(listener: (commands: readonly SlashCommandContribution[]) => void): () => void;
}

export interface PresentationSelectItem {
	readonly value: string;
	readonly label: string;
	readonly description?: string;
}

interface PresentationUI {
	select(
		title: string,
		items: readonly PresentationSelectItem[],
		selectedValue: string | undefined,
		context: Context,
	): Promise<string | undefined>;
	showStatus(message: string, context: Context): void;
}

const SlashCommands: Service<SlashCommands> = defineService<SlashCommands>("pi.local.slash-commands", { local: true });
const PresentationUI: Service<PresentationUI> = defineService<PresentationUI>("pi.local.presentation-ui", { local: true });

export interface FacetHostRpc {
	request(
		method: string,
		payload: JsonValue,
		options?: { readonly timeoutMs?: number; readonly signal?: AbortSignal },
	): Promise<unknown>;
	send(method: string, payload: JsonValue): Promise<void>;
}

export interface FacetHostBridgeOptions {
	readonly rpc: FacetHostRpc;
	readonly select: (
		title: string,
		items: readonly PresentationSelectItem[],
		selectedValue: string | undefined,
		signal: AbortSignal | undefined,
	) => Promise<string | undefined>;
	readonly showStatus: (message: string) => void;
	readonly onError?: (error: Error) => void;
}

interface HostRecord {
	readonly hostId: string;
	readonly entry: "session" | "tui";
	readonly manifestPaths: readonly string[];
	readonly artifacts: readonly JsonValue[];
	readonly builtinCatalogue: readonly ServiceCatalogueEntry[];
	readonly facetHost: FacetHost;
	readonly endpoint: RemoteServiceEndpoint;
	readonly localFacets: readonly Facet[];
	readonly slashRegistry: SlashCommandRegistry;
	loaded: readonly LoadedFacets[];
	disposed: boolean;
	readonly inFlight: Set<AbortController>;
}

interface SubscriptionRecord {
	readonly key: string;
	readonly subscriptionId: string;
	readonly listener: (update: ServiceProviderUpdate, context: Context) => void;
	readonly pending: Array<{ readonly update: ServiceProviderUpdate; readonly context: Context }>;
	active: boolean;
	closed: boolean;
}

class SlashCommandRegistry {
	readonly #stacks = new Map<string, SlashCommandContribution[]>();
	readonly #listeners = new Set<(commands: readonly SlashCommandContribution[]) => void>();
	readonly #onChanged: () => void;

	constructor(onChanged: () => void) {
		this.#onChanged = onChanged;
	}

	register(command: SlashCommandContribution): () => void {
		validateSlashCommand(command);
		const stack = this.#stacks.get(command.name);
		if (stack !== undefined && stack.length > 0) {
			throw new Error(`Slash command already registered: ${command.name}`);
		}
		this.#stacks.set(command.name, [command]);
		this.#publish();
		return () => this.#remove(command);
	}

	replace(command: SlashCommandContribution): () => void {
		validateSlashCommand(command);
		const stack = this.#stacks.get(command.name) ?? [];
		stack.push(command);
		this.#stacks.set(command.name, stack);
		this.#publish();
		return () => this.#remove(command);
	}

	list(): readonly SlashCommandContribution[] {
		return Object.freeze(
			[...this.#stacks.values()]
				.map((stack) => stack.at(-1))
				.filter((command): command is SlashCommandContribution => command !== undefined),
		);
	}

	subscribe(listener: (commands: readonly SlashCommandContribution[]) => void): () => void {
		this.#listeners.add(listener);
		listener(this.list());
		return () => this.#listeners.delete(listener);
	}

	find(name: string): SlashCommandContribution | undefined {
		return this.#stacks.get(name)?.at(-1);
	}

	#remove(command: SlashCommandContribution): void {
		const stack = this.#stacks.get(command.name);
		if (stack === undefined) return;
		const index = stack.lastIndexOf(command);
		if (index < 0) return;
		stack.splice(index, 1);
		if (stack.length === 0) this.#stacks.delete(command.name);
		this.#publish();
	}

	#publish(): void {
		const commands = this.list();
		for (const listener of this.#listeners) listener(commands);
		this.#onChanged();
	}
}

function createLocalFacets(
	registry: SlashCommandRegistry,
	options: FacetHostBridgeOptions,
): readonly Facet[] {
	const slashFacet = defineFacet({
		id: "pi.bridge.slash-commands",
		setup(environment: FacetEnvironment): void {
			environment.provide(SlashCommands, registry);
		},
	});
	const presentationFacet = defineFacet({
		id: "pi.bridge.presentation-ui",
		setup(environment: FacetEnvironment): void {
			environment.provide(PresentationUI, {
				select: (title, items, selectedValue, context) =>
					options.select(title, items, selectedValue, context.abortSignal),
				showStatus: (message) => options.showStatus(message),
			});
		},
	});
	return Object.freeze([slashFacet, presentationFacet]);

}
class NativeServiceTransport {
	readonly #bridge: FacetHostBridge;
	readonly #hostId: string;
	readonly #subscriptions = new Map<string, SubscriptionRecord>();
	#nextSubscription = 0;

	constructor(bridge: FacetHostBridge, hostId: string) {
		this.#bridge = bridge;
		this.#hostId = hostId;
	}

	invoke(call: ServiceCall, context: Context): Promise<JsonValue | undefined> {
		return this.#bridge.invokeNativeService(this.#hostId, call, context);
	}

	subscribe(
		serviceId: string,
		mode: ServiceMode,
		listener: (update: ServiceProviderUpdate, context: Context) => void,
		context: Context,
	): Promise<ServiceSubscription> {
		const subscriptionId = `${this.#hostId}:${++this.#nextSubscription}`;
		const key = this.#key(subscriptionId);
		const record: SubscriptionRecord = {
			key,
			subscriptionId,
			listener,
			pending: [],
			active: false,
			closed: false,
		};
		this.#subscriptions.set(key, record);
		return this.#bridge.invokeNativeService(
			this.#hostId,
			createServiceSubscribeCall(subscriptionId, serviceId, mode),
			context,
		).then((value) => {
			const snapshot = parseServiceSubscriptionSnapshot(value);
			if (snapshot.serviceId !== serviceId || snapshot.mode !== mode) {
				throw new Error(`Facet service ${serviceId} returned a mismatched subscription snapshot`);
			}
			return {
				snapshot,
				activate: () => this.#activate(record),
				close: (closeContext = BACKGROUND_CONTEXT) => this.#close(record, closeContext),
			};
		}).catch((error: unknown) => {
			this.#subscriptions.delete(key);
			throw error;
		});
	}

	dispatch(subscriptionId: string, update: unknown): void {
		const record = this.#subscriptions.get(this.#key(subscriptionId));
		if (record === undefined || record.closed) return;
		try {
			const parsed = parseServiceProviderUpdate(update);
			if (!record.active) {
				record.pending.push({ update: parsed, context: BACKGROUND_CONTEXT });
				return;
			}
			record.listener(parsed, BACKGROUND_CONTEXT);
		} catch (error) {
			this.#bridge.reportError(toError(error));
		}
	}

	async closeAll(): Promise<void> {
		const records = [...this.#subscriptions.values()];
		const results = await Promise.allSettled(records.map((record) => this.#close(record, BACKGROUND_CONTEXT)));
		const errors = results.flatMap((result) => result.status === "rejected" ? [result.reason] : []);
		if (errors.length === 1) throw errors[0];
		if (errors.length > 1) throw new AggregateError(errors, "Facet service subscriptions failed to close");
	}

	#activate(record: SubscriptionRecord): void {
		if (record.closed || record.active) return;
		record.active = true;
		const pending = record.pending.splice(0);
		for (const item of pending) record.listener(item.update, item.context);
	}

	async #close(record: SubscriptionRecord, context: Context): Promise<void> {
		if (record.closed) return;
		record.closed = true;
		record.pending.length = 0;
		this.#subscriptions.delete(record.key);
		await this.#bridge.invokeNativeService(this.#hostId, createServiceUnsubscribeCall(record.subscriptionId), context);
	}

	#key(subscriptionId: string): string {
		return `${this.#hostId}\u0000${subscriptionId}`;
	}
}

function createNativeServiceSource(
	bridge: FacetHostBridge,
	hostId: string,
	catalogue: readonly ServiceCatalogueEntry[],
	onError: (error: Error) => void,
): RemoteServiceSource {
	const transport = new NativeServiceTransport(bridge, hostId);
	bridge.registerNativeTransport(hostId, transport);
	return {
		acceptsUnavailableServices: false,
		catalogue: async () => catalogue,
		open: ({ services, assertAccess, onError: sourceError }) =>
			createRemoteServiceBinding({
				services,
				transport,
				bound: true,
				assertAccess,
				onError: (error) => {
					onError(error);
					sourceError(error);
				},
			}),
	};
}

export class FacetHostBridge {
	readonly #options: FacetHostBridgeOptions;
	readonly #hosts = new Map<string, HostRecord>();
	readonly #transports = new Map<string, NativeServiceTransport>();
	#disposed = false;

	constructor(options: FacetHostBridgeOptions) {
		this.#options = options;
	}

	handlesRequest(method: string): boolean {
		return method === "facet.bundle.build"
			|| method === "facet.host.load"
			|| method === "facet.host.reload"
			|| method === "facet.host.dispose"
			|| method === "facet.service.invoke"
			|| method === "facet.slash.run"
			|| method === "facet.slash.complete";
	}

	async handleRequest(method: string, payload: unknown): Promise<JsonValue> {
		if (this.#disposed) throw new Error("Facet host bridge is disposed");
		switch (method) {
			case "facet.bundle.build":
				return this.#bundleBuild(payload);
			case "facet.host.load":
				return this.#hostLoad(payload);
			case "facet.host.reload":
				return this.#hostReload(payload);
			case "facet.host.dispose":
				return this.#hostDispose(payload);
			case "facet.service.invoke":
				return this.#serviceInvoke(payload);
			case "facet.slash.run":
				return this.#slashRun(payload);
			case "facet.slash.complete":
				return this.#slashComplete(payload);
			default:
				throw new Error(`Unknown facet host method: ${method}`);
		}
	}

	handleEvent(method: string, payload: unknown): void {
		if (method !== "facet.service.update") return;
		const fields = record(payload, "facet service update");
		const hostId = requiredString(fields, "hostId", "facet service update.hostId");
		const subscriptionId = requiredString(fields, "subscriptionId", "facet service update.subscriptionId");
		const update = fields["update"];
		if (update === undefined) throw new TypeError("facet service update.update is required");
		this.#transports.get(hostId)?.dispatch(subscriptionId, update);
	}

	registerNativeTransport(hostId: string, transport: NativeServiceTransport): void {
		this.#transports.set(hostId, transport);
	}

	async invokeNativeService(hostId: string, call: ServiceCall, context: Context): Promise<JsonValue | undefined> {
		const response = await this.#options.rpc.request(
			"facet.service.invoke",
			objectValue({ hostId, call: serviceCallValue(call) }),
			{ signal: context.abortSignal },
		);
		const fields = record(response, "facet service invocation result");
		if (!("result" in fields)) return undefined;
		return jsonValue(fields["result"]);
	}

	reportError(error: Error): void {
		this.#options.onError?.(error);
	}

	async dispose(): Promise<void> {
		if (this.#disposed) return;
		this.#disposed = true;
		const errors: unknown[] = [];
		for (const host of [...this.#hosts.values()]) {
			try {
				await this.#disposeHost(host);
			} catch (error) {
				errors.push(error);
			}
		}
		this.#hosts.clear();
		this.#transports.clear();
		if (errors.length === 1) throw errors[0];
		if (errors.length > 1) throw new AggregateError(errors, "Facet host disposal failed");
	}

	async #bundleBuild(payload: unknown): Promise<JsonValue> {
		const fields = record(payload, "facet bundle build request");
		const packagePath = requiredString(fields, "packagePath", "facet bundle build packagePath");
		const outdir = requiredString(fields, "outdir", "facet bundle build outdir");
		const defaultFacetsValue = fields["defaultFacets"];
		const defaultFacets = defaultFacetsValue === undefined
			? DEFAULT_PLUGIN_FACETS
			: stringRecord(defaultFacetsValue, "facet bundle build defaultFacets");
		const result = await bundleFacetPackage({ packagePath, outdir, defaultFacets });
		const tuiArtifacts: JsonValue[] = [];
		if (result.manifest.entries["tui"] !== undefined) {
			tuiArtifacts.push(jsonValue(await readFacetBundleArtifact({
				manifestPath: result.manifestPath,
				entry: "tui",
			})));
		}
		return objectValue({ manifestPath: result.manifestPath, tuiArtifacts });
	}

	async #hostLoad(payload: unknown): Promise<JsonValue> {
		const request = parseHostLoadRequest(payload);
		if (this.#hosts.has(request.hostId)) throw new Error(`Facet host already exists: ${request.hostId}`);
		const slashRegistry = new SlashCommandRegistry(() => {
			const host = this.#hosts.get(request.hostId);
			if (host !== undefined && !host.disposed) void this.#sendSlashChanged(host);
		});
		const localFacets = createLocalFacets(slashRegistry, this.#options);
		const source = createNativeServiceSource(this, request.hostId, request.builtinCatalogue, (error) => this.reportError(error));
		let loaded: readonly LoadedFacets[];
		try {
			loaded = await this.#loadBundles(request);
		} catch (error) {
			this.#transports.delete(request.hostId);
			throw error;
		}
		let facetHost: FacetHost;
		try {
			facetHost = await createFacetHost({
				facets: [...localFacets, ...loaded.flatMap((item) => [...item.facets])],
				serviceSources: [source],
				onError: (error) => this.reportError(error),
			});
		} catch (error) {
			await disposeLoaded(loaded);
			this.#transports.delete(request.hostId);
			throw error;
		}
		const host: HostRecord = {
			hostId: request.hostId,
			entry: request.entry,
			manifestPaths: request.manifestPaths,
			artifacts: request.artifacts,
			builtinCatalogue: request.builtinCatalogue,
			facetHost,
			endpoint: createRemoteServiceEndpoint(facetHost.services),
			localFacets,
			slashRegistry,
			loaded,
			disposed: false,
			inFlight: new Set(),
		};
		this.#hosts.set(request.hostId, host);
		return hostResponse(host);
	}

	async #hostReload(payload: unknown): Promise<JsonValue> {
		const hostId = requiredString(record(payload, "facet host reload request"), "hostId", "facet host reload.hostId");
		const host = this.#hosts.get(hostId);
		if (host === undefined || host.disposed) throw new Error(`Facet host not found: ${hostId}`);
		const request: ParsedHostLoadRequest = {
			hostId,
			entry: host.entry,
			manifestPaths: host.manifestPaths,
			artifacts: host.artifacts,
			builtinCatalogue: host.builtinCatalogue,
		};
		const loaded = await this.#loadBundles(request);
		try {
			await host.facetHost.reload([...host.localFacets, ...loaded.flatMap((item) => [...item.facets])]);
		} catch (error) {
			await disposeLoaded(loaded);
			throw error;
		}
		const oldLoaded = host.loaded;
		host.loaded = loaded;
		await disposeLoaded(oldLoaded);
		return hostResponse(host);
	}

	async #hostDispose(payload: unknown): Promise<JsonValue> {
		const hostId = requiredString(record(payload, "facet host dispose request"), "hostId", "facet host dispose.hostId");
		const host = this.#hosts.get(hostId);
		if (host === undefined) throw new Error(`Facet host not found: ${hostId}`);
		await this.#disposeHost(host);
		return objectValue({});
	}

	async #serviceInvoke(payload: unknown): Promise<JsonValue> {
		const fields = record(payload, "facet service invocation request");
		const hostId = requiredString(fields, "hostId", "facet service invocation.hostId");
		const callValue = fields["call"];
		if (callValue === undefined) throw new TypeError("facet service invocation.call is required");
		const call = parseServiceCall(callValue);
		const host = this.#hosts.get(hostId);
		if (host === undefined || host.disposed) throw new Error(`Facet host not found: ${hostId}`);
		const controller = new AbortController();
		host.inFlight.add(controller);
		try {
			const context = withAbortSignal(controller.signal, BACKGROUND_CONTEXT);
			const value = await host.endpoint.invoke(call, (subscriptionId, update) =>
				this.#publishServiceUpdate(host, subscriptionId, update), context);
			return value === undefined ? objectValue({}) : objectValue({ result: value });
		} finally {
			host.inFlight.delete(controller);
		}
	}

	async #slashRun(payload: unknown): Promise<JsonValue> {
		const fields = record(payload, "facet slash run request");
		const hostId = requiredString(fields, "hostId", "facet slash run.hostId");
		const name = requiredString(fields, "name", "facet slash run.name");
		const args = requiredText(fields, "args", "facet slash run.args");
		const host = this.#hosts.get(hostId);
		if (host === undefined || host.disposed) throw new Error(`Facet host not found: ${hostId}`);
		const command = host.slashRegistry.find(name);
		if (command === undefined) throw new Error(`Slash command not found: ${name}`);
		const value = await command.run(args, BACKGROUND_CONTEXT);
		return value === undefined ? objectValue({}) : objectValue({ result: jsonValue(value) });
	}

	async #slashComplete(payload: unknown): Promise<JsonValue> {
		const fields = record(payload, "facet slash complete request");
		const hostId = requiredString(fields, "hostId", "facet slash complete.hostId");
		const name = requiredString(fields, "name", "facet slash complete.name");
		const argumentPrefix = requiredText(fields, "argumentPrefix", "facet slash complete.argumentPrefix");
		const host = this.#hosts.get(hostId);
		if (host === undefined || host.disposed) throw new Error(`Facet host not found: ${hostId}`);
		const command = host.slashRegistry.find(name);
		if (command?.getArgumentCompletions === undefined) return objectValue({ completions: null });
		const completions = await command.getArgumentCompletions(argumentPrefix);
		if (completions === null) return objectValue({ completions: null });
		return objectValue({ completions: completions.map(completionValue) });
	}

	async #loadBundles(request: ParsedHostLoadRequest): Promise<readonly LoadedFacets[]> {
		const loaded: LoadedFacets[] = [];
		try {
			for (const manifestPath of request.manifestPaths) {
				const manifest = await readFacetBundleManifest(manifestPath);
				if (manifest.entries[request.entry] === undefined) continue;
				const item = await createFacetBundleLoader({
					manifestPath,
					entry: request.entry,
					resolveExternal: resolvePluginExternal,
				}).load();
				loaded.push(item);
			}
			for (const artifact of request.artifacts) {
				loaded.push(await createFacetBundleArtifactLoader({
					artifact,
					resolveExternal: resolvePluginExternal,
				}).load());
			}
			return loaded;
		} catch (error) {
			await disposeLoaded(loaded);
			throw error;
		}
	}

	async #disposeHost(host: HostRecord): Promise<void> {
		if (host.disposed) return;
		host.disposed = true;
		this.#hosts.delete(host.hostId);
		for (const controller of host.inFlight) controller.abort();
		host.inFlight.clear();
		const errors: unknown[] = [];
		try {
			await host.facetHost.dispose();
		} catch (error) {
			errors.push(error);
		}
		try {
			host.endpoint.dispose();
		} catch (error) {
			errors.push(error);
		}
		const transport = this.#transports.get(host.hostId);
		if (transport !== undefined) {
			try {
				await transport.closeAll();
			} catch (error) {
				errors.push(error);
			}
		}
		try {
			await disposeLoaded(host.loaded);
		} catch (error) {
			errors.push(error);
		}
		this.#transports.delete(host.hostId);
		if (errors.length === 1) throw errors[0];
		if (errors.length > 1) throw new AggregateError(errors, `Failed to dispose facet host ${host.hostId}`);
	}

	async #publishServiceUpdate(host: HostRecord, subscriptionId: string, update: ServiceProviderUpdate): Promise<void> {
		await this.#options.rpc.send("facet.service.update", objectValue({
			hostId: host.hostId,
			subscriptionId,
			update: jsonValue(update),
		}));
	}

	async #sendSlashChanged(host: HostRecord): Promise<void> {
		try {
			await this.#options.rpc.send("facet.slash.changed", objectValue({
				hostId: host.hostId,
				commands: host.slashRegistry.list().map(commandMetadata),
			}));
		} catch (error) {
			this.reportError(toError(error));
		}
	}
}

interface ParsedHostLoadRequest {
	readonly hostId: string;
	readonly entry: "session" | "tui";
	readonly manifestPaths: readonly string[];
	readonly artifacts: readonly JsonValue[];
	readonly builtinCatalogue: readonly ServiceCatalogueEntry[];
}

function parseHostLoadRequest(value: unknown): ParsedHostLoadRequest {
	const fields = record(value, "facet host load request");
	const hostId = requiredString(fields, "hostId", "facet host load.hostId");
	const entry = requiredString(fields, "entry", "facet host load.entry");
	if (entry !== "session" && entry !== "tui") throw new TypeError("facet host load.entry must be session or tui");
	const manifestPaths = optionalStringArray(fields["manifestPaths"], "facet host load.manifestPaths");
	const artifacts = optionalJsonArray(fields["artifacts"], "facet host load.artifacts");
	const catalogue = fields["builtinCatalogue"];
	if (catalogue === undefined) throw new TypeError("facet host load.builtinCatalogue is required");
	return {
		hostId,
		entry,
		manifestPaths,
		artifacts,
		builtinCatalogue: parseServiceCatalogue(catalogue),
	};
}

function hostResponse(host: HostRecord): JsonValue {
	return objectValue({
		catalogue: host.facetHost.services.catalogue.map((entry) => objectValue({
			serviceId: entry.serviceId,
			mode: entry.mode,
		})),
		slashCommands: host.slashRegistry.list().map(commandMetadata),
	});
}

function commandMetadata(command: SlashCommandContribution): JsonValue {
	return objectValue({
		name: command.name,
		...(command.description === undefined ? {} : { description: command.description }),
		...(command.argumentHint === undefined ? {} : { argumentHint: command.argumentHint }),
	});
}

function completionValue(completion: SlashCommandCompletionWire): JsonValue {
	if (typeof completion.value !== "string" || typeof completion.label !== "string") {
		throw new TypeError("Slash command completion must contain string value and label");
	}
	return objectValue({
		value: completion.value,
		label: completion.label,
		...(completion.description === undefined ? {} : { description: completion.description }),
	});
}

function serviceCallValue(call: ServiceCall): JsonValue {
	return objectValue({
		serviceId: call.serviceId,
		member: call.member,
		args: [...call.args],
		...(call.instance === undefined ? {} : {
			instance: objectValue({ key: call.instance.key, generation: call.instance.generation }),
		}),
	});
}

function resolvePluginExternal(specifier: string): string | undefined {
	if (specifier !== PI_PLUGIN_API) return undefined;
	const packageMain = createRequire(import.meta.url).resolve("@earendil-works/pi-coding-agent");
	const extension = packageMain.endsWith(".ts") ? "ts" : "js";
	return join(dirname(packageMain), "experimental", `plugin.${extension}`);
}
function validateSlashCommand(command: SlashCommandContribution): void {
	if (!/^[a-z0-9][a-z0-9:-]*$/u.test(command.name)) {
		throw new TypeError(`Invalid slash command name: ${command.name}`);
	}
	if (typeof command.run !== "function") throw new TypeError(`Slash command ${command.name} has no run callback`);
}

function record(value: unknown, description: string): Record<string, unknown> {
	if (value === null || typeof value !== "object" || Array.isArray(value)) {
		throw new TypeError(`Invalid ${description}`);
	}
	return Object.fromEntries(Object.entries(value));
}

function requiredString(fields: Record<string, unknown>, name: string, description: string): string {
	const value = fields[name];
	if (typeof value !== "string" || value.length === 0) throw new TypeError(`Invalid ${description}`);
	return value;
}

function requiredText(fields: Record<string, unknown>, name: string, description: string): string {
	const value = fields[name];
	if (typeof value !== "string") throw new TypeError(`Invalid ${description}`);
	return value;
}
function optionalStringArray(value: unknown, description: string): readonly string[] {
	if (value === undefined) return [];
	if (!Array.isArray(value) || value.some((item) => typeof item !== "string" || item.length === 0)) {
		throw new TypeError(`Invalid ${description}`);
	}
	return value;
}

function optionalJsonArray(value: unknown, description: string): readonly JsonValue[] {
	if (value === undefined) return [];
	if (!Array.isArray(value)) throw new TypeError(`Invalid ${description}`);
	return value.map((item) => jsonValue(item));
}

function stringRecord(value: unknown, description: string): Readonly<Record<string, string>> {
	const fields = record(value, description);
	const result: Record<string, string> = {};
	for (const [key, item] of Object.entries(fields)) {
		if (key.length === 0 || typeof item !== "string" || item.length === 0) throw new TypeError(`Invalid ${description}`);
		result[key] = item;
	}
	return result;
}

function objectValue(fields: Record<string, JsonValue>): JsonValue {
	return fields;
}

function jsonValue(value: unknown): JsonValue {
	if (value === null || typeof value === "string" || typeof value === "boolean") return value;
	if (typeof value === "number" && Number.isFinite(value)) return value;
	if (Array.isArray(value)) return value.map((item) => jsonValue(item));
	if (typeof value === "object") {
		const result: Record<string, JsonValue> = {};
		for (const [key, item] of Object.entries(value)) result[key] = jsonValue(item);
		return result;
	}
	throw new TypeError("Value is not JSON");
}

async function disposeLoaded(loaded: readonly LoadedFacets[]): Promise<void> {
	const results = await Promise.allSettled([...loaded].reverse().map((item) => item.dispose()));
	const errors = results.flatMap((result) => result.status === "rejected" ? [result.reason] : []);
	if (errors.length === 1) throw errors[0];
	if (errors.length > 1) throw new AggregateError(errors, "Failed to dispose loaded facets");
}

function toError(error: unknown): Error {
	return error instanceof Error ? error : new Error(String(error));
}
