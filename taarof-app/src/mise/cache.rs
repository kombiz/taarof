//! Task discovery cache and background worker.

use super::*;

pub(super) fn task_discovery_cache() -> &'static Mutex<TaskDiscoveryCache> {
    static CACHE: OnceLock<Mutex<TaskDiscoveryCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(TaskDiscoveryCache::default()))
}

pub(crate) fn cached_task_discovery(target: &DiscoveryTarget) -> CachedTaskDiscovery {
    let key = DiscoveryCacheKey::from(target);
    let now = Instant::now();
    let mut cache = task_discovery_cache()
        .lock()
        .expect("task discovery cache lock should not be poisoned");

    match cache.entries.get(&key) {
        Some(TaskDiscoveryCacheEntry::Pending { started_at, .. })
            if now.duration_since(*started_at) <= DISCOVERY_LEASE =>
        {
            CachedTaskDiscovery::Pending
        }
        Some(TaskDiscoveryCacheEntry::Pending { .. }) => {
            cache.entries.insert(
                key,
                TaskDiscoveryCacheEntry::Failed {
                    reason: DiscoveryFailure::Timeout,
                    completed_at: now,
                },
            );
            CachedTaskDiscovery::Failed(DiscoveryFailure::Timeout)
        }
        Some(TaskDiscoveryCacheEntry::Failed { reason, .. }) => {
            CachedTaskDiscovery::Failed(*reason)
        }
        Some(TaskDiscoveryCacheEntry::Ready {
            tasks,
            completed_at,
        }) if now.duration_since(*completed_at) <= TASK_DISCOVERY_TTL => {
            CachedTaskDiscovery::Ready(tasks.clone())
        }
        Some(TaskDiscoveryCacheEntry::Ready { .. }) => {
            cache.entries.remove(&key);
            CachedTaskDiscovery::Missing
        }
        None => CachedTaskDiscovery::Missing,
    }
}

pub(super) fn prepare_task_discovery(target: &DiscoveryTarget) -> (TaskDiscoveryRequest, u64) {
    let key = DiscoveryCacheKey::from(target);
    let now = Instant::now();
    let mut cache = task_discovery_cache()
        .lock()
        .expect("task discovery cache lock should not be poisoned");

    match cache.entries.get(&key) {
        Some(TaskDiscoveryCacheEntry::Pending { started_at, .. })
            if now.duration_since(*started_at) <= DISCOVERY_LEASE =>
        {
            (TaskDiscoveryRequest::Pending, 0)
        }
        Some(TaskDiscoveryCacheEntry::Failed { completed_at, .. })
            if now.duration_since(*completed_at) <= DISCOVERY_RETRY =>
        {
            (TaskDiscoveryRequest::UseCached, 0)
        }
        Some(TaskDiscoveryCacheEntry::Ready { completed_at, .. })
            if now.duration_since(*completed_at) <= TASK_DISCOVERY_TTL =>
        {
            (TaskDiscoveryRequest::UseCached, 0)
        }
        _ => {
            let generation = next_discovery_generation();
            cache.entries.insert(
                key,
                TaskDiscoveryCacheEntry::Pending {
                    generation,
                    started_at: now,
                },
            );
            (TaskDiscoveryRequest::Start, generation)
        }
    }
}

pub(super) fn complete_task_discovery(
    target: &DiscoveryTarget,
    generation: u64,
    result: Result<Vec<MiseTask>, DiscoveryFailure>,
) {
    let key = DiscoveryCacheKey::from(target);
    let mut cache = task_discovery_cache()
        .lock()
        .expect("task discovery cache lock should not be poisoned");
    if !matches!(cache.entries.get(&key), Some(TaskDiscoveryCacheEntry::Pending { generation: current, .. }) if *current == generation)
    {
        return;
    }
    let completed_at = Instant::now();
    let entry = match result {
        Ok(tasks) => TaskDiscoveryCacheEntry::Ready {
            tasks,
            completed_at,
        },
        Err(reason) => TaskDiscoveryCacheEntry::Failed {
            reason,
            completed_at,
        },
    };
    cache.entries.insert(key, entry);
}

pub(super) fn spawn_task_discovery_worker<F>(target: DiscoveryTarget, generation: u64, work: F)
where
    F: FnOnce(&DiscoveryTarget) -> Result<Vec<MiseTask>, DiscoveryFailure> + Send + 'static,
{
    std::thread::spawn(move || {
        let tasks = work(&target);
        complete_task_discovery(&target, generation, tasks);
    });
}

pub(crate) fn spawn_task_discovery(target: DiscoveryTarget) -> TaskDiscoveryRequest {
    match prepare_task_discovery(&target) {
        (TaskDiscoveryRequest::Start, generation) => {
            spawn_task_discovery_worker(target, generation, try_discover_tasks_for_target);
            TaskDiscoveryRequest::Start
        }
        (other, _) => other,
    }
}

#[cfg(test)]
pub(crate) fn clear_task_discovery_cache_for_test() {
    task_discovery_cache()
        .lock()
        .expect("task discovery cache lock should not be poisoned")
        .entries
        .clear();
}

#[cfg(test)]
pub(crate) fn task_discovery_test_guard() -> std::sync::MutexGuard<'static, ()> {
    static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|err| err.into_inner())
}

#[cfg(test)]
pub(crate) fn set_pending_task_discovery_for_test(target: &DiscoveryTarget) {
    task_discovery_cache()
        .lock()
        .expect("task discovery cache lock should not be poisoned")
        .entries
        .insert(
            DiscoveryCacheKey::from(target),
            TaskDiscoveryCacheEntry::Pending {
                generation: next_discovery_generation(),
                started_at: Instant::now(),
            },
        );
}

#[cfg(test)]
pub(crate) fn set_ready_task_discovery_for_test(
    target: &DiscoveryTarget,
    tasks: Vec<MiseTask>,
    age: Duration,
) {
    task_discovery_cache()
        .lock()
        .expect("task discovery cache lock should not be poisoned")
        .entries
        .insert(
            DiscoveryCacheKey::from(target),
            TaskDiscoveryCacheEntry::Ready {
                tasks,
                completed_at: Instant::now() - age,
            },
        );
}

#[cfg(test)]
pub(super) fn spawn_task_discovery_for_test<F>(
    target: DiscoveryTarget,
    work: F,
) -> TaskDiscoveryRequest
where
    F: FnOnce(&DiscoveryTarget) -> Vec<MiseTask> + Send + 'static,
{
    match prepare_task_discovery(&target) {
        (TaskDiscoveryRequest::Start, generation) => {
            spawn_task_discovery_worker(target, generation, move |target| Ok(work(target)));
            TaskDiscoveryRequest::Start
        }
        (other, _) => other,
    }
}
