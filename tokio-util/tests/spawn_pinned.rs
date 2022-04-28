#![warn(rust_2018_idioms)]

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use tokio_util::task;

/// Simple test of running a !Send future via spawn_pinned
#[tokio::test]
async fn can_spawn_not_send_future() {
    let pool = task::LocalPoolHandle::<()>::new(1);

    let output = pool
        .spawn_pinned(|_| {
            // Rc is !Send + !Sync
            let local_data = Rc::new("test");

            // This future holds an Rc, so it is !Send
            async move { local_data.to_string() }
        })
        .await
        .unwrap();

    assert_eq!(output, "test");
}

/// Dropping the join handle still lets the task execute
#[test]
fn can_drop_future_and_still_get_output() {
    let pool = task::LocalPoolHandle::<()>::new(1);
    let (sender, receiver) = std::sync::mpsc::channel();

    let _ = pool.spawn_pinned(move |_| {
        // Rc is !Send + !Sync
        let local_data = Rc::new("test");

        // This future holds an Rc, so it is !Send
        async move {
            let _ = sender.send(local_data.to_string());
        }
    });

    assert_eq!(receiver.recv(), Ok("test".to_string()));
}

#[test]
#[should_panic(expected = "assertion failed: pool_size > 0")]
fn cannot_create_zero_sized_pool() {
    let _pool = task::LocalPoolHandle::<()>::new(0);
}

/// We should be able to spawn multiple futures onto the pool at the same time.
#[tokio::test]
async fn can_spawn_multiple_futures() {
    let pool = task::LocalPoolHandle::<()>::new(2);

    let join_handle1 = pool.spawn_pinned(|_| {
        let local_data = Rc::new("test1");
        async move { local_data.to_string() }
    });
    let join_handle2 = pool.spawn_pinned(|_| {
        let local_data = Rc::new("test2");
        async move { local_data.to_string() }
    });

    assert_eq!(join_handle1.await.unwrap(), "test1");
    assert_eq!(join_handle2.await.unwrap(), "test2");
}

/// A panic in the spawned task causes the join handle to return an error.
/// But, you can continue to spawn tasks.
#[tokio::test]
async fn task_panic_propagates() {
    let pool = task::LocalPoolHandle::<()>::new(1);

    let join_handle = pool.spawn_pinned(|_| async {
        panic!("Test panic");
    });

    let result = join_handle.await;
    assert!(result.is_err());
    let error = result.unwrap_err();
    assert!(error.is_panic());
    let panic_str: &str = *error.into_panic().downcast().unwrap();
    assert_eq!(panic_str, "Test panic");

    // Trying again with a "safe" task still works
    let join_handle = pool.spawn_pinned(|_| async { "test" });
    let result = join_handle.await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "test");
}

/// A panic during task creation causes the join handle to return an error.
/// But, you can continue to spawn tasks.
#[tokio::test]
async fn callback_panic_does_not_kill_worker() {
    let pool = task::LocalPoolHandle::<()>::new(1);

    let join_handle = pool.spawn_pinned(|_| {
        panic!("Test panic");
        #[allow(unreachable_code)]
        async {}
    });

    let result = join_handle.await;
    assert!(result.is_err());
    let error = result.unwrap_err();
    assert!(error.is_panic());
    let panic_str: &str = *error.into_panic().downcast().unwrap();
    assert_eq!(panic_str, "Test panic");

    // Trying again with a "safe" callback works
    let join_handle = pool.spawn_pinned(|_| async { "test" });
    let result = join_handle.await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "test");
}

/// Canceling the task via the returned join handle cancels the spawned task
/// (which has a different, internal join handle).
#[tokio::test]
async fn task_cancellation_propagates() {
    let pool = task::LocalPoolHandle::<()>::new(1);
    let notify_dropped = Arc::new(());
    let weak_notify_dropped = Arc::downgrade(&notify_dropped);

    let (start_sender, start_receiver) = tokio::sync::oneshot::channel();
    let (drop_sender, drop_receiver) = tokio::sync::oneshot::channel::<()>();
    let join_handle = pool.spawn_pinned(|_| async move {
        let _drop_sender = drop_sender;
        // Move the Arc into the task
        let _notify_dropped = notify_dropped;
        let _ = start_sender.send(());

        // Keep the task running until it gets aborted
        futures::future::pending::<()>().await;
    });

    // Wait for the task to start
    let _ = start_receiver.await;

    join_handle.abort();

    // Wait for the inner task to abort, dropping the sender.
    // The top level join handle aborts quicker than the inner task (the abort
    // needs to propagate and get processed on the worker thread), so we can't
    // just await the top level join handle.
    let _ = drop_receiver.await;

    // Check that the Arc has been dropped. This verifies that the inner task
    // was canceled as well.
    assert!(weak_notify_dropped.upgrade().is_none());
}

/// Tasks should be given to the least burdened worker. When spawning two tasks
/// on a pool with two empty workers the tasks should be spawned on separate
/// workers.
#[tokio::test]
async fn tasks_are_balanced() {
    let pool = task::LocalPoolHandle::<()>::new(2);

    // Spawn a task so one thread has a task count of 1
    let (start_sender1, start_receiver1) = tokio::sync::oneshot::channel();
    let (end_sender1, end_receiver1) = tokio::sync::oneshot::channel();
    let join_handle1 = pool.spawn_pinned(|_| async move {
        let _ = start_sender1.send(());
        let _ = end_receiver1.await;
        std::thread::current().id()
    });

    // Wait for the first task to start up
    let _ = start_receiver1.await;

    // This task should be spawned on the other thread
    let (start_sender2, start_receiver2) = tokio::sync::oneshot::channel();
    let join_handle2 = pool.spawn_pinned(|_| async move {
        let _ = start_sender2.send(());
        std::thread::current().id()
    });

    // Wait for the second task to start up
    let _ = start_receiver2.await;

    // Allow the first task to end
    let _ = end_sender1.send(());

    let thread_id1 = join_handle1.await.unwrap();
    let thread_id2 = join_handle2.await.unwrap();

    // Since the first task was active when the second task spawned, they should
    // be on separate workers/threads.
    assert_ne!(thread_id1, thread_id2);
}


#[test]
#[should_panic(expected = "the number of workers is 1 but the index is 1")]
fn cannot_spawn_task_with_index_out_of_range() {
    let pool = task::LocalPoolHandle::<()>::new(1);
    pool.spawn_pinned_at(1, |_| async { "test" });
}

#[tokio::test]
async fn can_access_shared_local_data() {
    let pool = task::LocalPoolHandle::<Rc<RefCell<Option<String>>>>::new(2);

    pool
        .spawn_pinned_at(1, |data| async move {
            data.borrow_mut().replace("test".to_string())
        })
        .await
        .unwrap();

    let output = pool
        .spawn_pinned_at(1, |data| async move { data.borrow().clone() })
        .await
        .unwrap();

    assert_eq!(output, Some("test".to_string()));
}

#[tokio::test]
async fn can_spawn_multiple_futures_on_same_worker() {
    let pool = task::LocalPoolHandle::<()>::new(2);

    let join_handle1 = pool.spawn_pinned_at(1, |_| {
        let local_data = Rc::new("test1");
        async move { local_data.to_string() }
    });
    let join_handle2 = pool.spawn_pinned_at(1, |_| {
        let local_data = Rc::new("test2");
        async move { local_data.to_string() }
    });

    assert_eq!(join_handle1.await.unwrap(), "test1");
    assert_eq!(join_handle2.await.unwrap(), "test2");
}
