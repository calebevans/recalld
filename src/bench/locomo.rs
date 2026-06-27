//! LoCoMo benchmark runner.
//!
//! Implements the Ingest → Search → Answer → Judge pipeline against
//! the LoCoMo dataset (Snap Research, ACL 2024). Each conversation
//! gets a fresh Recalld instance; raw dialogue turns are enriched via
//! LLM and stored as memories, then QA pairs are evaluated.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;
use std::time::Instant;

use rust_stemmers::{Algorithm, Stemmer};

use colored::Colorize;
use comfy_table::{Cell, Table};
use serde::{Deserialize, Serialize};

use crate::config::RecalldConfig;
use crate::model::record::DiskRecord;
use crate::model::{CachedRecord, DecayPhase, MemoryId};
use crate::search::PipelineSearchFilter;
use crate::search::{QueryMode, SearchQuery, VectorIndex, VectorMetadata};
use crate::storage::StorageEngine as _;
use crate::system::Recalld;

use super::claude::{GraphContext, LlmClient, MemoryContext, MemoryRelation};

// ── Category mapping ──────────────────────────────────────────────

fn category_name(id: u32) -> &'static str {
    match id {
        1 => "single-hop",
        2 => "temporal",
        3 => "multi-hop",
        4 => "open-domain",
        5 => "adversarial",
        _ => "unknown",
    }
}

// ── Dataset types ─────────────────────────────────────────────────

#[derive(Deserialize)]
struct RawConversation {
    #[serde(default)]
    sample_id: Option<String>,
    #[serde(default)]
    conversation: HashMap<String, serde_json::Value>,
    #[allow(dead_code)]
    observation: HashMap<String, serde_json::Value>,
    qa: Vec<RawQa>,
    #[allow(dead_code)]
    #[serde(flatten)]
    extra: HashMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
struct RawQa {
    question: String,
    #[serde(default, deserialize_with = "value_to_opt_string")]
    answer: Option<String>,
    #[serde(default, deserialize_with = "value_to_opt_string")]
    adversarial_answer: Option<String>,
    category: u32,
    #[allow(dead_code)]
    #[serde(default)]
    evidence: Vec<serde_json::Value>,
}

fn value_to_opt_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v: Option<serde_json::Value> = Option::deserialize(deserializer)?;
    Ok(v.map(|val| match val {
        serde_json::Value::String(s) => s,
        other => other.to_string(),
    }))
}

struct ConversationTurn {
    speaker: String,
    text: String,
    blip_caption: Option<String>,
    image_query: Option<String>,
    timestamp_ms: i64,
}

struct Conversation {
    id: String,
    #[allow(dead_code)]
    speaker_a: String,
    #[allow(dead_code)]
    speaker_b: String,
    turns: Vec<ConversationTurn>,
    qa_pairs: Vec<QaPair>,
}

struct QaPair {
    question: String,
    gold_answer: String,
    category: u32,
    is_adversarial: bool,
}

/// Parse a LoCoMo session date string like "1:56 pm on 8 May, 2023" into
/// a Unix timestamp in milliseconds.
fn parse_session_date(s: &str) -> Option<i64> {
    let s = s.trim();
    // Format: "H:MM am/pm on D Month, YYYY"
    let on_idx = s.find(" on ")?;
    let time_part = &s[..on_idx];
    let date_part = &s[on_idx + 4..];

    // Parse time.
    let (time_str, is_pm) = if let Some(t) = time_part.strip_suffix(" pm") {
        (t.trim(), true)
    } else if let Some(t) = time_part.strip_suffix(" am") {
        (t.trim(), false)
    } else {
        return None;
    };
    let mut parts = time_str.split(':');
    let mut hour: u32 = parts.next()?.parse().ok()?;
    let minute: u32 = parts.next()?.parse().ok()?;
    if is_pm && hour != 12 {
        hour += 12;
    } else if !is_pm && hour == 12 {
        hour = 0;
    }

    // Parse date: "8 May, 2023" or "20 January, 2023"
    let date_part = date_part.trim();
    let mut date_parts = date_part.splitn(3, ' ');
    let day: u32 = date_parts.next()?.parse().ok()?;
    let month_str = date_parts.next()?.trim_end_matches(',');
    let year: i32 = date_parts.next()?.parse().ok()?;

    let month: u32 = match month_str {
        "January" => 1,
        "February" => 2,
        "March" => 3,
        "April" => 4,
        "May" => 5,
        "June" => 6,
        "July" => 7,
        "August" => 8,
        "September" => 9,
        "October" => 10,
        "November" => 11,
        "December" => 12,
        _ => return None,
    };

    let dt = chrono::NaiveDate::from_ymd_opt(year, month, day)?.and_hms_opt(hour, minute, 0)?;
    let utc = dt.and_utc();
    Some(utc.timestamp_millis())
}

fn parse_dataset(path: &Path) -> Result<Vec<Conversation>, Box<dyn std::error::Error>> {
    let data = std::fs::read_to_string(path)?;
    let raw: Vec<RawConversation> = serde_json::from_str(&data)?;

    let now_ms = chrono::Utc::now().timestamp_millis();

    let mut conversations = Vec::with_capacity(raw.len());

    for (idx, raw_conv) in raw.into_iter().enumerate() {
        let id = raw_conv.sample_id.unwrap_or_else(|| format!("conv_{idx}"));

        // Build session number -> timestamp map from conversation dates.
        let mut session_dates: HashMap<u32, i64> = HashMap::new();
        for (key, value) in &raw_conv.conversation {
            if let Some(num) = key
                .strip_prefix("session_")
                .and_then(|s| s.strip_suffix("_date_time"))
                .and_then(|s| s.parse::<u32>().ok())
            {
                if let Some(date_str) = value.as_str() {
                    if let Some(ts) = parse_session_date(date_str) {
                        session_dates.insert(num, ts);
                    }
                }
            }
        }

        // Find the latest session date to compute the time shift.
        let time_shift: i64 = 0;

        // Identify the two speakers from the first session.
        let mut speaker_a = String::new();
        let mut speaker_b = String::new();

        // Collect session keys (e.g., "session_1", "session_2") and sort numerically.
        let mut session_keys: Vec<(u32, &str)> = Vec::new();
        for key in raw_conv.conversation.keys() {
            if key.ends_with("_date_time") {
                continue;
            }
            if let Some(num) = key
                .strip_prefix("session_")
                .and_then(|s| s.parse::<u32>().ok())
            {
                session_keys.push((num, key.as_str()));
            }
        }
        session_keys.sort_by_key(|(num, _)| *num);

        let mut turns = Vec::new();
        for (session_num, session_key) in &session_keys {
            let base_ts = session_dates.get(session_num).copied().unwrap_or(now_ms);
            let shifted_ts = base_ts + time_shift;

            let Some(turn_list) = raw_conv
                .conversation
                .get(*session_key)
                .and_then(|v| v.as_array())
            else {
                continue;
            };

            for (turn_idx, turn_val) in turn_list.iter().enumerate() {
                let Some(turn_obj) = turn_val.as_object() else {
                    continue;
                };

                let speaker = turn_obj
                    .get("speaker")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let text = turn_obj
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                if text.is_empty() && turn_obj.get("blip_caption").is_none() {
                    continue;
                }

                // Track speaker names.
                if !speaker.is_empty() {
                    if speaker_a.is_empty() {
                        speaker_a = speaker.clone();
                    } else if speaker_b.is_empty() && speaker != speaker_a {
                        speaker_b = speaker.clone();
                    }
                }

                let blip_caption = turn_obj
                    .get("blip_caption")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let image_query = turn_obj
                    .get("query")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                // Offset each turn slightly so they have distinct timestamps.
                let turn_ts = shifted_ts + (turn_idx as i64 * 1000);

                turns.push(ConversationTurn {
                    speaker,
                    text,
                    blip_caption,
                    image_query,
                    timestamp_ms: turn_ts,
                });
            }
        }

        // Turns are already in chronological order from sorted session keys.

        let qa_pairs: Vec<QaPair> = raw_conv
            .qa
            .into_iter()
            .filter_map(|qa| {
                let is_adversarial = qa.category == 5;
                let gold_answer = if is_adversarial {
                    qa.adversarial_answer?
                } else {
                    qa.answer?
                };
                Some(QaPair {
                    question: qa.question,
                    gold_answer,
                    category: qa.category,
                    is_adversarial,
                })
            })
            .collect();

        conversations.push(Conversation {
            id,
            speaker_a,
            speaker_b,
            turns,
            qa_pairs,
        });
    }

    Ok(conversations)
}

// ── Benchmark harness ─────────────────────────────────────────────

struct BenchHarness {
    system: Recalld,
    _temp_dir: tempfile::TempDir,
}

impl BenchHarness {
    async fn new(base_config: &RecalldConfig) -> Result<Self, Box<dyn std::error::Error>> {
        let temp_dir = tempfile::TempDir::new()?;

        let mut config = base_config.clone();
        config.storage.data_dir = temp_dir
            .path()
            .to_str()
            .ok_or("temp dir path is not valid UTF-8")?
            .to_string();
        config.decay.sweep_interval_hours = 999_999.0;
        config.decay.disable_sweep = true;

        let system = Recalld::new(config).await?;

        Ok(Self {
            system,
            _temp_dir: temp_dir,
        })
    }
}

// ── Results ───────────────────────────────────────────────────────

#[derive(Default, Serialize)]
struct CategoryStats {
    total: usize,
    correct: usize,
}

#[derive(Serialize)]
struct ConversationResult {
    id: String,
    turns: usize,
    memories_stored: usize,
    qa_total: usize,
    qa_correct: usize,
    accuracy: f64,
    ingestion_secs: f64,
    questions: Vec<QuestionResult>,
}

#[derive(Serialize)]
struct QuestionResult {
    question: String,
    gold_answer: String,
    generated_answer: String,
    correct: bool,
    category: String,
}

#[derive(Default, Serialize)]
pub struct LocomoReport {
    total_conversations: usize,
    total_qa: usize,
    total_turns: usize,
    total_correct: usize,
    model: String,
    ingest_model: String,
    judge_model: String,
    skip_adversarial: bool,
    stress_test: bool,
    top_k: usize,
    categories: HashMap<String, CategoryStats>,
    avg_retrieval_ms: f64,
    avg_answer_ms: f64,
    avg_judge_ms: f64,
    per_conversation: Vec<ConversationResult>,
}

// ── Main runner ───────────────────────────────────────────────────

pub async fn run(
    config: &RecalldConfig,
    data_path: &Path,
    top_k: usize,
    model: &str,
    ingest_model: &str,
    judge_model: &str,
    llm_url: Option<&str>,
    format: &str,
    skip_adversarial: bool,
    parallel: usize,
    qa_parallel: usize,
    stress_test: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let llm = LlmClient::new(model.to_string(), llm_url)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    let ingest_llm = LlmClient::new(ingest_model.to_string(), llm_url)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    let judge_llm = LlmClient::new(judge_model.to_string(), llm_url)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    eprintln!("  Loading dataset from {}...", data_path.display());
    let conversations = parse_dataset(data_path)?;

    let total_qa: usize = conversations
        .iter()
        .map(|c| {
            if skip_adversarial {
                c.qa_pairs.iter().filter(|q| !q.is_adversarial).count()
            } else {
                c.qa_pairs.len()
            }
        })
        .sum();
    let total_turns: usize = conversations.iter().map(|c| c.turns.len()).sum();
    let parallel = parallel.max(1);
    eprintln!(
        "  {} conversations, {} QA pairs{}, {} turns, parallelism: {}",
        conversations.len(),
        total_qa,
        if skip_adversarial {
            " (adversarial skipped)"
        } else {
            ""
        },
        total_turns,
        parallel,
    );
    eprintln!(
        "  Backend: {}    Model: {}    Ingest: {}    Judge: {}    Top-k: {}",
        llm.backend_label(),
        model,
        ingest_model,
        judge_model,
        top_k,
    );

    if stress_test {
        return run_stress_test(
            config,
            &conversations,
            top_k,
            &llm,
            &ingest_llm,
            &judge_llm,
            format,
            skip_adversarial,
            qa_parallel,
            model,
            ingest_model,
            judge_model,
        )
        .await;
    }

    let mut report = LocomoReport {
        total_conversations: conversations.len(),
        total_qa,
        total_turns,
        model: model.to_string(),
        ingest_model: ingest_model.to_string(),
        judge_model: judge_model.to_string(),
        skip_adversarial,
        top_k,
        ..Default::default()
    };

    let debug_log_path = std::path::PathBuf::from("bench_debug.log");
    let mut debug_log = std::io::BufWriter::new(std::fs::File::create(&debug_log_path)?);
    eprintln!("  Debug log: {}", debug_log_path.display());

    let mut retrieval_times = Vec::new();
    let mut answer_times = Vec::new();
    let mut judge_times = Vec::new();

    // Wrap conversations in Arc for sharing across tasks.
    let conversations = std::sync::Arc::new(conversations);
    let total_convs = conversations.len();

    // Spawn all conversations but limit concurrency with a semaphore.
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(parallel));
    let mut join_set = tokio::task::JoinSet::new();

    let qa_parallel = qa_parallel.max(1);
    for conv_idx in 0..total_convs {
        let config = config.clone();
        let llm = llm.clone();
        let ingest_llm = ingest_llm.clone();
        let judge_llm = judge_llm.clone();
        let conversations = conversations.clone();
        let sem = semaphore.clone();
        let delay = conv_idx as u64;

        join_set.spawn(async move {
            if delay > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay * 10)).await;
            }
            let _permit = sem.acquire().await.expect("semaphore closed");
            run_conversation(
                &config,
                &conversations[conv_idx],
                conv_idx,
                total_convs,
                &llm,
                &ingest_llm,
                &judge_llm,
                top_k,
                skip_adversarial,
                qa_parallel,
            )
            .await
        });
    }

    // Collect results as conversations complete.
    while let Some(result) = join_set.join_next().await {
        match result {
            Ok(Ok(conv_result)) => {
                let _ = debug_log.write_all(&conv_result.debug_buf);
                let _ = debug_log.flush();

                retrieval_times.extend(&conv_result.retrieval_times);
                answer_times.extend(&conv_result.answer_times);
                judge_times.extend(&conv_result.judge_times);

                for question in &conv_result.result.questions {
                    let stats = report
                        .categories
                        .entry(question.category.clone())
                        .or_default();
                    stats.total += 1;
                    if question.correct {
                        stats.correct += 1;
                        report.total_correct += 1;
                    }
                }

                report.per_conversation.push(conv_result.result);
            }
            Ok(Err(e)) => {
                eprintln!("  ERROR: conversation task failed: {e}");
            }
            Err(e) => {
                eprintln!("  ERROR: conversation task panicked: {e}");
            }
        }
    }

    // Sort per_conversation by id for deterministic output.
    report.per_conversation.sort_by(|a, b| a.id.cmp(&b.id));

    // Compute averages.
    report.avg_retrieval_ms = if retrieval_times.is_empty() {
        0.0
    } else {
        retrieval_times.iter().sum::<f64>() / retrieval_times.len() as f64
    };
    report.avg_answer_ms = if answer_times.is_empty() {
        0.0
    } else {
        answer_times.iter().sum::<f64>() / answer_times.len() as f64
    };
    report.avg_judge_ms = if judge_times.is_empty() {
        0.0
    } else {
        judge_times.iter().sum::<f64>() / judge_times.len() as f64
    };

    // Write results file.
    let results_path = std::path::PathBuf::from("bench_results.json");
    if let Ok(json) = serde_json::to_string_pretty(&report) {
        if let Err(e) = std::fs::write(&results_path, json) {
            eprintln!("  Warning: could not write results file: {e}");
        } else {
            eprintln!("  Results written to {}", results_path.display());
        }
    }

    match format {
        "json" => println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        ),
        _ => println!("{}", format_report(&report)),
    }

    Ok(())
}

// ── Stress test runner ───────────────────────────────────────────

/// Stress test: ingest ALL conversations into a single shared memory store,
/// then evaluate ALL QA pairs against it. Tests retrieval accuracy at scale
/// with cross-conversation noise.
async fn run_stress_test(
    config: &RecalldConfig,
    conversations: &[Conversation],
    top_k: usize,
    llm: &LlmClient,
    ingest_llm: &LlmClient,
    judge_llm: &LlmClient,
    format: &str,
    skip_adversarial: bool,
    qa_parallel: usize,
    model: &str,
    ingest_model: &str,
    judge_model: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let total_convs = conversations.len();
    let total_turns: usize = conversations.iter().map(|c| c.turns.len()).sum();
    let total_qa: usize = conversations
        .iter()
        .map(|c| {
            if skip_adversarial {
                c.qa_pairs.iter().filter(|q| !q.is_adversarial).count()
            } else {
                c.qa_pairs.len()
            }
        })
        .sum();

    eprintln!("  [stress-test] Mode: shared memory store");

    // Phase 1 — Ingest all conversations sequentially into one harness.
    let harness = std::sync::Arc::new(BenchHarness::new(config).await?);
    let mut total_memories = 0usize;

    for (conv_idx, conv) in conversations.iter().enumerate() {
        eprintln!(
            "  [stress-test] Ingesting conversation {}/{} ({}, {} turns)...",
            conv_idx + 1,
            total_convs,
            conv.id,
            conv.turns.len(),
        );
        let label = format!("[stress-test] {}", conv.id);
        let stored = ingest_conversation(&harness, conv, ingest_llm, &label).await?;
        total_memories += stored;
    }

    eprintln!(
        "  [stress-test] Ingestion complete: {} total memories from {} conversations",
        total_memories, total_convs,
    );

    // Phase 2 — Run all QA pairs against the shared store.
    let debug_log_path = std::path::PathBuf::from("bench_debug.log");
    let mut debug_log = std::io::BufWriter::new(std::fs::File::create(&debug_log_path)?);
    eprintln!("  Debug log: {}", debug_log_path.display());

    // Collect all QA pairs with their conversation ID.
    struct StressQa<'a> {
        conv_id: &'a str,
        question: &'a str,
        gold_answer: &'a str,
        category: u32,
        is_adversarial: bool,
    }

    let mut all_qa: Vec<StressQa> = Vec::with_capacity(total_qa);
    for conv in conversations {
        for qa in &conv.qa_pairs {
            if skip_adversarial && qa.is_adversarial {
                continue;
            }
            all_qa.push(StressQa {
                conv_id: &conv.id,
                question: &qa.question,
                gold_answer: &qa.gold_answer,
                category: qa.category,
                is_adversarial: qa.is_adversarial,
            });
        }
    }

    let qa_parallel = qa_parallel.max(1);
    let qa_sem = std::sync::Arc::new(tokio::sync::Semaphore::new(qa_parallel));
    let completed_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let correct_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let qa_total = all_qa.len();

    let mut qa_join_set = tokio::task::JoinSet::new();

    for (qa_idx, sqa) in all_qa.iter().enumerate() {
        let harness = harness.clone();
        let llm = llm.clone();
        let judge_llm = judge_llm.clone();
        let qa_sem = qa_sem.clone();
        let completed_count = completed_count.clone();
        let correct_count = correct_count.clone();
        let question = sqa.question.to_string();
        let gold_answer = sqa.gold_answer.to_string();
        let category = sqa.category;
        let is_adversarial = sqa.is_adversarial;
        let conv_id = sqa.conv_id.to_string();

        qa_join_set.spawn(async move {
            let _permit = qa_sem.acquire().await.expect("qa semaphore closed");

            let cat_name = category_name(category).to_string();
            let mut debug_buf: Vec<u8> = Vec::new();

            // Search.
            let search_start = Instant::now();
            let memories =
                search_memories(&harness, &question, top_k, Some(&llm), &mut debug_buf)
                    .await
                    .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                        format!("[stress-test] search failed: {e}").into()
                    })?;
            let retrieval_ms = search_start.elapsed().as_secs_f64() * 1000.0;

            // Collect graph context.
            let graph_context = collect_graph_context(&harness, &memories).await;

            // Build memory contexts.
            let mem_contexts: Vec<MemoryContext> = memories
                .iter()
                .map(|m| {
                    let text = match &m.full_text {
                        Some(ft) => format!("{}\nOriginal: {}", m.text, ft),
                        None => m.text.clone(),
                    };
                    let metadata = crate::model::parse_structured_tags(&m.tags);
                    MemoryContext {
                        memory_id: m.memory_id,
                        text,
                        score: m.score,
                        created_at: m.created_at,
                        entities: metadata.entities,
                        topics: metadata.topics,
                        emotions: metadata.emotions,
                    }
                })
                .collect();

            let answer_start = Instant::now();
            let generated = llm
                .generate_answer(&question, &mem_contexts, "unknown", &graph_context)
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
            let answer_ms = answer_start.elapsed().as_secs_f64() * 1000.0;

            // Judge.
            let judge_start = Instant::now();
            let result = judge_llm
                .judge_answer(&question, &gold_answer, &generated, is_adversarial)
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
            let judge_ms = judge_start.elapsed().as_secs_f64() * 1000.0;

            let verdict = if result.correct { "CORRECT" } else { "WRONG" };
            let _ = writeln!(debug_buf, "  [{}] Q: {}", conv_id, question);
            let _ = writeln!(debug_buf, "  Gold: {}", gold_answer);
            let _ = writeln!(
                debug_buf,
                "  Gen:  {}",
                generated.chars().take(300).collect::<String>()
            );
            let _ = writeln!(debug_buf, "  -> {} ({})", verdict, cat_name);
            let _ = writeln!(debug_buf);

            if result.correct {
                correct_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            let done = completed_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;

            // Report progress every 100 questions or on the last one.
            if done % 100 == 0 || done == qa_total {
                let cur_correct = correct_count.load(std::sync::atomic::Ordering::Relaxed);
                let running_acc = if done > 0 {
                    cur_correct as f64 / done as f64 * 100.0
                } else {
                    0.0
                };
                eprintln!(
                    "  [stress-test] QA {done}/{total} ({acc:.1}%)",
                    total = qa_total,
                    acc = running_acc,
                );
            }

            Ok(StressQaResult {
                qa_idx,
                conv_id,
                question_result: QuestionResult {
                    question,
                    gold_answer,
                    generated_answer: generated,
                    correct: result.correct,
                    category: cat_name,
                },
                debug_buf,
                retrieval_ms,
                answer_ms,
                judge_ms,
            }) as Result<StressQaResult, Box<dyn std::error::Error + Send + Sync>>
        });
    }

    // Collect all QA results.
    let mut qa_results: Vec<StressQaResult> = Vec::with_capacity(qa_total);
    while let Some(result) = qa_join_set.join_next().await {
        match result {
            Ok(Ok(qa_result)) => qa_results.push(qa_result),
            Ok(Err(e)) => {
                eprintln!("  [stress-test] ERROR: QA task failed: {e}");
            }
            Err(e) => {
                eprintln!("  [stress-test] ERROR: QA task panicked: {e}");
            }
        }
    }

    // Sort by original index for deterministic output.
    qa_results.sort_by_key(|r| r.qa_idx);

    // Build per-conversation results.
    let mut conv_results: HashMap<String, ConversationResult> = HashMap::new();
    for conv in conversations {
        conv_results.insert(
            conv.id.clone(),
            ConversationResult {
                id: conv.id.clone(),
                turns: conv.turns.len(),
                memories_stored: 0, // Not tracked per-conv in stress test
                qa_total: 0,
                qa_correct: 0,
                accuracy: 0.0,
                ingestion_secs: 0.0,
                questions: Vec::new(),
            },
        );
    }

    let mut retrieval_times = Vec::new();
    let mut answer_times = Vec::new();
    let mut judge_times = Vec::new();
    let mut report = LocomoReport {
        total_conversations: total_convs,
        total_qa,
        total_turns,
        model: model.to_string(),
        ingest_model: ingest_model.to_string(),
        judge_model: judge_model.to_string(),
        skip_adversarial,
        stress_test: true,
        top_k,
        ..Default::default()
    };

    for qa_result in qa_results {
        let _ = debug_log.write_all(&qa_result.debug_buf);
        let _ = debug_log.flush();

        retrieval_times.push(qa_result.retrieval_ms);
        answer_times.push(qa_result.answer_ms);
        judge_times.push(qa_result.judge_ms);

        let stats = report
            .categories
            .entry(qa_result.question_result.category.clone())
            .or_default();
        stats.total += 1;
        if qa_result.question_result.correct {
            stats.correct += 1;
            report.total_correct += 1;
        }

        if let Some(cr) = conv_results.get_mut(&qa_result.conv_id) {
            cr.qa_total += 1;
            if qa_result.question_result.correct {
                cr.qa_correct += 1;
            }
            cr.questions.push(qa_result.question_result);
        }
    }

    // Finalize per-conversation accuracy.
    for cr in conv_results.values_mut() {
        cr.accuracy = if cr.qa_total > 0 {
            cr.qa_correct as f64 / cr.qa_total as f64 * 100.0
        } else {
            0.0
        };
    }

    report.per_conversation = {
        let mut v: Vec<ConversationResult> = conv_results.into_values().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    };

    // Compute averages.
    report.avg_retrieval_ms = if retrieval_times.is_empty() {
        0.0
    } else {
        retrieval_times.iter().sum::<f64>() / retrieval_times.len() as f64
    };
    report.avg_answer_ms = if answer_times.is_empty() {
        0.0
    } else {
        answer_times.iter().sum::<f64>() / answer_times.len() as f64
    };
    report.avg_judge_ms = if judge_times.is_empty() {
        0.0
    } else {
        judge_times.iter().sum::<f64>() / judge_times.len() as f64
    };

    // Write results file.
    let results_path = std::path::PathBuf::from("bench_results.json");
    if let Ok(json) = serde_json::to_string_pretty(&report) {
        if let Err(e) = std::fs::write(&results_path, json) {
            eprintln!("  Warning: could not write results file: {e}");
        } else {
            eprintln!("  Results written to {}", results_path.display());
        }
    }

    match format {
        "json" => println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        ),
        _ => println!("{}", format_report(&report)),
    }

    Ok(())
}

/// Result from a single parallel QA task in stress test mode.
struct StressQaResult {
    qa_idx: usize,
    conv_id: String,
    question_result: QuestionResult,
    debug_buf: Vec<u8>,
    retrieval_ms: f64,
    answer_ms: f64,
    judge_ms: f64,
}

/// Internal result from running a single conversation, including data
/// needed by the caller to merge into the global report.
struct ConversationTaskResult {
    result: ConversationResult,
    debug_buf: Vec<u8>,
    retrieval_times: Vec<f64>,
    answer_times: Vec<f64>,
    judge_times: Vec<f64>,
}

/// Result from a single parallel QA task.
struct QaTaskResult {
    /// Original index in the active_qa list, for deterministic ordering.
    qa_idx: usize,
    question_result: QuestionResult,
    debug_buf: Vec<u8>,
    retrieval_ms: f64,
    answer_ms: f64,
    judge_ms: f64,
}

async fn run_conversation(
    config: &RecalldConfig,
    conv: &Conversation,
    conv_idx: usize,
    total_convs: usize,
    llm: &LlmClient,
    ingest_llm: &LlmClient,
    judge_llm: &LlmClient,
    top_k: usize,
    skip_adversarial: bool,
    qa_parallel: usize,
) -> Result<ConversationTaskResult, Box<dyn std::error::Error + Send + Sync>> {
    let tag = format!("{}", conv.id);
    let pos = format!("[{}/{}]", conv_idx + 1, total_convs);

    eprintln!(
        "  {pos} {tag}: {turns} turns, {qa} QA",
        turns = conv.turns.len(),
        qa = if skip_adversarial {
            conv.qa_pairs.iter().filter(|q| !q.is_adversarial).count()
        } else {
            conv.qa_pairs.len()
        },
    );

    let harness = BenchHarness::new(config).await.map_err(
        |e| -> Box<dyn std::error::Error + Send + Sync> {
            format!("{pos} {tag}: harness init failed: {e}").into()
        },
    )?;

    eprintln!("  {pos} {tag}: ingesting...");
    let ingest_start = Instant::now();
    let ingest_label = format!("{pos} {tag}");
    let stored = ingest_conversation(&harness, conv, ingest_llm, &ingest_label)
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            format!("{pos} {tag}: ingestion failed: {e}").into()
        })?;
    let ingestion_secs = ingest_start.elapsed().as_secs_f64();
    eprintln!(
        "  {pos} {tag}: ingested {stored} memories in {secs:.1}s",
        secs = ingestion_secs,
    );

    // Evaluate QA pairs concurrently.
    let active_qa: Vec<_> = if skip_adversarial {
        conv.qa_pairs.iter().filter(|q| !q.is_adversarial).collect()
    } else {
        conv.qa_pairs.iter().collect()
    };

    let qa_total = active_qa.len();
    let harness = std::sync::Arc::new(harness);
    let qa_sem = std::sync::Arc::new(tokio::sync::Semaphore::new(qa_parallel));
    let completed_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let correct_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let mut qa_join_set = tokio::task::JoinSet::new();

    for (qa_idx, qa) in active_qa.iter().enumerate() {
        let harness = harness.clone();
        let llm = llm.clone();
        let judge_llm = judge_llm.clone();
        let qa_sem = qa_sem.clone();
        let completed_count = completed_count.clone();
        let correct_count = correct_count.clone();
        let question = qa.question.clone();
        let gold_answer = qa.gold_answer.clone();
        let category = qa.category;
        let is_adversarial = qa.is_adversarial;
        let pos = pos.clone();
        let tag = tag.clone();
        let qa_total = qa_total;

        qa_join_set.spawn(async move {
            let _permit = qa_sem.acquire().await.expect("qa semaphore closed");

            let cat_name = category_name(category).to_string();
            let mut debug_buf: Vec<u8> = Vec::new();

            // Search.
            let search_start = Instant::now();
            let memories = search_memories(&harness, &question, top_k, Some(&llm), &mut debug_buf)
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                    format!("{pos} {tag}: search failed: {e}").into()
                })?;
            let retrieval_ms = search_start.elapsed().as_secs_f64() * 1000.0;

            // Collect graph context: neighbors + relationships.
            let graph_context = collect_graph_context(&harness, &memories).await;

            // Build memory contexts for the answer LLM.
            let mem_contexts: Vec<MemoryContext> = memories
                .iter()
                .map(|m| {
                    let text = match &m.full_text {
                        Some(ft) => format!("{}\nOriginal: {}", m.text, ft),
                        None => m.text.clone(),
                    };
                    let metadata = crate::model::parse_structured_tags(&m.tags);
                    MemoryContext {
                        memory_id: m.memory_id,
                        text,
                        score: m.score,
                        created_at: m.created_at,
                        entities: metadata.entities,
                        topics: metadata.topics,
                        emotions: metadata.emotions,
                    }
                })
                .collect();
            let answer_start = Instant::now();
            let generated = llm
                .generate_answer(&question, &mem_contexts, "unknown", &graph_context)
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
            let answer_ms = answer_start.elapsed().as_secs_f64() * 1000.0;

            // Judge (separate model to avoid self-grading bias).
            let judge_start = Instant::now();
            let result = judge_llm
                .judge_answer(&question, &gold_answer, &generated, is_adversarial)
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
            let judge_ms = judge_start.elapsed().as_secs_f64() * 1000.0;

            let verdict = if result.correct { "CORRECT" } else { "WRONG" };
            let _ = writeln!(debug_buf, "  Q: {}", question);
            let _ = writeln!(debug_buf, "  Gold: {}", gold_answer);
            let _ = writeln!(
                debug_buf,
                "  Gen:  {}",
                generated.chars().take(300).collect::<String>()
            );
            let _ = writeln!(debug_buf, "  -> {} ({})", verdict, cat_name);
            let _ = writeln!(debug_buf);

            if result.correct {
                correct_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            let done = completed_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;

            // Report progress every 20 questions or on the last one.
            if done % 20 == 0 || done == qa_total {
                let cur_correct = correct_count.load(std::sync::atomic::Ordering::Relaxed);
                let running_acc = if done > 0 {
                    cur_correct as f64 / done as f64 * 100.0
                } else {
                    0.0
                };
                eprintln!(
                    "  {pos} {tag}: QA {done}/{total} ({acc:.1}%)",
                    total = qa_total,
                    acc = running_acc,
                );
            }

            Ok(QaTaskResult {
                qa_idx,
                question_result: QuestionResult {
                    question,
                    gold_answer,
                    generated_answer: generated,
                    correct: result.correct,
                    category: cat_name,
                },
                debug_buf,
                retrieval_ms,
                answer_ms,
                judge_ms,
            }) as Result<QaTaskResult, Box<dyn std::error::Error + Send + Sync>>
        });
    }

    // Collect all QA results.
    let mut qa_results: Vec<QaTaskResult> = Vec::with_capacity(qa_total);
    while let Some(result) = qa_join_set.join_next().await {
        match result {
            Ok(Ok(qa_result)) => qa_results.push(qa_result),
            Ok(Err(e)) => {
                return Err(format!("{pos} {tag}: QA task failed: {e}").into());
            }
            Err(e) => {
                return Err(format!("{pos} {tag}: QA task panicked: {e}").into());
            }
        }
    }

    // Sort by original index for deterministic output.
    qa_results.sort_by_key(|r| r.qa_idx);

    // Merge results in order.
    let mut debug_buf: Vec<u8> = Vec::new();
    let mut retrieval_times = Vec::new();
    let mut answer_times = Vec::new();
    let mut judge_times = Vec::new();
    let mut questions = Vec::new();
    let mut qa_correct = 0usize;

    for qa_result in qa_results {
        debug_buf.extend_from_slice(&qa_result.debug_buf);
        retrieval_times.push(qa_result.retrieval_ms);
        answer_times.push(qa_result.answer_ms);
        judge_times.push(qa_result.judge_ms);
        if qa_result.question_result.correct {
            qa_correct += 1;
        }
        questions.push(qa_result.question_result);
    }

    let accuracy = if qa_total > 0 {
        qa_correct as f64 / qa_total as f64 * 100.0
    } else {
        0.0
    };

    Ok(ConversationTaskResult {
        result: ConversationResult {
            id: conv.id.clone(),
            turns: conv.turns.len(),
            memories_stored: stored,
            qa_total,
            qa_correct,
            accuracy,
            ingestion_secs,
            questions,
        },
        debug_buf,
        retrieval_times,
        answer_times,
        judge_times,
    })
}

// ── Ingest ────────────────────────────────────────────────────────

async fn store_single_memory(
    harness: &BenchHarness,
    summary: &str,
    full_text: Option<&str>,
    tag_strings: &[String],
    llm_entities: &[String],
    timestamp_ms: i64,
    ingested_memories: &[(MemoryId, i64)],
    supersedes: Option<MemoryId>,
) -> Result<MemoryId, Box<dyn std::error::Error>> {
    let ns_config = {
        let storage_r = harness
            .system
            .storage()
            .read()
            .map_err(|e| format!("storage lock poisoned: {e}"))?;
        storage_r
            .get_namespace_by_name("default")?
            .ok_or("namespace 'default' not found")?
    };

    let tags: Vec<crate::model::Tag> = tag_strings
        .iter()
        .filter_map(|t| crate::model::Tag::new(t).ok())
        .collect();

    let mut embed_text = match full_text {
        Some(ft) => format!("{}\n\n{}", summary, ft),
        None => summary.to_string(),
    };
    if !tag_strings.is_empty() {
        embed_text = format!("{} {}", embed_text, tag_strings.join(" "));
    }
    let embedding = harness
        .system
        .embedding()
        .embed(&embed_text)
        .await
        .map_err(|e| format!("embedding failed: {e}"))?;

    let memory_id = MemoryId::new();

    let mut record = DiskRecord {
        version: DiskRecord::CURRENT_VERSION,
        id: *memory_id.as_bytes(),
        namespace_id: ns_config.id.get(),
        created_at: timestamp_ms,
        last_accessed_at: timestamp_ms,
        phase: DecayPhase::Full,
        strength: 1.0,
        decay_strength: 1.0,
        stability: ns_config.initial_stability,
        difficulty: 5.0,
        is_permastore: 0,
        vector_slot: 0,
        edge_count: 0,
        summary: summary.to_string(),
        tags,
        access_history: Vec::new(),
        text_offset: 0,
        text_length: 0,
    };

    {
        let mut storage_w = harness
            .system
            .storage()
            .write()
            .map_err(|e| format!("storage lock poisoned: {e}"))?;
        storage_w.insert_memory(memory_id, ns_config.id, &mut record, &embedding, full_text)?;
    }

    let cached = CachedRecord::from(&record);
    harness.system.cache().insert(memory_id, cached).await;

    {
        let mut index = harness.system.vector_index().write().await;
        let metadata = VectorMetadata {
            namespace_id: ns_config.id,
            decay_phase: DecayPhase::Full.as_u8(),
            tags: tag_strings.to_vec(),
        };
        index.add(memory_id, &embedding, metadata)?;
    }

    {
        let fts = harness.system.fts_index().lock().await;
        if let Err(e) = fts.add(ns_config.id, memory_id, summary, full_text, tag_strings) {
            eprintln!("FTS index error: {e}");
        }
    }

    {
        let mut graph_w = harness.system.graph().write().await;
        let _ = graph_w.add_node(
            memory_id,
            ns_config.id,
            DecayPhase::Full,
            1.0,
            record.vector_slot,
        );

        if let Some(old_id) = supersedes {
            let _ = graph_w.add_edge(
                memory_id,
                old_id,
                crate::model::EdgeType::Supersedes,
                1.0,
                false,
            );
        }
    }

    let entities = llm_entities.to_vec();
    if harness.system.config().graph.autolink_enabled {
        let threshold = harness.system.config().graph.auto_link_threshold as f32;
        let max_links = harness.system.config().graph.max_auto_links;

        let _ = crate::graph::perform_autolink(
            memory_id,
            ns_config.id,
            &embedding,
            tag_strings,
            threshold,
            max_links,
            harness.system.vector_index(),
            harness.system.graph(),
            harness.system.storage(),
            harness.system.cache(),
        )
        .await;

        if !entities.is_empty() {
            let max_entity_links = harness.system.config().graph.max_entity_links;
            let _ = crate::graph::perform_entity_link(
                memory_id,
                ns_config.id,
                &entities,
                max_entity_links,
                harness.system.entity_index(),
                harness.system.graph(),
                harness.system.storage(),
                harness.system.cache(),
            )
            .await;
        }

        let temporal_window_ms = harness.system.config().graph.temporal_window_ms;
        let max_temporal_links = harness.system.config().graph.max_temporal_links;
        let _ = crate::graph::perform_temporal_link(
            memory_id,
            ns_config.id,
            timestamp_ms,
            temporal_window_ms,
            max_temporal_links,
            ingested_memories,
            harness.system.graph(),
            harness.system.storage(),
            harness.system.cache(),
        )
        .await;
    }

    if !entities.is_empty() {
        let mut idx = harness.system.entity_index().write().await;
        idx.add(memory_id, &entities);
    }

    Ok(memory_id)
}

async fn ingest_conversation(
    harness: &BenchHarness,
    conv: &Conversation,
    llm: &LlmClient,
    label: &str,
) -> Result<usize, Box<dyn std::error::Error>> {
    let mut ingested_memories: Vec<(MemoryId, i64)> = Vec::new();
    let mut stored_memories: Vec<(String, String)> = Vec::new(); // (id, summary)
    let mut total_stored = 0usize;
    let mut recent_turns: Vec<String> = Vec::new();
    let total_turns = conv.turns.len();
    const CONTEXT_WINDOW: usize = 20;
    const MEMORY_WINDOW: usize = 30;

    for (turn_idx, turn) in conv.turns.iter().enumerate() {
        if turn_idx % 50 == 0 && turn_idx > 0 {
            eprintln!("  {label}: ingesting... {turn_idx}/{total_turns} turns");
        }

        let mut turn_text = format!("{}: {}", turn.speaker, turn.text);
        if let Some(caption) = &turn.blip_caption {
            let query = turn.image_query.as_deref().unwrap_or("shared image");
            turn_text.push_str(&format!(
                "\n[{} shared an image — \"{}\". The image shows: {}]",
                turn.speaker, query, caption
            ));
        }

        let context: Vec<String> = if recent_turns.len() > CONTEXT_WINDOW {
            recent_turns[recent_turns.len() - CONTEXT_WINDOW..].to_vec()
        } else {
            recent_turns.clone()
        };

        let recent_mems: Vec<(String, String)> = if stored_memories.len() > MEMORY_WINDOW {
            stored_memories[stored_memories.len() - MEMORY_WINDOW..].to_vec()
        } else {
            stored_memories.clone()
        };

        let memories = match llm
            .process_turn(&context, &turn_text, &recent_mems, turn.timestamp_ms)
            .await
        {
            Ok(mems) => mems,
            Err(_) => Vec::new(),
        };

        for mem in &memories {
            let supersedes_id = mem
                .supersedes
                .as_ref()
                .and_then(|s| uuid::Uuid::parse_str(s).ok())
                .map(MemoryId::from_uuid);

            let mut tag_strings: Vec<String> = Vec::new();
            for entity in &mem.entities {
                tag_strings.push(format!("entity/{}", entity.to_lowercase()));
            }
            for topic in &mem.topics {
                tag_strings.push(format!("topic/{}", topic.to_lowercase()));
            }
            for emotion in &mem.emotions {
                tag_strings.push(format!("emotion/{}", emotion.to_lowercase()));
            }

            let mid = store_single_memory(
                harness,
                &mem.summary,
                mem.full_text.as_deref(),
                &tag_strings,
                &mem.entities,
                turn.timestamp_ms,
                &ingested_memories,
                supersedes_id,
            )
            .await?;
            ingested_memories.push((mid, turn.timestamp_ms));
            stored_memories.push((mid.to_string(), mem.summary.clone()));
            total_stored += 1;
        }

        recent_turns.push(turn_text);
    }

    Ok(total_stored)
}

// ── Search ────────────────────────────────────────────────────────

#[derive(Clone)]
struct ScoredMemory {
    memory_id: MemoryId,
    text: String,
    full_text: Option<String>,
    score: f32,
    created_at: i64,
    tags: Vec<crate::model::Tag>,
}

fn load_full_texts(
    harness: &BenchHarness,
    results: Vec<crate::search::PipelineSearchResult>,
) -> Result<Vec<ScoredMemory>, Box<dyn std::error::Error>> {
    let mut storage_w = harness
        .system
        .storage()
        .write()
        .map_err(|e| format!("storage lock poisoned: {e}"))?;

    let mut scored = Vec::with_capacity(results.len());
    for r in results {
        let summary = match r.summary {
            Some(s) => s,
            None => continue,
        };

        let full_text = if let Ok(Some(disk)) = storage_w.get_record(r.memory_id) {
            if disk.text_length > 0 {
                let text_ref = crate::storage::TextRef {
                    file_offset: disk.text_offset,
                    length: disk.text_length,
                };
                storage_w.get_text(text_ref).ok().flatten()
            } else {
                None
            }
        } else {
            None
        };

        scored.push(ScoredMemory {
            memory_id: r.memory_id,
            text: summary,
            full_text,
            score: r.composite_score.unwrap_or_else(|| r.score.unwrap_or(0.0)),
            created_at: r.created_at,
            tags: r.tags,
        });
    }
    Ok(scored)
}

async fn search_memories(
    harness: &BenchHarness,
    question: &str,
    top_k: usize,
    search_llm: Option<&LlmClient>,
    debug_log: &mut impl Write,
) -> Result<Vec<ScoredMemory>, Box<dyn std::error::Error>> {
    use crate::bench::claude::SearchQuery as BenchSearchQuery;

    let (queries, search_entities, require_tags, graph_depth, time_range_start, time_range_end) =
        if let Some(llm) = search_llm {
            match llm.construct_search_query(question).await {
                Ok(params) => {
                    let mut tags: Vec<crate::model::Tag> = Vec::new();
                    for entity in &params.entities {
                        if let Ok(tag) =
                            crate::model::Tag::new(format!("entity/{}", entity.to_lowercase()))
                        {
                            tags.push(tag);
                        }
                    }
                    for topic in &params.topics {
                        if let Ok(tag) = crate::model::Tag::new(format!("topic/{}", topic)) {
                            tags.push(tag);
                        }
                    }
                    for emotion in &params.emotions {
                        if let Ok(tag) = crate::model::Tag::new(format!("emotion/{}", emotion)) {
                            tags.push(tag);
                        }
                    }
                    (
                        params.queries,
                        params.entities,
                        tags,
                        params.depth.min(3) as u8,
                        params.time_range_start,
                        params.time_range_end,
                    )
                }
                Err(e) => {
                    eprintln!("      Warning: search query construction failed: {e}");
                    (
                        vec![BenchSearchQuery {
                            query: question.to_string(),
                            fts_query: None,
                        }],
                        Vec::new(),
                        Vec::new(),
                        1u8,
                        None,
                        None,
                    )
                }
            }
        } else {
            (
                vec![BenchSearchQuery {
                    query: question.to_string(),
                    fts_query: None,
                }],
                Vec::new(),
                Vec::new(),
                1u8,
                None,
                None,
            )
        };

    let _ = writeln!(
        debug_log,
        "  Depth: {graph_depth}, TimeRange: {:?}..{:?}",
        time_range_start, time_range_end
    );
    if !require_tags.is_empty() {
        let tag_strs: Vec<&str> = require_tags.iter().map(|t| t.as_str()).collect();
        let _ = writeln!(debug_log, "  Filters: {:?}", tag_strs);
    }

    let mut merged: HashMap<MemoryId, ScoredMemory> = HashMap::new();

    for (qi, bq) in queries.iter().enumerate() {
        let _ = writeln!(debug_log, "  Query[{}]: {:?}", qi + 1, bq.query);
        if let Some(ref fq) = bq.fts_query {
            let _ = writeln!(debug_log, "  FTS[{}]:   {:?}", qi + 1, fq);
        }

        let per_query_limit = if queries.len() > 1 {
            top_k / 2 + 5
        } else {
            top_k
        };
        let filter = PipelineSearchFilter {
            require_tags: require_tags.clone(),
            ..PipelineSearchFilter::default()
        };
        let query = SearchQuery {
            text: Some(bq.query.clone()),
            fts_query: bq.fts_query.clone(),
            namespace: "default".to_string(),
            filter,
            limit: per_query_limit,
            min_score: 0.0,
            include_ghosts: false,
            query_mode: QueryMode::EmbeddingPlusMetadata,
            graph_depth,
            time_range_start,
            time_range_end,
            entities: search_entities.clone(),
        };

        let response = harness.system.query_engine().search(query).await?;
        let results = load_full_texts(harness, response.results)?;

        for r in results {
            merged
                .entry(r.memory_id)
                .and_modify(|existing| {
                    if r.score > existing.score {
                        *existing = r.clone();
                    }
                })
                .or_insert(r);
        }
    }

    let mut results: Vec<ScoredMemory> = merged.into_values().collect();
    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    results.truncate(top_k);

    for (i, r) in results.iter().enumerate().take(5) {
        let snippet: String = r.text.chars().take(120).collect();
        let _ = writeln!(
            debug_log,
            "  [{}] score={:.3} | {}",
            i + 1,
            r.score,
            snippet
        );
    }

    Ok(results)
}

/// Collect graph context: 1-hop neighbor summaries + edge relationships
/// between all memories (results and neighbors).
async fn collect_graph_context(harness: &BenchHarness, results: &[ScoredMemory]) -> GraphContext {
    let result_ids: HashSet<MemoryId> = results.iter().map(|m| m.memory_id).collect();

    // Build label map for results.
    let mut id_to_label: HashMap<MemoryId, String> = HashMap::new();
    for (i, mem) in results.iter().enumerate() {
        id_to_label.insert(mem.memory_id, format!("{}", i + 1));
    }

    const MAX_NEIGHBORS: usize = 10;
    const FULL_TEXT_NEIGHBORS: usize = 5;

    let mut neighbor_weights: HashMap<MemoryId, f32> = HashMap::new();
    let mut relations: Vec<MemoryRelation> = Vec::new();
    let sorted_neighbors: Vec<(MemoryId, f32)>;

    {
        let graph_r = harness.system.graph().read().await;

        // Collect neighbors and their max edge weight from result memories.
        for mem in results {
            let Some(src_key) = graph_r.resolve(&mem.memory_id) else {
                continue;
            };
            for edge in graph_r.edges_for(&mem.memory_id) {
                let other_key = if edge.source == src_key {
                    edge.target
                } else {
                    edge.source
                };
                if let Some(neighbor_node) = graph_r.get_node_by_key(other_key) {
                    if !result_ids.contains(&neighbor_node.memory_id) {
                        neighbor_weights
                            .entry(neighbor_node.memory_id)
                            .and_modify(|w| *w = w.max(edge.weight))
                            .or_insert(edge.weight);
                    }
                }
            }
        }

        // Sort by weight descending and cap at MAX_NEIGHBORS.
        let mut sorted: Vec<(MemoryId, f32)> = neighbor_weights.into_iter().collect();
        sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        sorted.truncate(MAX_NEIGHBORS);

        let neighbor_ids: Vec<MemoryId> = sorted.iter().map(|(id, _)| *id).collect();

        // Assign labels for neighbors.
        for (i, &mid) in neighbor_ids.iter().enumerate() {
            id_to_label.insert(mid, format!("R{}", i + 1));
        }

        // Now collect edges between all known memories (results + neighbors).
        let all_ids: Vec<MemoryId> = results
            .iter()
            .map(|m| m.memory_id)
            .chain(neighbor_ids.iter().copied())
            .collect();

        for &mid in &all_ids {
            let Some(src_key) = graph_r.resolve(&mid) else {
                continue;
            };
            for edge in graph_r.edges_for(&mid) {
                let other_key = if edge.source == src_key {
                    edge.target
                } else {
                    edge.source
                };
                if let Some(other_node) = graph_r.get_node_by_key(other_key) {
                    let from_label = id_to_label.get(&mid);
                    let to_label = id_to_label.get(&other_node.memory_id);
                    if let (Some(from), Some(to)) = (from_label, to_label) {
                        if from < to {
                            let edge_type = format!("{:?}", edge.edge_type).to_lowercase();
                            relations.push(MemoryRelation {
                                from_label: from.clone(),
                                to_label: to.clone(),
                                edge_type,
                            });
                        }
                    }
                }
            }
        }

        sorted_neighbors = sorted;
    }

    // Load neighbor summaries + full_text (for top-5) + created_at.
    let top_full_text_ids: HashSet<MemoryId> = sorted_neighbors
        .iter()
        .take(FULL_TEXT_NEIGHBORS)
        .map(|(id, _)| *id)
        .collect();

    let neighbor_ids: Vec<MemoryId> = sorted_neighbors.iter().map(|(id, _)| *id).collect();

    let mut neighbors: Vec<(MemoryId, String, Option<String>, i64)> =
        Vec::with_capacity(neighbor_ids.len());
    let cache = harness.system.cache();

    // First pass: resolve from cache (summary only).
    let mut need_storage: Vec<MemoryId> = Vec::new();
    for &mid in &neighbor_ids {
        if let Some(cached) = cache.get(mid).await {
            if top_full_text_ids.contains(&mid) {
                need_storage.push(mid);
            } else {
                neighbors.push((mid, cached.summary.clone(), None, cached.created_at));
            }
        } else {
            need_storage.push(mid);
        }
    }

    // Second pass: resolve from storage (summary + optional full_text).
    if !need_storage.is_empty() {
        if let Ok(mut storage_w) = harness.system.storage().write() {
            for mid in need_storage {
                if let Ok(Some(disk)) = storage_w.get_record(mid) {
                    if !disk.summary.is_empty() {
                        let full_text = if top_full_text_ids.contains(&mid) && disk.text_length > 0
                        {
                            let text_ref = crate::storage::TextRef {
                                file_offset: disk.text_offset,
                                length: disk.text_length,
                            };
                            storage_w.get_text(text_ref).ok().flatten()
                        } else {
                            None
                        };
                        neighbors.push((mid, disk.summary.clone(), full_text, disk.created_at));
                    }
                }
            }
        }
    }

    // Deduplicate relations.
    let mut seen_edges: HashSet<(String, String)> = HashSet::new();
    relations.retain(|r| seen_edges.insert((r.from_label.clone(), r.to_label.clone())));

    GraphContext {
        neighbors,
        relations,
    }
}

// ── Retrieval diagnostics ────────────────────────────────────────

fn extract_key_terms(text: &str) -> Vec<String> {
    let stop_words: std::collections::HashSet<&str> = [
        "a", "an", "the", "is", "are", "was", "were", "be", "been", "being", "have", "has", "had",
        "do", "does", "did", "will", "would", "could", "should", "may", "might", "shall", "can",
        "need", "dare", "ought", "used", "to", "of", "in", "for", "on", "with", "at", "by", "from",
        "as", "into", "through", "during", "before", "after", "above", "below", "between", "out",
        "off", "over", "under", "again", "further", "then", "once", "here", "there", "when",
        "where", "why", "how", "all", "both", "each", "few", "more", "most", "other", "some",
        "such", "no", "nor", "not", "only", "own", "same", "so", "than", "too", "very", "just",
        "don", "t", "s", "and", "but", "or", "if", "while", "that", "this", "it", "its", "he",
        "she", "they", "them", "his", "her", "their", "what", "which", "who", "whom", "these",
        "those", "am", "about", "up", "down", "also", "like", "yes", "no", "because", "any",
    ]
    .into_iter()
    .collect();

    let stemmer = Stemmer::create(Algorithm::English);
    let mut seen = std::collections::HashSet::new();
    text.split(|c: char| !c.is_alphanumeric() && c != '\'')
        .map(|w| w.to_lowercase())
        .filter(|w| w.len() > 2 && !stop_words.contains(w.as_str()))
        .map(|w| stemmer.stem(&w).into_owned())
        .filter(|w| seen.insert(w.clone()))
        .collect()
}

fn synonym_variants(stemmed: &str) -> Vec<&'static str> {
    match stemmed {
        "children" | "child" | "kid" => vec!["children", "child", "kid"],
        "folk" | "individu" | "person" | "peopl" => vec!["folk", "individu", "person", "peopl"],
        "speech" | "talk" | "present" => vec!["speech", "talk", "present"],
        "counsel" | "counselor" | "therapi" | "therapist" => {
            vec!["counsel", "counselor", "therapi", "therapist"]
        }
        "mentor" | "mentorship" => vec!["mentor", "mentorship"],
        "outdoor" | "natur" | "hike" | "camp" => vec!["outdoor", "natur", "hike", "camp"],
        "happi" | "joy" | "glad" => vec!["happi", "joy", "glad"],
        "grate" | "thank" | "appreci" => vec!["grate", "thank", "appreci"],
        "scare" | "frighten" | "afraid" => vec!["scare", "frighten", "afraid"],
        "strong" | "resili" | "tough" => vec!["strong", "resili", "tough"],
        "bad" | "badli" | "poorli" | "terribl" => vec!["bad", "badli", "poorli", "terribl"],
        "mom" | "mother" | "mama" => vec!["mom", "mother", "mama"],
        "dad" | "father" | "papa" => vec!["dad", "father", "papa"],
        "amaz" | "wonder" | "incredibl" => vec!["amaz", "wonder", "incredibl"],
        "world" | "everyth" | "mean" => vec!["world", "everyth"],
        "invit" | "welcom" | "love" => vec!["invit", "welcom", "love"],
        "grow" | "growth" | "develop" => vec!["grow", "growth", "develop"],
        "thought" | "consider" | "care" => vec!["thought", "consider", "care"],
        "authent" | "genuin" | "true" => vec!["authent", "genuin", "true"],
        "driven" | "motiv" | "passion" | "drive" => {
            vec!["driven", "motiv", "passion", "drive"]
        }
        "walk" | "stroll" | "wander" => vec!["walk", "stroll", "wander"],
        _ => vec![],
    }
}

fn retrieval_hit_score(gold_answer: &str, retrieved_texts: &[&str]) -> (f64, usize, usize) {
    let key_terms = extract_key_terms(gold_answer);
    if key_terms.is_empty() {
        return (1.0, 0, 0);
    }

    // Stem the concatenated retrieved text so stemmed key terms can match.
    let stemmer = Stemmer::create(Algorithm::English);
    let raw_concat = retrieved_texts.join(" ").to_lowercase();
    let concat: String = raw_concat
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|w| !w.is_empty())
        .map(|w| stemmer.stem(w).into_owned())
        .collect::<Vec<_>>()
        .join(" ");

    let mut found = 0usize;
    for term in &key_terms {
        let variants = synonym_variants(term);
        if !variants.is_empty() {
            if variants.iter().any(|v| concat.contains(v)) {
                found += 1;
            }
        } else if concat.contains(term.as_str()) {
            found += 1;
        }
    }

    let total = key_terms.len();
    (found as f64 / total as f64, found, total)
}

#[derive(Default)]
struct DiagStats {
    total: usize,
    retrieval_hits: usize,
    partial_hits: usize,
    misses: usize,
    avg_hit_rate: f64,
    examples_miss: Vec<(String, String, Vec<String>)>,
}

/// Per-question diagnostic record written to the JSONL output file.
#[derive(Serialize)]
struct DiagRecord {
    conversation: String,
    category: String,
    question: String,
    gold_answer: String,
    hit_rate: f64,
    terms_found: usize,
    terms_total: usize,
    classification: String,
    top_retrieved: Vec<DiagRetrieved>,
}

#[derive(Serialize)]
struct DiagRetrieved {
    score: f32,
    text: String,
}

pub async fn run_diagnose(
    config: &RecalldConfig,
    data_path: &Path,
    top_k: usize,
    skip_adversarial: bool,
    llm: &LlmClient,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;

    let out_path = std::path::PathBuf::from("diag_results.jsonl");
    let mut out_file = std::fs::File::create(&out_path)?;

    eprintln!("  Loading dataset from {}...", data_path.display());
    eprintln!("  Writing results to {}", out_path.display());
    let conversations = parse_dataset(data_path)?;

    let total_qa: usize = conversations
        .iter()
        .map(|c| {
            if skip_adversarial {
                c.qa_pairs.iter().filter(|q| !q.is_adversarial).count()
            } else {
                c.qa_pairs.len()
            }
        })
        .sum();
    eprintln!(
        "  {} conversations, {} QA pairs{}, top-k: {}",
        conversations.len(),
        total_qa,
        if skip_adversarial {
            " (adversarial skipped)"
        } else {
            ""
        },
        top_k,
    );
    eprintln!("  Mode: retrieval diagnostics (conversation turn ingest)\n",);

    let mut by_category: HashMap<String, DiagStats> = HashMap::new();
    let mut overall = DiagStats::default();

    for (conv_idx, conv) in conversations.iter().enumerate() {
        eprintln!(
            "  Conversation {}/{} ({}, {} turns)...",
            conv_idx + 1,
            conversations.len(),
            conv.id,
            conv.turns.len(),
        );

        let harness = BenchHarness::new(config).await?;

        let diag_label = format!("[{}/{}] {}", conv_idx + 1, conversations.len(), conv.id);
        eprintln!("    Ingesting...");
        let t0 = Instant::now();
        let stored = ingest_conversation(&harness, conv, llm, &diag_label).await?;
        eprintln!(
            " done ({} memories, {:.1}s)",
            stored,
            t0.elapsed().as_secs_f64()
        );

        let active_qa: Vec<_> = if skip_adversarial {
            conv.qa_pairs.iter().filter(|q| !q.is_adversarial).collect()
        } else {
            conv.qa_pairs.iter().collect()
        };

        for (qa_idx, qa) in active_qa.iter().enumerate() {
            let cat = category_name(qa.category).to_string();
            let memories = search_memories(
                &harness,
                &qa.question,
                top_k,
                Some(llm),
                &mut std::io::sink(),
            )
            .await?;
            let combined_texts: Vec<String> = memories
                .iter()
                .map(|m| match &m.full_text {
                    Some(ft) => format!("{} {}", m.text, ft),
                    None => m.text.clone(),
                })
                .collect();
            let text_refs: Vec<&str> = combined_texts.iter().map(|s| s.as_str()).collect();

            let (hit_rate, found, total_terms) = retrieval_hit_score(&qa.gold_answer, &text_refs);

            let classification = if hit_rate >= 0.8 {
                "hit"
            } else if hit_rate >= 0.3 {
                "partial"
            } else {
                "miss"
            };

            // Write per-question record to JSONL immediately.
            let record = DiagRecord {
                conversation: conv.id.clone(),
                category: cat.clone(),
                question: qa.question.clone(),
                gold_answer: qa.gold_answer.clone(),
                hit_rate,
                terms_found: found,
                terms_total: total_terms,
                classification: classification.to_string(),
                top_retrieved: memories
                    .iter()
                    .take(20)
                    .map(|m| DiagRetrieved {
                        score: m.score,
                        text: m.text.clone(),
                    })
                    .collect(),
            };
            let _ = writeln!(
                out_file,
                "{}",
                serde_json::to_string(&record).unwrap_or_default()
            );
            let _ = out_file.flush();

            let stats = by_category.entry(cat).or_default();
            stats.total += 1;
            stats.avg_hit_rate += hit_rate;
            overall.total += 1;
            overall.avg_hit_rate += hit_rate;

            match classification {
                "hit" => {
                    stats.retrieval_hits += 1;
                    overall.retrieval_hits += 1;
                }
                "partial" => {
                    stats.partial_hits += 1;
                    overall.partial_hits += 1;
                }
                _ => {
                    stats.misses += 1;
                    overall.misses += 1;
                    if stats.examples_miss.len() < 3 {
                        let top3: Vec<String> = memories
                            .iter()
                            .take(3)
                            .map(|m| {
                                format!("  [{:.3}] {}", m.score, &m.text[..m.text.len().min(80)])
                            })
                            .collect();
                        stats.examples_miss.push((
                            qa.question.clone(),
                            format!("{} ({}/{})", qa.gold_answer, found, total_terms),
                            top3,
                        ));
                    }
                }
            }

            if (qa_idx + 1) % 20 == 0 || qa_idx + 1 == active_qa.len() {
                eprint!("\r    QA {}/{}    ", qa_idx + 1, active_qa.len(),);
            }
        }
        eprintln!();
    }

    // Build summary report.
    let mut report = String::new();
    report.push_str(&format!(
        "\n=== Retrieval Diagnostics (top-k: {}) ===\n\n",
        top_k
    ));

    let category_order = ["single-hop", "multi-hop", "temporal", "open-domain"];
    let mut table = Table::new();
    table.set_header(vec![
        "Category",
        "Count",
        "Hit (>=80%)",
        "Partial",
        "Miss (<30%)",
        "Avg Term Coverage",
    ]);

    for cat_name in &category_order {
        if let Some(stats) = by_category.get(*cat_name) {
            let avg = if stats.total > 0 {
                stats.avg_hit_rate / stats.total as f64 * 100.0
            } else {
                0.0
            };
            table.add_row(vec![
                Cell::new(cat_name),
                Cell::new(stats.total),
                Cell::new(format!(
                    "{} ({:.0}%)",
                    stats.retrieval_hits,
                    stats.retrieval_hits as f64 / stats.total as f64 * 100.0
                )),
                Cell::new(format!(
                    "{} ({:.0}%)",
                    stats.partial_hits,
                    stats.partial_hits as f64 / stats.total as f64 * 100.0
                )),
                Cell::new(format!(
                    "{} ({:.0}%)",
                    stats.misses,
                    stats.misses as f64 / stats.total as f64 * 100.0
                )),
                Cell::new(format!("{:.1}%", avg)),
            ]);
        }
    }

    let overall_avg = if overall.total > 0 {
        overall.avg_hit_rate / overall.total as f64 * 100.0
    } else {
        0.0
    };
    table.add_row(vec![
        Cell::new("OVERALL"),
        Cell::new(overall.total),
        Cell::new(format!(
            "{} ({:.0}%)",
            overall.retrieval_hits,
            overall.retrieval_hits as f64 / overall.total.max(1) as f64 * 100.0
        )),
        Cell::new(format!(
            "{} ({:.0}%)",
            overall.partial_hits,
            overall.partial_hits as f64 / overall.total.max(1) as f64 * 100.0
        )),
        Cell::new(format!(
            "{} ({:.0}%)",
            overall.misses,
            overall.misses as f64 / overall.total.max(1) as f64 * 100.0
        )),
        Cell::new(format!("{:.1}%", overall_avg)),
    ]);

    report.push_str(&format!("{table}\n\n"));

    report.push_str("  Note: open-domain hit rates are expected to be low. Gold answers for\n");
    report.push_str(
        "  open-domain questions are inferred conclusions (e.g., \"Liberal\", \"Likely\n",
    );
    report
        .push_str("  no\") that will not appear verbatim in retrieved memories. For open-domain\n");
    report.push_str("  accuracy, use the full `bench run` pipeline with LLM judging.\n\n");

    for cat_name in &category_order {
        if let Some(stats) = by_category.get(*cat_name) {
            if !stats.examples_miss.is_empty() {
                report.push_str(&format!("  {} misses (up to 3 examples):\n", cat_name));
                for (q, a, top3) in &stats.examples_miss {
                    report.push_str(&format!("    Q: {}\n", q));
                    report.push_str(&format!("    A: {}\n", a));
                    report.push_str("    Top retrieved:\n");
                    for line in top3 {
                        report.push_str(&format!("      {}\n", line));
                    }
                    report.push('\n');
                }
            }
        }
    }

    // Write summary to both stdout and file.
    print!("{report}");
    let _ = writeln!(out_file, "---");
    let _ = write!(out_file, "{report}");

    eprintln!("  Results written to {}", out_path.display());

    Ok(())
}

// ── Report formatting ─────────────────────────────────────────────

fn format_report(report: &LocomoReport) -> String {
    let mut out = String::new();

    let accuracy = if report.total_qa > 0 {
        report.total_correct as f64 / report.total_qa as f64 * 100.0
    } else {
        0.0
    };

    let mode_label = if report.stress_test {
        " [STRESS TEST]"
    } else {
        ""
    };
    out.push_str(&format!(
        "\n=== LoCoMo Benchmark ({} conversations, {} QA pairs){} ===\n",
        report.total_conversations, report.total_qa, mode_label,
    ));
    out.push_str(&format!(
        "  Model: {}    Ingest: {}    Judge: {}    Top-k: {}    Turns: {}\n\n",
        report.model, report.ingest_model, report.judge_model, report.top_k, report.total_turns,
    ));

    out.push_str(&format!(
        "  Overall accuracy:  {} ({}/{})\n\n",
        format!("{:.1}%", accuracy).bold(),
        report.total_correct,
        report.total_qa,
    ));

    // Category breakdown.
    let category_order = [
        "single-hop",
        "multi-hop",
        "temporal",
        "open-domain",
        "adversarial",
    ];
    let mut table = Table::new();
    table.set_header(vec!["Category", "Count", "Correct", "Accuracy"]);

    for cat in &category_order {
        if let Some(stats) = report.categories.get(*cat) {
            let acc = if stats.total > 0 {
                stats.correct as f64 / stats.total as f64 * 100.0
            } else {
                0.0
            };
            table.add_row(vec![
                Cell::new(cat),
                Cell::new(stats.total),
                Cell::new(stats.correct),
                Cell::new(format!("{:.1}%", acc)),
            ]);
        }
    }

    out.push_str(&format!("{}\n\n", table));

    out.push_str(&format!(
        "  Latency (avg):  retrieval: {:.0}ms    answer: {:.0}ms    judge: {:.0}ms\n",
        report.avg_retrieval_ms, report.avg_answer_ms, report.avg_judge_ms,
    ));

    out
}
