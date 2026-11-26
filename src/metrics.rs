use std::sync::{
    Mutex,
    atomic::{AtomicI64, AtomicU8, AtomicU64, Ordering},
};

use crate::state::now;

#[derive(Default)]
pub struct RuntimeStatus {
    last_event_at: AtomicI64,
    backend_health: AtomicU8,
    last_health_error: Mutex<Option<String>>,
    event_reconnects: AtomicU64,
    resyncs: AtomicU64,
    notification_retries: AtomicU64,
    notification_drops: AtomicU64,
}

impl RuntimeStatus {
    pub fn mark_event(&self) {
        self.last_event_at.store(now(), Ordering::Relaxed);
    }

    pub fn last_event_at(&self) -> Option<i64> {
        let value = self.last_event_at.load(Ordering::Relaxed);
        (value > 0).then_some(value)
    }

    pub fn set_health(&self, healthy: bool, error: Option<&str>) {
        self.backend_health
            .store(if healthy { 1 } else { 2 }, Ordering::Relaxed);
        if let Ok(mut last_error) = self.last_health_error.lock() {
            *last_error = error.map(ToString::to_string);
        }
    }

    pub fn backend_health(&self) -> Option<bool> {
        match self.backend_health.load(Ordering::Relaxed) {
            1 => Some(true),
            2 => Some(false),
            _ => None,
        }
    }

    pub fn last_health_error(&self) -> Option<String> {
        self.last_health_error
            .lock()
            .ok()
            .and_then(|error| error.clone())
    }

    pub fn inc_event_reconnect(&self) {
        self.event_reconnects.fetch_add(1, Ordering::Relaxed);
    }

    pub fn event_reconnects(&self) -> u64 {
        self.event_reconnects.load(Ordering::Relaxed)
    }

    pub fn inc_resync(&self) {
        self.resyncs.fetch_add(1, Ordering::Relaxed);
    }

    pub fn resyncs(&self) -> u64 {
        self.resyncs.load(Ordering::Relaxed)
    }

    pub fn inc_notification_retry(&self) {
        self.notification_retries.fetch_add(1, Ordering::Relaxed);
    }

    pub fn notification_retries(&self) -> u64 {
        self.notification_retries.load(Ordering::Relaxed)
    }

    pub fn inc_notification_drop(&self) {
        self.notification_drops.fetch_add(1, Ordering::Relaxed);
    }

    pub fn notification_drops(&self) -> u64 {
        self.notification_drops.load(Ordering::Relaxed)
    }
}
