use hex::FromHex;
use rocket::{
    data::ByteUnit,
    http::Status,
    request::{FromParam, FromRequest, Outcome},
    Data, Request, Rocket, State,
};

use crate::service::{Address, Db, MailboxKey, WatchError};

const AUTHORIZATION: &str = "Authorization";

impl<'a> FromParam<'a> for Address {
    type Error = String;

    fn from_param(param: &'a str) -> Result<Self, Self::Error> {
        let addr = <[u8; 32]>::from_hex(param).map_err(|e| format!("{e}"))?;
        Ok(Address { addr })
    }
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for MailboxKey {
    type Error = &'static str;

    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let failure_message = "\
            Authorization required, please provide an \"Authorization: Bearer\" header with a hex-encoded 32 octet mailbox key. \
            Example: \"Authorization: Bearer da5b485f9238be728487d3f12841725134889d72ff308fe80e24da8fe209334c\"\
        ";
        let failure = Outcome::Error((Status::Forbidden, failure_message));
        let header = match req.headers().get_one(AUTHORIZATION) {
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
        Outcome::Success(MailboxKey { token })
    }
}

pub fn rocket_build(db: Db) -> Rocket<rocket::Build> {
    rocket::build()
        .manage(db)
        .mount("/", rocket::routes![watch, hook])
}

#[rocket::get("/watch")]
async fn watch(db: &State<Db>, bearer: MailboxKey) -> Result<Vec<u8>, Status> {
    db.receive(bearer).await.map_err(|e| match e {
        WatchError::TimedOut => Status::RequestTimeout,
        WatchError::Full | WatchError::Closed => Status::ServiceUnavailable,
    })
}

#[rocket::post("/hook/<address>", data = "<bod>")]
async fn hook(
    db: &State<Db>,
    address: Result<Address, String>,
    bod: Data<'_>,
) -> Result<&'static str, (Status, String)> {
    let bytes = bod
        .open(ByteUnit::Kibibyte(4))
        .into_bytes()
        .await
        .map_err(|e| (Status::BadRequest, format!("{e}")))?;
    let address = address.map_err(|e| (Status::BadRequest, e))?;

    db.send(address, bytes.value).map(|()| "ok").map_err(|_| {
        (
            Status::ServiceUnavailable,
            "nobody is watching this address".into(),
        )
    })
}

#[cfg(test)]
mod tests {
    use hex::ToHex;
    use rocket::futures::FutureExt;
    use rocket::http::{ContentType, Header};
    use std::time::Duration;

    use super::*;

    const MANY_REQUESTS: usize = 20_000;

    impl From<MailboxKey> for Header<'static> {
        fn from(mb: MailboxKey) -> Self {
            Header::new(
                AUTHORIZATION,
                format!("Bearer {}", mb.encode_hex::<String>()),
            )
        }
    }

    fn rand_mailbox() -> MailboxKey {
        MailboxKey {
            token: rand::random(),
        }
    }

    async fn testing_client() -> rocket::local::asynchronous::Client {
        rocket::local::asynchronous::Client::untracked(rocket_build(Db::default()))
            .await
            .unwrap()
    }

    #[rocket::async_test]
    async fn check_empty_mailbox() {
        let client = testing_client().await;
        let req = client.get("/watch").header(rand_mailbox()).dispatch();
        rocket::tokio::time::timeout(Duration::from_millis(200), req)
            .await
            .unwrap_err();
    }

    #[rocket::async_test]
    async fn check_mailbox_noauth() {
        let client = testing_client().await;
        let status = client.get("/watch").dispatch().await.status();
        assert_eq!(status, Status::Forbidden);
    }

    #[rocket::async_test]
    async fn post_without_watcher_is_not_stored() {
        let client = testing_client().await;
        let mailbox = rand_mailbox();
        let address_hex = mailbox.hash().addr.encode_hex::<String>();

        let got = client
            .post(format!("/hook/{address_hex}"))
            .header(ContentType::JSON)
            .body(r#"{ "value": 42 }"#)
            .dispatch()
            .await;
        assert_eq!(got.status(), Status::ServiceUnavailable);

        let req = client.get("/watch").header(mailbox).dispatch();
        rocket::tokio::time::timeout(Duration::from_millis(200), req)
            .await
            .unwrap_err();
    }

    #[rocket::async_test]
    async fn check_mailbox_during_post() {
        let client = testing_client().await;
        let mailbox = rand_mailbox();
        let address = mailbox.hash();
        let address_hex = address.addr.encode_hex::<String>();
        let body = r#"{ "value": 42 }"#;

        let poster = async {
            rocket::tokio::time::sleep(Duration::from_millis(10)).await;
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
        assert_eq!(client.rocket().state::<Db>().unwrap().retained(), 0);
    }

    #[rocket::async_test]
    async fn unwatched_hooks_retain_nothing() {
        let client = testing_client().await;
        let body = "x".repeat(4096);
        for _ in 0..MANY_REQUESTS {
            let address_hex = rand_mailbox().hash().addr.encode_hex::<String>();
            client
                .post(format!("/hook/{address_hex}"))
                .body(&body)
                .dispatch()
                .await;
        }
        assert_eq!(client.rocket().state::<Db>().unwrap().retained(), 0);
    }

    #[rocket::async_test]
    async fn dropped_watches_retain_nothing() {
        let client = testing_client().await;
        let db = client.rocket().state::<Db>().unwrap();
        for _ in 0..MANY_REQUESTS {
            let mut watch = Box::pin(client.get("/watch").header(rand_mailbox()).dispatch());
            assert!((&mut watch).now_or_never().is_none());
            assert_eq!(db.retained(), 1);
        }
        assert_eq!(client.rocket().state::<Db>().unwrap().retained(), 0);
    }
}
