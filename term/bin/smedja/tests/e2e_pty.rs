//! E2E PTY smoke tests — verifies that the host system can spawn a PTY, write
//! to it, and read back output.  These tests exercise the `portable-pty` layer
//! rather than the full smedja GUI.

use std::io::Read;
use std::sync::mpsc;
use std::time::Duration;

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

#[test]
fn echo_command_output_appears_in_pty() {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty failed");

    let mut builder = CommandBuilder::new("echo");
    builder.arg("smedja-e2e-marker");
    let _child = pair.slave.spawn_command(builder).expect("spawn failed");

    // Drop the library's own slave handle so the master reader sees EOF once the
    // child process also exits and closes its copy of the slave fd.
    drop(pair.slave);

    // Reader thread collects all bytes until EIO/EOF on the master side.
    let mut reader = pair.master.try_clone_reader().expect("try_clone_reader");
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 1024];
        let mut out = Vec::new();
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
            }
        }
        let _ = tx.send(out);
    });

    // Wait for the reader thread to finish (child exits → slave closed → EOF).
    let output = rx.recv_timeout(Duration::from_secs(5)).unwrap_or_default();
    let text = String::from_utf8_lossy(&output);
    assert!(
        text.contains("smedja-e2e-marker"),
        "unexpected output: {text:?}"
    );
}

#[test]
fn pty_spawns_with_custom_dimensions() {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty failed");

    // Query the PTY size — it should reflect what we asked for.
    let size = pair.master.get_size().expect("get_size failed");
    assert_eq!(size.rows, 40);
    assert_eq!(size.cols, 120);
}
