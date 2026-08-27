# Custom providers

Ported from `.references/pi/packages/coding-agent/docs/custom-provider.md` at
pin `8fa7eebd`. In this port a custom provider is registered from an extension
through the extension host; the registration surface is exercised end to end
by the e2e-smoke harness, which registers a verification provider and streams
a scripted model through it. Claims are bound to
[evidence/custom-provider.json](evidence/custom-provider.json).

## Registering from an extension

An extension's default export receives the extension API. The example below
registers a provider whose model answers through a scripted stream; the
import comes from the extension-host root:

```ts
import type { Model } from "pi-ai";
import { pi } from ".";

export default function () {
    pi.registerProvider("my-provider", {
        models: [{ id: "my-model" } as Model],
        stream: async (options) => {
            // yield assistant chunks in response to options.messages
        },
    });
}
<!-- doc-c:fence=custom-provider.01 -->
```

The e2e harness runs exactly this shape: it loads an extension that calls
`registerProvider` with a `verification` provider and a scripted `model`, then
drives flag, shortcut, and session checks against it. The passing run is
recorded under `target/verification/e2e/` with 11 checks green.

## Selecting the provider

With the provider registered, the standard model-selection flags apply:

```text
pi - AI coding assistant with read, bash, edit, write tools

Usage:
  pi [options] [@files...] [messages...]

Commands:
  pi install <source> [-l]     Install extension source and add to settings
  pi remove <source> [-l]      Remove extension source from settings
<!-- doc-c:fence=custom-provider.02 source=target/verification/docs-topics/cli-help/pi--help.txt -->
```

## Pending port surface

- full custom-provider reference (auth, retry, attribution) — unported-feature
- catalog integration for registered providers — unported-feature
