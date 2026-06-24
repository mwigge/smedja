## ADDED Requirements

### Requirement: Registered glyph shapes are rasterised and cached on APC registration

When the PTY reader intercepts a `SMEDJA_GLYPH` APC registration, smedja SHALL assign the glyph ID a Unicode Private Use Area codepoint in `U+E000..=U+F8FF` and SHALL rasterise the carried shape (`format=svg` via `rasterize_svg`, `format=png` via `decode_png`) into an RGBA bitmap cached in the registry keyed by that codepoint. The registration MUST NOT discard the shape data.

#### Scenario: PNG registration is rasterised and cached by codepoint

- **WHEN** a child process emits `ESC _ SMEDJA_GLYPH;id=<id>;format=png;data=<base64> ESC \`
- **THEN** the registry SHALL assign `<id>` a PUA codepoint
- **AND** the registry SHALL hold a rasterised RGBA bitmap retrievable by that codepoint
- **AND** the bitmap's stored codepoint SHALL equal the assigned codepoint

#### Scenario: re-registration is idempotent and replaces the bitmap

- **WHEN** the same glyph ID is registered twice
- **THEN** the assigned PUA codepoint SHALL be identical both times
- **AND** the cached bitmap SHALL be the one from the most recent registration

#### Scenario: rasterisation failure keeps the mapping without a bitmap

- **WHEN** a registration carries shape data that cannot be rasterised
- **THEN** the glyph ID SHALL still resolve to a PUA codepoint
- **AND** no bitmap SHALL be cached for that codepoint

### Requirement: PUA codepoints render from their cached bitmap via the GPU atlas

The renderer SHALL render a cell whose character is a PUA codepoint with a cached registered bitmap by uploading that RGBA bitmap into a shelf-packed colour atlas region and sampling it, rather than shaping the codepoint through the font. A PUA codepoint without a cached bitmap SHALL fall through to the existing font shaping path.

#### Scenario: registered PUA codepoint samples the colour atlas

- **WHEN** a rendered cell contains a PUA codepoint that has a cached registered bitmap
- **THEN** the glyph SHALL be uploaded into the registered-glyph (RGBA) atlas
- **AND** its colour channels SHALL be preserved, not reduced to alpha-only
- **AND** the draw SHALL sample the registered-glyph atlas for that cell

#### Scenario: ordinary character is unaffected

- **WHEN** a rendered cell contains an ordinary (non-PUA) character
- **THEN** the glyph SHALL be rasterised through the existing font path
- **AND** the draw SHALL sample the alpha font atlas for that cell

#### Scenario: unregistered PUA codepoint falls through to the font path

- **WHEN** a rendered cell contains a PUA codepoint with no cached bitmap
- **THEN** the renderer SHALL NOT sample the registered-glyph atlas for that cell
- **AND** it SHALL use the existing font shaping path

### Requirement: Tier badges and status decorations resolve glyph IDs to PUA codepoints

Tier-badge and status-decoration rendering SHALL map a tier or status string to its built-in glyph ID, resolve that ID to a PUA codepoint via the registry, and render the codepoint. When the ID is unregistered or the terminal does not support APC sequences, the rendering SHALL use the plain-text fallback for that ID.

#### Scenario: known tier resolves to its PUA codepoint

- **WHEN** a block or status segment has tier `"deep"` and the `smedja.tier.deep` glyph is registered
- **THEN** the rendered badge SHALL use the PUA codepoint assigned to `smedja.tier.deep`

#### Scenario: unsupported terminal uses plain-text fallback

- **WHEN** a tier or status badge is rendered and the terminal does not support APC sequences
- **THEN** the badge SHALL use the plain-text fallback for the corresponding glyph ID

#### Scenario: unregistered glyph uses plain-text fallback

- **WHEN** a tier or status string maps to a glyph ID that is not registered
- **THEN** the badge SHALL use the plain-text fallback for that glyph ID
