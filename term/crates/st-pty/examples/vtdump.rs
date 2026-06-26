//! `vtdump` — feed raw VT bytes on stdin, print the resulting grid snapshot.
//!
//! Used by `scripts/smoke-term.sh` to diff recorded terminal byte streams
//! (e.g. captured claude-cli / vim / less sessions) against golden snapshots.
//! Grid size defaults to 80×24, overridable via `VT_COLS` / `VT_ROWS`.
//!
//! Usage: `cargo run --example vtdump -p st-pty < fixture.vt`

use std::io::Read as _;

fn main() {
    let cols: u16 = std::env::var("VT_COLS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(80);
    let rows: u16 = std::env::var("VT_ROWS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(24);

    let mut bytes = Vec::new();
    std::io::stdin()
        .read_to_end(&mut bytes)
        .expect("read stdin");

    println!("{}", st_pty::render_vt_snapshot(cols, rows, &bytes));
}
