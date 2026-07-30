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
/// Fields named in `list_keys` (lowercase `snake_case`, nested paths joined
/// with `.`, e.g. `miner_telemetry.cidrs`) are parsed as comma-separated
/// lists, e.g. `POOL__SUPPORTED_EXTENSIONS=1,2,3`. A single value without a
/// separator, e.g. `POOL__SUPPORTED_EXTENSIONS=2`, is a 1-element list.
///
/// Top-level fields named in `enum_keys` hold externally tagged enums (e.g.
/// `template_provider_type`). Variant tables from different sources would
/// otherwise merge — the file choosing one variant and the environment another
/// leaves two variant keys behind and fails deserialization — so when the
/// environment names a variant for such a key, it replaces the file's variant.
///
/// Upstream arrays cannot be expressed with `__` paths, so they use the
/// dedicated form `<PREFIX>__UPSTREAM_<NAME>__<FIELD>`. `<NAME>` groups one
/// upstream's fields together and orders entries alphabetically; if any such
/// variable is set, the resulting list replaces the file's `upstreams` array.
pub fn load_config<T: DeserializeOwned>(
    config_path: impl AsRef<Path>,
    env_prefix: &str,
    list_keys: &[&str],
    enum_keys: &[&str],
) -> Result<T, ConfigError> {
    let config_path = config_path.as_ref();
    let prefix_marker = format!("{}__", env_prefix.to_uppercase());
    if !config_path.exists()
        && !std::env::vars().any(|(key, _)| key.to_uppercase().starts_with(&prefix_marker))
    {
        return Err(ConfigError::Message(format!(
            "no configuration found: `{}` does not exist and no `{prefix_marker}*` \
             environment variables are set. Supply a TOML file (-c/--config) or set \
             `{prefix_marker}*` environment variables",
            config_path.display()
        )));
    }

    // `try_parsing` lets scalar environment values (numbers and booleans) be
    // coerced into their target types instead of staying raw strings.
    let environment = Environment::with_prefix(env_prefix)
        .separator("__")
        .try_parsing(true);

    let mut builder = Config::builder()
        .add_source(
            File::from(config_path)
                .format(FileFormat::Toml)
                .required(false),
        )
        .add_source(environment);

    // List-valued fields are split on `,` and applied as overrides. This is done
    // here rather than through the `Environment` source's list support because
    // the latter reads a separator-less value (`KEY=2`) as a scalar instead of
    // a 1-element list.
    for key in list_keys {
        let wanted = format!("{prefix_marker}{}", key.to_uppercase().replace('.', "__"));
        let Some((_, raw)) = std::env::vars().find(|(k, _)| k.to_uppercase() == wanted) else {
            continue;
        };
        let items: Vec<Value> = raw
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(|item| parse_env_value(item.to_owned()))
            .collect();
        builder = builder.set_override(*key, Value::new(None, ValueKind::Array(items)))?;
    }

    // Upstreams defined as `<PREFIX>__UPSTREAM_<NAME>__<FIELD>` are assembled into
    // an array and applied as a single override, which replaces the file's array.
    if let Some(upstreams) = collect_env_upstreams(env_prefix) {
        builder = builder.set_override("upstreams", upstreams)?;
    }

    let config = builder.build()?;
    if enum_keys.is_empty() {
        return config.try_deserialize();
    }

    // When the environment picks an enum variant, drop the other variants that
    // merged in from the file so exactly one variant key remains.
    let mut root: Map<String, Value> = config.try_deserialize()?;
    for key in enum_keys {
        let Some(variant) = env_enum_variant(&prefix_marker, key)? else {
            continue;
        };
        if let Some(value) = root.get_mut(*key) {
            if let ValueKind::Table(table) = &mut value.kind {
                table.retain(|name, _| name.eq_ignore_ascii_case(&variant));
            }
        }
    }
    Value::new(None, ValueKind::Table(root)).try_deserialize()
}

/// Returns the enum variant that the environment selects for the top-level
/// field `key` — the path segment following `<PREFIX>__<KEY>__` — or an error
/// if the environment names more than one variant.
fn env_enum_variant(prefix_marker: &str, key: &str) -> Result<Option<String>, ConfigError> {
    let marker = format!("{prefix_marker}{}__", key.to_uppercase());
    let mut variants: Vec<String> = Vec::new();
    for (env_key, _) in std::env::vars() {
        let Some(rest) = env_key
            .to_uppercase()
            .strip_prefix(&marker)
            .map(str::to_owned)
        else {
            continue;
        };
        let variant = rest.split("__").next().unwrap_or(&rest).to_lowercase();
        if !variant.is_empty() && !variants.contains(&variant) {
            variants.push(variant);
        }
    }
    match variants.len() {
        0 => Ok(None),
        1 => Ok(variants.pop()),
        _ => {
            variants.sort();
            Err(ConfigError::Message(format!(
                "conflicting `{marker}*` variants in the environment: {}; set exactly one",
                variants.join(", ")
            )))
        }
    }
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

    fn set_env(key: &str, value: &str) {
        unsafe { env::set_var(key, value) }
    }

    fn remove_env(key: &str) {
        unsafe { env::remove_var(key) }
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
        set_env("OVR__LISTEN_ADDRESS", "0.0.0.0:2222");
        set_env("OVR__NESTED__PORT", "20");

        let cfg: TestConfig =
            load_config(path.to_str().unwrap(), "OVR", list_keys(), &[]).expect("load config");

        assert_eq!(cfg.listen_address, "0.0.0.0:2222"); // from env
        assert_eq!(cfg.cert_validity_sec, 3600); // from file
        assert_eq!(cfg.nested.port, 20); // nested override from env

        remove_env("OVR__LISTEN_ADDRESS");
        remove_env("OVR__NESTED__PORT");
    }

    #[test]
    fn env_only_without_file() {
        // No file exists at this path: configuration comes entirely from env.
        let missing = env::temp_dir().join("loader-test-does-not-exist.toml");

        set_env("ENVONLY__LISTEN_ADDRESS", "127.0.0.1:3333");
        set_env("ENVONLY__CERT_VALIDITY_SEC", "1200");
        set_env("ENVONLY__VERIFY_PAYOUT", "true");
        set_env("ENVONLY__SUPPORTED_EXTENSIONS", "1,2,3");
        set_env("ENVONLY__NESTED__PORT", "42");

        let cfg: TestConfig = load_config(missing.to_str().unwrap(), "ENVONLY", list_keys(), &[])
            .expect("load config");

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
            remove_env(var);
        }
    }

    #[test]
    fn single_value_list_key_becomes_one_element_list() {
        let path = write_toml(
            "single-list",
            r#"
                listen_address = "0.0.0.0:1111"
                cert_validity_sec = 3600
                supported_extensions = [1, 2]
                [nested]
                port = 10
            "#,
        );

        // A single value without a separator must become a 1-element list.
        set_env("SINGLE__SUPPORTED_EXTENSIONS", "2");

        let cfg: TestConfig =
            load_config(path.to_str().unwrap(), "SINGLE", list_keys(), &[]).expect("load config");
        assert_eq!(cfg.supported_extensions, vec![2]);

        remove_env("SINGLE__SUPPORTED_EXTENSIONS");
    }

    #[derive(Debug, Deserialize)]
    struct NestedListConfig {
        #[serde(default)]
        telemetry: Telemetry,
    }

    #[derive(Debug, Deserialize, Default)]
    struct Telemetry {
        #[serde(default)]
        cidrs: Vec<String>,
    }

    #[test]
    fn nested_list_key_parses_from_env() {
        let missing = env::temp_dir().join("loader-test-nested-list.toml");

        set_env("NESTEDLIST__TELEMETRY__CIDRS", "192.168.1.0/24");

        let cfg: NestedListConfig = load_config(
            missing.to_str().unwrap(),
            "NESTEDLIST",
            &["telemetry.cidrs"],
            &[],
        )
        .expect("load config");
        assert_eq!(cfg.telemetry.cidrs, vec!["192.168.1.0/24"]);

        remove_env("NESTEDLIST__TELEMETRY__CIDRS");
    }

    #[test]
    fn missing_required_field_errors() {
        // The env supplies some fields but not all required ones.
        let missing = env::temp_dir().join("loader-test-empty.toml");
        set_env("PARTIAL__LISTEN_ADDRESS", "0.0.0.0:1111");

        let result: Result<TestConfig, _> =
            load_config(missing.to_str().unwrap(), "PARTIAL", list_keys(), &[]);
        assert!(result.is_err());

        remove_env("PARTIAL__LISTEN_ADDRESS");
    }

    #[test]
    fn no_config_sources_reports_helpful_error() {
        // No file and no `EMPTY__*` env vars: the error must tell the user
        // where configuration can come from.
        let missing = env::temp_dir().join("loader-test-no-sources.toml");
        let err = load_config::<TestConfig>(missing.to_str().unwrap(), "EMPTY", list_keys(), &[])
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

        set_env("UP__UPSTREAM_PRIMARY__ADDRESS", "jd_client_sv2");
        set_env("UP__UPSTREAM_PRIMARY__PORT", "34265");
        set_env("UP__UPSTREAM_PRIMARY__USER_IDENTITY", "env-user");

        let cfg: UpstreamConfig =
            load_config(path.to_str().unwrap(), "UP", &[], &[]).expect("load config");

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
            remove_env(var);
        }
    }

    #[test]
    fn env_upstreams_ordered_by_name() {
        let missing = env::temp_dir().join("loader-test-no-file.toml");

        // Defined out of order; entries must come out sorted by <NAME> (A before B).
        set_env("ORD__UPSTREAM_B__ADDRESS", "second");
        set_env("ORD__UPSTREAM_B__PORT", "2");
        set_env("ORD__UPSTREAM_B__USER_IDENTITY", "b");
        set_env("ORD__UPSTREAM_A__ADDRESS", "first");
        set_env("ORD__UPSTREAM_A__PORT", "1");
        set_env("ORD__UPSTREAM_A__USER_IDENTITY", "a");

        let cfg: UpstreamConfig =
            load_config(missing.to_str().unwrap(), "ORD", &[], &[]).expect("load config");

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
            remove_env(var);
        }
    }

    #[derive(Debug, Deserialize)]
    enum TestTp {
        Sv2Tp { address: String },
        BitcoinCoreIpc { version: u16, network: String },
    }

    #[derive(Debug, Deserialize)]
    struct TpConfig {
        template_provider_type: TestTp,
    }

    const ENUM_KEYS: &[&str] = &["template_provider_type"];

    #[test]
    fn env_switches_enum_variant() {
        // The file picks one variant; the environment picks the other. The
        // environment's variant must fully replace the file's.
        let path = write_toml(
            "enum-switch",
            r#"
                [template_provider_type.Sv2Tp]
                address = "tp:8442"
            "#,
        );

        set_env(
            "TPSWITCH__TEMPLATE_PROVIDER_TYPE__BITCOINCOREIPC__VERSION",
            "31",
        );
        set_env(
            "TPSWITCH__TEMPLATE_PROVIDER_TYPE__BITCOINCOREIPC__NETWORK",
            "mainnet",
        );

        let cfg: TpConfig =
            load_config(path.to_str().unwrap(), "TPSWITCH", &[], ENUM_KEYS).expect("load config");
        match cfg.template_provider_type {
            TestTp::BitcoinCoreIpc { version, network } => {
                assert_eq!(version, 31);
                assert_eq!(network, "mainnet");
            }
            other => panic!("expected BitcoinCoreIpc, got {other:?}"),
        }

        remove_env("TPSWITCH__TEMPLATE_PROVIDER_TYPE__BITCOINCOREIPC__VERSION");
        remove_env("TPSWITCH__TEMPLATE_PROVIDER_TYPE__BITCOINCOREIPC__NETWORK");

        // And the other direction: file picks BitcoinCoreIpc, env picks Sv2Tp.
        let path = write_toml(
            "enum-switch-back",
            r#"
                [template_provider_type.BitcoinCoreIpc]
                version = 31
                network = "mainnet"
            "#,
        );

        set_env(
            "TPSWITCHBACK__TEMPLATE_PROVIDER_TYPE__SV2TP__ADDRESS",
            "tp:8442",
        );

        let cfg: TpConfig = load_config(path.to_str().unwrap(), "TPSWITCHBACK", &[], ENUM_KEYS)
            .expect("load config");
        match cfg.template_provider_type {
            TestTp::Sv2Tp { address } => assert_eq!(address, "tp:8442"),
            other => panic!("expected Sv2Tp, got {other:?}"),
        }

        remove_env("TPSWITCHBACK__TEMPLATE_PROVIDER_TYPE__SV2TP__ADDRESS");
    }

    #[test]
    fn env_merges_fields_within_same_enum_variant() {
        // Same variant on both sides: individual fields still merge, with the
        // environment taking precedence.
        let path = write_toml(
            "enum-merge",
            r#"
                [template_provider_type.BitcoinCoreIpc]
                version = 30
                network = "signet"
            "#,
        );

        set_env(
            "TPMERGE__TEMPLATE_PROVIDER_TYPE__BITCOINCOREIPC__VERSION",
            "31",
        );

        let cfg: TpConfig =
            load_config(path.to_str().unwrap(), "TPMERGE", &[], ENUM_KEYS).expect("load config");
        match cfg.template_provider_type {
            TestTp::BitcoinCoreIpc { version, network } => {
                assert_eq!(version, 31); // from env
                assert_eq!(network, "signet"); // from file
            }
            other => panic!("expected BitcoinCoreIpc, got {other:?}"),
        }

        remove_env("TPMERGE__TEMPLATE_PROVIDER_TYPE__BITCOINCOREIPC__VERSION");
    }

    #[test]
    fn conflicting_env_enum_variants_error() {
        let missing = env::temp_dir().join("loader-test-enum-conflict.toml");

        set_env(
            "TPCONFLICT__TEMPLATE_PROVIDER_TYPE__SV2TP__ADDRESS",
            "tp:8442",
        );
        set_env(
            "TPCONFLICT__TEMPLATE_PROVIDER_TYPE__BITCOINCOREIPC__VERSION",
            "31",
        );

        let err = load_config::<TpConfig>(missing.to_str().unwrap(), "TPCONFLICT", &[], ENUM_KEYS)
            .expect_err("must fail with two env variants");
        assert!(err.to_string().contains("set exactly one"));

        remove_env("TPCONFLICT__TEMPLATE_PROVIDER_TYPE__SV2TP__ADDRESS");
        remove_env("TPCONFLICT__TEMPLATE_PROVIDER_TYPE__BITCOINCOREIPC__VERSION");
    }
}
