//! Where a Noise responder gets the static key and the certificate it presents to initiators.

use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use secp256k1::{Keypair, Secp256k1};
use serde::{Deserialize, Deserializer, de};
use stratum_core::noise_sv2::Responder;

use crate::{
    key_utils::{Certificate, Secp256k1PublicKey, Secp256k1SecretKey},
    network_helpers::Error,
};

/// How a Noise server authenticates to initiators: where its static key and the certificate
/// over it come from.
///
/// It deserializes from flat fields, so an application config embeds it with
/// `#[serde(flatten)]`. Each field can come from the configuration file or from the environment
/// through the config loader, which overrides field by field. On the merged fields, in order:
///
/// 1. `static_private_key` and `certificate` set: [`Self::PreIssued`]. `active_static_key` is
///    ignored if also set.
/// 2. Otherwise `active_static_key` set: [`Self::File`].
/// 3. Neither: refused.
///
/// Because the loader merges first, an inline pair in the file still wins over an
/// `active_static_key` from the environment: the environment overrides fields, not the choice
/// between inline and file. Half an inline pair is refused. The authority secret key never
/// belongs on a server, so `authority_secret_key` and `cert_validity_sec`, the fields of the
/// former sign-per-handshake setup, are refused instead of being ignored. An optional
/// `authority_public_key` applies to whichever source is used.
#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "ResponderCredentialsConfig")]
pub enum ResponderCredentials {
    /// A static key and a certificate issued for it where the authority private key is kept,
    /// loaded once with the configuration. Rotating the key means restarting the process.
    PreIssued {
        /// The static key the certificate was issued for.
        static_private_key: Secp256k1SecretKey,
        /// Sent verbatim in every handshake.
        certificate: Certificate,
        /// When present, the certificate's signature is checked against it at startup and before
        /// every handshake; its version and validity window are checked either way. Without it,
        /// a certificate from another authority is only noticed when pinning initiators reject
        /// the handshake: it is the initiators that authenticate this server, not this check.
        authority_public_key: Option<Secp256k1PublicKey>,
    },
    /// A static key and certificate read from a TOML file before every handshake, so rotation
    /// means replacing the file (or the symlink it is) and touches neither the process nor its
    /// open connections. The file holds `static_private_key`, `certificate` and optionally
    /// `authority_public_key`, with the same encodings as the configuration.
    ///
    /// The file's contents are parsed and dropped per handshake without wiping memory, the
    /// same posture the configuration loader has for the keys it holds; what this variant keeps
    /// off the host is the authority private key.
    File {
        /// The file to read before every handshake; `~` and environment variables expand.
        path: PathBuf,
        /// When present, every certificate read from the file is checked against it, in
        /// addition to any `authority_public_key` the file itself names.
        authority_public_key: Option<Secp256k1PublicKey>,
    },
}

// The flat fields `ResponderCredentials` is built from. See there for which combinations are
// accepted.
#[derive(Deserialize)]
struct ResponderCredentialsConfig {
    #[serde(default)]
    authority_public_key: Option<Secp256k1PublicKey>,
    // Only their presence matters, so the values are not parsed: a secret key that fails to
    // parse must not reach the error message either.
    #[serde(default)]
    authority_secret_key: Option<de::IgnoredAny>,
    #[serde(default)]
    cert_validity_sec: Option<de::IgnoredAny>,
    #[serde(default)]
    static_private_key: Option<Secp256k1SecretKey>,
    #[serde(default)]
    certificate: Option<Certificate>,
    #[serde(default, deserialize_with = "opt_expanded_path")]
    active_static_key: Option<PathBuf>,
}

impl TryFrom<ResponderCredentialsConfig> for ResponderCredentials {
    type Error = Error;

    fn try_from(config: ResponderCredentialsConfig) -> Result<Self, Self::Error> {
        if config.authority_secret_key.is_some() || config.cert_validity_sec.is_some() {
            return Err(Error::InvalidResponderCredentials(
                "authority_secret_key and cert_validity_sec are not accepted: a server never \
                 holds the authority secret key. Issue a certificate where that key is kept \
                 and set static_private_key and certificate, or active_static_key"
                    .into(),
            ));
        }
        match (config.static_private_key, config.certificate) {
            (Some(static_private_key), Some(certificate)) => Ok(Self::PreIssued {
                static_private_key,
                certificate,
                authority_public_key: config.authority_public_key,
            }),
            (None, None) => match config.active_static_key {
                Some(path) => Ok(Self::File {
                    path,
                    authority_public_key: config.authority_public_key,
                }),
                None => Err(Error::InvalidResponderCredentials(
                    "no server keys configured: set static_private_key and certificate, or \
                     active_static_key"
                        .into(),
                )),
            },
            _ => Err(Error::InvalidResponderCredentials(
                "static_private_key and certificate must be set together".into(),
            )),
        }
    }
}

// Expands `~` and environment variables in a path, like the other path fields in app configs.
// A copy of `config_helpers::opt_path_from_toml`: that module is behind the `config` feature,
// which the `network` feature this type lives under does not imply.
fn opt_expanded_path<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<PathBuf>, D::Error> {
    Option::<String>::deserialize(deserializer)?
        .map(|raw| {
            shellexpand::full(&raw)
                .map(|expanded| PathBuf::from(expanded.as_ref()))
                .map_err(de::Error::custom)
        })
        .transpose()
}

// The TOML file a [`ResponderCredentials::File`] points at. Unknown keys are refused so a
// misspelled `authority_public_key` cannot silently skip the signature check.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StaticKeyFile {
    static_private_key: Secp256k1SecretKey,
    certificate: Certificate,
    #[serde(default)]
    authority_public_key: Option<Secp256k1PublicKey>,
}

impl ResponderCredentials {
    /// Builds the responder for one handshake.
    ///
    /// For [`Self::File`] this reads the file, so a rotated file takes effect on the next
    /// connection. A pre-issued certificate is checked for version and validity window, and
    /// verified against every authority public key known for it, so a certificate that does not
    /// match fails here with a reason instead of as a rejected handshake on the initiator's side
    /// only.
    pub async fn responder(&self) -> Result<Box<Responder>, Error> {
        match self {
            Self::PreIssued {
                static_private_key,
                certificate,
                authority_public_key,
            } => pre_issued_responder(*static_private_key, *certificate, *authority_public_key),
            Self::File {
                path,
                authority_public_key,
            } => {
                let raw = tokio::fs::read_to_string(path).await.map_err(|e| {
                    Error::InvalidResponderCredentials(format!(
                        "cannot read {}: {e}",
                        path.display()
                    ))
                })?;
                // Only the message: the full error quotes the offending line, which for a
                // malformed `static_private_key` is the key itself.
                let file: StaticKeyFile = toml::from_str(&raw).map_err(|e| {
                    Error::InvalidResponderCredentials(format!(
                        "cannot parse {}: {}",
                        path.display(),
                        e.message()
                    ))
                })?;
                for authority in [*authority_public_key, file.authority_public_key]
                    .into_iter()
                    .flatten()
                {
                    verify_pre_issued(file.static_private_key, file.certificate, authority)?;
                }
                pre_issued_responder(file.static_private_key, file.certificate, None)
            }
        }
    }

    /// Startup check: everything [`Self::responder`] does for a handshake, so a missing file, an
    /// unreadable key or a certificate that does not match surfaces before the server accepts
    /// connections.
    pub async fn check(&self) -> Result<(), Error> {
        self.responder().await?;
        Ok(())
    }
}

fn unix_now() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as u32
}

fn verify_pre_issued(
    static_private_key: Secp256k1SecretKey,
    certificate: Certificate,
    authority_public_key: Secp256k1PublicKey,
) -> Result<(), Error> {
    certificate
        .verify(
            &Secp256k1PublicKey::from(static_private_key),
            &authority_public_key,
            unix_now(),
        )
        .map_err(|e| Error::InvalidResponderCredentials(e.to_string()))
}

fn pre_issued_responder(
    static_private_key: Secp256k1SecretKey,
    certificate: Certificate,
    authority_public_key: Option<Secp256k1PublicKey>,
) -> Result<Box<Responder>, Error> {
    match authority_public_key {
        Some(authority) => verify_pre_issued(static_private_key, certificate, authority)?,
        None => certificate
            .check_validity(unix_now())
            .map_err(|e| Error::InvalidResponderCredentials(e.to_string()))?,
    }
    let keypair = Keypair::from_secret_key(&Secp256k1::signing_only(), &static_private_key.0);
    Ok(Responder::from_certificate(keypair, certificate.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network_helpers::{
        NOISE_HANDSHAKE_TIMEOUT, connect_with_noise, noise_stream::NoiseTcpStream,
    };
    use tokio::net::{TcpListener, TcpStream};

    const AUTHORITY_PUBLIC: &str = "9auqWEzQDVyd2oe1JVGFLMLHZtCo2FFqZwtKA5gd9xbuEu7PH72";
    const AUTHORITY_SECRET: &str = "mkDLTBBRxdBv998612qipDYoTK3YUrqLe8uWw7gu3iXbSrn2n";
    const STATIC_SECRET: &str = "24Zfqe5i5LQVc8yV6okoMpiQn7DohfxTBRJSgBKeH65oS6Gtkd";

    fn now() -> u32 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as u32
    }

    // A certificate for `static_secret` issued by `authority_secret`, valid around now.
    fn issue(static_secret: &str, authority_secret: &str) -> Certificate {
        let authority: Secp256k1SecretKey = authority_secret.parse().unwrap();
        let static_key: Secp256k1SecretKey = static_secret.parse().unwrap();
        Certificate::sign(
            &Secp256k1PublicKey::from(static_key),
            &authority,
            now() - 60,
            now() + 3600,
        )
    }

    #[derive(Deserialize)]
    struct AppConfig {
        listen_address: String,
        #[serde(flatten)]
        credentials: ResponderCredentials,
    }

    fn parse(extra: &str) -> Result<ResponderCredentials, toml::de::Error> {
        toml::from_str::<AppConfig>(&format!("listen_address = \"0.0.0.0:1\"\n{extra}")).map(|c| {
            // The sibling field must survive the flattening untouched.
            assert_eq!(c.listen_address, "0.0.0.0:1");
            c.credentials
        })
    }

    #[test]
    fn config_fields_select_the_variant() {
        let certificate = issue(STATIC_SECRET, AUTHORITY_SECRET);
        let inline =
            format!("static_private_key = \"{STATIC_SECRET}\"\ncertificate = \"{certificate}\"\n");
        assert!(matches!(
            parse(&inline).unwrap(),
            ResponderCredentials::PreIssued {
                authority_public_key: None,
                ..
            }
        ));
        assert!(matches!(
            parse(&format!(
                "{inline}authority_public_key = \"{AUTHORITY_PUBLIC}\"\n"
            ))
            .unwrap(),
            ResponderCredentials::PreIssued {
                authority_public_key: Some(_),
                ..
            }
        ));

        assert!(matches!(
            parse("active_static_key = \"./keys/active.toml\"\n").unwrap(),
            ResponderCredentials::File { path, .. } if path == PathBuf::from("./keys/active.toml")
        ));
        // Inline fields win over the file.
        assert!(matches!(
            parse(&format!(
                "{inline}active_static_key = \"./keys/active.toml\"\n"
            ))
            .unwrap(),
            ResponderCredentials::PreIssued { .. }
        ));

        // Half of a pair, nothing at all, or the fields of the former sign-per-handshake setup
        // are refused, the last without parsing the secret key.
        assert!(parse(&format!("static_private_key = \"{STATIC_SECRET}\"\n")).is_err());
        assert!(parse("").is_err());
        assert!(parse(&format!("authority_public_key = \"{AUTHORITY_PUBLIC}\"\n")).is_err());
        let dev = format!(
            "authority_public_key = \"{AUTHORITY_PUBLIC}\"\nauthority_secret_key = \
             \"{AUTHORITY_SECRET}\"\ncert_validity_sec = 3600\n"
        );
        assert!(
            parse(&dev)
                .unwrap_err()
                .to_string()
                .contains("authority_secret_key")
        );
        assert!(
            parse(&format!(
                "{inline}authority_secret_key = \"{AUTHORITY_SECRET}\"\n"
            ))
            .is_err()
        );
        assert!(
            parse("active_static_key = \"./keys/active.toml\"\ncert_validity_sec = 3600\n")
                .is_err()
        );
        assert!(parse("authority_secret_key = \"not a key\"\n").is_err());
    }

    // Accepts one connection with `credentials` and reports whether the handshake succeeded.
    async fn serve_one(listener: &TcpListener, credentials: &ResponderCredentials) -> bool {
        let (stream, _) = listener.accept().await.unwrap();
        match credentials.responder().await {
            Ok(responder) => NoiseTcpStream::accept(stream, responder, NOISE_HANDSHAKE_TIMEOUT)
                .await
                .is_ok(),
            Err(_) => false,
        }
    }

    // Connects once, pinning the test authority, and reports whether the handshake succeeded.
    async fn connect_once(addr: std::net::SocketAddr) -> bool {
        let stream = TcpStream::connect(addr).await.unwrap();
        connect_with_noise(stream, AUTHORITY_PUBLIC.parse().unwrap())
            .await
            .is_ok()
    }

    #[tokio::test]
    async fn pre_issued_certificate_satisfies_a_pinning_initiator() {
        let credentials = parse(&format!(
            "static_private_key = \"{STATIC_SECRET}\"\ncertificate = \"{}\"\n",
            issue(STATIC_SECRET, AUTHORITY_SECRET)
        ))
        .unwrap();
        credentials.check().await.unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (served, connected) =
            tokio::join!(serve_one(&listener, &credentials), connect_once(addr));
        assert!(served && connected);

        // With the authority named, a certificate from another authority is refused up front.
        let other_authority =
            Secp256k1SecretKey(secp256k1::SecretKey::from_slice(&[9u8; 32]).unwrap());
        let foreign = parse(&format!(
            "static_private_key = \"{STATIC_SECRET}\"\ncertificate = \"{}\"\nauthority_public_key = \
             \"{AUTHORITY_PUBLIC}\"\n",
            issue(STATIC_SECRET, &other_authority.to_string())
        ))
        .unwrap();
        assert!(matches!(
            foreign.check().await,
            Err(Error::InvalidResponderCredentials(_))
        ));

        // Without an authority key the window is still checked, so an expired certificate is
        // refused here instead of by every initiator.
        let static_key: Secp256k1SecretKey = STATIC_SECRET.parse().unwrap();
        let authority: Secp256k1SecretKey = AUTHORITY_SECRET.parse().unwrap();
        let expired = ResponderCredentials::PreIssued {
            static_private_key: static_key,
            certificate: Certificate::sign(
                &Secp256k1PublicKey::from(static_key),
                &authority,
                now() - 7200,
                now() - 3600,
            ),
            authority_public_key: None,
        };
        assert!(matches!(
            expired.check().await,
            Err(Error::InvalidResponderCredentials(msg)) if msg.contains("valid from")
        ));
    }

    #[tokio::test]
    async fn rotating_the_key_file_takes_effect_on_the_next_connection() {
        let dir =
            std::env::temp_dir().join(format!("sv2-static-key-{}-{}", std::process::id(), now()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("active_static_key.toml");
        let write = |static_secret: &str, certificate: Certificate| {
            std::fs::write(
                &path,
                format!(
                    "static_private_key = \"{static_secret}\"\ncertificate = \"{certificate}\"\n"
                ),
            )
            .unwrap()
        };

        let credentials = parse(&format!(
            "active_static_key = \"{}\"\nauthority_public_key = \"{AUTHORITY_PUBLIC}\"\n",
            path.display()
        ))
        .unwrap();
        // The file is not there yet: startup must refuse.
        assert!(credentials.check().await.is_err());

        write(STATIC_SECRET, issue(STATIC_SECRET, AUTHORITY_SECRET));
        credentials.check().await.unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (served, connected) =
            tokio::join!(serve_one(&listener, &credentials), connect_once(addr));
        assert!(served && connected);

        // Rotate to a new static key, certified by the same authority: no restart needed.
        let new_static =
            Secp256k1SecretKey(secp256k1::SecretKey::from_slice(&[5u8; 32]).unwrap()).to_string();
        write(&new_static, issue(&new_static, AUTHORITY_SECRET));
        let (served, connected) =
            tokio::join!(serve_one(&listener, &credentials), connect_once(addr));
        assert!(served && connected);

        // A key file with a corrupted static key is refused without the key reaching the error
        // message, which the servers log.
        let mut corrupted = new_static.clone();
        corrupted.pop();
        corrupted.push('1');
        std::fs::write(
            &path,
            format!(
                "static_private_key = \"{corrupted}\"\ncertificate = \"{}\"\n",
                issue(&new_static, AUTHORITY_SECRET)
            ),
        )
        .unwrap();
        match credentials.check().await {
            Err(Error::InvalidResponderCredentials(msg)) => {
                assert!(!msg.contains(&corrupted[..20]), "{msg}")
            }
            other => panic!("expected a refusal, got {other:?}"),
        }

        // A rotation to a certificate from another authority is caught by the configured
        // authority_public_key before the handshake.
        let other_authority =
            Secp256k1SecretKey(secp256k1::SecretKey::from_slice(&[9u8; 32]).unwrap());
        write(
            &new_static,
            issue(&new_static, &other_authority.to_string()),
        );
        let (served, connected) =
            tokio::join!(serve_one(&listener, &credentials), connect_once(addr));
        assert!(!served && !connected);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
