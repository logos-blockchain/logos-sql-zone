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

pub struct Sequencer {
    sequencer: ZoneSequencer<NodeHttpClient>,
    handle: SequencerHandle<NodeHttpClient>,
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
        let (sequencer, handle) = ZoneSequencer::init(channel_id, signing_key, node, checkpoint);

        Ok(Self {
            sequencer,
            handle,
            state: InMemoryZoneState::default(),
            queue_file: queue_file.to_owned(),
            checkpoint_path: checkpoint_path.to_owned(),
        })
    }

    pub async fn run(self) {
        let Self { mut sequencer, handle, mut state, queue_file, checkpoint_path } = self;

        let mut batch_handle = handle.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(100));
            loop {
                interval.tick().await;
                batch_handle.wait_ready().await;
                if let Err(e) = process_pending_batch(&queue_file, &batch_handle).await {
                    error!("Batch processing failed: {e}");
                }
            }
        });

        loop {
            let Some(event) = sequencer.next_event().await else { continue; };
            handle_event(event, &handle, &mut state, &checkpoint_path).await;
        }
    }
}

async fn handle_event(
    event: Event,
    handle: &SequencerHandle<NodeHttpClient>,
    state: &mut InMemoryZoneState,
    checkpoint_path: &str,
) {
    match event {
        Event::Ready => {
            info!("Sequencer ready");
        }
        Event::ChannelUpdate { orphaned, adopted } => {
            state.on_adopted(&adopted);
            for info in &orphaned {
                state.on_orphaned(&info.this_msg);
                debug!(msg_id = %hex::encode(info.this_msg.as_ref()), "Auto-republishing orphan");
                if let Err(e) = handle.publish_message(info.payload.clone()).await {
                    error!("failed to auto-republish: {e}");
                }
            }
        }
        Event::TxsFinalized { inscriptions, .. } => {
            state.on_finalized(&inscriptions);
        }
        Event::Published { info, checkpoint } => {
            debug!(msg_id = %hex::encode(info.this_msg.as_ref()), "Published");
            state.on_published(&info);
            save_checkpoint(Path::new(checkpoint_path), &checkpoint);
            state.save_checkpoint(checkpoint);
        }
        Event::FinalizedInscriptions { inscriptions } => {
            state.on_finalized(&inscriptions);
        }
    }
}

async fn process_pending_batch(
    queue_file: &str,
    handle: &SequencerHandle<NodeHttpClient>,
) -> Result<()> {
    let pending = queue_drain(queue_file)?;
    if pending.is_empty() {
        return Ok(());
    }

    let count = pending.len();
    debug!("Processing batch of {} queries", count);

    let sql_text = pending.join("\n").as_bytes().to_vec();
    if let Err(e) = handle.publish_message(sql_text).await {
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
