//! The Hub from a client's side: TLS over TCP, frames `[u32 LE length][u32 LE 0xDEADBEEF][envelope]`. The bot logs in with
//! the account token of a `LoginData.json`, asks to go to a location, and gets from the answer the shard to connect to. The
//! session id of the login is what the shard's connect request carries, and the Hub connection must stay open while the
//! bot plays: its end tells the Hub the player left (`services/hub/src/main.rs`).

use std::{path::Path, sync::Arc, time::Duration};

use prost::Message;
use rustls::{
    client::{
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
        WebPkiServerVerifier,
    },
    pki_types::{pem::PemObject, CertificateDer, ServerName, UnixTime},
    CertificateError, ClientConfig, DigitallySignedStruct, Error as TlsError, RootCertStore, SignatureScheme,
};
use sc_proto::hub::{AccountLogin, AccountLoginResponse, Envelope, GotoLocation, MatchMakerUpdate};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use tokio_rustls::{client::TlsStream, TlsConnector};

const FRAME_MAGIC: u32 = 0xDEAD_BEEF;
const MAX_FRAME: usize = 16 * 1024 * 1024;
const LOGIN_SUCCESS: i32 = 1;

/// The client verifies the Hub's certificate against `client.crt` and does not check the name in it.
#[derive(Debug)]
struct IgnoreName(Arc<WebPkiServerVerifier>);

impl ServerCertVerifier for IgnoreName {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        match self.0.verify_server_cert(end_entity, intermediates, server_name, ocsp, now) {
            Err(TlsError::InvalidCertificate(CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. })) => Ok(ServerCertVerified::assertion()),
            other => other,
        }
    }

    fn verify_tls12_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, TlsError> {
        self.0.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, TlsError> {
        self.0.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}

/// The account of a `LoginData.json` that the Hub login needs.
#[derive(Clone, Debug)]
pub struct LoginData {
    pub token: String,
    pub username: String,
    /// `star_network.hostname` and `.port`, when the file has them.
    pub hub: Option<String>,
}

impl LoginData {
    pub fn read(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let json: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let token = json["token"].as_str().ok_or_else(|| format!("{}: no \"token\"", path.display()))?.to_string();
        let hub = match (json["star_network"]["hostname"].as_str(), json["star_network"]["port"].as_u64()) {
            (Some(host), Some(port)) => Some(format!("{host}:{port}")),
            _ => None,
        };
        Ok(Self { token, username: json["username"].as_str().unwrap_or_default().to_string(), hub })
    }
}

/// What the login gave.
#[derive(Clone, Debug)]
pub struct Login {
    pub session: String,
    pub account_id: i32,
    pub handle: String,
    pub geid: u64,
}

/// Where the Hub sent the bot.
#[derive(Clone, Debug)]
pub struct Join {
    pub address: String,
    pub port: i32,
    pub location: u64,
    pub shard: String,
}

pub struct Hub {
    stream: TlsStream<TcpStream>,
    next_id: u64,
}

fn frame(key: &str, payload: &[u8], id: u64) -> Vec<u8> {
    let body = Envelope { key: key.to_string(), payload: payload.to_vec(), id }.encode_to_vec();
    let mut out = (body.len() as u32).to_le_bytes().to_vec();
    out.extend_from_slice(&FRAME_MAGIC.to_le_bytes());
    out.extend_from_slice(&body);
    out
}

impl Hub {
    /// Connects to `address` (`host:port`), trusting the certificate authority in `ca` (the `client.crt` of the client files).
    pub async fn connect(address: &str, ca: &Path) -> Result<Hub, String> {
        let mut roots = RootCertStore::empty();
        for cert in CertificateDer::pem_file_iter(ca).map_err(|e| format!("read {}: {e}", ca.display()))? {
            roots.add(cert.map_err(|e| format!("{}: {e}", ca.display()))?).map_err(|e| format!("{}: {e}", ca.display()))?;
        }
        let verifier = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), Arc::new(rustls::crypto::ring::default_provider()))
            .build()
            .map_err(|e| format!("verifier: {e}"))?;
        let config = ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|e| format!("tls: {e}"))?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(IgnoreName(verifier)))
            .with_no_client_auth();
        let host = address.rsplit_once(':').map_or(address, |(host, _)| host);
        let name = ServerName::try_from(host.to_string()).map_err(|e| format!("hub host {host}: {e}"))?;
        let tcp = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(address))
            .await
            .map_err(|_| format!("hub {address}: connect timed out"))?
            .map_err(|e| format!("hub {address}: {e}"))?;
        let stream = TlsConnector::from(Arc::new(config)).connect(name, tcp).await.map_err(|e| format!("hub {address}: tls: {e}"))?;
        Ok(Hub { stream, next_id: 1 })
    }

    async fn send(&mut self, key: &str, payload: &[u8]) -> Result<u64, String> {
        let id = self.next_id;
        self.next_id += 1;
        self.stream.write_all(&frame(key, payload, id)).await.map_err(|e| format!("hub send: {e}"))?;
        Ok(id)
    }

    async fn receive(&mut self) -> Result<Envelope, String> {
        let mut head = [0u8; 8];
        self.stream.read_exact(&mut head).await.map_err(|e| format!("hub receive: {e}"))?;
        let length = u32::from_le_bytes(head[..4].try_into().unwrap()) as usize;
        if u32::from_le_bytes(head[4..].try_into().unwrap()) != FRAME_MAGIC || length > MAX_FRAME {
            return Err("hub sent a frame that is not one".into());
        }
        let mut body = vec![0u8; length];
        self.stream.read_exact(&mut body).await.map_err(|e| format!("hub receive: {e}"))?;
        Envelope::decode(body.as_slice()).map_err(|e| format!("hub envelope: {e}"))
    }

    /// Frames until one with `key` arrives, within `wait`.
    async fn expect(&mut self, key: &str, wait: Duration) -> Result<Envelope, String> {
        tokio::time::timeout(wait, async {
            loop {
                let envelope = self.receive().await?;
                if envelope.key == key {
                    return Ok(envelope);
                }
            }
        })
        .await
        .map_err(|_| format!("hub: no {key} in {wait:?}"))?
    }

    pub async fn login(&mut self, token: &str) -> Result<Login, String> {
        let payload = AccountLogin { token: token.to_string(), ..Default::default() }.encode_to_vec();
        self.send("account_login", &payload).await?;
        let reply = self.expect("account_login_response", Duration::from_secs(15)).await?;
        let response = AccountLoginResponse::decode(reply.payload.as_slice()).map_err(|e| format!("login response: {e}"))?;
        let account = response.account.unwrap_or_default();
        if response.result != LOGIN_SUCCESS {
            return Err(format!("hub login refused (result {})", response.result));
        }
        let session = account.session.filter(|session| !session.is_empty()).ok_or("hub login gave no session")?;
        Ok(Login { session, account_id: account.account_id.unwrap_or_default(), handle: account.handle.unwrap_or_default(), geid: account.geid.unwrap_or_default() })
    }

    /// Asks to go to `location` (0 for the Hub's default) and returns the shard it names.
    pub async fn goto_location(&mut self, location: u64) -> Result<Join, String> {
        self.send("cmsg_ls_goto_loc", &GotoLocation { location_id: location }.encode_to_vec()).await?;
        let reply = self.expect("smsg_do_join_instance", Duration::from_secs(30)).await?;
        let update = MatchMakerUpdate::decode(reply.payload.as_slice()).map_err(|e| format!("join update: {e}"))?;
        Ok(Join { address: update.game_ip, port: update.game_port, location: update.game_location, shard: update.target_shard_id })
    }
}
