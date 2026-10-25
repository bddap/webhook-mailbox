use std::future::{Future, IntoFuture};
use std::io;
use std::net::{Shutdown, SocketAddr};
use std::pin::{pin, Pin};
use std::sync::Arc;
use std::task::{ready, Context, Poll};
use std::time::Duration;

use axum::{
    body::Bytes,
    extract::{
        connect_info::Connected, ConnectInfo, DefaultBodyLimit, FromRequestParts, Path, State,
    },
    http::{
        header::{AUTHORIZATION, RETRY_AFTER},
        request::Parts,
        StatusCode,
    },
    response::{IntoResponse, Response},
    routing::{get, post},
    serve::{IncomingStream, Listener},
    Router,
};
use futures::future::{select, Either};
use hex::FromHex;
use socket2::{SockRef, TcpKeepalive};
use tokio::io::{AsyncRead, AsyncWrite, Interest, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::time::timeout;

use crate::service::{Address, Db, MailboxKey, Unavailable, Undelivered};

pub const DRAIN_DEADLINE: Duration = Duration::from_secs(5);
const RETRY_AFTER_SECONDS: &str = "1";
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
/// bytes buffered past the request, so it never sees that client hang up. `/watch`
/// waits on the same socket's readiness for read-closed, which arrives regardless.
#[derive(Clone)]
struct Peer(Arc<TcpStream>);

struct Connections(TcpListener);

impl Listener for Connections {
    type Io = Peer;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Peer, SocketAddr) {
        let (stream, addr) = Listener::accept(&mut self.0).await;
        (Peer(Arc::new(stream)), addr)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.0.local_addr()
    }
}

impl Connected<IncomingStream<'_, Connections>> for Peer {
    fn connect_info(stream: IncomingStream<'_, Connections>) -> Self {
        stream.io().clone()
    }
}

/// Every read tries the socket before waiting on readiness, because `hung_up`
/// clears the readiness it shares with this stream.
impl AsyncRead for Peer {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            match self.0.try_read(buf.initialize_unfilled()) {
                Ok(n) => {
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    ready!(self.0.poll_read_ready(cx))?
                }
                Err(e) => return Poll::Ready(Err(e)),
            }
        }
    }
}

impl AsyncWrite for Peer {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            match self.0.try_write(buf) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    ready!(self.0.poll_write_ready(cx))?
                }
                written => return Poll::Ready(written),
            }
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        loop {
            match self.0.try_write_vectored(bufs) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    ready!(self.0.poll_write_ready(cx))?
                }
                written => return Poll::Ready(written),
            }
        }
    }

    fn is_write_vectored(&self) -> bool {
        true
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(SockRef::from(&*self.0).shutdown(Shutdown::Write))
    }
}

async fn hung_up(socket: &TcpStream) {
    loop {
        match socket.ready(Interest::READABLE).await {
            Ok(ready) if !ready.is_read_closed() => {
                let _ = socket.try_io(Interest::READABLE, || {
                    Err::<(), _>(io::ErrorKind::WouldBlock.into())
                });
            }
            _ => return,
        }
    }
}

pub async fn serve(
    listener: TcpListener,
    db: Arc<Db>,
    shutdown: impl Future<Output = ()>,
) -> std::io::Result<()> {
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
        .with_state(db.clone());
    let stop_serving = Arc::new(Notify::new());
    let server = tokio::spawn(
        axum::serve(
            Connections(listener),
            router.into_make_service_with_connect_info::<Peer>(),
        )
        .with_graceful_shutdown({
            let stop_serving = stop_serving.clone();
            async move { stop_serving.notified().await }
        })
        .into_future(),
    );
    shutdown.await;
    db.close();
    stop_serving.notify_one();
    timeout(DRAIN_DEADLINE, server)
        .await
        .map_or(Ok(()), |joined| {
            joined.unwrap_or_else(|panicked| Err(std::io::Error::other(panicked)))
        })
}

fn try_again() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(RETRY_AFTER, RETRY_AFTER_SECONDS)],
    )
        .into_response()
}

async fn watch(
    State(db): State<Arc<Db>>,
    ConnectInfo(Peer(socket)): ConnectInfo<Peer>,
    key: MailboxKey,
) -> Result<Vec<u8>, Response> {
    let received = pin!(db.receive(key));
    let hung_up = pin!(hung_up(&socket));
    match select(received, hung_up).await {
        Either::Left((Ok(hook), _)) => Ok(hook),
        Either::Left((Err(Unavailable::Full), _)) => {
            Err(StatusCode::TOO_MANY_REQUESTS.into_response())
        }
        Either::Left((Err(Unavailable::Closing), _)) => Err(try_again()),
        Either::Right(((), _)) => Err(StatusCode::BAD_REQUEST.into_response()),
    }
}

async fn hook(
    State(db): State<Arc<Db>>,
    Path(address): Path<String>,
    body: Bytes,
) -> Result<&'static str, Response> {
    let addr = <[u8; 32]>::from_hex(&address)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()).into_response())?;
    match db.send(Address { addr }, body.to_vec()).await {
        Ok(()) => Ok("ok"),
        Err(Undelivered::Unwatched) => {
            Err((StatusCode::NOT_FOUND, "nobody is watching this address").into_response())
        }
        Err(Undelivered::Unavailable(Unavailable::Full)) => {
            Err(StatusCode::TOO_MANY_REQUESTS.into_response())
        }
        Err(Undelivered::Unavailable(Unavailable::Closing)) => Err(try_again()),
    }
}

#[cfg(test)]
mod tests {
    use hex::ToHex;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpSocket, TcpStream};
    use tokio::sync::oneshot;
    use tokio::time::{sleep, Instant};

    use super::*;
    use crate::service::{MAX_WATCHERS, REARM_WINDOW};

    const MANY_REQUESTS: usize = 20_000;
    const PROMPTLY: Duration = Duration::from_secs(1);

    struct Response {
        status: u16,
        head: String,
        body: Vec<u8>,
    }

    struct Process {
        server: SocketAddr,
        db: Arc<Db>,
        terminate: oneshot::Sender<()>,
        exited: oneshot::Receiver<Instant>,
    }

    fn spawn_process() -> Process {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = listener.local_addr().unwrap();
        let db = Arc::new(Db::default());
        let (terminate, terminated) = oneshot::channel();
        let (exit, exited) = oneshot::channel();
        let served = db.clone();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            runtime
                .block_on(async {
                    let listener = TcpListener::from_std(listener).unwrap();
                    serve(listener, served, async { terminated.await.unwrap() }).await
                })
                .unwrap();
            drop(runtime);
            exit.send(Instant::now()).unwrap();
        });
        Process {
            server,
            db,
            terminate,
            exited,
        }
    }

    async fn spawn_server() -> (SocketAddr, Arc<Db>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = listener.local_addr().unwrap();
        let db = Arc::new(Db::default());
        tokio::spawn(serve(listener, db.clone(), std::future::pending()));
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

    async fn read_response(stream: TcpStream) -> Response {
        try_read_response(stream).await.expect("no response")
    }

    async fn try_read_response(mut stream: TcpStream) -> Option<Response> {
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).await.ok()?;
        if raw.is_empty() {
            return None;
        }
        let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let status = std::str::from_utf8(&raw[9..12]).unwrap().parse().unwrap();
        Some(Response {
            status,
            head: String::from_utf8(raw[..split].to_vec()).unwrap(),
            body: raw[split + 4..].to_vec(),
        })
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

    async fn deliver_once(server: SocketAddr, db: &Db, mailbox: &MailboxKey) {
        let watch = open_watch(server, mailbox).await;
        retained_reaches(db, 1, PROMPTLY).await;
        assert_eq!(
            post_hook(server, &mailbox.hash(), b"first").await.status,
            200
        );
        assert_eq!(read_response(watch).await.body, b"first");
    }

    #[tokio::test]
    async fn a_hook_landing_while_its_watcher_rearms_is_delivered() {
        let (server, db) = spawn_server().await;
        let mailbox = rand_mailbox();
        deliver_once(server, &db, &mailbox).await;

        let address = mailbox.hash();
        let second = tokio::spawn(async move { post_hook(server, &address, b"second").await });
        retained_reaches(&db, 2, PROMPTLY).await;
        let got = read_response(open_watch(server, &mailbox).await).await;
        assert_eq!((got.status, got.body), (200, b"second".to_vec()));
        assert_eq!(second.await.unwrap().status, 200);
    }

    #[tokio::test]
    async fn a_hook_waits_for_rearm_no_longer_than_the_window() {
        let (server, db) = spawn_server().await;
        let mailbox = rand_mailbox();
        deliver_once(server, &db, &mailbox).await;

        let posted = Instant::now();
        let got = post_hook(server, &mailbox.hash(), b"second").await;
        assert_eq!(got.status, 404);
        assert!(posted.elapsed() >= REARM_WINDOW - PROMPTLY);
        retained_reaches(&db, 0, PROMPTLY).await;
        let got = timeout(PROMPTLY, post_hook(server, &mailbox.hash(), b"third")).await;
        assert_eq!(got.unwrap().status, 404);
    }

    #[tokio::test]
    async fn a_hook_waiting_for_rearm_at_terminate_gets_retry_after() {
        let process = spawn_process();
        let mailbox = rand_mailbox();
        deliver_once(process.server, &process.db, &mailbox).await;

        let address = mailbox.hash().encode_hex::<String>();
        let hook = send_request(
            process.server,
            &format!("POST /hook/{address} HTTP/1.1"),
            b"second",
        )
        .await;
        retained_reaches(&process.db, 2, PROMPTLY).await;
        process.terminate.send(()).unwrap();
        let got = timeout(PROMPTLY, read_response(hook)).await.unwrap();
        assert_eq!(got.status, 503);
        assert!(got.head.contains("\r\nretry-after: 1"), "{}", got.head);
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

    #[tokio::test]
    async fn terminate_ends_waiting_watches_with_retry_after_and_exits() {
        let process = spawn_process();
        let mut watches = Vec::new();
        for _ in 0..3 {
            watches.push(open_watch(process.server, &rand_mailbox()).await);
        }
        retained_reaches(&process.db, watches.len(), PROMPTLY).await;

        process.terminate.send(()).unwrap();
        for watch in watches {
            let got = timeout(PROMPTLY, read_response(watch)).await.unwrap();
            assert_eq!(got.status, 503);
            assert!(got.head.contains("\r\nretry-after: 1"), "{}", got.head);
        }
        timeout(PROMPTLY, process.exited).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_hook_answered_200_at_terminate_reaches_its_watcher() {
        const RACES: usize = 50;
        let mut delivered = 0;
        for _ in 0..RACES {
            let process = spawn_process();
            let mailbox = rand_mailbox();
            let watch = open_watch(process.server, &mailbox).await;
            retained_reaches(&process.db, 1, PROMPTLY).await;
            let address = mailbox.hash().encode_hex::<String>();
            let mut hook = send_raw(
                process.server,
                format!("POST /hook/{address} HTTP/1.1\r\nHost: test\r\nConnection: close\r\nContent-Length: 4\r\n\r\nbo").as_bytes(),
            )
            .await;
            sleep(Duration::from_millis(20)).await;

            hook.write_all(b"dy").await.unwrap();
            process.terminate.send(()).unwrap();
            let sent = timeout(PROMPTLY, try_read_response(hook)).await.unwrap();
            let got = timeout(PROMPTLY, try_read_response(watch)).await.unwrap();
            let got = got.expect("the watch got no response");
            match sent.map(|sent| sent.status) {
                Some(200) => {
                    assert_eq!((got.status, got.body), (200, b"body".to_vec()));
                    delivered += 1;
                }
                Some(503) | None => assert_eq!(got.status, 503),
                Some(other) => panic!("hook got {other}"),
            }
            timeout(PROMPTLY, process.exited).await.unwrap().unwrap();
        }
        assert!(delivered > 0, "no race delivered before terminate");
    }

    #[tokio::test]
    async fn watches_after_terminate_are_refused() {
        let process = spawn_process();
        let head = watch_head("/watch", &rand_mailbox());
        let mut late = send_raw(process.server, head.as_bytes()).await;
        let _stalled = send_raw(
            process.server,
            b"POST /hook/00 HTTP/1.1\r\nHost: test\r\nContent-Length: 2\r\n\r\n",
        )
        .await;
        sleep(Duration::from_millis(200)).await;

        process.terminate.send(()).unwrap();
        timeout(PROMPTLY, async {
            while TcpStream::connect(process.server).await.is_ok() {
                sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        late.write_all(b"\r\nHost: test\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let got = timeout(PROMPTLY, read_response(late)).await.unwrap();
        assert_eq!(got.status, 503);
    }

    #[tokio::test]
    async fn stalled_clients_cannot_hold_exit_past_the_deadline() {
        let process = spawn_process();
        let watch = open_watch(process.server, &rand_mailbox()).await;
        retained_reaches(&process.db, 1, PROMPTLY).await;

        let not_reading = TcpSocket::new_v4().unwrap();
        not_reading.set_recv_buffer_size(1024).unwrap();
        let mut not_reading = not_reading.connect(process.server).await.unwrap();
        let written = Arc::new(AtomicUsize::new(0));
        let flood = tokio::spawn({
            let written = written.clone();
            async move {
                let request = b"GET /watch HTTP/1.1\r\nHost: test\r\n\r\n";
                while not_reading.write_all(request).await.is_ok() {
                    written.fetch_add(request.len(), Ordering::Relaxed);
                }
            }
        });
        let mut before = usize::MAX;
        while before != written.load(Ordering::Relaxed) {
            before = written.load(Ordering::Relaxed);
            sleep(Duration::from_millis(100)).await;
        }
        let _not_sending = send_raw(
            process.server,
            b"POST /hook/00 HTTP/1.1\r\nHost: test\r\nContent-Length: 2\r\n\r\n",
        )
        .await;

        process.terminate.send(()).unwrap();
        let terminated = Instant::now();
        let got = timeout(PROMPTLY, read_response(watch)).await.unwrap();
        assert_eq!(got.status, 503);
        let exited = timeout(DRAIN_DEADLINE + 3 * PROMPTLY, process.exited)
            .await
            .unwrap()
            .unwrap();
        assert!(exited - terminated >= DRAIN_DEADLINE);
        flood.abort();
    }
}
