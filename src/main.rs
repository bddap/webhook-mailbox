//! ## Issues:
//!
//! The current implementation can be abused as generic data storage. Possible fixes:
//! - Data coming in over hook could just be streamed directly to the watcher
//!   that way we wouldn't need to store it.
//! - Set lifetime for data, delete after certain period.
//! - Delete the oldest data when more space is needed.
//!
//! Post bodies that are too large will be truncated. How do we communicate to the user
//! that the body was too large.
//!
//! Get `/hook/<address>` is ignored. Only post works. Maybe this is what we want?
//!
//! Does the user want to be able to get the http headers that were sent to `/hook/<address>`
//!
//! Want to move off of internal polling in `/watch`. Some sort of subscribe system would
//! be good.

use hex::FromHex;
use rocket::{
    data::ByteUnit,
    http::{hyper::header::AUTHORIZATION, Status},
    request::{FromParam, FromRequest, Outcome},
    Data, Request, Rocket,
};
use sha2::Digest;
use std::sync::Mutex;
use std::{
    collections::{HashMap, VecDeque},
    time::Duration,
};

lazy_static::lazy_static! {
    /// This is an example for using doc comment attributes
    static ref DB: Db = Db::default();
}

#[rocket::main]
async fn main() -> Result<(), rocket::Error> {
    rocket_build().launch().await
}

fn rocket_build() -> Rocket<rocket::Build> {
    rocket::build().mount("/", rocket::routes![watch, hook])
}

#[rocket::get("/watch")]
async fn watch(bearer: Mailbox) -> Vec<u8> {
    let addr = bearer.hash();
    // just keeps checking the mailbox
    loop {
        if let Some(ret) = DB.pop(addr) {
            return ret;
        }
        rocket::tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[rocket::post("/hook/<address>", data = "<bod>")]
async fn hook(
    address: Result<Address, String>,
    bod: Data<'_>,
) -> Result<&'static str, (Status, String)> {
    let bytes = bod
        .open(ByteUnit::Kibibyte(4))
        .into_bytes()
        .await
        .map_err(|e| (Status::BadRequest, format!("{e}")))?;
    let address = address.map_err(|e| (Status::BadRequest, e))?;

    DB.put(address, bytes.value);

    Ok("ok")
}

#[derive(Default)]
struct Db {
    mailbox: Mutex<HashMap<Address, VecDeque<Vec<u8>>>>,
}

impl Db {
    fn pop(&self, addr: Address) -> Option<Vec<u8>> {
        use std::collections::hash_map::Entry;
        match DB.mailbox.lock().unwrap().entry(addr) {
            Entry::Occupied(mut occ) => {
                let queue = occ.get_mut();
                assert_ne!(queue.len(), 0);
                let ret = queue.pop_front().unwrap();
                if queue.is_empty() {
                    occ.remove();
                }
                Some(ret)
            }
            Entry::Vacant(_) => None,
        }
    }

    fn put(&self, addr: Address, body: Vec<u8>) {
        let mut mailbox = DB.mailbox.lock().unwrap();
        let queue = mailbox.entry(addr).or_insert_with(Default::default);
        queue.push_back(body);
    }
}

#[derive(Hash, Eq, PartialEq, Clone, Copy)]
struct Address {
    addr: [u8; 32],
}

impl<'a> FromParam<'a> for Address {
    type Error = String;

    fn from_param(param: &'a str) -> Result<Self, Self::Error> {
        let addr = <[u8; 32]>::from_hex(param).map_err(|e| format!("{e}"))?;
        Ok(Address { addr })
    }
}

struct Mailbox {
    token: [u8; 32],
}

impl Mailbox {
    fn hash(&self) -> Address {
        Address {
            addr: sha2::Sha256::digest(self.token).into(),
        }
    }
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for Mailbox {
    type Error = &'static str;

    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let failure_message = "\
            Authorization required, please provide an \"Authorization: Bearer\" header with a hex-encoded 32 octet mailbox key. \
            Example: \"Authorization: Bearer da5b485f9238be728487d3f12841725134889d72ff308fe80e24da8fe209334c\"\
        ";
        let failure = Outcome::Failure((Status::Forbidden, failure_message));
        let header = match req.headers().get_one(AUTHORIZATION.as_str()) {
            Some(h) => h,
            None => return failure,
        };
        let token_hex = match header.strip_prefix("Bearer ") {
            Some(t) => t,
            None => return failure,
        };
        let token = match <[u8; 32]>::from_hex(token_hex) {
            Ok(t) => t,
            Err(_) => return failure,
        };
        Outcome::Success(Mailbox { token })
    }
}

#[cfg(test)]
mod tests {
    use hex::ToHex;
    use rocket::http::{ContentType, Header};

    use super::*;

    impl From<Mailbox> for Header<'static> {
        fn from(mb: Mailbox) -> Self {
            Header::new(
                AUTHORIZATION.as_str(),
                format!("Bearer {}", mb.token.encode_hex::<String>()),
            )
        }
    }

    fn rand_mailbox() -> Mailbox {
        Mailbox {
            token: rand::random(),
        }
    }

    async fn testing_client() -> rocket::local::asynchronous::Client {
        rocket::local::asynchronous::Client::untracked(rocket_build())
            .await
            .unwrap()
    }

    #[rocket::tokio::test]
    async fn check_empty_mailbox() {
        let client = testing_client().await;
        let req = client.get("/watch").header(rand_mailbox()).dispatch();
        tokio::time::timeout(Duration::from_millis(200), req)
            .await
            .unwrap_err();
    }

    #[rocket::tokio::test]
    async fn check_mailbox_noauth() {
        let client = testing_client().await;
        let status = client.get("/watch").dispatch().await.status();
        assert_eq!(status, Status::Forbidden);
    }

    #[rocket::tokio::test]
    async fn check_mailbox_after_post() {
        let client = testing_client().await;
        let mailbox = rand_mailbox();
        let address = mailbox.hash();
        let address_hex = address.addr.encode_hex::<String>();
        let body = r#"{ "value": 42 }"#;

        let got = client
            .post(format!("/hook/{address_hex}"))
            .header(ContentType::JSON)
            .body(body)
            .dispatch()
            .await;
        assert_eq!(got.status(), Status::Ok);

        let got = client.get("/watch").header(mailbox).dispatch().await;
        assert_eq!(got.status(), Status::Ok);
        assert_eq!(got.into_string().await.unwrap(), body);
    }

    #[rocket::tokio::test]
    async fn check_mailbox_during_post() {
        let client = testing_client().await;
        let mailbox = rand_mailbox();
        let address = mailbox.hash();
        let address_hex = address.addr.encode_hex::<String>();
        let body = r#"{ "value": 42 }"#;

        let poster = async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let got = client
                .post(format!("/hook/{address_hex}"))
                .header(ContentType::JSON)
                .body(body)
                .dispatch()
                .await;
            assert_eq!(got.status(), Status::Ok);
        };

        let watcher = async {
            let got = client.get("/watch").header(mailbox).dispatch().await;
            assert_eq!(got.status(), Status::Ok);
            assert_eq!(got.into_string().await.unwrap(), body);
        };

        rocket::futures::future::join(poster, watcher).await;
    }
}
