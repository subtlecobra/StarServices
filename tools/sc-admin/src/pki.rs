//! Private CA and Hub certificate.
//!
//! The Hub is TLS with SSL_VERIFY_PEER against the ROOT store plus a loose `client.crt`, and the
//! verify callback does not check the host name (CDiffusionCryClient ctor 0x147412da0, callback
//! 0x141099120 in 4.10.2). Shipping this CA as the private tree's `client.crt` is enough there.
//!
//! gRPC does check names, and it is reached by IP, so the certificate carries IP SANs. The launcher
//! points gRPC at the same CA through GRPC_DEFAULT_SSL_ROOTS_FILE_PATH.

use std::{fs, net::IpAddr, path::Path};

use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose, SanType};

const CA_CERT: &str = "ca.crt";
const CA_KEY: &str = "ca.key";
const HUB_CERT: &str = "hub.crt";
const HUB_KEY: &str = "hub.key";
const HUB_SANS: &str = "hub.sans";
const DEFAULT_SANS: [&str; 3] = ["localhost", "127.0.0.1", "127.0.0.2"];

fn write_private(path: &Path, text: &str) -> Result<(), String> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    file.write_all(text.as_bytes()).map_err(|e| format!("write {}: {e}", path.display()))
}

fn write_public(path: &Path, text: &str) -> Result<(), String> {
    fs::write(path, text).map_err(|e| format!("write {}: {e}", path.display()))
}

fn san(name: &str) -> Result<SanType, String> {
    match name.parse::<IpAddr>() {
        Ok(address) => Ok(SanType::IpAddress(address)),
        Err(_) => Ok(SanType::DnsName(
            name.to_string().try_into().map_err(|e| format!("bad host name '{name}': {e}"))?,
        )),
    }
}

/// Creates the CA if missing, and reissues the server certificate when `host` is not covered yet.
pub fn ensure(data_dir: &Path, host: &str) -> Result<(), String> {
    let err = |e: rcgen::Error| format!("certificate generation: {e}");
    let (ca_cert_path, ca_key_path) = (data_dir.join(CA_CERT), data_dir.join(CA_KEY));

    if !ca_cert_path.exists() {
        let ca_key = KeyPair::generate().map_err(err)?;
        let mut ca = CertificateParams::new(Vec::new()).map_err(err)?;
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        ca.distinguished_name.push(DnType::CommonName, "scbackend private CA");
        let ca_cert = ca.self_signed(&ca_key).map_err(err)?;
        write_public(&ca_cert_path, &ca_cert.pem())?;
        write_private(&ca_key_path, &ca_key.serialize_pem())?;
    }

    let sans_path = data_dir.join(HUB_SANS);
    let mut names: Vec<String> = fs::read_to_string(&sans_path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    let known = names.len();
    for name in DEFAULT_SANS.iter().copied().chain([host]) {
        if !names.iter().any(|existing| existing == name) {
            names.push(name.to_string());
        }
    }
    if names.len() == known && data_dir.join(HUB_CERT).exists() {
        return Ok(());
    }

    let read = |path: &Path| fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()));
    let ca_key = KeyPair::from_pem(&read(&ca_key_path)?).map_err(err)?;
    let issuer = Issuer::from_ca_cert_pem(&read(&ca_cert_path)?, ca_key).map_err(err)?;
    let hub_key = KeyPair::generate().map_err(err)?;
    let mut hub = CertificateParams::new(Vec::new()).map_err(err)?;
    hub.subject_alt_names = names.iter().map(|name| san(name)).collect::<Result<_, _>>()?;
    hub.distinguished_name.push(DnType::CommonName, "scbackend");
    let hub_cert = hub.signed_by(&hub_key, &issuer).map_err(err)?;
    write_public(&data_dir.join(HUB_CERT), &hub_cert.pem())?;
    write_private(&data_dir.join(HUB_KEY), &hub_key.serialize_pem())?;
    write_public(&sans_path, &names.join("\n"))
}

pub fn ca_pem(data_dir: &Path) -> Result<String, String> {
    let path = data_dir.join(CA_CERT);
    fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_host_reissues_the_certificate_but_keeps_the_ca() {
        let dir = std::env::temp_dir().join(format!("scbackend-pki-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        ensure(&dir, "127.0.0.2").unwrap();
        let ca = ca_pem(&dir).unwrap();
        let first = fs::read(dir.join(HUB_CERT)).unwrap();
        ensure(&dir, "127.0.0.2").unwrap();
        assert_eq!(first, fs::read(dir.join(HUB_CERT)).unwrap(), "unchanged host must not reissue");
        ensure(&dir, "192.168.1.20").unwrap();
        assert_ne!(first, fs::read(dir.join(HUB_CERT)).unwrap());
        assert_eq!(ca, ca_pem(&dir).unwrap());
        fs::remove_dir_all(dir).unwrap();
    }
}
