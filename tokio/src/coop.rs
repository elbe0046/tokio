//! Opt-in yield points for improved cooperative scheduling.
//!
//! A single call to [`poll`] on a top-level task may potentially do a lot of
//! work before it returns `Poll::Pending`. If a task runs for a long period of
//! time without yielding back to the executor, it can starve other tasks
//! waiting on that executor to execute them, or drive underlying resources.
//! Since Rust does not have a runtime, it is difficult to forcibly preempt a
//! long-running task. Instead, this module provides an opt-in mechanism for
//! futures to collaborate with the executor to avoid starvation.
//!
//! Consider a future like this one:
//!
//! ```
//! # use tokio::stream::{Stream, StreamExt};
//! async fn drop_all<I: Stream + Unpin>(mut input: I) {
//!     while let Some(_) = input.next().await {}
//! }
//! ```
//!
//! It may look harmless, but consider what happens under heavy load if the
//! input stream is _always_ ready. If we spawn `drop_all`, the task will never
//! yield, and will starve other tasks and resources on the same executor. With
//! opt-in yield points, this problem is alleviated:
//!
//! ```ignore
//! # use tokio::stream::{Stream, StreamExt};
//! async fn drop_all<I: Stream + Unpin>(mut input: I) {
//!     while let Some(_) = input.next().await {
//!         tokio::coop::proceed().await;
//!     }
//! }
//! ```
//!
//! The `proceed` future will coordinate with the executor to make sure that
//! every so often control is yielded back to the executor so it can run other
//! tasks.
//!
//! # Placing yield points
//!
//! Voluntary yield points should be placed _after_ at least some work has been
//! done. If they are not, a future sufficiently deep in the task hierarchy may
//! end up _never_ getting to run because of the number of yield points that
//! inevitably appear before it is reached. In general, you will want yield
//! points to only appear in "leaf" futures -- those that do not themselves poll
//! other futures. By doing this, you avoid double-counting each iteration of
//! the outer future against the cooperating budget.
//!
//! [`poll`]: https://doc.rust-lang.org/std/future/trait.Future.html#tymethod.poll

// NOTE: The doctests in this module are ignored since the whole module is (currently) private.

use std::cell::Cell;

thread_local! {
    static CURRENT: Cell<Budget> = Cell::new(Budget::unconstrained());
}

/// Opaque type tracking the amount of "work" a task may still do before
/// yielding back to the scheduler.
#[derive(Debug, Copy, Clone)]
pub(crate) struct Budget(Option<u8>);

impl Budget {
    /// Budget assigned to a task on each poll.
    ///
    /// The value itself is chosen somewhat arbitrarily. It needs to be high
    /// enough to amortize wakeup and scheduling costs, but low enough that we
    /// do not starve other tasks for too long. The value also needs to be high
    /// enough that particularly deep tasks are able to do at least some useful
    /// work at all.
    ///
    /// Note that as more yield points are added in the ecosystem, this value
    /// will probably also have to be raised.
    const fn initial() -> Budget {
        Budget(Some(128))
    }

    /// Returns an unconstrained budget. Operations will not be limited.
    const fn unconstrained() -> Budget {
        Budget(None)
    }
}

cfg_rt_threaded! {
    impl Budget {
        fn has_remaining(self) -> bool {
            self.0.map(|budget| budget > 0).unwrap_or(true)
        }
    }
}

/// Run the given closure with a cooperative task budget. When the function
/// returns, the budget is reset to the value prior to calling the function.
#[inline(always)]
pub(crate) fn budget<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    struct ResetGuard<'a> {
        cell: &'a Cell<Budget>,
        prev: Budget,
    }

    impl<'a> Drop for ResetGuard<'a> {
        fn drop(&mut self) {
            self.cell.set(self.prev);
        }
    }

    CURRENT.with(move |cell| {
        let prev = cell.get();

        cell.set(Budget::initial());

        let _guard = ResetGuard { cell, prev };

        f()
    })
}

cfg_rt_threaded! {
    #[inline(always)]
    pub(crate) fn has_budget_remaining() -> bool {
        CURRENT.with(|cell| cell.get().has_remaining())
    }
}

cfg_blocking_impl! {
    /// Forcibly remove the budgeting constraints early.
    pub(crate) fn stop() {
        CURRENT.with(|cell| {
            cell.set(Budget::unconstrained());
        });
    }
}

cfg_coop! {
    use std::task::{Context, Poll};

    /// Returns `Poll::Pending` if the current task has exceeded its budget and should yield.
    #[inline]
    pub(crate) fn poll_proceed(cx: &mut Context<'_>) -> Poll<()> {
        CURRENT.with(|cell| {
            let mut budget = cell.get();

            if budget.decrement() {
                cell.set(budget);
                Poll::Ready(())
            } else {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
    }

    impl Budget {
        /// Decrement the budget. Returns `true` if successful. Decrementing fails
        /// when there is not enough remaining budget.
        fn decrement(&mut self) -> bool {
            if let Some(num) = &mut self.0 {
                if *num > 0 {
                    *num -= 1;
                    true
                } else {
                    false
                }
            } else {
                true
            }
    }
    }
}

#[cfg(all(test, not(loom)))]
mod test {
    use super::*;

    fn get() -> Budget {
        CURRENT.with(|cell| cell.get())
    }

    #[test]
    fn bugeting() {
        use futures::future::poll_fn;
        use tokio_test::*;

        assert!(get().0.is_none());

        assert_ready!(task::spawn(()).enter(|cx, _| poll_proceed(cx)));

        assert!(get().0.is_none());

        budget(|| {
            assert_eq!(get().0, Budget::initial().0);
            assert_ready!(task::spawn(()).enter(|cx, _| poll_proceed(cx)));
            assert_eq!(get().0.unwrap(), Budget::initial().0.unwrap() - 1);
            assert_ready!(task::spawn(()).enter(|cx, _| poll_proceed(cx)));
            assert_eq!(get().0.unwrap(), Budget::initial().0.unwrap() - 2);

            budget(|| {
                assert_eq!(get().0, Budget::initial().0);

                assert_ready!(task::spawn(()).enter(|cx, _| poll_proceed(cx)));
                assert_eq!(get().0.unwrap(), Budget::initial().0.unwrap() - 1);
            });

            assert_eq!(get().0.unwrap(), Budget::initial().0.unwrap() - 2);
        });

        assert!(get().0.is_none());

        budget(|| {
            let n = get().0.unwrap();

            for _ in 0..n {
                assert_ready!(task::spawn(()).enter(|cx, _| poll_proceed(cx)));
            }

            let mut task = task::spawn(poll_fn(|cx| {
                ready!(poll_proceed(cx));
                Poll::Ready(())
            }));

            assert_pending!(task.poll());
        });
    }
}
