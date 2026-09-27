//! A-1 冒烟检查——**不是**单元测试。`cargo test` 天然定义 `cfg(test)`，没法
//! 在 `#[test]` 里验证"既不在跑测试、也没开 `dev-mock` feature"这个组合到底
//! 会不会拒绝 `mock://` scheme；只有一个不带 `cfg(test)` 的独立二进制才能
//! 真正跑到那个分支。这里复刻 `config/components/mock.yaml` 的真实形状
//! （`http_service` + `locality: loopback` + `endpoint: "mock://in-memory"`），
//! 分别验证两种编译方式的行为：
//!
//! - `cargo run -p idoris-policy --example dev_mock_scheme_gate`
//!   （不开 feature）期望：`UnsupportedScheme`，证明真实 yaml 在默认生产
//!   构建下确实会被拒绝——这正是 A-1 要修的问题，如果这个断言失败说明
//!   `dev-mock` 网关本身失效了。
//! - `cargo run -p idoris-policy --example dev_mock_scheme_gate --features dev-mock`
//!   期望：`Ok(())`，证明打开开关后 R2-D 能正常接线本地 mock 组件卡。

use idoris_contracts::ComponentCard;
use idoris_contracts::common::{Capability, FallbackPolicy, PrivacyClass, Tier};
use idoris_contracts::component_card::{Egress, Form};
use idoris_contracts::load_policy::{Admission, Keepalive, LoadMode, LoadPolicy};
use idoris_contracts::provider::{Cost, Family, Locality, ProviderDescriptor};
#[cfg(not(feature = "dev-mock"))]
use idoris_policy::RegistrationError;
use idoris_policy::{AdmissionStatus, Card, validate_registration};

/// 逐字段对照 `config/components/mock.yaml`。
fn mock_yaml_shaped_card() -> Card {
    Card {
        component: ComponentCard {
            provider: ProviderDescriptor {
                id: "mock".to_string(),
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
            endpoint: "mock://in-memory".to_string(),
            version_pin: "mock@0.1.0".to_string(),
            privacy_class: PrivacyClass::LocalOnly,
            allowed_egress: vec![Egress::None],
            fallback_policy: FallbackPolicy::FailClosed,
            fail_closed: true,
            load_policy: Some(LoadPolicy {
                mode: LoadMode::Resident,
                keepalive: Keepalive::Pinned { pinned: true },
                admission: Admission::Coexist,
            }),
            extensions: None,
        },
        roles: Vec::new(),
        experiment: false,
        min_ram_gb: 0.0,
        estimated_cost_minor: Some(0),
        admission_status: AdmissionStatus::Ready,
    }
}

fn main() {
    let result = validate_registration(&[mock_yaml_shaped_card()]);

    #[cfg(feature = "dev-mock")]
    {
        assert!(
            result.is_ok(),
            "dev-mock 打开时应该放行 config/components/mock.yaml 的真实形状，得到 {result:?}"
        );
        println!("OK: dev-mock feature 打开，mock:// scheme 被放行（R2-D 接线路径）。");
    }

    #[cfg(not(feature = "dev-mock"))]
    {
        assert!(
            matches!(result, Err(RegistrationError::UnsupportedScheme { .. })),
            "dev-mock 关闭时应该拒绝 mock:// scheme，得到 {result:?}"
        );
        println!("OK: dev-mock feature 关闭，mock:// scheme 按未知 scheme 拒绝（默认生产行为）。");
    }
}
