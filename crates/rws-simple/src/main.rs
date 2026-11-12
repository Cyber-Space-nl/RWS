#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    rws_core::run(rws_core::Flavor::Simple).await
}
