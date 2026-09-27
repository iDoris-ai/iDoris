//! 路由候选（`Card`）：融合 `idoris_contracts::ComponentCard`（隐私/出站/端点）
//! 与 catalog 侧的角色/硬件/准入信息（`packages/router/src/roles.ts`、
//! `packages/recommender/src/recommend.ts`——这两个文件目前在 `main` 分支，
//! 本 crate 所在的 Rust 骨架分支线尚未合并它们，porting 时以 `main` 上的版本
//! 为准，参见任务里 `git show origin/main:<path>` 的读取方式）。
//!
//! 本 crate 没有独立的 catalog/registry 类型（那些留在 `idoris-recommender`
//! 或未来的注册表 crate），这里只取决策管道实际需要的字段，避免跨 crate 的
//! 循环/多余依赖。目录角色相关字段（`roles`/`experiment`/`min_ram_gb`）由
//! 后续 PR（`role` 模块）加入。
//!
//! **一个 `Card` = 一个可路由的服务实例**（provider + 它能服务的角色集合），
//! 不是「一个具体模型」。同一个物理后端（例如一个 oMLX 进程）如果要以不同
//! admission/成本状态暴露给路由，需要注册成 `provider.id` 不同的多张卡——
//! `provider.id` 全局唯一是注册校验（一个后续 PR 里的 `registry` 模块）的
//! 前提，`Card::id` 就是这张卡在决策里的身份。
//!
//! **信任边界**：这个类型本身不重新校验 `component` 内部的隐私/出站一致性
//! （例如「`privacy_class: local_only` 却 `fail_closed: false`」这类自相矛盾）
//! ——那是注册校验（一个后续 PR 里的 `registry` 模块，连同
//! `idoris_contracts::ComponentCard::validate`）的职责，且必须在构造 `Card`
//! 之前跑过。决策管道假设传入的候选已经通过注册校验，本身不会补做这一步。
//! 这只覆盖 `component` 里静态声明的隐私/出站字段——`admission_status`、
//! `estimated_cost_minor` 这两个字段是每次决策都要重新提供的动态输入，本类型
//! 完全不校验它们，调用方必须保证它们同样来自可信来源（真实的容量探测/计价
//! 结果），而不是随意构造的值；伪造或过时的动态字段同样能让决策 fail-open，
//! 不止是绕过注册校验这一条路。

use idoris_contracts::ComponentCard;

/// 路由候选的准入状态。**这是决策层的动态状态**，和
/// `component.load_policy.admission`（[`idoris_contracts::load_policy::Admission`]，
/// `coexist | requires_eviction`，描述的是这张卡自己的加载策略）是两个不同的
/// 东西，取名不同以免混淆——不要假设两者会保持一致，调用方按运行时容量探测
/// 结果填充这个字段。
///
/// 对齐 `recommend.ts` 的 `AdmissionStatus`（`"ready" | "requires_eviction" |
/// "BLOCKED"`——注意 TS 源码里 `BLOCKED` 是全大写，是那份内部类型自己的历史
/// 拼法；接口规范 §3.3 `/capabilities` 对外的线上取值是全小写
/// `ready|requires_eviction|blocked`。这里统一用 Rust 命名约定，两套外部拼法
/// 都不直接复用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionStatus {
    Ready,
    RequiresEviction,
    Blocked,
}

/// 一个路由候选。字段来源见上面的模块文档。
///
/// `PartialEq` 是派生的，仅用于测试断言/日志比较，不是身份判定（用
/// [`Card::id`] 判定身份）——它会一路比较到 `component.provider.cost` 那两个
/// `f64`，继承了 IEEE 754 的相等语义：`NaN != NaN`，成本有细微浮点误差时两张
/// "同一张卡" 也会比较不相等。`idoris_contracts` 目前的校验只拒绝负成本，不
/// 拒绝 `NaN`，所以这不是本类型能独自堵上的洞。
#[derive(Debug, Clone, PartialEq)]
pub struct Card {
    pub component: ComponentCard,
    /// 本次请求走这张卡的估算成本（最小货币单位，例如分）。
    ///
    /// `None` 表示价格未知——按不变式 #3（价格未知 ≠ 免费），预算阶段会
    /// 无条件剔除这类候选，绝不当作免费处理。真正按 token 估算价格的逻辑
    /// 不在本 crate 里（那需要计价表 + 请求体大小，属于调用方职责）。
    ///
    /// 负值同样按「价格未知」处理（决策管道不会把它当成免费，也不会让它
    /// 悄悄从候选集合里消失而不留任何拒绝痕迹）——调用方不应该产出负值，
    /// 但这个类型本身不禁止它，所以管道侧要能防御。
    pub estimated_cost_minor: Option<i64>,
    pub admission_status: AdmissionStatus,
}

impl Card {
    pub fn id(&self) -> &str {
        &self.component.provider.id
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use idoris_contracts::ComponentCard;
    use idoris_contracts::common::{Capability, FallbackPolicy, PrivacyClass, Tier};
    use idoris_contracts::component_card::{Egress, Form};
    use idoris_contracts::load_policy::{Admission, Keepalive, LoadMode, LoadPolicy};
    use idoris_contracts::provider::{Cost, Family, Locality, ProviderDescriptor};

    use super::{AdmissionStatus, Card};

    /// 测试夹具：一张健康、本地、免费、`Ready` 的 `http_service` 卡——
    /// `tier: local` 的卡按接口规范需要声明 `load_policy`，这里给一个常驻
    /// + 可与其他候选共存（`coexist`）的最小合法配置。各测试按需覆盖单个字段。
    pub(crate) fn sample_card(id: &str) -> Card {
        Card {
            component: ComponentCard {
                provider: ProviderDescriptor {
                    id: id.to_string(),
                    family: Family::Idoris,
                    tier: Tier::Local,
                    capabilities: vec![Capability::Chat],
                    privacy_class: PrivacyClass::LocalOnly,
                    cost: Cost {
                        input_per_m: 0.0,
                        output_per_m: 0.0,
                    },
                    locality: Locality::Loopback,
                    extensions: None,
                },
                form: Form::HttpService,
                endpoint: "http://127.0.0.1:8740".to_string(),
                version_pin: "0.0.0".to_string(),
                privacy_class: PrivacyClass::LocalOnly,
                allowed_egress: vec![Egress::Loopback],
                fallback_policy: FallbackPolicy::FailClosed,
                fail_closed: true,
                load_policy: Some(LoadPolicy {
                    mode: LoadMode::Resident,
                    keepalive: Keepalive::Pinned { pinned: true },
                    admission: Admission::Coexist,
                }),
                extensions: None,
            },
            estimated_cost_minor: Some(0),
            admission_status: AdmissionStatus::Ready,
        }
    }

    #[test]
    fn sample_card_is_a_trusted_local_free_ready_candidate() {
        let card = sample_card("probe");
        assert_eq!(card.id(), "probe");
        assert_eq!(card.estimated_cost_minor, Some(0));
        assert_eq!(card.admission_status, AdmissionStatus::Ready);
        assert_eq!(card.component.provider.locality, Locality::Loopback);
        assert_eq!(card.component.privacy_class, PrivacyClass::LocalOnly);
        assert_eq!(card.component.allowed_egress, vec![Egress::Loopback]);
        assert!(card.component.fail_closed);
        assert!(card.component.load_policy.is_some());
    }
}
