# Test-only TLS certificates

**Never use these outside tests: the private keys are public.**

| File | What |
|---|---|
| `ca.pem` | Self-signed CA `Mockingbird Test CA` (serial `0x01`) |
| `server.pem`, `server-key.pem` | Server certificate for `localhost` and `127.0.0.1`, signed by the CA (serial `0x1001`) |
| `server2.pem`, `server2-key.pem` | A second pair for the same names, signed by the same CA (serial `0x1002`); the certificate-reload test swaps it in |

All are ECDSA P-256, valid for 100 years from their generation date
(2026-10-15).

## How they were generated

Once, with `rcgen` 0.14.10 in a throwaway program (`cargo new gencerts`,
dependencies `rcgen = { version = "0.14", features = ["pem", "aws_lc_rs"] }`
and `time = "0.3"`), run as `cargo run -- fixtures/tls`:

```rust
//! Writes the test-only CA and two server certificates for localhost/127.0.0.1.
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, KeyUsagePurpose, ExtendedKeyUsagePurpose, SerialNumber};
use time::{Duration, OffsetDateTime};

fn main() {
    let out = std::path::PathBuf::from(std::env::args().nth(1).expect("out dir"));
    let not_before = OffsetDateTime::now_utc() - Duration::days(1);
    let not_after = not_before + Duration::days(365 * 100 + 25);

    let ca_key = KeyPair::generate().unwrap();
    let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.distinguished_name.push(DnType::CommonName, "Mockingbird Test CA");
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
    ca.not_before = not_before;
    ca.not_after = not_after;
    ca.serial_number = Some(SerialNumber::from(vec![0x01]));
    let ca_cert = ca.self_signed(&ca_key).unwrap();
    let issuer = rcgen::Issuer::new(ca, ca_key);
    std::fs::write(out.join("ca.pem"), ca_cert.pem()).unwrap();

    for (name, serial) in [("server", 0x1001u16), ("server2", 0x1002u16)] {
        let key = KeyPair::generate().unwrap();
        let mut p = CertificateParams::new(vec!["localhost".to_string(), "127.0.0.1".to_string()]).unwrap();
        p.distinguished_name.push(DnType::CommonName, "localhost");
        p.key_usages = vec![KeyUsagePurpose::DigitalSignature, KeyUsagePurpose::KeyEncipherment];
        p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        p.not_before = not_before;
        p.not_after = not_after;
        p.serial_number = Some(SerialNumber::from(serial.to_be_bytes().to_vec()));
        let cert = p.signed_by(&key, &issuer).unwrap();
        std::fs::write(out.join(format!("{name}.pem")), cert.pem()).unwrap();
        std::fs::write(out.join(format!("{name}-key.pem")), key.serialize_pem()).unwrap();
    }
}
```

Regenerating replaces every file, so commit them together.
