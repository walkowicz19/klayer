//! Stage F: Notification Relay (Remote-Compatible, Trigger-Scoped).
//!
//! A generic webhook relay for four triggers: knowledge conflicts, Proposed
//! items aging past a threshold, Turso→SQLite fallback events, and
//! domain-permission denial spikes. Deliberately provider-agnostic: the
//! outbound payload (`RelayEvent`) is a flat JSON body meant to work as-is
//! against ntfy.sh, Pushover, Slack incoming webhooks, or any other
//! bring-your-own HTTP endpoint, rather than being shaped to one provider's
//! specific schema.
//!
//! Entirely optional: unless `KLAYER_NOTIFY_WEBHOOK_URL` is set, `NotifyState`
//! is fully inert — no channel, no background task, no HTTP calls.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use tokio::sync::mpsc;

pub const DEFAULT_PROPOSED_AGE_THRESHOLD_SECS: i64 = 7 * 24 * 3600;
pub const DEFAULT_DENIAL_SPIKE_THRESHOLD: u32 = 5;

/// Events within the same trigger are batched over this window before a
/// relay message is sent, so a burst (e.g. many conflicts from one ingest)
/// produces one message rather than one per item.
const BATCH_WINDOW_SECS: u64 = 30;
/// Sliding window for the denial-spike trigger.
const DENIAL_WINDOW_SECS: i64 = 60;
/// Once a spike fires for a domain, suppress repeat fires for this long to
/// avoid paging on every subsequent denial while the caller is still misusing
/// the same domain.
const DENIAL_COOLDOWN_SECS: i64 = 300;

/// Deliberately generic (see module docs): every field is a plain scalar so
/// this serializes to a body any webhook receiver can consume without
/// klayer-specific parsing.
#[derive(Debug, Clone, Serialize)]
pub struct RelayEvent {
    pub trigger: String,
    pub summary: String,
    pub detail: String,
    pub count: u32,
    pub ts: i64,
}

#[derive(Debug, Clone)]
pub struct NotifyConfig {
    pub webhook_url: String,
    pub proposed_age_threshold_secs: i64,
    pub denial_spike_threshold: u32,
}

impl NotifyConfig {
    pub fn from_env() -> Option<Self> {
        Self::from_values(
            std::env::var("KLAYER_NOTIFY_WEBHOOK_URL").ok(),
            std::env::var("KLAYER_PROPOSED_AGE_THRESHOLD_SECS").ok(),
            std::env::var("KLAYER_DENIAL_SPIKE_THRESHOLD").ok(),
        )
    }

    fn from_values(
        url: Option<String>,
        age: Option<String>,
        spike: Option<String>,
    ) -> Option<Self> {
        let webhook_url = url?.trim().to_string();
        if webhook_url.is_empty() {
            return None;
        }
        let proposed_age_threshold_secs = age
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_PROPOSED_AGE_THRESHOLD_SECS);
        let denial_spike_threshold = spike
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_DENIAL_SPIKE_THRESHOLD);
        Some(Self {
            webhook_url,
            proposed_age_threshold_secs,
            denial_spike_threshold,
        })
    }
}

/// POSTs a single `RelayEvent` as JSON to `url`. Not called directly for
/// individual triggers — the batcher task calls this once per flushed group.
pub async fn send_relay(url: &str, event: &RelayEvent) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .user_agent("klayer-notify/0.1")
        .build()?;
    client
        .post(url)
        .json(event)
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

/// Merges same-trigger events collected within a batch window into one
/// relay message.
pub fn merge_batch(trigger: &str, events: &[RelayEvent]) -> RelayEvent {
    let count: u32 = events.iter().map(|e| e.count).sum();
    let ts = events.iter().map(|e| e.ts).max().unwrap_or(0);
    let examples: Vec<&str> = events.iter().take(3).map(|e| e.summary.as_str()).collect();
    RelayEvent {
        trigger: trigger.to_string(),
        summary: format!("{count} {trigger} event(s)"),
        detail: examples.join("; "),
        count,
        ts,
    }
}

async fn run_batcher(url: String, mut rx: mpsc::UnboundedReceiver<RelayEvent>) {
    let mut buf: HashMap<String, Vec<RelayEvent>> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_secs(BATCH_WINDOW_SECS));
    tick.tick().await;
    loop {
        tokio::select! {
            maybe = rx.recv() => match maybe {
                Some(ev) => buf.entry(ev.trigger.clone()).or_default().push(ev),
                None => break,
            },
            _ = tick.tick() => {
                for (trigger, events) in buf.drain() {
                    let merged = merge_batch(&trigger, &events);
                    if let Err(e) = send_relay(&url, &merged).await {
                        tracing::warn!("notify relay failed: {e:#}");
                    }
                }
            }
        }
    }
}

/// Sending half of the relay pipeline. `None` sender means the relay is
/// disabled — `emit` becomes a pure no-op (no allocation of note, no task).
#[derive(Clone)]
pub struct NotifyHandle {
    tx: Option<mpsc::UnboundedSender<RelayEvent>>,
}

impl NotifyHandle {
    pub fn disabled() -> Self {
        Self { tx: None }
    }

    pub fn spawn(config: &NotifyConfig) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(run_batcher(config.webhook_url.clone(), rx));
        Self { tx: Some(tx) }
    }

    pub fn is_enabled(&self) -> bool {
        self.tx.is_some()
    }

    pub fn emit(&self, event: RelayEvent) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(event);
        }
    }
}

/// Tracks which `Proposed`-tier knowledge ids have already triggered an aging
/// notification. In-memory only (advisory feature, not a durability
/// guarantee) so a restart re-arms every still-Proposed item — acceptable
/// since the relay is a convenience nudge, not a compliance record.
#[derive(Default)]
pub struct AgingTracker {
    notified: HashSet<i64>,
}

impl AgingTracker {
    pub fn should_notify(
        &mut self,
        id: i64,
        created_at: i64,
        now: i64,
        threshold_secs: i64,
    ) -> bool {
        if now - created_at < threshold_secs {
            return false;
        }
        self.notified.insert(id)
    }
}

/// Delta-detects increases in a per-store `fallback_events` counter across
/// periodic polls, since Stage A's counter is cumulative and there's no
/// event hook to tap into without touching already-tested sync code.
#[derive(Default)]
pub struct FallbackTracker {
    last: HashMap<&'static str, u64>,
}

impl FallbackTracker {
    /// Returns `Some(delta)` if `current` is higher than the last observed
    /// value for `store_name`. The very first observation establishes a
    /// baseline and never fires.
    pub fn delta(&mut self, store_name: &'static str, current: u64) -> Option<u64> {
        let prev = self.last.insert(store_name, current).unwrap_or(current);
        if current > prev {
            Some(current - prev)
        } else {
            None
        }
    }
}

/// Sliding-window denial counter per domain, with a cooldown so a spike only
/// fires once per burst rather than once per denial after crossing the
/// threshold.
pub struct DenialTracker {
    threshold: u32,
    events: HashMap<String, Vec<i64>>,
    last_fired: HashMap<String, i64>,
}

impl DenialTracker {
    pub fn new(threshold: u32) -> Self {
        Self {
            threshold,
            events: HashMap::new(),
            last_fired: HashMap::new(),
        }
    }

    /// Records a denial for `domain` at time `now` and returns `true` if this
    /// denial newly crosses the spike threshold (and the cooldown has
    /// elapsed since the last time it fired for this domain).
    pub fn record(&mut self, domain: &str, now: i64) -> bool {
        let entry = self.events.entry(domain.to_string()).or_default();
        entry.push(now);
        entry.retain(|&t| now - t <= DENIAL_WINDOW_SECS);
        if entry.len() as u32 >= self.threshold {
            let last = self.last_fired.get(domain).copied().unwrap_or(i64::MIN / 2);
            if now - last >= DENIAL_COOLDOWN_SECS {
                self.last_fired.insert(domain.to_string(), now);
                return true;
            }
        }
        false
    }
}

/// Shared relay state threaded into `Klayer` (and the periodic watch task in
/// `main`). Cheap to hold even when disabled.
pub struct NotifyState {
    pub handle: NotifyHandle,
    pub proposed_age_threshold_secs: i64,
    denials: Mutex<DenialTracker>,
}

impl NotifyState {
    pub fn disabled() -> Self {
        Self {
            handle: NotifyHandle::disabled(),
            proposed_age_threshold_secs: DEFAULT_PROPOSED_AGE_THRESHOLD_SECS,
            denials: Mutex::new(DenialTracker::new(DEFAULT_DENIAL_SPIKE_THRESHOLD)),
        }
    }

    pub fn from_config(config: &NotifyConfig) -> Self {
        Self {
            handle: NotifyHandle::spawn(config),
            proposed_age_threshold_secs: config.proposed_age_threshold_secs,
            denials: Mutex::new(DenialTracker::new(config.denial_spike_threshold)),
        }
    }

    pub fn record_denial(&self, domain: &str) {
        if !self.handle.is_enabled() {
            return;
        }
        let now = chrono::Utc::now().timestamp();
        let fired = self.denials.lock().unwrap().record(domain, now);
        if fired {
            self.handle.emit(RelayEvent {
                trigger: "domain_denial_spike".to_string(),
                summary: format!("Denial spike in domain '{domain}'"),
                detail: format!(
                    "Repeated access denials for domain '{domain}' within {DENIAL_WINDOW_SECS}s"
                ),
                count: 1,
                ts: now,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_from_env_is_none_when_url_unset() {
        assert!(NotifyConfig::from_values(None, None, None).is_none());
        assert!(NotifyConfig::from_values(Some("  ".to_string()), None, None).is_none());
    }

    #[test]
    fn config_from_env_uses_defaults_when_unset() {
        let cfg =
            NotifyConfig::from_values(Some("https://example.com/hook".to_string()), None, None)
                .unwrap();
        assert_eq!(cfg.webhook_url, "https://example.com/hook");
        assert_eq!(
            cfg.proposed_age_threshold_secs,
            DEFAULT_PROPOSED_AGE_THRESHOLD_SECS
        );
        assert_eq!(cfg.denial_spike_threshold, DEFAULT_DENIAL_SPIKE_THRESHOLD);
    }

    #[test]
    fn config_from_env_parses_overrides() {
        let cfg = NotifyConfig::from_values(
            Some("https://example.com/hook".to_string()),
            Some("3600".to_string()),
            Some("10".to_string()),
        )
        .unwrap();
        assert_eq!(cfg.proposed_age_threshold_secs, 3600);
        assert_eq!(cfg.denial_spike_threshold, 10);
    }

    #[test]
    fn disabled_handle_emit_is_a_silent_noop() {
        let handle = NotifyHandle::disabled();
        assert!(!handle.is_enabled());
        handle.emit(RelayEvent {
            trigger: "x".into(),
            summary: "x".into(),
            detail: "x".into(),
            count: 1,
            ts: 0,
        });
    }

    #[test]
    fn disabled_notify_state_record_denial_never_fires_or_panics() {
        let state = NotifyState::disabled();
        for _ in 0..50 {
            state.record_denial("some-domain");
        }
    }

    #[test]
    fn aging_tracker_fires_once_past_threshold() {
        let mut tracker = AgingTracker::default();
        let created_at = 1_000;
        let threshold = 100;
        assert!(!tracker.should_notify(1, created_at, created_at + 50, threshold));
        assert!(tracker.should_notify(1, created_at, created_at + 150, threshold));
        assert!(!tracker.should_notify(1, created_at, created_at + 200, threshold));
    }

    #[test]
    fn aging_tracker_tracks_ids_independently() {
        let mut tracker = AgingTracker::default();
        assert!(tracker.should_notify(1, 0, 1000, 100));
        assert!(tracker.should_notify(2, 0, 1000, 100));
    }

    #[test]
    fn fallback_tracker_first_observation_never_fires() {
        let mut tracker = FallbackTracker::default();
        assert_eq!(tracker.delta("kl-code", 5), None);
    }

    #[test]
    fn fallback_tracker_detects_increase() {
        let mut tracker = FallbackTracker::default();
        tracker.delta("kl-code", 5);
        assert_eq!(tracker.delta("kl-code", 8), Some(3));
        assert_eq!(tracker.delta("kl-code", 8), None);
    }

    #[test]
    fn fallback_tracker_is_per_store() {
        let mut tracker = FallbackTracker::default();
        tracker.delta("kl-code", 5);
        tracker.delta("kl-train", 1);
        assert_eq!(tracker.delta("kl-code", 9), Some(4));
        assert_eq!(tracker.delta("kl-train", 3), Some(2));
    }

    #[test]
    fn denial_tracker_fires_once_threshold_crossed() {
        let mut tracker = DenialTracker::new(3);
        assert!(!tracker.record("d", 0));
        assert!(!tracker.record("d", 1));
        assert!(tracker.record("d", 2));
        assert!(
            !tracker.record("d", 3),
            "cooldown should suppress repeat fire"
        );
    }

    #[test]
    fn denial_tracker_expires_old_events_outside_window() {
        let mut tracker = DenialTracker::new(3);
        tracker.record("d", 0);
        tracker.record("d", 1);
        assert!(
            !tracker.record("d", 1000),
            "old denials should have expired out of the window"
        );
    }

    #[test]
    fn denial_tracker_refires_after_cooldown() {
        let mut tracker = DenialTracker::new(2);
        assert!(!tracker.record("d", 0));
        assert!(tracker.record("d", 1));
        let later = 1 + DENIAL_COOLDOWN_SECS;
        assert!(
            !tracker.record("d", later),
            "needs a fresh window to re-cross the threshold"
        );
        assert!(tracker.record("d", later + 1));
    }

    #[test]
    fn denial_tracker_is_per_domain() {
        let mut tracker = DenialTracker::new(2);
        assert!(!tracker.record("a", 0));
        assert!(!tracker.record("b", 0));
    }

    #[test]
    fn merge_batch_sums_counts_and_takes_latest_ts() {
        let events = vec![
            RelayEvent {
                trigger: "knowledge_conflict".into(),
                summary: "conflict #1".into(),
                detail: "".into(),
                count: 1,
                ts: 100,
            },
            RelayEvent {
                trigger: "knowledge_conflict".into(),
                summary: "conflict #2".into(),
                detail: "".into(),
                count: 1,
                ts: 200,
            },
        ];
        let merged = merge_batch("knowledge_conflict", &events);
        assert_eq!(merged.count, 2);
        assert_eq!(merged.ts, 200);
        assert!(merged.detail.contains("conflict #1"));
        assert!(merged.detail.contains("conflict #2"));
    }
}
