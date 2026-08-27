# pi-tui

Product-agnostic terminal components and lifecycle.

## Workspace topology

`pi-tui` is a **dependency-free** leaf crate — it has no workspace dependencies.
It is depended on by `pi-ext` and `pi`.

```
pi-ai  (no workspace deps)
  ↑
pi-agent → pi-ai
pi-ext   → {pi-ai, pi-agent, pi-tui}
pi       → {pi-ai, pi-agent, pi-ext, pi-tui}
```

`pi-tui` is product-agnostic: terminal components remain reusable without
provider, agent, extension, or product policy.

The full topology is owned by the root `AGENTS.md` and generated from workspace
`Cargo.toml` edges so this README and `AGENTS.md` share one source.

## Public modules

| Module | Description |
|---|---|
| `component` | Core component trait |
| `components` | Built-in components |
| `editor_support` | Editor support |
| `focus` | Focus management |
| `frame` | Frame rendering |
| `fuzzy` | Fuzzy matching |
| `image` | Image rendering |
| `keybindings` | Keybinding definitions |
| `keys` | Key detection (e.g. `is_kitty_protocol_active`) |
| `layout` | Layout types (`SizeValue`, `OverlayAnchor`, `OverlayMargin`, `OverlaySpec`) |
| `link` | Hyperlink support |
| `overlay` | Overlay management |
| `terminal` | Terminal abstraction |
| `text` | Text utilities |

## Feature flags

| Feature | Description |
|---|---|
| `testkit` | Exposes `pi_tui::testkit` for integration testing (avt, portable-pty, serde_json, sha2) |

## Handshake symmetry

The handshake asymmetry is documented in
`docs/extension-compatibility-contract.md`, the single owner doc. This README
references it; other docs point there rather than restating it.
