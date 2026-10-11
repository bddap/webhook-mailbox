use rocket::tokio::{sync::oneshot, time::timeout};
use sha2::Digest;
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

pub const MAX_WATCHERS: usize = 10_000;
pub const WATCH_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Default)]
pub struct Db {
    watchers: Mutex<Watchers>,
}

#[derive(Default)]
struct Watchers {
    count: usize,
    by_address: HashMap<Address, VecDeque<oneshot::Sender<Vec<u8>>>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum WatchError {
    Full,
    TimedOut,
    Closed,
}

struct Registration<'a> {
    db: &'a Db,
    addr: Address,
    rx: oneshot::Receiver<Vec<u8>>,
}

impl Drop for Registration<'_> {
    fn drop(&mut self) {
        self.rx.close();
        let mut watchers = self.db.watchers.lock().unwrap();
        let Watchers { count, by_address } = &mut *watchers;
        if let Some(queue) = by_address.get_mut(&self.addr) {
            let before = queue.len();
            queue.retain(|tx| !tx.is_closed());
            *count -= before - queue.len();
            if queue.is_empty() {
                by_address.remove(&self.addr);
            }
        }
    }
}

impl Db {
    pub async fn receive(&self, mailbox_key: MailboxKey) -> Result<Vec<u8>, WatchError> {
        let addr = mailbox_key.hash();
        let (tx, rx) = oneshot::channel();
        {
            let mut watchers = self.watchers.lock().unwrap();
            if watchers.count >= MAX_WATCHERS {
                return Err(WatchError::Full);
            }
            watchers.count += 1;
            watchers.by_address.entry(addr).or_default().push_back(tx);
        }
        let mut registration = Registration { db: self, addr, rx };
        match timeout(WATCH_TIMEOUT, &mut registration.rx).await {
            Ok(Ok(body)) => Ok(body),
            Ok(Err(_)) => Err(WatchError::Closed),
            Err(_) => Err(WatchError::TimedOut),
        }
    }

    pub fn send(&self, addr: Address, mut body: Vec<u8>) -> Result<(), Vec<u8>> {
        let mut watchers = self.watchers.lock().unwrap();
        let Watchers { count, by_address } = &mut *watchers;
        if let Some(queue) = by_address.get_mut(&addr) {
            while let Some(tx) = queue.pop_front() {
                *count -= 1;
                match tx.send(body) {
                    Ok(()) => return Ok(()),
                    Err(b) => body = b,
                }
            }
        }
        Err(body)
    }

    #[cfg(test)]
    pub fn retained(&self) -> usize {
        let watchers = self.watchers.lock().unwrap();
        let queued: Vec<usize> = watchers.by_address.values().map(VecDeque::len).collect();
        assert!(!queued.contains(&0));
        assert_eq!(watchers.count, queued.iter().sum::<usize>());
        watchers.count
    }
}

#[derive(Hash, Eq, PartialEq, Clone, Copy)]
pub struct Address {
    pub addr: [u8; 32],
}

impl hex::ToHex for Address {
    fn encode_hex<T: FromIterator<char>>(&self) -> T {
        self.addr.encode_hex()
    }

    fn encode_hex_upper<T: FromIterator<char>>(&self) -> T {
        self.addr.encode_hex_upper()
    }
}

pub struct MailboxKey {
    pub token: [u8; 32],
}

impl hex::ToHex for MailboxKey {
    fn encode_hex<T: FromIterator<char>>(&self) -> T {
        self.token.encode_hex()
    }

    fn encode_hex_upper<T: FromIterator<char>>(&self) -> T {
        self.token.encode_hex_upper()
    }
}

impl MailboxKey {
    pub fn hash(&self) -> Address {
        Address {
            addr: sha2::Sha256::digest(self.token).into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn watches_never_dropped_are_capped_then_expire() {
        let db: &'static Db = Box::leak(Box::default());
        let watches: Vec<_> = (0..2 * MAX_WATCHERS)
            .map(|_| {
                let key = MailboxKey {
                    token: rand::random(),
                };
                rocket::tokio::spawn(db.receive(key))
            })
            .collect();
        rocket::tokio::time::sleep(Duration::from_millis(1)).await;
        assert_eq!(db.retained(), MAX_WATCHERS);

        let outcomes = rocket::tokio::time::timeout(2 * WATCH_TIMEOUT, async {
            let mut outcomes = Vec::new();
            for watch in watches {
                outcomes.push(watch.await.unwrap());
            }
            outcomes
        })
        .await
        .expect("every watch ends within the timeout");
        let full = outcomes
            .iter()
            .filter(|o| **o == Err(WatchError::Full))
            .count();
        let timed_out = outcomes
            .iter()
            .filter(|o| **o == Err(WatchError::TimedOut))
            .count();
        assert_eq!((full, timed_out), (MAX_WATCHERS, MAX_WATCHERS));
        assert_eq!(db.retained(), 0);
    }
}
