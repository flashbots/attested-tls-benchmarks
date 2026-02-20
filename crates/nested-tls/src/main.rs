use std::{collections::HashMap, net::IpAddr, sync::Arc};
use tokio_rustls::rustls::{
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
    server::{danger::ClientCertVerifier, WebPkiClientVerifier},
    ClientConfig, RootCertStore, ServerConfig,
};

/// Helper to generate a self-signed certificate for testing
pub fn generate_certificate_chain(
    ip: IpAddr,
) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
    let mut params = rcgen::CertificateParams::new(vec![]).unwrap();
    params.subject_alt_names.push(rcgen::SanType::IpAddress(ip));
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, ip.to_string());

    let keypair = rcgen::KeyPair::generate().unwrap();
    let cert = params.self_signed(&keypair).unwrap();

    let certs = vec![CertificateDer::from(cert)];
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(keypair.serialize_der()));
    (certs, key)
}

/// Helper to generate TLS configuration for testing
///
/// For the server: A given self-signed certificate
/// For the client: A root certificate store with the server's certificate
pub fn generate_tls_config(
    certificate_chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> (ServerConfig, ClientConfig) {
    let server_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificate_chain.clone(), key)
        .expect("Failed to create rustls server config");

    let mut root_store = RootCertStore::empty();
    root_store.add(certificate_chain[0].clone()).unwrap();

    let mut client_config = ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    (server_config, client_config)
}

#[tokio::main]
async fn main() {
    let (cert_chain, private_key) = generate_certificate_chain("127.0.0.1".parse().unwrap());
    let (outer_server_config, outer_client_config) =
        generate_tls_config(cert_chain.clone(), private_key);

    let outer_acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(outer_server_config));
    let outer_connector = tokio_rustls::TlsConnector::from(Arc::new(outer_client_config));

    let inner_acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(outer_server_config));
    let inner_connector = tokio_rustls::TlsConnector::from(Arc::new(outer_client_config));

    // Inner TLS setup
    let cert_and_key = generate_self_signed_cert("127.0.0.1".parse().unwrap()).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server_addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        let (tcp_stream, _) = listener.accept().await.unwrap();

        // Do outer TLS handshake
        let tls_stream = outer_acceptor.accept(tcp_stream).await.unwrap();

        // Do inner TLS
        let tls_stream_inner = inner_acceptor.accept(tls_stream).await.unwrap();
    });

    let client_tcp_stream = tokio::net::TcpStream::connect(&server_addr).await.unwrap();

    // Outer TLS handshake
    let server_name = ServerName::try_from(server_addr.ip().to_string()).unwrap();
    let tls_stream = outer_connector
        .connect(server_name, client_tcp_stream)
        .await
        .unwrap();

    // Inner TLS
    let inner_stream = inner_connector.connect(&server_addr.to_string(), tls_stream);
}
