//! LLM client for benchmark answer generation and judging.
//!
//! Supports four backends:
//! - **OpenAI-compatible** (Ollama, vLLM, etc.): Pass `--llm-url`.
//! - **Gemini Vertex AI**: Set `GEMINI_VERTEX_PROJECT_ID` (and optionally
//!   `GEMINI_VERTEX_REGION`, default `us-central1`). Auth via
//!   `gcloud auth print-access-token`.
//! - **Vertex AI (Claude)**: Set `CLAUDE_CODE_USE_VERTEX=1`, `CLOUD_ML_REGION`,
//!   and `ANTHROPIC_VERTEX_PROJECT_ID`. Auth via `gcloud auth print-access-token`.
//! - **Anthropic API**: Set `ANTHROPIC_API_KEY`. Direct API calls.

use crate::model::MemoryId;
use crate::time::format_timestamp as format_timestamp_tz;

use chrono_tz::Tz;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

// ── Client ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct LlmClient {
    client: Client,
    model: String,
    backend: BackendKind,
    backend_label: String,
}

#[derive(Debug, Clone)]
enum BackendKind {
    OpenAiCompat { base_url: String },
    GeminiVertex { project_id: String, region: String },
    Vertex { project_id: String, region: String },
    Anthropic { api_key: String },
}

#[derive(Debug)]
pub struct JudgeResult {
    pub correct: bool,
    pub reason: String,
}

#[derive(Debug)]
pub struct MemoryToStore {
    pub summary: String,
    pub full_text: Option<String>,
    pub entities: Vec<String>,
    pub topics: Vec<String>,
    pub emotions: Vec<String>,
    pub supersedes: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MemoryContext {
    pub memory_id: MemoryId,
    pub text: String,
    pub score: f32,
    pub created_at: i64,
    pub entities: Vec<String>,
    pub topics: Vec<String>,
    pub emotions: Vec<String>,
}

pub struct MemoryRelation {
    pub from_label: String,
    pub to_label: String,
    pub edge_type: String,
}

pub struct GraphContext {
    /// (memory_id, summary, full_text, created_at)
    pub neighbors: Vec<(MemoryId, String, Option<String>, i64)>,
    pub relations: Vec<MemoryRelation>,
}

#[derive(Debug)]
pub struct SearchQuery {
    pub query: String,
    pub fts_query: Option<String>,
}

#[derive(Debug)]
pub struct SearchParams {
    pub queries: Vec<SearchQuery>,
    pub entities: Vec<String>,
    pub topics: Vec<String>,
    pub emotions: Vec<String>,
    pub depth: u32,
    pub time_range_start: Option<i64>,
    pub time_range_end: Option<i64>,
}

#[derive(Deserialize)]
struct SearchParamsOutput {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    queries: Option<Vec<SearchQueryOutput>>,
    #[serde(default)]
    fts_query: Option<String>,
    #[serde(default)]
    entities: Vec<String>,
    #[serde(default)]
    topics: Vec<String>,
    #[serde(default)]
    emotions: Vec<String>,
    #[serde(default)]
    depth: Option<u32>,
    #[serde(default)]
    time_range_start: Option<i64>,
    #[serde(default)]
    time_range_end: Option<i64>,
}

#[derive(Deserialize)]
struct SearchQueryOutput {
    query: String,
    #[serde(default)]
    fts_query: Option<String>,
}

#[derive(Deserialize)]
struct FollowupOutput {
    #[serde(default)]
    sufficient: bool,
    #[serde(default)]
    queries: Option<Vec<SearchQueryOutput>>,
    #[serde(default)]
    entities: Vec<String>,
}

#[derive(Deserialize)]
struct StoreMemoryCall {
    summary: String,
    #[serde(default)]
    full_text: Option<String>,
    #[serde(default)]
    entities: Vec<String>,
    #[serde(default)]
    topics: Vec<String>,
    #[serde(default)]
    emotions: Vec<String>,
    #[serde(default)]
    supersedes: Option<String>,
}

#[derive(Deserialize)]
struct ProcessTurnOutput {
    #[serde(default)]
    store_memory: Vec<StoreMemoryCall>,
}

// ── Request/response types ───────────────────────────────────────

#[derive(Serialize, Clone)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Serialize)]
struct OpenAiRequest {
    model: String,
    messages: Vec<ChatMessage>,
    max_tokens: u32,
    temperature: f32,
}

#[derive(Deserialize)]
struct OpenAiResponse {
    choices: Vec<OpenAiChoice>,
}

#[derive(Deserialize)]
struct OpenAiChoice {
    message: OpenAiMessageContent,
}

#[derive(Deserialize)]
struct OpenAiMessageContent {
    content: String,
}

#[derive(Serialize)]
struct AnthropicRequest {
    model: String,
    max_tokens: u32,
    temperature: f32,
    system: String,
    messages: Vec<ChatMessage>,
}

#[derive(Serialize)]
struct VertexRequest {
    anthropic_version: String,
    max_tokens: u32,
    temperature: f32,
    system: String,
    messages: Vec<ChatMessage>,
}

#[derive(Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicContentBlock>,
}

#[derive(Deserialize)]
struct AnthropicContentBlock {
    text: Option<String>,
}

#[derive(Serialize)]
struct GeminiPart {
    text: String,
}

#[derive(Serialize)]
struct GeminiContent {
    role: String,
    parts: Vec<GeminiPart>,
}

#[derive(Serialize)]
struct GeminiThinkingConfig {
    #[serde(rename = "thinkingBudget")]
    thinking_budget: u32,
}

#[derive(Serialize)]
struct GeminiGenerationConfig {
    #[serde(rename = "maxOutputTokens")]
    max_output_tokens: u32,
    temperature: f32,
    #[serde(rename = "thinkingConfig", skip_serializing_if = "Option::is_none")]
    thinking_config: Option<GeminiThinkingConfig>,
}

#[derive(Serialize)]
struct GeminiRequest {
    contents: Vec<GeminiContent>,
    #[serde(rename = "systemInstruction")]
    system_instruction: GeminiContent,
    #[serde(rename = "generationConfig")]
    generation_config: GeminiGenerationConfig,
}

#[derive(Deserialize)]
struct GeminiResponse {
    candidates: Vec<GeminiCandidate>,
}

#[derive(Deserialize)]
struct GeminiCandidate {
    content: GeminiCandidateContent,
}

#[derive(Deserialize)]
struct GeminiCandidateContent {
    parts: Vec<GeminiResponsePart>,
}

#[derive(Deserialize)]
struct GeminiResponsePart {
    text: Option<String>,
}

#[derive(Deserialize)]
struct JudgeOutput {
    #[serde(default)]
    label: String,
    #[serde(default)]
    reasoning: String,
}

// ── Backend detection ────────────────────────────────────────────

fn detect_backend(llm_url: Option<&str>) -> Result<BackendKind, String> {
    if let Some(url) = llm_url {
        return Ok(BackendKind::OpenAiCompat {
            base_url: url.trim_end_matches('/').to_string(),
        });
    }

    if let Ok(project_id) = std::env::var("GEMINI_VERTEX_PROJECT_ID") {
        let region =
            std::env::var("GEMINI_VERTEX_REGION").unwrap_or_else(|_| "us-central1".to_string());
        return Ok(BackendKind::GeminiVertex { project_id, region });
    }

    if std::env::var("CLAUDE_CODE_USE_VERTEX").unwrap_or_default() == "1" {
        let project_id = std::env::var("ANTHROPIC_VERTEX_PROJECT_ID")
            .map_err(|_| "CLAUDE_CODE_USE_VERTEX=1 but ANTHROPIC_VERTEX_PROJECT_ID not set")?;
        let region = std::env::var("CLOUD_ML_REGION").unwrap_or_else(|_| "us-east5".to_string());
        return Ok(BackendKind::Vertex { project_id, region });
    }

    if let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") {
        return Ok(BackendKind::Anthropic { api_key });
    }

    Err("No LLM backend configured. Use one of:\n  \
         - --llm-url http://host:port  (OpenAI-compatible server)\n  \
         - GEMINI_VERTEX_PROJECT_ID  (Gemini on Vertex AI)\n  \
         - CLAUDE_CODE_USE_VERTEX=1 + ANTHROPIC_VERTEX_PROJECT_ID\n  \
         - ANTHROPIC_API_KEY"
        .to_string())
}

fn get_gcloud_token() -> Result<String, String> {
    let output = std::process::Command::new("gcloud")
        .args(["auth", "print-access-token"])
        .output()
        .map_err(|e| format!("failed to run gcloud: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("gcloud auth failed: {stderr}"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

// ── Implementation ───────────────────────────────────────────────

fn format_timestamp(millis: i64) -> String {
    format_timestamp_tz(millis, Tz::UTC)
}

fn build_relation_map(
    relations: &[MemoryRelation],
    _memories: &[MemoryContext],
    _id_to_label: &std::collections::HashMap<MemoryId, String>,
) -> std::collections::HashMap<String, String> {
    let mut map: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
    for rel in relations {
        map.entry(rel.from_label.clone())
            .or_default()
            .push(format!("[{}] ({})", rel.to_label, rel.edge_type));
        map.entry(rel.to_label.clone())
            .or_default()
            .push(format!("[{}] ({})", rel.from_label, rel.edge_type));
    }
    map.into_iter().map(|(k, v)| (k, v.join(", "))).collect()
}

impl LlmClient {
    pub fn new(model: String, llm_url: Option<&str>) -> Result<Self, String> {
        let backend = detect_backend(llm_url)?;

        let client = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| format!("failed to build HTTP client: {e}"))?;

        let backend_label = match &backend {
            BackendKind::OpenAiCompat { base_url } => format!("openai-compat ({base_url})"),
            BackendKind::GeminiVertex { project_id, region } => {
                format!("gemini-vertex ({project_id}, {region})")
            }
            BackendKind::Vertex { project_id, region } => {
                format!("vertex ({project_id}, {region})")
            }
            BackendKind::Anthropic { .. } => "anthropic".to_string(),
        };

        Ok(Self {
            client,
            model,
            backend,
            backend_label,
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn backend_label(&self) -> &str {
        &self.backend_label
    }

    pub async fn generate_answer(
        &self,
        question: &str,
        memories: &[MemoryContext],
        category: &str,
        graph_context: &GraphContext,
    ) -> Result<String, String> {
        let base = "You are a memory assistant answering questions about long personal \
            conversations between two people who are friends. You receive retrieved memory \
            excerpts from these conversations, each with a relevance score (higher = \
            stronger match) and a date.\n\n\
            Memory types:\n\
            - Numbered memories [1], [2], etc. are directly retrieved for this question.\n\
            - Memories labeled [R1], [R2], etc. are related graph neighbors — contextually \
            connected but not directly matched to the question. Use them as supporting \
            context.\n\n\
            Each memory may include structured metadata:\n\
            - Entities: named entities (people, places, etc.) mentioned in the memory.\n\
            - Topics: subject areas the memory relates to.\n\
            - Mood: emotional tone recorded at the time.\n\
            Use this metadata to verify person references and understand context.\n\n\
            When a memory contains a direct quote or specific detail, prefer that detail \
            over your general knowledge.";

        let instructions = "\n\n<instructions>\n\
                Base your answer ONLY on the provided memories.\n\n\
                Step-by-step approach:\n\
                1. Identify which memories contain relevant evidence.\n\
                2. If the question requires combining information from multiple memories, \
                chain the pieces together.\n\
                3. Memories are sorted chronologically (earliest first). Time deltas between \
                consecutive memories are pre-computed and shown as '\u{23f1} N days after [X]'. \
                For duration questions, use these pre-computed deltas directly — they are exact. \
                The day of week is shown in each memory's date header.\n\
                4. For counting questions, enumerate each distinct instance explicitly \
                before stating the final count.\n\n\
                Qualifier verification: if the question names a SPECIFIC role, event \
                type, or entity (e.g., \"marketing director\", \"summer internship\", \
                \"Honda Civic\"), verify that this EXACT qualifier appears in at least \
                one memory. If only a DIFFERENT qualifier exists (e.g., \"sales manager\" \
                instead of \"marketing director\", \"part-time job\" instead of \"summer \
                internship\"), treat the question as unanswerable — do NOT answer from \
                the wrong entity.\n\
                Conversely, if a memory describes the same real-world fact using different \
                words (e.g., \"purchased a standing desk\" answers \"what desk did I \
                buy?\", or \"stayed up until midnight on Tuesday\" answers \"what time \
                did I go to bed the night before Wednesday?\"), extract the answer — do \
                NOT say \"I don't know.\"\n\n\
                Semantic matching: memories may use different words for the same concept \
                (e.g., \"plays guitar\" relates to \"rock music\"). Use the information \
                available even when the phrasing differs.\n\n\
                For hypothetical questions (\"would they...\", \"is it likely...\"), find \
                evidence of relevant preferences or behaviors and commit to a clear answer.\n\n\
                For questions requiring inference or world knowledge (e.g., \"what type \
                of archery bow would suit a beginner?\"), combine memory evidence with \
                your general knowledge to provide the most specific and helpful answer.\n\n\
                For list-type questions (\"what are X's hobbies?\", \"name the places Y \
                visited\"), systematically scan ALL retrieved memories for every relevant \
                item. Collect every instance before answering.\n\n\
                IMPORTANT — do NOT hedge or refuse when evidence exists:\n\
                - Never say \"the memories do not mention\" or \"there is no explicit evidence\" \
                when there IS relevant information in the retrieved memories — even if it \
                requires a small inference.\n\
                - If a memory contains a relevant fact but uses different wording than the \
                question, that IS sufficient evidence. Use it.\n\
                - If multiple memories together imply an answer, synthesize them into a \
                clear response rather than saying the answer is unclear.\n\
                - Prefer giving a well-reasoned answer from available evidence over hedging.\n\
                - Say \"I don't know\" ONLY when truly no relevant evidence exists in ANY \
                of the retrieved memories or their neighbors.\n\n\
                Keep your final answer concise: one or two sentences after your reasoning.\n\
                </instructions>";

        self.generate_answer_with_system(
            question,
            memories,
            category,
            graph_context,
            &format!("{base}{instructions}"),
            None,
        )
        .await
    }

    pub async fn generate_answer_longmemeval(
        &self,
        question: &str,
        memories: &[MemoryContext],
        category: &str,
        graph_context: &GraphContext,
        question_date: Option<&str>,
    ) -> Result<String, String> {
        let base = "You are a memory assistant answering questions about a user based on \
            their past chat history with an AI assistant. You receive retrieved memory \
            excerpts from these conversations, each with a relevance score (higher = \
            stronger match) and a date.\n\n\
            Memory types:\n\
            - Numbered memories [1], [2], etc. are directly retrieved for this question.\n\
            - Memories labeled [R1], [R2], etc. are related graph neighbors — contextually \
            connected but not directly matched to the question. Use them as supporting \
            context.\n\n\
            Each memory may include structured metadata:\n\
            - Entities: named entities (people, places, etc.) mentioned in the memory.\n\
            - Topics: subject areas the memory relates to.\n\
            - Mood: emotional tone recorded at the time.\n\
            Use this metadata to verify references and understand context.\n\n\
            When a memory contains a direct quote or specific detail, prefer that detail \
            over your general knowledge.";

        let instructions = "\n\n<instructions>\n\
                Base your answer ONLY on the provided memories.\n\n\
                Step-by-step approach:\n\
                1. Identify which memories contain relevant evidence.\n\
                2. If the question requires combining information from multiple memories, \
                chain the pieces together.\n\
                3. Memories are sorted chronologically (earliest first). The day of week \
                is shown in each memory's date header. Time deltas between consecutive \
                memories are shown as '\u{23f1} N days after [X]' for reference.\n\
                4. For relative time references ('two months ago', 'last week', 'recently'), \
                use Today's date (shown above the memories) to calculate which memory the \
                question refers to. Write out the dates and compute the difference step by \
                step. When a memory says 'for N months' or 'since N months ago', compute \
                the implied start date by subtracting from the memory's date.\n\
                5. For time-related questions, ALWAYS compute dates from the memory headers \
                directly. Extract the relevant dates, compute the difference, and state \
                the result.\n\
                6. For counting questions ('how many times', 'how many X did I'), follow \
                this EXACT procedure — do NOT skip any step:\n\
                  a. Scan ALL memories [1] through [N] AND all neighbors [R1] through [RN]. \
                     For each one, ask: does this describe a qualifying instance?\n\
                  b. Create a numbered list of every qualifying instance with: memory label, \
                     date, and key identifying detail.\n\
                  c. After completing the list, do a SECOND pass through ALL memories looking \
                     specifically for items you missed — check memories with low relevance \
                     scores and graph neighbors, they often contain additional instances.\n\
                  d. Check for double-counting: are any two entries the same event described \
                     in different memories? Deduplicate.\n\
                  e. Only then count the final list.\n\
                Common mistake: stopping after finding 2-3 obvious matches when more exist.\n\n\
                When multiple memories describe the same fact at different times \
                (e.g., the user's job, address, personal record), the MOST RECENT \
                memory is the current truth. Earlier memories are history. Always answer \
                with the latest known value.\n\n\
                Qualifier verification: if the question names a SPECIFIC role, event \
                type, or entity (e.g., \"marketing director\", \"summer internship\", \
                \"Honda Civic\"), verify that this EXACT qualifier appears in at least \
                one memory. If only a DIFFERENT qualifier exists (e.g., \"sales manager\" \
                instead of \"marketing director\", \"part-time job\" instead of \"summer \
                internship\"), treat the question as unanswerable — do NOT answer from \
                the wrong entity.\n\
                Conversely, if a memory describes the same real-world fact using different \
                words (e.g., \"purchased a standing desk\" answers \"what desk did I \
                buy?\", or \"stayed up until midnight on Tuesday\" answers \"what time \
                did I go to bed the night before Wednesday?\"), extract the answer — do \
                NOT say \"I don't know.\"\n\n\
                Semantic matching: memories may use different words for the same concept \
                (e.g., \"plays guitar\" relates to \"rock music\"). Use the information \
                available even when the phrasing differs.\n\n\
                For questions asking for suggestions or recommendations, personalize based on \
                the user's stated preferences, past purchases, habits, and dislikes. Pay \
                attention to negative preferences (things the user wants to avoid). Even \
                memories with low relevance scores may contain user context — read all of \
                them before answering.\n\n\
                For list-type questions (\"how many X have I...\", \"what are my hobbies?\"), \
                systematically scan ALL retrieved memories for every relevant instance. \
                Collect every item before answering.\n\n\
                IMPORTANT — do NOT hedge or refuse when evidence exists:\n\
                - Never say \"the memories do not mention\", \"there is no explicit evidence\", \
                or \"the user did not\" when there IS relevant information — even if the \
                wording differs from the question.\n\
                - The question's phrasing may not match the memory exactly. Different verbs, \
                tenses, or framing do not invalidate a memory. If a memory contains the \
                factual answer, GIVE that answer regardless of wording differences.\n\
                - If information can be INFERRED by combining multiple memories (e.g., \
                computing dates, chaining facts across sessions, adding amounts), provide \
                the inferred answer. An inference from available evidence is NOT the same \
                as making up information.\n\
                - If multiple memories together imply an answer, synthesize them into a \
                clear response rather than saying the answer is unclear.\n\
                - Prefer giving a well-reasoned answer from available evidence over hedging.\n\
                - Say \"I don't know\" or \"I don't have that information\" ONLY when truly \
                no relevant evidence exists in ANY of the retrieved memories or their \
                neighbors. This is the correct response for questions about things the \
                user never discussed.\n\
                - A question is unanswerable ONLY if the specific entity, event, or \
                attribute asked about is genuinely absent from ALL memories. If a memory \
                mentions a specific qualifier, verify that it matches the question before \
                answering or abstaining.\n\n\
                Before concluding \"I don't have that information\", perform these checks:\n\
                1. ADDITION CHECK: Can two or more memories be added together? (e.g., \
                   \"2 items from store A\" + \"3 items from store B\" = 5 total items)\n\
                2. SYNONYM CHECK: Does a memory describe the same thing using a different \
                   verb or noun? (e.g., \"owns X\" answers \"what did I buy?\"; \"went to \
                   bed at 2 AM\" answers \"what time did I go to sleep?\")\n\
                3. CHAIN CHECK: Can the answer be derived by chaining two facts? (e.g., \
                   \"event was on Wednesday\" + \"appointment was Thursday\" answers \
                   \"what happened the day before my appointment?\")\n\
                If any check succeeds, GIVE the answer — do not refuse.\n\n\
                Answer completeness rules:\n\
                - Include ALL specific details from the source memory: full qualifiers, \
                  locations, examples, subcategories. Never summarize away detail.\n\
                - Give the most DEFINITIVE answer the evidence supports. Do NOT hedge with \
                  \"close to\", \"approximately\", or \"around\" when a memory states a \
                  specific value — state the value directly.\n\
                - For numerical answers, give ONE specific number, not a range.\n\
                - When two items seem tied, pick the one with stronger or more recent \
                  evidence rather than reporting a tie.\n\n\
                Output format: State your final answer FIRST in a single line starting \
                with \"ANSWER:\", then explain your reasoning below. This format is \
                mandatory. Example:\n\
                ANSWER: The user's favorite color is blue.\n\
                Reasoning: Memory [3] from 2023-05-14 states...\n\
                </instructions>";

        self.generate_answer_with_system(
            question,
            memories,
            category,
            graph_context,
            &format!("{base}{instructions}"),
            question_date,
        )
        .await
    }

    async fn generate_answer_with_system(
        &self,
        question: &str,
        memories: &[MemoryContext],
        _category: &str,
        graph_context: &GraphContext,
        system: &str,
        question_date: Option<&str>,
    ) -> Result<String, String> {
        let mut sorted_memories = memories.to_vec();
        sorted_memories.sort_by_key(|m| m.created_at);

        let mut id_to_label: std::collections::HashMap<MemoryId, String> =
            std::collections::HashMap::new();
        for (i, mem) in sorted_memories.iter().enumerate() {
            id_to_label.insert(mem.memory_id, format!("{}", i + 1));
        }
        for (i, (mid, _, _, _)) in graph_context.neighbors.iter().enumerate() {
            id_to_label.insert(*mid, format!("R{}", i + 1));
        }

        let relation_map = build_relation_map(&graph_context.relations, memories, &id_to_label);

        let context = sorted_memories
            .iter()
            .enumerate()
            .map(|(i, mem)| {
                let label = format!("{}", i + 1);
                let date = format_timestamp(mem.created_at);
                let day_of_week = chrono::DateTime::from_timestamp_millis(mem.created_at)
                    .map(|dt| dt.format("%A").to_string())
                    .unwrap_or_default();
                let mut line = format!(
                    "[{}] (score: {:.2}, {} {}) {}",
                    label, mem.score, day_of_week, date, mem.text
                );

                if i > 0 {
                    let prev = &sorted_memories[i - 1];
                    let delta_ms = mem.created_at - prev.created_at;
                    let delta_days = delta_ms / 86_400_000;
                    if delta_days > 0 {
                        let delta_str = if delta_days == 1 {
                            "1 day".to_string()
                        } else if delta_days < 30 {
                            format!("{} days", delta_days)
                        } else if delta_days < 365 {
                            let months = delta_days / 30;
                            if months == 1 {
                                "~1 month".to_string()
                            } else {
                                format!("~{} months", months)
                            }
                        } else {
                            let years = delta_days / 365;
                            let remaining_months = (delta_days % 365) / 30;
                            if remaining_months > 0 {
                                format!("~{} year(s) {} month(s)", years, remaining_months)
                            } else {
                                format!("~{} year(s)", years)
                            }
                        };
                        line.push_str(&format!("\n    \u{23f1} {} after [{}]", delta_str, i));
                    }
                }

                let mut meta_parts: Vec<String> = Vec::new();
                if !mem.entities.is_empty() {
                    meta_parts.push(format!("Entities: {}", mem.entities.join(", ")));
                }
                if !mem.topics.is_empty() {
                    meta_parts.push(format!("Topics: {}", mem.topics.join(", ")));
                }
                if !mem.emotions.is_empty() {
                    meta_parts.push(format!("Mood: {}", mem.emotions.join(", ")));
                }
                if !meta_parts.is_empty() {
                    line.push_str(&format!("\n    {}", meta_parts.join(" | ")));
                }

                if let Some(links) = relation_map.get(&label) {
                    line.push_str(&format!("\n    → linked to {}", links));
                }
                line
            })
            .collect::<Vec<_>>()
            .join("\n");

        let neighbor_context = if graph_context.neighbors.is_empty() {
            String::new()
        } else {
            let items: Vec<String> = graph_context
                .neighbors
                .iter()
                .enumerate()
                .map(|(i, (_mid, summary, full_text, created_at))| {
                    let label = format!("R{}", i + 1);
                    let date = format_timestamp(*created_at);
                    let mut line = format!("[{}] ({}) {}", label, date, summary);
                    if let Some(ft) = full_text {
                        line.push_str(&format!("\n    Original: {}", ft));
                    }
                    if let Some(links) = relation_map.get(&label) {
                        line.push_str(&format!("\n    → linked to {}", links));
                    }
                    line
                })
                .collect();
            format!(
                "\n\nRelated memories (graph neighbors):\n{}",
                items.join("\n")
            )
        };

        let reasoning_hint = "Think step by step. First identify which memories are relevant. Then extract the key facts. If the question involves dates, write out the dates and compute. If it involves counting, list each instance. Finally, state your answer.";

        let date_context = match question_date {
            Some(d) if !d.is_empty() => format!("Today's date: {d}\n\n"),
            _ => String::new(),
        };

        let user = format!(
            "{date_context}Retrieved memories:\n{context}{neighbor_context}\n\n\
             Question: {question}\n\n\
             {reasoning_hint}\n\n\
             Answer:"
        );

        self.call(system, &user, 4096).await
    }

    pub async fn judge_answer(
        &self,
        question: &str,
        gold_answer: &str,
        generated_answer: &str,
        is_adversarial: bool,
    ) -> Result<JudgeResult, String> {
        let system = "You are evaluating conversational AI memory recall.\n\
            You must respond with exactly one JSON object containing two keys:\n\
            - \"label\": either \"CORRECT\" or \"WRONG\" (uppercase, no other values)\n\
            - \"reasoning\": a brief explanation of your judgment\n\n\
            Example response format:\n\
            {\"label\": \"CORRECT\", \"reasoning\": \"The generated answer matches the gold answer.\"}\n\n\
            Output ONLY the JSON object. No markdown fences, no extra text."
            .to_string();

        let user = if is_adversarial {
            format!(
                "This is an ADVERSARIAL question — it asks about something that was never \
                 discussed in the conversation. The correct behavior is to refuse to answer.\n\n\
                 Question: {question}\n\
                 Expected wrong answer (what a tricked system would say): {gold_answer}\n\
                 Generated answer: {generated_answer}\n\n\
                 Label the generated answer as CORRECT or WRONG.\n\
                 CORRECT: the answer says \"I don't know\", \"unanswerable\", or refuses to answer.\n\
                 WRONG: the answer provides the expected wrong answer or any specific answer \
                 as if it were true.\n\n\
                 Respond with a single JSON object: {{\"label\": \"...\", \"reasoning\": \"...\"}}"
            )
        } else {
            format!(
                "Label a generated answer as CORRECT or WRONG by comparing it to a gold answer.\n\n\
                 The gold answer is the ground truth. The generated answer is CORRECT if its \
                 core fact matches the gold answer, even if the wording differs or extra details \
                 are included.\n\n\
                 Grading rules:\n\
                 - Synonyms, paraphrases, and equivalent phrasings count as CORRECT \
                 (e.g., \"espresso and cappuccino\" matches \"espresso-based drinks\"; \
                 \"woodworking tools\" matches \"carpentry equipment\").\n\
                 - Extra correct details beyond the gold answer are fine — a SUPERSET of \
                 the gold answer is CORRECT.\n\
                 - Hedging does NOT make an answer wrong. If the generated answer says \
                 \"it is not explicitly stated\" but THEN provides the correct fact, that \
                 is still CORRECT. Read the ENTIRE generated answer before judging.\n\
                 - For yes/no or inferential questions (gold answer is \"Likely no\" or \
                 \"Liberal\"), CORRECT if the same conclusion is reached, regardless of \
                 wording or reasoning shown.\n\
                 - For time questions: dates within 2 days are CORRECT (\"May 7\" vs \
                 \"May 8\"). Different formats are CORRECT (\"May 7th\" vs \"7 May\" \
                 vs \"the first week of May\"). Relative dates that resolve correctly \
                 are CORRECT (\"last month\" said in April = March = CORRECT if gold \
                 says March).\n\
                 - For list questions: CORRECT if all items from the gold answer are present \
                 (extras are fine). CORRECT if most items match and the core facts align.\n\n\
                 Question: {question}\n\
                 Gold answer: {gold_answer}\n\
                 Generated answer: {generated_answer}\n\n\
                 Respond with a single JSON object: {{\"label\": \"...\", \"reasoning\": \"...\"}}"
            )
        };

        let response = self.call(&system, &user, 1024).await?;
        let cleaned = response
            .trim()
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim();

        match serde_json::from_str::<JudgeOutput>(cleaned) {
            Ok(parsed) => Ok(JudgeResult {
                correct: parsed.label.eq_ignore_ascii_case("CORRECT"),
                reason: parsed.reasoning,
            }),
            Err(_) => {
                let upper = cleaned.to_uppercase();
                let correct = upper.contains("\"CORRECT\"") && !upper.contains("\"WRONG\"");
                Ok(JudgeResult {
                    correct,
                    reason: format!("(parse fallback) {}", &response[..response.len().min(100)]),
                })
            }
        }
    }

    pub async fn judge_answer_longmemeval(
        &self,
        question: &str,
        gold_answer: &str,
        generated_answer: &str,
        question_type: &str,
        is_abstention: bool,
    ) -> Result<JudgeResult, String> {
        let prompt = if is_abstention {
            format!(
                "I will give you an unanswerable question, an explanation, and a response from \
                 a model. Please answer yes if the model correctly identifies the question as \
                 unanswerable. The model could say that the information is incomplete, or some \
                 other information is given but the asked information is not.\n\n\
                 Question: {question}\n\n\
                 Explanation: {gold_answer}\n\n\
                 Model Response: {generated_answer}\n\n\
                 Does the model correctly identify the question as unanswerable? Answer yes or \
                 no only."
            )
        } else {
            match question_type {
                "single-session-user" | "single-session-assistant" | "multi-session" => {
                    format!(
                        "I will give you a question, a correct answer, and a response from a \
                         model. Please answer yes if the response contains the correct answer. \
                         Otherwise, answer no. If the response is equivalent to the correct \
                         answer or contains all the intermediate steps to get the correct answer, \
                         you should also answer yes. If the response only contains a subset of \
                         the information required by the answer, answer no. \n\n\
                         Question: {question}\n\n\
                         Correct Answer: {gold_answer}\n\n\
                         Model Response: {generated_answer}\n\n\
                         Is the model response correct? Answer yes or no only."
                    )
                }
                "temporal-reasoning" => {
                    format!(
                        "I will give you a question, a correct answer, and a response from a \
                         model. Please answer yes if the response contains the correct answer. \
                         Otherwise, answer no. If the response is equivalent to the correct \
                         answer or contains all the intermediate steps to get the correct answer, \
                         you should also answer yes. If the response only contains a subset of \
                         the information required by the answer, answer no. In addition, do not \
                         penalize off-by-one errors for the number of days. If the question asks \
                         for the number of days/weeks/months, etc., and the model makes off-by-one \
                         errors (e.g., predicting 19 days when the answer is 18), the model's \
                         response is still correct. \n\n\
                         Question: {question}\n\n\
                         Correct Answer: {gold_answer}\n\n\
                         Model Response: {generated_answer}\n\n\
                         Is the model response correct? Answer yes or no only."
                    )
                }
                "knowledge-update" => {
                    format!(
                        "I will give you a question, a correct answer, and a response from a \
                         model. Please answer yes if the response contains the correct answer. \
                         Otherwise, answer no. If the response contains some previous information \
                         along with an updated answer, the response should be considered as \
                         correct as long as the updated answer is the required answer.\n\n\
                         Question: {question}\n\n\
                         Correct Answer: {gold_answer}\n\n\
                         Model Response: {generated_answer}\n\n\
                         Is the model response correct? Answer yes or no only."
                    )
                }
                "single-session-preference" => {
                    format!(
                        "I will give you a question, a rubric for desired personalized response, \
                         and a response from a model. Please answer yes if the response satisfies \
                         the desired response. Otherwise, answer no. The model does not need to \
                         reflect all the points in the rubric. The response is correct as long as \
                         it recalls and utilizes the user's personal information correctly.\n\n\
                         Question: {question}\n\n\
                         Rubric: {gold_answer}\n\n\
                         Model Response: {generated_answer}\n\n\
                         Is the model response correct? Answer yes or no only."
                    )
                }
                _ => {
                    format!(
                        "I will give you a question, a correct answer, and a response from a \
                         model. Please answer yes if the response contains the correct answer. \
                         Otherwise, answer no. If the response is equivalent to the correct \
                         answer or contains all the intermediate steps to get the correct answer, \
                         you should also answer yes. If the response only contains a subset of \
                         the information required by the answer, answer no. \n\n\
                         Question: {question}\n\n\
                         Correct Answer: {gold_answer}\n\n\
                         Model Response: {generated_answer}\n\n\
                         Is the model response correct? Answer yes or no only."
                    )
                }
            }
        };

        let response = self
            .call("You are an evaluation assistant.", &prompt, 10)
            .await?;
        let label = response.to_lowercase().contains("yes");
        Ok(JudgeResult {
            correct: label,
            reason: response.trim().to_string(),
        })
    }

    pub async fn process_turn(
        &self,
        recent_turns: &[String],
        current_turn: &str,
        recent_memories: &[(String, String)],
        turn_timestamp_ms: i64,
    ) -> Result<Vec<MemoryToStore>, String> {
        Self::process_turn_inner(
            self,
            recent_turns,
            current_turn,
            recent_memories,
            turn_timestamp_ms,
            Self::SYSTEM_PROMPT_LOCOMO,
        )
        .await
    }

    pub async fn process_turn_longmemeval(
        &self,
        recent_turns: &[String],
        current_turn: &str,
        recent_memories: &[(String, String)],
        turn_timestamp_ms: i64,
    ) -> Result<Vec<MemoryToStore>, String> {
        Self::process_turn_inner(
            self,
            recent_turns,
            current_turn,
            recent_memories,
            turn_timestamp_ms,
            Self::SYSTEM_PROMPT_LONGMEMEVAL,
        )
        .await
    }

    const SYSTEM_PROMPT_LOCOMO: &str = "You are an AI assistant listening to a conversation between two people. \
            You have a memory tool called store_memory that saves information for later recall.\n\n\
            You will see recent conversation context followed by the LATEST turn. \
            Store new factual information revealed in the latest turn, but be highly selective — \
            only store facts that would be useful to answer future questions about these people.\n\n\
            <task>\n\
            Your job: extract ONLY genuinely new, specific, useful facts from the latest turn.\n\
            Quality over quantity. A typical conversation turn contains 0-2 memories worth storing. \
            Most turns with casual chat, reactions, or continuation of an already-stored topic \
            should produce zero new memories.\n\
            </task>\n\n\
            <what_to_store>\n\
            Store facts that someone might ask about later:\n\
            - Concrete biographical details: names, relationships, occupations, where someone lives\n\
            - Specific preferences: favorite foods, hobbies, media they enjoy\n\
            - Events and plans: trips, milestones, upcoming events with dates\n\
            - Quantities and durations: \"5 years\", \"every morning\", \"three times a week\", \"$200\"\n\
            - Exact counts: how many children, pets, siblings, trips, etc.\n\
            - Cross-speaker opinions: When person A expresses an opinion about person B \
            (e.g., \"I think he'd make a great teacher\"), store it with both speakers as entities.\n\
            - Entity naming: When someone mentions another person, place, or title BY NAME, \
            always include both the speaker's name and the mentioned entity in the summary. \
            Use the same name form consistently (e.g., always 'Carlos' not sometimes 'he'). \
            This enables cross-memory linking via shared entity names.\n\
            - Specific names, titles, dates, objects: pets' names, book titles, brand names\n\
            - Emotional reactions and feelings: when someone says how they feel about an event \
            (grateful, scared, proud, happy), store it — especially after significant events.\n\
            - Specific phrases or descriptions: if someone describes something using a meaningful \
            phrase (e.g., \"an adventure of learning\", \"warmth and happiness\"), capture the \
            exact wording.\n\
            - What someone saw, read, or observed: sign text, book titles, details about photos \
            or images shared.\n\
            - Activities done TOGETHER: when speakers mention doing a specific activity with \
            someone (family, friends, each other), store what they did and with whom.\n\
            </what_to_store>\n\n\
            <specificity>\n\
            Use EXACT nouns from the conversation. Store the precise name, title, or object: \
            \"To Kill a Mockingbird\" not \"a book\", \"sneakers\" not \"shoes\", \"Portugal\" not \
            \"his home country\", \"labrador\" not \"dog\".\n\
            Always include specific items, places, brands, species, colors, and quantities.\n\
            If a book, movie, song, or other titled work is discussed, include the exact title \
            in both summary and full_text.\n\
            Resolve relative times ('last week', 'yesterday') to approximate absolute dates \
            using the conversation timestamp. The resolved date MUST appear in the summary \
            text itself (e.g., 'On approximately May 3, 2023, Carlos mentioned...'). Do NOT \
            put dates only in metadata — the summary must contain the date so it is searchable \
            and self-contained when retrieved independently.\n\
            </specificity>\n\n\
            <attribution>\n\
            Every summary MUST explicitly name the speaker or subject using their proper name. \
            Never use pronouns (he, she, they, him, her, his, hers, their) as the subject of \
            a summary — always use the person's actual name. Each memory is retrieved \
            independently without surrounding context, so it must be completely \
            self-contained and unambiguous.\n\
            BAD: \"She mentioned she's been taking calligraphy lessons.\"\n\
            GOOD: \"Melanie mentioned she has been taking calligraphy lessons since \
            approximately March 2023.\"\n\
            BAD: \"He got a new job at the hospital.\"\n\
            GOOD: \"Carlos Rivera started a new job at Memorial Hospital in May 2023.\"\n\
            When person A talks about person B, name BOTH speakers explicitly: \
            \"Melanie said that Carlos would make a great teacher.\"\n\
            When storing a cross-speaker opinion or reaction, always include both names as \
            entities so the memory links to both people.\n\
            </attribution>\n\n\
            <deduplication>\n\
            CRITICAL: Before creating any memory, check the already-stored memories listed below.\n\
            If a fact is already captured — even partially, with different wording, or as part of \
            a broader memory — skip it. Only store if the turn reveals a genuinely NEW fact \
            that no existing memory covers.\n\
            If an existing memory is OUTDATED or INCOMPLETE and the conversation reveals new \
            information that changes it, store a new memory with \"supersedes\" set to the \
            ID of the old memory.\n\
            </deduplication>\n\n\
            <output_format>\n\
            The tags you attach (entities, topics, emotions) are what the search system \
            uses to retrieve memories later. Thorough, accurate tagging directly determines \
            whether a memory can be found. If a fact involves a person, tag them as an entity. \
            If it has emotional content, tag the emotion. If it relates to a topic, tag it.\n\n\
            store_memory accepts:\n\
            - \"summary\" (required, string): A concise 1-2 sentence summary of the key fact.\n\
            - \"full_text\" (optional, string): Detailed version with all context, quotes, and \
            specifics. Provide this for any memory containing specific names, titles, dates, \
            numbers, or quotes. Include who said what, exact names, dates, numbers, and nuance.\n\
            - \"entities\" (required, array of strings): ALL people, pets, places, organizations, \
            titles, and proper nouns in this memory. Use canonical names (e.g., \"Alice\", \
            \"Portugal\", \"To Kill a Mockingbird\").\n\
            - \"topics\" (required, array of strings): 1-5 topic keywords. Examples: \"adoption\", \
            \"career\", \"cooking\", \"music\", \"travel\". Use lowercase.\n\
            - \"emotions\" (optional, array of strings): Only include if clear emotional content \
            is present. Examples: \"happy\", \"anxious\", \"grateful\".\n\
            - \"supersedes\" (optional, string): ID of an existing memory this one replaces.\n\n\
            Output: {\"store_memory\": [...]} or {\"store_memory\": []} if nothing new to store.\n\
            Output ONLY valid JSON. No markdown fences, no explanation, no commentary.\n\
            </output_format>\n\n\
            <constraints>\n\
            Skip greetings, filler, emotional reactions without new factual content, and \
            restatements of already-stored information.\n\
            Each memory must capture a UNIQUE fact not present in any existing memory.\n\
            If the latest turn just continues discussing a topic already stored, store nothing.\n\
            Aim for precision: fewer high-quality memories are better than many redundant ones.\n\
            </constraints>";

    const SYSTEM_PROMPT_LONGMEMEVAL: &str = "You are an AI assistant reviewing a chat history between a user and an \
            AI assistant. You have a memory tool called store_memory that saves information \
            about the user for later recall.\n\n\
            You will see recent conversation context followed by the LATEST turn. \
            Store new factual information about the user revealed in the latest turn, but \
            be highly selective — only store facts that would be useful to answer future \
            questions about the user.\n\n\
            <task>\n\
            Your job: extract genuinely new, specific, useful facts from the latest turn.\n\
            Store facts from BOTH the user AND the assistant:\n\
            - User turns: what the user says about themselves, their life, preferences, actions.\n\
            - Assistant turns: specific facts, recommendations, data, or answers the assistant \
            provided. The user may later ask \"what did you tell me about X?\" or \"remind me \
            of that thing you mentioned.\" These must be retrievable.\n\
            Quality over quantity. A typical USER turn contains 0-2 memories worth storing. \
            ASSISTANT turns with rich content (lists, schedules, detailed recommendations, \
            creative works) may warrant 2-4 memories to capture distinct details. \
            Most turns with casual chat, reactions, or continuation of an already-stored topic \
            should produce zero new memories.\n\
            </task>\n\n\
            <what_to_store>\n\
            Store facts that might be asked about later:\n\
            FROM THE USER:\n\
            - Concrete biographical details: name, relationships, occupation, where they live\n\
            - Specific preferences: favorite foods, hobbies, media they enjoy, tools they use\n\
            - Events and plans: trips, milestones, upcoming events with dates\n\
            - Quantities and durations: \"5 years\", \"every morning\", \"three times a week\", \"$200\"\n\
            - Exact counts: how many children, pets, siblings, trips, items purchased, etc.\n\
            - Purchases and acquisitions: what the user bought, where, how much, specific models\n\
            - People the user mentions: friends, family, coworkers — store their names and \
            relationship to the user.\n\
            - Specific names, titles, dates, objects: pets' names, book titles, brand names\n\
            - Emotional reactions and feelings about events or experiences\n\
            - Activities and hobbies: what the user does, how often, with whom\n\
            - Completion events: when the user finishes a book, course, project, or reaches a \
            goal, store the completion with the date. These are critical for duration questions.\n\
            - Changes in the user's situation: new job, moved, changed preferences. When a \
            fact changes, the new memory MUST supersede the old one.\n\n\
            FROM THE ASSISTANT:\n\
            - Specific recommendations: app names, product names, restaurant names, tools \
            suggested to the user (e.g., \"the assistant recommended Memrise for language learning\").\n\
            - Factual data provided: statistics, study results, specific numbers, dates, or \
            measurements the assistant cited (e.g., \"the assistant cited a study with 38 subjects\").\n\
            - Explanations of specific concepts the user asked about.\n\
            - Schedules, plans, or structured information the assistant created for the user \
            (e.g., shift rotations, itineraries, meal plans). Store individual assignments, \
            not just the fact that a schedule was created.\n\
            - Exact quotes, literary references, definitions, or terminology the assistant \
            provided. If the assistant quoted a source, listed alternative terms, or cited a \
            specific passage, store the verbatim content in full_text.\n\
            - Creative content the assistant produced: song lyrics with chords, poems, stories. \
            Store enough detail that the user could ask about specific parts later.\n\
            - Named individuals mentioned in factual discussions (e.g., a scientist, advisor, \
            or author the assistant referenced by name).\n\
            - Any specific detail the user might later say \"remind me what you said about...\" for.\n\
            </what_to_store>\n\n\
            <specificity>\n\
            Use EXACT nouns from the conversation. Store the precise name, title, or object: \
            \"To Kill a Mockingbird\" not \"a book\", \"sneakers\" not \"shoes\", \"Portugal\" not \
            \"a European country\", \"labrador\" not \"dog\".\n\
            Always include specific items, places, brands, species, colors, and quantities.\n\
            If a book, movie, song, or other titled work is discussed, include the exact title \
            in both summary and full_text.\n\
            Resolve relative times ('last week', 'yesterday') to approximate absolute dates \
            using the conversation timestamp. The resolved date MUST appear in the summary \
            text itself (e.g., 'On approximately May 3, 2023, the user mentioned...'). Do NOT \
            put dates only in metadata — the summary must contain the date so it is searchable \
            and self-contained when retrieved independently.\n\
            </specificity>\n\n\
            <attribution>\n\
            Every summary should refer to the user as \"the user\" consistently. Each memory \
            is retrieved independently without surrounding context, so it must be completely \
            self-contained and unambiguous.\n\
            BAD: \"They mentioned taking calligraphy lessons.\"\n\
            GOOD: \"The user mentioned taking calligraphy lessons since approximately March 2023.\"\n\
            BAD: \"Got a new job at the hospital.\"\n\
            GOOD: \"The user started a new job at Memorial Hospital in May 2023.\"\n\
            When the user mentions other people by name, include both \"the user\" and the \
            named person as entities: \"The user said that Rachel moved to the suburbs.\"\n\
            </attribution>\n\n\
            <deduplication>\n\
            CRITICAL: Before creating any memory, check the already-stored memories listed below.\n\
            If a fact is already captured — even partially, with different wording, or as part of \
            a broader memory — skip it. Only store if the turn reveals a genuinely NEW fact \
            that no existing memory covers.\n\
            If an existing memory is OUTDATED or INCOMPLETE and the conversation reveals new \
            information that changes it, store a new memory with \"supersedes\" set to the \
            ID of the old memory. This is especially important for facts that change over time \
            (e.g., the user's current job, address, personal best time, number of items owned).\n\
            </deduplication>\n\n\
            <output_format>\n\
            The tags you attach (entities, topics, emotions) are what the search system \
            uses to retrieve memories later. Thorough, accurate tagging directly determines \
            whether a memory can be found. If a fact involves a person, tag them as an entity. \
            If it has emotional content, tag the emotion. If it relates to a topic, tag it.\n\n\
            store_memory accepts:\n\
            - \"summary\" (required, string): A concise 1-2 sentence summary of the key fact.\n\
            - \"full_text\" (optional, string): Detailed version with all context, quotes, and \
            specifics. Provide this for any memory containing specific names, titles, dates, \
            numbers, or quotes. Include exact names, dates, numbers, and nuance.\n\
            - \"entities\" (required, array of strings): ALL people, pets, places, organizations, \
            titles, and proper nouns in this memory. Always include \"user\" as an entity. \
            Use canonical names (e.g., \"Rachel\", \"Portugal\", \"Spotify\").\n\
            - \"topics\" (required, array of strings): 1-5 topic keywords. Examples: \"adoption\", \
            \"career\", \"cooking\", \"music\", \"travel\". Use lowercase.\n\
            - \"emotions\" (optional, array of strings): Only include if clear emotional content \
            is present. Examples: \"happy\", \"anxious\", \"grateful\".\n\
            - \"supersedes\" (optional, string): ID of an existing memory this one replaces.\n\n\
            Output: {\"store_memory\": [...]} or {\"store_memory\": []} if nothing new to store.\n\
            Output ONLY valid JSON. No markdown fences, no explanation, no commentary.\n\
            </output_format>\n\n\
            <constraints>\n\
            Skip greetings, filler, emotional reactions without new factual content, and \
            restatements of already-stored information.\n\
            Each memory must capture a UNIQUE fact not present in any existing memory.\n\
            If the latest turn just continues discussing a topic already stored, store nothing.\n\
            Aim for precision: fewer high-quality memories are better than many redundant ones.\n\
            </constraints>";

    async fn process_turn_inner(
        &self,
        recent_turns: &[String],
        current_turn: &str,
        recent_memories: &[(String, String)],
        turn_timestamp_ms: i64,
        system: &str,
    ) -> Result<Vec<MemoryToStore>, String> {
        let mut user_msg = String::new();
        user_msg.push_str(&format!(
            "Conversation timestamp: {}\n\n",
            format_timestamp(turn_timestamp_ms)
        ));

        if !recent_memories.is_empty() {
            user_msg.push_str("Your already-stored memories (DO NOT duplicate these):\n");
            for (id, summary) in recent_memories {
                user_msg.push_str(&format!("  [{}] {}\n", id, summary));
            }
            user_msg.push('\n');
        }

        if !recent_turns.is_empty() {
            user_msg.push_str("Recent context:\n");
            for t in recent_turns {
                user_msg.push_str(t);
                user_msg.push('\n');
            }
            user_msg.push_str("\nLatest turn:\n");
        }
        user_msg.push_str(current_turn);

        let response = self.call(system, &user_msg, 2048).await?;
        let cleaned = response
            .trim()
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim();

        match serde_json::from_str::<ProcessTurnOutput>(cleaned) {
            Ok(parsed) => Ok(parsed
                .store_memory
                .into_iter()
                .map(|m| MemoryToStore {
                    summary: m.summary,
                    full_text: m.full_text,
                    entities: m.entities,
                    topics: m.topics,
                    emotions: m.emotions,
                    supersedes: m.supersedes,
                })
                .collect()),
            Err(_) => Ok(Vec::new()),
        }
    }

    pub async fn construct_search_query(&self, question: &str) -> Result<SearchParams, String> {
        let system = "Given a question, construct optimal search parameters to find relevant \
            memories. Output a single JSON object.\n\n\
            <schema>\n\
            The JSON object accepts these fields:\n\
            - \"queries\" (required, array of 1-3 objects): Each object has:\n\
              - \"query\" (required, string): Natural language search query for semantic similarity \
              against stored memory summaries. Write a descriptive query that sounds like a memory \
              summary — think about how the ANSWER might be phrased in a stored memory.\n\
              - \"fts_query\" (optional, string): Keyword-focused query for full-text search (BM25). \
              Include ONLY key entities, names, and distinctive terms. Example: for \"What did Sarah \
              say about her trip to Japan?\", use \"Sarah trip Japan\". When the question mentions a \
              proper noun (book title, place name, event name), always include it in fts_query.\n\
            - \"entities\" (optional, array of strings): Filter to memories mentioning specific \
            people, places, or proper nouns. Use canonical names (e.g., \"Alice\", \"Tokyo\"). \
            Include when the question asks about a specific person. At most 1-2 entities.\n\
            - \"topics\" (optional, array of strings): Filter by topic. Use lowercase. Examples: \
            \"adoption\", \"cooking\", \"career\". Only include when the question clearly focuses on \
            one subject area. At most ONE topic (multiple use AND logic, which is too restrictive).\n\
            - \"emotions\" (optional, array of strings): Filter by emotional tag. Only include when \
            the question asks about feelings. At most ONE emotion.\n\
            - \"depth\" (optional, integer 0-2, default 2): Graph hops for related memories. \
            Use 0 for simple factual lookups with a clear entity. Use 1 for straightforward \
            single-fact questions. Use 2 (default) for temporal, multi-step, or inference \
            questions.\n\
            - \"time_range_start\" (optional, integer): Lower bound timestamp in milliseconds \
            since Unix epoch.\n\
            - \"time_range_end\" (optional, integer): Upper bound timestamp in milliseconds \
            since Unix epoch.\n\
            </schema>\n\n\
            <strategy>\n\
            ALWAYS use 2-3 queries. Multiple queries from different angles dramatically \
            improve recall. Each query should approach the question differently:\n\
            - \"What movies has Alice watched?\" -> (1) Alice's movie-watching habits, \
            (2) specific film titles Alice mentioned, (3) Alice entertainment preferences.\n\
            - \"Would Marcus enjoy cooking?\" -> (1) Marcus's food-related activities, \
            (2) Marcus's hobbies or interests, (3) things Marcus has said about cooking.\n\
            - \"What's my cat's name?\" -> (1) user's pet cat name, (2) user's animals \
            or pets at home.\n\
            Each query should approach from a genuinely different angle, not be a paraphrase.\n\
            For counting or aggregation questions ('how many X', 'how much total'), use \
            3 queries to cover different instances — each instance may be stored in \
            a separate memory with different wording.\n\
            For opinion or inference questions, search for underlying facts and experiences \
            rather than the inference itself.\n\
            </strategy>\n\n\
            Output ONLY the JSON object. No markdown fences, no explanation, no commentary.";

        let user_msg = format!("Question: {question}");
        let response = self.call(system, &user_msg, 1024).await?;
        let cleaned = response
            .trim()
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim();

        match serde_json::from_str::<SearchParamsOutput>(cleaned) {
            Ok(parsed) => {
                let queries = if let Some(qs) = parsed.queries {
                    qs.into_iter()
                        .map(|q| SearchQuery {
                            query: q.query,
                            fts_query: q.fts_query,
                        })
                        .collect()
                } else if let Some(q) = parsed.query {
                    vec![SearchQuery {
                        query: q,
                        fts_query: parsed.fts_query,
                    }]
                } else {
                    vec![SearchQuery {
                        query: question.to_string(),
                        fts_query: None,
                    }]
                };
                Ok(SearchParams {
                    queries,
                    entities: parsed.entities,
                    topics: parsed.topics,
                    emotions: parsed.emotions,
                    depth: parsed.depth.unwrap_or(2).min(2),
                    time_range_start: parsed.time_range_start,
                    time_range_end: parsed.time_range_end,
                })
            }
            Err(_) => Ok(SearchParams {
                queries: vec![SearchQuery {
                    query: question.to_string(),
                    fts_query: None,
                }],
                entities: Vec::new(),
                topics: Vec::new(),
                emotions: Vec::new(),
                depth: 2,
                time_range_start: None,
                time_range_end: None,
            }),
        }
    }

    pub async fn construct_followup_query(
        &self,
        question: &str,
        retrieved_summaries: &str,
    ) -> Result<Option<SearchParams>, String> {
        let system = "You are evaluating whether retrieved memories are sufficient to answer a \
            question. You receive the question and summaries of what was already retrieved.\n\n\
            Your job is to find GAPS and chase CONNECTIONS across sessions.\n\n\
            Step 1: Read the question carefully. What specific facts are needed to answer it?\n\
            Step 2: Check the retrieved memories. Which needed facts are present? Which are \
            missing?\n\
            Step 3: Look at entities, people, places, and topics MENTIONED in the retrieved \
            memories. Are there related memories that could be found by searching for those \
            entities in a different context?\n\n\
            Common patterns requiring follow-up:\n\
            - Counting/aggregation: some items found, but the question implies more exist. \
            Search for related items using different wording or categories.\n\
            - Cross-session connections: a retrieved memory mentions a person, place, or \
            event that appears in other sessions. Search for that entity specifically.\n\
            - Temporal chains: the question asks about changes over time but only one \
            time point was found. Search for earlier or later mentions.\n\
            - The question asks about multiple aspects but only one was found.\n\
            - All retrieved memories have very low relevance (none clearly match).\n\n\
            If the retrieved memories are sufficient, output:\n\
            {\"sufficient\": true}\n\n\
            If more information is needed, output a JSON object with:\n\
            - \"sufficient\": false\n\
            - \"queries\" (array of 1-3 objects with \"query\" and optional \"fts_query\"): \
            Search queries targeting SPECIFICALLY what is missing. Use entities and topics \
            from the retrieved memories to find connected information in other sessions. \
            Do NOT repeat the original queries — search for the gap.\n\
            - \"entities\" (optional, array): Entity filters for the follow-up.\n\n\
            Default to searching MORE rather than less — a follow-up that finds nothing \
            costs little, but a skipped follow-up that would have found the answer is \
            a missed opportunity.\n\n\
            Output ONLY the JSON object. No markdown fences, no explanation.";

        let user_msg = format!("Question: {question}\n\nAlready retrieved:\n{retrieved_summaries}");
        let response = self.call(system, &user_msg, 512).await?;
        let cleaned = response
            .trim()
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim();

        match serde_json::from_str::<FollowupOutput>(cleaned) {
            Ok(parsed) => {
                if parsed.sufficient || parsed.queries.is_none() {
                    return Ok(None);
                }
                let queries = parsed
                    .queries
                    .unwrap()
                    .into_iter()
                    .map(|q| SearchQuery {
                        query: q.query,
                        fts_query: q.fts_query,
                    })
                    .collect();
                Ok(Some(SearchParams {
                    queries,
                    entities: parsed.entities,
                    topics: Vec::new(),
                    emotions: Vec::new(),
                    depth: 2,
                    time_range_start: None,
                    time_range_end: None,
                }))
            }
            Err(_) => Ok(None),
        }
    }

    async fn call(&self, system: &str, user: &str, max_tokens: u32) -> Result<String, String> {
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: user.to_string(),
        }];

        let mut last_err = String::new();
        for attempt in 0..3 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(1000 * (1 << attempt))).await;
            }

            let resp = match &self.backend {
                BackendKind::OpenAiCompat { base_url } => {
                    let url = format!("{base_url}/v1/chat/completions");
                    // OpenAI format: system prompt is a message with role "system"
                    let mut all_messages = vec![ChatMessage {
                        role: "system".to_string(),
                        content: system.to_string(),
                    }];
                    all_messages.extend(messages.clone());
                    let body = OpenAiRequest {
                        model: self.model.clone(),
                        messages: all_messages,
                        max_tokens,
                        temperature: 0.0,
                    };
                    self.client
                        .post(&url)
                        .header("content-type", "application/json")
                        .json(&body)
                        .send()
                        .await
                }
                BackendKind::GeminiVertex { project_id, region } => {
                    let token = get_gcloud_token()?;
                    let url = format!(
                        "https://{region}-aiplatform.googleapis.com/v1/projects/{project_id}/locations/{region}/publishers/google/models/{}:generateContent",
                        self.model
                    );
                    let body = GeminiRequest {
                        contents: messages
                            .iter()
                            .map(|m| GeminiContent {
                                role: if m.role == "assistant" {
                                    "model".to_string()
                                } else {
                                    m.role.clone()
                                },
                                parts: vec![GeminiPart {
                                    text: m.content.clone(),
                                }],
                            })
                            .collect(),
                        system_instruction: GeminiContent {
                            role: "user".to_string(),
                            parts: vec![GeminiPart {
                                text: system.to_string(),
                            }],
                        },
                        generation_config: GeminiGenerationConfig {
                            max_output_tokens: max_tokens + 2048,
                            temperature: 0.0,
                            thinking_config: Some(GeminiThinkingConfig {
                                thinking_budget: 2048,
                            }),
                        },
                    };
                    self.client
                        .post(&url)
                        .bearer_auth(&token)
                        .header("content-type", "application/json")
                        .json(&body)
                        .send()
                        .await
                }
                BackendKind::Vertex { project_id, region } => {
                    let token = get_gcloud_token()?;
                    let host = if region == "global" {
                        "aiplatform.googleapis.com".to_string()
                    } else {
                        format!("{region}-aiplatform.googleapis.com")
                    };
                    let url = format!(
                        "https://{host}/v1/projects/{project_id}/locations/{region}/publishers/anthropic/models/{}:rawPredict",
                        self.model
                    );
                    let body = VertexRequest {
                        anthropic_version: "vertex-2023-10-16".to_string(),
                        max_tokens,
                        temperature: 0.0,
                        system: system.to_string(),
                        messages: messages.clone(),
                    };
                    self.client
                        .post(&url)
                        .bearer_auth(&token)
                        .header("content-type", "application/json")
                        .json(&body)
                        .send()
                        .await
                }
                BackendKind::Anthropic { api_key } => {
                    let body = AnthropicRequest {
                        model: self.model.clone(),
                        max_tokens,
                        temperature: 0.0,
                        system: system.to_string(),
                        messages: messages.clone(),
                    };
                    self.client
                        .post("https://api.anthropic.com/v1/messages")
                        .header("x-api-key", api_key)
                        .header("anthropic-version", "2023-06-01")
                        .header("content-type", "application/json")
                        .json(&body)
                        .send()
                        .await
                }
            };

            match resp {
                Ok(r) => {
                    let status = r.status();
                    let text = r.text().await.unwrap_or_default();

                    if status.is_success() {
                        let extracted = match &self.backend {
                            BackendKind::OpenAiCompat { .. } => {
                                serde_json::from_str::<OpenAiResponse>(&text)
                                    .map(|r| {
                                        r.choices
                                            .into_iter()
                                            .map(|c| c.message.content)
                                            .collect::<Vec<_>>()
                                            .join("")
                                    })
                                    .map_err(|e| format!("parse error: {e}"))
                            }
                            BackendKind::GeminiVertex { .. } => serde_json::from_str::<
                                GeminiResponse,
                            >(&text)
                            .and_then(|r| {
                                let text = r
                                    .candidates
                                    .into_iter()
                                    .filter_map(|c| {
                                        c.content.parts.into_iter().filter_map(|p| p.text).next()
                                    })
                                    .collect::<Vec<_>>()
                                    .join("");
                                Ok(text)
                            })
                            .map_err(|e| format!("parse error: {e}")),
                            _ => serde_json::from_str::<AnthropicResponse>(&text)
                                .map(|r| {
                                    r.content
                                        .into_iter()
                                        .filter_map(|b| b.text)
                                        .collect::<Vec<_>>()
                                        .join("")
                                })
                                .map_err(|e| format!("parse error: {e}")),
                        };

                        match extracted {
                            Ok(text) => return Ok(text),
                            Err(e) => {
                                last_err = format!("{e}, body: {}", &text[..text.len().min(200)]);
                            }
                        }
                    } else if status == 429 || status.is_server_error() {
                        last_err = format!("HTTP {status}: {}", &text[..text.len().min(200)]);
                        continue;
                    } else {
                        return Err(format!("HTTP {status}: {}", &text[..text.len().min(500)]));
                    }
                }
                Err(e) => {
                    last_err = format!("request error: {e}");
                    continue;
                }
            }
        }

        Err(format!("failed after 3 attempts: {last_err}"))
    }
}
