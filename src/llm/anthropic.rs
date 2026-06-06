//! Anthropic Messages API adapter for the [`Splitter`](super::Splitter) port.
//!
//! Prompts live in `prompts/*.md.j2` (rendered with minijinja), never inlined here. Structured
//! output comes from a `record_split` tool; an on-demand `get_atom_detail` tool lets the model pull
//! full hunk text only for atoms it's unsure about. The request is laid out for prompt caching: the
//! stable tools + system prefix carries the cache breakpoint; the volatile catalog follows it.
//!
//! Model and thinking effort adapt to the changeset size and are easily overridden (config or the
//! `JJK_LLM_MODEL` / `JJK_LLM_THINKING` env vars) when a tougher split wants a more powerful model.

use super::{EdgeHint, SplitInput, SplitPlan, Splitter};
use crate::error::{JjkError, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How many tool round-trips to allow (detail fetches + the final record_split).
const MAX_ITERS: usize = 6;

// Model ids (latest families). `auto` resolves to Sonnet — the right balance of quality and speed
// for splitting — for every changeset; override to any id or the `opus`/`sonnet`/`haiku` shorthands
// (or via config / `JJK_LLM_MODEL`) to pin a different one.
pub const HAIKU: &str = "claude-haiku-4-5-20251001";
pub const SONNET: &str = "claude-sonnet-4-6";
pub const OPUS: &str = "claude-opus-4-8";

/// Default `llm_model` config value: Sonnet (via `auto`).
pub const DEFAULT_MODEL: &str = "auto";
/// Default `llm_thinking` config value: scale the reasoning effort to changeset complexity.
pub const DEFAULT_THINKING: &str = "auto";

/// Which model to use. `Auto` is Sonnet for everything; `Fixed` pins one (the override path).
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

    /// Resolve to a concrete model id. `auto` is always Sonnet — fast enough for interactive use and
    /// strong enough for the splitting task; reach for Opus/Haiku explicitly via the override path.
    fn resolve(&self) -> String {
        match self {
            ModelChoice::Fixed(m) => m.clone(),
            ModelChoice::Auto => SONNET.to_string(),
        }
    }
}

/// Reasoning effort for a split. Maps to the Messages API `output_config.effort` on
/// adaptive-thinking models (Opus / Sonnet 4.6+), or to a legacy extended-thinking
/// `budget_tokens` value on older models (Haiku 4.5), which reject `effort`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effort {
    Off,
    Low,
    Medium,
    High,
    Max,
}

impl Effort {
    /// The `output_config.effort` word, or `None` when thinking is off.
    fn as_word(self) -> Option<&'static str> {
        match self {
            Effort::Off => None,
            Effort::Low => Some("low"),
            Effort::Medium => Some("medium"),
            Effort::High => Some("high"),
            Effort::Max => Some("max"),
        }
    }

    /// Legacy `budget_tokens` for models without the effort API (0 = no thinking). The API
    /// floor is 1024 and the budget must stay under `max_tokens`.
    fn budget_tokens(self) -> u32 {
        match self {
            Effort::Off => 0,
            Effort::Low => 2048,
            Effort::Medium => 6144,
            Effort::High => 12288,
            Effort::Max => 24576,
        }
    }
}

/// How hard the model should think about a split. `Auto` scales with changeset size; `Fixed`
/// pins an effort tier (the override path).
#[derive(Debug, Clone)]
pub enum ThinkingChoice {
    Auto,
    Fixed(Effort),
}

impl ThinkingChoice {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => ThinkingChoice::Auto,
            "off" | "none" | "no" | "0" => ThinkingChoice::Fixed(Effort::Off),
            "low" => ThinkingChoice::Fixed(Effort::Low),
            "medium" | "med" => ThinkingChoice::Fixed(Effort::Medium),
            "high" => ThinkingChoice::Fixed(Effort::High),
            "max" => ThinkingChoice::Fixed(Effort::Max),
            // A bare token count maps onto the nearest tier — raw thinking budgets are no longer
            // a first-class API control (they 400 on current models), so we bucket them.
            other => match other.parse::<u32>() {
                Ok(0) => ThinkingChoice::Fixed(Effort::Off),
                Ok(n) if n <= 3072 => ThinkingChoice::Fixed(Effort::Low),
                Ok(n) if n <= 8192 => ThinkingChoice::Fixed(Effort::Medium),
                Ok(n) if n <= 16384 => ThinkingChoice::Fixed(Effort::High),
                Ok(_) => ThinkingChoice::Fixed(Effort::Max),
                Err(_) => ThinkingChoice::Auto,
            },
        }
    }

    /// Resolve to an effort tier for a given atom count. Tiny splits need no thinking; small ones
    /// (the common single-PR case) get a light pass so they stay fast; only large, tangled diffs
    /// pull the heavier tiers.
    fn resolve(&self, atoms: usize) -> Effort {
        match self {
            ThinkingChoice::Fixed(e) => *e,
            ThinkingChoice::Auto if atoms < 8 => Effort::Off,
            ThinkingChoice::Auto if atoms < 40 => Effort::Low,
            ThinkingChoice::Auto if atoms < 120 => Effort::Medium,
            ThinkingChoice::Auto => Effort::High,
        }
    }
}

/// Whether `model` speaks the modern adaptive-thinking + `effort` API (Opus / Sonnet 4.6+), versus
/// the legacy extended-thinking `budget_tokens` API. Haiku 4.5 (and older models) reject `effort`
/// and adaptive thinking; everything else jjk tiers to uses the modern surface.
fn uses_effort_api(model: &str) -> bool {
    !model.contains("haiku")
}

pub struct AnthropicLlm {
    client: reqwest::Client,
    api_key: String,
    model: ModelChoice,
    thinking: ThinkingChoice,
    base_url: String,
    /// Output tokens reserved for the response itself (added on top of a legacy thinking budget).
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

    /// POST a split request while animating a live elapsed-time spinner on stderr, so a long
    /// thinking call shows progress instead of looking like a hang. Off when stderr isn't a TTY
    /// (CI / pipes) — `split` logs a one-line note there instead. The spinner runs as a side task;
    /// the network call is unchanged.
    async fn post_with_spinner(&self, body: &Value, label: &str) -> Result<Value> {
        if !std::io::stderr().is_terminal() {
            return self.post(body).await;
        }
        let done = Arc::new(AtomicBool::new(false));
        let ticker = {
            let done = done.clone();
            let label = label.to_string();
            tokio::spawn(async move {
                const FRAMES: [&str; 10] =
                    ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
                let start = Instant::now();
                let mut i = 0usize;
                while !done.load(Ordering::Relaxed) {
                    // \r to the line start, \x1b[K clears it, then redraw.
                    eprint!(
                        "\r\x1b[K  {} {label} · {}s",
                        FRAMES[i % FRAMES.len()],
                        start.elapsed().as_secs()
                    );
                    let _ = std::io::stderr().flush();
                    i += 1;
                    tokio::time::sleep(Duration::from_millis(120)).await;
                }
            })
        };
        let result = self.post(body).await;
        done.store(true, Ordering::Relaxed);
        let _ = ticker.await; // let the task observe `done` and stop before we clear the line
        eprint!("\r\x1b[K");
        let _ = std::io::stderr().flush();
        result
    }
}

impl AnthropicLlm {
    /// Render the user turn (catalog) from a split input.
    fn user_message(input: &SplitInput) -> Result<String> {
        render_user(&render_catalog(input))
    }

    /// The model/effort + tool-loop core, shared by `split` and `revise`. `user` is the first user
    /// turn (the catalog, plus a revision preamble when revising).
    async fn run_split(&self, input: &SplitInput, user: String) -> Result<SplitPlan> {
        let atoms = input.atoms.len();
        let model = self.model.resolve();
        let effort = self.thinking.resolve(atoms);
        let adaptive = uses_effort_api(&model);
        let thinking_on = effort != Effort::Off;

        let system = render_system(&input.mode, input.instruction.as_deref())?;
        let tools = json!([record_split_tool(), get_atom_detail_tool()]);
        // Adaptive thinking budgets aren't a fixed token count, so give the response generous
        // headroom (thinking + the structured output share max_tokens); legacy budgets add a
        // fixed reserve. We don't stream, but the reqwest client has no timeout, so a large
        // ceiling is safe.
        let max_tokens = if adaptive {
            if thinking_on {
                32_768
            } else {
                self.output_reserve
            }
        } else {
            let budget = effort.budget_tokens();
            if budget > 0 {
                budget + self.output_reserve
            } else {
                8192
            }
        };

        let mut messages = vec![json!({
            "role": "user",
            "content": [{"type": "text", "text": user}]
        })];

        // Accumulate token usage across the tool loop so cost & cache hits are visible.
        let mut usage = Usage::default();
        let effort_label = effort.as_word().unwrap_or("off");
        // Non-TTY runs get no spinner; a one-line note still shows the call is underway.
        if !std::io::stderr().is_terminal() {
            eprintln!("domain split: calling {model} (effort {effort_label}, {atoms} atoms)…");
        }

        for iter in 0..MAX_ITERS {
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
            // `disable_parallel_tool_use` keeps the model to one tool call per turn (we also handle
            // multiple defensively below). Thinking forbids forcing a tool, so it uses tool_choice
            // "auto"; the system prompt instructs exactly one record_split call. Adaptive models
            // (Opus/Sonnet 4.6+) take `thinking: adaptive` + `output_config.effort`; legacy models
            // take a budget.
            if thinking_on {
                if adaptive {
                    body["thinking"] = json!({"type": "adaptive"});
                    if let Some(word) = effort.as_word() {
                        body["output_config"] = json!({"effort": word});
                    }
                } else {
                    body["thinking"] = json!({"type": "enabled", "budget_tokens": effort.budget_tokens()});
                }
                body["tool_choice"] = json!({"type": "auto", "disable_parallel_tool_use": true});
            } else {
                // No thinking: force a single tool call directly.
                if adaptive {
                    body["thinking"] = json!({"type": "disabled"});
                }
                body["tool_choice"] = json!({"type": "any", "disable_parallel_tool_use": true});
            }

            let label = if iter == 0 {
                format!("domain split · {model} · effort {effort_label}")
            } else {
                format!("domain split · {model} · reading detail (round {})", iter + 1)
            };
            let resp = self.post_with_spinner(&body, &label).await?;
            usage.add(resp.get("usage"));
            let content = resp
                .get("content")
                .and_then(|c| c.as_array())
                .cloned()
                .unwrap_or_default();
            let tool_uses: Vec<&Value> = content
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
                .collect();
            if tool_uses.is_empty() {
                return Err(JjkError::Msg("anthropic returned no tool call".into()).into());
            }

            // record_split is terminal: parse it and return. We won't send another request, so any
            // sibling tool_use blocks don't need a tool_result.
            if let Some(rec) = tool_uses
                .iter()
                .find(|t| t.get("name").and_then(|n| n.as_str()) == Some("record_split"))
            {
                let plan: SplitPlan =
                    serde_json::from_value(rec.get("input").cloned().unwrap_or(Value::Null))
                        .map_err(|e| {
                            JjkError::Msg(format!("record_split output didn't match schema: {e}"))
                        })?;
                eprintln!(
                    "domain split: model={model} effort={effort_label} atoms={atoms} \
                     tokens(in={} out={} cache_read={} cache_write={})",
                    usage.input, usage.output, usage.cache_read, usage.cache_write
                );
                return Ok(plan);
            }

            // Otherwise the model wants atom detail. Reply with a tool_result for EVERY tool_use
            // block — a missing one makes the next request 400 (`tool_use` without `tool_result`).
            messages.push(json!({"role": "assistant", "content": content}));
            let mut tool_results = Vec::with_capacity(tool_uses.len());
            let mut fetched: Vec<String> = Vec::new();
            for tu in &tool_uses {
                let id = tu.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
                match tu.get("name").and_then(|n| n.as_str()).unwrap_or("") {
                    "get_atom_detail" => {
                        let labels: Vec<String> = tu
                            .get("input")
                            .and_then(|i| i.get("labels"))
                            .and_then(|l| l.as_array())
                            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                            .unwrap_or_default();
                        let detail = labels
                            .iter()
                            .map(|l| {
                                let body =
                                    input.details.get(l).map(String::as_str).unwrap_or("(no detail)");
                                format!("### {l}\n{body}")
                            })
                            .collect::<Vec<_>>()
                            .join("\n\n");
                        tool_results.push(json!({
                            "type": "tool_result", "tool_use_id": id, "content": detail
                        }));
                        fetched.extend(labels);
                    }
                    other => {
                        return Err(JjkError::Msg(format!(
                            "anthropic called unexpected tool '{other}'"
                        ))
                        .into())
                    }
                }
            }
            eprintln!("  ↳ inspecting {} atom(s): {}", fetched.len(), fetched.join(", "));
            messages.push(json!({"role": "user", "content": tool_results}));
        }
        Err(JjkError::Msg("splitter did not produce a plan within the tool-loop budget".into()).into())
    }
}

#[async_trait]
impl Splitter for AnthropicLlm {
    async fn split(&self, input: &SplitInput) -> Result<SplitPlan> {
        self.run_split(input, Self::user_message(input)?).await
    }

    async fn revise(
        &self,
        input: &SplitInput,
        previous: &SplitPlan,
        feedback: &str,
    ) -> Result<SplitPlan> {
        // Show the model its prior proposal + the user's feedback and ask for an adjusted split.
        // The atom labels in `previous` match the catalog, so it can move/merge them directly.
        let plan_json = serde_json::to_string_pretty(previous).unwrap_or_default();
        let mut user = Self::user_message(input)?;
        user.push_str(&format!(
            "\n\n## Revise the proposal\nYou previously proposed this split (the atom labels match \
             the catalog above):\n```json\n{plan_json}\n```\n\nThe user wants to adjust it:\n\n{feedback}\n\n\
             Produce the revised split via record_split — keep what still fits, apply the feedback, \
             and place every atom exactly once."
        ));
        self.run_split(input, user).await
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

/// Render the split input as a compact, line-oriented catalog — far cheaper than serializing the
/// `SplitInput` as JSON (no field names/braces/quotes repeated per atom). One line per atom, then
/// the cross-file dependency hints, then the author's commit subjects. Same-file affinity is
/// deliberately omitted: the path is already on every atom line, so `samefile` edges (which grow
/// O(n²) within a file) would be pure redundancy. Full hunk text is never inlined — the model pulls
/// it on demand via `get_atom_detail`.
fn render_catalog(input: &SplitInput) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    s.push_str(
        "## Atoms\nFormat: `<label>  <kind>  <path>  +<added>/-<removed>  [defines: <symbols>]`\n",
    );
    for a in &input.atoms {
        let _ = write!(s, "{}  {}  {}  +{}/-{}", a.label, a.kind, a.path, a.added, a.removed);
        if !a.defs.is_empty() {
            let _ = write!(s, "  defines: {}", a.defs.join(", "));
        }
        s.push('\n');
    }
    // Cross-file dependency hints only ("A needs B": A uses a symbol B defines). Same-file edges are
    // implicit in the shared path above, so we don't spend tokens on them.
    let deps: Vec<&EdgeHint> = input.edges.iter().filter(|e| e.kind == "defuse").collect();
    if !deps.is_empty() {
        s.push_str(
            "\n## Dependency hints\n\"A needs B\" — atom A uses a symbol atom B defines, so B must not sit above A.\n",
        );
        for e in &deps {
            let _ = writeln!(s, "{} needs {}", e.from, e.to);
        }
    }
    if !input.commit_subjects.is_empty() {
        s.push_str("\n## Author's original commit subjects (bottom→top)\n");
        for (i, c) in input.commit_subjects.iter().enumerate() {
            let _ = writeln!(s, "{}. {}", i + 1, c);
        }
    }
    s
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
    fn model_choice_parses_and_resolves() {
        assert!(matches!(ModelChoice::parse("auto"), ModelChoice::Auto));
        assert!(matches!(ModelChoice::parse(""), ModelChoice::Auto));
        // auto is always Sonnet; shorthands and ids pin explicitly.
        assert_eq!(ModelChoice::Auto.resolve(), SONNET);
        assert_eq!(ModelChoice::parse("opus").resolve(), OPUS);
        assert_eq!(ModelChoice::parse("haiku").resolve(), HAIKU);
        assert_eq!(ModelChoice::parse("claude-x").resolve(), "claude-x");
    }

    #[test]
    fn thinking_choice_parses_and_scales() {
        assert_eq!(ThinkingChoice::parse("off").resolve(100), Effort::Off);
        assert_eq!(ThinkingChoice::parse("high").resolve(1), Effort::High);
        assert_eq!(ThinkingChoice::parse("max").resolve(1), Effort::Max);
        // a bare token count buckets onto a tier
        assert_eq!(ThinkingChoice::parse("4000").resolve(1), Effort::Medium);
        assert_eq!(ThinkingChoice::parse("0").resolve(1), Effort::Off);
        // auto scales with atom count: tiny → off, small → low, mid → medium, large → high
        assert_eq!(ThinkingChoice::Auto.resolve(3), Effort::Off);
        assert_eq!(ThinkingChoice::Auto.resolve(30), Effort::Low);
        assert_eq!(ThinkingChoice::Auto.resolve(50), Effort::Medium);
        assert_eq!(ThinkingChoice::Auto.resolve(500), Effort::High);
    }

    #[test]
    fn effort_maps_to_the_right_api_per_model() {
        // Opus/Sonnet use adaptive thinking + effort; Haiku uses legacy budget_tokens.
        assert!(uses_effort_api(OPUS));
        assert!(uses_effort_api(SONNET));
        assert!(!uses_effort_api(HAIKU));
        assert_eq!(Effort::High.as_word(), Some("high"));
        assert_eq!(Effort::Off.as_word(), None);
        assert!(Effort::High.budget_tokens() > 0);
        assert_eq!(Effort::Off.budget_tokens(), 0);
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

    #[test]
    fn catalog_is_compact_and_drops_samefile_hints() {
        use crate::llm::{AtomGist, EdgeHint};
        use std::collections::HashMap;
        let input = SplitInput {
            mode: "change".into(),
            instruction: None,
            atoms: vec![
                AtomGist {
                    label: "a0".into(),
                    path: "src/cache.rs".into(),
                    kind: "add".into(),
                    defs: vec!["cache_get".into(), "cache_set".into()],
                    gist: "add src/cache.rs".into(),
                    added: 12,
                    removed: 0,
                },
                AtomGist {
                    label: "a1".into(),
                    path: "src/main.rs".into(),
                    kind: "modify".into(),
                    defs: vec![],
                    gist: "modify src/main.rs".into(),
                    added: 3,
                    removed: 1,
                },
            ],
            edges: vec![
                EdgeHint { from: "a1".into(), to: "a0".into(), kind: "defuse".into() },
                EdgeHint { from: "a0".into(), to: "a1".into(), kind: "samefile".into() },
            ],
            commit_subjects: vec!["Add cache".into()],
            details: HashMap::new(),
        };
        let cat = render_catalog(&input);
        // One dense line per atom, with size + defs.
        assert!(cat.contains("a0  add  src/cache.rs  +12/-0  defines: cache_get, cache_set"), "got:\n{cat}");
        assert!(cat.contains("a1  modify  src/main.rs  +3/-1\n"), "got:\n{cat}");
        // defuse hint kept; samefile hint omitted (path already conveys it).
        assert!(cat.contains("a1 needs a0"), "defuse hint missing:\n{cat}");
        assert!(!cat.contains("samefile"), "samefile leaked into the payload:\n{cat}");
        // Commit subjects included.
        assert!(cat.contains("1. Add cache"));
        // Far smaller than the equivalent pretty JSON would be.
        assert!(cat.len() < serde_json::to_string_pretty(&input).unwrap().len());
    }
}
