//! Generation-and-lease lifetime machine for the extension runtime set.
//!
//! # Shape (WHY)
//!
//! `GenerationMachine` is a stateless namespace: every piece of generation
//! state already lives inside `Arc<Generation>` (endpoints, bridge handles, the
//! lease counter, the drain notify), and that arc is owned and swapped by
//! `PublishedRuntimeState` in the parent module. Holding a `GenerationMachine`
//! field on the set would duplicate ownership of state it does not control, so
//! the machine is realized as a unit struct carrying only the choreography
//! (`retire_reap`) plus the lifetime primitives moved onto `Generation` itself
//! (`drain_leases`, `stop_generation`, `abort_bridges`). Callers hand the
//! machine a generation they already hold; it never stores one.
//!
//! The generation `u64` token and the `EndpointId` comparison
//! (`id.generation != self.id` inside `Generation::endpoint`) live only here.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use futures::stream::{FuturesUnordered, StreamExt};

use super::EndpointKind;
use crate::core::extension_host::HostExtensionRunner;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct EndpointId {
    pub(super) generation: u64,
    pub(super) position: usize,
}

#[derive(Clone)]
pub(crate) struct Endpoint {
    pub(super) id: EndpointId,
    pub(super) kind: EndpointKind,
    pub(super) label: String,
    pub(super) runner: Arc<HostExtensionRunner>,
}

pub(crate) struct Generation {
    pub(super) id: u64,
    pub(super) endpoints: Arc<[Endpoint]>,
    bridges: StdMutex<Vec<tokio::task::JoinHandle<()>>>,
    leases: AtomicUsize,
    drained: tokio::sync::Notify,
}

impl Generation {
    /// Assemble a generation with no leases, no bridges, and a fresh drain notify.
    pub(super) fn new(id: u64, endpoints: Arc<[Endpoint]>) -> Self {
        Self {
            id,
            endpoints,
            bridges: StdMutex::new(Vec::new()),
            leases: AtomicUsize::new(0),
            drained: tokio::sync::Notify::new(),
        }
    }

    /// Increment the outstanding-lease counter. Paired with `GenerationLease::drop`.
    pub(super) fn count_lease(&self) {
        self.leases.fetch_add(1, Ordering::Relaxed);
    }

    /// Current outstanding-lease count (test/diagnostic probe).
    #[cfg(test)]
    pub(super) fn lease_count(&self) -> usize {
        self.leases.load(Ordering::Acquire)
    }

    pub(super) fn endpoint(&self, id: EndpointId) -> Option<&Endpoint> {
        if id.generation != self.id {
            return None;
        }
        self.endpoints.get(id.position)
    }

    pub(super) fn has_one_active_compat_endpoint(&self) -> bool {
        let mut active = self
            .endpoints
            .iter()
            .filter(|endpoint| endpoint.runner.is_active());
        matches!(active.next(), Some(endpoint) if endpoint.kind == EndpointKind::TsCompat && active.next().is_none())
    }

    /// Abort every bridge relay task owned by this generation.
    ///
    /// The bridge handles are taken (not borrowed) so a poisoned mutex never
    /// strands a still-running relay: the lock is recovered in place and the
    /// tasks are aborted regardless.
    pub(super) fn abort_bridges(&self) {
        let handles = std::mem::take(
            &mut *self
                .bridges
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for handle in handles {
            handle.abort();
        }
    }

    /// Wait until every outstanding `GenerationLease` has been dropped.
    pub(super) async fn drain_leases(&self) {
        while self.leases.load(Ordering::Acquire) != 0 {
            self.drained.notified().await;
        }
    }

    /// Shut every endpoint down exactly once, concurrently.
    pub(super) async fn stop_generation(&self) {
        let mut stops = self
            .endpoints
            .iter()
            .map(|endpoint| endpoint.runner.shutdown_once())
            .collect::<FuturesUnordered<_>>();
        while stops.next().await.is_some() {}
    }

    /// Record the bridge relay tasks spawned for this generation.
    pub(super) fn push_bridges(&self, handles: Vec<tokio::task::JoinHandle<()>>) {
        self.bridges
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(handles);
    }
}

pub(super) struct GenerationLease {
    pub(super) generation: Arc<Generation>,
    pub(super) counted: bool,
}

impl GenerationLease {
    pub(super) fn endpoints(&self) -> &[Endpoint] {
        if self.counted {
            &self.generation.endpoints
        } else {
            &[]
        }
    }

    pub(super) fn live_endpoints(&self) -> impl DoubleEndedIterator<Item = &Endpoint> {
        self.endpoints()
            .iter()
            .filter(|endpoint| endpoint.runner.is_active())
    }

    pub(super) fn is_active(&self) -> bool {
        self.endpoints()
            .iter()
            .any(|endpoint| endpoint.runner.is_active())
    }

    pub(super) fn is_running(&self) -> bool {
        self.endpoints()
            .iter()
            .any(|endpoint| endpoint.runner.is_running())
    }
}

impl Drop for GenerationLease {
    fn drop(&mut self) {
        if self.counted && self.generation.leases.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.generation.drained.notify_one();
        }
    }
}

/// Choreography owner for the retire-reap sequence shared by cutover and
/// commit-reload. The order is load-bearing: drain in-flight leases so no
/// relay publishes through a dying endpoint, invalidate every runner so
/// relays stop sourcing events, abort bridge tasks, then stop endpoints.
pub(super) struct GenerationMachine;

impl GenerationMachine {
    pub(super) async fn retire_reap(old: &Arc<Generation>) {
        old.drain_leases().await;
        for endpoint in old.endpoints.iter() {
            endpoint.runner.invalidate();
        }
        old.abort_bridges();
        old.stop_generation().await;
    }
}
