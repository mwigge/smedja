//! WGSL shader sources for the render pipelines.

/// Alpha-atlas glyph shader: samples the R8 atlas and tints by cell foreground.
pub(crate) const SHADER_SRC: &str = r"
struct VertexInput {
    @location(0) position:   vec2<f32>,
    @location(1) tex_coords: vec2<f32>,
    @location(2) color:      vec4<f32>,
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) tex_coords: vec2<f32>,
    @location(1) color:      vec4<f32>,
}

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.clip_position = vec4<f32>(in.position, 0.0, 1.0);
    out.tex_coords    = in.tex_coords;
    out.color         = in.color;
    return out;
}

@group(0) @binding(0) var t_atlas:  texture_2d<f32>;
@group(0) @binding(1) var s_atlas:  sampler;

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let alpha = textureSample(t_atlas, s_atlas, in.tex_coords).r;
    return vec4<f32>(in.color.rgb, in.color.a * alpha);
}
";

/// Shader for registered glyphs: samples the RGBA colour atlas directly so the
/// glyph keeps its own colours (only the cell foreground alpha modulates it).
pub(crate) const COLOUR_SHADER_SRC: &str = r"
struct VertexInput {
    @location(0) position:   vec2<f32>,
    @location(1) tex_coords: vec2<f32>,
    @location(2) color:      vec4<f32>,
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) tex_coords: vec2<f32>,
    @location(1) color:      vec4<f32>,
}

@vertex
fn vs_colour(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.clip_position = vec4<f32>(in.position, 0.0, 1.0);
    out.tex_coords    = in.tex_coords;
    out.color         = in.color;
    return out;
}

@group(0) @binding(0) var t_colour: texture_2d<f32>;
@group(0) @binding(1) var s_colour: sampler;

@fragment
fn fs_colour(in: VertexOutput) -> @location(0) vec4<f32> {
    let texel = textureSample(t_colour, s_colour, in.tex_coords);
    return vec4<f32>(texel.rgb, texel.a * in.color.a);
}
";

/// Background quad shader: position + colour, no texture.
pub(crate) const BG_SHADER_SRC: &str = r"
struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) color:    vec4<f32>,
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) color: vec4<f32>,
}

@vertex
fn vs_bg(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.clip_position = vec4<f32>(in.position, 0.0, 1.0);
    out.color = in.color;
    return out;
}

@fragment
fn fs_bg(in: VertexOutput) -> @location(0) vec4<f32> {
    return in.color;
}
";

/// Full-screen background image shader with an opacity uniform.
pub(crate) const BG_IMAGE_SHADER_SRC: &str = r"
struct VIn {
    @location(0) position:   vec2<f32>,
    @location(1) tex_coords: vec2<f32>,
}
struct VOut {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) tex_coords: vec2<f32>,
}
@vertex fn vs_bg_img(in: VIn) -> VOut {
    var out: VOut;
    out.clip_pos  = vec4<f32>(in.position, 0.0, 1.0);
    out.tex_coords = in.tex_coords;
    return out;
}

@group(0) @binding(0) var t_bg: texture_2d<f32>;
@group(0) @binding(1) var s_bg: sampler;

struct Params { opacity: f32, _p1: f32, _p2: f32, _p3: f32 }
@group(1) @binding(0) var<uniform> params: Params;

@fragment fn fs_bg_img(in: VOut) -> @location(0) vec4<f32> {
    let c = textureSample(t_bg, s_bg, in.tex_coords);
    return vec4<f32>(c.rgb, c.a * params.opacity);
}
";
