use anyhow::Result;

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    // TODO: initialise winit event loop, wgpu surface, PTY, block model
    println!("smedja-term — coming soon");
    Ok(())
}
