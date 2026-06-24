## Context

The Glyph Protocol lets a child process ship a custom vector/raster shape and have smedja render it inside an ordinary terminal cell, without patching a Nerd Font. The transport is an APC sequence; the rendering target is the existing wgpu glyph atlas. The pieces exist but are not connected:

- **Parser** (`st-glyph/src/lib.rs`): `parse_glyph_registration(payload)` decodes `SMEDJA_GLYPH;id=<id>;format=<svg|png>;data=<base64>` into `GlyphRegistration { id, format, data }`. Unknown fields are ignored; bad base64/format yields `None`.
- **Registry** (`st-glyph/src/lib.rs:187`): `GlyphRegistry` maps `id -> char` over `PUA_START = U+E000 ..= PUA_END = U+F8FF`, assigned sequentially from `next`. `register(id)` is idempotent; exhaustion logs a warning and reuses `U+F8FF`. `lookup(id) -> Option<char>` and `entries()` exist. It holds **no bitmaps**.
- **Rasterisers** (`st-glyph/src/lib.rs`): `rasterize_svg(svg, size) -> Option<Vec<u8>>` (RGBA, currently a solid-fill placeholder via `tiny-skia`; full SVG is out of scope) and `decode_png(png) -> Option<GlyphAtlasEntry>` (RGBA, expands RGB→RGBA). `GlyphAtlasEntry { codepoint, pixels, width, height }` is the rasterised-glyph carrier; `decode_png` leaves `codepoint` as a `U+E000` placeholder the caller must overwrite.
- **APC interception** (`st-pty/src/lib.rs:609`): vte 0.13 routes APC bytes to `Ignore` with no performer callback, so an `ApcScanner` state machine runs alongside the vte parser. On a complete `ESC _ … ESC \`, both reader threads (`lib.rs:1293` and `lib.rs:1349`) call `parse_glyph_registration` then `registry.register(&reg.id)` — **`reg.format` and `reg.data` are dropped**.
- **Atlas** (`st-render/src/lib.rs:277`): `GlyphAtlas` is a 1024×1024 `R8Unorm` (alpha-only) texture with a `ShelfPacker` (`lib.rs:192`) and a `HashMap<(char, bool, bool, u32), GlyphEntry>` cache. `get_or_insert` (`lib.rs:327`) shapes a `char` via cosmic-text + swash and uploads the alpha mask. `ensure_cell_glyphs` (`lib.rs:1036`) warms glyphs for every visible cell `char`. The atlas has no reference to a `GlyphRegistry`.
- **Blocks** (`st-blocks/src/lib.rs:88`): `Block.tier: Option<String>` is persisted but never mapped to a glyph.
- **Startup** (`term/bin/smedja/src/main.rs:375`): `register_builtin_glyphs(&mut reg)` pre-assigns codepoints for the seven built-ins; `build_glyph_registration_sequence` emits APC bytes for downstream panes.

So today a PUA codepoint reaches the atlas only by accident (it is not in any visible cell), and even if it did it would shape to tofu. The missing links are: (1) rasterise + store the registered bitmap, (2) carry the registry into the renderer, (3) make the atlas sample the stored bitmap for a PUA codepoint, (4) resolve tier/status IDs to codepoints.

## Goals / Non-Goals

Goals:
- A child-emitted `SMEDJA_GLYPH` registration results in a cached, rasterised bitmap keyed by its PUA codepoint.
- A PUA codepoint that appears in a rendered cell samples its registered bitmap from the GPU atlas.
- Tier badges and block decorations resolve a tier/status string to a PUA codepoint, with `fallback_text` when unregistered or APC-unsupported.
- Re-registration is idempotent on the codepoint and replaces the bitmap.

Non-Goals:
- Full SVG fidelity. `rasterize_svg` stays a `tiny-skia` solid-fill approximation (full vector rendering via `resvg` is explicitly out of scope, per the crate docs). PNG glyphs render at full fidelity.
- Animated or multi-frame glyphs.
- Persisting the registry across daemon/pane restarts — it is rebuilt per pane, matching the built-in pre-assignment at pane start.
- Cross-pane glyph sharing — each `PtySession` owns its `Arc<Mutex<GlyphRegistry>>`.
- Changing the APC wire format or the `build_glyph_registration_sequence` output (the `codepoint=<hex>` echo for downstream panes is unchanged).

## Decisions

**Decision: APC registration grammar is unchanged; the reader rasterises after assigning the codepoint.**
The inbound grammar stays `SMEDJA_GLYPH;id=<id>;format=<svg|png>;data=<base64>` parsed by `parse_glyph_registration`. In each PTY reader thread the call becomes `registry.register_shape(&reg.id, reg.format, &reg.data)` instead of `register(&reg.id)`. `register_shape` assigns/reuses the PUA codepoint, rasterises, and stores the bitmap under that codepoint.
- Rationale: the parser already produces `format` + `data`; only the reader was discarding them. No wire change keeps `build_glyph_registration_sequence` and any existing emitters compatible.
- Rasterisation failure (bad PNG, zero-size SVG) logs a warning and keeps the id→codepoint mapping with no bitmap, so `lookup` still resolves and the cell falls back to tofu/`fallback_text` rather than failing the registration.

**Decision: PUA codepoint range is the existing `U+E000 ..= U+F8FF`, assigned sequentially, idempotent per id.**
No range change. `register_shape` reuses an existing id's codepoint (replacing its bitmap) so re-registration is idempotent. Exhaustion keeps the current behaviour: warn and reuse `U+F8FF`.
- Rationale: the BMP PUA block has 6400 codepoints — far more than the seven built-ins plus realistic per-pane custom glyphs. Plane-15/16 supplementary PUA is unnecessary and would complicate the `char` key.
- Alternative considered: hashing the id into a stable codepoint. Rejected — sequential assignment is already implemented, simpler, and the codepoint is an internal handle never persisted.

**Decision: vector-shape representation in the registry is a rasterised `GlyphAtlasEntry` (RGBA), not the raw bytes.**
`GlyphRegistry` gains `bitmaps: HashMap<char, GlyphAtlasEntry>` alongside the existing `map: HashMap<String, char>`. `register_shape` rasterises immediately (at a fixed registration size, e.g. 32×32 for SVG; native size for PNG) and stores the RGBA `GlyphAtlasEntry` with its `codepoint` field set. `bitmap(codepoint) -> Option<&GlyphAtlasEntry>` exposes it to the renderer.
- Rationale: rasterise-once-at-registration keeps the render path allocation-free and avoids re-decoding per frame; the renderer only uploads. Storing the entry (not raw bytes) means the renderer never sees `format`/base64.
- Alternative: store raw bytes and rasterise lazily in the atlas. Rejected — pushes `tiny-skia`/`png` decode into the render loop and couples `st-render` to the wire format.

**Decision: registered glyphs render through a dedicated RGBA colour atlas in `st-render`.**
The existing `GlyphAtlas` texture is `R8Unorm` (alpha-only) — correct for font glyphs but lossy for colour badges. Add a parallel RGBA (`Rgba8UnormSrgb`) atlas + `ShelfPacker` for registered glyphs (the bitmap uploader path is the same shelf-pack + `write_texture` already used at `lib.rs:408`). `get_or_insert` checks: if `ch` is in the PUA range **and** the registry has a `bitmap(ch)`, allocate in the RGBA atlas and upload `entry.pixels`; otherwise fall through to the cosmic-text mask path unchanged.
- Rationale: status icons/tier badges are colour (the built-in SVGs carry distinct fills). Forcing them through the alpha atlas would discard colour and tint them with the cell foreground. A second atlas isolates the format without touching the hot font path.
- Alternative: convert RGBA→alpha and tint with foreground colour, reusing the single atlas. Rejected for the default path because it loses the glyph's own colours; may be offered as a degraded mode but not the primary design.
- The renderer's per-glyph draw selects which atlas/bind-group to sample based on whether the codepoint resolved to a registered bitmap.

**Decision: the renderer holds a clone of the PTY's `Arc<Mutex<GlyphRegistry>>`.**
`term/bin/smedja/src/main.rs` already owns `pty.glyph_registry` (`Arc<Mutex<GlyphRegistry>>`). Pass an `Arc::clone` into the renderer (or into `ensure_cell_glyphs` per frame) so the atlas can resolve PUA bitmaps. The registry is locked briefly to read a `bitmap(ch)` and the bytes are copied into the upload.
- Rationale: the registry is already shared/`Arc<Mutex<…>>` for the reader thread; reusing it avoids a second source of truth. Reads are short and off the per-pixel path.

**Decision: tier/status badge resolution lives in a small `glyph_id_for` helper.**
Map domain strings to built-in glyph IDs: tier `"local"|"fast"|"deep"` → `smedja.tier.{local|fast|deep}`; status `ok|fail|pending` → `smedja.status.{…}`. The caller (status bar / block decoration) calls `registry.lookup(id)` to get the PUA `char` to place in the cell; if `lookup` is `None` or `supports_apc(term)` is false, it uses `fallback_text(id)`.
- Rationale: keeps the string→id mapping in one place; reuses the existing `fallback_text` and `supports_apc` degradation helpers; `st-blocks` stays persistence-only (the mapping can live in `st-glyph` or the status-bar crate, not in the DB layer).

**Decision: cache and eviction.**
Bitmaps are cached for the pane lifetime in `GlyphRegistry.bitmaps`, keyed by PUA codepoint; the GPU-side `GlyphEntry` for a registered glyph is cached in the RGBA atlas keyed by the codepoint. Re-registering an id replaces the CPU bitmap and invalidates the GPU entry so the next `get_or_insert` re-uploads. No LRU eviction in this change — the PUA range bounds the count at 6400 and the atlas is 1024×1024; if the RGBA atlas fills, allocation returns `None` and the cell falls back, matching the existing `ShelfPacker` full behaviour.
- Rationale: bounded, simple, and consistent with the font atlas which also never evicts.

## Risks / Trade-offs

- [Risk] A second RGBA atlas adds a texture, bind group, and a draw-selection branch → Mitigation: registered glyphs are rare and small; the branch is a single PUA-range + `HashMap` check already needed for resolution; the font path is untouched.
- [Risk] `rasterize_svg` is a solid-fill placeholder, so SVG badges will not match their full vector art → Mitigation: documented non-goal; PNG glyphs render faithfully; the built-in tier/status colours still read as distinct fills.
- [Risk] Holding the registry mutex during `ensure_cell_glyphs` could contend with the reader thread that registers glyphs → Mitigation: the render side only takes the lock to copy out a `bitmap(ch)` (short read); registration is infrequent and also short.
- [Risk] Format gap (R8 vs RGBA) handled wrong would tint or blank badges → Mitigation: dedicated RGBA atlas with its own sampler; a test asserts a registered RGBA bitmap round-trips its colour, not just its alpha.
- [Risk] Rasterisation failure could silently drop a glyph → Mitigation: warn-and-keep-mapping; the cell falls back to `fallback_text`, and a test covers the failure path returning a codepoint with no bitmap.
- [Risk] PUA exhaustion reuses `U+F8FF`, so two glyphs could collide on one codepoint and one bitmap → Mitigation: pre-existing behaviour; a warning is logged; out of scope to redesign here given the 6400-slot range.
