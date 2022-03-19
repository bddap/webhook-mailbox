use futures::channel::oneshot;
use sha2::Digest;
use std::sync::Mutex;
use std::{
    collections::{HashMap, VecDeque},
    iter::FromIterator,
};

#[derive(Default)]
struct Mailbox {
    pending_senders: VecDeque<oneshot::Sender<Vec<u8>>>,
    pending_recievers: VecDeque<oneshot::Receiver<Vec<u8>>>,
}

impl Mailbox {
    fn receive(&mut self) -> oneshot::Receiver<Vec<u8>> {
        match self.pending_recievers.pop_front() {
            Some(rx) => rx,
            None => {
                let (tx, rx) = oneshot::channel();
                self.pending_senders.push_back(tx);
                rx
            }
        }
    }

    fn sender(&mut self) -> oneshot::Sender<Vec<u8>> {
        match self.pending_senders.pop_front() {
            Some(tx) => tx,
            None => {
                let (tx, rx) = oneshot::channel();
                self.pending_recievers.push_back(rx);
                tx
            }
        }
    }

    fn send(&mut self, body: Vec<u8>) {
        let mut body = body;
        loop {
            match self.sender().send(body) {
                Ok(()) => break,
                Err(b) => {
                    body = b;
                }
            }
        }
    }

    /// an inactive mailbox can be deleted
    fn active(&self) -> bool {
        !(self.pending_recievers.is_empty() && self.pending_senders.is_empty())
    }
}

#[derive(Default)]
pub struct Db {
    mailboxen: Mutex<HashMap<Address, Mailbox>>,
}

impl Db {
    pub fn receive(&self, mailbox_key: MailboxKey) -> oneshot::Receiver<Vec<u8>> {
        let mut mailboxen = self.mailboxen.lock().unwrap();
        let addr = mailbox_key.hash();
        let mailbox = mailboxen.entry(addr).or_insert_with(Default::default);
        let ret = mailbox.receive();
        if !mailbox.active() {
            mailboxen.remove(&addr);
        }
        ret
    }

    pub fn send(&self, addr: Address, body: Vec<u8>) {
        let mut mailboxen = self.mailboxen.lock().unwrap();
        let mailbox = mailboxen.entry(addr).or_insert_with(Default::default);
        mailbox.send(body);
        if !mailbox.active() {
            mailboxen.remove(&addr);
        }
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
