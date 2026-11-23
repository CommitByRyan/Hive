use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Cooperative cancellation flag shared by the runner, model wait and tools.
///
/// A token created with [`CancellationToken::child`] is cancelled whenever its
/// parent is cancelled, but cancelling the child never cancels the parent.
/// This lets a whole run share one token while each node keeps its own.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    flag: Arc<AtomicBool>,
    parent: Option<Box<CancellationToken>>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a child token that is cancelled when `self` is cancelled.
    pub fn child(&self) -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
            parent: Some(Box::new(self.clone())),
        }
    }

    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        if self.flag.load(Ordering::SeqCst) {
            return true;
        }
        match &self.parent {
            Some(parent) => parent.is_cancelled(),
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_starts_uncancelled_and_cancels() {
        let token = CancellationToken::new();
        assert!(!token.is_cancelled());
        token.cancel();
        assert!(token.is_cancelled());
    }

    #[test]
    fn parent_cancel_cascades_to_child() {
        let parent = CancellationToken::new();
        let child = parent.child();
        assert!(!child.is_cancelled());
        parent.cancel();
        assert!(child.is_cancelled());
    }

    #[test]
    fn child_cancel_does_not_cancel_parent() {
        let parent = CancellationToken::new();
        let child = parent.child();
        child.cancel();
        assert!(child.is_cancelled());
        assert!(!parent.is_cancelled());
    }

    #[test]
    fn nested_children_cascade() {
        let root = CancellationToken::new();
        let child = root.child();
        let grandchild = child.child();
        assert!(!grandchild.is_cancelled());
        root.cancel();
        assert!(grandchild.is_cancelled());
    }
}
