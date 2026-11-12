//! 从凭证推导调用方（计算服务 S5，设计稿 §3、§9「数据面的租户」）：付款上下文、租户、数据处理要求。
//!
//! # 请求自己什么都不能指定
//!
//! 设计稿 §3：「一次请求的付款上下文由**凭证**决定，再加上开通方提交的、经过校验的成员 id。请求本身不能自己指定由谁付款。」
//! 云入口从一张认证过的凭证推导出这个请求的**全部**调用方信息：
//!
//! | 要什么 | 从哪里来 | 请求能影响吗 |
//! |---|---|---|
//! | 付款账户（payer） | [`AccountStore::payment_context`]：凭证的绑定，加上（只对 Service 凭证）`X-iDoris-Member` 里的成员 id，**每次请求逐次验证、不缓存** | 只有那个成员 id，而且它必须在这张凭证的范围内，否则拒绝 |
//! | 租户 | payer 在账户树里的祖先链：平台根之下的**顶层客户**（跳过平台根和补贴池） | 不能。`X-iDoris-Tenant` 在入口最外层就被删掉，云入口不读、不信 |
//! | 数据类别（是不是 `child_edu`）、留存、区域、DPA | 租户在云入口配置里的声明（`config`） | 不能 |
//! | 计费性质（付费 / 补贴 / 不计费） | 同上 | 不能 |
//! | 各级上限 | 租户的声明，没写就用默认 | 不能 |
//!
//! 租户用 payer 的祖先链推导、不用凭证绑定的账户，是故意的：一张 Service 凭证（eduai 后端）绑在 `eduai` 或者它下面的
//! 某所学校，断言的成员在它范围内的任何位置，payer 都落在成员自己的账户上；租户永远是这个 payer 往上数的顶层客户。
//! 凭证没有办法「选」一个租户。
//!
//! # 失败就是失败，不回落
//!
//! 成员 id 缺失、越界、对不该带成员 id 的凭证带了成员 id、同一个主体在范围内对应多个账户、主体被撤销、成员关系被撤销、
//! 账户被冻结……全部拒绝，付款上下文解析不出来就不继续。**不存在**「成员校验失败时退回凭证自己的账户」这条路：
//! [`PaymentContext`] 只能由契约的 `resolve_payment_context` 产生（字段私有，没有公开构造函数），这里没有任何办法
//! 凭空造一个。
//!
//! # 哪些拒绝是 401，哪些是 403
//!
//! - 凭证本身的状态（不认识、撤销、冻结、过期）：**同一个不透明的 401**，不告诉调用方是哪一种（和本地数据面、数据面闸门一致）。
//!   不在账户库里的密钥（本地 `idoris key issue` 签发的数据密钥）同理：它没有付款上下文，在云上不能用。
//! - 凭证有效，但这一次请求的付款上下文建立不了（成员 id 的问题、主体 / 成员关系被撤销、账户被冻结）：403，
//!   带 S0 的稳定原因码（`payment_context_invalid`、`principal_revoked`、`membership_revoked`、`account_inactive`）。
//!   eduai 后端需要这个区别：「这个孩子的同意撤回了」和「后端发了个错的成员 id」要给不同的处理。
//! - 租户没有在配置里声明：403 `tenant_not_provisioned`。
//! - 账户库读不出来：503，失败即关闭。

use std::sync::Arc;

use idoris_compute_contracts::account::{Account, AccountKind};
use idoris_compute_contracts::credential::ResolveError;
use idoris_compute_contracts::ids::{AccountId, CredentialId, PrincipalId, TimestampMs};
use idoris_compute_contracts::reason::ReasonCode;
use idoris_tenancy::accounts::{AccountStore, PaymentContextError};

use super::config::IngressConfig;
use super::limits::{CallerLimits, LimitKeys};
use crate::cloud::CallerPolicy;

/// 云入口给下游（S4 的处理函数、S6 的儿童安全、S7 的云端档）的调用方信息，放在请求扩展里。
///
/// 数据类别在 `policy.tenant`（S0 的 `TenantDataPolicy`）：`policy.tenant.handles_child_data()` 就是「这是不是
/// `child_edu` 租户」，`policy.tenant.data_classes()` 是完整的类别集合。S6 读它，不要自己再推一遍。
#[derive(Debug, Clone)]
pub struct IngressContext {
    /// 推导出来的租户（账户树里平台根之下的顶层客户）。
    pub tenant: AccountId,
    /// 提交了成员 id 的 Service 凭证、或者成员凭证自己的主体。节点凭证没有。
    pub principal: Option<PrincipalId>,
    /// 这次请求用的凭证。
    pub credential: CredentialId,
}

/// 解析出来的调用方：交给 S4 处理函数的 [`CallerPolicy`]，加上入口自己要用的东西。
#[derive(Debug, Clone)]
pub struct ResolvedCaller {
    pub policy: CallerPolicy,
    pub context: IngressContext,
    /// 三级上限里租户级和成员级的限额（租户自己声明的，或默认）。
    pub limits: CallerLimits,
    /// 三级上限的键。
    pub keys: LimitKeys,
}

/// 解析失败。HTTP 映射见模块文档。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// 凭证本身不能用：不透明的 401。
    Unauthorized,
    /// 付款上下文建立不了（S0 的稳定原因码）：403。
    Payment(ReasonCode),
    /// 租户没有声明：403。
    TenantNotProvisioned,
    /// 账户库读不出来：503。
    Unavailable,
}

/// 租户 = 付款账户的祖先链（`[自己, 父, ..., 根]`）里，离根最近的、既不是平台根也不是补贴池的那个账户。
/// 链里除了根和补贴池什么都没有（不可能：付款账户本身不是根也不是池）时返回 `None`。
pub fn derive_tenant(chain: &[Account]) -> Option<&Account> {
    chain.iter().rev().find(|account| {
        !matches!(
            account.kind,
            AccountKind::Platform | AccountKind::SubsidyPool
        )
    })
}

/// 凭证 -> 调用方。
pub struct CallerResolver {
    accounts: Arc<AccountStore>,
    config: Arc<IngressConfig>,
}

impl CallerResolver {
    pub fn new(accounts: Arc<AccountStore>, config: Arc<IngressConfig>) -> Self {
        Self { accounts, config }
    }

    /// 解析这一次请求的调用方。**同步、会读库**（账户存储是 SQLite）：异步调用方要放进 `spawn_blocking`。
    /// `member` 是请求里 `X-iDoris-Member` 提交的成员 id（已经过格式校验，还没有验证它属于这张凭证）。
    pub fn resolve(
        &self,
        credential: &CredentialId,
        member: Option<&PrincipalId>,
        now: TimestampMs,
    ) -> Result<ResolvedCaller, Refusal> {
        // 1. 付款上下文：S1/S2 的逐次验证，不缓存。失败一律拒绝，没有「退回凭证自己的账户」。
        let payment = match self.accounts.payment_context(credential, member, now) {
            Ok(payment) => payment,
            Err(PaymentContextError::UnknownCredential(_)) => return Err(Refusal::Unauthorized),
            Err(PaymentContextError::Resolve(error)) => return Err(refusal_for(&error)),
            Err(PaymentContextError::Store(error)) => {
                eprintln!("[idoris] cloud ingress: could not resolve the payment context: {error}");
                return Err(Refusal::Unavailable);
            }
        };
        // 云形态只有货币账户（THB / USD）；配额（QUOTA）是组织形态的东西，出现在云库里说明库不对，失败即关闭。
        if !payment.unit().is_currency() {
            return Err(Refusal::Payment(ReasonCode::PaymentContextInvalid));
        }

        // 2. 租户：从 payer 的祖先链推导（请求没有任何办法影响它）。
        let chain = match self.accounts.ancestors(payment.payer()) {
            Ok(chain) => chain,
            Err(error) => {
                eprintln!("[idoris] cloud ingress: could not read the account chain: {error}");
                return Err(Refusal::Unavailable);
            }
        };
        let Some(tenant) = derive_tenant(&chain) else {
            return Err(Refusal::Unavailable);
        };

        // 3. 数据处理要求：租户在配置里的声明。没有声明就不处理。
        let Some(profile) = self.config.tenant(&tenant.id) else {
            return Err(Refusal::TenantNotProvisioned);
        };

        let principal = payment.principal().cloned();
        let member_key = match &principal {
            Some(principal) => format!("p:{principal}"),
            None => format!("c:{credential}"),
        };
        Ok(ResolvedCaller {
            context: IngressContext {
                tenant: tenant.id.clone(),
                principal,
                credential: credential.clone(),
            },
            limits: profile.limits,
            keys: LimitKeys {
                tenant: tenant.id.to_string(),
                member: member_key,
            },
            policy: CallerPolicy {
                payment,
                tenant: profile.data.clone(),
                billing: profile.billing,
            },
        })
    }
}

/// S0 的解析错误 -> 入口的拒绝。
fn refusal_for(error: &ResolveError) -> Refusal {
    match error {
        ResolveError::CredentialRevoked(_)
        | ResolveError::CredentialFrozen(_)
        | ResolveError::CredentialExpired { .. } => Refusal::Unauthorized,
        other => Refusal::Payment(other.reason_code()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use idoris_compute_contracts::account::{
        AccountStatus, Membership, MembershipRole, MembershipStatus,
    };
    use idoris_compute_contracts::credential::{Credential, CredentialBinding, CredentialStatus};
    use idoris_compute_contracts::ids::MembershipId;
    use idoris_compute_contracts::money::Unit;
    use idoris_compute_contracts::permit::Billing;

    use super::*;

    const NOW: TimestampMs = TimestampMs(1_794_500_000_000);

    fn aid(text: &str) -> AccountId {
        AccountId::new(text).unwrap()
    }
    fn pid(text: &str) -> PrincipalId {
        PrincipalId::new(text).unwrap()
    }
    fn cid(text: &str) -> CredentialId {
        CredentialId::new(text).unwrap()
    }

    fn account(id: &str, kind: AccountKind, parent: Option<&str>) -> Account {
        Account {
            id: aid(id),
            kind,
            parent: parent.map(aid),
            unit: Unit::Thb,
            status: AccountStatus::Active,
        }
    }

    struct World {
        _dir: tempfile::TempDir,
        accounts: Arc<AccountStore>,
        resolver: CallerResolver,
    }

    const CONFIG: &str = "schema_version: 1
tenants:
  - account: eduai
    data_classes: [child_edu]
    max_retention: zdr
    require_dpa: true
    billing: subsidized
  - account: agent24
    data_classes: [adult]
    max_retention: with_exceptions
    billing: paid
    limits:
      member: {requests_per_minute: 7, burst: 3, max_concurrent: 1}
";

    /// platform -> pool -> eduai(Org) -> school-a(Org) -> {teacher(Sponsor), s1(Member)}; platform -> agent24(Org) -> a1(Member);
    /// platform -> stray(Org，没有在配置里声明)；platform -> solo（个人云用户直接挂平台）
    fn world() -> World {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(AccountStore::open(dir.path().join("a.sqlite3")).unwrap());
        for a in [
            account("platform", AccountKind::Platform, None),
            account("pool", AccountKind::SubsidyPool, Some("platform")),
            account("eduai", AccountKind::Org, Some("pool")),
            account("school-a", AccountKind::Org, Some("eduai")),
            account("teacher", AccountKind::Sponsor, Some("school-a")),
            account("s1", AccountKind::Member, Some("school-a")),
            account("school-b", AccountKind::Org, Some("eduai")),
            account("s2", AccountKind::Member, Some("school-b")),
            account("agent24", AccountKind::Org, Some("platform")),
            account("a1", AccountKind::Member, Some("agent24")),
            account("stray", AccountKind::Org, Some("platform")),
            account("stray-m", AccountKind::Member, Some("stray")),
            account("solo", AccountKind::Member, Some("platform")),
        ] {
            store.create_account(&a, NOW).unwrap();
        }
        for p in ["kid-1", "kid-2", "agent-1", "stray-1", "solo-1", "both"] {
            store.ensure_principal(&pid(p), NOW).unwrap();
        }
        let member = |id: &str, principal: &str, org: &str, account: &str| Membership {
            id: MembershipId::new(id).unwrap(),
            principal: pid(principal),
            org: aid(org),
            role: MembershipRole::Member,
            account: Some(aid(account)),
            status: MembershipStatus::Active,
        };
        for m in [
            member("m-kid-1", "kid-1", "school-a", "s1"),
            member("m-kid-2", "kid-2", "school-b", "s2"),
            member("m-agent-1", "agent-1", "agent24", "a1"),
            member("m-stray-1", "stray-1", "stray", "stray-m"),
            member("m-solo-1", "solo-1", "platform", "solo"),
        ] {
            store.create_membership(&m, NOW).unwrap();
        }
        let credential = |id: &str, binding: CredentialBinding| {
            Credential::new(cid(id), binding, CredentialStatus::Active, None, vec![]).unwrap()
        };
        for c in [
            credential(
                "svc-eduai",
                CredentialBinding::Service {
                    account: aid("eduai"),
                },
            ),
            credential(
                "svc-school-a",
                CredentialBinding::Service {
                    account: aid("school-a"),
                },
            ),
            credential(
                "svc-agent24",
                CredentialBinding::Service {
                    account: aid("agent24"),
                },
            ),
            credential(
                "svc-stray",
                CredentialBinding::Service {
                    account: aid("stray"),
                },
            ),
            credential(
                "node-agent24",
                CredentialBinding::Node {
                    account: aid("agent24"),
                },
            ),
            credential(
                "mem-kid-1",
                CredentialBinding::Member {
                    membership: MembershipId::new("m-kid-1").unwrap(),
                },
            ),
            credential(
                "mem-solo",
                CredentialBinding::Member {
                    membership: MembershipId::new("m-solo-1").unwrap(),
                },
            ),
        ] {
            store.create_credential(&c, NOW).unwrap();
        }
        let config = Arc::new(IngressConfig::parse(CONFIG).unwrap());
        World {
            _dir: dir,
            resolver: CallerResolver::new(store.clone(), config),
            accounts: store,
        }
    }

    fn resolve(
        w: &World,
        credential: &str,
        member: Option<&str>,
    ) -> Result<ResolvedCaller, Refusal> {
        let member = member.map(pid);
        w.resolver.resolve(&cid(credential), member.as_ref(), NOW)
    }

    #[test]
    fn the_tenant_is_the_top_level_customer_above_the_payer() {
        let w = world();
        let chain = |id: &str| w.accounts.ancestors(&aid(id)).unwrap();
        for (payer, tenant) in [
            ("s1", "eduai"), // platform -> pool -> eduai -> school-a -> s1：跳过根和池，取最靠近根的客户
            ("teacher", "eduai"),
            ("school-a", "eduai"),
            ("eduai", "eduai"),
            ("a1", "agent24"),
            ("solo", "solo"), // 个人云用户直接挂平台：自己就是自己的租户
        ] {
            let chain = chain(payer);
            assert_eq!(
                derive_tenant(&chain).unwrap().id.as_str(),
                tenant,
                "{payer}"
            );
        }
        // 只有根和池的链（不可能出现，但函数要有确定的答案）
        let only_roots = [
            account("pool", AccountKind::SubsidyPool, Some("platform")),
            account("platform", AccountKind::Platform, None),
        ];
        assert!(derive_tenant(&only_roots).is_none());
    }

    #[test]
    fn a_service_credential_resolves_the_asserted_member_and_carries_the_tenants_policy() {
        let w = world();
        let caller = resolve(&w, "svc-eduai", Some("kid-1")).unwrap();
        assert_eq!(caller.policy.payment.payer().as_str(), "s1");
        assert_eq!(caller.policy.payment.credential().as_str(), "svc-eduai");
        assert_eq!(caller.policy.payment.principal().unwrap().as_str(), "kid-1");
        assert_eq!(caller.context.tenant.as_str(), "eduai");
        assert_eq!(caller.context.principal.as_ref().unwrap().as_str(), "kid-1");
        // 数据类别来自租户的声明，不是请求
        assert!(caller.policy.tenant.handles_child_data());
        assert_eq!(caller.policy.billing, Billing::Subsidized);
        assert_eq!(caller.keys.tenant, "eduai");
        assert_eq!(caller.keys.member, "p:kid-1");
        // 范围更窄的凭证（只绑在 school-a）：kid-1 在范围内，租户仍然是顶层的 eduai
        let narrow = resolve(&w, "svc-school-a", Some("kid-1")).unwrap();
        assert_eq!(narrow.policy.payment.payer().as_str(), "s1");
        assert_eq!(narrow.context.tenant.as_str(), "eduai");
        // 另一个租户的声明不同
        let agent = resolve(&w, "svc-agent24", Some("agent-1")).unwrap();
        assert!(!agent.policy.tenant.handles_child_data());
        assert_eq!(agent.policy.billing, Billing::Paid);
        assert_eq!(agent.limits.member.rate.requests_per_minute(), 7);
    }

    #[test]
    fn member_and_node_credentials_resolve_without_a_member_id() {
        let w = world();
        let member = resolve(&w, "mem-kid-1", None).unwrap();
        assert_eq!(member.policy.payment.payer().as_str(), "s1");
        assert_eq!(member.keys.member, "p:kid-1");
        // 带上自己的主体也行
        assert!(resolve(&w, "mem-kid-1", Some("kid-1")).is_ok());
        // 节点凭证：payer 就是绑定的组织；没有主体，成员键用凭证 id
        let node = resolve(&w, "node-agent24", None).unwrap();
        assert_eq!(node.policy.payment.payer().as_str(), "agent24");
        assert_eq!(node.keys.member, "c:node-agent24");
        assert!(node.context.principal.is_none());
        // 个人云用户直接挂平台：租户是它自己，但配置里没声明 -> 拒绝
        assert_eq!(
            resolve(&w, "mem-solo", None).unwrap_err(),
            Refusal::TenantNotProvisioned
        );
    }

    #[test]
    fn every_way_to_name_someone_else_is_refused_and_none_falls_back_to_the_credentials_own_account()
     {
        let w = world();
        let payment_invalid = Refusal::Payment(ReasonCode::PaymentContextInvalid);
        // Service 凭证不带成员 id
        assert_eq!(resolve(&w, "svc-eduai", None).unwrap_err(), payment_invalid);
        // 断言一个不在范围内的成员：另一个租户的、范围外学校的、不存在的
        assert_eq!(
            resolve(&w, "svc-eduai", Some("agent-1")).unwrap_err(),
            payment_invalid
        );
        assert_eq!(
            resolve(&w, "svc-school-a", Some("kid-2")).unwrap_err(),
            payment_invalid
        );
        assert_eq!(
            resolve(&w, "svc-eduai", Some("nobody")).unwrap_err(),
            payment_invalid
        );
        // 节点凭证带成员 id
        assert_eq!(
            resolve(&w, "node-agent24", Some("agent-1")).unwrap_err(),
            payment_invalid
        );
        // 成员凭证断言别人
        assert_eq!(
            resolve(&w, "mem-kid-1", Some("kid-2")).unwrap_err(),
            payment_invalid
        );
        // 不存在的凭证（比如本地数据密钥）：不透明的 401
        assert_eq!(
            resolve(&w, "vk_not_in_the_account_store", None).unwrap_err(),
            Refusal::Unauthorized
        );
    }

    #[test]
    fn a_revoked_principal_membership_credential_or_a_frozen_account_stop_the_next_request() {
        let w = world();
        assert!(resolve(&w, "svc-eduai", Some("kid-1")).is_ok());
        // 撤销主体（同意撤回）
        w.accounts.revoke_principal(&pid("kid-1"), NOW).unwrap();
        assert_eq!(
            resolve(&w, "svc-eduai", Some("kid-1")).unwrap_err(),
            Refusal::Payment(ReasonCode::PrincipalRevoked)
        );
        // 撤销成员关系
        assert!(resolve(&w, "svc-eduai", Some("kid-2")).is_ok());
        w.accounts
            .revoke_membership(&MembershipId::new("m-kid-2").unwrap(), NOW)
            .unwrap();
        assert_eq!(
            resolve(&w, "svc-eduai", Some("kid-2")).unwrap_err(),
            Refusal::Payment(ReasonCode::MembershipRevoked)
        );
        // 冻结付款账户的祖先
        assert!(resolve(&w, "svc-agent24", Some("agent-1")).is_ok());
        w.accounts
            .set_account_status(&aid("agent24"), AccountStatus::Frozen)
            .unwrap();
        assert_eq!(
            resolve(&w, "svc-agent24", Some("agent-1")).unwrap_err(),
            Refusal::Payment(ReasonCode::AccountInactive)
        );
        // 凭证本身：冻结、撤销都是不透明的 401
        w.accounts
            .set_credential_status(&cid("svc-eduai"), CredentialStatus::Frozen, NOW)
            .unwrap();
        assert_eq!(
            resolve(&w, "svc-eduai", Some("kid-1")).unwrap_err(),
            Refusal::Unauthorized
        );
        w.accounts
            .set_credential_status(&cid("svc-eduai"), CredentialStatus::Revoked, NOW)
            .unwrap();
        assert_eq!(
            resolve(&w, "svc-eduai", Some("kid-1")).unwrap_err(),
            Refusal::Unauthorized
        );
    }

    #[test]
    fn an_expired_credential_is_an_opaque_401_at_its_exact_expiry_instant() {
        let w = world();
        let expiring = Credential::new(
            cid("svc-short"),
            CredentialBinding::Service {
                account: aid("eduai"),
            },
            CredentialStatus::Active,
            Some(TimestampMs(NOW.0 + 1_000)),
            vec![],
        )
        .unwrap();
        w.accounts.create_credential(&expiring, NOW).unwrap();
        let at = |ms: i64| {
            w.resolver
                .resolve(&cid("svc-short"), Some(&pid("kid-1")), TimestampMs(ms))
        };
        assert!(at(NOW.0 + 999).is_ok());
        assert_eq!(at(NOW.0 + 1_000).unwrap_err(), Refusal::Unauthorized);
    }

    #[test]
    fn a_tenant_that_is_not_declared_is_refused_not_guessed() {
        let w = world();
        assert_eq!(
            resolve(&w, "svc-stray", Some("stray-1")).unwrap_err(),
            Refusal::TenantNotProvisioned
        );
    }

    #[test]
    fn a_principal_who_belongs_to_two_orgs_resolves_to_the_account_under_each_credentials_own_scope()
     {
        let w = world();
        let both = pid("both");
        // 同一个主体在学校 A 的账户 s1x、学校 B 的账户 s2x
        for (id, parent) in [("s1x", "school-a"), ("s2x", "school-b")] {
            w.accounts
                .create_account(&account(id, AccountKind::Member, Some(parent)), NOW)
                .unwrap();
        }
        for (membership, org, acct) in [
            ("m-both-a", "school-a", "s1x"),
            ("m-both-b", "school-b", "s2x"),
        ] {
            w.accounts
                .create_membership(
                    &Membership {
                        id: MembershipId::new(membership).unwrap(),
                        principal: both.clone(),
                        org: aid(org),
                        role: MembershipRole::Member,
                        account: Some(aid(acct)),
                        status: MembershipStatus::Active,
                    },
                    NOW,
                )
                .unwrap();
        }
        // 绑在学校 A / B 的两张 Service 凭证，各自落到自己范围里的账户
        for (id, school) in [("svc-a", "school-a"), ("svc-b", "school-b")] {
            w.accounts
                .create_credential(
                    &Credential::new(
                        cid(id),
                        CredentialBinding::Service {
                            account: aid(school),
                        },
                        CredentialStatus::Active,
                        None,
                        vec![],
                    )
                    .unwrap(),
                    NOW,
                )
                .unwrap();
        }
        assert_eq!(
            resolve(&w, "svc-a", Some("both"))
                .unwrap()
                .policy
                .payment
                .payer()
                .as_str(),
            "s1x"
        );
        assert_eq!(
            resolve(&w, "svc-b", Some("both"))
                .unwrap()
                .policy
                .payment
                .payer()
                .as_str(),
            "s2x"
        );
        // 两个都在 eduai 的范围内：歧义，拒绝，不猜
        assert_eq!(
            resolve(&w, "svc-eduai", Some("both")).unwrap_err(),
            Refusal::Payment(ReasonCode::PaymentContextInvalid)
        );
        // 每次请求都是独立解析的：交替调用，每次结果都一样
        for _ in 0..3 {
            assert_eq!(
                resolve(&w, "svc-a", Some("both"))
                    .unwrap()
                    .policy
                    .payment
                    .payer()
                    .as_str(),
                "s1x"
            );
            assert_eq!(
                resolve(&w, "svc-b", Some("both"))
                    .unwrap()
                    .policy
                    .payment
                    .payer()
                    .as_str(),
                "s2x"
            );
        }
    }

    #[test]
    fn a_quota_account_in_a_cloud_database_is_refused() {
        // 云形态只有货币账户。把一棵 QUOTA 树放进库里（组织形态的样子）：解析必须失败即关闭。
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(AccountStore::open(dir.path().join("q.sqlite3")).unwrap());
        let quota = Unit::Quota(idoris_compute_contracts::ids::NodeId::new("node-1").unwrap());
        let q = |id: &str, kind, parent: Option<&str>| Account {
            id: aid(id),
            kind,
            parent: parent.map(aid),
            unit: quota.clone(),
            status: AccountStatus::Active,
        };
        store
            .create_account(&q("platform", AccountKind::Platform, None), NOW)
            .unwrap();
        store
            .create_account(&q("quota-org", AccountKind::Org, Some("platform")), NOW)
            .unwrap();
        store
            .create_credential(
                &Credential::new(
                    cid("node-q"),
                    CredentialBinding::Node {
                        account: aid("quota-org"),
                    },
                    CredentialStatus::Active,
                    None,
                    vec![],
                )
                .unwrap(),
                NOW,
            )
            .unwrap();
        let config = Arc::new(
            IngressConfig::parse(
                "schema_version: 1\ntenants:\n  - account: quota-org\n    data_classes: [adult]\n    max_retention: zdr\n    billing: paid\n",
            )
            .unwrap(),
        );
        let resolver = CallerResolver::new(store, config);
        assert_eq!(
            resolver.resolve(&cid("node-q"), None, NOW).unwrap_err(),
            Refusal::Payment(ReasonCode::PaymentContextInvalid)
        );
    }
}
