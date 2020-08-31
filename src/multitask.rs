//! An executor for running async tasks.

#![forbid(unsafe_code)]
#![warn(missing_docs, missing_debug_implementations, rust_2018_idioms)]

use crate::task::task;
use crate::task::JoinHandle;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

/// A runnable future, ready for execution.
///
/// When a future is internally spawned using `task::spawn()` or `task::spawn_local()`,
/// we get back two values:
///
/// 1. an `task::Task<()>`, which we refer to as a `Runnable`
/// 2. an `task::JoinHandle<T, ()>`, which is wrapped inside a `Task<T>`
///
/// Once a `Runnable` is run, it "vanishes" and only reappears when its future is woken. When it's
/// woken up, its schedule function is called, which means the `Runnable` gets pushed into a task
/// queue in an executor.
pub type Runnable = task::Task<()>;

/// A spawned future.
///
/// Tasks are also futures themselves and yield the output of the spawned future.
///
/// When a task is dropped, its gets canceled and won't be polled again. To cancel a task a bit
/// more gracefully and wait until it stops running, use the [`cancel()`][Task::cancel()] method.
///
/// Tasks that panic get immediately canceled. Awaiting a canceled task also causes a panic.
///
/// If a task panics, the panic will be thrown by the [`Ticker::tick()`] invocation that polled it.
///
/// # Examples
///
/// ```
/// use blocking::block_on;
/// use multitask::Executor;
/// use std::thread;
///
/// let ex = Executor::new();
///
/// // Spawn a future onto the executor.
/// let task = ex.spawn(async {
///     println!("Hello from a task!");
///     1 + 2
/// });
///
/// // Run an executor thread.
/// thread::spawn(move || {
///     let (p, u) = parking::pair();
///     let ticker = ex.ticker(move || u.unpark());
///     loop {
///         if !ticker.tick() {
///             p.park();
///         }
///     }
/// });
///
/// // Wait for the result.
/// assert_eq!(block_on(task), 3);
/// ```
#[must_use = "tasks get canceled when dropped, use `.detach()` to run them in the background"]
#[derive(Debug)]
pub struct Task<T>(Option<JoinHandle<T, ()>>);

impl<T> Task<T> {
    /// Detaches the task to let it keep running in the background.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_io::Timer;
    /// use multitask::Executor;
    /// use std::time::Duration;
    ///
    /// let ex = Executor::new();
    ///
    /// // Spawn a deamon future.
    /// ex.spawn(async {
    ///     loop {
    ///         println!("I'm a daemon task looping forever.");
    ///         Timer::new(Duration::from_secs(1)).await;
    ///     }
    /// })
    /// .detach();
    /// ```
    pub fn detach(mut self) {
        self.0.take().unwrap();
    }

    /// Cancels the task and waits for it to stop running.
    ///
    /// Returns the task's output if it was completed just before it got canceled, or [`None`] if
    /// it didn't complete.
    ///
    /// While it's possible to simply drop the [`Task`] to cancel it, this is a cleaner way of
    /// canceling because it also waits for the task to stop running.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_io::Timer;
    /// use blocking::block_on;
    /// use multitask::Executor;
    /// use std::thread;
    /// use std::time::Duration;
    ///
    /// let ex = Executor::new();
    ///
    /// // Spawn a deamon future.
    /// let task = ex.spawn(async {
    ///     loop {
    ///         println!("Even though I'm in an infinite loop, you can still cancel me!");
    ///         Timer::new(Duration::from_secs(1)).await;
    ///     }
    /// });
    ///
    /// // Run an executor thread.
    /// thread::spawn(move || {
    ///     let (p, u) = parking::pair();
    ///     let ticker = ex.ticker(move || u.unpark());
    ///     loop {
    ///         if !ticker.tick() {
    ///             p.park();
    ///         }
    ///     }
    /// });
    ///
    /// block_on(async {
    ///     Timer::new(Duration::from_secs(3)).await;
    ///     task.cancel().await;
    /// });
    /// ```
    pub async fn cancel(self) -> Option<T> {
        let mut task = self;
        let handle = task.0.take().unwrap();
        handle.cancel();
        handle.await
    }
}

impl<T> Drop for Task<T> {
    fn drop(&mut self) {
        if let Some(handle) = &self.0 {
            handle.cancel();
        }
    }
}

impl<T> Future for Task<T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.0.as_mut().unwrap()).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(output) => Poll::Ready(output.expect("task has failed")),
        }
    }
}

#[derive(Debug)]
struct LocalQueue {
    queue: RefCell<VecDeque<Runnable>>,
}

impl LocalQueue {
    fn new() -> Rc<Self> {
        Rc::new(LocalQueue {
            queue: RefCell::new(VecDeque::new()),
        })
    }

    fn push(&self, runnable: Runnable) {
        self.queue.borrow_mut().push_back(runnable);
    }

    fn pop(&self) -> Option<Runnable> {
        self.queue.borrow_mut().pop_front()
    }
}

/// A single-threaded executor.
#[derive(Debug)]
pub struct LocalExecutor {
    local_queue: Rc<LocalQueue>,

    /// Callback invoked to wake the executor up.
    callback: Callback,

    /// Make sure the type is `!Send` and `!Sync`.
    _marker: PhantomData<Rc<()>>,
}

impl UnwindSafe for LocalExecutor {}
impl RefUnwindSafe for LocalExecutor {}

impl LocalExecutor {
    /// Creates a new single-threaded executor.
    ///
    /// # Examples
    ///
    /// ```
    /// use multitask::LocalExecutor;
    ///
    /// let (p, u) = parking::pair();
    /// let ex = LocalExecutor::new(move || u.unpark());
    /// ```
    pub fn new(notify: impl Fn() + 'static) -> LocalExecutor {
        LocalExecutor {
            local_queue: LocalQueue::new(),
            callback: Callback::new(notify),
            _marker: PhantomData,
        }
    }

    /// Spawns a thread-local future onto this executor.
    ///
    /// Returns a [`Task`] handle for the spawned future.
    ///
    /// # Examples
    ///
    /// ```
    /// use multitask::LocalExecutor;
    ///
    /// let (p, u) = parking::pair();
    /// let ex = LocalExecutor::new(move || u.unpark());
    ///
    /// let task = ex.spawn(async { println!("hello") });
    /// ```
    pub fn spawn<T: 'static>(&self, future: impl Future<Output = T> + 'static) -> Task<T> {
        let callback = self.callback.clone();
        let queue = self.local_queue.clone();

        // The function that schedules a runnable task when it gets woken up.
        let schedule = move |runnable: Runnable| {
            queue.push(runnable);
            callback.call();
        };

        // Create a task, push it into the queue by scheduling it, and return its `Task` handle.
        let (runnable, handle) = task::spawn_local(future, schedule, ());
        runnable.schedule();
        return Task(Some(handle));
    }

    /// Gets one task from the queue, if one exists.
    ///
    /// Returns an option rapping the task.
    pub fn get_task(&self) -> Option<Runnable> {
        self.local_queue.pop()
    }
}

impl Drop for LocalExecutor {
    fn drop(&mut self) {
        // TODO(stjepang): Close the local queue and empty it.
        // TODO(stjepang): Cancel all remaining tasks.
    }
}

/// A cloneable callback function.
#[derive(Clone)]
struct Callback(Rc<Box<dyn Fn()>>);

impl Callback {
    fn new(f: impl Fn() + 'static) -> Callback {
        Callback(Rc::new(Box::new(f)))
    }

    fn call(&self) {
        (self.0)();
    }
}

impl PartialEq for Callback {
    fn eq(&self, other: &Callback) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Callback {}

impl fmt::Debug for Callback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("<callback>").finish()
    }
}