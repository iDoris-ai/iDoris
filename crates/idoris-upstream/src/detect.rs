//! Host-fact backend recommendation, matching `packages/adapters/src/detect.ts`.
//! This only selects a slot; it never constructs an adapter or loads a model.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostPlatform {
    Darwin,
    Win32,
    Linux,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostArch {
    Arm64,
    X64,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostFacts {
    pub platform: HostPlatform,
    pub arch: HostArch,
    pub has_nvidia_gpu: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Omlx,
    Vllm,
    LlamaCpp,
    Ollama,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendChoice {
    pub kind: BackendKind,
    /// Whether the recommended slot has an implemented adapter on this host.
    pub implemented: bool,
    pub reason: &'static str,
}

/// Injected facts make the platform matrix testable without probing hardware.
pub fn detect_backend(facts: HostFacts) -> BackendChoice {
    if facts.platform == HostPlatform::Darwin && facts.arch == HostArch::Arm64 {
        return BackendChoice {
            kind: BackendKind::Omlx,
            implemented: true,
            reason: "macOS Apple Silicon: oMLX is the default local backend",
        };
    }
    if matches!(facts.platform, HostPlatform::Win32 | HostPlatform::Linux) && facts.has_nvidia_gpu {
        return BackendChoice {
            kind: BackendKind::Vllm,
            implemented: false,
            reason: "Windows/Linux with NVIDIA: vLLM slot (adapter not implemented yet)",
        };
    }
    BackendChoice {
        kind: BackendKind::LlamaCpp,
        implemented: false,
        reason: "no dedicated GPU / non-Apple-Silicon: llama.cpp (GGUF) slot (adapter not implemented yet)",
    }
}

/// Like TS, GPU detection is currently a conservative `false` placeholder.
/// Reading compile-target facts performs no I/O or model loading.
pub fn current_host_facts() -> HostFacts {
    host_facts(std::env::consts::OS, std::env::consts::ARCH)
}

fn host_facts(os: &str, arch: &str) -> HostFacts {
    HostFacts {
        platform: match os {
            "macos" => HostPlatform::Darwin,
            "windows" => HostPlatform::Win32,
            "linux" => HostPlatform::Linux,
            _ => HostPlatform::Other,
        },
        arch: match arch {
            "aarch64" => HostArch::Arm64,
            "x86_64" => HostArch::X64,
            _ => HostArch::Other,
        },
        has_nvidia_gpu: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use BackendKind::{LlamaCpp, Omlx, Vllm};
    use HostArch::{Arm64, Other as OtherArch, X64};
    use HostPlatform::{Darwin, Linux, Other, Win32};

    #[test]
    fn injected_matrix_matches_ts_kind_and_implemented() {
        // Each row: arm64, x64, other, first without NVIDIA then with it.
        let rows = [
            (Darwin, [Omlx, LlamaCpp, LlamaCpp, Omlx, LlamaCpp, LlamaCpp]),
            (Win32, [LlamaCpp, LlamaCpp, LlamaCpp, Vllm, Vllm, Vllm]),
            (Linux, [LlamaCpp, LlamaCpp, LlamaCpp, Vllm, Vllm, Vllm]),
            (Other, [LlamaCpp; 6]),
        ];
        for (platform, expected) in rows {
            for (index, kind) in expected.into_iter().enumerate() {
                let facts = HostFacts {
                    platform,
                    arch: [Arm64, X64, OtherArch][index % 3],
                    has_nvidia_gpu: index >= 3,
                };
                let choice = detect_backend(facts);
                assert_eq!(choice.kind, kind, "{facts:?}");
                assert_eq!(choice.implemented, kind == Omlx, "{facts:?}");
                assert!(!choice.reason.is_empty());
            }
        }
    }

    #[test]
    fn linux_without_gpu_does_not_claim_omlx_is_implemented() {
        let choice = detect_backend(HostFacts {
            platform: Linux,
            arch: Arm64,
            has_nvidia_gpu: false,
        });
        assert_eq!(choice.kind, LlamaCpp);
        assert!(!choice.implemented);
    }

    #[test]
    fn rust_target_names_map_to_ts_facts_and_unknowns_stay_conservative() {
        for (os, arch, platform, expected_arch) in [
            ("macos", "aarch64", Darwin, Arm64),
            ("macos", "x86_64", Darwin, X64),
            ("windows", "x86_64", Win32, X64),
            ("linux", "aarch64", Linux, Arm64),
            ("linux", "x86_64", Linux, X64),
            ("freebsd", "riscv64", Other, OtherArch),
            ("linux", "x86", Linux, OtherArch),
            ("unknown", "aarch64", Other, Arm64),
        ] {
            assert_eq!(
                host_facts(os, arch),
                HostFacts {
                    platform,
                    arch: expected_arch,
                    has_nvidia_gpu: false
                },
                "{os}/{arch}"
            );
        }
    }

    #[test]
    fn current_host_does_not_assume_a_gpu() {
        let facts = current_host_facts();
        assert!(!facts.has_nvidia_gpu);
        assert_eq!(
            facts,
            host_facts(std::env::consts::OS, std::env::consts::ARCH)
        );
    }
}
