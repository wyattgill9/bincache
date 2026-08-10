//! Protocol conformance, asserted against a real server over a real socket.
//!
//! Every check here corresponds to a client behaviour recorded in `research/DESIGN_V2.md`
//! under "Client behaviours that constrain the design". Conformance gates every
//! optimization, so these run in `cargo nextest run` rather than in a script an operator
//! has to remember.
//!
//! The wire is spoken directly rather than through a client library, because the point is
//! the exact bytes.

/// A minimal but genuine `nix-archive-1` serialization of one regular file, so the NAR
/// hash under test is a hash of something a client could actually have produced.
fn nar(contents: &[u8]) -> Vec<u8> {
    fn padded(value: &[u8], out: &mut Vec<u8>) {
        out.extend_from_slice(&u64::try_from(value.len()).expect("fits").to_le_bytes());
        out.extend_from_slice(value);
        out.resize(out.len() + (8 - value.len() % 8) % 8, 0);
    }

    let mut out = Vec::new();
    for token in [b"nix-archive-1".as_slice(), b"(", b"type", b"regular", b"contents"] {
        padded(token, &mut out);
    }
    padded(contents, &mut out);
    padded(b")", &mut out);
    out
}

struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

struct Server {
    address: std::net::SocketAddr,
    token: String,
    key: bincache_core::sign::PublicKey,
    dir: bincache_core::storepath::Dir,
}

impl Server {
    async fn start(name: &str) -> Self {
        let root = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target"))
            .join("test-artifacts")
            .join("bincache-serve")
            .join(name);
        if let Err(error) = std::fs::remove_dir_all(&root) {
            assert_eq!(error.kind(), std::io::ErrorKind::NotFound, "stale root not removable");
        }

        let store = bincache_store::nar::Store::open(root.join("payload")).await.expect("opens");
        let index = bincache_index::index::Index::open(root.join("index.redb")).expect("opens");
        let secret = bincache_core::sign::SecretKey::generate("bincache-test-1".to_owned());
        let dir =
            bincache_core::storepath::Dir::new(bincache_core::storepath::DIR_DEFAULT.to_owned())
                .expect("absolute");

        let ingest = bincache_ingest::ingest::Ingest::new(bincache_ingest::ingest::Parts {
            store,
            index,
            key: secret.clone(),
            dir: dir.clone(),
            level: bincache_ingest::upload::Level::new(3).expect("in range"),
        });

        let token = bincache_ingest::auth::generate();
        let cache = bincache_serve::handler::Cache::new(bincache_serve::handler::Parts {
            ingest,
            tokens: bincache_ingest::auth::Tokens::new([token.clone()]),
            info: bincache_core::cacheinfo::CacheInfo {
                store_dir: dir.clone(),
                mass_query: bincache_core::cacheinfo::MassQuery::Wanted,
                priority: bincache_core::cacheinfo::Priority(30),
            },
            stats: bincache_serve::stats::Stats::default(),
        });

        // Port zero: the kernel picks, so parallel test binaries never collide.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("binds");
        let address = listener.local_addr().expect("has an address");
        let router = bincache_serve::handler::router(cache);
        tokio::spawn(async move { axum::serve(listener, router).await.expect("serves") });

        Self { address, token, key: secret.public(), dir }
    }

    async fn connect(&self) -> tokio::net::TcpStream {
        tokio::net::TcpStream::connect(self.address).await.expect("connects")
    }

    /// One request on its own connection.
    async fn request(&self, head: &str, body: &[u8]) -> Response {
        let mut stream = self.connect().await;
        exchange(&mut stream, head, body).await
    }

    fn authorized(&self, method: &str, target: &str, len: usize) -> String {
        format!(
            "{method} {target} HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer {}\r\n\
             Content-Length: {len}\r\n\r\n",
            self.token
        )
    }
}

async fn exchange(stream: &mut tokio::net::TcpStream, head: &str, body: &[u8]) -> Response {
    send(stream, head, body).await;
    read_response(stream, Body::from(head)).await
}

async fn send(stream: &mut tokio::net::TcpStream, head: &str, body: &[u8]) {
    let mut request = head.as_bytes().to_vec();
    request.extend_from_slice(body);
    tokio::io::AsyncWriteExt::write_all(stream, &request).await.expect("writes the request");
}

/// Whether a response is followed by body bytes on the wire.
///
/// `Content-Length` on a `HEAD` response describes the body a `GET` would return, and a
/// `100 Continue` has no length at all. Reading either as a body waits forever, so the
/// reader is told which to expect rather than guessing from the headers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Body {
    Expected,
    None,
}

impl Body {
    fn from(head: &str) -> Self {
        if head.starts_with("HEAD ") { Self::None } else { Self::Expected }
    }
}

/// Reads exactly one response, using `Content-Length` for the body, which is the only
/// framing this server ever emits.
async fn read_response(stream: &mut tokio::net::TcpStream, body: Body) -> Response {
    try_read_response(stream, body).await.expect("peer closed before a complete response head")
}

/// [`read_response`], but reporting a closed connection rather than asserting on it. A test
/// that is checking whether the connection survived has to be able to see that it did not.
async fn try_read_response(stream: &mut tokio::net::TcpStream, body: Body) -> Option<Response> {
    let mut buffer: Vec<u8> = Vec::new();
    let mut scratch = [0u8; 64 * 1024];
    let head_end = loop {
        if let Some(at) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break at + 4;
        }
        let read = tokio::io::AsyncReadExt::read(stream, &mut scratch).await.expect("reads");
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&scratch[..read]);
    };

    let text = String::from_utf8(buffer[..head_end].to_vec()).expect("head is utf8");
    let mut lines = text.split_terminator("\r\n");
    let status: u16 = lines
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse().ok())
        .expect("a status line");
    let headers: Vec<(String, String)> = lines
        .filter(|line| !line.is_empty())
        .filter_map(|line| line.split_once(": "))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();

    // An interim 1xx carries no length and no body; it is a marker, not a message.
    let length: usize = if body == Body::None || (100..200).contains(&status) {
        0
    } else {
        headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.parse().ok())
            .expect("a Content-Length")
    };

    let mut body = buffer[head_end..].to_vec();
    while body.len() < length {
        let read = tokio::io::AsyncReadExt::read(stream, &mut scratch).await.expect("reads");
        assert!(read > 0, "peer closed before the body finished");
        body.extend_from_slice(&scratch[..read]);
    }
    body.truncate(length);
    Some(Response { status, headers, body })
}

fn fields(body: &[u8]) -> std::collections::HashMap<String, Vec<String>> {
    let mut parsed: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for line in String::from_utf8_lossy(body).lines() {
        let (key, value) = match line.split_once(": ") {
            Some(split) => split,
            None => (line.trim_end_matches(':'), ""),
        };
        parsed.entry(key.to_owned()).or_default().push(value.to_owned());
    }
    parsed
}

/// Uploads a NAR and publishes a narinfo for it, exactly as `nix copy --to
/// 'http://host?compression=none'` does: the payload first, then the metadata.
struct Published {
    path: String,
    key: String,
    nar: Vec<u8>,
    nar_hash32: String,
    narinfo: String,
}

async fn publish(server: &Server, seed: &[u8]) -> Published {
    let body = nar(seed);
    let nar_hash = bincache_core::hash::Sha256::digest(&body);
    let nar_hash32 = nar_hash.base32();

    let digest = bincache_core::hash::Sha256::digest(seed);
    let mut raw = [0u8; bincache_core::storepath::HASH_WIDTH];
    raw.copy_from_slice(&digest.as_bytes()[..bincache_core::storepath::HASH_WIDTH]);
    let key = bincache_core::storepath::Hash::from_bytes(raw).to_string();
    let path = format!("/nix/store/{key}-conformance-1.0");

    let head = server.authorized("PUT", &format!("/nar/{nar_hash32}.nar"), body.len());
    assert_eq!(server.request(&head, &body).await.status, 201, "the NAR upload is accepted");

    let narinfo = format!(
        "StorePath: {path}\nURL: nar/{nar_hash32}.nar\nCompression: none\n\
         FileHash: sha256:{nar_hash32}\nFileSize: {}\nNarHash: sha256:{nar_hash32}\n\
         NarSize: {}\nReferences: \n",
        body.len(),
        body.len()
    );
    let head = server.authorized("PUT", &format!("/{key}.narinfo"), narinfo.len());
    assert_eq!(
        server.request(&head, narinfo.as_bytes()).await.status,
        201,
        "the narinfo publish is accepted"
    );

    Published { path, key, nar: body, nar_hash32, narinfo }
}

#[tokio::test]
async fn nix_cache_info_carries_the_three_fields_a_client_reads() {
    let server = Server::start("cache-info").await;
    let response = server.request("GET /nix-cache-info HTTP/1.1\r\nHost: t\r\n\r\n", b"").await;
    assert_eq!(response.status, 200);
    assert_eq!(response.header("Content-Type"), Some("text/x-nix-cache-info"));
    expect_test::expect![[r#"
        StoreDir: /nix/store
        WantMassQuery: 1
        Priority: 30
    "#]]
    .assert_eq(&String::from_utf8(response.body).expect("utf8"));
}

/// The conformance floor from `research/DESIGN_V2.md`: `StorePath`, `NarHash`, `URL`, and a
/// nonzero `NarSize`. Below that, `NarInfo::NarInfo` throws `corrupt` on the client.
#[tokio::test]
async fn a_published_narinfo_verifies_the_way_a_client_verifies_it() {
    let server = Server::start("narinfo").await;
    let published = publish(&server, b"conformance payload").await;

    let response = server
        .request(&format!("GET /{}.narinfo HTTP/1.1\r\nHost: t\r\n\r\n", published.key), b"")
        .await;
    assert_eq!(response.status, 200);
    assert_eq!(response.header("Content-Type"), Some("text/x-nix-narinfo"));
    assert_eq!(response.header("Content-Encoding"), None, "narinfo must not be encoded");

    let parsed = fields(&response.body);
    for required in ["StorePath", "URL", "NarHash", "NarSize"] {
        assert!(parsed.contains_key(required), "{required} is missing from {parsed:?}");
    }
    assert_eq!(parsed["StorePath"][0], published.path);
    assert_eq!(parsed["NarHash"][0], format!("sha256:{}", published.nar_hash32));
    assert_ne!(parsed["NarSize"][0], "0");
    assert_eq!(parsed["Compression"][0], "zstd", "the server recompressed on receipt");

    // The decision a real client makes: accept the path if any Sig verifies against a key
    // in trusted-public-keys, over the canonical fingerprint.
    let record = bincache_core::narinfo::parse::parse(
        &String::from_utf8(response.body).expect("utf8"),
        &server.dir,
    )
    .expect("the served body parses as a narinfo");
    assert_eq!(record.sigs.len(), 1, "managed signing replaces client signatures");
    server
        .key
        .verify(&record.fingerprint(&server.dir), &record.sigs[0])
        .expect("the signature verifies under the advertised public key");
}

#[tokio::test]
async fn head_reuses_the_body_length_and_writes_no_body() {
    let server = Server::start("head").await;
    let published = publish(&server, b"head payload").await;

    let target = format!("/{}.narinfo", published.key);
    let get = server.request(&format!("GET {target} HTTP/1.1\r\nHost: t\r\n\r\n"), b"").await;
    let head = server.request(&format!("HEAD {target} HTTP/1.1\r\nHost: t\r\n\r\n"), b"").await;

    assert_eq!(head.status, 200);
    assert_eq!(head.header("Content-Length"), Some(get.body.len().to_string().as_str()));
    assert!(head.body.is_empty());
}

/// `maybeRetry` in `nix/src/libstore/filetransfer.cc` resumes a dropped NAR only when the
/// original response advertised `Accept-Ranges: bytes` *and* carried no `Content-Encoding`.
/// Getting either wrong makes a dropped 10 GB transfer restart at zero, which presents as a
/// throughput cliff and never as an error.
#[tokio::test]
async fn a_nar_response_is_resumable() {
    let server = Server::start("resumable").await;
    let published = publish(&server, &b"payload for resume".repeat(2048)).await;

    let record =
        bincache_core::narinfo::parse::parse(&published.narinfo, &server.dir).expect("parses");
    let served = server
        .request(&format!("GET /{}.narinfo HTTP/1.1\r\nHost: t\r\n\r\n", published.key), b"")
        .await;
    let url = fields(&served.body)["URL"][0].clone();
    assert_ne!(url, record.nar().url(), "the stored artifact is the recompressed one");

    let whole = server.request(&format!("GET /{url} HTTP/1.1\r\nHost: t\r\n\r\n"), b"").await;
    assert_eq!(whole.status, 200);
    assert_eq!(whole.header("Content-Type"), Some("application/x-nix-nar"));
    assert_eq!(whole.header("Accept-Ranges"), Some("bytes"), "resume needs this header");
    assert_eq!(whole.header("Content-Encoding"), None, "resume is disabled by this header");
    assert_eq!(whole.header("Content-Length"), Some(whole.body.len().to_string().as_str()));

    // The single open-ended form `CURLOPT_RESUME_FROM_LARGE` emits, and the only one a Nix
    // client ever sends.
    let at = whole.body.len() / 2;
    let resumed = server
        .request(&format!("GET /{url} HTTP/1.1\r\nHost: t\r\nRange: bytes={at}-\r\n\r\n"), b"")
        .await;
    assert_eq!(resumed.status, 206);
    assert_eq!(
        resumed.header("Content-Range"),
        Some(format!("bytes {at}-{}/{}", whole.body.len() - 1, whole.body.len()).as_str())
    );
    assert_eq!(resumed.body, whole.body[at..], "the resumed body is the tail, not a restart");
    assert_eq!(resumed.header("Accept-Ranges"), Some("bytes"));

    let past = server
        .request(
            &format!(
                "GET /{url} HTTP/1.1\r\nHost: t\r\nRange: bytes={}-\r\n\r\n",
                whole.body.len()
            ),
            b"",
        )
        .await;
    assert_eq!(past.status, 416);
    assert_eq!(
        past.header("Content-Range"),
        Some(format!("bytes */{}", whole.body.len()).as_str())
    );
}

#[tokio::test]
async fn the_served_artifact_is_what_the_client_uploaded() {
    let server = Server::start("artifact").await;
    let published = publish(&server, &b"round trip payload".repeat(1024)).await;

    let served = server
        .request(&format!("GET /{}.narinfo HTTP/1.1\r\nHost: t\r\n\r\n", published.key), b"")
        .await;
    let parsed = fields(&served.body);
    let url = parsed["URL"][0].clone();

    let artifact = server.request(&format!("GET /{url} HTTP/1.1\r\nHost: t\r\n\r\n"), b"").await;
    assert_eq!(parsed["FileSize"][0], artifact.body.len().to_string());
    assert_eq!(
        parsed["FileHash"][0],
        bincache_core::hash::Sha256::digest(&artifact.body).to_string(),
        "FileHash names the bytes actually served"
    );

    let decompressed = zstd::stream::decode_all(artifact.body.as_slice()).expect("decompresses");
    assert_eq!(decompressed, published.nar);
    assert_eq!(
        bincache_core::hash::Sha256::digest(&decompressed).base32(),
        published.nar_hash32,
        "the decompressed bytes hash to the published NarHash"
    );
}

/// `BinaryCacheStore::addToStore` HEADs the NAR URL before uploading. bincache stores a
/// recompressed artifact under a different name, so this is answered from the NAR-hash
/// index; a 404 would make every build node re-upload every NAR forever.
#[tokio::test]
async fn head_on_an_uploaded_nar_url_reports_it_present() {
    let server = Server::start("nar-probe").await;
    let published = publish(&server, b"probe payload").await;

    let present = server
        .request(
            &format!("HEAD /nar/{}.nar HTTP/1.1\r\nHost: t\r\n\r\n", published.nar_hash32),
            b"",
        )
        .await;
    assert_eq!(present.status, 200);

    let absent = bincache_core::hash::Sha256::digest(b"never uploaded").base32();
    let missing =
        server.request(&format!("HEAD /nar/{absent}.nar HTTP/1.1\r\nHost: t\r\n\r\n"), b"").await;
    assert_eq!(missing.status, 404);
}

#[tokio::test]
async fn a_malformed_key_dies_at_the_socket() {
    let server = Server::start("malformed").await;
    for target in
        ["/abc.narinfo", "/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee.narinfo", "/nar/notahash.nar.zst"]
    {
        let response =
            server.request(&format!("GET {target} HTTP/1.1\r\nHost: t\r\n\r\n"), b"").await;
        assert_eq!(response.status, 400, "{target} should have been refused");
    }

    // Routes this cache does not serve are absent, not malformed.
    for target in ["/log/whatever.drv", "/5rnvz1n7hdmvbdzq0d5m5xrz3xz6ky8j.ls"] {
        let response =
            server.request(&format!("GET {target} HTTP/1.1\r\nHost: t\r\n\r\n"), b"").await;
        assert_eq!(response.status, 404, "{target} should have been a miss");
    }
}

#[tokio::test]
async fn a_miss_is_a_404_on_both_methods() {
    let server = Server::start("miss").await;
    let key = bincache_core::hash::Sha256::digest(b"absent").base32();
    let key = &key[..bincache_core::storepath::HASH_TEXT_LEN];
    for method in ["GET", "HEAD"] {
        let response = server
            .request(&format!("{method} /{key}.narinfo HTTP/1.1\r\nHost: t\r\n\r\n"), b"")
            .await;
        assert_eq!(response.status, 404);
    }
}

#[tokio::test]
async fn the_write_path_refuses_what_it_cannot_verify() {
    let server = Server::start("write-refusals").await;
    let body = nar(b"verified payload");
    let hash32 = bincache_core::hash::Sha256::digest(&body).base32();

    let unauthenticated = format!(
        "PUT /nar/{hash32}.nar HTTP/1.1\r\nHost: t\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    assert_eq!(server.request(&unauthenticated, &body).await.status, 401);

    let wrong = format!(
        "PUT /nar/{hash32}.nar HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer nope\r\n\
         Content-Length: {}\r\n\r\n",
        body.len()
    );
    assert_eq!(server.request(&wrong, &body).await.status, 401);

    // The target states what the body must hash to, so a mismatch is caught before
    // anything durable is named after it.
    let lying = server.authorized("PUT", &format!("/nar/{hash32}.nar"), 5);
    assert_eq!(server.request(&lying, b"wrong").await.status, 400);
    let probe = format!("HEAD /nar/{hash32}.nar HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(server.request(&probe, b"").await.status, 404, "nothing durable was left");

    // bincache compresses on receipt, so a pre-compressed upload is refused rather than
    // stored under a hash of bytes it cannot verify.
    let precompressed = server.authorized("PUT", &format!("/nar/{hash32}.nar.xz"), 2);
    assert_eq!(server.request(&precompressed, b"xz").await.status, 400);

    // A narinfo whose NAR was never uploaded has nothing to describe.
    let orphan = "StorePath: /nix/store/5rnvz1n7hdmvbdzq0d5m5xrz3xz6ky8j-orphan-1.0\n\
                  URL: nar/x.nar\nCompression: none\n\
                  FileHash: sha256:0mdqa9w1p6cmli6976v4wi0sw9r4p5prkj7lzfd1877wk11c9c73\n\
                  FileSize: 1\n\
                  NarHash: sha256:0mdqa9w1p6cmli6976v4wi0sw9r4p5prkj7lzfd1877wk11c9c73\n\
                  NarSize: 1\nReferences: \n";
    let head = server.authorized("PUT", "/5rnvz1n7hdmvbdzq0d5m5xrz3xz6ky8j.narinfo", orphan.len());
    assert_eq!(server.request(&head, orphan.as_bytes()).await.status, 400);
}

/// A refusal the pusher caused says why, on the wire.
///
/// The reason exists as the `Display` of a typed error either way. What this pins is that it
/// reaches the client rather than only the server log, because the log belongs to the
/// operator and the mistake belongs to whoever ran `nix copy`. The pre-compressed case is
/// the one that matters most: it is the only refusal a correct client hits by being
/// configured wrong rather than by being broken, and the fix is a URI setting nobody can
/// guess from a bare `400`.
#[tokio::test]
async fn a_client_fault_is_refused_with_its_reason() {
    let server = Server::start("refusal-bodies").await;
    let body = nar(b"refusal payload");
    let hash32 = bincache_core::hash::Sha256::digest(&body).base32();

    let precompressed = server.authorized("PUT", &format!("/nar/{hash32}.nar.zst"), 2);
    let refused = server.request(&precompressed, b"no").await;
    assert_eq!(refused.status, 400);
    let text = String::from_utf8(refused.body).expect("the refusal is utf8");
    assert!(text.contains("?compression=none"), "the refusal was {text:?}");

    // A hash mismatch names both hashes, so a build node's log says which artifact was
    // wrong rather than that something was.
    let lying = server.authorized("PUT", &format!("/nar/{hash32}.nar"), 5);
    let mismatch = server.request(&lying, b"wrong").await;
    assert_eq!(mismatch.status, 400);
    let text = String::from_utf8(mismatch.body).expect("the refusal is utf8");
    assert!(text.contains(&hash32), "the refusal was {text:?}");

    // Publishing before uploading is the ordering mistake a hand-rolled pusher makes.
    let orphan = "StorePath: /nix/store/5rnvz1n7hdmvbdzq0d5m5xrz3xz6ky8j-orphan-1.0\n\
                  URL: nar/x.nar\nCompression: none\n\
                  FileHash: sha256:0mdqa9w1p6cmli6976v4wi0sw9r4p5prkj7lzfd1877wk11c9c73\n\
                  FileSize: 1\n\
                  NarHash: sha256:0mdqa9w1p6cmli6976v4wi0sw9r4p5prkj7lzfd1877wk11c9c73\n\
                  NarSize: 1\nReferences: \n";
    let head = server.authorized("PUT", "/5rnvz1n7hdmvbdzq0d5m5xrz3xz6ky8j.narinfo", orphan.len());
    let unknown = server.request(&head, orphan.as_bytes()).await;
    assert_eq!(unknown.status, 400);
    let text = String::from_utf8(unknown.body).expect("the refusal is utf8");
    assert!(text.contains("has been uploaded"), "the refusal was {text:?}");
}

/// Content addressing plus an idempotent publish is what makes a client retry harmless,
/// which matters because Nix's own HTTP store has no locking.
#[tokio::test]
async fn pushing_the_same_path_twice_changes_nothing() {
    let server = Server::start("idempotent").await;
    let first = publish(&server, b"idempotent payload").await;
    let target = format!("GET /{}.narinfo HTTP/1.1\r\nHost: t\r\n\r\n", first.key);
    let before = server.request(&target, b"").await.body;

    let second = publish(&server, b"idempotent payload").await;
    assert_eq!(second.key, first.key);
    assert_eq!(server.request(&target, b"").await.body, before);
}

/// A closure query fires hundreds of lookups at once, and `http-connections` caps
/// connections rather than in-flight requests, so keep-alive is the difference between one
/// handshake and hundreds.
#[tokio::test]
async fn one_connection_serves_many_requests() {
    let server = Server::start("keep-alive").await;
    let published = publish(&server, b"keep-alive payload").await;

    let mut stream = server.connect().await;
    let hit = format!("GET /{}.narinfo HTTP/1.1\r\nHost: t\r\n\r\n", published.key);
    let miss = "GET /5rnvz1n7hdmvbdzq0d5m5xrz3xz6ky8j.narinfo HTTP/1.1\r\nHost: t\r\n\r\n";

    for _ in 0..4 {
        assert_eq!(exchange(&mut stream, &hit, b"").await.status, 200);
        assert_eq!(exchange(&mut stream, miss, b"").await.status, 404);
    }
}

/// A push is two requests per path, and a closure is hundreds of paths. Answering a
/// consumed body with a close would make `nix copy` reconnect for every one of them.
#[tokio::test]
async fn a_whole_push_fits_on_one_connection() {
    let server = Server::start("push-keep-alive").await;
    let mut stream = server.connect().await;

    for seed in [b"first path".as_slice(), b"second path".as_slice()] {
        let body = nar(seed);
        let nar_hash32 = bincache_core::hash::Sha256::digest(&body).base32();
        let digest = bincache_core::hash::Sha256::digest(seed);
        let mut raw = [0u8; bincache_core::storepath::HASH_WIDTH];
        raw.copy_from_slice(&digest.as_bytes()[..bincache_core::storepath::HASH_WIDTH]);
        let key = bincache_core::storepath::Hash::from_bytes(raw).to_string();

        let head = server.authorized("PUT", &format!("/nar/{nar_hash32}.nar"), body.len());
        assert_eq!(exchange(&mut stream, &head, &body).await.status, 201);

        let narinfo = format!(
            "StorePath: /nix/store/{key}-conformance-1.0\nURL: nar/{nar_hash32}.nar\n\
             Compression: none\nFileHash: sha256:{nar_hash32}\nFileSize: {}\n\
             NarHash: sha256:{nar_hash32}\nNarSize: {}\nReferences: \n",
            body.len(),
            body.len()
        );
        let head = server.authorized("PUT", &format!("/{key}.narinfo"), narinfo.len());
        assert_eq!(exchange(&mut stream, &head, narinfo.as_bytes()).await.status, 201);

        let get = format!("GET /{key}.narinfo HTTP/1.1\r\nHost: t\r\n\r\n");
        assert_eq!(exchange(&mut stream, &get, b"").await.status, 200);
    }
}

/// A request whose body is refused unread must not leave that body to be read as the next
/// request.
///
/// Closing is one way to guarantee it and draining is another, so the assertion is the
/// invariant rather than the mechanism: either the connection ends, or the follow-up gets
/// its own correct answer. What must never happen is a reply derived from the leftover
/// body.
#[tokio::test]
async fn a_refused_body_never_becomes_the_next_request() {
    let server = Server::start("refused-body").await;
    let body = nar(b"unauthorized payload");
    let hash32 = bincache_core::hash::Sha256::digest(&body).base32();
    let head = format!(
        "PUT /nar/{hash32}.nar HTTP/1.1\r\nHost: t\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );

    let mut stream = server.connect().await;
    let refused = exchange(&mut stream, &head, &body).await;
    assert_eq!(refused.status, 401);

    send(&mut stream, "GET /nix-cache-info HTTP/1.1\r\nHost: t\r\n\r\n", b"").await;
    if let Some(answered) = try_read_response(&mut stream, Body::Expected).await {
        assert_eq!(answered.status, 200, "the follow-up read the refused body");
    }
}

/// A refusal must land before the body does, which is the whole point of checking the
/// target and the credential first.
///
/// The largest refusal is a pre-compressed multi-gigabyte NAR. If the server drained the
/// upload before answering, a build node configured without `?compression=none` would push
/// the whole thing across the network to be told no. The `Content-Length` here promises a
/// body that is never sent, so an answer at all proves the server did not wait for it.
#[tokio::test]
async fn a_push_is_refused_without_reading_its_body() {
    let server = Server::start("early-refusal").await;
    let hash32 = bincache_core::hash::Sha256::digest(&nar(b"never sent")).base32();

    let mut stream = server.connect().await;
    let head = format!(
        "PUT /nar/{hash32}.nar.zst HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer {}\r\n\
         Content-Length: 10737418240\r\n\r\n",
        server.token
    );
    send(&mut stream, &head, b"").await;

    let answered = tokio::time::timeout(
        core::time::Duration::from_secs(5),
        read_response(&mut stream, Body::Expected),
    )
    .await
    .expect("the refusal must not wait for ten gigabytes that are never sent");

    assert_eq!(answered.status, 400);
    let text = String::from_utf8(answered.body).expect("the refusal is utf8");
    assert!(text.contains("?compression=none"), "the refusal was {text:?}");
}

#[tokio::test]
async fn a_connection_close_request_is_honoured() {
    let server = Server::start("close").await;
    let response = server
        .request("GET /nix-cache-info HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n", b"")
        .await;
    assert_eq!(response.header("Connection"), Some("close"));
}

/// RFC 9112 §6.1: when a message carries both `Transfer-Encoding` and `Content-Length`,
/// the framing is ambiguous, `Transfer-Encoding` overrides, and a server must answer `400`
/// and close. Two recipients that resolve the ambiguity differently disagree about where
/// this message ends and the next one begins, which is the whole of request smuggling.
///
/// Everything else about this upload is valid: the token is real and the body hashes to
/// what the target declares. The framing conflict is the only defect, so a `201` here means
/// the server resolved the ambiguity rather than refusing it.
#[tokio::test]
async fn a_conflicting_framing_is_refused_rather_than_resolved() {
    let server = Server::start("framing-conflict").await;
    let body = nar(b"smuggled payload");
    let hash32 = bincache_core::hash::Sha256::digest(&body).base32();

    let head = format!(
        "PUT /nar/{hash32}.nar HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer {}\r\n\
         Transfer-Encoding: chunked\r\nContent-Length: {}\r\n\r\n",
        server.token,
        body.len()
    );
    let response = server.request(&head, &body).await;
    assert_eq!(response.status, 400, "an ambiguously framed request must be refused");
    assert_eq!(response.header("Connection"), Some("close"), "and the connection must close");
}

/// A chunk-framed body must be consumed or the connection must close. Leaving it in the
/// socket means the next request on a kept-alive connection starts mid-message, and the
/// chunk data gets parsed as a request head.
///
/// The follow-up `GET` is the assertion. It is a well-formed request on a connection the
/// server said it would keep, so anything other than `200` means the server answered the
/// previous request's body instead.
#[tokio::test]
async fn a_chunked_body_does_not_desync_the_connection() {
    let server = Server::start("chunked").await;
    let body = nar(b"chunked payload");
    let hash32 = bincache_core::hash::Sha256::digest(&body).base32();

    let mut chunked = format!("{:x}\r\n", body.len()).into_bytes();
    chunked.extend_from_slice(&body);
    chunked.extend_from_slice(b"\r\n0\r\n\r\n");

    let head = format!(
        "PUT /nar/{hash32}.nar HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer {}\r\n\
         Transfer-Encoding: chunked\r\n\r\n",
        server.token
    );

    let mut stream = server.connect().await;
    let upload = exchange(&mut stream, &head, &chunked).await;
    if upload.header("Connection") == Some("close") {
        return;
    }

    send(&mut stream, "GET /nix-cache-info HTTP/1.1\r\nHost: t\r\n\r\n", b"").await;
    let answered = try_read_response(&mut stream, Body::Expected).await;
    let Some(answered) = answered else {
        panic!(
            "the upload answered {} with Connection: keep-alive, then closed: \
             the chunk-framed body was left in the socket and parsed as the next request",
            upload.status
        );
    };
    assert_eq!(
        answered.status, 200,
        "the connection was kept but the next request read the previous body"
    );
}

/// curl sets `Expect: 100-continue` on uploads past about a kilobyte and stalls for a
/// second if nothing answers.
#[tokio::test]
async fn an_expect_continue_upload_is_answered_before_the_body() {
    let server = Server::start("expect").await;
    let body = nar(&b"expecting payload".repeat(256));
    let hash32 = bincache_core::hash::Sha256::digest(&body).base32();

    let mut stream = server.connect().await;
    let head = format!(
        "PUT /nar/{hash32}.nar HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer {}\r\n\
         Expect: 100-continue\r\nContent-Length: {}\r\n\r\n",
        server.token,
        body.len()
    );
    tokio::io::AsyncWriteExt::write_all(&mut stream, head.as_bytes()).await.expect("writes");

    let interim = read_response(&mut stream, Body::Expected).await;
    assert_eq!(interim.status, 100, "the server invites the body before it is sent");

    tokio::io::AsyncWriteExt::write_all(&mut stream, &body).await.expect("writes the body");
    assert_eq!(read_response(&mut stream, Body::Expected).await.status, 201);
}

#[tokio::test]
async fn metrics_report_what_was_counted() {
    let server = Server::start("metrics").await;
    let published = publish(&server, b"metrics payload").await;
    server
        .request(&format!("GET /{}.narinfo HTTP/1.1\r\nHost: t\r\n\r\n", published.key), b"")
        .await;
    server
        .request("GET /5rnvz1n7hdmvbdzq0d5m5xrz3xz6ky8j.narinfo HTTP/1.1\r\nHost: t\r\n\r\n", b"")
        .await;

    let response = server.request("GET /metrics HTTP/1.1\r\nHost: t\r\n\r\n", b"").await;
    assert_eq!(response.status, 200);
    let body = String::from_utf8(response.body).expect("utf8");
    assert!(body.contains("bincache_metadata_hits_total 1"), "{body}");
    assert!(body.contains("bincache_metadata_misses_total 1"), "{body}");
    assert!(body.contains("bincache_uploads_total 1"), "{body}");
    assert!(body.contains("bincache_paths 1"), "{body}");
}
