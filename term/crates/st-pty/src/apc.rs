//! APC (`ESC _ … ESC \`) pre-scanner for the smedja Glyph Protocol.

/// State machine that scans raw PTY bytes for `ESC _ … ESC \` (APC) sequences.
///
/// vte 0.13 routes APC bytes to its `Ignore` state and never fires a
/// performer callback, so this scanner runs alongside the vte parser to
/// intercept smedja Glyph Protocol registrations emitted by child processes.
#[derive(Debug, Default)]
pub(crate) struct ApcScanner {
    state: ApcScanState,
    payload: Vec<u8>,
}

#[derive(Debug, Default)]
enum ApcScanState {
    #[default]
    Ground,
    GotEsc,
    InApc,
    InApcGotEsc,
}

impl ApcScanner {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Feeds one byte into the scanner.
    ///
    /// Returns the completed APC payload bytes when a full `ESC _ … ESC \`
    /// sequence has been received, or `None` otherwise.
    pub(crate) fn advance(&mut self, byte: u8) -> Option<Vec<u8>> {
        match self.state {
            ApcScanState::Ground => {
                if byte == 0x1B {
                    self.state = ApcScanState::GotEsc;
                }
                None
            }
            ApcScanState::GotEsc => {
                if byte == b'_' {
                    self.state = ApcScanState::InApc;
                    self.payload.clear();
                } else {
                    self.state = ApcScanState::Ground;
                }
                None
            }
            ApcScanState::InApc => {
                if byte == 0x1B {
                    self.state = ApcScanState::InApcGotEsc;
                } else {
                    self.payload.push(byte);
                }
                None
            }
            ApcScanState::InApcGotEsc => {
                if byte == b'\\' {
                    let payload = std::mem::take(&mut self.payload);
                    self.state = ApcScanState::Ground;
                    Some(payload)
                } else {
                    // ESC inside APC not followed by '\' — include both bytes in payload.
                    self.payload.push(0x1B);
                    self.payload.push(byte);
                    self.state = ApcScanState::InApc;
                    None
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::sync::Arc;

    #[test]
    fn apc_scanner_extracts_payload_from_complete_sequence() {
        let mut scanner = ApcScanner::new();
        let seq = b"\x1b_hello;world\x1b\\";
        let mut result = None;
        for &byte in seq {
            if let Some(payload) = scanner.advance(byte) {
                result = Some(payload);
            }
        }
        assert_eq!(result.as_deref(), Some(b"hello;world" as &[u8]));
    }

    #[test]
    fn apc_scanner_returns_none_for_incomplete_sequence() {
        let mut scanner = ApcScanner::new();
        for &byte in b"\x1b_incomplete" {
            assert!(scanner.advance(byte).is_none());
        }
    }

    #[test]
    fn apc_scanner_handles_esc_in_payload_not_followed_by_backslash() {
        let mut scanner = ApcScanner::new();
        // ESC followed by 'X' (not backslash) inside APC payload — should be included in payload.
        let seq = b"\x1b_foo\x1bXbar\x1b\\";
        let mut result = None;
        for &byte in seq {
            if let Some(payload) = scanner.advance(byte) {
                result = Some(payload);
            }
        }
        let payload = result.expect("complete APC sequence should yield a payload");
        assert!(
            payload.contains(&b'\x1b'),
            "ESC inside payload should be preserved"
        );
    }

    #[test]
    fn glyph_registration_via_apc_updates_registry() {
        // "PHN2Zy8+" is base64("<svg/>") — hardcoded to avoid adding base64 as test dep
        let mut apc_seq = Vec::new();
        apc_seq.extend_from_slice(b"\x1b_");
        apc_seq.extend_from_slice(b"SMEDJA_GLYPH;id=test.icon;format=svg;data=PHN2Zy8+");
        apc_seq.extend_from_slice(b"\x1b\\");

        let registry = Arc::new(Mutex::new(st_glyph::GlyphRegistry::new()));
        let mut scanner = ApcScanner::new();

        for &byte in &apc_seq {
            if let Some(payload) = scanner.advance(byte) {
                if let Some(reg) = st_glyph::parse_glyph_registration(&payload) {
                    let mut r = registry.lock();
                    r.register(&reg.id);
                }
            }
        }

        assert!(
            registry.lock().lookup("test.icon").is_some(),
            "test.icon should be in the registry after APC registration"
        );
    }

    #[test]
    fn glyph_registration_via_apc_rasterises_and_stores_bitmap() {
        // Hardcoded base64 of a 1×1 RGB PNG so register_shape can decode it to a
        // bitmap without adding base64/png as a dev-dependency.
        const PNG_B64: &str =
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGMQUDAAAACkAGE0Zn1yAAAAAElFTkSuQmCC";

        let mut apc_seq = Vec::new();
        apc_seq.extend_from_slice(b"\x1b_");
        apc_seq.extend_from_slice(
            format!("SMEDJA_GLYPH;id=test.png;format=png;data={PNG_B64}").as_bytes(),
        );
        apc_seq.extend_from_slice(b"\x1b\\");

        let registry = Arc::new(Mutex::new(st_glyph::GlyphRegistry::new()));
        let mut scanner = ApcScanner::new();

        for &byte in &apc_seq {
            if let Some(payload) = scanner.advance(byte) {
                if let Some(reg) = st_glyph::parse_glyph_registration(&payload) {
                    let mut r = registry.lock();
                    r.register_shape(&reg.id, reg.format, &reg.data);
                }
            }
        }

        let r = registry.lock();
        let cp = r
            .lookup("test.png")
            .expect("test.png should be registered after APC registration");
        assert!(
            r.bitmap(cp).is_some(),
            "registered PNG should have a rasterised bitmap keyed by its codepoint"
        );
    }

    #[test]
    fn startup_sequence_contains_apc_bytes_for_builtins() {
        let mut registry = st_glyph::GlyphRegistry::new();
        st_glyph::register_builtin_glyphs(&mut registry);
        let seq = st_glyph::build_glyph_registration_sequence(&registry);
        assert!(
            seq.windows(2).any(|w| w == b"\x1b_"),
            "startup sequence should contain at least one APC introducer"
        );
        assert!(
            seq.windows(2).any(|w| w == b"\x1b\\"),
            "startup sequence should contain at least one string terminator"
        );
    }
}
