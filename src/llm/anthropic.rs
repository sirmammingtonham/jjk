//! Anthropic Messages API adapter for the [`Splitter`](super::Splitter) port.
//!
//! Prompts live in `prompts/*.md.j2` (rendered with minijinja), never inlined here. Structured
//! output comes from a `record_split` tool; an on-demand `get_atom_detail` tool lets the model pull
//! full hunk text only for atoms it's unsure about. The request is laid out for prompt caching: the
//! stable tools + system prefix carries the cache breakpoint; the volatile catalog follows it.
//!
//! Model and thinking effort adapt to the changeset size and are easily overridden (config or the
//! `JJK_LLM_MODEL` / `JJK_LLM_THINKING` env vars) when a tougher split wants a more powerful model.

use super::{SplitInput, SplitPlan, Splitter};
use crate::error::{JjkError, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

/// How many tool round-trips to allow (detail fetches + the final record_split).
const MAX_ITERS: usize = 6;

// Model ids (latest families). `auto` tiers between haiku and sonnet; override to any id or the
// `opus`/`sonnet`/`haiku` shorthands for a stronger split.
pub const HAIKU: &str = "claude-haiku-4-5-20251001";
pub const SONNET: &str = "claude-sonnet-4-6";
pub const OPUS: &str = "claude-opus-4-8";

/// Default `llm_model` config value: tier automatically by changeset size.
pub const DEFAULT_MODEL: &str = "auto";
/// Default `llm_thinking` config value: scale the thinking budget to changeset complexity.
pub const DEFAULT_THINKING: &str = "auto";

/// Which model to use. `Auto` tiers by atom count; `Fixed` pins one (the override path).
#[derive(Debug, Clone)]
pub enum ModelChoice {
    Auto,
    Fixed(String),
}

impl ModelChoice {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => ModelChoice::Auto,
            "opus" => ModelChoice::Fixed(OPUS.into()),
            "sonnet" => ModelChoice::Fixed(SONNET.into()),
            "haiku" => ModelChoice::Fixed(HAIKU.into()),
            _ => ModelChoice::Fixed(s.trim().to_string()),
        }
    }

    /// Resolve to a concrete model id for a given atom count. Hierarchical split bounds bucket size,
    /// so a single call stays modest; small splits go to the fast tier.
    fn resolve(&self, atoms: usize) -> String {
        match self {
            ModelChoice::Fixed(m) => m.clone(),
            ModelChoice::Auto if atoms <= 20 => HAIKU.to_string(),
            ModelChoice::Auto => SONNET.to_string(),
        }
    }
}

/// How much extended-thinking budget to give the split. `Auto` scales with complexity; `Off`
/// disables it; `Budget` pins a token count (the override path). Effort words map: low/medium/high/max.
#[derive(Debug, Clone)]
pub enum ThinkingChoice {
    Auto,
    Off,
    Budget(u32),
}

impl ThinkingChoice {
    pub fn parse(s: &str) -> Self {
        let t = s.trim().to_ascii_lowercase();
        match t.as_str() {
            "" | "auto" => ThinkingChoice::Auto,
            "off" | "none" | "no" | "0" => ThinkingChoice::Off,
            "low" => ThinkingChoice::Budget(2048),
            "medium" | "med" => ThinkingChoice::Budget(6144),
            "high" => ThinkingChoice::Budget(12288),
            "max" => ThinkingChoice::Budget(24576),
            _ => t
                .parse::<u32>()
                .map(|n| if n == 0 { ThinkingChoice::Off } else { ThinkingChoice::Budget(n) })
                .unwrap_or(ThinkingChoice::Auto),
        }
    }

    /// Resolve to a budget in tokens (0 = no extended thinking). The API floor is 1024.
    fn resolve(&self, atoms: usize) -> u32 {
        match self {
            ThinkingChoice::Off => 0,
            ThinkingChoice::Budget(n) => (*n).max(1024),
            // Small splits don't need it; larger ones scale up to a cap.
            ThinkingChoice::Auto if atoms < 10 => 0,
            ThinkingChoice::Auto => ((atoms as u32) * 256).clamp(2048, 12288),
        }
    }
}

pub struct AnthropicLlm {
    client: reqwest::Client,
    api_key: String,
    model: ModelChoice,
    thinking: ThinkingChoice,
    base_url: String,
    /// Output tokens reserved on top of the thinking budget for the response itself.
    output_reserve: u32,
}

impl AnthropicLlm {
    pub fn new(api_key: String, model: ModelChoice, thinking: ThinkingChoice) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            model,
            thinking,
            base_url: "https://api.anthropic.com/v1/messages".into(),
            output_reserve: 8192,
        }
    }

    /// Build from `ANTHROPIC_API_KEY` (else `None`, so the caller falls back to a local split).
    /// `model_cfg`/`thinking_cfg` come from config or the `JJK_LLM_*` env overrides.
    pub fn from_env(model_cfg: &str, thinking_cfg: &str) -> Option<Self> {
        let key = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|k| !k.trim().is_empty())?;
        Some(Self::new(
            key,
            ModelChoice::parse(model_cfg),
            ThinkingChoice::parse(thinking_cfg),
        ))
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
        let atoms = input.atoms.len();
        let model = self.model.resolve(atoms);
        let budget = self.thinking.resolve(atoms);

        let system = render_system(&input.mode, input.instruction.as_deref())?;
        let catalog = serde_json::to_string_pretty(input)
            .map_err(|e| JjkError::Msg(format!("serializing split input: {e}")))?;
        let user = render_user(&catalog)?;
        let tools = json!([record_split_tool(), get_atom_detail_tool()]);
        let max_tokens = if budget > 0 { budget + self.output_reserve } else { 8192 };

        let mut messages = vec![json!({
            "role": "user",
            "content": [{"type": "text", "text": user}]
        })];

        // Accumulate token usage across the tool loop so cost & cache hits are visible.
        let mut usage = Usage::default();

        for _ in 0..MAX_ITERS {
            // Cache the stable prefix (tools + system); the volatile catalog message follows it.
            let mut body = json!({
                "model": model,
                "max_tokens": max_tokens,
                "system": [{
                    "type": "text",
                    "text": system,
                    "cache_control": {"type": "ephemeral"}
                }],
                "messages": messages,
                "tools": tools,
            });
            if budget > 0 {
                // Extended thinking requires tool_choice "auto" (no forcing); the system prompt
                // instructs exactly one record_split call.
                body["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
                body["tool_choice"] = json!({"type": "auto"});
            } else {
                body["tool_choice"] = json!({"type": "any"});
            }

            let resp = self.post(&body).await?;
            usage.add(resp.get("usage"));
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
            match tool_use.get("name").and_then(|n| n.as_str()).unwrap_or("") {
                "record_split" => {
                    let plan: SplitPlan =
                        serde_json::from_value(tool_use.get("input").cloned().unwrap_or(Value::Null))
                            .map_err(|e| {
                                JjkError::Msg(format!("record_split output didn't match schema: {e}"))
                            })?;
                    eprintln!(
                        "domain split: model={model} thinking={budget} atoms={atoms} \
                         tokens(in={} out={} cache_read={} cache_write={})",
                        usage.input, usage.output, usage.cache_read, usage.cache_write
                    );
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
                    // Preserve the assistant turn verbatim (incl. any thinking blocks) before replying.
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

/// Token usage accumulated across the split's tool-loop turns (for cost/cache observability).
#[derive(Default)]
struct Usage {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
}

impl Usage {
    fn add(&mut self, usage: Option<&Value>) {
        let Some(u) = usage else { return };
        let g = |k: &str| u.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
        self.input += g("input_tokens");
        self.output += g("output_tokens");
        self.cache_read += g("cache_read_input_tokens");
        self.cache_write += g("cache_creation_input_tokens");
    }
}

/// Render the system prompt from `prompts/system.md.j2`.
pub(crate) fn render_system(mode: &str, instruction: Option<&str>) -> Result<String> {
    render("system", include_str!("prompts/system.md.j2"), minijinja::context! {
        mode => mode,
        instruction => instruction,
    })
}

/// Render the user message (catalog wrapper) from `prompts/user.md.j2`.
fn render_user(catalog: &str) -> Result<String> {
    render("user", include_str!("prompts/user.md.j2"), minijinja::context! {
        catalog => catalog,
    })
}

fn render(name: &'static str, src: &'static str, ctx: minijinja::Value) -> Result<String> {
    let mut env = minijinja::Environment::new();
    env.add_template(name, src)
        .map_err(|e| JjkError::Msg(format!("prompt template '{name}' invalid: {e}")))?;
    let tmpl = env
        .get_template(name)
        .map_err(|e| JjkError::Msg(format!("prompt template '{name}' missing: {e}")))?;
    tmpl.render(ctx)
        .map_err(|e| JjkError::Msg(format!("prompt template '{name}' render failed: {e}")).into())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_choice_parses_and_tiers() {
        assert!(matches!(ModelChoice::parse("auto"), ModelChoice::Auto));
        assert!(matches!(ModelChoice::parse(""), ModelChoice::Auto));
        assert_eq!(ModelChoice::parse("opus").resolve(5), OPUS);
        assert_eq!(ModelChoice::parse("claude-x").resolve(999), "claude-x");
        // auto tiers by size
        assert_eq!(ModelChoice::Auto.resolve(5), HAIKU);
        assert_eq!(ModelChoice::Auto.resolve(40), SONNET);
    }

    #[test]
    fn thinking_choice_parses_and_scales() {
        assert_eq!(ThinkingChoice::parse("off").resolve(100), 0);
        assert_eq!(ThinkingChoice::parse("high").resolve(1), 12288);
        assert_eq!(ThinkingChoice::parse("4000").resolve(1), 4000);
        // auto: tiny → off, larger → scaled within the cap
        assert_eq!(ThinkingChoice::Auto.resolve(3), 0);
        assert_eq!(ThinkingChoice::Auto.resolve(20), (20 * 256u32).clamp(2048, 12288));
        assert_eq!(ThinkingChoice::Auto.resolve(1000), 12288);
        // explicit budget honors the API floor
        assert_eq!(ThinkingChoice::Budget(10).resolve(1), 1024);
    }

    #[test]
    fn system_prompt_renders_mode_and_instruction() {
        let feat = render_system("feature", None).unwrap();
        assert!(feat.contains("FEATURE"));
        assert!(!feat.contains("HIGHEST priority"), "no instruction block when none given");

        let slice = render_system("slice", Some("group by API surface")).unwrap();
        assert!(slice.contains("SLICE"));
        assert!(slice.contains("group by API surface"));
        assert!(slice.contains("record_split"));

        // unknown/default mode → CHANGE
        assert!(render_system("change", None).unwrap().contains("CHANGE"));
    }

    #[test]
    fn user_prompt_embeds_catalog() {
        let u = render_user("{\"atoms\":[]}").unwrap();
        assert!(u.contains("{\"atoms\":[]}"));
    }
}
