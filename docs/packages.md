# Packages

Ported from `.references/pi/packages/coding-agent/docs/packages.md` at pin
`8fa7eebd23535522c8104166b4f1f959b4e2f10`. Claims below are bound to the
evidence manifest [evidence/packages.json](evidence/packages.json); anything not
yet provable in the Rust port is listed under "Pending port surface" instead of
being described as working.

> **Security:** packages run with full system access. Extensions execute
> arbitrary code, and skills can instruct the model to perform any action,
> including running executables. Review source code before installing
> third-party packages.

## Install and manage

The Commands block from the executed `--help` snapshot:

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
<!-- doc-c:fence=packages.01 source=target/verification/docs-topics/cli-help/pi--help.txt -->
```

The same surface as shell invocations:

```bash
pi install ./my-extension
pi list
pi remove ./my-extension
<!-- doc-c:fence=packages.02 -->
```

By default `install` and `remove` write to user settings
(`~/.pi/agent/settings.json`); `-l` writes project settings
(`.pi/settings.json`) instead. `pi uninstall` is an alias for `remove`. `pi
update` updates pi, extensions, or model catalogs, and `pi list` lists
installed extensions from settings. `pi <command> --help` prints per-command
help. To load an extension for the current run without installing it, use
`--extension`/`-e`, which is repeatable per the options block quoted in
[usage.md](usage.md).

## pi config

The executed `pi config --help` snapshot:

```text
Usage:
  pi config [-l] [--approve|--no-approve]

Open the resource configuration TUI to enable or disable package resources.
Without -l, starts in global settings (~/.pi/agent/settings.json).
Press Tab in the TUI to switch between global and project-local modes.

Options:
  -l, --local       Edit project overrides (.pi/settings.json)
  -a, --approve     Trust project-local files for this command with -l
  -na, --no-approve Ignore project-local files for this command with -l
<!-- doc-c:fence=packages.03 source=target/verification/docs-topics/cli-help/pi-config--help.txt -->
```

`pi config` opens the resource configuration TUI to enable or disable package
resources. Without `-l` it starts in global settings
(`~/.pi/agent/settings.json`); Tab switches between global and project-local
modes. `-l` starts in project overrides (`.pi/settings.json`), and
`--approve`/`--no-approve` trust or ignore project-local files for the command.

## Package sources

The ported package manager parses sources as npm specs (`npm:...`), git URLs,
or local paths, and resolves them against user or project scope. `npmCommand`
in `settings.json` pins the wrapper command used for npm package operations.
The `PI_PACKAGE_DIR` variable overrides the package directory for store paths;
see [environment-variables.md](environment-variables.md). Resource types a
package can carry are covered in [extensions.md](extensions.md),
[skills.md](skills.md), [prompt-templates.md](prompt-templates.md), and
[themes.md](themes.md).

## Pending port surface

- pi.dev package gallery, `pi-package` keyword discoverability, and
  video/image preview metadata — unported-feature
- npm and git store-layout and reconciliation walkthroughs bound to executed
  transcripts — DOC-D
- Package authoring guide (the `pi` manifest key, convention directories,
  dependency bundling) bound to executed fixtures — DOC-D
