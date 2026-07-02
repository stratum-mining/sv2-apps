//! Helpers for loading application configuration from an optional TOML file
//! and the process environment, with the environment taking precedence.

use std::{collections::BTreeMap, path::Path};

use ext_config::{Config, ConfigError, Environment, File, FileFormat, Map, Value, ValueKind};
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
///
/// Upstream arrays cannot be expressed with `__` paths, so they use the
/// dedicated form `<PREFIX>__UPSTREAM_<NAME>__<FIELD>`. `<NAME>` groups one
/// upstream's fields together and orders entries alphabetically; if any such
/// variable is set, the resulting list replaces the file's `upstreams` array.
pub fn load_config<T: DeserializeOwned>(
    config_path: &str,
    env_prefix: &str,
    list_keys: &[&str],
) -> Result<T, ConfigError> {
    let prefix_marker = format!("{}__", env_prefix.to_uppercase());
    if !Path::new(config_path).exists()
        && !std::env::vars().any(|(key, _)| key.to_uppercase().starts_with(&prefix_marker))
    {
        return Err(ConfigError::Message(format!(
            "no configuration found: `{config_path}` does not exist and no `{prefix_marker}*` \
             environment variables are set. Supply a TOML file (-c/--config) or set \
             `{prefix_marker}*` environment variables"
        )));
    }

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

    let mut builder = Config::builder()
        .add_source(File::new(config_path, FileFormat::Toml).required(false))
        .add_source(environment);

    // Upstreams defined as `<PREFIX>__UPSTREAM_<NAME>__<FIELD>` are assembled into
    // an array and applied as a single override, which replaces the file's array.
    if let Some(upstreams) = collect_env_upstreams(env_prefix) {
        builder = builder.set_override("upstreams", upstreams)?;
    }

    builder.build()?.try_deserialize()
}

/// Collects `<PREFIX>__UPSTREAM_<NAME>__<FIELD>` environment variables into a
/// `config` array value, or `None` if none are set.
fn collect_env_upstreams(env_prefix: &str) -> Option<Value> {
    let marker = format!("{}__UPSTREAM_", env_prefix.to_uppercase());

    // name -> (field -> raw value); BTreeMaps keep a deterministic, sorted order.
    let mut grouped: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for (key, value) in std::env::vars() {
        let Some(rest) = key.to_uppercase().strip_prefix(&marker).map(str::to_owned) else {
            continue;
        };
        // Split the upstream name from the field path (the first `__` after the name).
        let Some((name, field)) = rest.split_once("__") else {
            continue;
        };
        if name.is_empty() || field.is_empty() {
            continue;
        }
        grouped
            .entry(name.to_owned())
            .or_default()
            .insert(field.to_lowercase().replace("__", "."), value);
    }

    if grouped.is_empty() {
        return None;
    }

    let entries: Vec<Value> = grouped
        .into_values()
        .map(|fields| {
            let mut table: Map<String, Value> = Map::new();
            for (field, raw) in fields {
                table.insert(field, parse_env_value(raw));
            }
            Value::new(None, ValueKind::Table(table))
        })
        .collect();

    Some(Value::new(None, ValueKind::Array(entries)))
}

/// Parses a raw environment string into a typed `config` value, mirroring the
/// `Environment` source's `try_parsing` behaviour (bool, then i64, then f64,
/// else string).
fn parse_env_value(raw: String) -> Value {
    let kind = if let Ok(b) = raw.to_lowercase().parse::<bool>() {
        ValueKind::Boolean(b)
    } else if let Ok(i) = raw.parse::<i64>() {
        ValueKind::I64(i)
    } else if let Ok(f) = raw.parse::<f64>() {
        ValueKind::Float(f)
    } else {
        ValueKind::String(raw)
    };
    Value::new(None, kind)
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

    #[test]
    fn no_config_sources_reports_helpful_error() {
        // No file and no `EMPTY__*` env vars: the error must tell the user
        // where configuration can come from.
        let missing = env::temp_dir().join("loader-test-no-sources.toml");
        let err = load_config::<TestConfig>(missing.to_str().unwrap(), "EMPTY", list_keys())
            .expect_err("must fail without any config source");
        let message = err.to_string();
        assert!(message.contains("no configuration found"));
        assert!(message.contains("EMPTY__*"));
    }

    #[derive(Debug, Deserialize)]
    struct TestUpstream {
        address: String,
        port: u16,
        user_identity: String,
    }

    #[derive(Debug, Deserialize)]
    struct UpstreamConfig {
        #[serde(default)]
        upstreams: Vec<TestUpstream>,
    }

    #[test]
    fn env_upstreams_replace_file_array() {
        // The file defines one upstream; the environment defines a different one.
        let path = write_toml(
            "upstream-replace",
            r#"
                [[upstreams]]
                address = "from-file"
                port = 1111
                user_identity = "file-user"
            "#,
        );

        env::set_var("UP__UPSTREAM_PRIMARY__ADDRESS", "jd_client_sv2");
        env::set_var("UP__UPSTREAM_PRIMARY__PORT", "34265");
        env::set_var("UP__UPSTREAM_PRIMARY__USER_IDENTITY", "env-user");

        let cfg: UpstreamConfig =
            load_config(path.to_str().unwrap(), "UP", &[]).expect("load config");

        // The env-defined upstream fully replaces the file's array.
        assert_eq!(cfg.upstreams.len(), 1);
        assert_eq!(cfg.upstreams[0].address, "jd_client_sv2");
        assert_eq!(cfg.upstreams[0].port, 34265); // parsed as a number
        assert_eq!(cfg.upstreams[0].user_identity, "env-user");

        for var in [
            "UP__UPSTREAM_PRIMARY__ADDRESS",
            "UP__UPSTREAM_PRIMARY__PORT",
            "UP__UPSTREAM_PRIMARY__USER_IDENTITY",
        ] {
            env::remove_var(var);
        }
    }

    #[test]
    fn env_upstreams_ordered_by_name() {
        let missing = env::temp_dir().join("loader-test-no-file.toml");

        // Defined out of order; entries must come out sorted by <NAME> (A before B).
        env::set_var("ORD__UPSTREAM_B__ADDRESS", "second");
        env::set_var("ORD__UPSTREAM_B__PORT", "2");
        env::set_var("ORD__UPSTREAM_B__USER_IDENTITY", "b");
        env::set_var("ORD__UPSTREAM_A__ADDRESS", "first");
        env::set_var("ORD__UPSTREAM_A__PORT", "1");
        env::set_var("ORD__UPSTREAM_A__USER_IDENTITY", "a");

        let cfg: UpstreamConfig =
            load_config(missing.to_str().unwrap(), "ORD", &[]).expect("load config");

        assert_eq!(cfg.upstreams.len(), 2);
        assert_eq!(cfg.upstreams[0].address, "first");
        assert_eq!(cfg.upstreams[1].address, "second");

        for var in [
            "ORD__UPSTREAM_B__ADDRESS",
            "ORD__UPSTREAM_B__PORT",
            "ORD__UPSTREAM_B__USER_IDENTITY",
            "ORD__UPSTREAM_A__ADDRESS",
            "ORD__UPSTREAM_A__PORT",
            "ORD__UPSTREAM_A__USER_IDENTITY",
        ] {
            env::remove_var(var);
        }
    }
}
