//! Manual same-machine timing; structural scan-count regression is a unit test.
use idoris_policy::inspect::{InspectCtx, inspect};
use std::time::Instant;

#[test]
#[ignore = "manual benchmark; no flaky wall-clock assertion in regular CI"]
fn inspect_cost_by_input_shape() {
    for (name, unit) in [
        ("prose", "Ordinary public prose without secrets. "),
        ("numeric_rows", "2024 1203 1306 2001\n"),
        ("base64_tokens", "aGVsbG8gd29ybGQgdGhpcyBpcyBwdWJsaWM= "),
        ("long_token", "a"),
        ("unicode_escapes", r"\u4e2d\u6587 "),
    ] {
        let text = unit.repeat(240_000 / unit.len());
        let mut samples = Vec::new();
        for _ in 0..3 {
            let now = Instant::now();
            std::hint::black_box(inspect(&text, &InspectCtx::fail_closed()));
            samples.push(now.elapsed().as_micros());
        }
        samples.sort_unstable();
        println!("{name}: {} bytes, median_us={}", text.len(), samples[1]);
    }
}
