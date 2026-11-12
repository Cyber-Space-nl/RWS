#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    rsw_core::run(rsw_core::Flavor::Comfy).await
}
