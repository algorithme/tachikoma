use std::io;

use tokio::net::TcpListener;

use common::config::Config;

mod common;
mod web;

#[tokio::main]
async fn main() -> io::Result<()> {
    let config = Config::from_env()?;
    let address = config.bind_addr();
    let listener = TcpListener::bind(address).await.map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("Could not bind http://{address}/: {error}"),
        )
    })?;

    println!(
        "Tachikoma core listening on http://{}/",
        listener.local_addr()?
    );

    axum::serve(listener, web::build_router()).await
}
