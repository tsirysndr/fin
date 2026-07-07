//! Run the MediaRenderer on its own for protocol debugging:
//!
//!   cargo run --release -p fin-mediarenderer --example standalone
//!
//! Then probe it with a UPnP control point (BubbleUPnP, Kodi, `gupnp-av-cp`)
//! or plain curl against the printed description URL.

use std::sync::Arc;

use fin_mediarenderer::{MediaRendererServer, Options, RendererCell};
use fin_player::{LocalRenderer, Renderer};
use parking_lot::Mutex;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("debug")
        .with_writer(std::io::stderr)
        .init();

    let renderer: Arc<dyn Renderer> = Arc::new(LocalRenderer::new());
    let cell: RendererCell = Arc::new(Mutex::new(renderer));
    let server = MediaRendererServer::start(
        Options {
            friendly_name: "fin standalone".into(),
            uuid: "f1a2b3c4-d5e6-4788-99aa-bbccddeeff00".into(),
            port: 47899,
        },
        cell,
    )
    .await?;

    println!("description: http://{}/description.xml", server.http_addr());
    tokio::signal::ctrl_c().await?;
    server.shutdown().await;
    Ok(())
}
