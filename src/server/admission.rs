//! Non-waiting admission for daemon searches. A permit covers execution and
//! response delivery, so a slow reader cannot build an unlimited result queue.
use std::sync::{
    OnceLock,
    atomic::{AtomicUsize, Ordering},
};

use super::protocol::{Request, Response};

pub(crate) struct Admission {
    limit: usize,
    active: AtomicUsize,
}
impl Admission {
    pub(crate) fn new(limit: usize) -> Self {
        assert!(limit > 0);
        Self {
            limit,
            active: AtomicUsize::new(0),
        }
    }

    pub(crate) fn try_acquire(&self) -> Option<Permit<'_>> {
        self.active
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |active| {
                (active < self.limit).then_some(active + 1)
            })
            .ok()
            .map(|_| Permit { admission: self })
    }
}

pub(crate) struct Permit<'a> {
    admission: &'a Admission,
}
impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.admission.active.fetch_sub(1, Ordering::Release);
    }
}

pub(crate) fn searches() -> &'static Admission {
    static SEARCHES: OnceLock<Admission> = OnceLock::new();
    SEARCHES.get_or_init(|| {
        let default = std::thread::available_parallelism()
            .map_or(1, usize::from)
            .min(8);
        Admission::new(configured_limit(
            std::env::var("FXI_MAX_ACTIVE_SEARCHES").ok().as_deref(),
            default,
        ))
    })
}

fn configured_limit(value: Option<&str>, default: usize) -> usize {
    value
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(default)
        .clamp(1, 256)
}

pub(crate) fn is_search(request: &Request) -> bool {
    matches!(
        request,
        Request::Search { .. } | Request::ContentSearch { .. }
    )
}

pub(crate) fn overloaded() -> Response {
    Response::Error {
        message: format!(
            "{}Daemon search capacity exhausted; retry after an active search finishes (FXI_MAX_ACTIVE_SEARCHES).",
            super::protocol::SEARCH_OVERLOADED_PREFIX
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[test]
    fn concurrent_saturation_and_recovery_without_waiters() {
        let admission = Admission::new(3);
        let entered = Arc::new(Barrier::new(4));
        let release = Arc::new(Barrier::new(4));
        std::thread::scope(|scope| {
            for _ in 0..3 {
                let entered = entered.clone();
                let release = release.clone();
                let admission = &admission;
                scope.spawn(move || {
                    let _permit = admission.try_acquire().unwrap();
                    entered.wait();
                    release.wait();
                });
            }
            entered.wait();
            assert!(admission.try_acquire().is_none());
            release.wait();
        });
        let permits: Vec<_> = (0..3).map(|_| admission.try_acquire().unwrap()).collect();
        assert!(admission.try_acquire().is_none());
        drop(permits);
        assert!(admission.try_acquire().is_some());
    }

    #[test]
    fn unwind_returns_capacity_and_configuration_is_bounded() {
        let admission = Admission::new(1);
        let _ = std::panic::catch_unwind(|| {
            let _permit = admission.try_acquire().unwrap();
            panic!("handler failed");
        });
        assert!(admission.try_acquire().is_some());
        for (value, expected) in [
            (None, 8),
            (Some("bad"), 8),
            (Some("0"), 1),
            (Some("2"), 2),
            (Some("9999"), 256),
        ] {
            assert_eq!(configured_limit(value, 8), expected);
        }
        for control in [
            Request::Ping,
            Request::Status,
            Request::Shutdown,
            Request::Hello {
                protocol_version: 1,
            },
            Request::WatchStatus { root_path: None },
        ] {
            assert!(!is_search(&control));
        }
    }
}
