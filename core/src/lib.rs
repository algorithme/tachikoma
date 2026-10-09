#![doc = include_str!("../README.md")]

use std::io;

use tokio::net::TcpListener;

pub mod agent;
mod auth;
pub mod clients;
mod common;
mod execution;
pub mod llm;
mod web;

pub use auth::AuthError;
pub use common::config::{
    AnthropicConfig, OpenAIAuthMode, OpenAIConfig, RetryConfig, SecretString,
};

/// Run the existing local setup and stateless text-invocation application.
pub async fn serve() -> io::Result<()> {
    let config = common::config::Config::from_env()?;
    execution::ensure_ready(&config.docker)
        .await
        .map_err(|error| io::Error::other(error.to_string()))?;
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
    let address = listener.local_addr()?;
    axum::serve(listener, web::build_router(config.llm, address)).await
}
