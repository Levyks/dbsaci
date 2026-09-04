//! TLS (TCPS) helpers for the TNS listener and optional backend connections.

use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;

use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::TlsAcceptor;

use crate::error::{Error, Result};

/// Load a PEM certificate + private key into a `TlsAcceptor`.
pub fn load_tls_acceptor(cert_path: &Path, key_path: &Path) -> Result<TlsAcceptor> {
    let cert_pem = std::fs::read(cert_path)
        .map_err(|e| Error::Protocol(format!("read TLS cert {}: {e}", cert_path.display())))?;
    let key_pem = std::fs::read(key_path)
        .map_err(|e| Error::Protocol(format!("read TLS key {}: {e}", key_path.display())))?;
    let certs = rustls_pemfile::certs(&mut Cursor::new(cert_pem))
        .collect::<std::result::Result<Vec<CertificateDer<'static>>, _>>()
        .map_err(|e| Error::Protocol(format!("parse TLS cert: {e}")))?;
    if certs.is_empty() {
        return Err(Error::Protocol(
            "TLS cert file contained no certificates".into(),
        ));
    }
    let mut keys = rustls_pemfile::pkcs8_private_keys(&mut Cursor::new(&key_pem))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| Error::Protocol(format!("parse TLS key: {e}")))?;
    let key = if let Some(k) = keys.pop() {
        PrivateKeyDer::Pkcs8(k)
    } else {
        let mut rsa = rustls_pemfile::rsa_private_keys(&mut Cursor::new(&key_pem))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::Protocol(format!("parse TLS RSA key: {e}")))?;
        PrivateKeyDer::Pkcs1(
            rsa.pop()
                .ok_or_else(|| Error::Protocol("TLS key file contained no private key".into()))?,
        )
    };
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| Error::Protocol(format!("TLS server config: {e}")))?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

#[cfg(test)]
mod tests {
    #[test]
    fn missing_cert_is_an_error() {
        let err = super::load_tls_acceptor(
            std::path::Path::new("/no/such/cert.pem"),
            std::path::Path::new("/no/such/key.pem"),
        )
        .err()
        .expect("missing cert must error");
        let s = err.to_string();
        assert!(s.contains("read TLS cert") || s.contains("TLS"), "{s}");
    }
}
