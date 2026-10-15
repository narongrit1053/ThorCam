//! A one-element lossy queue: a new value replaces one that was not taken yet.
//! This is the LabVIEW version's frame queue of size 1 with Lossy Enqueue Element, so the processing thread
//! always works on the newest frame and the camera thread never waits for it.

use std::sync::{Condvar, Mutex};
use std::time::Duration;

pub struct Latest<T> {
    state: Mutex<State<T>>,
    cv: Condvar,
}

struct State<T> {
    value: Option<T>,
    closed: bool,
}

pub enum Take<T> {
    Value(T),
    Timeout,
    /// The producer is gone and nothing is left.
    Closed,
}

impl<T> Latest<T> {
    pub fn new() -> Self {
        Self { state: Mutex::new(State { value: None, closed: false }), cv: Condvar::new() }
    }

    pub fn put(&self, v: T) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.value = Some(v);
        self.cv.notify_one();
    }

    /// Wakes the consumer; once the last value is taken, [`take`](Self::take) returns [`Take::Closed`].
    pub fn close(&self) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.closed = true;
        self.cv.notify_all();
    }

    pub fn take(&self, timeout: Duration) -> Take<T> {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let (mut s, _) = self
            .cv
            .wait_timeout_while(s, timeout, |s| s.value.is_none() && !s.closed)
            .unwrap_or_else(|e| e.into_inner());
        match s.value.take() {
            Some(v) => Take::Value(v),
            None if s.closed => Take::Closed,
            None => Take::Timeout,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_value_wins_and_close_ends() {
        let q = Latest::new();
        q.put(1);
        q.put(2);
        assert!(matches!(q.take(Duration::ZERO), Take::Value(2)));
        assert!(matches!(q.take(Duration::from_millis(1)), Take::Timeout));
        q.put(3);
        q.close();
        assert!(matches!(q.take(Duration::ZERO), Take::Value(3)));
        assert!(matches!(q.take(Duration::ZERO), Take::Closed));
    }
}
