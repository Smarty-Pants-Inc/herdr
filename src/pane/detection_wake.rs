//! Coalesced parsed-output wakes and one cheap safety clock shared by all panes.
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use tokio::sync::{watch, Notify};

const FALLBACK_INTERVAL: Duration = Duration::from_millis(300);
// Reuse the 300 ms identified-agent latency budget: sustained PTY output must
// not force per-pane detection snapshots at 20 Hz. The shared clock and
// state-specific deadlines can still preempt this first-event window.
const OUTPUT_COALESCE: Duration = Duration::from_millis(300);

type Producers = Mutex<HashMap<tokio::runtime::Id, Weak<watch::Sender<()>>>>;

fn fallback_receiver() -> watch::Receiver<()> {
    static PRODUCERS: OnceLock<Producers> = OnceLock::new();
    subscribe_fallback(PRODUCERS.get_or_init(|| Mutex::new(HashMap::new())))
}

fn subscribe_fallback(producers: &Producers) -> watch::Receiver<()> {
    let id = tokio::runtime::Handle::current().id();
    let mut producers = producers
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    producers.retain(|_, producer| producer.strong_count() > 0);
    if let Some(sender) = producers.get(&id).and_then(Weak::upgrade) {
        return sender.subscribe();
    }
    let (sender, receiver) = watch::channel(());
    let sender = Arc::new(sender);
    producers.insert(id, Arc::downgrade(&sender));
    // Only the producer task strong-owns the sender. Runtime cancellation closes
    // receivers and expires the static Weak, allowing another runtime to recover.
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(FALLBACK_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            sender.send_replace(());
            if sender.receiver_count() == 0 {
                break;
            }
        }
    });
    receiver
}

pub(super) struct DetectionWake {
    fallback: watch::Receiver<()>,
    output: Arc<Notify>,
    output_deadline: Option<Instant>,
}

async fn wait_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
        None => std::future::pending().await,
    }
}

impl DetectionWake {
    pub(super) fn new(output: Arc<Notify>) -> Self {
        Self {
            fallback: fallback_receiver(),
            output,
            output_deadline: None,
        }
    }

    /// True means reset; all other wakes preserve detector-local state. The
    /// coalescing window is bounded from the first byte, never trailing-edge.
    pub(super) async fn wait(&mut self, reset: &Notify, deadline: Option<Instant>) -> bool {
        loop {
            tokio::select! {
                biased;
                _ = reset.notified() => {
                    self.output_deadline = None;
                    return true;
                }
                _ = wait_until(deadline) => {
                    self.output_deadline = None;
                    return false;
                }
                tick = self.fallback.changed() => {
                    if tick.is_err() {
                        self.fallback = fallback_receiver();
                    }
                    self.output_deadline = None;
                    return false;
                }
                _ = wait_until(self.output_deadline) => {
                    self.output_deadline = None;
                    return false;
                }
                _ = self.output.notified(), if self.output_deadline.is_none() => {
                    self.output_deadline = Some(Instant::now() + OUTPUT_COALESCE);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_reconnects_after_producer_runtime_cancellation() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        // Isolate ownership from other tests' live runtimes/receivers.
        let producer = Mutex::new(HashMap::new());
        let mut receiver = runtime.block_on(async { subscribe_fallback(&producer) });
        drop(runtime);
        assert!(receiver.has_changed().is_err());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            receiver = subscribe_fallback(&producer);
            tokio::time::timeout(Duration::from_secs(1), receiver.changed())
                .await
                .expect("new runtime must produce ticks")
                .expect("new producer must remain open");
        });
    }

    #[test]
    fn dormant_runtime_cannot_own_another_runtimes_fallback() {
        let producer = Mutex::new(HashMap::new());
        let first = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let _first_receiver = first.block_on(async { subscribe_fallback(&producer) });
        // Keep the first runtime alive but stop driving it.
        let second = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        second.block_on(async {
            let mut receiver = subscribe_fallback(&producer);
            let other_pane = subscribe_fallback(&producer);
            assert!(
                receiver.same_channel(&other_pane),
                "one clock for all panes on this runtime"
            );
            tokio::time::timeout(Duration::from_secs(1), receiver.changed())
                .await
                .expect("dormant first runtime must not stall this runtime")
                .unwrap();
        });
    }
}
