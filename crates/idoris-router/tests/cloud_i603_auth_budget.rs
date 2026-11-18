//! 计算服务 issue #603 的 N4：公网入口上「一个来源就能让所有租户的冷凭证被拒」的可用性问题。
//!
//! 认证之前的有界工作（在途个数 + 全局未知前缀预算）原来只有一个全局的桶：一个来源用随机 `idk_` key 刷，10 秒就把它耗光，
//! 之后所有冷的合法 key（重启之后、新签发、被挤出「已知」表）全拿 429。现在每个来源有自己的份额，先扣它、再扣全局总闸。
//! 整条路径是真的：HTTP -> 云入口（边缘解析客户端来源、认证之前的有界工作、认证）-> 账户库 -> 假上游（`127.0.0.1`）。
//! 不连接任何真实的推理或安全服务。每个场景在修复之前的代码上都会失败。
//!
//! | 场景 | 断言 |
//! |---|---|
//! | issue 复现（默认配置） | 一个来源刷 10 秒随机 key，100 个冷的合法 key（各自来源）一个都不 429 |
//! | 来源 A 刷随机 key | 来源 B 的冷的合法 key 照常通过；A 自己被挡在存储之前 |
//! | 数据库查询次数的上界 | 不论多少个来源，走到认证存储的查询数不超过全局预算；预算用完后不碰库 |
//! | 被自己份额挡住的来源 | 不消耗全局预算（全局总闸仍然是总闸） |
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/ingress_harness.rs"]
mod h;

use std::time::Duration;

use h::*;
use serde_json::json;

/// 让「没有凭证」的软配额不碍事：这里要看的 429 一定是认证之前的有界工作给的。
const ROOMY_ANONYMOUS: &str = "  anonymous: {requests_per_minute: 600000, burst: 100000}\n  per_source_anonymous: {requests_per_minute: 600000, burst: 100000}\n  auth_failures: {max_failures: 100000, window_secs: 60}\n";

/// 在另一个线程里占着虚拟 key 存储的锁：认证查询会卡在 `spawn_blocking` 里等这把锁，
/// 所以「被拒绝的请求没有碰库」可以用「库被占着也立刻返回」来证明。
struct StoreLock {
    release: std::sync::mpsc::Sender<()>,
    thread: std::thread::JoinHandle<()>,
}

impl StoreLock {
    fn hold(w: &World) -> Self {
        let keys = w.keys.clone();
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            let _guard = keys.lock().unwrap();
            locked_tx.send(()).unwrap();
            let _ = release_rx.recv();
        });
        locked_rx.recv().unwrap();
        Self { release, thread }
    }

    fn release(self) {
        let _ = self.release.send(());
        self.thread.join().unwrap();
    }
}

// ---------------------------------------------------------------------------
// N4：认证之前的有界工作按来源分份额
// ---------------------------------------------------------------------------

/// 一个格式合法、前缀各不相同的随机凭证，从 `source` 发来。
fn random_key_from(w: &World, source: &str, seed: u32) -> Req {
    Req::chat(w.cred("svc_eduai"))
        .bearer_raw(Some(&unknown_secret(seed)))
        .member("kid-1")
        .peer(source)
}

/// 一个来源刷随机凭证，数一数有几个走到了认证存储（401）、几个在存储之前被拒（429 `auth_capacity_limited`）。
async fn flood(w: &World, source: &str, seeds: std::ops::Range<u32>) -> (usize, usize) {
    let (mut reached_the_store, mut refused_before_it) = (0, 0);
    for seed in seeds {
        let reply = w.send(&random_key_from(w, source, seed)).await;
        match reply.status.as_u16() {
            401 => reached_the_store += 1,
            429 => {
                assert_eq!(reply.code(), "auth_capacity_limited", "{source} {seed}");
                refused_before_it += 1;
            }
            other => panic!("{source} {seed}: unexpected {other}: {}", reply.text()),
        }
    }
    (reached_the_store, refused_before_it)
}

/// 用真实的运维 bootstrap 给 `account` 签发 `count` 张 Service 凭证，返回明文密钥。它们在这个世界里都是「冷」的：
/// 从来没有认证成功过，所以认证之前每一张都要从未知前缀预算里取一个令牌。
fn issue_cold_service_credentials(w: &World, account: &str, count: usize) -> Vec<String> {
    struct Adapter(std::sync::Arc<idoris_router::cloud::ManualClock>);
    impl idoris_router::control::ControlClock for Adapter {
        fn now(&self) -> idoris_compute_contracts::ids::TimestampMs {
            idoris_router::cloud::Clock::now(self.0.as_ref())
        }
    }
    let one = json!({
        "binding": {"kind": "service", "account": account},
        "admin_scopes": [],
        "owner": account,
        "tenant_id": account,
        "allowed_privacy": ["local_only"],
        "allowed_roles": ["fast"],
    });
    let spec = json!({"credentials": vec![one; count]});
    let mut out = Vec::new();
    idoris_router::control::bootstrap::run(
        &w.accounts,
        &mut spec.to_string().as_bytes(),
        &mut out,
        &Adapter(w.clock.clone()),
    )
    .unwrap();
    let secrets: Vec<String> = String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|line| line["kind"] == "credential")
        .map(|line| line["secret"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(secrets.len(), count);
    secrets
}

/// issue #603 N4 的复现，用默认配置：一个来源用随机 `idk_` key 刷 10 秒（每秒 100 个），之后 100 个冷的合法 key
/// （分别从 100 个不同的来源来）一个都不该拿 429。修复之前：全局预算 200 个突发令牌 + 每秒 20 个补充，10 秒内被一个来源耗光，
/// 100/100 个冷的合法 key 拿 429。
#[tokio::test]
async fn the_issue_scenario_a_hundred_cold_valid_keys_survive_ten_seconds_of_random_key_flooding() {
    let w =
        World::new(WorldOptions::plenty().ingress(IngressYaml::default().abuse(ROOMY_ANONYMOUS)))
            .await;
    let cold = issue_cold_service_credentials(&w, "eduai", 100);
    let mut reached_the_store = 0;
    for seed in 0..1_000 {
        w.clock.advance_ms(10);
        let reply = w.send(&random_key_from(&w, "198.51.100.1", seed)).await;
        if reply.status == 401 {
            reached_the_store += 1;
        } else {
            assert_eq!(reply.status, 429, "{seed}: {}", reply.text());
        }
    }
    assert!(
        reached_the_store <= 41,
        "one source's own share is its burst plus its refill over the ten seconds, not the whole global budget: {reached_the_store}"
    );
    let mut served = 0;
    for (index, secret) in cold.iter().enumerate() {
        let request = Req::chat(w.cred("svc_eduai"))
            .bearer_raw(Some(secret))
            .member("kid-1")
            .peer(&format!("203.0.113.{}", index + 1));
        let reply = w.send(&request).await;
        if reply.status == 200 {
            served += 1;
        } else {
            panic!("cold key {index}: {} {}", reply.status, reply.text());
        }
    }
    assert_eq!(served, 100);
}

#[tokio::test]
async fn one_source_flooding_random_keys_cannot_starve_the_cold_valid_keys_of_other_sources() {
    // 全局的未知前缀预算：突发 100。冷的合法凭证（刚签发、重启之后、被挤出「已知」表）每个都要从里面取一个令牌。
    let w = World::new(
        WorldOptions::plenty().ingress(IngressYaml::default().abuse(&format!(
            "{ROOMY_ANONYMOUS}  auth_unknown: {{requests_per_minute: 600, burst: 100}}\n"
        ))),
    )
    .await;
    // 来源 A 刷 300 个随机 key：修复之前它一个人把全局的 100 个令牌耗光（100 个走到存储，200 个 429）
    let (reached, refused) = flood(&w, "198.51.100.1", 0..300).await;
    assert_eq!(reached + refused, 300);
    assert!(
        reached < 100,
        "one source must not be able to spend the whole global budget (it reached the store {reached} times)"
    );
    // 来源 B 的冷的合法 key（这个世界里还没有任何凭证认证成功过）：照常通过，一个都不拿 429
    for (credential, member) in [
        ("svc_eduai", Some("kid-1")),
        ("mem_kid1", None),
        ("mem_agent1", None),
    ] {
        let mut request = Req::chat(w.cred(credential)).peer("198.51.100.2");
        if let Some(member) = member {
            request = request.member(member);
        }
        let reply = w.send(&request).await;
        assert_eq!(
            reply.status,
            200,
            "{credential}: a cold valid key from another source was refused: {}",
            reply.text()
        );
    }
    // 吵闹的来源自己仍然被挡在存储之前：份额是它自己的
    let (reached_again, refused_again) = flood(&w, "198.51.100.1", 1_000..1_050).await;
    assert_eq!((reached_again, refused_again), (0, 50));
}

#[tokio::test]
async fn however_many_sources_flood_the_store_never_sees_more_queries_than_the_global_budget_allows()
 {
    // 全局 40、单来源 15（写出来，让这条测试不依赖默认值）。五个来源各刷 60 个随机 key。
    let w = World::new(
        WorldOptions::plenty().ingress(IngressYaml::default().abuse(&format!(
            "{ROOMY_ANONYMOUS}  auth_unknown: {{requests_per_minute: 60, burst: 40}}\n  per_source_auth_unknown: {{requests_per_minute: 60, burst: 15}}\n"
        ))),
    )
    .await;
    let mut per_source = Vec::new();
    for (index, source) in [
        "198.51.100.1",
        "198.51.100.2",
        "198.51.100.3",
        "198.51.100.4",
        "198.51.100.5",
    ]
    .into_iter()
    .enumerate()
    {
        let first = u32::try_from(index).unwrap() * 1_000;
        per_source.push(flood(&w, source, first..first + 60).await.0);
    }
    // 每个来源最多 15 个走到存储；总数最多 40（全局的突发）。前两个来源用满自己的 15，第三个拿到剩下的 10
    assert_eq!(per_source, vec![15, 15, 10, 0, 0], "{per_source:?}");
    assert_eq!(per_source.iter().sum::<usize>(), 40);
    // 全局预算用完之后，不管是哪个来源，请求都在存储之前被拒：库被占着也立刻返回
    let lock = StoreLock::hold(&w);
    for (index, source) in ["198.51.100.1", "198.51.100.9", "203.0.113.77"]
        .into_iter()
        .enumerate()
    {
        let seed = 50_000 + u32::try_from(index).unwrap();
        let reply = tokio::time::timeout(
            Duration::from_millis(1_500),
            w.send(&random_key_from(&w, source, seed)),
        )
        .await
        .expect("the request must be refused without touching the (locked) authentication store");
        assert_eq!(
            (reply.status.as_u16(), reply.code().as_str()),
            (429, "auth_capacity_limited")
        );
        assert!(reply.header("retry-after").is_some());
    }
    lock.release();
    assert_eq!(w.upstream.request_count(), 0);
}

#[tokio::test]
async fn a_source_refused_by_its_own_share_never_spends_the_global_budget() {
    // 全局只有 3 个令牌，单来源 2 个。来源 A 刷 50 个：只有 2 个走到存储（用掉 2 个全局令牌），其余 48 个被它自己的份额挡住，
    // 没有碰全局预算——全局正好还剩 1 个：B 的冷的合法 key 拿到它，之后 C 的就没有了（全局预算是真的总闸，不是摆设）。
    let w = World::new(
        WorldOptions::plenty().ingress(IngressYaml::default().abuse(&format!(
            "{ROOMY_ANONYMOUS}  auth_unknown: {{requests_per_minute: 60, burst: 3}}\n  per_source_auth_unknown: {{requests_per_minute: 60, burst: 2}}\n"
        ))),
    )
    .await;
    assert_eq!(flood(&w, "198.51.100.1", 0..50).await, (2, 48));
    let b = w
        .send(
            &Req::chat(w.cred("svc_eduai"))
                .member("kid-1")
                .peer("198.51.100.2"),
        )
        .await;
    assert_eq!(b.status, 200, "{}", b.text());
    let c = w
        .send(&Req::chat(w.cred("mem_kid1")).peer("198.51.100.3"))
        .await;
    assert_eq!(
        (c.status.as_u16(), c.code().as_str()),
        (429, "auth_capacity_limited"),
        "the global budget is the shared gate in front of the database"
    );
}
