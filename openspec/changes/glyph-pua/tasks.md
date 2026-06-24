## 1. Extend GlyphRegistry to store rasterised bitmaps

- [x] 1.1 Add a failing test in `st-glyph` (`register_shape_stores_bitmap_keyed_by_codepoint`): registering a valid PNG via `register_shape(id, GlyphFormat::Png, bytes)` returns a PUA codepoint and `bitmap(codepoint)` yields a `GlyphAtlasEntry` whose `codepoint` equals the returned char
- [x] 1.2 Add a failing test (`register_shape_is_idempotent_and_replaces_bitmap`): re-registering the same id returns the same codepoint and updates the stored bitmap
- [x] 1.3 Add a failing test (`register_shape_rasterise_failure_keeps_mapping_without_bitmap`): an undecodable PNG returns a codepoint but `bitmap(codepoint)` is `None`
- [x] 1.4 Add `bitmaps: HashMap<char, GlyphAtlasEntry>` to `GlyphRegistry`; implement `register_shape(&mut self, id, format, data) -> char` (assign/reuse codepoint, rasterise via `rasterize_svg`/`decode_png`, set `GlyphAtlasEntry.codepoint`, store) and `bitmap(&self, cp: char) -> Option<&GlyphAtlasEntry>`
- [x] 1.5 Run `cargo test -p st-glyph` — green

## 2. Rasterise on APC registration in st-pty

- [x] 2.1 Add a failing test in `st-pty` driving an APC `SMEDJA_GLYPH` PNG payload through the scanner-and-register path so the shared registry has a `bitmap` for the assigned codepoint (extend the existing scanner test ~`lib.rs:1985`)
- [x] 2.2 In both reader threads (`lib.rs` ~1294 and ~1349) replace `registry.register(&reg.id)` with `registry.register_shape(&reg.id, reg.format.clone(), &reg.data)`
- [x] 2.3 On rasterisation yielding no bitmap, keep the `debug!`/add a `warn!` and continue (registration still maps the id)
- [x] 2.4 Run `cargo test -p st-pty` — green

## 3. Tier/status glyph-id resolution helper

- [x] 3.1 Add a failing test (`glyph_id_for_tier_maps_known_tiers` and `glyph_id_for_unknown_returns_none`) for a `glyph_id_for_tier(&str) -> Option<&'static str>` (and status variant) helper in `st-glyph`
- [x] 3.2 Implement `glyph_id_for_tier` / `glyph_id_for_status` mapping `local|fast|deep` and `ok|fail|pending` to the built-in IDs in `BUILTIN_GLYPHS`
- [x] 3.3 Add a failing test asserting badge resolution falls back to `fallback_text(id)` when `lookup(id)` is `None` or `supports_apc(term)` is false, and yields the PUA `char` otherwise
- [x] 3.4 Run `cargo test -p st-glyph` — green

## 4. RGBA registered-glyph atlas in st-render

- [x] 4.1 Add a failing test (`registered_rgba_bitmap_round_trips_colour`) using the pure-logic atlas helpers: a registered RGBA bitmap allocated into the colour atlas preserves its RGB channels (not just alpha)
- [x] 4.2 Add an `Rgba8UnormSrgb` colour atlas texture + `ShelfPacker` + `HashMap<char, GlyphEntry>` to `GlyphAtlas` for registered glyphs
- [x] 4.3 In `get_or_insert`, when `ch` is in `U+E000..=U+F8FF` and the registry has `bitmap(ch)`, allocate in the colour atlas and `write_texture` the entry's `pixels`; otherwise fall through to the existing cosmic-text mask path unchanged
- [x] 4.4 Thread an `Arc<Mutex<GlyphRegistry>>` reference into the atlas / `ensure_cell_glyphs` so PUA codepoints can be resolved
- [x] 4.5 Run `cargo test -p st-render` — green

## 5. Render-path selection and wiring

- [x] 5.1 Make the per-glyph draw sample the colour atlas (its bind group/sampler) when the codepoint resolved to a registered bitmap, and the alpha atlas otherwise
- [x] 5.2 In `ensure_cell_glyphs`, consult the registry for PUA cells so registered glyphs are warmed alongside font glyphs
- [x] 5.3 In `term/bin/smedja/src/main.rs`, pass an `Arc::clone(&pty.glyph_registry)` into the renderer after `register_builtin_glyphs`
- [x] 5.4 Add a failing-then-passing test (or pure-logic assertion) that a cell containing a registered PUA codepoint selects the colour atlas, and a normal ASCII cell selects the alpha atlas

## 6. Tier-badge / block-decoration integration

- [x] 6.1 Add a failing test that a block/status with `tier = "deep"` resolves to the `smedja.tier.deep` PUA codepoint via the registry, and to `fallback_text` when APC is unsupported
- [x] 6.2 Wire the status-bar/block-decoration rendering to call `glyph_id_for_tier`/`glyph_id_for_status` then `registry.lookup` to place the PUA codepoint (or fallback text)
- [x] 6.3 Run `cargo test` for the touched crates — green

## 7. Verify

- [x] 7.1 Run `cargo test --workspace` — all green
- [x] 7.2 Run `cargo clippy -p st-glyph -p st-pty -p st-render -- -D warnings` — clean for the touched code
- [x] 7.3 Update the README Glyph Protocol section to describe the end-to-end registration → render flow accurately
- [x] 7.4 Run `openspec validate glyph-pua --strict` — clean
