pub mod detector;
pub mod embedding;

use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::OnceLock;

use idoris_backend::ChatMessage;
use idoris_contracts::Contract;
use idoris_contracts::common::PrivacyClass;

use crate::profile::{IntentSource, ParsedProfile};
use detector::{Embedder, IntentHit, IntentMessage, UtteranceIntentDetector, UtteranceRoute};
use embedding::hashing_embedding;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectorLocality {
    Loopback,
    Remote,
}

/// Wiring boundary around an intent detector. The request path only needs
/// locality plus one detection call; concrete embedding behavior stays in
/// `detector.rs` (B1 task 10).
pub trait ProfileIntentDetector: Send + Sync {
    fn locality(&self) -> DetectorLocality;

    fn detect<'a>(
        &'a self,
        messages: &'a [ChatMessage],
    ) -> Pin<Box<dyn Future<Output = Result<Option<IntentHit>, String>> + Send + 'a>>;
}

struct HashingEmbedder;

impl Embedder for HashingEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f64>>, String> {
        let dim =
            NonZeroUsize::new(256).ok_or_else(|| "invalid embedding dimension".to_string())?;
        Ok(texts
            .iter()
            .map(|text| hashing_embedding(text, dim))
            .collect())
    }
}

struct LocalDetector<E> {
    inner: UtteranceIntentDetector<E>,
}

impl<E: Embedder> ProfileIntentDetector for LocalDetector<E> {
    fn locality(&self) -> DetectorLocality {
        DetectorLocality::Loopback
    }

    fn detect<'a>(
        &'a self,
        messages: &'a [ChatMessage],
    ) -> Pin<Box<dyn Future<Output = Result<Option<IntentHit>, String>> + Send + 'a>> {
        Box::pin(async move {
            let context = messages
                .iter()
                .map(|message| IntentMessage {
                    role: &message.role,
                    content: &message.content,
                })
                .collect::<Vec<_>>();
            self.inner.detect(&context).await
        })
    }
}

fn default_detector() -> &'static LocalDetector<HashingEmbedder> {
    static DETECTOR: OnceLock<LocalDetector<HashingEmbedder>> = OnceLock::new();
    DETECTOR.get_or_init(|| LocalDetector {
        inner: UtteranceIntentDetector::new(
            vec![
                route(
                    "web_search",
                    &[
                        "搜索一下最新的资料",
                        "帮我上网查一下",
                        "search the web for this",
                        "查一下网上的说法",
                    ],
                ),
                route(
                    "coding",
                    &[
                        "写一个函数",
                        "帮我实现这个功能",
                        "fix this bug",
                        "重构这段代码",
                        "implement this in Rust",
                    ],
                ),
                route(
                    "email",
                    &[
                        "帮我发一封邮件",
                        "send an email to the team",
                        "回复这封邮件",
                        "draft a reply to this message",
                    ],
                ),
                route(
                    "agent_task",
                    &[
                        "帮我规划并执行这个任务",
                        "plan and execute this step by step",
                        "一步一步帮我做完这件事",
                    ],
                ),
            ],
            HashingEmbedder,
            0.65,
        ),
    })
}

fn route(intent: &str, utterances: &[&str]) -> UtteranceRoute {
    UtteranceRoute {
        intent: intent.to_string(),
        utterances: utterances.iter().map(|text| (*text).to_string()).collect(),
    }
}

fn has_detectable_text(messages: &[ChatMessage]) -> bool {
    let context = messages
        .iter()
        .map(|message| IntentMessage {
            role: &message.role,
            content: &message.content,
        })
        .collect::<Vec<_>>();
    !detector::query_text(&context).is_empty()
}

/// Applies fallback intent detection without allowing it to rewrite any other
/// control-plane field. Explicit headers always win. Detector failures,
/// misses, and invalid intents preserve the parsed `chat` default.
pub async fn resolve_profile(mut parsed: ParsedProfile, messages: &[ChatMessage]) -> ParsedProfile {
    resolve_profile_with_detector(&mut parsed, messages, default_detector()).await;
    parsed
}

async fn resolve_profile_with_detector(
    parsed: &mut ParsedProfile,
    messages: &[ChatMessage],
    detector: &impl ProfileIntentDetector,
) {
    if parsed.intent_source == IntentSource::Header
        || (parsed.task.privacy.unwrap_or(PrivacyClass::LocalOnly) == PrivacyClass::LocalOnly
            && detector.locality() == DetectorLocality::Remote)
        || !has_detectable_text(messages)
    {
        return;
    }
    let Ok(Some(hit)) = detector.detect(messages).await else {
        return;
    };
    let mut candidate = parsed.task.clone();
    candidate.intent = Some(hit.intent);
    if candidate.validate().is_err() {
        return;
    }
    parsed.task = candidate;
    parsed.intent_source = IntentSource::Detected;
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use idoris_contracts::TaskProfile;

    use super::*;

    struct FakeDetector {
        locality: DetectorLocality,
        calls: AtomicUsize,
        result: Result<Option<IntentHit>, String>,
    }

    impl ProfileIntentDetector for FakeDetector {
        fn locality(&self) -> DetectorLocality {
            self.locality
        }

        fn detect<'a>(
            &'a self,
            _messages: &'a [ChatMessage],
        ) -> Pin<Box<dyn Future<Output = Result<Option<IntentHit>, String>> + Send + 'a>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let result = match &self.result {
                Ok(Some(hit)) => Ok(Some(IntentHit {
                    intent: hit.intent.clone(),
                    score: hit.score,
                })),
                Ok(None) => Ok(None),
                Err(message) => Err(message.clone()),
            };
            Box::pin(async move { result })
        }
    }

    fn parsed(privacy: PrivacyClass, source: IntentSource) -> ParsedProfile {
        ParsedProfile {
            task: TaskProfile {
                privacy: Some(privacy),
                intent: Some(
                    if source == IntentSource::Header {
                        "email"
                    } else {
                        "chat"
                    }
                    .into(),
                ),
                ..TaskProfile::default()
            },
            role: None,
            tenant_id: None,
            intent_source: source,
        }
    }

    fn messages(content: &str) -> Vec<ChatMessage> {
        vec![ChatMessage {
            role: "user".into(),
            content: content.into(),
        }]
    }

    fn fake(locality: DetectorLocality, result: Result<Option<IntentHit>, String>) -> FakeDetector {
        FakeDetector {
            locality,
            calls: AtomicUsize::new(0),
            result,
        }
    }

    #[tokio::test]
    async fn explicit_header_never_calls_detector() {
        let detector = fake(
            DetectorLocality::Loopback,
            Ok(Some(IntentHit {
                intent: "coding".into(),
                score: 1.0,
            })),
        );
        let mut profile = parsed(PrivacyClass::Any, IntentSource::Header);
        resolve_profile_with_detector(&mut profile, &messages("fix this bug"), &detector).await;
        assert_eq!(profile.task.intent.as_deref(), Some("email"));
        assert_eq!(profile.intent_source, IntentSource::Header);
        assert_eq!(detector.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn remote_detector_is_blocked_for_local_only_but_runs_for_any() {
        let detector = fake(
            DetectorLocality::Remote,
            Ok(Some(IntentHit {
                intent: "coding".into(),
                score: 1.0,
            })),
        );
        let mut local = parsed(PrivacyClass::LocalOnly, IntentSource::Default);
        resolve_profile_with_detector(&mut local, &messages("fix this bug"), &detector).await;
        assert_eq!(local.task.intent.as_deref(), Some("chat"));
        assert_eq!(detector.calls.load(Ordering::SeqCst), 0);

        let mut missing_privacy = parsed(PrivacyClass::Any, IntentSource::Default);
        missing_privacy.task.privacy = None;
        resolve_profile_with_detector(&mut missing_privacy, &messages("fix this bug"), &detector)
            .await;
        assert_eq!(missing_privacy.task.intent.as_deref(), Some("chat"));
        assert_eq!(missing_privacy.intent_source, IntentSource::Default);
        assert_eq!(detector.calls.load(Ordering::SeqCst), 0);

        let mut any = parsed(PrivacyClass::Any, IntentSource::Default);
        resolve_profile_with_detector(&mut any, &messages("fix this bug"), &detector).await;
        assert_eq!(any.task.intent.as_deref(), Some("coding"));
        assert_eq!(any.intent_source, IntentSource::Detected);
        assert_eq!(detector.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn detector_error_invalid_intent_and_empty_text_fall_back_to_chat() {
        for result in [
            Err("temporary failure".into()),
            Ok(Some(IntentHit {
                intent: "".into(),
                score: 1.0,
            })),
        ] {
            let detector = fake(DetectorLocality::Loopback, result);
            let mut profile = parsed(PrivacyClass::Any, IntentSource::Default);
            resolve_profile_with_detector(&mut profile, &messages("fix this bug"), &detector).await;
            assert_eq!(profile.task.intent.as_deref(), Some("chat"));
            assert_eq!(profile.intent_source, IntentSource::Default);
        }

        let detector = fake(
            DetectorLocality::Loopback,
            Ok(Some(IntentHit {
                intent: "coding".into(),
                score: 1.0,
            })),
        );
        let mut profile = parsed(PrivacyClass::Any, IntentSource::Default);
        resolve_profile_with_detector(&mut profile, &messages("   "), &detector).await;
        assert_eq!(detector.calls.load(Ordering::SeqCst), 0);
        assert_eq!(profile.task.intent.as_deref(), Some("chat"));
    }

    #[tokio::test]
    async fn default_detector_recognizes_coding() {
        let profile = resolve_profile(
            parsed(PrivacyClass::LocalOnly, IntentSource::Default),
            &messages("fix this bug"),
        )
        .await;
        assert_eq!(profile.task.intent.as_deref(), Some("coding"));
        assert_eq!(profile.intent_source, IntentSource::Detected);
    }
}
