//! LLM client for benchmark answer generation and judging.
//!
//! Supports three backends:
//! - **OpenAI-compatible** (Ollama, vLLM, etc.): Pass `--llm-url`.
//! - **Vertex AI**: Set `CLAUDE_CODE_USE_VERTEX=1`, `CLOUD_ML_REGION`, and
//!   `ANTHROPIC_VERTEX_PROJECT_ID`. Auth via `gcloud auth print-access-token`.
//! - **Anthropic API**: Set `ANTHROPIC_API_KEY`. Direct API calls.

use crate::model::MemoryId;
use crate::time::format_timestamp as format_timestamp_tz;

use chrono_tz::Tz;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

// ── Client ────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct LlmClient {
    client: Client,
    model: String,
    backend: BackendKind,
    backend_label: String,
}

#[derive(Debug)]
enum BackendKind {
    OpenAiCompat { base_url: String },
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

#[derive(Debug)]
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
        let mut id_to_label: std::collections::HashMap<MemoryId, String> =
            std::collections::HashMap::new();
        for (i, mem) in memories.iter().enumerate() {
            id_to_label.insert(mem.memory_id, format!("{}", i + 1));
        }
        for (i, (mid, _, _, _)) in graph_context.neighbors.iter().enumerate() {
            id_to_label.insert(*mid, format!("R{}", i + 1));
        }

        let relation_map = build_relation_map(&graph_context.relations, memories, &id_to_label);

        let context = memories
            .iter()
            .enumerate()
            .map(|(i, mem)| {
                let label = format!("{}", i + 1);
                let date = format_timestamp(mem.created_at);
                let mut line = format!(
                    "[{}] (score: {:.2}, {}) {}",
                    label, mem.score, date, mem.text
                );

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

        let base = "You are a memory assistant answering questions about long personal \
            conversations between two people who are friends. You receive retrieved memory \
            excerpts from these conversations, each with a relevance score (higher = \
            stronger match) and a date.\n\n\
            Memory types:\n\
            - Numbered memories [1], [2], etc. are directly retrieved for this question.\n\
            - Memories labeled [R1], [R2], etc. are related graph neighbors -- contextually \
            connected but not directly matched to the question. Use them as supporting \
            context.\n\n\
            Each memory may include structured metadata:\n\
            - Entities: named entities (people, places, etc.) mentioned in the memory.\n\
            - Topics: subject areas the memory relates to.\n\
            - Mood: emotional tone recorded at the time.\n\
            Use this metadata to verify person references and understand context.\n\n\
            When a memory contains a direct quote or specific detail, prefer that detail \
            over your general knowledge.";

        let instructions = match category {
            "open-domain" => {
                "\n\nInstructions:\n\
                - These questions ARE answerable from the evidence -- you MUST provide a \
                definitive answer.\n\
                - Reason step by step:\n\
                  1. Identify which memories contain relevant evidence.\n\
                  2. State what each relevant memory tells you.\n\
                  3. Combine the evidence into a clear conclusion.\n\
                - For hypothetical questions (\"would they...\", \"is it likely...\"), \
                commit to a clear yes/no with brief reasoning from evidence.\n\
                - For trait or preference questions (political views, personality, interests), \
                synthesize a conclusion from behavioral patterns. Name the trait directly \
                (e.g., \"liberal\", \"introverted\", \"athletic\").\n\
                - State your conclusion assertively. Do not hedge or add qualifiers like \
                \"possibly\" or \"it seems.\"\n\
                - Never say \"I don't know\" or \"there is not enough information.\"\n\
                - Keep your final answer concise (one or two sentences after your reasoning)."
            }
            "temporal" => {
                "\n\nInstructions:\n\
                - Base your answer ONLY on the provided memories.\n\
                - Reason about time step by step:\n\
                  1. Extract all dates and time references from the relevant memories.\n\
                  2. If the question asks WHEN something happened, find the memory that \
                  describes the event and report its date.\n\
                  3. If the question asks about ordering (first, last, before, after), \
                  arrange the relevant events chronologically using their dates.\n\
                  4. If the question asks about duration or intervals, calculate from \
                  the dates.\n\
                - Pay close attention to the dates shown in parentheses for each memory -- \
                these are the dates the information was recorded.\n\
                - Relative time references in memories (\"last week\", \"a few months ago\") \
                should be interpreted relative to that memory's date.\n\
                - If the memories do not contain enough information to answer the question, \
                say \"I don't know\".\n\
                - Keep your final answer concise -- state the date, time period, or ordering \
                directly."
            }
            "multi-hop" => {
                "\n\nInstructions:\n\
                - Base your answer ONLY on the provided memories.\n\
                - This question requires combining information from multiple memories. \
                Reason step by step:\n\
                  1. Break the question into sub-parts. What pieces of information do \
                  you need?\n\
                  2. Find the memory (or memories) that address each sub-part.\n\
                  3. Chain the pieces together to form your answer.\n\
                - For \"would they\" hypothetical questions, find evidence of relevant \
                preferences or behaviors, then apply common-sense reasoning to answer \
                yes or no.\n\
                - Memories may use different words for the same concept (e.g., \"plays \
                violin\" relates to \"classical music\"; \"collects classic children's \
                books\" relates to specific book titles).\n\
                - Check the related memories [R1], [R2], etc. -- they may contain the \
                connecting information between the directly retrieved memories.\n\
                - If the memories do not contain enough information to answer the question, \
                say \"I don't know\".\n\
                - Keep your final answer concise (one or two sentences after your reasoning)."
            }
            "adversarial" => {
                "\n\nInstructions:\n\
                - These questions may ask about something that was NOT discussed in the \
                conversation, or may attribute an event to the WRONG person.\n\
                - Before answering, carefully verify:\n\
                  1. Does any memory ACTUALLY describe the event or fact asked about?\n\
                  2. If a memory describes something similar, does it name the SAME person \
                  the question asks about? (e.g., if the question asks about Melanie but \
                  the memory says Caroline did it, that is NOT a match.)\n\
                - Common trap: The question swaps person names. A memory about Caroline's \
                camping trip does NOT answer a question about Melanie's camping trip.\n\
                - If no memory matches BOTH the event AND the person in the question, \
                say \"I don't know\" or \"This was not discussed.\"\n\
                - Only answer if you find a memory that specifically matches the person \
                AND the event asked about.\n\
                - Keep your answer concise."
            }
            _ => {
                "\n\nInstructions:\n\
                - Base your answer ONLY on the provided memories.\n\
                - These are direct fact-lookup questions. The answer should be stated or \
                closely paraphrased in one of the memories.\n\
                - If the question names a person, verify the memory is about THAT specific \
                person before answering.\n\
                - Prefer the highest-scoring memory when multiple memories seem relevant.\n\
                - Memories may describe the answer using different words than the question \
                (e.g., \"plays violin\" answers \"what instrument\"). Use the information \
                available even when the phrasing differs.\n\
                - If the memories do not contain enough information to answer the question, \
                say \"I don't know\".\n\
                - Keep your final answer concise (one or two sentences)."
            }
        };

        let system = format!("{base}{instructions}");

        let reasoning_hint = match category {
            "temporal" => "First extract the relevant dates from the memories, then answer.",
            "multi-hop" => "First identify the pieces of information needed, then combine them.",
            "adversarial" => {
                "Check: does any memory match BOTH the person and the event in the question?"
            }
            "open-domain" => "Identify the relevant evidence, then state your conclusion.",
            _ => "",
        };

        let user = if reasoning_hint.is_empty() {
            format!(
                "Retrieved memories:\n{context}{neighbor_context}\n\n\
                 Question: {question}\n\n\
                 Answer:"
            )
        } else {
            format!(
                "Retrieved memories:\n{context}{neighbor_context}\n\n\
                 Question: {question}\n\n\
                 {reasoning_hint}\n\n\
                 Answer:"
            )
        };

        self.call(&system, &user).await
    }

    pub async fn judge_answer(
        &self,
        question: &str,
        gold_answer: &str,
        generated_answer: &str,
        is_adversarial: bool,
    ) -> Result<JudgeResult, String> {
        let system = "You are evaluating conversational AI memory recall. Return JSON only \
            with the format requested."
            .to_string();

        let user = if is_adversarial {
            format!(
                "Your task is to label an answer to a question as 'CORRECT' or 'WRONG'.\n\n\
                 This is an ADVERSARIAL question — it asks about something that was never \
                 discussed in the conversation. The correct behavior is to indicate the \
                 question cannot be answered.\n\n\
                 Question: {question}\n\
                 Expected wrong answer (what a tricked system would say): {gold_answer}\n\
                 Generated answer: {generated_answer}\n\n\
                 The generated answer is CORRECT if it says \"I don't know\", \"unanswerable\", \
                 or otherwise refuses to answer. It is WRONG if it provides the expected \
                 wrong answer or any specific answer as if it were true.\n\n\
                 Return your response in JSON format with two keys: \
                 \"reasoning\" for your explanation and \"label\" for CORRECT or WRONG.\n\
                 Do NOT include both CORRECT and WRONG in your response."
            )
        } else {
            format!(
                "Your task is to label an answer to a question as 'CORRECT' or 'WRONG'. \
                 You will be given the following data:\n\
                 (1) a question (posed by one user to another user),\n\
                 (2) a 'gold' (ground truth) answer,\n\
                 (3) a generated answer\n\
                 which you will score as CORRECT/WRONG.\n\n\
                 The point of the question is to ask about something one user should know \
                 about the other user based on their prior conversations.\n\
                 The gold answer will usually be a concise and short answer that includes \
                 the referenced topic, for example:\n\
                 Question: Do you remember what I got the last time I went to Hawaii?\n\
                 Gold answer: A shell necklace\n\
                 The generated answer might be much longer, but you should be generous \
                 with your grading - as long as the core fact matches the gold answer, \
                 it should be counted as CORRECT. Synonyms, paraphrases, and additional \
                 correct details are all acceptable.\n\n\
                 For yes/no or inferential questions (e.g., gold answer is \"Likely no\" \
                 or \"Liberal\"), the generated answer is CORRECT if it reaches the same \
                 conclusion, regardless of wording or amount of reasoning shown.\n\n\
                 For time related questions, the gold answer will be a specific date, month, \
                 year, etc. The generated answer might be much longer or use relative time \
                 references (like \"last Tuesday\" or \"next month\"), but you should be \
                 generous with your grading - as long as it refers to the same date or time \
                 period as the gold answer, it should be counted as CORRECT. Even if the \
                 format differs (e.g., \"May 7th\" vs \"7 May\"), consider it CORRECT if \
                 it's the same date.\n\n\
                 Now it's time for the real question:\n\
                 Question: {question}\n\
                 Gold answer: {gold_answer}\n\
                 Generated answer: {generated_answer}\n\n\
                 Return your response in JSON format with two keys: \
                 \"reasoning\" for your explanation and \"label\" for CORRECT or WRONG.\n\
                 Do NOT include both CORRECT and WRONG in your response."
            )
        };

        let response = self.call(&system, &user).await?;
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

    pub async fn process_turn(
        &self,
        recent_turns: &[String],
        current_turn: &str,
        recent_memories: &[(String, String)],
        turn_timestamp_ms: i64,
    ) -> Result<Vec<MemoryToStore>, String> {
        let system = "You are an AI assistant listening to a conversation between two people. \
            You have a memory tool called store_memory that saves information for later recall.\n\n\
            You will see recent conversation context followed by the LATEST turn. \
            Store any new factual information revealed in the latest turn.\n\n\
            Guidelines:\n\
            - NEVER generalize specific nouns. Store the EXACT name, title, or object: \
            \"Charlotte's Web\" NOT \"a book\", \"hoodies\" NOT \"clothing\", \"Sweden\" NOT \
            \"her home country\", \"golden retriever\" NOT \"dog\", \"cup with a dog face\" \
            NOT \"pottery\".\n\
            - Always name specific items, places, brands, species, colors, and quantities.\n\
            - Store EVERY distinct fact: names, places, events, activities, preferences, \
            hobbies, relationships, opinions, plans, dates, specific details. Err on the side \
            of storing more rather than less.\n\
            - Store facts from BOTH speakers, not just the main topic.\n\
            - Small facts matter: pets' names, weekend plans, favorite foods, how long someone \
            has done something — these are exactly the kind of details questions will ask about.\n\
            - Quantities and durations: Always preserve specific numbers — \"5 years\", \
            \"every morning\", \"three times a week\", \"$200\". If someone says something \
            vague like \"we've been married a while\" but context gives a specific duration, \
            store the specific duration.\n\
            - Cross-speaker opinions: When person A expresses an opinion about person B \
            (e.g., \"I think she'd be an awesome mom\"), store it as its own separate memory \
            with both speakers as entities. These are distinct from factual statements.\n\
            - Inferrable conclusions: When facts in the conversation strongly imply a conclusion \
            (e.g., a bad experience on a trip implies they wouldn't want to repeat it), store \
            the inference as a separate memory with appropriate context. Tag it with the \
            relevant entities and topics so it can be found later.\n\
            - Multiple memories per turn is fine if the turn contains multiple distinct facts.\n\
            - Do NOT store: greetings, filler, or emotional reactions without new factual content.\n\
            - Be specific. Include names, titles, dates, objects, and details.\n\
            - Book/media titles: If a book, movie, song, or other titled work is discussed, \
            ALWAYS include the exact title in both the summary and full_text. If the speaker \
            references a work by description without naming it, try to identify it from context. \
            If you truly cannot identify it, store what you know but note the title is unknown.\n\
            - Always include relevant dates, times, and temporal markers in the summary. \
            If the fact has a date, the summary MUST contain it.\n\
            - If the conversation mentions relative time ('last week', 'yesterday', \
            'a few months ago'), resolve to an approximate absolute date using the \
            conversation timestamp provided below.\n\
            - Check your already-stored memories below. Do not store a fact that is already \
            captured. But if the turn adds ANY new detail not in your existing memories, store it.\n\
            - If an existing memory is OUTDATED or INCOMPLETE and the conversation has revealed \
            new information that changes it, store a new memory with \"supersedes\" set to the \
            ID of the old memory. The old memory will be replaced in search results.\n\n\
            store_memory accepts:\n\
            - \"summary\" (required, string): A concise 1-2 sentence summary of the key fact\n\
            - \"full_text\" (optional, string): A detailed version of the memory with all \
            relevant context, quotes, specifics, and surrounding details from the conversation. \
            Include who said what, exact names, dates, numbers, and any nuance. This should be \
            a rich, self-contained account that someone could read without seeing the original \
            conversation. Provide this for any memory where the summary alone would lose \
            important detail. Include direct quotes from the conversation when possible. \
            IMPORTANT: ALWAYS provide full_text for any memory containing specific names, titles, \
            dates, numbers, or quotes. The summary can be brief, but full_text must capture all \
            surrounding context verbatim — this is critical for recall accuracy.\n\
            - \"entities\" (required, array of strings): ALL people, pets, places, organizations, \
            book/movie/song titles, and proper nouns mentioned in this memory. You MUST always \
            provide this field. Use their canonical name (e.g., \"Caroline\", \"Oliver\", \"Sweden\", \
            \"Charlotte's Web\", \"LGBTQ\"). These are used for search indexing and graph linking.\n\
            - \"topics\" (required, array of strings): 1-5 topic keywords describing what the \
            memory is about. You MUST always provide this field. Examples: \"adoption\", \"career\", \
            \"cooking\", \"music\", \"camping\", \"self-care\", \"family\", \"art\", \"pets\", \
            \"travel\", \"health\", \"education\". Use lowercase single words or short phrases.\n\
            - \"emotions\" (optional, array of strings): Emotional tone of the memory if relevant. \
            Examples: \"happy\", \"anxious\", \"grateful\", \"excited\", \"sad\", \"proud\", \
            \"hopeful\", \"frustrated\", \"nostalgic\". Only include if the conversation has \
            clear emotional content.\n\
            - \"supersedes\" (optional, string): ID of an existing memory this one replaces. \
            Use when a fact has been updated, corrected, or significantly expanded.\n\n\
            Output JSON: {\"store_memory\": [...]} or {\"store_memory\": []} if nothing to store yet.\n\
            Output ONLY valid JSON, no markdown fences or explanation.";

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

        let response = self.call(system, &user_msg).await?;
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
        let system = "You have access to a memory search tool. Given a question, construct the optimal \
            search parameters to find relevant memories.\n\n\
            The tool accepts:\n\
            - \"queries\" (required, array of 1-3 objects): Each object has:\n\
              - \"query\" (required, string): Natural language search query embedded and compared \
              against stored memory summaries via semantic similarity. Write a descriptive, natural-language \
              query that sounds like a memory summary.\n\
              - \"fts_query\" (optional, string): Keyword-focused query for full-text search (BM25). \
              Include ONLY key entities, names, and distinctive terms. Example: for \"What did Sarah \
              say about her trip to Japan?\", use \"Sarah trip Japan\". When the question mentions a \
              specific proper noun (book title, place name, event name), ALWAYS include it in the \
              fts_query even if it appears in the semantic query too — FTS excels at exact name matching.\n\
            Use multiple queries when the question has multiple angles. For example:\n\
              - \"What books has Melanie read?\" -> one query about Melanie reading habits, another about \
              specific book titles Melanie mentioned.\n\
              - \"Would Caroline enjoy hiking?\" -> one query about Caroline's outdoor activities, another \
              about Caroline's exercise or fitness preferences.\n\
            For simple factual questions, one query is fine.\n\
            Each query in the set should come from a genuinely different angle — not paraphrases of \
            each other. Think about how the ANSWER might be phrased in a stored memory, not just how \
            the question is phrased. One query for direct matches, one for how the information might \
            have been originally described or stored.\n\
            For opinion or inference questions (\"Would X do Y?\", \"What does X think about Y?\"), \
            search for the underlying facts and experiences rather than the inference itself. The \
            memory system stores what happened, not what it means.\n\
            - \"entities\" (optional, array of strings): Filter to memories mentioning specific \
            people, places, or proper nouns. Use the canonical name as it would appear in a memory \
            (e.g., \"Caroline\", \"Sweden\", \"Oliver\"). When the question asks about a specific person, \
            include their name here to prioritize memories about them. Provide at most 1-2 entities.\n\
            - \"topics\" (optional, array of strings): Filter to memories about specific topics. \
            Use lowercase single words or short phrases. Examples: \"adoption\", \"cooking\", \"career\", \
            \"travel\", \"music\". Only include when the question clearly focuses on a specific subject \
            area and filtering would help narrow results. Do NOT use for broad questions. \
            Provide at most ONE topic -- if multiple are given, only memories matching ALL topics \
            will be returned, which is usually too restrictive.\n\
            - \"emotions\" (optional, array of strings): Filter to memories tagged with specific \
            emotions. Use lowercase single words. Examples: \"anxious\", \"excited\", \"grateful\", \
            \"frustrated\". Only include when the question specifically asks about feelings or \
            emotional states. Provide at most ONE emotion -- multiple emotions use AND logic.\n\
            - \"depth\" (optional, integer 0-3, default 2): Graph hops to include related memories. \
            Use depth 2 as the DEFAULT for any question about opinions, personality, hypotheticals, \
            \"would X...\", cross-person relationships, or questions requiring inference from multiple \
            facts. Use depth 1 only for simple direct factual lookups (\"When did X?\", \"What is X's \
            name?\"). Use depth 3 for complex multi-hop reasoning.\n\
            - \"time_range_start\" (optional, integer): Lower bound timestamp in milliseconds since Unix \
            epoch. Set when the question references a specific time period.\n\
            - \"time_range_end\" (optional, integer): Upper bound timestamp in milliseconds since Unix \
            epoch.\n\n\
            Output ONLY a JSON object with these fields. No markdown fences or explanation.";

        let user_msg = format!("Question: {question}");
        let response = self.call(system, &user_msg).await?;
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
                    depth: parsed.depth.unwrap_or(2).min(3),
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

    async fn call(&self, system: &str, user: &str) -> Result<String, String> {
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
                        max_tokens: 512,
                        temperature: 0.0,
                    };
                    self.client
                        .post(&url)
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
                        max_tokens: 512,
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
                        max_tokens: 512,
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
