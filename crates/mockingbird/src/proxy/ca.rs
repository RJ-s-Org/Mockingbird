//! The local certificate authority the proxy signs per-host leaf
//! certificates with. It lives in `~/.mockingbird/ca` (`ca.pem`, and
//! `ca.key` readable by the owner only) and is created on first use. It is
//! never installed in a system trust store: clients trust it through
//! `NODE_EXTRA_CA_CERTS`, `SSL_CERT_FILE` and the like (see
//! [`super::env`]).

use std::collections::HashMap;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, anyhow};
use base64::Engine as _;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose,
};
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

/// The CA certificate's common name.
pub const CA_COMMON_NAME: &str = "Mockingbird Local CA";

const CERT_FILE: &str = "ca.pem";
const KEY_FILE: &str = "ca.key";
const BUNDLE_FILE: &str = "bundle.pem";
const TRUSTSTORE_FILE: &str = "truststore.jks";

/// The password of [`LocalCa::write_truststore`]'s keystore. It guards
/// nothing (the store holds public certificates only); Java wants one to
/// check the file's integrity.
pub const TRUSTSTORE_PASSWORD: &str = "changeit";

/// How long a process that finds the key but not yet the certificate (one
/// being written by another process) waits for it.
const CERT_WAIT: Duration = Duration::from_secs(2);

/// `MB_CA_DIR` when set and non-empty, else `~/.mockingbird/ca`.
pub fn default_ca_dir() -> anyhow::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("MB_CA_DIR").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|h| !h.is_empty()))
        .ok_or_else(|| anyhow!("cannot find the home directory (set HOME or MB_CA_DIR)"))?;
    Ok(PathBuf::from(home).join(".mockingbird").join("ca"))
}

/// The local CA's certificate path in [`default_ca_dir`], creating the CA
/// when it is missing.
pub fn ca_cert_path() -> anyhow::Result<PathBuf> {
    Ok(LocalCa::load_or_create(&default_ca_dir()?)?.cert_path())
}

/// The local CA, with the leaf configurations issued so far.
pub struct LocalCa {
    dir: PathBuf,
    cert_pem: String,
    cert_der: CertificateDer<'static>,
    /// The CA's SubjectPublicKeyInfo, DER.
    spki_der: Vec<u8>,
    issuer: Issuer<'static, KeyPair>,
    leaves: Mutex<HashMap<String, Arc<ServerConfig>>>,
}

impl std::fmt::Debug for LocalCa {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalCa").field("dir", &self.dir).finish()
    }
}

impl LocalCa {
    /// Loads the CA in `dir`, creating it (and a missing `dir`, mode `0700`) when
    /// there is none. Safe against another process creating it at the same
    /// time: the key is created exclusively, and the loser loads the
    /// winner's CA.
    pub fn load_or_create(dir: &Path) -> anyhow::Result<LocalCa> {
        if !dir.exists() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("cannot create the CA directory {}", dir.display()))?;
            // Only a directory made here is restricted: an existing one
            // (a configured `proxy.ca_dir`) keeps its mode.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                    .with_context(|| format!("cannot restrict {}", dir.display()))?;
            }
        }
        let key_path = dir.join(KEY_FILE);
        let cert_path = dir.join(CERT_FILE);
        let (key, cert_pem) = match create_key_file(&key_path)? {
            Some(key) => {
                let pem = ca_params()?
                    .self_signed(&key)
                    .context("cannot sign the local CA certificate")?
                    .pem();
                write_atomically(dir, &cert_path, pem.as_bytes())?;
                (key, pem)
            }
            None => (read_key(&key_path)?, read_cert(&cert_path)?),
        };
        let cert_der = pem_to_der(&cert_pem)
            .with_context(|| format!("{} is not a PEM certificate", cert_path.display()))?;
        let spki_der = rcgen::PublicKeyData::subject_public_key_info(&key);
        let issuer = Issuer::from_ca_cert_pem(&cert_pem, key)
            .with_context(|| format!("cannot load the local CA from {}", cert_path.display()))?;
        Ok(LocalCa {
            dir: dir.to_path_buf(),
            cert_pem,
            cert_der,
            spki_der,
            issuer,
            leaves: Mutex::new(HashMap::new()),
        })
    }

    /// `ca.pem` in the CA directory.
    pub fn cert_path(&self) -> PathBuf {
        self.dir.join(CERT_FILE)
    }

    /// `ca.key` in the CA directory.
    pub fn key_path(&self) -> PathBuf {
        self.dir.join(KEY_FILE)
    }

    /// The CA certificate, PEM-encoded.
    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }

    /// A TLS server configuration presenting a leaf for `host` (a DNS name
    /// or IP address) signed by this CA, issued once per host.
    pub fn server_config(&self, host: &str) -> anyhow::Result<Arc<ServerConfig>> {
        if let Some(config) = self.leaves.lock().ok().and_then(|l| l.get(host).cloned()) {
            return Ok(config);
        }
        let config = Arc::new(self.issue(host)?);
        if let Ok(mut leaves) = self.leaves.lock() {
            return Ok(Arc::clone(leaves.entry(host.to_owned()).or_insert(config)));
        }
        Ok(config)
    }

    fn issue(&self, host: &str) -> anyhow::Result<ServerConfig> {
        let mut params = CertificateParams::new(vec![host.to_owned()])
            .with_context(|| format!("cannot issue a certificate for {host:?}"))?;
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, host);
        params.distinguished_name = dn;
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.use_authority_key_identifier_extension = true;
        let now = time::OffsetDateTime::now_utc();
        params.not_before = now - time::Duration::days(1);
        params.not_after = now + time::Duration::days(397);
        let key = KeyPair::generate().context("cannot generate a leaf key")?;
        let leaf = params
            .signed_by(&key, &self.issuer)
            .with_context(|| format!("cannot sign the certificate for {host:?}"))?;
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
        crate::tls::server_config_from(vec![leaf.der().clone(), self.cert_der.clone()], key)
    }

    /// Writes `bundle.pem` next to the CA: the system's root certificates
    /// (Mozilla's when the system has none), then this CA. For
    /// `SSL_CERT_FILE`-style variables, so passthrough hosts keep working.
    /// Rewritten on each call.
    pub fn write_bundle(&self) -> anyhow::Result<PathBuf> {
        let mut text = String::new();
        for der in root_ders() {
            text.push_str(&der_to_pem(&der));
        }
        text.push_str(self.cert_pem.trim_end());
        text.push('\n');
        let path = self.dir.join(BUNDLE_FILE);
        write_atomically(&self.dir, &path, text.as_bytes())?;
        Ok(path)
    }

    /// Writes `truststore.jks` next to the CA: a Java keystore (JKS) with
    /// the same certificates as [`write_bundle`](Self::write_bundle), for
    /// `-Djavax.net.ssl.trustStore`. Its password is
    /// [`TRUSTSTORE_PASSWORD`]. Rewritten on each call.
    pub fn write_truststore(&self) -> anyhow::Result<PathBuf> {
        let mut certs = root_ders();
        certs.push(self.cert_der.to_vec());
        let jks = jks_truststore(&certs, TRUSTSTORE_PASSWORD);
        let path = self.dir.join(TRUSTSTORE_FILE);
        write_atomically(&self.dir, &path, &jks)?;
        Ok(path)
    }

    /// Base64 of SHA-256 over the CA's SubjectPublicKeyInfo: what Chrome's
    /// `--ignore-certificate-errors-spki-list` takes.
    pub fn spki_sha256(&self) -> String {
        use sha2::Digest as _;
        base64::engine::general_purpose::STANDARD.encode(sha2::Sha256::digest(&self.spki_der))
    }
}

/// The system's root certificates (DER), or Mozilla's when it has none.
fn root_ders() -> Vec<Vec<u8>> {
    let native = rustls_native_certs::load_native_certs().certs;
    if native.is_empty() {
        webpki_root_certs::TLS_SERVER_ROOT_CERTS
            .iter()
            .map(|c| c.to_vec())
            .collect()
    } else {
        native.iter().map(|c| c.to_vec()).collect()
    }
}

/// A JKS keystore holding `certs` (DER) as trusted-certificate entries
/// (`ca0`, `ca1`, …), sealed with `password` (Java's keyed SHA-1 digest).
fn jks_truststore(certs: &[Vec<u8>], password: &str) -> Vec<u8> {
    use sha1::Digest as _;
    fn utf(out: &mut Vec<u8>, s: &str) {
        out.extend_from_slice(&(s.len() as u16).to_be_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    let mut out = Vec::new();
    out.extend_from_slice(&0xFEED_FEED_u32.to_be_bytes());
    out.extend_from_slice(&2u32.to_be_bytes());
    out.extend_from_slice(&(certs.len() as u32).to_be_bytes());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    for (i, der) in certs.iter().enumerate() {
        out.extend_from_slice(&2u32.to_be_bytes()); // trusted certificate
        utf(&mut out, &format!("ca{i}"));
        out.extend_from_slice(&now.to_be_bytes());
        utf(&mut out, "X.509");
        out.extend_from_slice(&(der.len() as u32).to_be_bytes());
        out.extend_from_slice(der);
    }
    let mut digest = sha1::Sha1::new();
    for unit in password.encode_utf16() {
        digest.update(unit.to_be_bytes());
    }
    digest.update(b"Mighty Aphrodite");
    digest.update(&out);
    out.extend_from_slice(&digest.finalize());
    out
}

fn ca_params() -> anyhow::Result<CertificateParams> {
    let mut params = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, CA_COMMON_NAME);
    dn.push(DnType::OrganizationName, "Mockingbird");
    params.distinguished_name = dn;
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::days(1);
    params.not_after = now + time::Duration::days(3650);
    Ok(params)
}

/// Creates `path` exclusively (mode `0600`) with a fresh key; `None` when
/// it already exists (made earlier, or by another process just now).
fn create_key_file(path: &Path) -> anyhow::Result<Option<KeyPair>> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::AlreadyExists => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("cannot create {}", path.display())),
    };
    let key = KeyPair::generate().context("cannot generate the CA key")?;
    file.write_all(key.serialize_pem().as_bytes())
        .and_then(|()| file.sync_all())
        .with_context(|| format!("cannot write {}", path.display()))?;
    Ok(Some(key))
}

/// Reads the key, waiting up to [`CERT_WAIT`] for a process that just
/// created the file to finish writing it.
fn read_key(path: &Path) -> anyhow::Result<KeyPair> {
    let mut waited = Duration::ZERO;
    loop {
        let pem = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        match KeyPair::from_pem(&pem) {
            Ok(key) => return Ok(key),
            Err(_) if waited < CERT_WAIT => {
                std::thread::sleep(Duration::from_millis(100));
                waited += Duration::from_millis(100);
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("{} is not a PEM private key", path.display()));
            }
        }
    }
}

/// Reads the certificate, waiting up to [`CERT_WAIT`] for a process that
/// just created the key to write it.
fn read_cert(path: &Path) -> anyhow::Result<String> {
    let mut waited = Duration::ZERO;
    loop {
        match std::fs::read_to_string(path) {
            Ok(pem) => return Ok(pem),
            Err(e) if e.kind() == ErrorKind::NotFound && waited < CERT_WAIT => {
                std::thread::sleep(Duration::from_millis(100));
                waited += Duration::from_millis(100);
            }
            Err(e) => {
                return Err(e).with_context(|| {
                    format!(
                        "cannot read {} (delete {} to create a new CA)",
                        path.display(),
                        path.with_file_name(KEY_FILE).display()
                    )
                });
            }
        }
    }
}

/// Writes `contents` to a temporary file in `dir`, then renames it over
/// `path`, so readers never see a partial file.
fn write_atomically(dir: &Path, path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("{} has no file name", path.display()))?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(&tmp, contents).with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("cannot write {}", path.display()))
}

fn pem_to_der(pem: &str) -> anyhow::Result<CertificateDer<'static>> {
    use rustls::pki_types::pem::PemObject as _;
    CertificateDer::from_pem_slice(pem.as_bytes()).map_err(|e| anyhow!("{e}"))
}

/// One `CERTIFICATE` PEM block with 64-column lines.
fn der_to_pem(der: &[u8]) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let mut out = String::from("-----BEGIN CERTIFICATE-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        out.push_str(&String::from_utf8_lossy(chunk));
        out.push('\n');
    }
    out.push_str("-----END CERTIFICATE-----\n");
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::Arc;

    use rustls::pki_types::CertificateDer;
    use rustls::pki_types::pem::PemObject;

    use super::*;

    #[test]
    fn creates_once_with_private_key_and_reloads() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("ca");
        let ca = LocalCa::load_or_create(dir.as_path()).unwrap();
        assert!(ca.cert_pem().starts_with("-----BEGIN CERTIFICATE-----"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(ca.key_path())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(dir.as_path())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        let again = LocalCa::load_or_create(dir.as_path()).unwrap();
        assert_eq!(again.cert_pem(), ca.cert_pem());
    }

    #[cfg(unix)]
    #[test]
    fn an_existing_directory_keeps_its_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        LocalCa::load_or_create(dir.path()).unwrap();
        assert_eq!(
            std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn concurrent_creation_yields_one_ca() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().to_path_buf();
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let p = p.clone();
                std::thread::spawn(move || {
                    LocalCa::load_or_create(&p).unwrap().cert_pem().to_owned()
                })
            })
            .collect();
        let pems: std::collections::BTreeSet<String> =
            handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(pems.len(), 1);
    }

    #[tokio::test]
    async fn leaf_chains_to_the_ca_with_strict_extensions() {
        let dir = tempfile::tempdir().unwrap();
        let ca = LocalCa::load_or_create(dir.path()).unwrap();
        let server = tokio_rustls::TlsAcceptor::from(ca.server_config("api.example.test").unwrap());
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(CertificateDer::from_pem_slice(ca.cert_pem().as_bytes()).unwrap())
            .unwrap();
        let client = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        let (c, s) = tokio::io::duplex(64 * 1024);
        let accept = tokio::spawn(async move { server.accept(s).await.map(|_| ()) });
        let name = rustls::pki_types::ServerName::try_from("api.example.test").unwrap();
        let tls = tokio_rustls::TlsConnector::from(Arc::new(client))
            .connect(name, c)
            .await
            .unwrap();
        accept.await.unwrap().unwrap();
        let leaf = &tls.get_ref().1.peer_certificates().unwrap()[0];
        let (_, cert) = x509_parser::parse_x509_certificate(leaf.as_ref()).unwrap();
        assert!(
            cert.extensions()
                .iter()
                .any(|e| e.oid == x509_parser::oid_registry::OID_X509_EXT_AUTHORITY_KEY_IDENTIFIER)
        );
        let ca_der = CertificateDer::from_pem_slice(ca.cert_pem().as_bytes()).unwrap();
        let (_, ca_cert) = x509_parser::parse_x509_certificate(ca_der.as_ref()).unwrap();
        for oid in [
            x509_parser::oid_registry::OID_X509_EXT_BASIC_CONSTRAINTS,
            x509_parser::oid_registry::OID_X509_EXT_KEY_USAGE,
        ] {
            assert!(
                ca_cert
                    .extensions()
                    .iter()
                    .any(|e| e.oid == oid && e.critical),
                "{oid} must be critical"
            );
        }
        assert!(Arc::ptr_eq(
            &ca.server_config("api.example.test").unwrap(),
            &ca.server_config("api.example.test").unwrap()
        ));
    }

    #[test]
    fn bundle_holds_roots_and_the_local_ca() {
        let dir = tempfile::tempdir().unwrap();
        let ca = LocalCa::load_or_create(dir.path()).unwrap();
        let text = std::fs::read_to_string(ca.write_bundle().unwrap()).unwrap();
        assert!(text.matches("BEGIN CERTIFICATE").count() > 10);
        assert!(text.trim_end().ends_with(ca.cert_pem().trim_end()));
    }
}
