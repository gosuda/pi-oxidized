# Extensions

Ported from `.references/pi/packages/coding-agent/docs/extensions.md` at pin
`8fa7eebd`, at reduced depth. Claims below are bound to the evidence manifest
[evidence/extensions.json](evidence/extensions.json); the full extension API
reference has not been ported yet and is listed under "Pending port surface"
instead of being described as working.

## What extensions are

Extensions are TypeScript modules that extend pi's behavior: they subscribe to
lifecycle events, register custom tools callable by the LLM, and add commands.
In this port the Rust binary does not embed a JavaScript runtime. It launches
a bundled extension-host executable and speaks a structured JSONL bridge to it
(`crates/pi-ext` is the Rust side, `packages/extension-host` the host side).
Host resolution is source-pinned: the `PI_EXTENSION_HOST` override or the host
installed beside the `pi` binary, never a `PATH` search.

## Loading and discovery

The executed `--help` snapshot of this build documents the loading flags:

```text
pi - AI coding assistant with read, bash, edit, write tools

Usage:
  pi [options] [@files...] [messages...]

Commands:
  pi install <source> [-l]     Install extension source and add to settings
  pi remove <source> [-l]      Remove extension source from settings
  pi uninstall <source> [-l]   Alias for remove
  pi update [source|self|pi]   Update pi, extensions, or model catalogs
<!-- doc-c:fence=extensions.01 source=target/verification/docs-topics/cli-help/pi--help.txt -->
```

`--extension` is repeatable, and explicit `-e` paths still load when
`--no-extensions` skips package and settings discovery. The reference discovery
contract loads extensions from `~/.pi/agent/extensions/*.ts` (global) and
`.pi/extensions/*.ts` (project-local, only after the project is trusted), plus
`extensions` paths from settings. The same snapshot notes that extensions can
widen the CLI surface themselves:

```text
pi - AI coding assistant with read, bash, edit, write tools

Usage:
  pi [options] [@files...] [messages...]

Commands:
  pi install <source> [-l]     Install extension source and add to settings
  pi remove <source> [-l]      Remove extension source from settings
  pi uninstall <source> [-l]   Alias for remove
  pi update [source|self|pi]   Update pi, extensions, or model catalogs
  pi list                      List installed extensions from settings
  pi config [-l]               Open TUI to enable/disable package resources (Tab switches scope)
  pi <command> --help          Show help for install/remove/uninstall/update/list/config
<!-- doc-c:fence=extensions.02 source=target/verification/docs-topics/cli-help/pi--help.txt -->
```

Managing installed extension sources (`pi install` / `remove` / `list`) is
covered in [packages.md](packages.md); the tool gating flags that also apply to
extension tools (`--no-tools`, `--tools`, `--exclude-tools`) are covered in
[usage.md](usage.md).

## A minimal extension

An extension exports a default factory function that receives the extension
API. Adapted from the reference quick start, with a plain JSON Schema for the
tool parameters:

```ts
import type { ExtensionAPI } from ".";

export default function (pi: ExtensionAPI) {
  pi.on("session_start", async (_event, ctx) => {
    ctx.ui.notify("Extension loaded!", "info");
  });

  pi.registerTool({
    name: "greet",
    label: "Greet",
    description: "Greet someone by name",
    parameters: {
      type: "object",
      properties: { name: { type: "string", description: "Name to greet" } },
      required: ["name"],
      additionalProperties: false,
    },
    async execute(_toolCallId, params) {
      return {
        content: [{ type: "text", text: `Hello, ${params.name}!` }],
        details: {},
      };
    },
  });

  pi.registerCommand("hello", {
    description: "Say hello",
    handler: async (args, ctx) => {
      ctx.ui.notify(`Hello ${args || "world"}!`, "info");
    },
  });
}
<!-- doc-c:fence=extensions.03 -->
```

Security note: extensions run with your full system permissions and can
execute arbitrary code. Only install from sources you trust; see
[security.md](security.md) for the project-trust rules that gate project-local
resources.

## Extension host, proven end to end

The e2e-smoke harness builds the extension host, loads a real extension into
the Rust binary through `--extension`, and drives it through session start,
an extension-registered shortcut, dialogs, custom UI, and a reload that must
preserve the extension flag. From the passing run transcript: the resolved
executables,

```text
{
  "check": 11,
  "status": "pass",
  "startedAt": "<ts>",
  "finishedAt": "<ts>",
  "runRoot": "/target/verification/e2e/run-<latest>",
  "machine": {
<!-- doc-c:fence=extensions.04 source=target/verification/docs-topics/harness/e2e-result.json -->
```

and the first two extension steps of that run:

```text
{
  "check": 11,
  "status": "pass",
  "startedAt": "<ts>",
  "finishedAt": "<ts>",
  "runRoot": "/target/verification/e2e/run-<latest>",
  "machine": {
    "platform": "linux",
    "arch": "x64",
    "bunVersion": "<bun>"
  },
  "paths": {
    "rustBinary": "/target/release/pi",
    "extensionHost": "/packages/extension-host/dist/pi-extension-host",
    "extension": "/scripts/verification/extension.ts",
    "typescriptCli": "/.references/pi/packages/coding-agent/src/cli.ts",
    "originalSession": "/target/verification/e2e/run-<latest>/sessions/<ts>.jsonl",
    "forkSession": "/target/verification/e2e/run-<latest>/sessions/<ts>.jsonl"
  },
  "loadGeneration": 5,
  "compatibility": {
    "path": "compatibility.jsonl",
    "markerCount": 32,
    "rustProfile": "rust-compatibility-profile",
    "typescriptProfile": "typescript-compatibility-profile",
    "rustInitialInstance": "<instance>",
    "rustReloadInstance": "<instance>",
    "typescriptInstance": "<instance>"
  },
  "steps": [
    {
      "name": "rust-interactive-tools-steering-compaction",
      "startedAt": "<ts>",
      "finishedAt": "<ts>",
      "detail": {
        "session": "sessions/<ts>.jsonl",
        "entries": 14,
        "sha256": "<sha256>"
      }
    },
    {
      "name": "rust-extension-flag-session-start",
      "startedAt": "<ts>",
      "finishedAt": "<ts>",
      "detail": {
        "profile": "rust-compatibility-profile",
<!-- doc-c:fence=extensions.05 source=target/verification/docs-topics/harness/e2e-result.json -->
```

The shortcut dispatch arrives as the kitty `CSI 120;6u` sequence and lands in
the same extension instance that observed `session_start`. Later steps in the
same transcript cover extension dialogs, custom UI, flag preservation across
reload, and a TypeScript-pinned control instance repeating the session-start
check; the full step list is bound by
[evidence/extensions.json](evidence/extensions.json).

## Pending port surface

- available-imports table, covering which packages an extension may import
  and their purposes (DOC-D)
- full event catalog: startup, resource, session, agent, model, tool,
  user-bash, and input events (unported-feature)
- `ExtensionContext` and `ExtensionCommandContext` reference, and the
  `ExtensionAPI` method reference (`registerTool`, `registerCommand`,
  `sendMessage`, `appendEntry`, `registerShortcut`, `registerFlag`,
  providers, and the rest) (unported-feature)
- custom UI chapter: dialogs, widgets and status, autocomplete providers,
  custom components, custom editor, message and entry renderers
  (unported-feature)
- state management, dynamic tool loading, output truncation, error handling,
  and mode behavior tables (unported-feature)
- extension styles, async factory and shutdown guidance, and the examples
  reference (unported-feature)
