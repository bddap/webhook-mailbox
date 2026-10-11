use std::os::fd::{AsFd, OwnedFd};
use std::sync::Arc;
use std::time::Duration;

use axum::{
    body::Bytes,
    extract::{
        connect_info::Connected, ConnectInfo, DefaultBodyLimit, FromRequestParts, Path, State,
    },
    http::{header::AUTHORIZATION, request::Parts, StatusCode},
    routing::{get, post},
    serve::IncomingStream,
    Router,
};
use hex::FromHex;
use socket2::{SockRef, TcpKeepalive};
use tokio::io::{unix::AsyncFd, Interest};
use tokio::net::TcpListener;

use crate::service::{Address, Db, Full, MailboxKey};

const MAX_HOOK_BYTES: usize = 4096;
const KEEPALIVE_IDLE: Duration = Duration::from_secs(60);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);

const AUTHORIZATION_REQUIRED: &str = "\
    Authorization required, please provide an \"Authorization: Bearer\" header with a hex-encoded 32 octet mailbox key. \
    Example: \"Authorization: Bearer da5b485f9238be728487d3f12841725134889d72ff308fe80e24da8fe209334c\"\
";

impl<S: Send + Sync> FromRequestParts<S> for MailboxKey {
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|header| header.to_str().ok())
            .and_then(|header| header.strip_prefix("Bearer "))
            .and_then(|token| <[u8; 32]>::from_hex(token).ok())
            .map(|token| MailboxKey { token })
            .ok_or((StatusCode::FORBIDDEN, AUTHORIZATION_REQUIRED))
    }
}

/// hyper stops reading a connection whose request body was left unread or that has
/// bytes buffered past the request, so it never sees that client hang up. A duplicate
/// of the socket watched for read-closed sees it regardless.
#[derive(Clone)]
struct Peer(Option<Arc<AsyncFd<OwnedFd>>>);

impl Connected<IncomingStream<'_, TcpListener>> for Peer {
    fn connect_info(stream: IncomingStream<'_, TcpListener>) -> Self {
        let socket = stream
            .io()
            .as_fd()
            .try_clone_to_owned()
            .and_then(|fd| AsyncFd::with_interest(fd, Interest::READABLE));
        if let Err(e) = &socket {
            eprintln!("cannot watch {} for hangup: {e}", stream.remote_addr());
        }
        Peer(socket.ok().map(Arc::new))
    }
}

async fn hung_up(socket: &AsyncFd<OwnedFd>) {
    loop {
        match socket.readable().await {
            Ok(mut ready) if !ready.ready().is_read_closed() => ready.clear_ready(),
            _ => return,
        }
    }
}

pub async fn serve(listener: TcpListener, db: Arc<Db>) -> std::io::Result<()> {
    SockRef::from(&listener).set_tcp_keepalive(
        &TcpKeepalive::new()
            .with_time(KEEPALIVE_IDLE)
            .with_interval(KEEPALIVE_INTERVAL),
    )?;
    let router = Router::new()
        .route("/watch", get(watch))
        .route("/watch/", get(watch))
        .route("/hook/{address}", post(hook))
        .route("/hook/{address}/", post(hook))
        .layer(DefaultBodyLimit::max(MAX_HOOK_BYTES))
        .with_state(db);
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<Peer>(),
    )
    .await
}

async fn watch(
    State(db): State<Arc<Db>>,
    ConnectInfo(Peer(socket)): ConnectInfo<Peer>,
    key: MailboxKey,
) -> Result<Vec<u8>, StatusCode> {
    let socket = socket.ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    tokio::select! {
        biased;
        received = db.receive(key) => received.map_err(|Full| StatusCode::TOO_MANY_REQUESTS),
        () = hung_up(&socket) => Err(StatusCode::BAD_REQUEST),
    }
}

async fn hook(
    State(db): State<Arc<Db>>,
    Path(address): Path<String>,
    body: Bytes,
) -> Result<&'static str, (StatusCode, String)> {
    let addr =
        <[u8; 32]>::from_hex(&address).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    db.send(Address { addr }, body.to_vec())
        .map(|()| "ok")
        .map_err(|_| {
            (
                StatusCode::NOT_FOUND,
                "nobody is watching this address".into(),
            )
        })
}

#[cfg(test)]
mod tests {
    use hex::ToHex;
    use std::net::SocketAddr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use tokio::time::{sleep, timeout};

    use super::*;
    use crate::service::MAX_WATCHERS;

    const MANY_REQUESTS: usize = 20_000;
    const PROMPTLY: Duration = Duration::from_secs(1);

    struct Response {
        status: u16,
        body: Vec<u8>,
    }

    async fn spawn_server() -> (SocketAddr, Arc<Db>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = listener.local_addr().unwrap();
        let db = Arc::new(Db::default());
        tokio::spawn(serve(listener, db.clone()));
        (server, db)
    }

    fn rand_mailbox() -> MailboxKey {
        MailboxKey {
            token: rand::random(),
        }
    }

    async fn send_raw(server: SocketAddr, raw: &[u8]) -> TcpStream {
        let mut stream = TcpStream::connect(server).await.unwrap();
        stream.write_all(raw).await.unwrap();
        stream
    }

    async fn send_request(server: SocketAddr, head: &str, body: &[u8]) -> TcpStream {
        let head = format!(
            "{head}\r\nHost: test\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        send_raw(server, &[head.as_bytes(), body].concat()).await
    }

    async fn read_response(mut stream: TcpStream) -> Response {
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).await.unwrap();
        let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let status = std::str::from_utf8(&raw[9..12]).unwrap().parse().unwrap();
        Response {
            status,
            body: raw[split + 4..].to_vec(),
        }
    }

    async fn request(server: SocketAddr, head: &str, body: &[u8]) -> Response {
        read_response(send_request(server, head, body).await).await
    }

    fn watch_head(path: &str, key: &MailboxKey) -> String {
        let token = key.encode_hex::<String>();
        format!("GET {path} HTTP/1.1\r\nAuthorization: Bearer {token}")
    }

    async fn open_watch(server: SocketAddr, key: &MailboxKey) -> TcpStream {
        send_request(server, &watch_head("/watch", key), b"").await
    }

    async fn post_hook(server: SocketAddr, address: &Address, body: &[u8]) -> Response {
        let address = address.encode_hex::<String>();
        request(server, &format!("POST /hook/{address} HTTP/1.1"), body).await
    }

    async fn retained_reaches(db: &Db, expected: usize, within: Duration) {
        timeout(within, async {
            while db.retained() != expected {
                sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("retained {} != {expected}", db.retained()));
    }

    #[tokio::test]
    async fn watch_without_key_is_forbidden() {
        let (server, _) = spawn_server().await;
        for auth in [
            "",
            "\r\nAuthorization: Bearer nothex",
            "\r\nAuthorization: abcd",
        ] {
            let got = request(server, &format!("GET /watch HTTP/1.1{auth}"), b"").await;
            assert_eq!(got.status, 403);
        }
    }

    #[tokio::test]
    async fn hook_to_malformed_address_is_bad_request() {
        let (server, _) = spawn_server().await;
        let got = request(server, "POST /hook/zz HTTP/1.1", b"x").await;
        assert_eq!(got.status, 400);
    }

    #[tokio::test]
    async fn waiting_watch_receives_raw_hook_bytes() {
        let (server, db) = spawn_server().await;
        let mailbox = rand_mailbox();
        let body: Vec<u8> = (0..=255).collect();
        let watch = open_watch(server, &mailbox).await;
        retained_reaches(&db, 1, PROMPTLY).await;

        let sent = post_hook(server, &mailbox.hash(), &body).await;
        assert_eq!((sent.status, sent.body.as_slice()), (200, &b"ok"[..]));
        let got = read_response(watch).await;
        assert_eq!((got.status, got.body), (200, body));
        assert_eq!(db.retained(), 0);
    }

    #[tokio::test]
    async fn trailing_slashes_are_accepted() {
        let (server, db) = spawn_server().await;
        let mailbox = rand_mailbox();
        let watch = send_request(server, &watch_head("/watch/", &mailbox), b"").await;
        retained_reaches(&db, 1, PROMPTLY).await;

        let address = mailbox.hash().encode_hex::<String>();
        let sent = request(server, &format!("POST /hook/{address}/ HTTP/1.1"), b"x").await;
        assert_eq!(sent.status, 200);
        assert_eq!(read_response(watch).await.body, b"x");
    }

    #[tokio::test]
    async fn hook_over_the_body_limit_is_refused() {
        let (server, db) = spawn_server().await;
        let mailbox = rand_mailbox();
        let _watch = open_watch(server, &mailbox).await;
        retained_reaches(&db, 1, PROMPTLY).await;

        let got = post_hook(server, &mailbox.hash(), &[b'x'; MAX_HOOK_BYTES + 1]).await;
        assert_eq!(got.status, 413);
        assert_eq!(db.retained(), 1);
    }

    #[tokio::test]
    async fn post_without_watcher_is_not_stored() {
        let (server, db) = spawn_server().await;
        let mailbox = rand_mailbox();

        let got = post_hook(server, &mailbox.hash(), b"{ \"value\": 42 }").await;
        assert_eq!(got.status, 404);

        let mut watch = open_watch(server, &mailbox).await;
        retained_reaches(&db, 1, PROMPTLY).await;
        let mut buf = [0; 1];
        timeout(Duration::from_millis(200), watch.read(&mut buf))
            .await
            .unwrap_err();
    }

    #[tokio::test]
    async fn unwatched_hooks_retain_nothing() {
        let (server, db) = spawn_server().await;
        let body = [b'x'; MAX_HOOK_BYTES];
        for _ in 0..MANY_REQUESTS {
            let got = post_hook(server, &rand_mailbox().hash(), &body).await;
            assert_eq!(got.status, 404);
        }
        assert_eq!(db.retained(), 0);
    }

    #[tokio::test]
    async fn dropped_watches_retain_nothing() {
        const EACH: usize = 20;
        let (server, db) = spawn_server().await;
        let shapes: [(&str, &[u8]); 5] = [
            ("", b""),
            ("Content-Length: 1\r\n", b""),
            ("Transfer-Encoding: chunked\r\n", b""),
            ("", b"GET /watch HTTP/1.1\r\nHost: test\r\n\r\n"),
            ("", b"GET /wa"),
        ];
        let mut watches = Vec::new();
        for (headers, tail) in shapes {
            for _ in 0..EACH {
                let head = watch_head("/watch", &rand_mailbox());
                let raw = format!("{head}\r\nHost: test\r\n{headers}\r\n");
                watches.push(send_raw(server, &[raw.as_bytes(), tail].concat()).await);
            }
        }
        retained_reaches(&db, shapes.len() * EACH, PROMPTLY).await;

        drop(watches);
        retained_reaches(&db, 0, PROMPTLY).await;
    }

    #[tokio::test]
    async fn watch_waits_as_long_as_its_client_stays_connected() {
        let (server, db) = spawn_server().await;
        let mailbox = rand_mailbox();
        let watch = open_watch(server, &mailbox).await;
        retained_reaches(&db, 1, PROMPTLY).await;

        sleep(Duration::from_secs(61)).await;
        let sent = post_hook(server, &mailbox.hash(), b"late").await;
        assert_eq!(sent.status, 200);
        let got = read_response(watch).await;
        assert_eq!((got.status, got.body), (200, b"late".to_vec()));
    }

    #[tokio::test]
    async fn watches_beyond_the_cap_are_refused() {
        let (server, db) = spawn_server().await;
        let waiting: Vec<_> = (0..MAX_WATCHERS)
            .map(|_| {
                let db = db.clone();
                tokio::spawn(async move { db.receive(rand_mailbox()).await.is_ok() })
            })
            .collect();
        retained_reaches(&db, MAX_WATCHERS, PROMPTLY).await;

        let refused = read_response(open_watch(server, &rand_mailbox()).await).await;
        assert_eq!(refused.status, 429);

        for watch in waiting {
            watch.abort();
        }
        retained_reaches(&db, 0, PROMPTLY).await;
    }
}
