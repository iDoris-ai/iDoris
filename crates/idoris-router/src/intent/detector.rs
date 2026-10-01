//! Example-utterance intent detection, matching `packages/router/src/intent.ts`.

use super::embedding::cosine;
use std::future::Future;
use tokio::sync::OnceCell;

pub trait Embedder: Send + Sync {
    fn embed(&self, texts: &[String])
    -> impl Future<Output = Result<Vec<Vec<f64>>, String>> + Send;
}

pub struct IntentMessage<'a> {
    pub role: &'a str,
    pub content: &'a str,
}

pub struct UtteranceRoute {
    pub intent: String,
    pub utterances: Vec<String>,
}

#[derive(Debug, PartialEq)]
pub struct IntentHit {
    pub intent: String,
    pub score: f64,
}

pub struct UtteranceIntentDetector<E> {
    routes: Vec<UtteranceRoute>,
    embed: E,
    min_score: f64,
    encoded: OnceCell<Vec<Vec<f64>>>,
}

impl<E: Embedder> UtteranceIntentDetector<E> {
    pub fn new(routes: Vec<UtteranceRoute>, embed: E, min_score: f64) -> Self {
        Self {
            routes,
            embed,
            min_score,
            encoded: OnceCell::new(),
        }
    }

    pub async fn detect(&self, ctx: &[IntentMessage<'_>]) -> Result<Option<IntentHit>, String> {
        let query = query_text(ctx);
        if query.is_empty() || !self.min_score.is_finite() {
            return Ok(None);
        }
        let query_vectors = self.embed.embed(&[query.to_owned()]).await?;
        let Some(query_vector) = query_vectors.first() else {
            return Ok(None);
        };
        // Only successful initialization is cached; errors/cancellation allow a retry.
        let vectors = self
            .encoded
            .get_or_try_init(|| async {
                let texts: Vec<String> = self
                    .routes
                    .iter()
                    .flat_map(|r| r.utterances.clone())
                    .collect();
                let vectors = self.embed.embed(&texts).await?;
                if vectors.len() != texts.len() {
                    return Err("embedder vector count does not match utterance count".to_owned());
                }
                Ok(vectors)
            })
            .await?;
        let mut vectors = vectors.iter();
        let mut best: Option<IntentHit> = None;
        for route in &self.routes {
            for vector in vectors.by_ref().take(route.utterances.len()) {
                let score = cosine(query_vector, vector);
                // Strict comparison keeps the first route on a tie, as in TS.
                if score.is_finite() && best.as_ref().is_none_or(|hit| score > hit.score) {
                    best = Some(IntentHit {
                        intent: route.intent.clone(),
                        score,
                    });
                }
            }
        }
        Ok(best.filter(|hit| hit.score >= self.min_score))
    }
}

/// Last nonempty user content is kept verbatim; otherwise trim the final message.
pub fn query_text<'a>(messages: &[IntentMessage<'a>]) -> &'a str {
    for m in messages.iter().rev() {
        if m.role == "user" && !js_trim(m.content).is_empty() {
            return m.content;
        }
    }
    messages.last().map_or("", |m| js_trim(m.content))
}

fn js_trim(text: &str) -> &str {
    text.trim_matches(|c| {
        matches!(c, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}'
        | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}'
        | '\u{205f}' | '\u{3000}' | '\u{feff}')
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct FakeEmbedder {
        calls: Mutex<Vec<Vec<String>>>,
        fail_call: usize,
        short_call: usize,
    }

    impl Embedder for FakeEmbedder {
        async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f64>>, String> {
            tokio::task::yield_now().await;
            let mut calls = self.calls.lock().unwrap();
            calls.push(texts.to_vec());
            if calls.len() == self.fail_call {
                return Err("temporary failure".into());
            }
            if calls.len() == self.short_call {
                return Ok(vec![]);
            }
            Ok(texts
                .iter()
                .map(|t| match t.as_str() {
                    "QUERY" | "A" => vec![1.0, 0.0],
                    "MID" => vec![0.6, 0.8],
                    "B" => vec![0.0, 1.0],
                    "NAN" => vec![f64::NAN, 0.0],
                    _ => vec![0.0, 0.0],
                })
                .collect())
        }
    }
    type Detector = UtteranceIntentDetector<FakeEmbedder>;
    fn detector(routes: &[(&str, &[&str])], fail: usize, short: usize) -> Detector {
        let routes = routes
            .iter()
            .map(|(intent, examples)| UtteranceRoute {
                intent: (*intent).into(),
                utterances: examples.iter().map(|s| (*s).into()).collect(),
            })
            .collect();
        let embed = FakeEmbedder {
            calls: Mutex::default(),
            fail_call: fail,
            short_call: short,
        };
        UtteranceIntentDetector::new(routes, embed, 0.6)
    }

    fn message<'a>(role: &'a str, content: &'a str) -> IntentMessage<'a> {
        IntentMessage { role, content }
    }
    fn user(content: &str) -> [IntentMessage<'_>; 1] {
        [message("user", content)]
    }
    async fn hit(d: &Detector) -> IntentHit {
        d.detect(&user("QUERY")).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn route_maximum_beats_first_example_and_average() {
        let d = detector(
            &[("beta", &["MID"]), ("alpha", &["B", "A"]), ("tie", &["A"])],
            0,
            0,
        );
        assert_eq!(hit(&d).await.intent, "alpha");
        assert_eq!(hit(&d).await.score, 1.0);
    }

    #[tokio::test]
    async fn threshold_is_inclusive_and_low_scores_do_not_hit() {
        let mut d = detector(&[("alpha", &["MID"])], 0, 0);
        assert_eq!(hit(&d).await.score, 0.6);
        d.min_score = 0.600001;
        assert_eq!(d.detect(&user("QUERY")).await.unwrap(), None);
        let d = detector(&[("alpha", &["B"])], 0, 0);
        assert_eq!(d.detect(&user("QUERY")).await.unwrap(), None);
    }

    #[test]
    fn last_nonempty_user_and_fallback_match_ts() {
        let messages = [
            message("user", "old"),
            message("user", " QUERY "),
            message("user", "\u{feff}\t"),
            message("assistant", "wrong"),
        ];
        assert_eq!(query_text(&messages), " QUERY ");
        assert_eq!(query_text(&messages[2..]), "wrong");
        assert_eq!(
            query_text(&[message("system", "\u{feff} fallback \n")]),
            "fallback"
        );
        assert_eq!(query_text(&user("\u{85}")), "\u{85}");
    }

    #[tokio::test]
    async fn empty_text_never_embeds() {
        let d = detector(&[("alpha", &["A"])], 0, 0);
        assert_eq!(d.detect(&[]).await.unwrap(), None);
        assert_eq!(d.detect(&user("\u{feff} \n")).await.unwrap(), None);
        assert!(d.embed.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn successful_examples_are_cached_even_for_concurrent_queries() {
        let d = detector(&[("alpha", &["A", "B"])], 0, 0);
        let messages = user("QUERY");
        let (a, b) = tokio::join!(d.detect(&messages), d.detect(&messages));
        assert_eq!(a.unwrap().unwrap().intent, "alpha");
        assert_eq!(b.unwrap().unwrap().intent, "alpha");
        let calls = d.embed.calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls.iter().filter(|c| **c == ["A", "B"]).count(), 1);
    }

    #[tokio::test]
    async fn first_example_failure_is_retried_then_cached() {
        let d = detector(&[("alpha", &["A", "B"])], 2, 0);
        assert!(d.detect(&user("QUERY")).await.is_err());
        assert_eq!(hit(&d).await.intent, "alpha");
        assert_eq!(hit(&d).await.intent, "alpha");
        let calls = d.embed.calls.lock().unwrap();
        assert_eq!(calls.iter().filter(|c| **c == ["A", "B"]).count(), 2);
        assert_eq!(calls.len(), 5);
    }

    #[tokio::test]
    async fn embedding_failures_and_missing_vectors_can_recover() {
        for (fail, short) in [(1, 0), (0, 1), (0, 2)] {
            let d = detector(&[("alpha", &["A"])], fail, short);
            let result = d.detect(&user("QUERY")).await;
            if short != 1 {
                assert!(result.is_err());
            } else {
                assert_eq!(result.unwrap(), None);
            }
            assert_eq!(hit(&d).await.intent, "alpha");
        }
    }

    #[tokio::test]
    async fn nonfinite_scores_and_thresholds_fail_closed() {
        let d = detector(&[("alpha", &["NAN"])], 0, 0);
        assert_eq!(d.detect(&user("QUERY")).await.unwrap(), None);
        let mut d = detector(&[("alpha", &["A"])], 0, 0);
        d.min_score = f64::NAN;
        assert_eq!(d.detect(&user("QUERY")).await.unwrap(), None);
    }
}
