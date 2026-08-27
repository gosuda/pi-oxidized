# Keybindings

Ported from `.references/pi/packages/coding-agent/docs/keybindings.md` at pin
`8fa7eebd`. Claims below are bound to the evidence manifest
[evidence/keybindings.json](evidence/keybindings.json); anything not yet
provable in the Rust port is listed under "Pending port surface" instead of
being described as working.

## Shortcut surface exercised by e2e

The port's interactive key handling is verified by the e2e-smoke harness,
which drove the released `target/release/pi` binary in a real PTY. One check
sends the Kitty keyboard protocol sequence for `ctrl+shift+x` (`CSI 120;6u`)
to the TUI and asserts that the extension-registered shortcut dispatches in
the same extension instance that observed session start. The run passed with
11 checks and status `pass`; see
[evidence/keybindings.json](evidence/keybindings.json) for the transcript
binding. This proves protocol-level key decoding, modifier reporting, and
shortcut dispatch to extensions in this build.

Model cycling is documented on the command line itself: the executed `--help`
snapshot describes `--models` in terms of `Ctrl+P` cycling:

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

Options:
  --provider <name>              Provider name (default: google)
<!-- doc-c:fence=keybindings.01 source=target/verification/docs-topics/cli-help/pi--help.txt -->
```

## Pending port surface

- the full default shortcut table — editor cursor movement, deletion, kill
  ring, clipboard and selection, fullscreen viewport, application, sessions,
  models and thinking, display and message queue, tree navigation, and the
  scoped models selector (TUI-CLOSE)
- `keybindings.json` custom configuration, the `modifier+key` key format, and
  automatic migration of pre-namespaced ids (unported-feature)
- `/reload` to apply keybinding changes without restarting the session
  (unported-feature)
- fullscreen `--tui-mode` viewport bindings and mouse routing
  (unported-feature)
