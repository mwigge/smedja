//! Renderer error types.

use thiserror::Error;

/// Errors produced by the renderer.
#[derive(Debug, Error)]
pub enum RenderError {
    /// wgpu surface configuration failed.
    #[error("surface error: {0}")]
    Surface(String),
    /// No suitable GPU adapter found.
    #[error("no suitable GPU adapter")]
    NoAdapter,
    /// wgpu device request failed.
    #[error("device request failed: {0}")]
    DeviceRequest(#[from] wgpu::RequestDeviceError),
    /// Surface texture acquire failed.
    #[error("frame acquire error: {0}")]
    Frame(#[from] wgpu::SurfaceError),
    /// Generic render error.
    #[error("render error: {0}")]
    Other(String),
}
