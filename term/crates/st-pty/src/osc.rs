//! OSC (Operating System Command) dispatch.

use tracing::debug;

use crate::grid::CellGrid;
use crate::marker::{BlockMarker, MarkerKind};
use crate::notification::{parse_osc777, parse_osc7_uri, parse_osc9};

/// Handles an OSC sequence, mutating `grid` accordingly.
pub(crate) fn dispatch(grid: &mut CellGrid, params: &[&[u8]]) {
    if params.is_empty() {
        return;
    }
    let command = std::str::from_utf8(params[0]).unwrap_or("");
    match command {
        // OSC 0/2 — set window title and/or icon name.
        "0" | "2" => {
            if let Some(title) = params.get(1).and_then(|b| std::str::from_utf8(b).ok()) {
                grid.title = Some(title.to_owned());
            }
        }
        "8" => {
            // OSC 8 ; params ; uri ST — hyperlink.
            let uri = params.get(2).and_then(|b| std::str::from_utf8(b).ok());
            grid.sgr.url = uri.filter(|s| !s.is_empty()).map(String::from);
        }
        "133" => {
            // OSC 133 — shell integration.
            grid.osc133_seen = true;
            let code = params.get(1).and_then(|b| std::str::from_utf8(b).ok());
            let row = grid.cursor.1;
            match code {
                Some("A") => grid.block_markers.push(BlockMarker {
                    kind: MarkerKind::PromptStart,
                    row,
                }),
                Some("B") => grid.block_markers.push(BlockMarker {
                    kind: MarkerKind::CommandStart,
                    row,
                }),
                Some("C") => grid.block_markers.push(BlockMarker {
                    kind: MarkerKind::CommandExecuted,
                    row,
                }),
                Some(d) if d.starts_with('D') => {
                    let exit_code = d.split(';').nth(1).and_then(|s| s.parse::<i32>().ok());
                    grid.block_markers.push(BlockMarker {
                        kind: MarkerKind::CommandDone { exit_code },
                        row,
                    });
                }
                _ => {}
            }
        }
        "7" => {
            // OSC 7 ; file://hostname/path BEL — current working directory.
            if let Some(uri) = params.get(1).and_then(|b| std::str::from_utf8(b).ok()) {
                if let Some(path) = parse_osc7_uri(uri) {
                    let row = grid.cursor.1;
                    grid.block_markers.push(BlockMarker {
                        kind: MarkerKind::Osc7Cwd { path },
                        row,
                    });
                }
            }
        }
        "9" => {
            // OSC 9 ; <message> ST
            let msg = params
                .get(1)
                .and_then(|b| std::str::from_utf8(b).ok())
                .unwrap_or("");
            if let Some(n) = parse_osc9(msg) {
                debug!("OSC 9 notification: {:?}", n.body);
                grid.notifications.push(n);
            }
        }
        "777" => {
            // OSC 777 ; notify ; <title> ; <body> ST
            // Reconstruct payload as "notify;<title>;<body>"
            let payload = params[1..]
                .iter()
                .filter_map(|b| std::str::from_utf8(b).ok())
                .collect::<Vec<_>>()
                .join(";");
            if let Some(n) = parse_osc777(&payload) {
                debug!(
                    "OSC 777 notification: title={:?} body={:?}",
                    n.title, n.body
                );
                grid.notifications.push(n);
            }
        }
        "52" => {
            // OSC 52 ; Pc ; Pd — clipboard write.
            // Pd is base64-encoded UTF-8 text; "?" means query (not supported).
            if let Some(b64) = params.get(2).and_then(|b| std::str::from_utf8(b).ok()) {
                if b64 != "?" && !b64.is_empty() {
                    use base64::Engine as _;
                    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) {
                        if let Ok(text) = String::from_utf8(bytes) {
                            grid.pending_clipboard_write = Some(text);
                        }
                    }
                }
            }
        }
        _ => {
            debug!("unhandled OSC: {:?}", command);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::grid::CellGrid;
    use crate::vt::VtHandler;
    use parking_lot::Mutex;
    use std::sync::Arc;

    fn make_grid(cols: u16, rows: u16) -> CellGrid {
        CellGrid::new(cols, rows)
    }

    fn make_handler(grid: Arc<Mutex<CellGrid>>) -> VtHandler {
        VtHandler {
            grid,
            glyph_registry: Arc::new(Mutex::new(st_glyph::GlyphRegistry::new())),
        }
    }

    #[test]
    fn vte_osc0_sets_window_title() {
        let grid = Arc::new(Mutex::new(make_grid(80, 24)));
        let mut handler = make_handler(Arc::clone(&grid));
        let mut parser = vte::Parser::new();
        // OSC 0 ; title BEL
        let seq = b"\x1b]0;my terminal title\x07";
        for &byte in seq {
            parser.advance(&mut handler, byte);
        }
        let g = grid.lock();
        assert_eq!(
            g.title.as_deref(),
            Some("my terminal title"),
            "OSC 0 should set the window title"
        );
    }

    #[test]
    fn vte_osc2_sets_window_title() {
        let grid = Arc::new(Mutex::new(make_grid(80, 24)));
        let mut handler = make_handler(Arc::clone(&grid));
        let mut parser = vte::Parser::new();
        // OSC 2 ; title BEL
        let seq = b"\x1b]2;icon title\x07";
        for &byte in seq {
            parser.advance(&mut handler, byte);
        }
        let g = grid.lock();
        assert_eq!(
            g.title.as_deref(),
            Some("icon title"),
            "OSC 2 should set the window title"
        );
    }
}
