//! Task helpers.

/// Aborts a tokio task when dropped, so a task stops with its owner.
///
/// Dropping a plain `JoinHandle` detaches the task instead.
pub(crate) struct AbortOnDrop<T>(pub(crate) tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}
