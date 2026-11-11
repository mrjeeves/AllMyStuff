use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex, OnceLock, Weak},
    time::Duration,
};

type LaunchLock = tokio::sync::Mutex<()>;

/// Coalesce concurrent opens for the same window without holding up launches
/// for other machines. Weak entries disappear when no launch is using them.
pub fn launch_lock(label: &str) -> Arc<LaunchLock> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Weak<LaunchLock>>>> = OnceLock::new();
    let mut locks = LOCKS.get_or_init(Mutex::default).lock().unwrap();
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(label).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(LaunchLock::new(()));
    locks.insert(label.to_string(), Arc::downgrade(&lock));
    lock
}

/// Wait for both a responsive renderer and a mounted app. A renderer that
/// accepts an eval but never calls back must be bounded by the same deadline.
pub async fn wait_until_ready<F, P>(mut probe: F, timeout: Duration) -> Result<(), String>
where
    F: FnMut() -> P,
    P: Future<Output = Result<bool, String>>,
{
    tokio::time::timeout(timeout, async {
        loop {
            if probe().await? {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .map_err(|_| "The window did not finish starting. Please try again.".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn duplicate_clicks_wait_but_other_machines_can_open() {
        let first = launch_lock("console-test-first");
        let first_guard = first.lock().await;
        let duplicate = launch_lock("console-test-first");
        assert!(duplicate.try_lock().is_err());
        let other = launch_lock("console-test-other");
        assert!(other.try_lock().is_ok());
        drop(first_guard);
        assert!(duplicate.try_lock().is_ok());
    }

    #[tokio::test]
    async fn waits_for_app_mount_before_succeeding() {
        let mut calls = 0;
        wait_until_ready(
            || {
                calls += 1;
                std::future::ready(Ok(calls == 3))
            },
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        assert_eq!(calls, 3);
    }

    #[tokio::test]
    async fn missing_renderer_callback_cannot_hold_launch_forever() {
        let error = wait_until_ready(
            std::future::pending::<Result<bool, String>>,
            Duration::from_millis(20),
        )
        .await
        .unwrap_err();
        assert!(error.contains("try again"));
        // A failed launch does not leave a pending probe blocking the retry.
        wait_until_ready(|| std::future::ready(Ok(true)), Duration::from_secs(1))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn failed_mount_is_bounded_even_when_renderer_answers() {
        assert!(
            wait_until_ready(|| std::future::ready(Ok(false)), Duration::from_millis(20))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn native_errors_reach_the_caller_without_waiting_for_deadline() {
        let result = wait_until_ready(
            || std::future::ready(Err("window was destroyed".into())),
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(result, Err("window was destroyed".into()));
    }
}
