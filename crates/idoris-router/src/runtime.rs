//! Construct and index lifecycle-managed runtimes by component provider.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use idoris_backend::{
    BackendError, BackendStatus, GlobalCapacityLedger, GlobalCapacitySnapshot, RuntimeAdapter,
    Supervisor, SupervisorConfig, SupervisorHandle,
};
use idoris_contracts::{ComponentCard, component_card::Form};
use idoris_policy::is_subscription_provider_id;
use idoris_upstream::factory::{AdapterParams, create_adapter, create_adapter_with};
use idoris_upstream::{LocalHttpRuntimeAdapter, LocalHttpRuntimeConfig};

use crate::dispatch::BoundSupervisor;

/// Decimal 1GB kept unreserved inside an allocated budget unless a chat
/// request confirms the breach (`X-iDoris-Headroom-Breach: confirmed`).
const HEADROOM_BYTES: u64 = 1_000_000_000;

#[derive(Debug, Clone, Default)]
pub struct RuntimeRegistry {
    supervisors: BTreeMap<String, BoundSupervisor>,
    startup_capacity: Option<Arc<GlobalCapacityLedger>>,
    #[cfg(test)]
    ready_snapshot_calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl RuntimeRegistry {
    pub fn spawn(cards: &[ComponentCard]) -> Result<Self, String> {
        Self::spawn_with_factory(cards, create_adapter)
    }

    /// Construct lifecycle runtimes using an explicitly supplied adapter
    /// factory. The production default remains [`create_adapter`]; this seam
    /// lets trusted startup configuration provide process-owned local runtimes
    /// without smuggling executable/model paths into portable component cards.
    pub fn spawn_with_factory<F>(cards: &[ComponentCard], factory: F) -> Result<Self, String>
    where
        F: Fn(&ComponentCard) -> Result<std::sync::Arc<dyn RuntimeAdapter>, BackendError>,
    {
        Self::spawn_with_factory_and_budget(cards, None, factory)
    }

    /// Same as [`Self::spawn_with_factory`] but with the remote kill switch
    /// engaged at boot: cards that fail `is_local_capable` are not status-probed.
    pub fn spawn_with_factory_budget_and_kill_switch<F>(
        cards: &[ComponentCard],
        allocated_budget_bytes: Option<u64>,
        remote_kill_switch: bool,
        factory: F,
    ) -> Result<Self, String>
    where
        F: Fn(&ComponentCard) -> Result<std::sync::Arc<dyn RuntimeAdapter>, BackendError>,
    {
        Self::spawn_with_factory_budgeted(
            cards,
            allocated_budget_bytes,
            remote_kill_switch,
            factory,
        )
    }

    /// Apply an already allocated model budget to both process-wide admission
    /// and each runtime. Bytes use decimal GB, matching model memory estimates.
    /// `None` preserves the historical default; no OS reserve is deducted here.
    pub fn spawn_with_factory_and_budget<F>(
        cards: &[ComponentCard],
        allocated_budget_bytes: Option<u64>,
        factory: F,
    ) -> Result<Self, String>
    where
        F: Fn(&ComponentCard) -> Result<std::sync::Arc<dyn RuntimeAdapter>, BackendError>,
    {
        Self::spawn_with_factory_budgeted(cards, allocated_budget_bytes, false, factory)
    }

    fn spawn_with_factory_budgeted<F>(
        cards: &[ComponentCard],
        allocated_budget_bytes: Option<u64>,
        remote_kill_switch: bool,
        factory: F,
    ) -> Result<Self, String>
    where
        F: Fn(&ComponentCard) -> Result<std::sync::Arc<dyn RuntimeAdapter>, BackendError>,
    {
        let config = SupervisorConfig {
            budget_gb: allocated_budget_bytes
                .map(|bytes| bytes as f64 / 1_000_000_000.0)
                .unwrap_or_else(|| SupervisorConfig::default().budget_gb),
            ..SupervisorConfig::default()
        };
        let budget_gb = config.budget_gb;
        let startup_capacity = Arc::new(match allocated_budget_bytes {
            Some(bytes) => GlobalCapacityLedger::new_bytes_with_headroom(bytes, HEADROOM_BYTES),
            None => GlobalCapacityLedger::new(budget_gb)
                .map_err(|error| format!("startup capacity ledger: {error}"))?,
        });
        let mut registry = Self {
            supervisors: BTreeMap::new(),
            startup_capacity: Some(startup_capacity.clone()),
            #[cfg(test)]
            ready_snapshot_calls: Arc::default(),
        };
        for card in cards {
            let Some(handle) = spawn_runtime_with_factory(
                card,
                &factory,
                Some(&startup_capacity),
                config.clone(),
                remote_kill_switch,
            )?
            else {
                continue;
            };
            let provider_id = card.provider.id.clone();
            if registry
                .supervisors
                .insert(provider_id.clone(), BoundSupervisor::new(card, handle))
                .is_some()
            {
                return Err(format!(
                    "duplicate lifecycle runtime for provider {provider_id}"
                ));
            }
        }
        Ok(registry)
    }

    pub fn spawn_with_local_configs(
        cards: &[ComponentCard],
        local: &BTreeMap<String, LocalHttpRuntimeConfig>,
    ) -> Result<Self, String> {
        Self::spawn_with_local_configs_and_budget(cards, local, None)
    }

    pub fn spawn_with_local_configs_and_budget(
        cards: &[ComponentCard],
        local: &BTreeMap<String, LocalHttpRuntimeConfig>,
        allocated_budget_bytes: Option<u64>,
    ) -> Result<Self, String> {
        Self::spawn_with_local_configs_budget_and_kill_switch(
            cards,
            local,
            allocated_budget_bytes,
            false,
        )
    }

    /// Startup entry point: both the allocated model budget and the remote kill
    /// switch engaged at boot are threaded into the per-runtime construction.
    pub fn spawn_with_local_configs_budget_and_kill_switch(
        cards: &[ComponentCard],
        local: &BTreeMap<String, LocalHttpRuntimeConfig>,
        allocated_budget_bytes: Option<u64>,
        remote_kill_switch: bool,
    ) -> Result<Self, String> {
        Self::spawn_with_local_factory(
            cards,
            local,
            allocated_budget_bytes,
            remote_kill_switch,
            create_adapter,
        )
    }

    /// Build the Supervisor table when the start-up gate opens (interface spec §2.7.6, erratum 12 (3)).
    ///
    /// Managed cards the gate gave up on (`degraded`: no Supervisor is created for an engine that did not come up)
    /// are left out of the card set. The local-runtime table needs no matching projection: a local runtime
    /// configuration can never be bound to a managed card ([`Self::validate_local_configs`] refuses that at
    /// start-up, before anything starts), so every configuration's card is still in the set whatever the gate gave
    /// up on, and the second validation inside [`Self::spawn_with_local_factory`] passes by construction. (The
    /// earlier finalize removed only the cards, which made that second validation turn a legal-looking
    /// configuration into "没有对应的组件卡" and took the whole process down on a degraded open; the first fix
    /// projected the table too, silently dropping a configuration the operator had written. Refusing the
    /// combination up front is the honest version.)
    /// `params` were resolved at start-up ([`Self::resolve_adapter_params`]): nothing reads the environment here.
    pub fn spawn_after_gate(
        cards: &[ComponentCard],
        local: &BTreeMap<String, LocalHttpRuntimeConfig>,
        degraded: impl Fn(&str) -> bool,
        allocated_budget_bytes: Option<u64>,
        remote_kill_switch: bool,
        params: &AdapterParams,
    ) -> Result<Self, String> {
        let cards: Vec<ComponentCard> = cards
            .iter()
            .filter(|card| !degraded(&card.provider.id))
            .cloned()
            .collect();
        Self::spawn_with_local_factory(
            &cards,
            local,
            allocated_budget_bytes,
            remote_kill_switch,
            |card| create_adapter_with(card, params),
        )
    }

    /// Resolve the adapters' environment parameters for the **original** card set, before anything starts (interface
    /// spec erratum 12 (3)): an invalid value (`IDORIS_LOAD_TIMEOUT_S`) is a start-up failure whether or not the
    /// managed engine later turns out to be ready. Only a card that will actually be built by the adapter factory
    /// (Supervisor-managed, not a subscription relay, not a resident direct-proxy card, no local-runtime
    /// configuration of its own) needs the environment; with none, nothing is read (unchanged behaviour).
    ///
    /// Then **every** card that gets a Supervisor is dry-run through the very constructor path the gate-open build
    /// uses ([`adapter_for`]) and the adapter dropped: a card whose adapter cannot be built (`no adapter for
    /// provider X`, a form with `no runtime constructor`) is a start-up failure here, not an error at gate-open
    /// (after the engines already started and the discovery file was published) that a degraded projection could
    /// even have hidden. Constructing an adapter does no I/O; it only reads the environment and settings for the
    /// oMLX API key, so the dry-run neither depends on nor disturbs the engine's state.
    pub fn resolve_adapter_params(
        cards: &[ComponentCard],
        local: &BTreeMap<String, LocalHttpRuntimeConfig>,
    ) -> Result<AdapterParams, String> {
        let mut supervised = Vec::new();
        for card in cards {
            if uses_supervisor(card)? {
                supervised.push(card);
            }
        }
        let params = match supervised
            .iter()
            .find(|card| !local.contains_key(&card.provider.id))
        {
            Some(card) => AdapterParams::from_env()
                .map_err(|error| format!("provider {}: {error}", card.provider.id))?,
            None => AdapterParams::default(),
        };
        for card in supervised {
            adapter_for(card, local, |card| create_adapter_with(card, &params))
                .map_err(|error| format!("provider {}: {error}", card.provider.id))?;
        }
        Ok(params)
    }

    /// One validation of the local-runtime table against `cards`, then one Supervisor per lifecycle card; `make`
    /// builds the adapter of a card that has no local-runtime configuration of its own.
    fn spawn_with_local_factory<F>(
        cards: &[ComponentCard],
        local: &BTreeMap<String, LocalHttpRuntimeConfig>,
        allocated_budget_bytes: Option<u64>,
        remote_kill_switch: bool,
        make: F,
    ) -> Result<Self, String>
    where
        F: Fn(&ComponentCard) -> Result<std::sync::Arc<dyn RuntimeAdapter>, BackendError>,
    {
        Self::validate_local_configs(cards, local)?;
        let mut registry = Self::spawn_with_factory_budgeted(
            cards,
            allocated_budget_bytes,
            remote_kill_switch,
            |card| adapter_for(card, local, &make),
        )?;
        for (provider_id, config) in local {
            if let Some(bound) = registry.supervisors.get_mut(provider_id) {
                *bound = bound
                    .clone()
                    .with_admission_model(config.model_id.clone(), config.memory_gb);
            }
        }
        Ok(registry)
    }

    /// 本地 runtime 配置与组件卡对得上：每个配置都有对应的卡、不绑托管卡（`extensions.idoris_managed`）、不绑 Resident
    /// 直连卡、form/locality/endpoint 一致。不启动任何任务。启动门控期间（接口规范 §2.7.6）Supervisor 要等引擎落定才创建，
    /// 但这些配置错误仍要在启动的一开头报出来（先于绑定端口），所以单独拿出来，
    /// [`Self::spawn_with_local_configs_budget_and_kill_switch`] 也先调它。
    ///
    /// 本地 runtime 配置绑托管卡是矛盾的（勘误十二（3））：托管卡的引擎由引擎管理器拉起、停止、回收（所有权记录、`stop_on_exit`、
    /// 退出码 5），本地 runtime 配置是另一条拉起路径（`LocalHttpRuntimeAdapter` 自己起 `mlx_lm_server`），两条路都想管同一个
    /// 端口。降级放行时配置只能随卡一起默默消失，那是在替操作员丢掉他写下的配置；所以启动时就拒绝。
    pub fn validate_local_configs(
        cards: &[ComponentCard],
        local: &BTreeMap<String, LocalHttpRuntimeConfig>,
    ) -> Result<(), String> {
        for (provider_id, config) in local {
            let card = cards
                .iter()
                .find(|card| card.provider.id == *provider_id)
                .ok_or_else(|| {
                    format!("本地 runtime {provider_id:?} 没有对应的组件卡，拒绝启动")
                })?;
            if is_managed_card(card) {
                return Err(format!(
                    "本地 runtime {provider_id:?} 不能绑定由引擎管理器托管的组件卡（extensions.{}）：托管卡的引擎由 iDoris 的引擎管理器拉起与停止，本地 runtime 配置是另一条拉起路径，两者只能留一个，拒绝启动",
                    crate::engine_kind::MANAGED_EXTENSION
                ));
            }
            if crate::dispatch::is_resident_http_service(card) {
                return Err(format!(
                    "本地 runtime {provider_id:?} 不能绑定 Resident 直连组件卡"
                ));
            }
            if card.form != Form::HttpService
                || card.provider.locality != idoris_contracts::provider::Locality::Loopback
                || card.endpoint != config.launch.endpoint()
            {
                return Err(format!(
                    "本地 runtime {provider_id:?} 与组件卡的 form/locality/endpoint 不一致"
                ));
            }
        }
        Ok(())
    }

    pub fn get(&self, provider_id: &str) -> Option<&BoundSupervisor> {
        self.supervisors.get(provider_id)
    }

    pub(crate) async fn status_observations(
        &self,
    ) -> Vec<(String, Result<BackendStatus, BackendError>)> {
        let mut observations = Vec::with_capacity(self.supervisors.len());
        for (provider_id, supervisor) in &self.supervisors {
            observations.push((provider_id.clone(), supervisor.status().await));
        }
        observations
    }

    #[cfg(test)]
    pub(crate) fn from_supervisors(supervisors: impl IntoIterator<Item = BoundSupervisor>) -> Self {
        Self {
            supervisors: supervisors
                .into_iter()
                .map(|bound| (bound.provider_id().to_string(), bound))
                .collect(),
            startup_capacity: None,
            ready_snapshot_calls: Arc::default(),
        }
    }

    pub fn len(&self) -> usize {
        self.supervisors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.supervisors.is_empty()
    }

    /// Wait for every lifecycle runtime's startup reconciliation, then return
    /// the shared residency snapshot. The same ledger admits load claims
    /// across runtimes and settles them after load/unload completion.
    pub async fn startup_capacity_snapshot(
        &self,
    ) -> Result<Option<GlobalCapacitySnapshot>, BackendError> {
        let Some(ledger) = &self.startup_capacity else {
            return Ok(None);
        };
        for supervisor in self.supervisors.values() {
            supervisor.status().await?;
        }
        ledger.snapshot().map(Some)
    }

    pub async fn queue_depth(&self) -> u64 {
        let mut depth = 0_u64;
        for (provider_id, supervisor) in &self.supervisors {
            depth = add_status_depth(depth, provider_id, supervisor.status().await);
        }
        depth
    }

    /// Return providers whose exact requested lifecycle target is currently
    /// chat-ready. Errors are conservatively treated as not ready.
    pub async fn exact_ready_providers(&self, targets: Vec<(String, String)>) -> BTreeSet<String> {
        #[cfg(test)]
        self.ready_snapshot_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut ready = BTreeSet::new();
        for (provider_id, model_id) in targets {
            if let Some(supervisor) = self.supervisors.get(&provider_id)
                && ready_for_affinity(supervisor.is_ready(&model_id).await)
            {
                ready.insert(provider_id);
            }
        }
        ready
    }

    #[cfg(test)]
    pub(crate) fn ready_snapshot_calls(&self) -> usize {
        self.ready_snapshot_calls
            .load(std::sync::atomic::Ordering::SeqCst)
    }
}

fn ready_for_affinity(result: Result<bool, BackendError>) -> bool {
    result.unwrap_or(false)
}

fn add_status_depth(
    depth: u64,
    provider_id: &str,
    status: Result<idoris_backend::BackendStatus, idoris_backend::BackendError>,
) -> u64 {
    match status {
        Ok(status) => depth.saturating_add(status.loaded.len() as u64),
        Err(error) => {
            eprintln!(
                "[idoris] capabilities: provider {provider_id} status failed ({}), excluded from queue depth",
                error.reason_code()
            );
            depth
        }
    }
}

impl From<BoundSupervisor> for RuntimeRegistry {
    fn from(bound: BoundSupervisor) -> Self {
        let mut supervisors = BTreeMap::new();
        supervisors.insert(bound.provider_id().to_string(), bound);
        Self {
            supervisors,
            startup_capacity: None,
            #[cfg(test)]
            ready_snapshot_calls: Arc::default(),
        }
    }
}

impl From<Option<BoundSupervisor>> for RuntimeRegistry {
    fn from(bound: Option<BoundSupervisor>) -> Self {
        bound.map_or_else(Self::default, Self::from)
    }
}

/// Return `None` for resident HTTP services, which use the direct proxy path.
///
/// # Panics
/// Lifecycle construction requires a running Tokio runtime, like `Supervisor::spawn`.
pub fn spawn_runtime(card: &ComponentCard) -> Result<Option<SupervisorHandle>, String> {
    spawn_runtime_with_factory(
        card,
        &create_adapter,
        None,
        SupervisorConfig::default(),
        false,
    )
}

/// Whether `card` gets a lifecycle Supervisor built by the adapter factory. Subscription relays and resident
/// direct-proxy cards do not (`Ok(false)`); anything else must be an `http_service`.
fn uses_supervisor(card: &ComponentCard) -> Result<bool, String> {
    if is_subscription_provider_id(&card.provider.id) {
        return Ok(false);
    }
    if crate::dispatch::is_resident_http_service(card) {
        return Ok(false);
    }
    if card.form != Form::HttpService {
        return Err(format!(
            "provider {} with form {:?} has no runtime constructor",
            card.provider.id, card.form
        ));
    }
    Ok(true)
}

/// Whether the card declares an engine the engine manager owns (`extensions.idoris_managed`; the same predicate the
/// card validation uses to set `EngineIdentity::managed`).
fn is_managed_card(card: &ComponentCard) -> bool {
    card.extensions
        .as_ref()
        .is_some_and(|extensions| extensions.contains_key(crate::engine_kind::MANAGED_EXTENSION))
}

/// The one way a Supervisor card's adapter is built: from its own local-runtime configuration when it has one,
/// otherwise by `make` (the adapter factory). The gate-open build and the start-up dry-run
/// ([`RuntimeRegistry::resolve_adapter_params`]) both go through here, so the dry-run checks exactly what the build
/// will do.
fn adapter_for<F>(
    card: &ComponentCard,
    local: &BTreeMap<String, LocalHttpRuntimeConfig>,
    make: F,
) -> Result<std::sync::Arc<dyn RuntimeAdapter>, BackendError>
where
    F: Fn(&ComponentCard) -> Result<std::sync::Arc<dyn RuntimeAdapter>, BackendError>,
{
    if let Some(config) = local.get(&card.provider.id) {
        return LocalHttpRuntimeAdapter::new(config.clone())
            .map(|adapter| std::sync::Arc::new(adapter) as std::sync::Arc<dyn RuntimeAdapter>);
    }
    make(card)
}

fn is_trusted_local_card(card: &ComponentCard) -> bool {
    idoris_policy::privacy::is_local_capable(&idoris_policy::Card {
        component: card.clone(),
        roles: Vec::new(),
        experiment: false,
        min_ram_gb: 0.0,
        estimated_cost_minor: None,
        admission_status: idoris_policy::AdmissionStatus::Ready,
    })
}

fn spawn_runtime_with_factory<F>(
    card: &ComponentCard,
    factory: &F,
    startup_capacity: Option<&Arc<GlobalCapacityLedger>>,
    config: SupervisorConfig,
    remote_kill_switch: bool,
) -> Result<Option<SupervisorHandle>, String>
where
    F: Fn(&ComponentCard) -> Result<std::sync::Arc<dyn RuntimeAdapter>, BackendError>,
{
    if !uses_supervisor(card)? {
        return Ok(None);
    }
    let adapter =
        factory(card).map_err(|error| format!("provider {}: {error}", card.provider.id))?;
    // Engaged at boot: do not status-probe a card that fails `is_local_capable`.
    // Trusted loopback local_only cards are still probed.
    let skip_startup_probe = remote_kill_switch && !is_trusted_local_card(card);
    let supervisor = match (startup_capacity, skip_startup_probe) {
        (Some(ledger), true) => Supervisor::spawn_with_startup_capacity_tracking_without_probe(
            adapter,
            config,
            ledger.clone(),
            card.provider.id.clone(),
        ),
        (Some(ledger), false) => Supervisor::spawn_with_startup_capacity_tracking(
            adapter,
            config,
            ledger.clone(),
            card.provider.id.clone(),
        ),
        // Registry startup always passes a ledger; `spawn_runtime` passes the
        // switch as false, so a missing ledger never takes the skip arm.
        (None, _) => Supervisor::spawn(adapter, config),
    };
    supervisor
        .map(Some)
        .map_err(|error| format!("provider {}: {error}", card.provider.id))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::dispatch::{DispatchFailure, DispatchInput, dispatch_local};
    use crate::profile::{IntentSource, ParsedProfile};
    use idoris_backend::{ChatMessage, MockAdapter, ModelInfo};
    use idoris_contracts::TaskProfile;
    use idoris_contracts::load_policy::{Admission, Keepalive, LoadMode, LoadPolicy};
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    fn card() -> ComponentCard {
        serde_yaml::from_str(include_str!("../../../config/components/omlx.yaml"))
            .expect("fixture is a valid card")
    }

    fn named_card(id: &str) -> ComponentCard {
        let mut card = card();
        card.provider.id = id.to_string();
        card
    }

    fn mock_bound(card: &ComponentCard) -> BoundSupervisor {
        let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
            id: card.provider.id.clone(),
            memory_gb: 1.0,
        }]));
        let handle = Supervisor::spawn(adapter, SupervisorConfig::default()).unwrap();
        BoundSupervisor::new(card, handle)
    }

    fn profile() -> ParsedProfile {
        ParsedProfile {
            task: TaskProfile::default(),
            role: None,
            tenant_id: None,
            intent_source: IntentSource::Default,
        }
    }

    async fn budget_dispatch(
        registry: &RuntimeRegistry,
        card: &ComponentCard,
        model: Option<&str>,
    ) -> Result<idoris_backend::ChatResponse, DispatchFailure> {
        dispatch_local(
            std::slice::from_ref(card),
            registry.get(&card.provider.id),
            None,
            &profile(),
            DispatchInput::with_model(model, "hello"),
            vec![ChatMessage {
                role: "user".into(),
                content: "hello".into(),
            }],
            CancellationToken::new(),
        )
        .await
        .unwrap()
        .result
    }

    #[tokio::test]
    async fn allocated_budget_zero_and_individually_oversized_models_never_reach_adapter() {
        // Include a sub-epsilon positive footprint: zero means no positive loads.
        for (bytes, memory_gb) in [(0, 1.0), (0, 1e-9), (1, 500e-9), (1_000_000_000, 2.0)] {
            let card = named_card("budget-model");
            let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
                id: card.provider.id.clone(),
                memory_gb,
            }]));
            let registry = RuntimeRegistry::spawn_with_factory_and_budget(
                std::slice::from_ref(&card),
                Some(bytes),
                |_| Ok(adapter.clone()),
            )
            .unwrap();
            assert!(
                budget_dispatch(&registry, &card, None).await.is_err(),
                "allocation {bytes} must deny footprint {memory_gb}; adapter loads: {}",
                adapter.load_call_count(&card.provider.id)
            );
            assert_eq!(
                registry
                    .get(&card.provider.id)
                    .unwrap()
                    .status()
                    .await
                    .unwrap()
                    .model_memory_max_gb,
                bytes as f64 / 1_000_000_000.0
            );
            assert_eq!(adapter.load_call_count(&card.provider.id), 0);
            assert_eq!(
                registry
                    .startup_capacity_snapshot()
                    .await
                    .unwrap()
                    .unwrap()
                    .reserved_gb,
                0.0
            );
        }
    }

    #[tokio::test]
    async fn allocated_budget_is_shared_across_dispatch_and_released_after_eviction() {
        let a = named_card("budget-a");
        let b = named_card("budget-b");
        let adapter_a = Arc::new(MockAdapter::new(vec![
            ModelInfo {
                id: a.provider.id.clone(),
                memory_gb: 2.0,
            },
            ModelInfo {
                id: "replacement".into(),
                memory_gb: 2.0,
            },
        ]));
        let adapter_b = Arc::new(MockAdapter::new(vec![ModelInfo {
            id: b.provider.id.clone(),
            memory_gb: 2.0,
        }]));
        adapter_a.set_load_script("replacement", vec![idoris_backend::mock::LoadOutcome::Fail]);
        let registry = RuntimeRegistry::spawn_with_factory_and_budget(
            &[a.clone(), b.clone()],
            Some(2_000_000_000 + HEADROOM_BYTES),
            |card| {
                Ok(if card.provider.id == a.provider.id {
                    adapter_a.clone()
                } else {
                    adapter_b.clone()
                })
            },
        )
        .unwrap();
        assert!(budget_dispatch(&registry, &a, None).await.is_ok());
        assert!(budget_dispatch(&registry, &b, None).await.is_err());
        assert_eq!(adapter_a.load_call_count(&a.provider.id), 1);
        assert_eq!(adapter_b.load_call_count(&b.provider.id), 0);
        assert_eq!(
            registry
                .startup_capacity_snapshot()
                .await
                .unwrap()
                .unwrap()
                .reserved_gb,
            2.0
        );

        // A real eviction unloads A, then a confirmed failed replacement releases its claim.
        assert!(
            budget_dispatch(&registry, &a, Some("replacement"))
                .await
                .is_err()
        );
        assert_eq!(adapter_a.unload_call_count(&a.provider.id), 1);
        assert_eq!(adapter_a.load_call_count("replacement"), 1);
        assert_eq!(
            registry
                .startup_capacity_snapshot()
                .await
                .unwrap()
                .unwrap()
                .reserved_gb,
            0.0
        );
        assert!(budget_dispatch(&registry, &b, None).await.is_ok());
        assert_eq!(adapter_b.load_call_count(&b.provider.id), 1);
    }

    #[tokio::test]
    async fn allocated_budget_concurrent_dispatches_admit_only_one_adapter_load() {
        let a = named_card("concurrent-a");
        let b = named_card("concurrent-b");
        let adapters: BTreeMap<_, _> = [&a, &b]
            .into_iter()
            .map(|card| {
                let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
                    id: card.provider.id.clone(),
                    memory_gb: 1.0,
                }]));
                adapter.set_load_delay(&card.provider.id, std::time::Duration::from_millis(50));
                (card.provider.id.clone(), adapter)
            })
            .collect();
        let registry = RuntimeRegistry::spawn_with_factory_and_budget(
            &[a.clone(), b.clone()],
            Some(1_000_000_000 + HEADROOM_BYTES),
            |card| Ok(adapters[&card.provider.id].clone()),
        )
        .unwrap();
        let (first, second) = tokio::join!(
            budget_dispatch(&registry, &a, None),
            budget_dispatch(&registry, &b, None),
        );
        assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
        let loads: u32 = adapters
            .iter()
            .map(|(id, adapter)| adapter.load_call_count(id))
            .sum();
        assert_eq!(loads, 1, "losing runtime must never call adapter.load");
        assert_eq!(
            registry
                .startup_capacity_snapshot()
                .await
                .unwrap()
                .unwrap()
                .reserved_gb,
            1.0
        );
    }

    #[tokio::test]
    async fn allocated_budget_cancelled_dispatch_retains_engine_residency_charge() {
        let card = named_card("budget-cancel");
        let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
            id: card.provider.id.clone(),
            memory_gb: 1.0,
        }]));
        adapter.set_load_delay(&card.provider.id, std::time::Duration::from_millis(50));
        let registry = RuntimeRegistry::spawn_with_factory_and_budget(
            std::slice::from_ref(&card),
            Some(1_000_000_000 + HEADROOM_BYTES),
            |_| Ok(adapter.clone()),
        )
        .unwrap();
        let task = {
            let registry = registry.clone();
            let card = card.clone();
            tokio::spawn(async move { budget_dispatch(&registry, &card, None).await })
        };
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while adapter.load_call_count(&card.provider.id) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !registry
                .get(&card.provider.id)
                .unwrap()
                .is_ready(&card.provider.id)
                .await
                .unwrap()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            registry
                .startup_capacity_snapshot()
                .await
                .unwrap()
                .unwrap()
                .reserved_gb,
            1.0
        );
        assert_eq!(adapter.load_call_count(&card.provider.id), 1);
        assert!(budget_dispatch(&registry, &card, None).await.is_ok());
        assert_eq!(adapter.load_call_count(&card.provider.id), 1);
    }

    #[tokio::test]
    async fn allocated_budget_positive_byte_boundaries_reach_adapter_only_when_admitted() {
        for (bytes, memory_gb, admitted) in [
            (1, 0.5e-9, true),
            (125_000_000, 0.125, true),
            (125_000_000, 0.125 + 0.5e-9, false),
        ] {
            let card = named_card("byte-boundary");
            let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
                id: card.provider.id.clone(),
                memory_gb,
            }]));
            let registry = RuntimeRegistry::spawn_with_factory_and_budget(
                std::slice::from_ref(&card),
                // Reduced cap (budget minus headroom) is the byte boundary under test.
                Some(bytes + HEADROOM_BYTES),
                |_| Ok(adapter.clone()),
            )
            .unwrap();
            assert_eq!(
                budget_dispatch(&registry, &card, None).await.is_ok(),
                admitted
            );
            assert_eq!(
                adapter.load_call_count(&card.provider.id),
                u32::from(admitted)
            );
            let snapshot = registry.startup_capacity_snapshot().await.unwrap().unwrap();
            assert!(snapshot.reserved_gb <= bytes as f64 / 1e9);
        }
    }

    #[tokio::test]
    async fn remote_kill_switch_startup_sends_no_request_for_untrusted_omlx_http_cards() {
        use idoris_contracts::common::PrivacyClass;
        use idoris_contracts::provider::Locality;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        async fn startup_paths(mut card: ComponentCard, remote_kill_switch: bool) -> Vec<String> {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/api/status"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "loaded_models": [],
                    "model_memory_used": 0,
                    "model_memory_max": 0
                })))
                .mount(&server)
                .await;
            card.endpoint = server.uri();
            let registry = RuntimeRegistry::spawn_with_local_configs_budget_and_kill_switch(
                std::slice::from_ref(&card),
                &BTreeMap::new(),
                None,
                remote_kill_switch,
            )
            .unwrap();
            let _ = registry
                .get(&card.provider.id)
                .expect("omlx http card spawns a lifecycle runtime")
                .status()
                .await;
            server
                .received_requests()
                .await
                .unwrap()
                .into_iter()
                .map(|request| request.url.path().to_string())
                .collect()
        }

        let mut privacy_any_loopback = card();
        privacy_any_loopback.privacy_class = PrivacyClass::Any;
        privacy_any_loopback.provider.privacy_class = PrivacyClass::Any;
        let mut lan = card();
        lan.provider.locality = Locality::Lan;

        for untrusted in [privacy_any_loopback, lan] {
            let suppressed = startup_paths(untrusted.clone(), true).await;
            assert!(
                suppressed.is_empty(),
                "kill switch on must send 0 requests, including /api/status, got {suppressed:?}"
            );
            let open = startup_paths(untrusted, false).await;
            assert!(
                open.iter().any(|path| path == "/api/status"),
                "kill switch off must probe /api/status, got {open:?}"
            );
        }

        let trusted = startup_paths(card(), true).await;
        assert!(
            trusted.iter().any(|path| path == "/api/status"),
            "trusted loopback omlx card must still be probed, got {trusted:?}"
        );
    }
    #[tokio::test]
    async fn allocated_budget_none_preserves_default_load_behavior() {
        let card = named_card("budget-default");
        let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
            id: card.provider.id.clone(),
            // The historical positive-budget epsilon still applies for None.
            memory_gb: SupervisorConfig::default().budget_gb + 500e-9,
        }]));
        let registry = RuntimeRegistry::spawn_with_factory_and_budget(
            std::slice::from_ref(&card),
            None,
            |_| Ok(adapter.clone()),
        )
        .unwrap();
        assert!(budget_dispatch(&registry, &card, None).await.is_ok());
        let snapshot = registry.startup_capacity_snapshot().await.unwrap().unwrap();
        assert_eq!(snapshot.budget_gb, SupervisorConfig::default().budget_gb);
        assert!(snapshot.reserved_gb > snapshot.budget_gb);
        assert!(snapshot.reserved_gb <= snapshot.budget_gb + 1e-6);
        assert_eq!(adapter.load_call_count(&card.provider.id), 1);
    }

    #[tokio::test]
    async fn on_demand_and_omitted_policy_construct_handles() {
        let mut card = card();
        assert!(spawn_runtime(&card).expect("on-demand runtime").is_some());
        card.load_policy = None;
        assert!(
            spawn_runtime(&card)
                .expect("default lifecycle runtime")
                .is_some()
        );
    }

    #[tokio::test]
    async fn unknown_lifecycle_provider_is_rejected() {
        let mut card = card();
        card.provider.id = "unknown".into();
        let error = spawn_runtime(&card).expect_err("unknown provider rejected");
        assert!(error.contains("provider unknown"));
    }

    #[test]
    fn resident_http_service_uses_proxy_path() {
        let mut card = card();
        card.provider.id = "generic".into();
        card.load_policy = Some(LoadPolicy {
            mode: LoadMode::Resident,
            keepalive: Keepalive::Pinned { pinned: true },
            admission: Admission::Coexist,
        });
        assert!(
            spawn_runtime(&card)
                .expect("resident HTTP bypass")
                .is_none()
        );
    }

    #[test]
    fn unsupported_non_http_form_is_rejected() {
        let mut card = card();
        card.form = Form::SpawnCli;
        assert!(
            spawn_runtime(&card)
                .unwrap_err()
                .contains("no runtime constructor")
        );
    }

    #[tokio::test]
    async fn registry_keeps_mock_supervisors_bound_to_their_provider() {
        let a = named_card("a");
        let b = named_card("b");
        let registry = RuntimeRegistry {
            supervisors: BTreeMap::from([
                ("a".into(), mock_bound(&a)),
                ("b".into(), mock_bound(&b)),
            ]),
            startup_capacity: None,
            ready_snapshot_calls: Arc::default(),
        };
        for card in [&a, &b] {
            let outcome = dispatch_local(
                std::slice::from_ref(card),
                registry.get(&card.provider.id),
                None,
                &profile(),
                DispatchInput::new("hello"),
                vec![ChatMessage {
                    role: "user".into(),
                    content: "hello".into(),
                }],
                CancellationToken::new(),
            )
            .await
            .unwrap();
            let response = outcome.result.unwrap();
            assert_eq!(response.model, card.provider.id);
        }
    }

    #[tokio::test]
    async fn exact_ready_snapshot_requires_the_requested_model_to_be_ready() {
        let card = named_card("runtime-b");
        let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
            id: "target".into(),
            memory_gb: 1.0,
        }]));
        let handle = Supervisor::spawn(adapter, SupervisorConfig::default()).unwrap();
        handle
            .load("target", 1.0, card.load_policy.unwrap())
            .await
            .unwrap();
        let registry = RuntimeRegistry::from(BoundSupervisor::new(&card, handle));

        let ready = registry
            .exact_ready_providers(vec![
                ("runtime-b".into(), "target".into()),
                ("runtime-b".into(), "other".into()),
            ])
            .await;
        assert_eq!(ready, ["runtime-b".to_string()].into());
    }

    #[tokio::test]
    async fn startup_inherited_residency_is_not_claimed_as_chat_ready() {
        let card = named_card("runtime-b");
        let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
            id: "target".into(),
            memory_gb: 1.0,
        }]));
        adapter
            .load("target", card.load_policy.as_ref())
            .await
            .unwrap();
        let handle = Supervisor::spawn(adapter, SupervisorConfig::default()).unwrap();
        let registry = RuntimeRegistry::from(BoundSupervisor::new(&card, handle));

        assert!(
            registry
                .exact_ready_providers(vec![("runtime-b".into(), "target".into())])
                .await
                .is_empty()
        );
    }

    #[test]
    fn readiness_query_errors_are_neutral_for_affinity() {
        assert!(!ready_for_affinity(Err(BackendError::internal(
            "readiness unavailable"
        ))));
    }

    #[tokio::test]
    async fn resident_direct_path_never_receives_lifecycle_readiness_boost() {
        let mut resident = named_card("resident");
        resident.load_policy.as_mut().unwrap().mode = LoadMode::Resident;
        let registry = RuntimeRegistry::spawn_with_factory(std::slice::from_ref(&resident), |_| {
            panic!("Resident direct path must not construct a lifecycle runtime")
        })
        .unwrap();

        assert!(
            registry
                .exact_ready_providers(vec![("resident".into(), "resident".into())])
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn injected_factory_constructs_non_omlx_lifecycle_runtime_without_card_extensions() {
        let mut custom = named_card("local-http");
        custom.extensions = None;
        let registry = RuntimeRegistry::spawn_with_factory(std::slice::from_ref(&custom), |card| {
            Ok(Arc::new(MockAdapter::new(vec![ModelInfo {
                id: card.provider.id.clone(),
                memory_gb: 2.0,
            }])) as Arc<dyn RuntimeAdapter>)
        })
        .unwrap();

        assert_eq!(registry.len(), 1);
        assert!(registry.get("local-http").is_some());
    }

    #[tokio::test]
    async fn registry_tracks_each_runtime_startup_in_one_shared_capacity_snapshot() {
        let a = named_card("runtime-a");
        let b = named_card("runtime-b");
        let registry = RuntimeRegistry::spawn_with_factory(&[a, b], |card| {
            Ok(Arc::new(MockAdapter::new(vec![ModelInfo {
                id: card.provider.id.clone(),
                memory_gb: 2.0,
            }])) as Arc<dyn RuntimeAdapter>)
        })
        .unwrap();

        let snapshot = registry.startup_capacity_snapshot().await.unwrap().unwrap();
        assert_eq!(snapshot.budget_gb, SupervisorConfig::default().budget_gb);
        assert_eq!(snapshot.reserved_gb, 0.0);
        assert_eq!(snapshot.allocations["runtime:runtime-a"], 0.0);
        assert_eq!(snapshot.allocations["runtime:runtime-b"], 0.0);
    }

    #[tokio::test]
    async fn injected_factory_is_not_called_for_subscription_or_resident_proxy_cards() {
        let mut resident = named_card("resident");
        resident.load_policy.as_mut().unwrap().mode = LoadMode::Resident;
        let mut subscription = named_card(idoris_policy::SUBSCRIPTION_PROVIDER_ID);
        subscription.load_policy = None;
        let calls = std::sync::atomic::AtomicUsize::new(0);

        let registry = RuntimeRegistry::spawn_with_factory(&[resident, subscription], |_| {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(BackendError::internal("factory must not run"))
        })
        .unwrap();

        assert!(registry.is_empty());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn local_config_requires_exact_card_binding() {
        let mut custom = named_card("local-http");
        custom.endpoint = "http://127.0.0.1:18111".into();
        let launch = idoris_upstream::LocalRuntimeLaunch::new(
            idoris_upstream::LocalRuntimeKind::MlxLmServer,
            "/usr/bin/python3",
            "/models/qwen-mlx",
            18111,
        )
        .unwrap();
        let fences = tempfile::tempdir().unwrap();
        let config = LocalHttpRuntimeConfig::new(
            launch,
            "local-http",
            2.0,
            fences.path().join("default.pending"),
        )
        .unwrap();
        let mut configs = BTreeMap::from([("local-http".to_string(), config)]);
        assert!(RuntimeRegistry::spawn_with_local_configs(&[custom.clone()], &configs).is_ok());
        configs.get_mut("local-http").unwrap().load_fence_path =
            fences.path().join("allocated.pending");
        let allocated = RuntimeRegistry::spawn_with_local_configs_and_budget(
            std::slice::from_ref(&custom),
            &configs,
            Some(1_000_000_000),
        )
        .unwrap();
        assert_eq!(
            allocated
                .startup_capacity_snapshot()
                .await
                .unwrap()
                .unwrap()
                .budget_gb,
            1.0
        );
        assert_eq!(
            allocated
                .get("local-http")
                .unwrap()
                .status()
                .await
                .unwrap()
                .model_memory_max_gb,
            1.0
        );
        let outcome = budget_dispatch(&allocated, &custom, None).await;
        assert!(matches!(
            outcome,
            Err(DispatchFailure::Backend(
                BackendError::EvictionImpossible { .. }
            ))
        ));

        custom.endpoint = "http://127.0.0.1:19999".into();
        assert!(RuntimeRegistry::spawn_with_local_configs(&[custom], &configs).is_err());
        assert!(RuntimeRegistry::spawn_with_local_configs(&[], &configs).is_err());
    }

    fn local_runtime_config(
        provider: &str,
        port: u16,
        dir: &std::path::Path,
    ) -> LocalHttpRuntimeConfig {
        let launch = idoris_upstream::LocalRuntimeLaunch::new(
            idoris_upstream::LocalRuntimeKind::MlxLmServer,
            "/usr/bin/python3",
            "/models/qwen-mlx",
            port,
        )
        .unwrap();
        LocalHttpRuntimeConfig::new(
            launch,
            provider,
            2.0,
            dir.join(format!("{provider}.pending")),
        )
        .unwrap()
    }

    fn managed_named_card(id: &str, port: u16) -> ComponentCard {
        let mut card = named_card(id);
        card.endpoint = format!("http://127.0.0.1:{port}");
        card.extensions = Some(
            serde_json::json!({"idoris_managed": {"engine": "omlx", "mode": "auto"}})
                .as_object()
                .unwrap()
                .clone(),
        );
        card
    }

    /// 规范勘误十二（3）（L4）：本地 runtime 配置绑在托管卡（`extensions.idoris_managed`）上，启动时就拒绝：托管卡的引擎由引擎管理器
    /// 拉起与停止，本地 runtime 配置是另一条拉起路径，不能等降级放行时默默把配置丢掉。两个入口（有托管卡的启动校验、没有托管卡的
    /// 直接建表）都拒绝；同一份配置绑在不托管的卡上是合法的（对照）。
    #[tokio::test]
    async fn a_local_runtime_config_bound_to_a_managed_card_is_refused_at_start_up() {
        let managed = managed_named_card("managed-local", 18121);
        let dir = tempfile::tempdir().unwrap();
        let configs = BTreeMap::from([(
            "managed-local".to_string(),
            local_runtime_config("managed-local", 18121, dir.path()),
        )]);
        let error =
            RuntimeRegistry::validate_local_configs(std::slice::from_ref(&managed), &configs)
                .unwrap_err();
        assert!(error.contains("managed-local"), "{error}");
        assert!(error.contains("托管"), "{error}");
        assert!(error.contains("idoris_managed"), "{error}");
        let error =
            RuntimeRegistry::spawn_with_local_configs(std::slice::from_ref(&managed), &configs)
                .unwrap_err();
        assert!(error.contains("托管"), "{error}");

        // 对照：同一份配置，卡不声明托管：合法。
        let mut plain = managed.clone();
        plain.extensions = None;
        RuntimeRegistry::validate_local_configs(std::slice::from_ref(&plain), &configs).unwrap();
        assert!(
            RuntimeRegistry::spawn_with_local_configs(std::slice::from_ref(&plain), &configs)
                .is_ok()
        );
    }

    /// 降级放行（勘误十二（3））：没就绪的托管卡不建 Supervisor，别的卡照常建；本地 runtime 配置表原样不动（它从不绑托管卡，
    /// 所以第二次校验必然通过，不会再把合法的配置变成启动失败）。
    #[tokio::test]
    async fn a_degraded_gate_leaves_the_managed_card_out_and_builds_every_other_card() {
        let managed = managed_named_card("omlx", 18121);
        let mut healthy = named_card("healthy-local");
        healthy.endpoint = "http://127.0.0.1:18122".into();
        let fences = tempfile::tempdir().unwrap();
        let configs = BTreeMap::from([(
            "healthy-local".to_string(),
            local_runtime_config("healthy-local", 18122, fences.path()),
        )]);
        let original = [managed, healthy];
        RuntimeRegistry::validate_local_configs(&original, &configs).unwrap();

        let registry = RuntimeRegistry::spawn_after_gate(
            &original,
            &configs,
            |provider_id| provider_id == "omlx",
            None,
            false,
            &AdapterParams::default(),
        )
        .unwrap();
        assert!(
            registry.get("omlx").is_none(),
            "the degraded managed card gets no Supervisor"
        );
        let healthy_runtime = registry
            .get("healthy-local")
            .expect("the healthy card is served");
        assert!(
            healthy_runtime.status().await.is_ok(),
            "its Supervisor is alive"
        );
        assert_eq!(configs.len(), 1, "the table is not touched");

        // 都没降级：两张卡都建。全降级：只剩带本地配置的那张（它不是托管卡，门控降级与它无关）。
        let all = RuntimeRegistry::spawn_after_gate(
            &original,
            &configs,
            |_| false,
            None,
            false,
            &AdapterParams::default(),
        )
        .unwrap();
        assert_eq!(all.len(), 2);
        let managed_degraded_only = RuntimeRegistry::spawn_after_gate(
            &original,
            &configs,
            |provider_id| provider_id == "omlx",
            None,
            false,
            &AdapterParams::default(),
        )
        .unwrap();
        assert_eq!(managed_degraded_only.len(), 1);
    }

    /// L3：启动时对**每一张**要 Supervisor 的卡干跑一遍适配器的构造（和门控放行时建表走同一条路 `adapter_for`），建不出适配器的卡
    /// （`no adapter for provider`、`no runtime constructor`）是启动失败，不是门控放行之后才发现的错误（降级投影甚至能把它藏起来）。
    /// 不要 Supervisor 的卡（订阅中转、resident 直连）不被干跑；带自己本地配置的卡由那份配置建，不看工厂。
    #[test]
    fn resolving_the_adapter_params_dry_runs_the_adapter_construction_of_every_supervisor_card() {
        let good = named_card("omlx");
        let no_adapter = named_card("vllm-lifecycle");
        // 好卡在前、坏卡在后：错误不被前面的好卡遮住；换个顺序也一样。
        for cards in [
            [good.clone(), no_adapter.clone()],
            [no_adapter.clone(), good.clone()],
        ] {
            let error =
                RuntimeRegistry::resolve_adapter_params(&cards, &BTreeMap::new()).unwrap_err();
            assert!(error.contains("vllm-lifecycle"), "{error}");
            assert!(error.contains("no adapter for provider"), "{error}");
        }
        // 没有 runtime 构造器的形态：同样是启动失败（以前这里不报错，等门控放行时才报）。
        let mut cli = named_card("cli-thing");
        cli.form = Form::SpawnCli;
        let error = RuntimeRegistry::resolve_adapter_params(&[good.clone(), cli], &BTreeMap::new())
            .unwrap_err();
        assert!(error.contains("cli-thing"), "{error}");
        assert!(error.contains("has no runtime constructor"), "{error}");
        // 全是好卡：通过，参数是环境解析出来的（这里没设：默认值）。
        assert_eq!(
            RuntimeRegistry::resolve_adapter_params(std::slice::from_ref(&good), &BTreeMap::new())
                .unwrap(),
            AdapterParams::default()
        );
        // 不要 Supervisor 的卡不被干跑：resident 直连与订阅中转，哪怕它们的 provider id 没有适配器。
        let mut resident = named_card("resident-thing");
        resident.load_policy.as_mut().unwrap().mode = LoadMode::Resident;
        let mut subscription = named_card(idoris_policy::SUBSCRIPTION_PROVIDER_ID);
        subscription.load_policy = None;
        RuntimeRegistry::resolve_adapter_params(&[resident, subscription], &BTreeMap::new())
            .unwrap();
        // 带自己本地配置的卡由那份配置建，不走工厂：provider id 没有适配器也不是错误。
        let mut custom = named_card("local-http");
        custom.endpoint = "http://127.0.0.1:18111".into();
        let dir = tempfile::tempdir().unwrap();
        let configs = BTreeMap::from([(
            "local-http".to_string(),
            local_runtime_config("local-http", 18111, dir.path()),
        )]);
        RuntimeRegistry::resolve_adapter_params(&[custom], &configs).unwrap();
    }

    /// 哪些卡要 Supervisor（`uses_supervisor`，`spawn_runtime_with_factory` 与启动时的参数解析共用这一个判据）：订阅中转与 resident
    /// 直连卡不要；其余必须是 `http_service`，别的形态没有 runtime 构造器，是启动失败（建表与启动时的参数解析都报）。
    #[test]
    fn only_http_service_lifecycle_cards_get_a_supervisor_and_other_forms_have_no_constructor() {
        assert_eq!(uses_supervisor(&named_card("lifecycle")), Ok(true));
        let mut resident = named_card("resident");
        resident.load_policy.as_mut().unwrap().mode = LoadMode::Resident;
        assert_eq!(uses_supervisor(&resident), Ok(false));
        let mut subscription = named_card(idoris_policy::SUBSCRIPTION_PROVIDER_ID);
        subscription.load_policy = None;
        assert_eq!(uses_supervisor(&subscription), Ok(false));

        let mut cli = named_card("cli-thing");
        cli.form = Form::SpawnCli;
        let error = uses_supervisor(&cli).unwrap_err();
        assert!(error.contains("cli-thing"), "{error}");
        assert!(error.contains("has no runtime constructor"), "{error}");
        // 建运行时登记报同一个错误（启动失败）；启动时的参数解析（L3）对这张卡也报同一个错误，不读环境、不等门控放行。
        let error = RuntimeRegistry::spawn_with_factory(std::slice::from_ref(&cli), |_| {
            Err(BackendError::internal("factory must not run"))
        })
        .unwrap_err();
        assert!(error.contains("has no runtime constructor"), "{error}");
        let error =
            RuntimeRegistry::resolve_adapter_params(std::slice::from_ref(&cli), &BTreeMap::new())
                .unwrap_err();
        assert!(error.contains("has no runtime constructor"), "{error}");
    }

    #[tokio::test]
    async fn missing_runtime_fails_closed_without_breaking_another_backend() {
        let a = named_card("a");
        let b = named_card("b");
        let registry: RuntimeRegistry = mock_bound(&a).into();
        let missing = dispatch_local(
            std::slice::from_ref(&b),
            registry.get("b"),
            None,
            &profile(),
            DispatchInput::new(""),
            Vec::new(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(matches!(
            missing.result,
            Err(DispatchFailure::Backend(ref err))
                if err.reason_code() == "supervisor_unavailable"
        ));

        let healthy = dispatch_local(
            std::slice::from_ref(&a),
            registry.get("a"),
            None,
            &profile(),
            DispatchInput::new("hello"),
            vec![ChatMessage {
                role: "user".into(),
                content: "hello".into(),
            }],
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(healthy.result.unwrap().model, "a");
    }

    #[test]
    fn queue_depth_excludes_a_failed_backend_without_losing_healthy_depth() {
        let healthy = idoris_backend::BackendStatus {
            pressure: idoris_backend::Pressure::Ok,
            used_gb: 2.0,
            model_memory_max_gb: 24.0,
            loaded: vec!["a".into(), "b".into()],
        };
        let depth = add_status_depth(0, "healthy", Ok(healthy));
        let depth = add_status_depth(
            depth,
            "failed",
            Err(idoris_backend::BackendError::supervisor_unavailable()),
        );
        assert_eq!(depth, 2);
    }
}
