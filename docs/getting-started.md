# Get started from source

This guide takes a clean Linux checkout to one assistant response. It builds the native Rust product and its bundled TypeScript extension host. The repository does not publish pre-built GitHub releases yet.

## What you will do

You will:

1. build the TypeScript extension host and the `pi` binary;
2. provide a Gemini API key through the environment;
3. launch an explicit Google model; and
4. send one prompt and receive a streamed response.

## Prerequisites

You need:

- Git;
- the Rust toolchain and Bun versions pinned in [the compatibility reference](compatibility.md);
- Node.js 24 LTS with npm;
- a C and C++ build toolchain, CMake, and `pkg-config`; and
- a Gemini API key.

The build uses the committed Cargo, Bun, and npm lockfiles. Do not update
dependencies during setup.

## Clone and build

```bash
git clone https://github.com/gosuda/pi-oxidized.git
cd pi-oxidized
mkdir -p .references
git clone https://github.com/earendil-works/pi.git .references/pi-2.0
git -C .references/pi-2.0 checkout --detach 853a80d26c90a14c1886f0ebb8ffaae133ca2185
test "$(git -C .references/pi-2.0 rev-parse HEAD)" = "853a80d26c90a14c1886f0ebb8ffaae133ca2185"
bun install --frozen-lockfile
bun run scripts/reconstruct-provider-data.ts
npm ci --ignore-scripts --prefix .references/pi-2.0
bun run build:extension-host --target x86_64-unknown-linux-gnu
cargo build -p pi --release --locked
target/release/pi --help
```

The host builder compiles and probes the sidecar at
`dist/release/.staging-host/host/x86_64-unknown-linux-gnu/pi-extension-host`.
The Cargo build produces `target/release/pi`. The help command must exit
successfully before you continue.

## Configure Google authentication

`/login` does not complete provider authentication in the current native TUI.
Provide the key through `GEMINI_API_KEY` instead.

Read the key without echoing it or adding it to shell history:

```bash
read -rsp "Enter Gemini API key: " GEMINI_API_KEY && export GEMINI_API_KEY
printf '\n'
```

The export applies only to the current shell and its child processes. Do not
put the key in the command line, repository, or shell profile.

## Start pi

Run the verified host with an explicit provider and model:

```bash
PI_EXTENSION_HOST="$PWD/dist/release/.staging-host/host/x86_64-unknown-linux-gnu/pi-extension-host" \
  target/release/pi --provider google --model gemini-flash-latest
```

The optional theme and analytics wizard runs only when experimental features
are enabled. A normal source build opens the editor directly.

The empty editor shows this readiness text:

```text
pi v0.1.0  •  type a message to begin
```

Enter this prompt:

```text
In one sentence, explain what this repository builds.
```

The first run is complete when an assistant response starts streaming and
finishes without an authentication error. The wording of the model response
can vary.

## Troubleshooting

### Authentication fails

Confirm that `GEMINI_API_KEY` is exported in the same shell that starts `pi`.
Enter it again if you opened a new terminal. Do not print the key while
checking it.

### The model cannot be resolved

Use both arguments from this guide:

```bash
target/release/pi --provider google --model gemini-flash-latest
```

The provider flag does not select a CLI model by itself.

### An extension host error appears

Rebuild and probe the host for the Linux target:

```bash
bun run build:extension-host --target x86_64-unknown-linux-gnu
PI_EXTENSION_HOST="$PWD/dist/release/.staging-host/host/x86_64-unknown-linux-gnu/pi-extension-host" \
  target/release/pi --provider google --model gemini-flash-latest
```

`pi` resolves the host only from `PI_EXTENSION_HOST` or a sibling packaged
asset. It never searches `PATH`. Do not point it at an unrelated executable.

### The interface does not render correctly

Run `pi` in a real terminal. The interactive mode requires a TTY. Use
`target/release/pi --help` to confirm that the binary itself starts outside
the TUI.

## Next steps

- Read the [extension compatibility contract](extension-compatibility-contract.md) before loading TypeScript extensions.
- Read the [performance evidence](performance/PERF-CLOSE-evidence.md) before citing benchmark results.
- Read the [supported-platforms guide](supported-platforms.md) before packaging another target.
- Read [CONTRIBUTING.md](../CONTRIBUTING.md) before changing the workspace or protocol.
