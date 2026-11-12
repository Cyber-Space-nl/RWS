/// Prefer the server's existing AWS-LC provider when no process provider has been set.
/// Another embedding application may already have installed its own provider.
pub(crate) fn ensure_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}
