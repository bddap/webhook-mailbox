use sha2::Digest;
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::{timeout, Instant};

/// Bounds everything held at once: waiting watches, waiting hooks and re-arm leases.
pub const MAX_WATCHERS: usize = 10_000;
/// How long after a delivery its address keeps hooks waiting for the watcher to re-arm.
pub const REARM_WINDOW: Duration = Duration::from_secs(5);

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
    by_address: HashMap<Address, Mailbox>,
    leases: VecDeque<(Instant, Address)>,
}

#[derive(Default)]
struct Mailbox {
    watches: VecDeque<oneshot::Sender<Vec<u8>>>,
    hooks: VecDeque<Hook>,
    leases: usize,
}

struct Hook {
    body: Vec<u8>,
    taken: oneshot::Sender<()>,
}

impl Mailbox {
    fn len(&self) -> usize {
        self.watches.len() + self.hooks.len() + self.leases
    }
}

impl Watchers {
    fn expire_leases(&mut self, now: Instant) {
        while let Some(&(deadline, addr)) = self.leases.front() {
            if deadline > now {
                break;
            }
            self.leases.pop_front();
            self.by_address.get_mut(&addr).unwrap().leases -= 1;
            self.count -= 1;
            self.remove_if_empty(addr);
        }
    }

    /// Called only after taking one entry, so the lease cannot push `count` past the cap.
    fn lease(&mut self, addr: Address) {
        self.count += 1;
        self.by_address.entry(addr).or_default().leases += 1;
        self.leases.push_back((Instant::now() + REARM_WINDOW, addr));
    }

    fn take_hook(&mut self, addr: Address) -> Option<Vec<u8>> {
        let mailbox = self.by_address.get_mut(&addr)?;
        while let Some(hook) = mailbox.hooks.pop_front() {
            self.count -= 1;
            if hook.taken.send(()).is_ok() {
                self.lease(addr);
                return Some(hook.body);
            }
        }
        None
    }

    fn deliver(&mut self, addr: Address, mut body: Vec<u8>) -> Result<(), Vec<u8>> {
        let Some(mailbox) = self.by_address.get_mut(&addr) else {
            return Err(body);
        };
        while let Some(tx) = mailbox.watches.pop_front() {
            self.count -= 1;
            match tx.send(body) {
                Ok(()) => {
                    self.lease(addr);
                    return Ok(());
                }
                Err(b) => body = b,
            }
        }
        Err(body)
    }

    fn prune(&mut self, addr: Address) {
        if let Some(mailbox) = self.by_address.get_mut(&addr) {
            let before = mailbox.len();
            mailbox.watches.retain(|tx| !tx.is_closed());
            mailbox.hooks.retain(|hook| !hook.taken.is_closed());
            self.count -= before - mailbox.len();
            self.remove_if_empty(addr);
        }
    }

    fn remove_if_empty(&mut self, addr: Address) {
        if self.by_address.get(&addr).is_some_and(|m| m.len() == 0) {
            self.by_address.remove(&addr);
        }
    }
}

pub enum Unavailable {
    Full,
    Closing,
}

pub enum Undelivered {
    Unwatched,
    Unavailable(Unavailable),
}

struct Registration<'a, T> {
    db: &'a Db,
    addr: Address,
    rx: oneshot::Receiver<T>,
}

impl<T> Drop for Registration<'_, T> {
    fn drop(&mut self) {
        self.rx.close();
        if let Some(watchers) = &mut *self.db.watchers.lock().unwrap() {
            watchers.prune(self.addr);
        }
    }
}

impl Db {
    fn with<R>(&self, f: impl FnOnce(&mut Watchers) -> R) -> Result<R, Unavailable> {
        let mut watchers = self.watchers.lock().unwrap();
        let watchers = watchers.as_mut().ok_or(Unavailable::Closing)?;
        watchers.expire_leases(Instant::now());
        Ok(f(watchers))
    }

    pub async fn receive(&self, mailbox_key: MailboxKey) -> Result<Vec<u8>, Unavailable> {
        let addr = mailbox_key.hash();
        let (tx, rx) = oneshot::channel();
        let hook = self.with(|watchers| {
            if let Some(hook) = watchers.take_hook(addr) {
                return Ok(Some(hook));
            }
            if watchers.count >= MAX_WATCHERS {
                return Err(Unavailable::Full);
            }
            watchers.count += 1;
            watchers
                .by_address
                .entry(addr)
                .or_default()
                .watches
                .push_back(tx);
            Ok(None)
        })??;
        if let Some(hook) = hook {
            return Ok(hook);
        }
        let mut registration = Registration { db: self, addr, rx };
        (&mut registration.rx)
            .await
            .map_err(|_| Unavailable::Closing)
    }

    /// With no watch waiting, a hook to an address that delivered within the last
    /// `REARM_WINDOW` waits up to `REARM_WINDOW` for its watcher to re-arm.
    pub async fn send(&self, addr: Address, body: Vec<u8>) -> Result<(), Undelivered> {
        let (tx, rx) = oneshot::channel();
        let queued = self.with(|watchers| {
            let Err(body) = watchers.deliver(addr, body) else {
                return Ok(false);
            };
            if watchers.by_address.get(&addr).is_none_or(|m| m.leases == 0) {
                return Err(Undelivered::Unwatched);
            }
            if watchers.count >= MAX_WATCHERS {
                return Err(Undelivered::Unavailable(Unavailable::Full));
            }
            watchers.count += 1;
            let mailbox = watchers.by_address.get_mut(&addr).unwrap();
            mailbox.hooks.push_back(Hook { body, taken: tx });
            Ok(true)
        });
        if !queued.map_err(Undelivered::Unavailable)?? {
            return Ok(());
        }
        let mut registration = Registration { db: self, addr, rx };
        match timeout(REARM_WINDOW, &mut registration.rx).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(Undelivered::Unavailable(Unavailable::Closing)),
            Err(_) => {
                registration.rx.close();
                registration
                    .rx
                    .try_recv()
                    .map_err(|_| Undelivered::Unwatched)
            }
        }
    }

    pub fn close(&self) {
        self.watchers.lock().unwrap().take();
    }

    #[cfg(test)]
    pub fn retained(&self) -> usize {
        self.with(|watchers| {
            let held: Vec<usize> = watchers.by_address.values().map(Mailbox::len).collect();
            assert!(!held.contains(&0));
            assert_eq!(watchers.count, held.iter().sum::<usize>());
            assert_eq!(
                watchers.leases.len(),
                watchers
                    .by_address
                    .values()
                    .map(|m| m.leases)
                    .sum::<usize>()
            );
            watchers.count
        })
        .unwrap_or(0)
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
