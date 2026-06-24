## Why

The Glyph Protocol — custom glyphs (tier badges, status icons, block decorations) registered via APC escape sequences that map vector shapes to Unicode Private Use Area (PUA) codepoints, so no Nerd Font patches are needed — is specified and partially scaffolded but its core promise is not wired. A child process can emit a `SMEDJA_GLYPH` registration, but its **shape data never reaches the screen**:

- `st_glyph::parse_glyph_registration` decodes the full `GlyphRegistration { id, format, data }` (SVG or base64-PNG bytes), but the PTY reader (`st-pty/src/lib.rs` ~line 1294 and the second reader ~line 1349) calls only `registry.register(&reg.id)` — it assigns a PUA codepoint and **discards `reg.data`**. The vector/raster bytes are never rasterised or stored.
- `st_glyph::rasterize_svg` and `st_glyph::decode_png` exist and produce a `GlyphAtlasEntry { codepoint, pixels, width, height }` in **RGBA**, but nothing calls them on the registration path and nothing carries the pixels onward.
- The wgpu atlas (`st-render::GlyphAtlas`, `st-render/src/lib.rs:277`) has **no knowledge of the registry**. `GlyphAtlas::get_or_insert` (`lib.rs:327`) only shapes a `char` through cosmic-text + swash. A PUA codepoint therefore shapes to a missing-glyph/tofu box; there is no path to inject a registered bitmap into the shelf-packed atlas. The atlas texture is also `R8Unorm` (alpha-only, `lib.rs:291`) while registered glyphs are RGBA — a format gap that must be resolved.
- Tier badges in `st-blocks` store `Block.tier` as a string (`st-blocks/src/lib.rs:88`) but never look up a PUA codepoint via `GlyphRegistry::lookup`; block decorations render no custom glyph.

The result: `register_builtin_glyphs` is called at pane start (`term/bin/smedja/src/main.rs:376`) and `build_glyph_registration_sequence` emits APC bytes, yet a registered glyph displays as a blank or tofu cell. This change wires the PUA registration end-to-end: rasterise the registered shape, store it keyed by PUA codepoint, route a PUA codepoint through the atlas to its stored bitmap, and let tier badges and block decorations resolve glyph IDs to PUA codepoints.

## What Changes

- **Rasterise on registration**: when the PTY reader intercepts a `SMEDJA_GLYPH` APC payload, after assigning the PUA codepoint it SHALL rasterise the shape (`rasterize_svg` for `format=svg`, `decode_png` for `format=png`) and store the resulting `GlyphAtlasEntry` (with its codepoint set) in the registry keyed by PUA codepoint.
- **Extend `GlyphRegistry`** to hold rasterised bitmaps, not just the id→codepoint map: add a `bitmap(codepoint) -> Option<&GlyphAtlasEntry>` lookup and a `register_shape(id, format, data) -> char` that registers and rasterises in one step. Keep `register(id)` for id-only built-in pre-assignment.
- **Route PUA codepoints through the atlas**: `GlyphAtlas::get_or_insert` SHALL, for a codepoint in the PUA range with a registered bitmap, upload the registered RGBA bitmap into a shelf-packed region instead of shaping through cosmic-text. Resolve the R8/RGBA gap by storing registered glyphs as RGBA in a dedicated colour atlas (or converting to the existing atlas) — see design.
- **Tier badges / block decorations resolve glyphs**: provide a helper mapping a `Block.tier` / status string to its built-in glyph ID, look the ID up in the registry, and emit the PUA codepoint (falling back to `fallback_text` when unregistered or the terminal lacks APC support).
- **Cache / eviction**: registered bitmaps are cached for the pane lifetime keyed by PUA codepoint; re-registering an existing id is idempotent (same codepoint, bitmap replaced). PUA exhaustion keeps the existing `U+F8FF` reuse behaviour with a warning.

## Capabilities

### New Capabilities

- `glyph-protocol`: a child process registers a custom glyph via a `SMEDJA_GLYPH` APC sequence; smedja assigns a PUA codepoint, rasterises the vector/PNG shape, caches the bitmap keyed by that codepoint, renders the codepoint by sampling the cached bitmap from the GPU atlas, and resolves tier/status glyph IDs to PUA codepoints with plain-text fallback.

## Impact

- `term/crates/st-glyph/src/lib.rs`: extend `GlyphRegistry` with a codepoint→`GlyphAtlasEntry` store, `register_shape(id, format, data)`, and `bitmap(codepoint)`; set `GlyphAtlasEntry.codepoint` on rasterise.
- `term/crates/st-pty/src/lib.rs`: in both reader threads, call `register_shape` (rasterise + store) instead of `register(id)`; warn-and-skip on rasterisation failure.
- `term/crates/st-render/src/lib.rs`: add a registered-glyph (RGBA) atlas path; `get_or_insert` routes PUA codepoints with a registered bitmap to the bitmap upload instead of cosmic-text; `ensure_cell_glyphs` consults the registry.
- `term/crates/st-blocks/src/lib.rs` (or a small helper module): map tier/status strings to built-in glyph IDs for badge/decoration rendering.
- `term/bin/smedja/src/main.rs`: thread the shared `GlyphRegistry` to the renderer so the atlas can resolve PUA bitmaps.
- README: the Glyph Protocol section becomes accurate (registered glyphs actually render).
