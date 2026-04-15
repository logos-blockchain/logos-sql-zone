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

pub struct Indexer {
    zone_indexer: ZoneIndexer<NodeHttpClient>,
    db_path: String,
}

fn parse_channel_id(channel_id_str: &str) -> Result<ChannelId> {
    let decoded = hex::decode(channel_id_str).map_err(|_| {
        Error::InvalidChannelId(format!(
            "INDEXER_CHANNEL_ID must be a valid hex string, got: '{channel_id_str}'"
        ))
    })?;
    let channel_bytes: [u8; 32] = decoded.try_into().map_err(|v: Vec<u8>| {
        Error::InvalidChannelId(format!(
            "INDEXER_CHANNEL_ID must be exactly 64 hex characters (32 bytes), got {} characters ({} bytes)",
            v.len() * 2,
            v.len()
        ))
    })?;
    Ok(ChannelId::from(channel_bytes))
}

impl Indexer {
    pub fn new(
        db_path: &str,
        node_endpoint: &str,
        channel_path: &str,
        node_auth_username: Option<String>,
        node_auth_password: Option<String>,
    ) -> Result<Self> {
        let node_url = Url::parse(node_endpoint).map_err(|e| Error::Url(e.to_string()))?;

        let basic_auth = node_auth_username
            .map(|username| BasicAuthCredentials::new(username, node_auth_password));

        let channel_id_str = fs::read_to_string(channel_path).map_err(|e| {
            Error::InvalidChannelId(format!("Failed to read channel path '{channel_path}': {e}"))
        })?;
        let channel_id = parse_channel_id(channel_id_str.trim())?;

        info!("Channel ID: {}", hex::encode(channel_id.as_ref()));

        let node = NodeHttpClient::new(CommonHttpClient::new(basic_auth), node_url);
        let zone_indexer = ZoneIndexer::new(channel_id, node);

        Ok(Self { zone_indexer, db_path: db_path.to_owned() })
    }

    pub async fn run(&self) {
        let db = match DatabaseReadOnly::open(&self.db_path) {
            Ok(db) => db,
            Err(e) => {
                error!("Failed to open database: {e}");
                return;
            }
        };

        loop {
            info!("Connecting to zone block stream...");
            let stream = match self.zone_indexer.follow().await {
                Ok(s) => s,
                Err(e) => {
                    error!("Failed to connect to block stream: {e}");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }
            };
            info!("Connected to zone block stream");

            futures::pin_mut!(stream);
            while let Some(zone_msg) = stream.next().await {
                let logos_blockchain_zone_sdk::ZoneMessage::Block(zone_block) = zone_msg else {
                    continue;
                };
                let sql_text = match String::from_utf8(zone_block.data) {
                    Ok(s) => s,
                    Err(e) => {
                        error!("Zone block data is not valid UTF-8: {e}");
                        continue;
                    }
                };

                let statements: Vec<&str> = sql_text
                    .lines()
                    .map(|l| l.trim().trim_end_matches(';').trim())
                    .filter(|s| !s.is_empty())
                    .collect();

                if statements.is_empty() {
                    continue;
                }

                info!("Applying {} SQL statement(s)", statements.len());

                for stmt in &statements {
                    if let Err(e) = db.execute_batch(stmt) {
                        error!("Failed to execute SQL '{}': {e}", stmt);
                    }
                }
                info!("Applied {} statement(s)", statements.len());
            }

            error!("Zone block stream ended, reconnecting...");
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    }
}
