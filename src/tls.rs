//! HTTPS for the REST API.
//!
//! The server speaks TLS only.  There is no PKI here: on first run a
//! self-signed certificate is generated and stored next to the config
//! file, and that certificate is what the server presents from then on.
//! Clients will (correctly) warn that the certificate is untrusted —
//! for a LAN annotation appliance the operator accepts the warning once
//! per browser, or pins the certificate out of band.  Proper certificate
//! management (bring-your-own cert, renewal, trust chains) is a v2
//! concern.
//!
//! TLS is provided by the system OpenSSL (`openssl-sys` is pure FFI — it
//! compiles no C), so the aarch64 cross build still needs no C toolchain:
//! the prebuilt libssl comes from the sysroot like every other library.

use std::fs;
use std::path::{Path, PathBuf};

use openssl::asn1::Asn1Time;
use openssl::bn::{BigNum, MsbOption};
use openssl::ec::{EcGroup, EcKey};
use openssl::hash::MessageDigest;
use openssl::nid::Nid;
use openssl::pkey::{PKey, Private};
use openssl::ssl::{SslAcceptor, SslMethod};
use openssl::x509::{X509, X509NameBuilder};

/// Certificate file name inside the config directory.
const CERT_FILE: &str = "annotator-cert.pem";
/// Private key file name inside the config directory.
const KEY_FILE: &str = "annotator-key.pem";

/// How long the generated certificate stays valid.  Self-signed and
/// long-lived so a kiosk never bricks itself; cert lifecycle management
/// is deliberately out of scope until v2.
const CERT_DAYS: u32 = 3650;

/// Build the acceptor for the API server, loading the certificate that
/// sits next to the config file or generating it on first run.
pub fn acceptor_from_config_dir() -> Result<SslAcceptor, String> {
    let cfg = crate::config::config_path();
    let dir = cfg
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let cert = dir.join(CERT_FILE);
    let key = dir.join(KEY_FILE);
    acceptor_from_paths(&cert, &key)
}

/// Build an acceptor that exists only in memory.  Tests use it so they
/// never touch the operator's real config directory.
#[cfg(test)]
pub fn acceptor_ephemeral() -> Result<SslAcceptor, String> {
    let (cert, key) = generate_cert()?;
    build_acceptor(&cert, &key)
}

/// Load `cert`/`key`, generating both when either is missing.
fn acceptor_from_paths(cert: &Path, key: &Path) -> Result<SslAcceptor, String> {
    if cert.exists() && key.exists() {
        let cert_pem = fs::read(cert).map_err(|e| format!("read {}: {e}", cert.display()))?;
        let key_pem = fs::read(key).map_err(|e| format!("read {}: {e}", key.display()))?;
        let x509 = X509::from_pem(&cert_pem)
            .map_err(|e| format!("parse {}: {e} — delete it to regenerate", cert.display()))?;
        let pkey = PKey::private_key_from_pem(&key_pem)
            .map_err(|e| format!("parse {}: {e} — delete it to regenerate", key.display()))?;
        return build_acceptor(&x509, &pkey);
    }

    // First run: generate and persist.  The key gets owner-only
    // permissions like the config itself — it is the API's only secret.
    let (x509, pkey) = generate_cert()?;
    let cert_pem = x509
        .to_pem()
        .map_err(|e| format!("encode the certificate: {e}"))?;
    let key_pem = pkey
        .private_key_to_pem_pkcs8()
        .map_err(|e| format!("encode the private key: {e}"))?;
    fs::write(cert, &cert_pem).map_err(|e| format!("write {}: {e}", cert.display()))?;
    fs::write(key, &key_pem).map_err(|e| format!("write {}: {e}", key.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(key, fs::Permissions::from_mode(0o600));
        let _ = fs::set_permissions(cert, fs::Permissions::from_mode(0o644));
    }
    eprintln!(
        "[api] generated a self-signed certificate at {}",
        cert.display()
    );
    build_acceptor(&x509, &pkey)
}

/// Generate a fresh ECDSA P-256 self-signed certificate.  P-256 is
/// cheap to create (unlike RSA), small on the wire, and accepted by
/// every TLS 1.2+ client.
fn generate_cert() -> Result<(X509, PKey<Private>), String> {
    let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1)
        .map_err(|e| format!("P-256 curve group: {e}"))?;
    let ec = EcKey::generate(&group).map_err(|e| format!("generate the key pair: {e}"))?;
    let pkey = PKey::from_ec_key(ec).map_err(|e| format!("wrap the key: {e}"))?;

    let mut name = X509NameBuilder::new().map_err(|e| format!("certificate subject: {e}"))?;
    name.append_entry_by_nid(Nid::COMMONNAME, "annotator")
        .map_err(|e| format!("certificate CN: {e}"))?;
    name.append_entry_by_nid(Nid::ORGANIZATIONNAME, "annotator")
        .map_err(|e| format!("certificate O: {e}"))?;
    let name = name.build();

    let mut builder = X509::builder().map_err(|e| format!("certificate builder: {e}"))?;
    builder
        .set_version(2) // X.509 v3
        .map_err(|e| format!("certificate version: {e}"))?;
    let mut serial = BigNum::new().map_err(|e| format!("certificate serial: {e}"))?;
    serial
        .rand(128, MsbOption::MAYBE_ZERO, false)
        .map_err(|e| format!("certificate serial: {e}"))?;
    let serial = serial
        .to_asn1_integer()
        .map_err(|e| format!("certificate serial: {e}"))?;
    builder
        .set_serial_number(&serial)
        .map_err(|e| format!("certificate serial: {e}"))?;
    builder
        .set_subject_name(&name)
        .and_then(|()| builder.set_issuer_name(&name))
        .map_err(|e| format!("certificate names: {e}"))?;
    let not_before = Asn1Time::days_from_now(0).map_err(|e| format!("notBefore: {e}"))?;
    let not_after = Asn1Time::days_from_now(CERT_DAYS).map_err(|e| format!("notAfter: {e}"))?;
    builder
        .set_not_before(&not_before)
        .and_then(|()| builder.set_not_after(&not_after))
        .map_err(|e| format!("certificate validity: {e}"))?;
    builder
        .set_pubkey(&pkey)
        .map_err(|e| format!("certificate public key: {e}"))?;
    builder
        .sign(&pkey, MessageDigest::sha256())
        .map_err(|e| format!("sign the certificate: {e}"))?;
    Ok((builder.build(), pkey))
}

/// Wrap cert + key in a server acceptor.  TLS 1.2 is the floor — the
/// intermediate profile minus client certs, matching what browsers and
/// control processors (Crestron et al.) all speak.
fn build_acceptor(cert: &X509, key: &PKey<Private>) -> Result<SslAcceptor, String> {
    let mut acceptor = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls_server())
        .map_err(|e| format!("TLS acceptor: {e}"))?;
    acceptor
        .set_private_key(key)
        .and_then(|()| acceptor.set_certificate(cert))
        .and_then(|()| acceptor.check_private_key())
        .map_err(|e| format!("certificate/key mismatch: {e}"))?;
    Ok(acceptor.build())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ephemeral_acceptor_can_be_built() {
        acceptor_ephemeral().expect("generating an in-memory certificate should work");
    }

    #[test]
    fn the_cert_is_regenerated_only_when_missing() {
        let dir = std::env::temp_dir().join(format!("annotator-tls-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let cert = dir.join(CERT_FILE);
        let key = dir.join(KEY_FILE);

        acceptor_from_paths(&cert, &key).expect("first build generates");
        let generated = fs::read(&cert).unwrap();
        acceptor_from_paths(&cert, &key).expect("second build loads");
        assert_eq!(
            fs::read(&cert).unwrap(),
            generated,
            "the certificate must be stable across restarts"
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
