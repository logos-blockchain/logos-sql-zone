use std::fs;

use futures::StreamExt as _;
use lb_common_http_client::{BasicAuthCredentials, CommonHttpClient};
use lb_core::mantle::ops::channel::ChannelId;
use logos_blockchain_zone_sdk::adapter::NodeHttpClient;
use logos_blockchain_zone_sdk::indexer::ZoneIndexer;
use reqwest::Url;
use tracing::{error, info};

use crate::{db::DatabaseReadOnly, error::Error};

pub type Result<T> = std::result::Result<T, Error>;

// Your Code Here