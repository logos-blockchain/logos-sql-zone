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
use lb_core::mantle::ops::channel::{ChannelId, inscribe::Inscription};
use lb_key_management_system_service::keys::{ED25519_SECRET_KEY_SIZE, Ed25519Key};
use logos_blockchain_zone_sdk::{
    adapter::NodeHttpClient,
    sequencer::{Event, FinalizedOp, SequencerCheckpoint, SequencerClient, ZoneSequencer},
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
    #[error("Inscription too large: {0}")]
    InscriptionTooLarge(String),
}

pub type Result<T> = std::result::Result<T, SequencerError>;

pub struct Sequencer {
    sequencer: ZoneSequencer<NodeHttpClient>,
    client: SequencerClient,
    state: InMemoryZoneState,
    queue_file: String,
    checkpoint_path: String,
}

fn save_checkpoint(path: &Path, checkpoint: &SequencerCheckpoint) {
    let data = serde_json::to_vec(checkpoint).expect("failed to serialize checkpoint");
    fs::write(path, data).expect("failed to write checkpoint file");
}

fn load_checkpoint(path: &Path) -> Option<SequencerCheckpoint> {
    if !path.exists() {
        return None;
    }
    let data = fs::read(path).expect("failed to read checkpoint file");
    Some(serde_json::from_slice(&data).expect("failed to deserialize checkpoint"))
}

fn load_or_create_signing_key(path: &Path) -> Ed25519Key {
    if path.exists() {
        let key_bytes = fs::read(path).expect("failed to read key file");
        assert!(
            key_bytes.len() == ED25519_SECRET_KEY_SIZE,
            "invalid key file: expected {} bytes, got {}",
            ED25519_SECRET_KEY_SIZE,
            key_bytes.len()
        );
        let key_array: [u8; ED25519_SECRET_KEY_SIZE] =
            key_bytes.try_into().expect("length already checked");
        Ed25519Key::from_bytes(&key_array)
    } else {
        let mut key_bytes = [0u8; ED25519_SECRET_KEY_SIZE];
        rand::RngCore::fill_bytes(&mut rand::rng(), &mut key_bytes);
        fs::write(path, key_bytes).expect("failed to write key file");
        Ed25519Key::from_bytes(&key_bytes)
    }
}

impl Sequencer {
    pub async fn new(
        node_endpoint: &str,
        signing_key_path: &str,
        node_auth_username: Option<String>,
        node_auth_password: Option<String>,
        queue_file: &str,
        checkpoint_path: &str,
        channel_path: &str,
    ) -> Result<Self> {
        let node_url = Url::parse(node_endpoint).map_err(|e| SequencerError::Url(e.to_string()))?;

        let basic_auth = node_auth_username
            .map(|username| BasicAuthCredentials::new(username, node_auth_password));

        for path in [signing_key_path, checkpoint_path, channel_path] {
            if let Some(parent) = Path::new(path).parent() {
                fs::create_dir_all(parent)?;
            }
        }

        let checkpoint = load_checkpoint(Path::new(checkpoint_path));
        if checkpoint.is_some() {
            println!("  Restored checkpoint from {checkpoint_path}");
        }

        let signing_key = load_or_create_signing_key(Path::new(signing_key_path));
        let channel_id = ChannelId::from(signing_key.public_key().to_bytes());
        fs::write(channel_path, hex::encode(channel_id.as_ref()))
            .expect("failed to write channel id");

        let node = NodeHttpClient::new(CommonHttpClient::new(basic_auth), node_url);
        let sequencer = ZoneSequencer::init(channel_id, signing_key, node, checkpoint);
        let client = sequencer.client();

        Ok(Self {
            sequencer,
            client,
            state: InMemoryZoneState::default(),
            queue_file: queue_file.to_owned(),
            checkpoint_path: checkpoint_path.to_owned(),
        })
    }

    pub async fn run(self) {
        let Self { mut sequencer, client, mut state, queue_file, checkpoint_path } = self;

        let batch_client = client;
        tokio::spawn(async move {
            // Wait until the sequencer completes cold-start backfill before publishing.
            let mut ready_rx = batch_client.subscribe_ready();
            drop(ready_rx.wait_for(|r| *r).await);

            let mut interval = tokio::time::interval(Duration::from_millis(100));
            loop {
                interval.tick().await;
                if let Err(e) = process_pending_batch(&queue_file, &batch_client).await {
                    error!("Batch processing failed: {e}");
                }
            }
        });

        loop {
            let event = sequencer.next_event().await;
            handle_event(event, &mut state, &checkpoint_path);
        }
    }
}

fn handle_event(
    event: Event,
    state: &mut InMemoryZoneState,
    checkpoint_path: &str,
) {
    match event {
        Event::Ready => {
            info!("Sequencer ready");
        }
        Event::BlocksProcessed { checkpoint, channel_update: _, finalized } => {
            let inscriptions: Vec<_> = finalized
                .into_iter()
                .flat_map(|tx| tx.ops.into_iter())
                .filter_map(|op| match op {
                    FinalizedOp::Inscription(info) => Some(info),
                    _ => None,
                })
                .collect();
            state.on_finalized(&inscriptions);
            save_checkpoint(Path::new(checkpoint_path), &checkpoint);
        }
        Event::MempoolPending(_) | Event::TurnNotification { .. } => {}
    }
}

async fn process_pending_batch(queue_file: &str, client: &SequencerClient) -> Result<()> {
    let pending = queue_drain(queue_file)?;
    if pending.is_empty() {
        return Ok(());
    }

    let count = pending.len();
    debug!("Processing batch of {} queries", count);

    let sql_bytes = pending.join("\n").into_bytes();
    let inscription = Inscription::try_from(sql_bytes)
        .map_err(|e| SequencerError::InscriptionTooLarge(e.to_string()))?;
    if let Err(e) = client.publish(inscription).await {
        error!("failed to publish batch: {e}");
    } else {
        info!("Submitted batch of {} statement(s)", count);
    }

    Ok(())
}

fn queue_drain(queue_file: &str) -> Result<Vec<String>> {
    let file = match OpenOptions::new().read(true).write(true).open(queue_file) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(SequencerError::Io(e)),
    };

    file.lock_exclusive()?;

    let reader = BufReader::new(&file);
    let mut queue_vec = Vec::new();
    for query in reader.lines() {
        queue_vec.push(query?);
    }

    file.set_len(0)?;

    Ok(queue_vec)
}
