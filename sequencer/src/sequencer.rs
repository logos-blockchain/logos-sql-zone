use std::{
    fs,
    fs::OpenOptions,
    io,
    io::{BufRead as _, BufReader},
    path::Path,
    time::Duration,
};

use demo_sqlite_common::state::{InMemoryZoneState, ZoneState as _};
use fs2::FileExt as _;
use lb_common_http_client::{BasicAuthCredentials, CommonHttpClient};
use lb_core::mantle::ops::channel::ChannelId;
use lb_key_management_system_service::keys::{ED25519_SECRET_KEY_SIZE, Ed25519Key};
use logos_blockchain_zone_sdk::{
    adapter::NodeHttpClient,
    sequencer::{Event, SequencerCheckpoint, SequencerHandle, ZoneSequencer},
};
use reqwest::Url;
use thiserror::Error;
use tracing::{debug, error, info};

#[derive(Debug, Error)]
pub enum SequencerError {
    #[error("URL parse error: {0}")]
    Url(String),
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
}

pub type Result<T> = std::result::Result<T, SequencerError>;

// Your Code Here