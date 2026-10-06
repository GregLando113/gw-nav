//! A bounded pool of fileserver connections, shared between threads.
//!
//! [`Pool::get`] hands out an idle connection, opens a new one while fewer
//! than `max` are open, or else waits for one to be returned. Connections go
//! back to the pool when the [`Pooled`] guard drops, so a thread should hold
//! at most one at a time: one that waits for a second while holding the
//! first can deadlock.

use std::ops::{Deref, DerefMut};
use std::sync::{Condvar, Mutex};

use super::{FileClient, FileConnError};

type Connect<T> = Box<dyn Fn() -> Result<T, FileConnError> + Send + Sync>;

/// A pool of fileserver connections ([`ConnectionPool::fileserver`]).
pub type ConnectionPool = Pool<FileClient>;

pub struct Pool<T> {
    connect: Connect<T>,
    max: usize,
    state: Mutex<State<T>>,
    returned: Condvar,
}

struct State<T> {
    idle: Vec<T>,
    /// Connections handed out or idle.
    open: usize,
}

impl ConnectionPool {
    /// At most `max` connections to the official fileservers
    /// ([`FileClient::connect`]).
    pub fn fileserver(max: usize) -> Self {
        Self::new(max, FileClient::connect)
    }
}

impl<T> Pool<T> {
    pub fn new(max: usize, connect: impl Fn() -> Result<T, FileConnError> + Send + Sync + 'static) -> Self {
        Self {
            connect: Box::new(connect),
            max: max.max(1),
            state: Mutex::new(State { idle: Vec::new(), open: 0 }),
            returned: Condvar::new(),
        }
    }

    pub fn max(&self) -> usize {
        self.max
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State<T>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A connection: an idle one, a new one (calling `on_connect` first), or
    /// the next one returned.
    pub fn get(&self, on_connect: impl FnOnce()) -> Result<Pooled<'_, T>, FileConnError> {
        let mut state = self.lock();
        loop {
            if let Some(conn) = state.idle.pop() {
                return Ok(Pooled { pool: self, conn: Some(conn) });
            }
            if state.open < self.max {
                state.open += 1;
                drop(state);
                on_connect();
                let mut pooled = Pooled { pool: self, conn: None };
                // On failure, dropping the empty guard releases the slot.
                pooled.conn = Some((self.connect)()?);
                return Ok(pooled);
            }
            state = self.returned.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// A connection taken from a [`Pool`]; returned to it on drop.
pub struct Pooled<'a, T> {
    pool: &'a Pool<T>,
    /// `None` only while (re)connecting.
    conn: Option<T>,
}

impl<T> Pooled<'_, T> {
    /// Replace the connection with a new one (after it failed). If that
    /// fails too, the guard is left empty, and its slot is released on drop.
    pub fn reconnect(&mut self) -> Result<(), FileConnError> {
        self.conn = None;
        self.conn = Some((self.pool.connect)()?);
        Ok(())
    }
}

impl<T> Deref for Pooled<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.conn.as_ref().expect("pooled connection")
    }
}

impl<T> DerefMut for Pooled<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.conn.as_mut().expect("pooled connection")
    }
}

impl<T> Drop for Pooled<'_, T> {
    fn drop(&mut self) {
        let mut state = self.pool.lock();
        match self.conn.take() {
            Some(conn) => state.idle.push(conn),
            None => state.open -= 1,
        }
        drop(state);
        self.pool.returned.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use super::*;

    /// A pool of numbered fake connections.
    fn numbered(max: usize) -> (Pool<u32>, Arc<AtomicU32>) {
        let made = Arc::new(AtomicU32::new(0));
        let counter = made.clone();
        (Pool::new(max, move || Ok(counter.fetch_add(1, Ordering::SeqCst))), made)
    }

    #[test]
    fn reuses_idle_connections() {
        let (pool, made) = numbered(2);
        let mut connects = 0;
        let a = pool.get(|| connects += 1).unwrap();
        let b = pool.get(|| connects += 1).unwrap();
        assert_eq!((*a, *b, connects), (0, 1, 2));
        drop(a);
        assert_eq!(*pool.get(|| connects += 1).unwrap(), 0);
        assert_eq!((made.load(Ordering::SeqCst), connects), (2, 2));
    }

    #[test]
    fn waits_for_a_returned_connection() {
        let (pool, made) = numbered(1);
        let held = pool.get(|| {}).unwrap();
        std::thread::scope(|scope| {
            let waiter = scope.spawn(|| *pool.get(|| {}).unwrap());
            std::thread::sleep(Duration::from_millis(50));
            assert!(!waiter.is_finished());
            drop(held);
            assert_eq!(waiter.join().unwrap(), 0);
        });
        assert_eq!(made.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn failed_connects_release_their_slot() {
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let flag = fail.clone();
        let pool = Pool::new(1, move || {
            if flag.load(Ordering::SeqCst) { Err(FileConnError::NoServer("down".into())) } else { Ok(7u32) }
        });
        assert!(pool.get(|| {}).is_err());
        fail.store(false, Ordering::SeqCst);
        let mut conn = pool.get(|| {}).unwrap();
        assert_eq!(*conn, 7);

        // A failed reconnect empties the guard and frees the slot on drop.
        fail.store(true, Ordering::SeqCst);
        assert!(conn.reconnect().is_err());
        drop(conn);
        fail.store(false, Ordering::SeqCst);
        assert_eq!(*pool.get(|| {}).unwrap(), 7);
    }
}
