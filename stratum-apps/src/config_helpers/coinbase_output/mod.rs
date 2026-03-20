mod errors;
mod serde_types;

use miniscript::{
    bitcoin::{address::NetworkUnchecked, Address, Network, ScriptBuf},
    descriptor::DescriptorPublicKey,
    DefiniteDescriptorKey, Descriptor,
};

pub use errors::Error;

/// Coinbase output transaction.
///
/// Typically used for parsing coinbase outputs defined in SRI role configuration files.
///
/// Supports two modes:
/// 1. **Static address**: A fixed address (e.g., `addr(bc1q...)`) - `script_pubkey` is set directly
/// 2. **Wildcard descriptor**: A derivable descriptor (e.g., `wpkh(xpub.../0/*)`) - stores the
///    descriptor string for later derivation via [`XpubDerivator`](crate::config_helpers::XpubDerivator)
///
/// Use [`has_wildcard()`](Self::has_wildcard) to check which mode is active.
#[derive(Debug, serde::Deserialize, Clone)]
#[serde(try_from = "serde_types::SerdeCoinbaseOutput")]
pub struct CoinbaseRewardScript {
    /// The script pubkey for static addresses, or the index-0 derived script for wildcards.
    script_pubkey: ScriptBuf,
    /// Whether this output is valid for mainnet.
    ok_for_mainnet: bool,
    /// For wildcard descriptors, stores the original descriptor string for later derivation.
    /// None for static addresses.
    /// Note: We store the string instead of `Descriptor<DescriptorPublicKey>` because the latter
    /// is not `Send + Sync` due to internal `RefCell` usage for taproot caching.
    wildcard_descriptor_str: Option<String>,
}

impl CoinbaseRewardScript {
    /// Creates a new [`CoinbaseRewardScript`] from a descriptor string.
    ///
    /// Supports both static descriptors (e.g., `addr(bc1q...)`) and wildcard descriptors
    /// (e.g., `wpkh(xpub.../0/*)`).
    ///
    /// For wildcard descriptors, the initial `script_pubkey` is derived at index 0.
    /// Use [`has_wildcard()`](Self::has_wildcard) and [`wildcard_descriptor_str()`](Self::wildcard_descriptor_str)
    /// to access the underlying descriptor for runtime derivation.
    pub fn from_descriptor(s: &str) -> Result<Self, Error> {
        // First, try to parse as a wildcard descriptor (DescriptorPublicKey).
        // This handles descriptors like wpkh(xpub.../0/*).
        if let Ok(wildcard_desc) = s.parse::<Descriptor<DescriptorPublicKey>>() {
            if wildcard_desc.has_wildcard() {
                // Derive at index 0 to get the initial script_pubkey
                let definite = wildcard_desc
                    .at_derivation_index(0)
                    .map_err(|e| Error::Miniscript(miniscript::Error::Unexpected(e.to_string())))?;

                return Ok(Self {
                    script_pubkey: definite.script_pubkey(),
                    ok_for_mainnet: true,
                    wildcard_descriptor_str: Some(s.to_string()),
                });
            }
        }

        // Taproot descriptors cannot be parsed with `expression::Tree::from_str` and
        // need special handling. So we special-case them early and just pass to
        // rust-miniscript. In Miniscript 13 we will not need to do this.
        if s.starts_with("tr") {
            let desc = s.parse::<Descriptor<DefiniteDescriptorKey>>()?;
            return Ok(Self {
                script_pubkey: desc.script_pubkey(),
                // Descriptors don't have a way to specify a network, so we assume
                // they are OK to be used on mainnet.
                ok_for_mainnet: true,
                wildcard_descriptor_str: None,
            });
        }

        let tree = miniscript::expression::Tree::from_str(s)?;
        let root = tree.root();
        match root.name() {
            "addr" => {
                let addr: Address<NetworkUnchecked> = root
                    .verify_terminal_parent("addr", "a valid Bitcoin address")
                    .map_err(miniscript::Error::Parse)?;

                Ok(Self {
                    script_pubkey: addr.assume_checked_ref().script_pubkey(),
                    ok_for_mainnet: addr.is_valid_for_network(Network::Bitcoin),
                    wildcard_descriptor_str: None,
                })
            }
            "raw" => {
                let script_hex: String = root
                    .verify_terminal_parent(
                        "raw",
                        "a hex-encoded Bitcoin script without length prefix",
                    )
                    .map_err(miniscript::Error::Parse)?;

                Ok(Self {
                    script_pubkey: ScriptBuf::from_hex(&script_hex)?,
                    // Users of hex scriptpubkeys are on their own.
                    ok_for_mainnet: true,
                    wildcard_descriptor_str: None,
                })
            }
            _ => {
                use miniscript::expression::FromTree as _;

                let desc = Descriptor::<DefiniteDescriptorKey>::from_tree(root)?;
                Ok(Self {
                    script_pubkey: desc.script_pubkey(),
                    // Descriptors don't have a way to specify a network, so we assume
                    // they are OK to be used on mainnet.
                    ok_for_mainnet: true,
                    wildcard_descriptor_str: None,
                })
            }
        }
    }

    /// Whether this coinbase output is okay for use on mainnet.
    ///
    /// This is a "best effort" check and currently only returns false if the user
    /// provides an addr() descriptor in which they specified a testnet or regtest
    /// address.
    pub fn ok_for_mainnet(&self) -> bool {
        self.ok_for_mainnet
    }

    /// The `scriptPubKey` associated with the coinbase output.
    ///
    /// For wildcard descriptors, this returns the script derived at index 0.
    /// To get scripts at other indices, use [`wildcard_descriptor_str()`](Self::wildcard_descriptor_str)
    /// with [`XpubDerivator`](crate::config_helpers::XpubDerivator).
    pub fn script_pubkey(&self) -> ScriptBuf {
        self.script_pubkey.clone()
    }

    /// Returns `true` if this is a wildcard descriptor that supports rotation.
    ///
    /// Wildcard descriptors contain `/*` in the derivation path (e.g., `wpkh(xpub.../0/*)`).
    /// When rotation is enabled, a new address should be derived for each block found.
    pub fn has_wildcard(&self) -> bool {
        self.wildcard_descriptor_str.is_some()
    }

    /// Returns the underlying wildcard descriptor string, if any.
    ///
    /// Use this with [`XpubDerivator`](crate::config_helpers::XpubDerivator) to derive
    /// addresses at specific indices for coinbase rotation.
    ///
    /// Returns `None` for static addresses (non-wildcard descriptors).
    pub fn wildcard_descriptor_str(&self) -> Option<&str> {
        self.wildcard_descriptor_str.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_vector_addr() {
        // Valid
        assert_eq!(
            CoinbaseRewardScript::from_descriptor(
                "addr(1BvBMSEYstWetqTFn5Au4m4GFg7xJaNVN2)#wdnlkpe8"
            )
            .unwrap()
            .script_pubkey()
            .to_hex_string(),
            "76a91477bff20c60e522dfaa3350c39b030a5d004e839a88ac",
        );
        assert_eq!(
            CoinbaseRewardScript::from_descriptor(
                "addr(3J98t1WpEZ73CNmQviecrnyiWrnqRhWNLy)#rsjl0crt"
            )
            .unwrap()
            .script_pubkey()
            .to_hex_string(),
            "a914b472a266d0bd89c13706a4132ccfb16f7c3b9fcb87",
        );
        assert_eq!(
            CoinbaseRewardScript::from_descriptor(
                "addr(bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4)#uyjndxcw"
            )
            .unwrap()
            .script_pubkey()
            .to_hex_string(),
            "0014751e76e8199196d454941c45d1b3a323f1433bd6",
        );
        assert_eq!(
            CoinbaseRewardScript::from_descriptor(
                "addr(bc1qrp33g0q5c5txsp9arysrx4k6zdkfs4nce4xj0gdcccefvpysxf3qccfmv3)#8kzm8txf"
            )
            .unwrap()
            .script_pubkey()
            .to_hex_string(),
            "00201863143c14c5166804bd19203356da136c985678cd4d27a1b8c6329604903262",
        );
        // no checksum is ok
        assert_eq!(
            CoinbaseRewardScript::from_descriptor("addr(1BvBMSEYstWetqTFn5Au4m4GFg7xJaNVN2)")
                .unwrap()
                .script_pubkey()
                .to_hex_string(),
            "76a91477bff20c60e522dfaa3350c39b030a5d004e839a88ac",
        );
        assert_eq!(
            CoinbaseRewardScript::from_descriptor("addr(1BvBMSEYstWetqTFn5Au4m4GFg7xJaNVN2,)")
                .unwrap_err()
                .to_string(),
            "Miniscript: addr must have 1 children, but found 2",
        );

        // Invalid
        // But empty checksum is not (in Miniscript 13 these error messages will be cleaner)
        assert_eq!(
            CoinbaseRewardScript::from_descriptor("addr(1BvBMSEYstWetqTFn5Au4m4GFg7xJaNVN2)#")
                .unwrap_err()
                .to_string(),
            "Miniscript: invalid checksum (length 0, expected 8)",
        );
        assert_eq!(
            CoinbaseRewardScript::from_descriptor(
                "addr(1BvBMSEYstWetqTFn5Au4m4GFg7xJaNVN2)#wdnlkpe7"
            )
            .unwrap_err()
            .to_string(),
            "Miniscript: invalid checksum wdnlkpe7; expected wdnlkpe8",
        );
        // Bad base58ck checksum even though the descriptor checksum is OK. Note that rust-bitcoin
        // 0.32 interprets bad bech32 checksums as "base58 errors" because it doesn't know
        // what encoding an invalid string is supposed to have. See https://github.com/rust-bitcoin/rust-bitcoin/issues/3044
        // Expected error: "Bitcoin address: base58 error: incorrect checksum: base58 checksum
        // 0x6c7615f4 does not match expected 0x6b7615f4" (hex-conservative v0.3.0)
        // or "Bitcoin address: base58 error" (hex-conservative v0.2.1)
        assert!(CoinbaseRewardScript::from_descriptor(
            "addr(1BvBMSEYstWetqTFn5Au4m4GFg7xJaNVN3)#5v55uzec"
        )
        .is_err());
        // Expected error: "Bitcoin address: base58 error: decode: invalid base58 character 0x30"
        // (hex-conservative v0.3.0) or "Bitcoin address: base58 error" (hex-conservative
        // v0.2.1)
        assert!(CoinbaseRewardScript::from_descriptor(
            "addr(bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t3)#wfr7lfxf"
        )
        .is_err());
        // Flagrantly bad stuff -- should probably PR these upstream to rust-miniscript.
        // Expected error: "Bitcoin address: base58 error: too short: base58 decoded data was not
        // long enough, must be at least 4 byte: 0" (hex-conservative v0.3.0) or "Bitcoin
        // address: base58 error" (hex-conservative v0.2.1)
        assert!(CoinbaseRewardScript::from_descriptor("addr()").is_err());
        assert_eq!(
            CoinbaseRewardScript::from_descriptor("addr(It's a mad mad world!?! 🙃)")
                .unwrap_err()
                .to_string(),
            "Miniscript: invalid character '🙃' (position 29)",
        );
        // This error is just wrong lol. Fixed in Miniscript 13.
        assert_eq!(
            CoinbaseRewardScript::from_descriptor("addr(It's a mad mad world!?! 🙃)#abcdefg")
                .unwrap_err()
                .to_string(),
            "Miniscript: invalid character '🙃' (position 29)",
        );
        // Expected error: "Bitcoin address: base58 error: decode: invalid base58 character 0x49"
        // (hex-conservative v0.3.0) or "Bitcoin address: base58 error" (hex-conservative
        // v0.2.1)
        assert!(
            CoinbaseRewardScript::from_descriptor("addr(It's a mad mad world!?!)#hmeprl29")
                .is_err()
        );
        assert_eq!(
            CoinbaseRewardScript::from_descriptor("addr(It's a mad mad world!?!)#🙃🙃🙃🙃🙃🙃")
                .unwrap_err()
                .to_string(),
            "Miniscript: invalid character '🙃' (position 30)",
        );
    }

    #[test]
    fn fixed_vector_combo() {
        // We do not support combo descriptors. Nobody should.
        assert_eq!(
            CoinbaseRewardScript::from_descriptor(
                "combo(0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798)"
            )
            .unwrap_err()
            .to_string(),
            "Miniscript: unrecognized name 'combo'",
        );
    }

    #[test]
    fn fixed_vector_musig() {
        // We do not support musig descriptors. One day.
        assert_eq!(
            CoinbaseRewardScript::from_descriptor("musig(0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798,03fff97bd5755eeea420453a14355235d382f6472f8568a18b2f057a1460297556)").unwrap_err().to_string(),
            "Miniscript: unrecognized name '03fff97bd5755eeea420453a14355235d382f6472f8568a18b2f057a1460297556'",
        );
        assert_eq!(
            CoinbaseRewardScript::from_descriptor("tr(musig(0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798,03fff97bd5755eeea420453a14355235d382f6472f8568a18b2f057a1460297556))").unwrap_err().to_string(),
            "Miniscript: internal key must have no children, but found 2",
        );
    }

    #[test]
    fn fixed_vector_raw() {
        // Empty raw descriptors are OK; correspond to the empty script.
        assert_eq!(
            CoinbaseRewardScript::from_descriptor("raw()")
                .unwrap()
                .script_pubkey()
                .to_hex_string(),
            "",
        );
        assert_eq!(
            CoinbaseRewardScript::from_descriptor("raw(deadbeef)")
                .unwrap()
                .script_pubkey()
                .to_hex_string(),
            "deadbeef",
        );
        assert_eq!(
            CoinbaseRewardScript::from_descriptor("raw(DEADBEEF)")
                .unwrap()
                .script_pubkey()
                .to_hex_string(),
            "deadbeef",
        );
        // Should we allow this? We do, so I guess we should test it and make sure we don't stop..
        assert_eq!(
            CoinbaseRewardScript::from_descriptor("raw(DEADbeef)")
                .unwrap()
                .script_pubkey()
                .to_hex_string(),
            "deadbeef",
        );
        // Expected error: "Decoding hex-formatted script: odd length, failed to create bytes from
        // hex: odd hex string length 1" (hex-conservative v0.3.0) or "Decoding
        // hex-formatted script: odd length, failed to create bytes from hex" (hex-conservative
        // v0.2.1)
        assert!(CoinbaseRewardScript::from_descriptor("raw(0)").is_err());
        assert_eq!(
            CoinbaseRewardScript::from_descriptor("raw(0,1)")
                .unwrap_err()
                .to_string(),
            "Miniscript: raw must have 1 children, but found 2",
        );
    }

    #[test]
    fn fixed_vector_miniscript() {
        assert_eq!(
            CoinbaseRewardScript::from_descriptor("sh(wsh(multi(2,0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798,03fff97bd5755eeea420453a14355235d382f6472f8568a18b2f057a1460297556)))#qpcmf2lu").unwrap().script_pubkey().to_hex_string(),
            "a9141cb55de50b72c67709ab16307d69557e6bb1a98787",
        );
        assert_eq!(
            CoinbaseRewardScript::from_descriptor(
                "tr(0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798)"
            )
            .unwrap()
            .script_pubkey()
            .to_hex_string(),
            "5120da4710964f7852695de2da025290e24af6d8c281de5a0b902b7135fd9fd74d21",
        );
        assert_eq!(
            CoinbaseRewardScript::from_descriptor("tr(0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798,{pk(03fff97bd5755eeea420453a14355235d382f6472f8568a18b2f057a1460297556),{multi_a(2,026a245bf6dc698504c89a20cfded60853152b695336c28063b61c65cbd269e6b4,0231ecbfac95d972f0b8f81ec6e01e9c621d91a4b48d5f9d12d7e95febe9f34d64),multi_a(2,026a245bf6dc698504c89a20cfded60853152b695336c28063b61c65cbd269e6b4,0231ecbfac95d972f0b8f81ec6e01e9c621d91a4b48d5f9d12d7e95febe9f34d64)}})")
            .unwrap()
            .script_pubkey()
            .to_hex_string(),
            "5120493bdae0d225af5cb88c4cb2a1e1e89e391153ba7699c91ebee2fd082ed1636c",
        );
    }

    #[test]
    fn fixed_vector_keys() {
        // xpub
        assert_eq!(
            CoinbaseRewardScript::from_descriptor("pkh(xpub661MyMwAqRbcFtXgS5sYJABqqG9YLmC4Q1Rdap9gSE8NqtwybGhePY2gZ29ESFjqJoCu1Rupje8YtGqsefD265TMg7usUDFdp6W1EGMcet8)").unwrap().script_pubkey().to_hex_string(),
            "76a9143442193e1bb70916e914552172cd4e2dbc9df81188ac",
        );
        // xpub with non-hardened path
        assert_eq!(
            CoinbaseRewardScript::from_descriptor("pkh(xpub661MyMwAqRbcFtXgS5sYJABqqG9YLmC4Q1Rdap9gSE8NqtwybGhePY2gZ29ESFjqJoCu1Rupje8YtGqsefD265TMg7usUDFdp6W1EGMcet8/1/2/3)").unwrap().script_pubkey().to_hex_string(),
            "76a914f2d2e1401c88353c2298d1a928d4ed827ff46ff688ac",
        );
        // xpub with hardened path (not allowed)
        assert_eq!(
            CoinbaseRewardScript::from_descriptor("pkh(xpub661MyMwAqRbcFtXgS5sYJABqqG9YLmC4Q1Rdap9gSE8NqtwybGhePY2gZ29ESFjqJoCu1Rupje8YtGqsefD265TMg7usUDFdp6W1EGMcet8/1'/2/3)").unwrap_err().to_string(),
            "Miniscript: key with hardened derivation steps cannot be a DerivedDescriptorKey",
        );
        // wildcards ARE now allowed - they create a wildcard descriptor for rotation
        let wildcard_desc = CoinbaseRewardScript::from_descriptor(
            "pkh(xpub661MyMwAqRbcFtXgS5sYJABqqG9YLmC4Q1Rdap9gSE8NqtwybGhePY2gZ29ESFjqJoCu1Rupje8YtGqsefD265TMg7usUDFdp6W1EGMcet8/*)"
        ).unwrap();
        assert!(wildcard_desc.has_wildcard());
        assert!(wildcard_desc.wildcard_descriptor_str().is_some());
        // script_pubkey should be derived at index 0
        assert!(!wildcard_desc.script_pubkey().is_empty());

        // No multipath descriptors allowed; this is not a wallet with change
        assert_eq!(
            CoinbaseRewardScript::from_descriptor("pkh(xpub661MyMwAqRbcFtXgS5sYJABqqG9YLmC4Q1Rdap9gSE8NqtwybGhePY2gZ29ESFjqJoCu1Rupje8YtGqsefD265TMg7usUDFdp6W1EGMcet8/<0;1>)").unwrap_err().to_string(),
            "Miniscript: multipath key cannot be a DerivedDescriptorKey",
        );
        // Private keys are not allowed, or xprvs.
        assert_eq!(
            CoinbaseRewardScript::from_descriptor(
                "pkh(L4rK1yDtCWekvXuE6oXD9jCYfFNV2cWRpVuPLBcCU2z8TrisoyY1)"
            )
            .unwrap_err()
            .to_string(),
            "Miniscript: key too short",
        );
        // This is a confusing error message which should be fixed in Miniscript 13.
        assert_eq!(
            CoinbaseRewardScript::from_descriptor("pkh(xprv9s21ZrQH143K3QTDL4LXw2F7HEK3wJUD2nW2nRk4stbPy6cq3jPPqjiChkVvvNKmPGJxWUtg6LnF5kejMRNNU3TGtRBeJgk33yuGBxrMPHi)").unwrap_err().to_string(),
            "Miniscript: public keys must be 64, 66 or 130 characters in size",
        );
    }

    #[test]
    fn test_wildcard_wpkh_descriptor() {
        // wpkh with wildcard - common format for BIP84 wallets
        let desc = CoinbaseRewardScript::from_descriptor(
            "wpkh(tpubD6NzVbkrYhZ4XgiXtGrdW5XDAPFCL9h7we1vwNCpn8tGbBcgfVYjXyhWo4E1xkh56hjod1RhGjxbaTLV3X4FyWuejifB9jusQ46QzG87VKp/0/*)"
        ).unwrap();

        assert!(desc.has_wildcard());
        assert!(desc.wildcard_descriptor_str().is_some());

        // script_pubkey should be a valid p2wpkh script (starts with 0x0014)
        let script = desc.script_pubkey();
        assert!(script.to_hex_string().starts_with("0014"));
    }

    #[test]
    fn test_wildcard_tr_descriptor() {
        // Taproot with wildcard
        let desc = CoinbaseRewardScript::from_descriptor(
            "tr(tpubD6NzVbkrYhZ4XgiXtGrdW5XDAPFCL9h7we1vwNCpn8tGbBcgfVYjXyhWo4E1xkh56hjod1RhGjxbaTLV3X4FyWuejifB9jusQ46QzG87VKp/0/*)"
        ).unwrap();

        assert!(desc.has_wildcard());
        assert!(desc.wildcard_descriptor_str().is_some());

        // script_pubkey should be a valid p2tr script (starts with 0x5120)
        let script = desc.script_pubkey();
        assert!(script.to_hex_string().starts_with("5120"));
    }

    #[test]
    fn test_non_wildcard_has_no_descriptor() {
        // Static address - no wildcard
        let desc = CoinbaseRewardScript::from_descriptor(
            "addr(tb1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsx)",
        )
        .unwrap();

        assert!(!desc.has_wildcard());
        assert!(desc.wildcard_descriptor_str().is_none());
    }

    #[test]
    fn test_xpub_without_wildcard_has_no_descriptor() {
        // xpub with fixed path (no wildcard)
        let desc = CoinbaseRewardScript::from_descriptor(
            "wpkh(xpub661MyMwAqRbcFtXgS5sYJABqqG9YLmC4Q1Rdap9gSE8NqtwybGhePY2gZ29ESFjqJoCu1Rupje8YtGqsefD265TMg7usUDFdp6W1EGMcet8/0/0)"
        ).unwrap();

        assert!(!desc.has_wildcard());
        assert!(desc.wildcard_descriptor_str().is_none());
    }

    #[test]
    fn test_mainnet_xpub_wildcard() {
        // Mainnet xpub with wildcard
        let desc = CoinbaseRewardScript::from_descriptor(
            "wpkh(xpub661MyMwAqRbcFtXgS5sYJABqqG9YLmC4Q1Rdap9gSE8NqtwybGhePY2gZ29ESFjqJoCu1Rupje8YtGqsefD265TMg7usUDFdp6W1EGMcet8/0/*)"
        ).unwrap();

        assert!(desc.has_wildcard());
        // Mainnet xpubs are allowed
        assert!(desc.ok_for_mainnet());
    }
}
