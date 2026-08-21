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
        Some(TaskDiscoveryCacheEntry::Pending) => CachedTaskDiscovery::Pending,
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

pub(super) fn prepare_task_discovery(target: &DiscoveryTarget) -> TaskDiscoveryRequest {
    let key = DiscoveryCacheKey::from(target);
    let now = Instant::now();
    let mut cache = task_discovery_cache()
        .lock()
        .expect("task discovery cache lock should not be poisoned");

    match cache.entries.get(&key) {
        Some(TaskDiscoveryCacheEntry::Pending) => TaskDiscoveryRequest::Pending,
        Some(TaskDiscoveryCacheEntry::Ready { completed_at, .. })
            if now.duration_since(*completed_at) <= TASK_DISCOVERY_TTL =>
        {
            TaskDiscoveryRequest::UseCached
        }
        _ => {
            cache.entries.insert(key, TaskDiscoveryCacheEntry::Pending);
            TaskDiscoveryRequest::Start
        }
    }
}

pub(super) fn complete_task_discovery(target: &DiscoveryTarget, tasks: Vec<MiseTask>) {
    let key = DiscoveryCacheKey::from(target);
    task_discovery_cache()
        .lock()
        .expect("task discovery cache lock should not be poisoned")
        .entries
        .insert(
            key,
            TaskDiscoveryCacheEntry::Ready {
                tasks,
                completed_at: Instant::now(),
            },
        );
}

pub(super) fn spawn_task_discovery_worker<F>(target: DiscoveryTarget, work: F)
where
    F: FnOnce(&DiscoveryTarget) -> Vec<MiseTask> + Send + 'static,
{
    std::thread::spawn(move || {
        let tasks = work(&target);
        complete_task_discovery(&target, tasks);
    });
}

pub(crate) fn spawn_task_discovery(target: DiscoveryTarget) -> TaskDiscoveryRequest {
    match prepare_task_discovery(&target) {
        TaskDiscoveryRequest::Start => {
            spawn_task_discovery_worker(target, discover_tasks_for_target);
            TaskDiscoveryRequest::Start
        }
        other => other,
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
            TaskDiscoveryCacheEntry::Pending,
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
        TaskDiscoveryRequest::Start => {
            spawn_task_discovery_worker(target, work);
            TaskDiscoveryRequest::Start
        }
        other => other,
    }
}
