//! One Redis per test binary (B4 T1), next to tests/common's one Postgres: `REDIS_URL` when set (CI's rust-library
//! job starts `redis_image` in dind and exports it), else a throwaway `redis:8.10.1-alpine` from the LAN mirror
//! (`TEST_PG_REGISTRY`, default `docker.io/library`), removed when the process exits.
//! Tests share the server and run in parallel, so every test works under its own key prefix ([`prefix`]): never
//! `FLUSHALL`, never `DBSIZE`, never a key without the prefix.
#![allow(dead_code)]

use std::time::{Duration, Instant};

use agent_api::runtime::bus::{Bus, ConsumeOptions, Envelope};
use agent_api::runtime::{Env, Scope};
use redis::streams::StreamRangeReply;
use testcontainers_modules::redis::{REDIS_PORT, Redis};
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use tokio::sync::OnceCell;

/// The mirrored image tag (same as CI's `redis_image`).
pub const REDIS_TAG: &str = "8.10.1-alpine";

struct Server {
    url: String,
    _container: Option<ContainerAsync<Redis>>,
}

static SERVER: OnceCell<Server> = OnceCell::const_new();
static CONTAINER_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Remove this process's own container at exit (statics are never dropped). Never a prefix match.
extern "C" fn remove_container() {
    if let Some(id) = CONTAINER_ID.get() {
        let _ = std::process::Command::new("docker")
            .args(["rm", "-f", "-v", id])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

async fn start_container() -> (String, ContainerAsync<Redis>) {
    let reg = match std::env::var("TEST_PG_REGISTRY") {
        Ok(r) if !r.is_empty() => r,
        _ => "docker.io/library".to_string(),
    };
    let image = format!("{}/redis", reg.trim_end_matches('/'));
    let mut last = String::new();
    for attempt in 1..=3 {
        match Redis::default()
            .with_name(&image)
            .with_tag(REDIS_TAG)
            .start()
            .await
        {
            Ok(c) => {
                let mut port = None;
                for _ in 0..40 {
                    match c.get_host_port_ipv4(REDIS_PORT).await {
                        Ok(p) => {
                            port = Some(p);
                            break;
                        }
                        Err(e) => last = format!("{e:?}"),
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                if let Some(port) = port {
                    return (format!("redis://127.0.0.1:{port}"), c);
                }
                eprintln!("redis container port never published (attempt {attempt}/3): {last}");
            }
            Err(e) => {
                last = format!("{e:?}");
                eprintln!("redis container start failed (attempt {attempt}/3): {last}");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
    panic!("could not start redis {REDIS_TAG} (is Docker running?): {last}");
}

async fn server() -> &'static Server {
    SERVER
        .get_or_init(|| async {
            if let Ok(url) = std::env::var("REDIS_URL")
                && !url.is_empty()
            {
                return Server {
                    url,
                    _container: None,
                };
            }
            let (url, c) = start_container().await;
            if CONTAINER_ID.set(c.id().to_string()).is_ok() {
                // SAFETY: registering a plain `extern "C" fn()` with atexit.
                unsafe { libc::atexit(remove_container) };
            }
            Server {
                url,
                _container: Some(c),
            }
        })
        .await
}

/// The shared server's URL.
pub async fn url() -> String {
    server().await.url.clone()
}

/// A fresh environment prefix for one test: `t:{test}:{uuid}:`.
pub fn prefix(test: &str) -> String {
    format!("t:{test}:{}:", uuid::Uuid::new_v4().simple())
}

/// The root (unscoped) bus under a fresh per-test prefix: registry, nudges, heartbeats.
pub async fn root(test: &str) -> Bus {
    // 5 s, not the redis crate's 500 ms default: under CI load a reply can exceed 500 ms and flake the test.
    Bus::connect_with_response_timeout(
        &url().await,
        &prefix(test),
        std::time::Duration::from_secs(5),
    )
    .await
    .expect("the test Redis is reachable")
}

/// The shared server's URL with its host and port replaced by `addr` (a proxy in front of it): the credentials and
/// database of `REDIS_URL` are kept, so a password-protected local test Redis answers through the proxy too.
pub async fn url_at(addr: std::net::SocketAddr) -> String {
    let url = url().await;
    let (scheme, rest) = url.split_once("://").unwrap_or(("redis", url.as_str()));
    let (auth, rest) = rest.rsplit_once('@').map_or(("", rest), |(a, r)| (a, r));
    let db = rest.split_once('/').map_or("", |(_, d)| d);
    let auth = if auth.is_empty() {
        String::new()
    } else {
        format!("{auth}@")
    };
    let db = if db.is_empty() {
        String::new()
    } else {
        format!("/{db}")
    };
    format!("{scheme}://{auth}{addr}{db}")
}

/// A loopback address nothing listens on (yet): a Redis that is down, until [`proxy_at`] brings it up.
pub async fn dead_addr() -> std::net::SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    drop(l);
    addr
}

/// Redis "comes up" at `addr`: a TCP proxy to the shared test server. Abort the handle to stop accepting.
pub async fn proxy_at(addr: std::net::SocketAddr) -> tokio::task::JoinHandle<()> {
    let url = url().await;
    // `redis://[user:pw@]host:port[/db]` → host:port
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let rest = rest.rsplit_once('@').map_or(rest, |(_, r)| r);
    let upstream = rest.split('/').next().unwrap_or(rest).to_string();
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    tokio::spawn(async move {
        while let Ok((mut inbound, _)) = listener.accept().await {
            let upstream = upstream.clone();
            tokio::spawn(async move {
                if let Ok(mut out) = tokio::net::TcpStream::connect(&upstream).await {
                    let _ = tokio::io::copy_bidirectional(&mut inbound, &mut out).await;
                }
            });
        }
    })
}

/// `REDIS_URL` + a fresh `AGENT_PREFIX`: the variables that point an agent-api test environment
/// (`common::env_with(|v| v.extend(vars.clone()))`) at the shared Redis. The prefix is `vars[1].1`.
pub async fn api_vars(test: &str) -> [(String, String); 2] {
    [
        ("REDIS_URL".to_string(), url().await),
        ("AGENT_PREFIX".to_string(), prefix(test)),
    ]
}

pub const BOOT_TENANT: &str = "0195f48b-d000-7000-8000-000000000099";
pub const OTHER_TENANT: &str = "0195f48b-d000-7000-8000-0000000000aa";

/// The bootstrap tenant's `customer_care_agent` job, prod.
pub fn scope_a() -> Scope {
    Scope::parse(&format!("{BOOT_TENANT}/customer_care_agent/prod")).unwrap()
}

/// Another tenant, same job key, prod.
pub fn scope_b() -> Scope {
    Scope::parse(&format!("{OTHER_TENANT}/customer_care_agent/prod")).unwrap()
}

/// `scope_a` in the test environment.
pub fn scope_a_test() -> Scope {
    Scope {
        env: Env::Test,
        ..scope_a()
    }
}

/// `root(test)` scoped to `scope_a()`: what most bus tests run on.
pub async fn bus(test: &str) -> Bus {
    root(test).await.scoped(&scope_a())
}

/// Fast polling for tests.
pub fn fast() -> ConsumeOptions {
    ConsumeOptions {
        batch: 16,
        block: Duration::from_millis(100),
        reclaim_idle: Duration::from_millis(300),
        max_deliveries: 3,
    }
}

/// Poll `f` every 50 ms until it holds; panic after 5 s.
pub async fn eventually(f: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !f() {
        assert!(Instant::now() < deadline, "condition not met within 5 s");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Every envelope on `stream`, oldest first.
pub async fn entries(bus: &Bus, stream: &str) -> Vec<Envelope> {
    let reply: StreamRangeReply = redis::cmd("XRANGE")
        .arg(stream)
        .arg("-")
        .arg("+")
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    reply
        .ids
        .iter()
        .map(|e| serde_json::from_str(&e.get::<String>("envelope").unwrap()).unwrap())
        .collect()
}

/// Poll until `n` entries on `stream` match `pred` (10 s limit).
pub async fn wait_entries(
    bus: &Bus,
    stream: &str,
    n: usize,
    pred: impl Fn(&Envelope) -> bool,
) -> Vec<Envelope> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let found: Vec<Envelope> = entries(bus, stream)
            .await
            .into_iter()
            .filter(|e| pred(e))
            .collect();
        if found.len() >= n {
            return found;
        }
        assert!(
            Instant::now() < deadline,
            "only {} of {n} entries on {stream}",
            found.len()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
