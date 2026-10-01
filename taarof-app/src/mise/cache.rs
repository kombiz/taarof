//! Task discovery cache and background worker.

use super::*;

#[derive(Debug)]
pub(super) struct Subscriber {
    key: DiscoveryCacheKey,
    generation: u64,
    reply: tokio::sync::oneshot::Sender<CachedTaskDiscovery>,
}

/// GTK-local ownership: dropping this removes the sender and aborts the local
/// receiver, including its single lease deadline. No GTK value crosses threads.
pub(crate) struct TaskDiscoverySubscription {
    id: u64,
    task: glib::JoinHandle<()>,
    completed: std::rc::Rc<std::cell::Cell<bool>>,
}

impl Drop for TaskDiscoverySubscription {
    fn drop(&mut self) {
        task_discovery_cache()
            .lock()
            .expect("task discovery cache lock should not be poisoned")
            .subscribers
            .remove(&self.id);
        if !self.completed.get() {
            self.task.abort();
        }
    }
}

fn notify_subscribers(
    cache: &mut TaskDiscoveryCache,
    key: &DiscoveryCacheKey,
    generation: u64,
    result: CachedTaskDiscovery,
) {
    let ids: Vec<_> = cache
        .subscribers
        .iter()
        .filter_map(|(id, subscriber)| {
            (&subscriber.key == key && subscriber.generation == generation).then_some(*id)
        })
        .collect();
    for id in ids {
        if let Some(subscriber) = cache.subscribers.remove(&id) {
            let _ = subscriber.reply.send(result.clone());
        }
    }
}

fn expire_subscription(key: &DiscoveryCacheKey, generation: u64) {
    let mut cache = task_discovery_cache()
        .lock()
        .expect("task discovery cache lock should not be poisoned");
    if matches!(cache.entries.get(key), Some(TaskDiscoveryCacheEntry::Pending { generation: current, .. }) if *current == generation)
    {
        cache.entries.insert(
            key.clone(),
            TaskDiscoveryCacheEntry::Failed {
                reason: DiscoveryFailure::Timeout,
                completed_at: Instant::now(),
            },
        );
    }
    notify_subscribers(
        &mut cache,
        key,
        generation,
        CachedTaskDiscovery::Failed(DiscoveryFailure::Timeout),
    );
}

/// Registration and the cache read share the completion mutex. Even if the
/// worker completed between the caller's Ready check and this call, the reply
/// is queued once. Only a pending generation installs a subscription/deadline.
fn register_subscription(
    target: &DiscoveryTarget,
) -> (
    u64,
    DiscoveryCacheKey,
    u64,
    Instant,
    tokio::sync::oneshot::Receiver<CachedTaskDiscovery>,
) {
    let key = DiscoveryCacheKey::from(target);
    let id = next_discovery_generation();
    let (reply, receiver) = tokio::sync::oneshot::channel();
    let mut cache = task_discovery_cache()
        .lock()
        .expect("task discovery cache lock should not be poisoned");
    let (generation, deadline) = match cache.entries.get(&key) {
        Some(TaskDiscoveryCacheEntry::Pending {
            generation,
            started_at,
        }) => (*generation, *started_at + DISCOVERY_LEASE),
        Some(TaskDiscoveryCacheEntry::Ready {
            tasks,
            completed_at,
        }) if completed_at.elapsed() <= TASK_DISCOVERY_TTL => {
            let _ = reply.send(CachedTaskDiscovery::Ready(tasks.clone()));
            return (id, key, 0, Instant::now(), receiver);
        }
        Some(TaskDiscoveryCacheEntry::Failed { reason, .. }) => {
            let _ = reply.send(CachedTaskDiscovery::Failed(*reason));
            return (id, key, 0, Instant::now(), receiver);
        }
        _ => {
            let _ = reply.send(CachedTaskDiscovery::Missing);
            return (id, key, 0, Instant::now(), receiver);
        }
    };
    cache.subscribers.insert(
        id,
        Subscriber {
            key: key.clone(),
            generation,
            reply,
        },
    );
    (id, key, generation, deadline, receiver)
}

pub(crate) fn subscribe_task_discovery(
    target: &DiscoveryTarget,
    callback: impl FnOnce(CachedTaskDiscovery) + 'static,
) -> TaskDiscoverySubscription {
    let (id, key, generation, deadline, receiver) = register_subscription(target);
    let target_for_delivery = target.clone();
    let completed = std::rc::Rc::new(std::cell::Cell::new(false));
    let completed_for_task = completed.clone();
    let task = glib::spawn_future_local(async move {
        let result = if generation == 0 {
            receiver.await.ok()
        } else {
            let mut receiver = receiver;
            let timeout = glib::timeout_future(deadline.saturating_duration_since(Instant::now()));
            tokio::select! {
                biased;
                result = &mut receiver => result.ok(),
                _ = timeout => {
                    expire_subscription(&key, generation);
                    receiver.await.ok()
                }
            }
        };
        completed_for_task.set(true);
        if result.is_some() {
            // Recheck freshness once at dispatch, without starting work. GTK
            // may have been blocked past Ready TTL, or a later request may
            // already own a new Pending generation. Never resurrect a queued
            // stale Ready payload or follow it with an autonomous retry.
            let current = match cached_task_discovery(&target_for_delivery) {
                CachedTaskDiscovery::Pending => CachedTaskDiscovery::Missing,
                current => current,
            };
            callback(current);
        }
    });
    TaskDiscoverySubscription {
        id,
        task,
        completed,
    }
}

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
        Some(TaskDiscoveryCacheEntry::Pending { generation, .. }) => {
            let generation = *generation;
            cache.entries.insert(
                key.clone(),
                TaskDiscoveryCacheEntry::Failed {
                    reason: DiscoveryFailure::Timeout,
                    completed_at: now,
                },
            );
            notify_subscribers(
                &mut cache,
                &key,
                generation,
                CachedTaskDiscovery::Failed(DiscoveryFailure::Timeout),
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
            if let Some(TaskDiscoveryCacheEntry::Pending { generation, .. }) =
                cache.entries.get(&key)
            {
                let generation = *generation;
                notify_subscribers(
                    &mut cache,
                    &key,
                    generation,
                    CachedTaskDiscovery::Failed(DiscoveryFailure::Timeout),
                );
            }
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
    // The deadline belongs to the worker generation, not the GTK dispatch
    // time. A delayed main context must not turn an expired lease into Ready.
    let result = if matches!(cache.entries.get(&key), Some(TaskDiscoveryCacheEntry::Pending { started_at, .. }) if completed_at.duration_since(*started_at) > DISCOVERY_LEASE)
    {
        Err(DiscoveryFailure::Timeout)
    } else {
        result
    };
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
    let result = match &entry {
        TaskDiscoveryCacheEntry::Ready { tasks, .. } => CachedTaskDiscovery::Ready(tasks.clone()),
        TaskDiscoveryCacheEntry::Failed { reason, .. } => CachedTaskDiscovery::Failed(*reason),
        TaskDiscoveryCacheEntry::Pending { .. } => unreachable!(),
    };
    cache.entries.insert(key.clone(), entry);
    notify_subscribers(&mut cache, &key, generation, result);
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
pub(crate) fn complete_current_task_discovery_for_test(
    target: &DiscoveryTarget,
    result: Result<Vec<MiseTask>, DiscoveryFailure>,
) {
    let generation = match task_discovery_cache()
        .lock()
        .unwrap()
        .entries
        .get(&DiscoveryCacheKey::from(target))
    {
        Some(TaskDiscoveryCacheEntry::Pending { generation, .. }) => *generation,
        _ => panic!("test completion requires a pending generation"),
    };
    complete_task_discovery(target, generation, result);
}

#[cfg(test)]
pub(crate) fn task_subscriber_count_for_test() -> usize {
    task_discovery_cache().lock().unwrap().subscribers.len()
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

#[cfg(test)]
mod subscription_tests {
    use super::*;

    fn target(name: &str) -> DiscoveryTarget {
        DiscoveryTarget::Local {
            cwd: name.into(),
            binary_path: None,
        }
    }

    #[test]
    fn completion_notifies_both_same_target_viewers_but_not_other_target() {
        let _guard = task_discovery_test_guard();
        clear_task_discovery_cache_for_test();
        let first = target("subscription-first");
        let second = target("subscription-second");
        let (_, generation) = prepare_task_discovery(&first);
        let (_, other_generation) = prepare_task_discovery(&second);
        let (_, _, _, _, mut one) = register_subscription(&first);
        let (_, _, _, _, mut two) = register_subscription(&first);
        let (_, _, _, _, mut other) = register_subscription(&second);
        assert_eq!(
            prepare_task_discovery(&first).0,
            TaskDiscoveryRequest::Pending
        );
        complete_task_discovery(&first, generation, Ok(Vec::new()));
        assert_eq!(
            one.try_recv().unwrap(),
            CachedTaskDiscovery::Ready(Vec::new())
        );
        assert_eq!(
            two.try_recv().unwrap(),
            CachedTaskDiscovery::Ready(Vec::new())
        );
        assert!(matches!(
            other.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        complete_task_discovery(
            &second,
            other_generation,
            Err(DiscoveryFailure::InvalidJson),
        );
        assert_eq!(
            other.try_recv().unwrap(),
            CachedTaskDiscovery::Failed(DiscoveryFailure::InvalidJson)
        );
        assert!(task_discovery_cache()
            .lock()
            .unwrap()
            .subscribers
            .is_empty());
    }

    #[test]
    fn completion_before_or_after_registration_is_delivered_once_and_ready_ttl_is_preserved() {
        let _guard = task_discovery_test_guard();
        clear_task_discovery_cache_for_test();
        let target = target("subscription-race");
        let (_, generation) = prepare_task_discovery(&target);
        let (_, _, _, _, mut before) = register_subscription(&target);
        complete_task_discovery(&target, generation, Ok(Vec::new()));
        let (_, _, generation_after, _, mut after) = register_subscription(&target);
        assert_eq!(generation_after, 0);
        assert_eq!(
            before.try_recv().unwrap(),
            CachedTaskDiscovery::Ready(Vec::new())
        );
        assert_eq!(
            after.try_recv().unwrap(),
            CachedTaskDiscovery::Ready(Vec::new())
        );
        assert!(before.try_recv().is_err());
        assert_eq!(
            prepare_task_discovery(&target).0,
            TaskDiscoveryRequest::UseCached
        );
        set_ready_task_discovery_for_test(
            &target,
            Vec::new(),
            TASK_DISCOVERY_TTL + Duration::from_secs(1),
        );
        let (_, _, _, _, mut expired) = register_subscription(&target);
        assert_eq!(expired.try_recv().unwrap(), CachedTaskDiscovery::Missing);
        assert_eq!(
            prepare_task_discovery(&target).0,
            TaskDiscoveryRequest::Start
        );
    }

    #[test]
    fn expiry_settles_all_viewers_and_stale_completion_cannot_overwrite_retry() {
        let _guard = task_discovery_test_guard();
        clear_task_discovery_cache_for_test();
        let target = target("subscription-expiry");
        let (_, generation) = prepare_task_discovery(&target);
        let (_, key, _, deadline, mut one) = register_subscription(&target);
        let (_, _, _, _, mut two) = register_subscription(&target);
        assert!(deadline.saturating_duration_since(Instant::now()) <= DISCOVERY_LEASE);
        expire_subscription(&key, generation);
        assert_eq!(
            one.try_recv().unwrap(),
            CachedTaskDiscovery::Failed(DiscoveryFailure::Timeout)
        );
        assert_eq!(
            two.try_recv().unwrap(),
            CachedTaskDiscovery::Failed(DiscoveryFailure::Timeout)
        );
        assert_eq!(
            prepare_task_discovery(&target).0,
            TaskDiscoveryRequest::UseCached
        );
        if let Some(TaskDiscoveryCacheEntry::Failed { completed_at, .. }) =
            task_discovery_cache().lock().unwrap().entries.get_mut(&key)
        {
            *completed_at = Instant::now() - DISCOVERY_RETRY - Duration::from_secs(1);
        }
        let (_, next) = prepare_task_discovery(&target);
        assert!(next > generation);
        let (_, _, _, _, mut retry) = register_subscription(&target);
        complete_task_discovery(&target, generation, Ok(Vec::new()));
        assert!(matches!(
            retry.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        complete_task_discovery(&target, next, Ok(Vec::new()));
        assert_eq!(
            retry.try_recv().unwrap(),
            CachedTaskDiscovery::Ready(Vec::new())
        );
    }

    #[test]
    fn pending_completion_after_absolute_lease_is_timeout_before_main_dispatch() {
        let _guard = task_discovery_test_guard();
        clear_task_discovery_cache_for_test();
        let target = target("subscription-late-worker");
        let (_, generation) = prepare_task_discovery(&target);
        let (_, key, _, _, mut first) = register_subscription(&target);
        let (_, _, _, _, mut second) = register_subscription(&target);
        if let Some(TaskDiscoveryCacheEntry::Pending { started_at, .. }) =
            task_discovery_cache().lock().unwrap().entries.get_mut(&key)
        {
            *started_at = Instant::now() - DISCOVERY_LEASE - Duration::from_secs(1);
        }
        complete_task_discovery(&target, generation, Ok(Vec::new()));
        assert_eq!(
            first.try_recv().unwrap(),
            CachedTaskDiscovery::Failed(DiscoveryFailure::Timeout)
        );
        assert_eq!(
            second.try_recv().unwrap(),
            CachedTaskDiscovery::Failed(DiscoveryFailure::Timeout)
        );
        assert_eq!(
            cached_task_discovery(&target),
            CachedTaskDiscovery::Failed(DiscoveryFailure::Timeout)
        );
        assert_eq!(
            prepare_task_discovery(&target).0,
            TaskDiscoveryRequest::UseCached
        );
    }

    #[test]
    fn local_subscription_uses_absolute_deadline_even_when_first_poll_is_delayed() {
        let _guard = task_discovery_test_guard();
        clear_task_discovery_cache_for_test();
        let context = glib::MainContext::new();
        context
            .with_thread_default(|| {
                let target = target("subscription-delayed-dispatch");
                prepare_task_discovery(&target);
                if let Some(TaskDiscoveryCacheEntry::Pending { started_at, .. }) =
                    task_discovery_cache()
                        .lock()
                        .unwrap()
                        .entries
                        .get_mut(&DiscoveryCacheKey::from(&target))
                {
                    *started_at = Instant::now() - DISCOVERY_LEASE + Duration::from_millis(20);
                }
                let result = std::rc::Rc::new(std::cell::RefCell::new(None));
                let result_for_callback = result.clone();
                let subscription = subscribe_task_discovery(&target, move |value| {
                    *result_for_callback.borrow_mut() = Some(value)
                });
                std::thread::sleep(Duration::from_millis(40));
                while context.pending() {
                    context.iteration(false);
                }
                assert_eq!(
                    *result.borrow(),
                    Some(CachedTaskDiscovery::Failed(DiscoveryFailure::Timeout)),
                    "GTK dispatch must not extend the original lease"
                );
                drop(subscription);
            })
            .unwrap();
    }

    #[test]
    fn timely_ready_completion_survives_delayed_dispatch_past_pending_lease() {
        let _guard = task_discovery_test_guard();
        clear_task_discovery_cache_for_test();
        let context = glib::MainContext::new();
        context
            .with_thread_default(|| {
                let target = target("subscription-timely-ready");
                let (_, generation) = prepare_task_discovery(&target);
                let result = std::rc::Rc::new(std::cell::RefCell::new(None));
                let result_for_callback = result.clone();
                let subscription = subscribe_task_discovery(&target, move |value| {
                    *result_for_callback.borrow_mut() = Some(value)
                });
                complete_task_discovery(&target, generation, Ok(Vec::new()));
                // Simulate a concluded lease with timely Ready queued, followed by
                // GTK resuming later. Ready retains its independent45s lifetime.
                set_ready_task_discovery_for_test(
                    &target,
                    Vec::new(),
                    DISCOVERY_LEASE + Duration::from_secs(1),
                );
                while context.pending() {
                    context.iteration(false);
                }
                assert_eq!(
                    *result.borrow(),
                    Some(CachedTaskDiscovery::Ready(Vec::new()))
                );
                assert_eq!(
                    prepare_task_discovery(&target).0,
                    TaskDiscoveryRequest::UseCached
                );
                drop(subscription);
            })
            .unwrap();
    }

    #[test]
    fn queued_ready_completion_past_ttl_settles_missing_without_retry() {
        let _guard = task_discovery_test_guard();
        clear_task_discovery_cache_for_test();
        let context = glib::MainContext::new();
        context
            .with_thread_default(|| {
                let target = target("subscription-expired-ready");
                let (_, generation) = prepare_task_discovery(&target);
                let result = std::rc::Rc::new(std::cell::RefCell::new(None));
                let result_for_callback = result.clone();
                let subscription = subscribe_task_discovery(&target, move |value| {
                    *result_for_callback.borrow_mut() = Some(value)
                });
                complete_task_discovery(&target, generation, Ok(Vec::new()));
                set_ready_task_discovery_for_test(
                    &target,
                    Vec::new(),
                    TASK_DISCOVERY_TTL + Duration::from_secs(1),
                );
                while context.pending() {
                    context.iteration(false);
                }
                assert_eq!(*result.borrow(), Some(CachedTaskDiscovery::Missing));
                assert_eq!(cached_task_discovery(&target), CachedTaskDiscovery::Missing);
                assert_eq!(task_subscriber_count_for_test(), 0);
                drop(subscription);
            })
            .unwrap();
    }

    #[test]
    fn local_subscription_teardown_cancels_queued_result_and_releases_subscriber() {
        let _guard = task_discovery_test_guard();
        clear_task_discovery_cache_for_test();
        let context = glib::MainContext::new();
        context
            .with_thread_default(|| {
                let target = target("subscription-teardown");
                let (_, generation) = prepare_task_discovery(&target);
                let calls = std::rc::Rc::new(std::cell::Cell::new(0));
                let calls_for_callback = calls.clone();
                let subscription = subscribe_task_discovery(&target, move |_| {
                    calls_for_callback.set(calls_for_callback.get() + 1)
                });
                complete_task_discovery(&target, generation, Ok(Vec::new()));
                drop(subscription);
                while context.pending() {
                    context.iteration(false);
                }
                assert_eq!(calls.get(), 0);
                let subscription =
                    subscribe_task_discovery(&target, |_| panic!("cancelled viewer must not run"));
                drop(subscription);
                while context.pending() {
                    context.iteration(false);
                }
                assert!(task_discovery_cache()
                    .lock()
                    .unwrap()
                    .subscribers
                    .is_empty());
            })
            .unwrap();
    }

    #[test]
    fn local_completion_delivers_to_both_viewers_without_a_send_callback() {
        let _guard = task_discovery_test_guard();
        clear_task_discovery_cache_for_test();
        let context = glib::MainContext::new();
        context
            .with_thread_default(|| {
                let target = target("subscription-local-delivery");
                let (_, generation) = prepare_task_discovery(&target);
                let results = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
                let first_results = results.clone();
                let first = subscribe_task_discovery(&target, move |result| {
                    first_results.borrow_mut().push(result)
                });
                let second_results = results.clone();
                let second = subscribe_task_discovery(&target, move |result| {
                    second_results.borrow_mut().push(result)
                });
                complete_task_discovery(&target, generation, Err(DiscoveryFailure::InvalidJson));
                while context.pending() {
                    context.iteration(false);
                }
                assert_eq!(
                    *results.borrow(),
                    vec![CachedTaskDiscovery::Failed(DiscoveryFailure::InvalidJson); 2]
                );
                assert!(task_discovery_cache()
                    .lock()
                    .unwrap()
                    .subscribers
                    .is_empty());
                drop((first, second));
                assert_eq!(
                    prepare_task_discovery(&target).0,
                    TaskDiscoveryRequest::UseCached
                );
            })
            .unwrap();
    }

    #[test]
    fn local_lease_deadline_settles_abandoned_generation_without_starting_retry() {
        let _guard = task_discovery_test_guard();
        clear_task_discovery_cache_for_test();
        let context = glib::MainContext::new();
        context
            .with_thread_default(|| {
                let target = target("subscription-local-expiry");
                let (_, generation) = prepare_task_discovery(&target);
                if let Some(TaskDiscoveryCacheEntry::Pending { started_at, .. }) =
                    task_discovery_cache()
                        .lock()
                        .unwrap()
                        .entries
                        .get_mut(&DiscoveryCacheKey::from(&target))
                {
                    *started_at = Instant::now() - DISCOVERY_LEASE - Duration::from_secs(1);
                }
                let result = std::rc::Rc::new(std::cell::RefCell::new(None));
                let result_for_callback = result.clone();
                let subscription = subscribe_task_discovery(&target, move |value| {
                    *result_for_callback.borrow_mut() = Some(value)
                });
                let started_at = Instant::now();
                while result.borrow().is_none() && started_at.elapsed() < Duration::from_secs(1) {
                    context.iteration(false);
                    std::thread::yield_now();
                }
                assert_eq!(
                    *result.borrow(),
                    Some(CachedTaskDiscovery::Failed(DiscoveryFailure::Timeout))
                );
                complete_task_discovery(&target, generation, Ok(Vec::new()));
                assert_eq!(
                    cached_task_discovery(&target),
                    CachedTaskDiscovery::Failed(DiscoveryFailure::Timeout)
                );
                assert_eq!(
                    prepare_task_discovery(&target).0,
                    TaskDiscoveryRequest::UseCached
                );
                assert!(task_discovery_cache()
                    .lock()
                    .unwrap()
                    .subscribers
                    .is_empty());
                drop(subscription);
            })
            .unwrap();
    }

    fn viewer_state() -> (std::rc::Rc<std::cell::RefCell<crate::AppState>>, u32, u32) {
        let mut state = crate::AppState::new();
        let workspace = state.active_workspace;
        let (first, _) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace,
            "first",
            crate::HeadlessPaneSeed::default(),
        )
        .unwrap();
        let (second, _) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace,
            "second",
            crate::HeadlessPaneSeed::default(),
        )
        .unwrap();
        state.activate_tab(first).unwrap();
        (
            std::rc::Rc::new(std::cell::RefCell::new(state)),
            first,
            second,
        )
    }

    #[test]
    fn task_viewer_identity_rejects_tab_workspace_and_focus_round_trips_with_noop_preserved() {
        let (state, first, second) = viewer_state();
        let active = TaskViewerIdentity::for_active(&state.borrow()).unwrap();
        let row = TaskViewerIdentity::for_tab(&state.borrow(), first).unwrap();
        state.borrow_mut().activate_tab(first).unwrap();
        assert!(active.is_current(&state.borrow()));
        state.borrow_mut().activate_tab(second).unwrap();
        state.borrow_mut().activate_tab(first).unwrap();
        assert!(!active.is_current(&state.borrow()));
        assert!(row.is_current(&state.borrow()));
        let active = TaskViewerIdentity::for_active(&state.borrow()).unwrap();
        let workspace = state.borrow().active_workspace;
        let other_workspace = state.borrow_mut().create_workspace("other", None);
        state.borrow_mut().activate_workspace(workspace).unwrap();
        assert!(!active.is_current(&state.borrow()));
        assert!(row.is_current(&state.borrow()));
        state
            .borrow_mut()
            .activate_workspace(other_workspace)
            .unwrap();
        state.borrow_mut().activate_workspace(workspace).unwrap();
        let pane = state.borrow().find_tab(first).unwrap().1.focused_pane_id;
        state.borrow_mut().set_focused_pane(first, pane);
        assert!(row.is_current(&state.borrow()));
        assert!(!state.borrow_mut().set_focused_pane(u32::MAX, pane));
        state.borrow_mut().set_focused_pane(first, pane + 100);
        state.borrow_mut().set_focused_pane(first, pane);
        assert!(!row.is_current(&state.borrow()));
    }

    #[test]
    fn task_viewer_identity_rejects_removed_and_restored_same_tab_id_without_issuer_reset() {
        let (state, first, _) = viewer_state();
        let row = TaskViewerIdentity::for_tab(&state.borrow(), first).unwrap();
        let workspace = state.borrow().active_workspace;
        let origin = state
            .borrow()
            .find_tab(first)
            .unwrap()
            .1
            .work_origin
            .clone();
        state.borrow_mut().remove_tab(first);
        assert_eq!(state.borrow().task_pane_epoch(first), 0);
        assert!(!row.is_current(&state.borrow()));
        state.borrow_mut().next_id = first;
        crate::seed_headless_terminal_tab(
            &mut state.borrow_mut(),
            workspace,
            "recreated",
            crate::HeadlessPaneSeed::default(),
        )
        .unwrap();
        state.borrow_mut().find_tab_mut(first).unwrap().work_origin = origin.clone();
        assert!(!row.is_current(&state.borrow()));
        let recreated = TaskViewerIdentity::for_tab(&state.borrow(), first).unwrap();
        crate::RuntimeHandle::from_shared_state(state.clone()).clear_for_session_restore();
        assert_eq!(state.borrow().task_pane_epoch(first), 0);
        let workspace = state.borrow_mut().create_workspace("restored", None);
        state.borrow_mut().next_id = first;
        crate::seed_headless_terminal_tab(
            &mut state.borrow_mut(),
            workspace,
            "restored",
            crate::HeadlessPaneSeed::default(),
        )
        .unwrap();
        state.borrow_mut().find_tab_mut(first).unwrap().work_origin = origin;
        assert!(!recreated.is_current(&state.borrow()));
    }

    #[test]
    fn task_viewer_callbacks_reject_away_back_but_deliver_current_other_rows() {
        let _guard = task_discovery_test_guard();
        clear_task_discovery_cache_for_test();
        let context = glib::MainContext::new();
        context
            .with_thread_default(|| {
                let (state, first, second) = viewer_state();
                let first_target = target("viewer-first");
                let other_target = target("viewer-other");
                let (_, generation) = prepare_task_discovery(&first_target);
                let (_, other_generation) = prepare_task_discovery(&other_target);
                let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
                let palette_calls = calls.clone();
                let palette = subscribe_task_discovery_for_viewer(
                    &state,
                    TaskViewerIdentity::for_active(&state.borrow()).unwrap(),
                    &first_target,
                    move |_| palette_calls.borrow_mut().push("stale palette"),
                );
                let row_calls = calls.clone();
                let row = subscribe_task_discovery_for_viewer(
                    &state,
                    TaskViewerIdentity::for_tab(&state.borrow(), first).unwrap(),
                    &first_target,
                    move |_| row_calls.borrow_mut().push("first row"),
                );
                let other_calls = calls.clone();
                let other = subscribe_task_discovery_for_viewer(
                    &state,
                    TaskViewerIdentity::for_tab(&state.borrow(), second).unwrap(),
                    &other_target,
                    move |_| other_calls.borrow_mut().push("other row"),
                );
                state.borrow_mut().activate_tab(second).unwrap();
                state.borrow_mut().activate_tab(first).unwrap();
                complete_task_discovery(&first_target, generation, Ok(Vec::new()));
                complete_task_discovery(&other_target, other_generation, Ok(Vec::new()));
                while context.pending() {
                    context.iteration(false);
                }
                let mut actual = calls.borrow().clone();
                actual.sort_unstable();
                assert_eq!(actual, vec!["first row", "other row"]);
                assert_eq!(task_subscriber_count_for_test(), 0);
                drop((palette, row, other));
            })
            .unwrap();
    }
}
