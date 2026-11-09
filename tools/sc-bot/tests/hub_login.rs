//! The Hub login over TLS against a stand-in Hub: a certificate that chains to the CA the bot is given and names another host
//! (the real client checks the chain and not the name), the framing, `account_login` and `cmsg_ls_goto_loc`.

use std::sync::Arc;

use prost::Message;
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sc_bot::hub::Hub;
use sc_proto::hub::{AccountLogin, AccountLoginResponse, AccountRecord, Envelope, GotoLocation, MatchMakerUpdate};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tokio_rustls::TlsAcceptor;

fn frame(key: &str, payload: Vec<u8>, id: u64) -> Vec<u8> {
    let body = Envelope { key: key.into(), payload, id }.encode_to_vec();
    [&(body.len() as u32).to_le_bytes()[..], &0xDEAD_BEEFu32.to_le_bytes(), &body].concat()
}

#[tokio::test]
async fn the_bot_logs_in_and_is_sent_to_a_shard() {
    let ca_key = KeyPair::generate().unwrap();
    let mut ca = CertificateParams::new(Vec::new()).unwrap();
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca.key_usages = vec![KeyUsagePurpose::KeyCertSign];
    ca.distinguished_name.push(DnType::CommonName, "test CA");
    let ca_cert = ca.self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca, ca_key);
    let hub_key = KeyPair::generate().unwrap();
    let hub_cert = CertificateParams::new(vec!["some-other-host.example".to_string()]).unwrap().signed_by(&hub_key, &issuer).unwrap();

    let directory = std::env::temp_dir().join(format!("sc-bot-hub-test-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let ca_path = directory.join("client.crt");
    std::fs::write(&ca_path, ca_cert.pem()).unwrap();

    let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![CertificateDer::from(hub_cert.der().to_vec())], PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(hub_key.serialize_der())))
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut stream = acceptor.accept(tcp).await.unwrap();
        for _ in 0..2 {
            let mut head = [0u8; 8];
            stream.read_exact(&mut head).await.unwrap();
            assert_eq!(u32::from_le_bytes(head[4..].try_into().unwrap()), 0xDEAD_BEEF);
            let mut body = vec![0u8; u32::from_le_bytes(head[..4].try_into().unwrap()) as usize];
            stream.read_exact(&mut body).await.unwrap();
            let envelope = Envelope::decode(body.as_slice()).unwrap();
            let reply = match envelope.key.as_str() {
                "account_login" => {
                    assert_eq!(AccountLogin::decode(envelope.payload.as_slice()).unwrap().token, "the-token");
                    // A push the bot must skip before the answer.
                    stream.write_all(&frame("login_queue_status", Vec::new(), 0)).await.unwrap();
                    let account = AccountRecord { session: Some("00112233445566778899aabbccddeeff".into()), account_id: Some(7), handle: Some("bot".into()), geid: Some(42), ..Default::default() };
                    frame("account_login_response", AccountLoginResponse { result: 1, account: Some(account), queue_wait: 0, field6: 0, field4: None }.encode_to_vec(), envelope.id)
                }
                "cmsg_ls_goto_loc" => {
                    assert_eq!(GotoLocation::decode(envelope.payload.as_slice()).unwrap().location_id, 0x1234);
                    let update = MatchMakerUpdate { status: 1, game_ip: "192.0.2.7".into(), game_port: 64090, game_location: 0x1234, target_shard_id: "shard-a".into(), ..Default::default() };
                    frame("smsg_do_join_instance", update.encode_to_vec(), envelope.id)
                }
                other => panic!("unexpected {other}"),
            };
            stream.write_all(&reply).await.unwrap();
        }
    });

    let mut hub = Hub::connect(&address, &ca_path).await.expect("a certificate for another host name is accepted");
    let login = hub.login("the-token").await.unwrap();
    assert_eq!((login.session.as_str(), login.account_id, login.handle.as_str(), login.geid), ("00112233445566778899aabbccddeeff", 7, "bot", 42));
    let join = hub.goto_location(0x1234).await.unwrap();
    assert_eq!((join.address.as_str(), join.port, join.shard.as_str()), ("192.0.2.7", 64090, "shard-a"));
    server.await.unwrap();

    // A certificate from another authority is refused.
    let other = KeyPair::generate().unwrap();
    let stranger = CertificateParams::new(Vec::new()).unwrap().self_signed(&other).unwrap();
    std::fs::write(&ca_path, stranger.pem()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![CertificateDer::from(hub_cert.der().to_vec())], PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(hub_key.serialize_der())))
        .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(config));
    tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let _ = acceptor.accept(tcp).await;
    });
    assert!(Hub::connect(&address, &ca_path).await.is_err(), "a certificate that does not chain to the CA is refused");
    std::fs::remove_dir_all(&directory).ok();
}
