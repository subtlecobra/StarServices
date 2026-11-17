//! Server TLS from the certificate that `sc-admin init` issues.

use std::{path::Path, sync::Arc};

use rustls::{
    pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer},
    ServerConfig,
};

const CERT: &str = "hub.crt";
const KEY: &str = "hub.key";
const CA: &str = "ca.crt";

fn config(data_dir: &Path) -> Result<ServerConfig, String> {
    let read_cert = |name: &str| {
        let path = data_dir.join(name);
        CertificateDer::from_pem_file(&path).map_err(|e| format!("read {}: {e}", path.display()))
    };
    let chain = vec![read_cert(CERT)?, read_cert(CA)?];
    let key_path = data_dir.join(KEY);
    let key = PrivateKeyDer::from_pem_file(&key_path)
        .map_err(|e| format!("read {}: {e}", key_path.display()))?;
    ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS versions: {e}"))?
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .map_err(|e| format!("TLS certificate: {e}"))
}

pub fn plain(data_dir: &Path) -> Result<Arc<ServerConfig>, String> {
    Ok(Arc::new(config(data_dir)?))
}

pub fn http2(data_dir: &Path) -> Result<Arc<ServerConfig>, String> {
    let mut server = config(data_dir)?;
    server.alpn_protocols = vec![b"h2".to_vec()];
    Ok(Arc::new(server))
}
