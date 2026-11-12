//! Issues the static key and certificate an Sv2 Noise server presents to its clients.
//!
//! Run it where the authority private key is kept, never on the server: the server only needs
//! the static private key and the certificate this tool writes, and the authority public key to
//! check them against.
//!
//! ```sh
//! cargo run --features cli --bin sv2-certificate -- keygen
//! SV2_AUTHORITY_SECRET_KEY=<KEY> cargo run --features cli --bin sv2-certificate -- issue \
//!     --valid-for 93600 --toml /etc/sv2/key_2.toml --env /etc/sv2/key_2.env --env-prefix POOL
//! cargo run --features cli --bin sv2-certificate -- inspect <CERTIFICATE>
//! ```

use std::{
    io::Write,
    path::{Path, PathBuf},
    process::ExitCode,
    time::{SystemTime, UNIX_EPOCH},
};

use clap::{Parser, Subcommand};
use stratum_apps::key_utils::{Certificate, Secp256k1PublicKey, Secp256k1SecretKey};

#[derive(Parser)]
#[command(
    about = "Issue the static key and certificate an Sv2 Noise server presents",
    long_about = "Run this where the authority private key is kept, not on the server. The \
                  server needs only the static private key and the certificate written here, \
                  plus the authority public key to check them against."
)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate a keypair: an authority key to keep offline, or a static key for a server.
    Keygen,
    /// Issue a certificate for a static key, generating the static key unless one is given.
    Issue {
        /// The authority's secret key (base58check), which signs the certificate. Prefer the
        /// environment variable: a flag lands in the shell history and in `ps` output.
        // Kept as a string and parsed in `run`: clap echoes a value it fails to parse, and a
        // secret with one mistyped character is still a secret.
        #[arg(long, env = "SV2_AUTHORITY_SECRET_KEY", hide_env_values = true)]
        authority_secret_key: String,
        /// Static private key to certify (base58check). A fresh one is generated when absent.
        // A string for the same reason as the authority secret key.
        #[arg(long)]
        static_private_key: Option<String>,
        /// Start of the validity window, unix time. Defaults to now. Set it before the moment
        /// the key goes live so the certificate is already valid then.
        #[arg(long)]
        valid_from: Option<u32>,
        /// Length of the validity window in seconds, from valid_from. Make it outlast the next
        /// rotation by a margin so a late rotation does not break handshakes.
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
        valid_for: u32,
        /// Write the result as the TOML file a server's `active_static_key` points at; the name
        /// must end in `.toml`.
        #[arg(long, value_parser = path_with_extension("toml"))]
        toml: Option<PathBuf>,
        /// Write the result as a `.env` file of `<PREFIX>__*` variables; the name must end in
        /// `.env`. Needs --env-prefix.
        #[arg(long, requires = "env_prefix", value_parser = path_with_extension("env"))]
        env: Option<PathBuf>,
        /// Variable prefix for --env: the application's, e.g. POOL or JDC.
        #[arg(long, requires = "env", value_parser = clap::builder::NonEmptyStringValueParser::new())]
        env_prefix: Option<String>,
    },
    /// Show a certificate's version and validity window.
    Inspect {
        /// The certificate (base58check).
        certificate: Certificate,
    },
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> Result<(), String> {
    match args.command {
        Command::Keygen => {
            let secret = fresh_secret_key();
            println!("secret_key = \"{secret}\"");
            println!("public_key = \"{}\"", Secp256k1PublicKey::from(secret));
            Ok(())
        }
        Command::Issue {
            authority_secret_key,
            static_private_key,
            valid_from,
            valid_for,
            toml,
            env,
            env_prefix,
        } => {
            // Flat errors: the parse error quotes checksums computed from the typed value.
            let authority_secret_key: Secp256k1SecretKey = authority_secret_key
                .parse()
                .map_err(|_| "authority secret key: not a valid base58check secret key")?;
            let static_private_key = match static_private_key {
                Some(raw) => raw
                    .parse()
                    .map_err(|_| "static private key: not a valid base58check secret key")?,
                None => fresh_secret_key(),
            };
            let static_public_key = Secp256k1PublicKey::from(static_private_key);
            let authority_public_key = Secp256k1PublicKey::from(authority_secret_key);
            let valid_from = valid_from.unwrap_or_else(now);
            let not_valid_after = valid_from
                .checked_add(valid_for)
                .ok_or("valid_from + valid_for does not fit a u32 unix time")?;
            let certificate = Certificate::sign(
                &static_public_key,
                &authority_secret_key,
                valid_from,
                not_valid_after,
            );

            let (from, until) = (when(valid_from), when(not_valid_after));
            let mut outputs = Vec::new();
            if let Some(path) = &toml {
                outputs.push((
                    path,
                    format!(
                        "# Static key and certificate for an Sv2 Noise server, valid from \
                         {from} until {until}.\n# Point the \
                         server's `active_static_key` at this file (through a symlink, so a \
                         rotation is one atomic rename),\n# or copy the values into its \
                         configuration.\nstatic_private_key = \"{static_private_key}\"\n\
                         certificate = \"{certificate}\"\nauthority_public_key = \
                         \"{authority_public_key}\"\n"
                    ),
                ));
            }
            if let (Some(path), Some(prefix)) = (&env, &env_prefix) {
                let prefix = prefix.to_uppercase();
                outputs.push((
                    path,
                    format!(
                        "# Static key and certificate for an Sv2 Noise server, valid from \
                         {from} until {until}.\n\
                         {prefix}__STATIC_PRIVATE_KEY={static_private_key}\n\
                         {prefix}__CERTIFICATE={certificate}\n\
                         {prefix}__AUTHORITY_PUBLIC_KEY={authority_public_key}\n"
                    ),
                ));
            }
            // Every output is created before any is written, so a name that cannot be created
            // (it exists already, or its directory does not) fails before anything is on disk.
            let files = outputs
                .iter()
                .map(|(path, _)| create_secret_file(path))
                .collect::<Result<Vec<_>, _>>()?;
            for (mut file, (path, contents)) in files.into_iter().zip(&outputs) {
                file.write_all(contents.as_bytes())
                    .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
            }
            // Reported only once everything succeeded, so a failure is the whole output.
            println!("# certificate for static public key {static_public_key}");
            println!("# valid from {from} until {until}");
            println!("# signed by authority public key {authority_public_key}");
            for (path, _) in &outputs {
                println!("# wrote {}", path.display());
            }
            if outputs.is_empty() {
                println!("static_private_key = \"{static_private_key}\"");
                println!("certificate = \"{certificate}\"");
                println!("authority_public_key = \"{authority_public_key}\"");
            }
            Ok(())
        }
        Command::Inspect { certificate } => {
            let (valid_from, not_valid_after) =
                (certificate.valid_from(), certificate.not_valid_after());
            println!("version = {}", certificate.version());
            println!("valid_from = {valid_from}  # {}", date(valid_from));
            println!(
                "not_valid_after = {not_valid_after}  # {}",
                date(not_valid_after)
            );
            // The verdict a server or initiator would give now, clock leeway included.
            let now = now();
            match certificate.check_validity(now) {
                Ok(()) => println!(
                    "# accepted now, expires in {} seconds",
                    not_valid_after.saturating_sub(now)
                ),
                Err(e) => println!("# rejected now: {e}"),
            }
            Ok(())
        }
    }
}

// The two output files look alike inside, so the name is what tells an operator which is which.
fn path_with_extension(
    extension: &'static str,
) -> impl Fn(&str) -> Result<PathBuf, String> + Clone {
    move |raw| {
        let path = PathBuf::from(raw);
        if path.extension().is_some_and(|e| e == extension) {
            Ok(path)
        } else {
            Err(format!("the file name must end in .{extension}"))
        }
    }
}

// A unix time as a UTC date, for humans; the config fields keep the number.
fn date(unix: u32) -> String {
    chrono::DateTime::from_timestamp(unix.into(), 0)
        .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_default()
}

fn when(unix: u32) -> String {
    format!("{unix} ({})", date(unix))
}

fn fresh_secret_key() -> Secp256k1SecretKey {
    Secp256k1SecretKey(secp256k1::SecretKey::new(&mut rand::thread_rng()))
}

fn now() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as u32)
        .unwrap_or_default()
}

// The file holds a private key, so it is created readable by its owner only and never
// overwrites an existing file: a rotation writes a new file and moves a symlink.
fn create_secret_file(path: &Path) -> Result<std::fs::File, String> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .map_err(|e| format!("cannot create {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_paths_must_carry_their_extension() {
        assert!(path_with_extension("toml")("/etc/sv2/key_2.toml").is_ok());
        assert!(path_with_extension("env")("key_2.env").is_ok());
        assert!(path_with_extension("env")("key_2.toml").is_err());
        assert!(path_with_extension("toml")("test").is_err());
        assert!(path_with_extension("env")(".env").is_err());
    }

    #[test]
    fn dates_are_utc_and_cover_the_whole_u32_range() {
        assert_eq!(date(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(date(u32::MAX), "2106-02-07 06:28:15 UTC");
    }
}
