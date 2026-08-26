/**
 * PAR-CLI-PROTO fetch shim — preload script.
 *
 * Intercepts globalThis.fetch calls to known OAuth endpoint domains and
 * returns pinned fixture responses instead of making real network requests.
 * Loaded via `bun --preload` (or NODE_OPTIONS=--import) before the upstream
 * dist/cli.js runs, so the CLI's OAuth flows execute entirely hermetically.
 *
 * Provider is selected via MOCK_PROVIDER env var; fixture path via
 * MOCK_FIXTURE_PATH.  Call counts per route key are tracked so polling
 * endpoints can return "pending" on the first call and "complete" on the
 * second.
 *
 * For browser-PKCE flows the CLI also starts a real loopback callback server
 * (node:http).  That server is NOT intercepted — it runs normally.  The
 * scripted stdin provides the authorization code via the manual_code prompt,
 * so the callback server is never exercised.
 */

type FixtureRoute = {
	method: string;
	pattern: string; // e.g. "auth.x.ai/oauth2/device/code" — hostname + pathname
	responses: Array<{
		status?: number;
		headers?: Record<string, string>;
		body: unknown; // JSON object or string
	}>;
};

type Fixture = {
	provider: string;
	routes: FixtureRoute[];
};

// ── env ──────────────────────────────────────────────────────────────────
const provider = process.env.MOCK_PROVIDER;
const fixturePath = process.env.MOCK_FIXTURE_PATH;

if (!provider || !fixturePath) {
	throw new Error("fetch-shim requires MOCK_PROVIDER and MOCK_FIXTURE_PATH env vars");
}

// ── load fixture ──────────────────────────────────────────────────────────
const fixtureText = await Bun.file(fixturePath).text();
const fixture: Fixture = JSON.parse(fixtureText);

// ── call-count tracker ────────────────────────────────────────────────────
const callCounts: Map<string, number> = new Map();

function nextCallIndex(key: string): number {
	const idx = (callCounts.get(key) ?? 0) + 1;
	callCounts.set(key, idx);
	return idx;
}

// ── OAuth domains to intercept ─────────────────────────────────────────────
const oauthDomains = new Set([
	"auth.x.ai",
	"github.com",
	"api.github.com",
	"api.individual.githubcopilot.com",
	"claude.ai",
	"platform.claude.com",
	"openrouter.ai",
	"auth.openai.com",
	"radius.pi.dev",
	"auth.kimi.com",
]);

// ── original fetch ─────────────────────────────────────────────────────────
const originalFetch = globalThis.fetch.bind(globalThis);

// ── route matching ─────────────────────────────────────────────────────────
function matchRoute(method: string, hostname: string, pathname: string): FixtureRoute | undefined {
	const target = `${method.toUpperCase()} ${hostname}${pathname}`;
	for (const route of fixture.routes) {
		if (target === `${route.method.toUpperCase()} ${route.pattern}`) {
			return route;
		}
	}
	// Fallback: partial pathname match (for dynamic paths like /models/{id}/policy)
	for (const route of fixture.routes) {
		const routeMethod = route.method.toUpperCase();
		const routePattern = route.pattern;
		if (method.toUpperCase() === routeMethod && pathname.startsWith(routePattern)) {
			return route;
		}
	}
	return undefined;
}

// ── build mock Response ────────────────────────────────────────────────────
function buildResponse(route: FixtureRoute, callIdx: number): Response {
	const responses = route.responses;
	// Clamp to last response for calls beyond the array length
	const idx = Math.min(callIdx - 1, responses.length - 1);
	const resp = responses[idx];
	const status = resp.status ?? 200;
	const headers = resp.headers ?? { "content-type": "application/json" };

	let body: string;
	if (typeof resp.body === "string") {
		body = resp.body;
	} else {
		body = JSON.stringify(resp.body);
	}

	return new Response(body, {
		status,
		headers: { ...headers },
	});
}

// ── patched fetch ──────────────────────────────────────────────────────────
globalThis.fetch = async (input: string | URL | Request, init?: RequestInit): Promise<Response> => {
	let url: URL;
	let method: string;

	if (input instanceof Request) {
		url = new URL(input.url);
		method = init?.method ?? input.method ?? "GET";
	} else {
		url = new URL(typeof input === "string" ? input : input.toString());
		method = init?.method ?? "GET";
	}

	if (oauthDomains.has(url.hostname)) {
		const route = matchRoute(method, url.hostname, url.pathname);
		if (route) {
			const key = `${method.toUpperCase()} ${url.hostname}${url.pathname}`;
			const callIdx = nextCallIndex(key);
			return buildResponse(route, callIdx);
		}
		// If no route matches but domain is known, fall through to real fetch
		// (shouldn't happen in hermetic mode — indicates a missing fixture)
		console.error(`[fetch-shim] WARNING: no fixture route for ${method} ${url.hostname}${url.pathname}`);
	}

	return originalFetch(input as RequestInfo | URL, init);
};

// Signal that the shim is active
if (process.env.MOCK_SHIM_DEBUG === "1") {
	console.error(`[fetch-shim] active for provider=${provider} fixture=${fixturePath}`);
}
