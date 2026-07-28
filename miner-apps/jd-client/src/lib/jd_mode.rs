//! Global configuration for Job Declarator (JD) operating mode.
//!
//! This module defines different operating modes for the Job Declarator
//! and provides atomic accessors for setting and retrieving the current mode.
//!
//! Mode state is stored in `JDMode::inner` (`Arc<AtomicU8>`) to allow safe concurrent access
//! across threads.
use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};

use crate::config::ConfigJDCMode;

#[derive(Clone, Debug)]
pub struct JDMode {
    inner: Arc<AtomicU8>,
    // Currently, how JDC works is the mode in config
    // only gets activated once an upstream connection
    // is made.
    config_mode: ConfigJDCMode,
}

impl JDMode {
    pub fn new(config_mode: ConfigJDCMode) -> JDMode {
        JDMode {
            inner: Arc::new(AtomicU8::new(ConfigJDCMode::SoloMining as u8)),
            config_mode,
        }
    }

    /// This activates mode based on config file once
    /// upstream connection is made.
    pub fn activate(&self) {
        match self.config_mode {
            ConfigJDCMode::CoinbaseOnly => self.set_coinbase_only(),
            ConfigJDCMode::FullTemplate => self.set_full_template(),
            ConfigJDCMode::SoloMining => self.set_solo_mining(),
        }
    }

    pub fn set_solo_mining(&self) {
        self.inner
            .store(ConfigJDCMode::SoloMining as u8, Ordering::Relaxed);
    }

    fn set_full_template(&self) {
        self.inner
            .store(ConfigJDCMode::FullTemplate as u8, Ordering::Relaxed);
    }

    fn set_coinbase_only(&self) {
        self.inner
            .store(ConfigJDCMode::CoinbaseOnly as u8, Ordering::Relaxed);
    }

    pub fn is_solo_mining(&self) -> bool {
        let mode = self.inner.load(Ordering::Relaxed);
        mode == ConfigJDCMode::SoloMining as u8
    }

    pub fn is_full_template(&self) -> bool {
        let mode = self.inner.load(Ordering::Relaxed);
        mode == ConfigJDCMode::FullTemplate as u8
    }

    pub fn is_coinbase_only(&self) -> bool {
        let mode = self.inner.load(Ordering::Relaxed);
        mode == ConfigJDCMode::CoinbaseOnly as u8
    }

    pub fn is_config_full_template(&self) -> bool {
        self.config_mode == ConfigJDCMode::FullTemplate
    }

    pub fn is_config_coinbase_only(&self) -> bool {
        self.config_mode == ConfigJDCMode::CoinbaseOnly
    }
}
