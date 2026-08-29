# extension-host fixture

```ts
import {
	COMPATIBILITY_VERSION,
	ExtensionHost,
	MAX_HYPERLINK_URI_BYTES,
	getExtensionAliases,
	parseAnsiLine,
} from "@earendil-works/pi-extension-host";
import type { TerminalInputHandler } from "@earendil-works/pi-extension-host";

const compatibility: string = COMPATIBILITY_VERSION;
const maxUriBytes: number = MAX_HYPERLINK_URI_BYTES;
const runs = parseAnsiLine("plain");
const aliases = getExtensionAliases();
const HostCtor: typeof ExtensionHost = ExtensionHost;
type Handler = TerminalInputHandler;

void compatibility;
void maxUriBytes;
void runs;
void aliases;
void HostCtor;
type _Handler = Handler;
```

```ts
import type { ExtensionMode, SourceInfo } from "@earendil-works/pi-coding-agent";

const mode: ExtensionMode = "tui";
const source: SourceInfo = {
	path: "fixture.ts",
	source: "fixture",
	scope: "project",
	origin: "top-level",
};

void mode;
void source;
```
