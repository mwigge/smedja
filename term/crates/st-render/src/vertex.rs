//! Vertex types and uniform blocks for the render pipelines.

use bytemuck::{Pod, Zeroable};

/// A vertex for the glyph quad pipeline.
///
/// Each cell is rendered as two triangles (a quad); vertices carry screen
/// position, atlas UV coordinates, and a tint colour.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct GlyphVertex {
    /// NDC position `[x, y]`.
    pub position: [f32; 2],
    /// Atlas UV coordinates `[u, v]`.
    pub tex_coords: [f32; 2],
    /// Linear RGBA tint colour.
    pub color: [f32; 4],
}

impl GlyphVertex {
    const ATTRIBS: [wgpu::VertexAttribute; 3] = wgpu::vertex_attr_array![
        0 => Float32x2,
        1 => Float32x2,
        2 => Float32x4,
    ];

    /// Returns the wgpu vertex buffer layout for [`GlyphVertex`].
    #[must_use]
    pub fn layout() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Self>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::ATTRIBS,
        }
    }
}

/// Background vertex: position + colour only.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct BgVertex {
    pub position: [f32; 2],
    pub color: [f32; 4],
}

impl BgVertex {
    const ATTRIBS: [wgpu::VertexAttribute; 2] =
        wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x4];

    #[must_use]
    pub fn layout() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Self>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::ATTRIBS,
        }
    }
}

/// Vertex for the full-screen background image quad.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct BgImageVertex {
    /// NDC position `[x, y]`.
    pub position: [f32; 2],
    /// UV texture coordinates `[u, v]` in `[0, 1]`.
    pub tex_coords: [f32; 2],
}

impl BgImageVertex {
    const ATTRIBS: [wgpu::VertexAttribute; 2] =
        wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2];

    /// Returns the wgpu vertex buffer layout for [`BgImageVertex`].
    #[must_use]
    pub fn layout() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Self>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::ATTRIBS,
        }
    }
}

/// 16-byte uniform block for the background image shader pass.
///
/// `opacity` controls how opaque the image is; the remaining three floats are
/// padding required by the WGSL uniform buffer alignment rules.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub(crate) struct BgImageParams {
    pub(crate) opacity: f32,
    pub(crate) _pad: [f32; 3],
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyph_vertex_layout_stride_matches_size() {
        let layout = GlyphVertex::layout();
        assert_eq!(
            layout.array_stride as usize,
            std::mem::size_of::<GlyphVertex>()
        );
    }

    #[test]
    fn bg_vertex_layout_stride_matches_size() {
        let layout = BgVertex::layout();
        assert_eq!(
            layout.array_stride as usize,
            std::mem::size_of::<BgVertex>()
        );
    }

    #[test]
    fn bg_image_vertex_layout_stride_matches_size() {
        let layout = BgImageVertex::layout();
        assert_eq!(
            layout.array_stride as usize,
            std::mem::size_of::<BgImageVertex>()
        );
    }

    #[test]
    fn bg_image_params_size_is_16() {
        assert_eq!(std::mem::size_of::<BgImageParams>(), 16);
    }
}
