//! Helpers for loading application configuration from an optional TOML file
//! and the process environment, with the environment taking precedence.

use ext_config::{Config, ConfigError, Environment, File, FileFormat};
use serde::de::DeserializeOwned;

/// Loads configuration of type `T` from an optional TOML file and environment
/// variables, preferring the environment when both define the same value.
///
/// Environment variables join `env_prefix` and the field path with `__`, e.g.
/// `POOL__JDS__LISTEN_ADDRESS` for the field `jds.listen_address`. Tagged enum
/// variants are just another path segment, matched case-insensitively, e.g.
/// `POOL__TEMPLATE_PROVIDER_TYPE__BITCOINCOREIPC__NETWORK=mainnet`.
///
/// Fields named in `list_keys` (lowercase `snake_case`) are parsed as
/// comma-separated lists, e.g. `POOL__SUPPORTED_EXTENSIONS=1,2,3`. A single
/// numeric value is read as a scalar, not a 1-element list, so list at least
/// two values.
pub fn load_config<T: DeserializeOwned>(
    config_path: &str,
    env_prefix: &str,
    list_keys: &[&str],
) -> Result<T, ConfigError> {
    // `try_parsing` lets scalar environment values (numbers and booleans) be
    // coerced into their target types instead of staying raw strings.
    let mut environment = Environment::with_prefix(env_prefix)
        .separator("__")
        .try_parsing(true);

    // A list separator can only be enabled together with explicit keys;
    // otherwise the `config` crate would turn *every* string value into a list.
    if !list_keys.is_empty() {
        environment = environment.list_separator(",");
        for key in list_keys {
            environment = environment.with_list_parse_key(key);
        }
    }

    let builder = Config::builder()
        .add_source(File::new(config_path, FileFormat::Toml).required(false))
        .add_source(environment);

    builder.build()?.try_deserialize()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::{env, io::Write};

    #[derive(Debug, Deserialize)]
    struct Nested {
        port: u16,
    }

    #[derive(Debug, Deserialize)]
    struct TestConfig {
        listen_address: String,
        cert_validity_sec: u64,
        #[serde(default)]
        verify_payout: bool,
        #[serde(default)]
        supported_extensions: Vec<u16>,
        nested: Nested,
    }

    /// Writes `contents` to a uniquely named temp TOML file and returns its path.
    fn write_toml(name: &str, contents: &str) -> std::path::PathBuf {
        let path = env::temp_dir().join(format!("loader-test-{name}.toml"));
        let mut file = std::fs::File::create(&path).expect("create temp config");
        file.write_all(contents.as_bytes()).expect("write config");
        path
    }

    fn list_keys() -> &'static [&'static str] {
        &["supported_extensions", "required_extensions"]
    }

    #[test]
    fn env_overrides_file() {
        let path = write_toml(
            "override",
            r#"
                listen_address = "0.0.0.0:1111"
                cert_validity_sec = 3600
                [nested]
                port = 10
            "#,
        );

        // Same keys present in the file are overridden by the environment.
        env::set_var("OVR__LISTEN_ADDRESS", "0.0.0.0:2222");
        env::set_var("OVR__NESTED__PORT", "20");

        let cfg: TestConfig =
            load_config(path.to_str().unwrap(), "OVR", list_keys()).expect("load config");

        assert_eq!(cfg.listen_address, "0.0.0.0:2222"); // from env
        assert_eq!(cfg.cert_validity_sec, 3600); // from file
        assert_eq!(cfg.nested.port, 20); // nested override from env

        env::remove_var("OVR__LISTEN_ADDRESS");
        env::remove_var("OVR__NESTED__PORT");
    }

    #[test]
    fn env_only_without_file() {
        // No file exists at this path: configuration comes entirely from env.
        let missing = env::temp_dir().join("loader-test-does-not-exist.toml");

        env::set_var("ENVONLY__LISTEN_ADDRESS", "127.0.0.1:3333");
        env::set_var("ENVONLY__CERT_VALIDITY_SEC", "1200");
        env::set_var("ENVONLY__VERIFY_PAYOUT", "true");
        env::set_var("ENVONLY__SUPPORTED_EXTENSIONS", "1,2,3");
        env::set_var("ENVONLY__NESTED__PORT", "42");

        let cfg: TestConfig =
            load_config(missing.to_str().unwrap(), "ENVONLY", list_keys()).expect("load config");

        assert_eq!(cfg.listen_address, "127.0.0.1:3333");
        assert_eq!(cfg.cert_validity_sec, 1200); // string parsed into u64
        assert!(cfg.verify_payout); // string parsed into bool
        assert_eq!(cfg.supported_extensions, vec![1, 2, 3]); // comma-separated list
        assert_eq!(cfg.nested.port, 42);

        for var in [
            "ENVONLY__LISTEN_ADDRESS",
            "ENVONLY__CERT_VALIDITY_SEC",
            "ENVONLY__VERIFY_PAYOUT",
            "ENVONLY__SUPPORTED_EXTENSIONS",
            "ENVONLY__NESTED__PORT",
        ] {
            env::remove_var(var);
        }
    }

    #[test]
    fn missing_required_field_errors() {
        // The env supplies some fields but not all required ones.
        let missing = env::temp_dir().join("loader-test-empty.toml");
        env::set_var("PARTIAL__LISTEN_ADDRESS", "0.0.0.0:1111");

        let result: Result<TestConfig, _> =
            load_config(missing.to_str().unwrap(), "PARTIAL", list_keys());
        assert!(result.is_err());

        env::remove_var("PARTIAL__LISTEN_ADDRESS");
    }
}
