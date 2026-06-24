use lb_zone_sdk::sequencer::InscriptionInfo;

use crate::message::Msg;

/// Trait for the sequencer/indexer's view of zone state.
///
/// The sequencer/indexer feeds SDK events into this trait; the trait owns persistence.
/// `InMemoryZoneState` is the demo implementation. A real sequencer would
/// implement it over a DB so `published`/`finalized` survive
/// restarts (the SDK's own checkpoint covers tx-level resume separately).
///
/// Three lists, each ordered by arrival:
/// - `published`: our submissions, in submit order, until they finalize or get
///   orphaned.
/// - `finalized`: all inscriptions below LIB, in canonical order — the SDK
///   delivers `TxsFinalized`/`FinalizedInscriptions` per block.
///
/// Replay-idempotent: `on_finalized` dedup by `msg_id`, so
/// resuming from a persisted state and re-receiving backfill is harmless.
pub trait ZoneState: Send {
    fn on_published(&mut self, info: &InscriptionInfo);
    fn on_finalized(&mut self, inscriptions: &[InscriptionInfo]);

    fn published(&self) -> &[Msg];
    fn finalized(&self) -> &[Msg];
}

/// In-memory implementation of [`ZoneState`].
#[derive(Default)]
pub struct InMemoryZoneState {
    published: Vec<Msg>,
    finalized: Vec<Msg>,
}

impl ZoneState for InMemoryZoneState {
    fn on_published(&mut self, info: &InscriptionInfo) {
        self.published
            .push(Msg::from_payload(info.this_msg, &info.payload));
    }

    fn on_finalized(&mut self, inscriptions: &[InscriptionInfo]) {
        for info in inscriptions {
            if let Some(i) = self
                .published
                .iter()
                .position(|m| m.msg_id == info.this_msg)
            {
                self.published.remove(i);
            }
            if !self.finalized.iter().any(|m| m.msg_id == info.this_msg) {
                self.finalized
                    .push(Msg::from_payload(info.this_msg, &info.payload));
            }
        }
    }

    fn published(&self) -> &[Msg] {
        &self.published
    }

    fn finalized(&self) -> &[Msg] {
        &self.finalized
    }
}
