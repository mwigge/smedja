# smedja Terminal

`smedja` is a GPU-accelerated terminal emulator built in Rust. It knows what a smdjad session is — agent turns render as structured `AgentBlock` widgets, not raw byte streams.

---

## Architecture

| Component | Technology | Role |
|-----------|-----------|------|
| GPU renderer | wgpu (Metal / Vulkan / DX12) | Cell rasterisation, glyph atlas, frame submission |
| Font shaping | `cosmic-text` | Unicode layout, ligatures, bidirectional text |
| Layout | `taffy` flexbox | Split panes, block layout |
| PTY | platform PTY | Subprocess I/O, OSC sequence parsing |

**Startup**: `FontSystem` initialises with an empty database (< 5 ms). System fonts are loaded lazily on first glyph rasterisation. Individual atlas misses are logged as warnings but do not crash the renderer.

---

## Block Model

The smedja terminal renders two kinds of content blocks:

### AgentBlock

An agent turn from smdjad. Rendered with:

- **Header bar**: tier badge, model name, token counts, latency
- **Content area**: the assistant's text output
- **Tool entries**: `▸ tool_name  args  → result` inline
- **Inline diffs**: `+`/`-` coloured lines for `edit_file` calls
- **Cowork gate**: approval prompt inline when cowork mode is on
- **Footer**: `✓ complete - trace: <traceparent>` on completion

### Block (shell)

Standard shell command output — Warp-style blocks. Each command execution produces one block: independently selectable, copyable, and scrollable.

---

## Glyph Protocol

Custom glyphs (tier badges, status icons, block decorations) register without Nerd Font patches via **Application Program Command (APC) sequences**.

A child process emits:

```
ESC _ SMEDJA_GLYPH;id=<id>;format=<svg|png>;data=<base64> ESC \
```

The PTY reader:
1. Assigns the id a PUA codepoint (`U+E000..=U+F8FF`)
2. Rasterises the shape (PNG at native size; SVG as a 32×32 solid-fill approximation)
3. Caches the RGBA bitmap keyed by that codepoint

When a registered codepoint appears in a cell, the renderer samples its bitmap from a dedicated RGBA colour atlas (font glyphs use the alpha-only atlas).

**Degradation**: when the terminal lacks APC support, or the glyph is unregistered, built-in badges fall back to plain-text labels: `[deep]`, `✓`, `✗`, `…`.

**SVG note**: full vector rendering via `resvg` is out of scope. SVG glyphs are approximated as a 32×32 solid fill. Use PNG for faithful rendering.

---

## Status Bar

The terminal's own status bar (distinct from the smedja-tui status bar) is TOML-configured in Starship-compatible module format:

```toml
[statusbar]
format = "{tier} {mode} {session} {git} {time}"

[module.tier]
# style = "bold magenta"

[module.context_pct]
threshold = 80   # turn yellow above 80%, red above 90%
```

smedja-specific modules (`tier`, `model`, `context_pct`, `milliways_task`) sit alongside the standard Starship module set. Existing Starship config is portable — copy your `~/.config/starship.toml` to `.smedja/statusbar.toml` to start.

Modules are evaluated in parallel (rayon) on every render tick with a < 50 ms target.

---

## Config

Config is TOML. A migration tool converts existing WezTerm Lua config:

```sh
smj term convert-wezterm ~/.config/wezterm/wezterm.lua > ~/.config/smedja/config.toml
```

Example config:

```toml
[terminal]
font_size   = 14.0
line_height = 1.2

[font]
family  = "JetBrains Mono"
fallback = ["Noto Sans CJK", "Noto Color Emoji"]

[colors]
# Overrides for the 16-color palette
background = "#0b0d0f"
foreground = "#d0c8b8"

[panes]
split_ratio = 0.5   # for vertical splits

[keybindings]
# additional keybindings in WezTerm syntax
```

---

## Launching smedja-tui

`smedja` opens `smedja-tui` as its default application. You can also open it manually from any shell prompt inside the terminal:

```sh
smedja-tui
smedja-tui --mode review --tier deep
smedja-tui --session <id>
```

`smedja` and `smedja-tui` are independent — `smedja-tui` runs as a normal process inside any terminal, and `smedja` can run any application.

---

## Cowork Approval Gate

In cowork mode, every tool call pauses for approval inline inside the `AgentBlock`. The approval widget (planned roadmap item) will show `y` / `n` / `m` keyboard shortcuts. Currently, approval events display as text lines in the agent block and are confirmed via the TUI's `/approve` command.

---

## OSC Sequences

The terminal handles a subset of OSC sequences beyond the standard set:

| Sequence | Effect |
|----------|--------|
| `OSC 9 ; <text> ST` | Desktop notification (turn-complete notification from smedja-tui) |
| `OSC 8 ; params ; uri ST` | Hyperlink (standard) |
| `APC SMEDJA_GLYPH ; ... ST` | Glyph Protocol registration (see above) |

---

## Current State

| Feature | Status |
|---------|--------|
| Text rendering via `cosmic-text` | Shipped |
| GPU rasterisation (wgpu) | Shipped |
| System font loading (lazy) | Shipped |
| Split panes | Shipped |
| Glyph Protocol (APC) | Shipped |
| Shell blocks (Warp-style) | Shipped |
| AgentBlock with tier/token/trace | Shipped |
| Background image blit | Roadmap |
| Inline cowork approval widget (y/n/m) | Roadmap |
