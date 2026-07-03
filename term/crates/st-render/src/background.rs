//! Terminal background image and transparency configuration.

/// Configuration for terminal background image and transparency.
pub struct BackgroundConfig {
    /// Path to the background image file, if configured.
    pub image_path: Option<std::path::PathBuf>,
    /// Window opacity in the range `0.0` (transparent) to `1.0` (opaque).
    pub opacity: f32,
    /// Decoded RGBA pixel data, populated by [`BackgroundConfig::load_image`].
    pub image_pixels: Option<Vec<u8>>,
    /// Width of the loaded image in pixels.
    pub image_width: u32,
    /// Height of the loaded image in pixels.
    pub image_height: u32,
}

impl Default for BackgroundConfig {
    fn default() -> Self {
        Self {
            image_path: None,
            opacity: 1.0,
            image_pixels: None,
            image_width: 0,
            image_height: 0,
        }
    }
}

impl BackgroundConfig {
    /// Loads the image at [`Self::image_path`] into [`Self::image_pixels`].
    ///
    /// Returns an error if no path is configured or the image cannot be opened
    /// or decoded.
    ///
    /// # ponytail
    ///
    /// GPU blit is deferred — pixels are loaded here; the actual draw call is a
    /// `TODO` comment in the render loop.
    ///
    /// # Errors
    ///
    /// Returns a boxed error if the path is absent or the image cannot be read.
    pub fn load_image(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let path = self.image_path.as_ref().ok_or("no image path configured")?;
        let img = image::open(path)?.to_rgba8();
        self.image_width = img.width();
        self.image_height = img.height();
        self.image_pixels = Some(img.into_raw());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_config_default_opacity_is_1() {
        let bg = BackgroundConfig::default();
        assert!((bg.opacity - 1.0).abs() < f32::EPSILON);
        assert!(bg.image_pixels.is_none());
    }

    #[test]
    fn background_config_load_image_nonexistent_returns_err() {
        let mut bg = BackgroundConfig {
            image_path: Some(std::path::PathBuf::from("/nonexistent/path/image.png")),
            ..BackgroundConfig::default()
        };
        assert!(bg.load_image().is_err());
    }

    #[test]
    fn background_config_stores_image_path() {
        let path = std::path::PathBuf::from("/tmp/wall.png");
        let bg = BackgroundConfig {
            image_path: Some(path.clone()),
            ..BackgroundConfig::default()
        };
        assert_eq!(bg.image_path, Some(path));
        assert!(bg.image_pixels.is_none());
    }

    #[test]
    fn background_config_load_image_missing_path_returns_err() {
        let mut bg = BackgroundConfig {
            image_path: Some(std::path::PathBuf::from("/no/such/image.png")),
            ..BackgroundConfig::default()
        };
        assert!(bg.load_image().is_err(), "loading a missing file must fail");
    }
}
