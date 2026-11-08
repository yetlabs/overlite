//! TLS configuration from the environment.

use crate::error::{Result, errf};
use overlite_core::SqlState;
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use std::sync::Arc;

fn fatal(msg: String) -> ! {
    log::error!("{msg}");
    std::process::exit(1);
}

fn getenv(key: &str) -> String {
    std::env::var(key).unwrap_or_default()
}

/// Builds the server's TLS config from the environment, or None (TLS disabled,
/// SSLRequest declined). Config:
///
///   - POSTGRES_SSL_CERT + POSTGRES_SSL_KEY: PEM cert/key files to serve.
///   - POSTGRES_SSL=on|true|1|selfsigned: enable with an in-memory self-signed
///     certificate (handy for dev / internal networks; clients use
///     sslmode=require, which encrypts without verifying the cert).
pub(crate) fn load_tls() -> Option<Arc<ServerConfig>> {
    let (cert_file, key_file) = (getenv("POSTGRES_SSL_CERT"), getenv("POSTGRES_SSL_KEY"));
    if !cert_file.is_empty() && !key_file.is_empty() {
        return match load_key_pair(&cert_file, &key_file) {
            Ok(cfg) => Some(cfg),
            Err(e) => fatal(format!("tls: load cert/key: {e}")),
        };
    }

    match getenv("POSTGRES_SSL").as_str() {
        "on" | "true" | "1" | "selfsigned" => match self_signed_tls() {
            Ok(cfg) => Some(cfg),
            Err(e) => fatal(format!("tls: self-signed cert: {e}")),
        },
        _ => None,
    }
}

/// `tls.LoadX509KeyPair` plus the config around it.
fn load_key_pair(cert_file: &str, key_file: &str) -> Result<Arc<ServerConfig>> {
    let cert_pem = std::fs::read(cert_file)?;
    let key_pem = std::fs::read(key_file)?;
    let certs = rustls_pemfile::certs(&mut cert_pem.as_slice()).collect::<std::result::Result<Vec<_>, _>>()?;
    if certs.is_empty() {
        return Err(errf!(
            SqlState::INTERNAL_ERROR,
            "failed to find any PEM data in certificate input"
        ));
    }
    let key = rustls_pemfile::private_key(&mut key_pem.as_slice())?
        .ok_or_else(|| errf!(SqlState::INTERNAL_ERROR, "failed to find any PEM data in key input"))?;
    server_config(certs, key)
}

fn server_config(certs: Vec<CertificateDer<'static>>, key: PrivateKeyDer<'static>) -> Result<Arc<ServerConfig>> {
    let cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| errf!(SqlState::INTERNAL_ERROR, "{}", e))?;
    Ok(Arc::new(cfg))
}

/// Generates an ephemeral self-signed certificate for localhost.
fn self_signed_tls() -> Result<Arc<ServerConfig>> {
    let fail = |e: rcgen::Error| errf!(SqlState::INTERNAL_ERROR, "{}", e);
    let priv_key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).map_err(fail)?;

    // rcgen's times are `time::OffsetDateTime`; build them from the epoch.
    let at = |t: chrono::DateTime<chrono::Utc>| {
        rcgen::date_time_ymd(1970, 1, 1) + std::time::Duration::from_secs(t.timestamp() as u64)
    };
    let now = chrono::Utc::now();
    let mut tmpl = rcgen::CertificateParams::default();
    tmpl.serial_number = Some(rcgen::SerialNumber::from(vec![1u8]));
    tmpl.distinguished_name = rcgen::DistinguishedName::new();
    tmpl.distinguished_name.push(rcgen::DnType::CommonName, "overlite");
    tmpl.not_before = at(now - chrono::Duration::hours(1));
    tmpl.not_after = at(now.checked_add_months(chrono::Months::new(120)).unwrap_or(now));
    tmpl.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
    tmpl.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    tmpl.is_ca = rcgen::IsCa::ExplicitNoCa;
    tmpl.subject_alt_names = vec![
        rcgen::SanType::DnsName("localhost".to_string().try_into().map_err(fail)?),
        rcgen::SanType::IpAddress(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
        rcgen::SanType::IpAddress(std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)),
    ];
    let cert = tmpl.self_signed(&priv_key).map_err(fail)?;
    let der = cert.der().clone();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(priv_key.serialize_der()));
    server_config(vec![der], key)
}
