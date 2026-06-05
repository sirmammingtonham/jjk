//! Anthropic Messages API adapter for the [`Splitter`](super::Splitter) port.
//!
//! Structured output via a forced `record_split` tool; an on-demand `get_atom_detail` tool lets the
//! model pull full hunk text only for atoms it's unsure about (token-efficient — the catalog itself
//! is gists only). The catalog block is marked for prompt caching so incremental re-splits are cheap.

use super::{SplitInput, SplitPlan, Splitter};
use crate::error::{JjkError, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

/// How many tool round-trips to allow before giving up (detail fetches + the final record_split).
const MAX_ITERS: usize = 6;

/// Default model for splitting — a strong-but-efficient balance.
pub const DEFAULT_MODEL: &str = "claude-sonnet-4-6";

pub struct AnthropicLlm {
    client: reqwest::Client,
    api_key: String,
    model: String,
    base_url: String,
    max_tokens: u32,
}

impl AnthropicLlm {
    pub fn new(api_key: String, model: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            model: model.into(),
            base_url: "https://api.anthropic.com/v1/messages".into(),
            max_tokens: 8192,
        }
    }

    /// Build from `ANTHROPIC_API_KEY`, or `None` if unset/empty (caller falls back to a local split).
    pub fn from_env(model: impl Into<String>) -> Option<Self> {
        let key = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|k| !k.trim().is_empty())?;
        Some(Self::new(key, model))
    }

    async fn post(&self, body: &Value) -> Result<Value> {
        let resp = self
            .client
            .post(&self.base_url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(body)
            .send()
            .await
            .map_err(|e| JjkError::Msg(format!("anthropic request failed: {e}")))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| JjkError::Msg(format!("reading anthropic response: {e}")))?;
        if !status.is_success() {
            return Err(JjkError::Msg(format!("anthropic API {status}: {text}")).into());
        }
        serde_json::from_str(&text)
            .map_err(|e| JjkError::Msg(format!("parsing anthropic response: {e}")).into())
    }
}

#[async_trait]
impl Splitter for AnthropicLlm {
    async fn split(&self, input: &SplitInput) -> Result<SplitPlan> {
        let system = system_prompt(&input.mode, input.instruction.as_deref());
        let catalog = serde_json::to_string_pretty(input)
            .map_err(|e| JjkError::Msg(format!("serializing split input: {e}")))?;
        let tools = json!([record_split_tool(), get_atom_detail_tool()]);

        // The big catalog block is cacheable so incremental re-splits hit the prompt cache.
        let mut messages = vec![json!({
            "role": "user",
            "content": [{
                "type": "text",
                "text": format!("Split this changeset.\n\n{catalog}"),
                "cache_control": {"type": "ephemeral"}
            }]
        })];

        for _ in 0..MAX_ITERS {
            let body = json!({
                "model": self.model,
                "max_tokens": self.max_tokens,
                "system": [{
                    "type": "text",
                    "text": system,
                    "cache_control": {"type": "ephemeral"}
                }],
                "messages": messages,
                "tools": tools,
                // Force a tool call each turn (get_atom_detail or record_split) — no free-text dither.
                "tool_choice": {"type": "any"},
            });
            let resp = self.post(&body).await?;
            let content = resp
                .get("content")
                .and_then(|c| c.as_array())
                .cloned()
                .unwrap_or_default();
            let Some(tool_use) = content
                .iter()
                .find(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
            else {
                return Err(JjkError::Msg("anthropic returned no tool call".into()).into());
            };
            let name = tool_use.get("name").and_then(|n| n.as_str()).unwrap_or("");
            match name {
                "record_split" => {
                    let plan: SplitPlan =
                        serde_json::from_value(tool_use.get("input").cloned().unwrap_or(Value::Null))
                            .map_err(|e| {
                                JjkError::Msg(format!("record_split output didn't match schema: {e}"))
                            })?;
                    return Ok(plan);
                }
                "get_atom_detail" => {
                    let id = tool_use
                        .get("id")
                        .and_then(|i| i.as_str())
                        .unwrap_or("")
                        .to_string();
                    let labels: Vec<String> = tool_use
                        .get("input")
                        .and_then(|i| i.get("labels"))
                        .and_then(|l| l.as_array())
                        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                        .unwrap_or_default();
                    let detail = labels
                        .iter()
                        .map(|l| {
                            let body = input.details.get(l).map(String::as_str).unwrap_or("(no detail)");
                            format!("### {l}\n{body}")
                        })
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    messages.push(json!({"role": "assistant", "content": content}));
                    messages.push(json!({
                        "role": "user",
                        "content": [{"type": "tool_result", "tool_use_id": id, "content": detail}]
                    }));
                }
                other => {
                    return Err(JjkError::Msg(format!("anthropic called unexpected tool '{other}'")).into())
                }
            }
        }
        Err(JjkError::Msg("splitter did not produce a plan within the tool-loop budget".into()).into())
    }
}

fn system_prompt(mode: &str, instruction: Option<&str>) -> String {
    let mode_rule = match mode {
        "feature" => "FEATURE mode: produce a few PRs, each a coherent capability/feature.",
        "slice" => "SLICE mode: produce many thin PRs, each a minimal, easily-reviewed increment.",
        _ => "CHANGE mode: produce self-contained changes — each PR one cohesive, independently-mergeable change.",
    };
    let extra = instruction
        .map(|i| format!("\n\nUser's additional instructions (highest priority):\n{i}"))
        .unwrap_or_default();
    format!(
        "You are jjk's stack splitter. You decide how to break ONE branch's changeset into an \
ordered stack of small, independently-reviewable pull requests.\n\n\
You are given a catalog of change *atoms* (each a hunk or a whole file change) with one-line \
gists, plus advisory dependency hints (defuse: one atom uses a symbol another defines; samefile) \
and the author's original commit subjects. You may call `get_atom_detail` to see the full diff of \
specific atoms before deciding — do this ONLY when a gist is insufficient, to keep it cheap.\n\n\
Rules:\n\
- Assign EVERY atom to exactly one layer. Do not drop or duplicate atoms.\n\
- Order layers bottom→top: a layer may depend only on layers BELOW it. Respect defuse hints \
(an atom that uses a symbol should not be in a layer below the one that defines it).\n\
- Each layer must be BACKWARD-COMPATIBLE: safe to merge on its own assuming no later layer exists \
(it must compile and not break trunk). If you can't guarantee this for a layer, set \
backward_compatible=false and explain in compat_notes; prefer merging co-dependent atoms into one \
layer over emitting a broken layer.\n\
- Give each layer a short kebab-case slug, a clear PR title, and a concise PR body explaining the \
slice. Add a one-line rationale.\n\n\
{mode_rule}{extra}\n\n\
When done, call `record_split` exactly once with the full plan."
    )
}

fn record_split_tool() -> Value {
    json!({
        "name": "record_split",
        "description": "Record the final ordered split of the changeset into layers (bottom→top).",
        "input_schema": {
            "type": "object",
            "properties": {
                "layers": {
                    "type": "array",
                    "description": "Layers bottom→top (stack order).",
                    "items": {
                        "type": "object",
                        "properties": {
                            "slug": {"type": "string", "description": "short kebab-case identity"},
                            "atoms": {
                                "type": "array",
                                "items": {"type": "string"},
                                "description": "atom labels in this layer"
                            },
                            "title": {"type": "string", "description": "PR title"},
                            "body": {"type": "string", "description": "PR description"},
                            "rationale": {"type": "string", "description": "why these atoms group together"},
                            "backward_compatible": {"type": "boolean"},
                            "compat_notes": {"type": "string"}
                        },
                        "required": ["slug", "atoms", "title", "body", "backward_compatible"]
                    }
                }
            },
            "required": ["layers"]
        }
    })
}

fn get_atom_detail_tool() -> Value {
    json!({
        "name": "get_atom_detail",
        "description": "Fetch the full diff hunks for specific atoms (by label) when their gist isn't enough to decide placement.",
        "input_schema": {
            "type": "object",
            "properties": {
                "labels": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "atom labels to expand"
                }
            },
            "required": ["labels"]
        }
    })
}
