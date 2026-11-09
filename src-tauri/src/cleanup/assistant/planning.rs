//! Request budgets and the local, credential-free planning audit trail.
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(crate) const DEFAULT_CONTEXT_TOKENS: usize = 32_768;
pub(super) const MAX_INPUT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub(in crate::cleanup) struct Budget {
    pub context_tokens: usize,
    pub output_tokens: usize,
}

impl Budget {
    pub fn new(context_tokens: usize) -> Self {
        Self {
            context_tokens,
            output_tokens: (context_tokens / 3).clamp(1024, 16_384),
        }
    }

    pub fn fits(self, input: &str, instructions: &str, schema: &Value) -> bool {
        input.len() <= MAX_INPUT_BYTES
            && self.estimate(input, instructions, schema) + self.output_tokens + 2048
                <= self.context_tokens
    }

    pub fn estimate(self, input: &str, instructions: &str, schema: &Value) -> usize {
        estimate_tokens(input)
            + estimate_tokens(instructions)
            + estimate_tokens(&schema.to_string())
    }
}

impl Default for Budget {
    fn default() -> Self {
        Self::new(DEFAULT_CONTEXT_TOKENS)
    }
}

// This is deliberately labelled an estimate, not an advertised model limit.
// Non-ASCII bytes get extra headroom. A provider context rejection triggers
// smaller sections; Cutline never truncates transcript text to make it fit.
fn estimate_tokens(text: &str) -> usize {
    let ascii = text.bytes().filter(u8::is_ascii).count();
    ascii.div_ceil(3) + text.len() - ascii
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Diagnostics {
    pub strategy: String,
    pub provider: String,
    pub model: String,
    pub timeline_key: String,
    pub total_utterances: usize,
    pub reviewed_utterances: usize,
    pub context_budget_tokens: usize,
    pub output_budget_tokens: usize,
    pub fallback_reason: Option<String>,
    pub reviews: Vec<Review>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Review {
    pub scope: String,
    pub start: f64,
    pub end: f64,
    pub utterances: usize,
    pub completed: bool,
    pub error: Option<String>,
    pub edits: usize,
    pub passes: Vec<Pass>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Pass {
    pub input_bytes: usize,
    pub estimated_input_tokens: usize,
    pub response: Value,
    pub validation: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn budget_includes_instructions_schema_and_output_headroom() {
        let budget = Budget::new(8192);
        assert!(budget.fits("A short transcript", "Instructions", &json!({})));
        assert!(!budget.fits("small", &"a".repeat(30_000), &json!({})));
        assert!(!budget.fits("small", "", &json!({"schema": "a".repeat(30_000)})));
        assert!(!budget.fits(&"a".repeat(MAX_INPUT_BYTES + 1), "", &json!({})));
        assert!(estimate_tokens("हिन्दी") > estimate_tokens("speech"));
    }
}
