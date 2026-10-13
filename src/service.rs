use sha2::Digest;
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use tokio::sync::oneshot;

pub const MAX_WATCHERS: usize = 10_000;

pub struct Db {
    watchers: Mutex<Option<Watchers>>,
}

impl Default for Db {
    fn default() -> Self {
        Db {
            watchers: Mutex::new(Some(Watchers::default())),
        }
    }
}

#[derive(Default)]
struct Watchers {
    count: usize,
    by_address: HashMap<Address, VecDeque<oneshot::Sender<Vec<u8>>>>,
}

pub enum Unavailable {
    Full,
    Closing,
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
        let Some(Watchers { count, by_address }) = &mut *watchers else {
            return;
        };
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
    pub async fn receive(&self, mailbox_key: MailboxKey) -> Result<Vec<u8>, Unavailable> {
        let addr = mailbox_key.hash();
        let (tx, rx) = oneshot::channel();
        {
            let mut watchers = self.watchers.lock().unwrap();
            let watchers = watchers.as_mut().ok_or(Unavailable::Closing)?;
            if watchers.count >= MAX_WATCHERS {
                return Err(Unavailable::Full);
            }
            watchers.count += 1;
            watchers.by_address.entry(addr).or_default().push_back(tx);
        }
        let mut registration = Registration { db: self, addr, rx };
        (&mut registration.rx)
            .await
            .map_err(|_| Unavailable::Closing)
    }

    pub fn close(&self) {
        self.watchers.lock().unwrap().take();
    }

    pub fn send(&self, addr: Address, mut body: Vec<u8>) -> Result<(), Vec<u8>> {
        let mut watchers = self.watchers.lock().unwrap();
        let Some(Watchers { count, by_address }) = &mut *watchers else {
            return Err(body);
        };
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
        let Some(watchers) = &*watchers else {
            return 0;
        };
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
