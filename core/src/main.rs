#[tokio::main]
async fn main() -> std::io::Result<()> {
    tachikoma_core::serve().await
}
