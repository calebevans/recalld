# Benchmark Methodology

This document describes the evaluation methodology used to benchmark recalld
against the LoCoMo dataset. It is intended to be precise, transparent, and
reproducible.

## 1. Dataset

**LoCoMo** (Long-term Conversational Memory) is a benchmark for evaluating
long-term conversational memory in LLM agents, introduced by Maharana et al.
at ACL 2024.

> Maharana, A., Lee, D. H., Tulyakov, S., Bansal, M., Barbieri, F., & Fang, Y.
> (2024). Evaluating Very Long-Term Conversational Memory of LLM Agents.
> *Proceedings of the 62nd Annual Meeting of the Association for Computational
> Linguistics (ACL 2024)*, 13820--13837.
> [ACL Anthology](https://aclanthology.org/2024.acl-long.747/) |
> [arXiv:2402.17753](https://arxiv.org/abs/2402.17753)

The full dataset contains 50 conversations and 7,512 QA pairs. The publicly
released evaluation subset contains **10 conversations** and **1,986 questions**
across 5 categories.

### Category breakdown

| # | Category    | Questions | % of total |
|---|-------------|-----------|------------|
| 1 | Single-hop  | 282       | 14.2%      |
| 2 | Temporal    | 321       | 16.2%      |
| 3 | Multi-hop   | 96        | 4.8%       |
| 4 | Open-domain | 841       | 42.3%      |
| 5 | Adversarial | 446       | 22.5%      |
|   | **Total**   | **1,986** |            |
|   | **Non-adversarial (1--4)** | **1,540** | |

Each conversation averages roughly 600 turns across up to 32 sessions.
Conversations were generated via a machine-human pipeline grounded in personas
and temporal event graphs, then verified by human annotators.

### Adversarial questions

Category 5 questions ask about events that never occurred in the conversation.
The correct response is a refusal ("I don't know" or equivalent). These are
**included** in recalld's benchmark results. An optional `--skip-adversarial`
flag exists for comparison purposes.

Adversarial questions are included in all recalld benchmark results. The
standard industry convention is to exclude them. The `--skip-adversarial` flag
is available for comparison purposes.

## 2. Evaluation Pipeline

The benchmark runs a 4-stage pipeline:

```
Ingestion --> Retrieval --> Answer Generation --> Judging
```

Each conversation gets a fresh recalld instance in a temporary directory with
memory decay disabled. This means every benchmark run starts from zero stored
memories -- there is no pre-loaded knowledge.

### 2.1 Ingestion

| Parameter     | Value |
|---------------|-------|
| Default model | `gemini-2.5-flash` (configurable via `--ingest-model`) |
| Temperature   | 0.0 |
| max_tokens    | 1024 |
| Context window | Last 20 conversation turns + last 30 stored memories |

Each conversation turn is processed sequentially. For each turn, the ingestion
LLM receives the recent conversation context and recently stored memories, then
outputs zero or more structured memories to store.

**What is stored per memory:**
- `summary` (required): 1--2 sentence concise summary
- `full_text` (optional): detailed version with context, quotes, and specifics
- `entities` (required): people, places, proper nouns
- `topics` (required): 1--5 topic keywords
- `emotions` (optional): emotional tone
- `supersedes` (optional): ID of a memory this one replaces

**Storage pipeline per memory:**
1. Generate embedding from concatenation of summary + full_text + tags
2. Insert into storage engine (SQLite-backed)
3. Add to SIMD vector index
4. Add to FTS5 full-text search index
5. Add as a node in the memory graph
6. If `supersedes` is set, add a Supersedes edge
7. Run automatic graph linking (similarity, entity, temporal)

**Graph linking** (three types, all automatic):
- **Similarity-based**: vector cosine similarity above threshold (default 0.50),
  up to 15 links per memory
- **Entity-based**: links memories sharing the same named entities, up to 10
  links per memory
- **Temporal**: links memories within a time window (default 1 hour), up to 20
  links per memory

The memory **storage pipeline** (embedding, indexing, graph linking) is the
**same as production**. The LLM-based memory extraction from conversation turns
(`process_turn`) is benchmark-specific -- in production, the calling LLM client
directly invokes `store_memory` with already-decided content.

### 2.2 Retrieval

| Parameter       | Value |
|-----------------|-------|
| Default model   | `gemini-2.5-flash` (same instance as answer generation) |
| Temperature     | 0.0 |
| max_tokens      | 1024 |
| Default top_k   | 15 (configurable via `--top-k`) |
| Default graph depth | 2 (LLM can set 0--3 per query) |

Retrieval uses an LLM call to decompose the question into optimized search
parameters before searching. The `construct_search_query` method takes the
question text and returns:

- 1--3 semantic search queries (each with optional FTS keyword query)
- Entity, topic, and emotion filters
- Graph traversal depth (0--3, capped at 3)
- Optional time range bounds

If the LLM call fails, the system falls back to a single raw-text query with
depth 1.

**Search execution per query:**
1. SIMD vector similarity search + FTS5 BM25 keyword search, combined via
   `EmbeddingPlusMetadata` query mode
2. Per-query result limit: `top_k/2 + 5` for multi-query, `top_k` for single
   query
3. Results across queries merged by memory ID, keeping the highest score
4. Final results sorted by score descending and truncated to `top_k`

**Post-retrieval graph expansion:**
After the search pipeline returns results, a 1-hop graph traversal collects up
to 10 neighbor memories (sorted by edge weight descending). The top 5 neighbors
include their full_text. All edge relationships between result memories and
neighbor memories are collected and shown to the answer model.

### 2.3 Answer Generation

| Parameter     | Value |
|---------------|-------|
| Default model | `gemini-2.5-flash` (configurable via `--model`) |
| Temperature   | 0.0 |
| max_tokens    | 1024 |

A **single unified prompt** is used for all question categories. The `category`
parameter is passed as the literal string `"unknown"` and is never used in the
prompt. There are no category-specific instructions, examples, or routing.

The system prompt instructs the model to:
- Base answers only on provided memories
- Reason step by step (identify evidence, chain multi-hop pieces, attend to dates)
- Verify person references before using a memory as evidence
- Use information even when phrasing differs from the question
- Say "I don't know" if insufficient information
- Keep the final answer concise (1--2 sentences after reasoning)

**Context provided to the model:**
- Retrieved memories (up to `top_k`) with: relevance score, creation date,
  summary text, full_text (if present), entity/topic/emotion metadata, and
  graph link annotations
- Graph neighbor memories (up to 10) with: summary, full_text (for top 5),
  creation date, and edge relationship annotations

The gold answer is **never visible** to the answer model.

### 2.4 Judging

| Parameter     | Value |
|---------------|-------|
| Default model | `gemini-2.5-flash-lite` (configurable via `--judge-model`) |
| Temperature   | 0.0 |
| max_tokens    | 1024 |
| Scoring       | Binary: CORRECT or WRONG |

The judge model is deliberately different from the answer model to avoid
self-grading bias.

**Non-adversarial judging** (categories 1--4): The prompt instructs the judge to
be lenient -- synonyms, paraphrases, additional correct details, equivalent date
formats, and same-conclusion answers are all accepted as CORRECT. The example
in the prompt (Hawaii / shell necklace) is not from the LoCoMo dataset.

**Adversarial judging** (category 5): Inverted scoring. CORRECT if the model
refuses to answer or says "I don't know." WRONG if it provides any specific
answer as if true. The gold answer field contains the *wrong* answer a tricked
system would give; it is shown to the judge as "Expected wrong answer."

**Fallback parsing**: If the judge's JSON output fails to parse, the system
checks whether the cleaned, uppercased response (with markdown code fences
already stripped) contains the quoted string `"CORRECT"` (with surrounding
double quotes) and does not contain the quoted string `"WRONG"`.

## 3. Methodology Notes

### 3.1 Comparison to Other Systems

Direct numeric comparisons across memory systems should be interpreted with
caution. There is **no standardized LoCoMo evaluation protocol**. Each system
uses different:

- Ingestion models and prompts
- Answer generation models and prompts
- Judge models and prompts
- Retrieval configurations and architectures
- Scoring criteria and leniency thresholds

The Mem0/Zep dispute (documented in
[zep-papers#5](https://github.com/getzep/zep-papers/issues/5)) demonstrated that
methodology differences can swing reported scores by over 25 percentage points
on the same dataset. Zep's initially reported 84% was corrected to 58.44% after
fixing category inclusion errors and prompt normalization.

**Model configuration:**

| Component   | recalld (default)     | Mem0 (published, April 2026) |
|-------------|----------------------|------|
| Ingestion   | `gemini-2.5-flash`   | Not disclosed for published score; open-source default is `gpt-4o-mini` |
| Answer gen  | `gemini-2.5-flash`   | Not disclosed; open-source default is `gpt-4o` |
| Judge       | `gemini-2.5-flash-lite` | Not disclosed; open-source default is `gpt-4o` |
| Embeddings  | `embeddinggemma:latest` (768d, via Ollama) | `text-embedding-3-small` (1536d) |

Mem0's published 92.5% score does not disclose which exact models were used.
Their open-source evaluation framework notes "using a frontier model will likely
produce higher scores."

### 3.2 Prompt Design

All prompts (ingestion, answer generation, search query construction, judging)
use generic examples and instructions. No LoCoMo-specific content, question
formats, or dataset knowledge is embedded in any prompt.

The answer generation prompt describes "long personal conversations between two
people who are friends" -- this matches the LoCoMo format but is not
LoCoMo-specific; it describes the domain recalld operates in.

The **original LoCoMo paper** used category-specific prompts for evaluation.
recalld deliberately uses a single unified prompt, following the emerging
industry practice advocated by LoCoMo-Plus (arXiv:2602.10715), which proposes a
"unified-input, differentiated-judgment" paradigm.

### 3.3 LLM-Augmented Retrieval

recalld uses an additional LLM call per question to decompose it into optimized
search parameters (multiple semantic queries, FTS queries, entity/topic filters,
graph depth, time range).

**Why this is not a benchmark artifact.** recalld is designed as an MCP tool for
AI agents. In production, the calling agent (e.g., Claude Code) already reasons
about what to search for before invoking `recall_memories` — it decides the
query text, which entities to filter on, what graph depth to use, and whether to
set a time range. The `construct_search_query()` call in the benchmark simulates
this agent-side reasoning. The MCP tool schema exposes these parameters
precisely because they are meant to be driven by an LLM. Removing this step
would benchmark a usage pattern that does not reflect how the tool is actually
used.

**Production cost implications.** In the benchmark, `construct_search_query()`
appears as a separate LLM call. In production, this work is done by the calling
agent as part of normal tool use — when Claude Code decides to call
`recall_memories`, it already reasons about what query to send, which entities to
filter on, and what depth to use. That reasoning is part of the agent's existing
turn, not an additional API call. The benchmark isolates this step into a
dedicated call to simulate agent-side reasoning in a controlled way, but it does
not represent an extra cost that production users pay beyond what their agent
would spend anyway.

**Architectural note.** In the benchmark, query construction appears as a
separate LLM call. In production, the calling agent already reasons about what
to search for as part of its tool use — it decides the query text, filters,
and depth before invoking `recall_memories`. The benchmark isolates this step
to simulate that agent-side reasoning in a controlled environment.

### 3.4 Statistical Considerations

**Confidence intervals.** Wilson 95% confidence intervals should be reported
alongside point estimates. For reference, at n = 1,986 (all questions):

| Observed accuracy | 95% CI (Wilson) |
|-------------------|-----------------|
| 80%               | [77.9%, 81.9%]  |
| 85%               | [83.1%, 86.7%]  |
| 90%               | [88.4%, 91.4%]  |
| 92.5%             | [91.1%, 93.7%]  |

**Clustering.** The 1,986 questions are clustered within 10 conversations.
Questions within a conversation share ingested context. The effective sample
size for between-conversation variation is 10, not 1,986.

**LLM judge noise.** Binary decisions from LLM judges have approximately
10--20% disagreement with human annotators. The LoCoMo-Plus paper reports
human-LLM judge agreement of 0.80--0.82, compared to 0.90 inter-annotator
agreement. Mem0's documentation states "+/- 1 point confidence interval due to
judge inconsistency." Different judge prompts have been observed to produce
~10% scoring differences on the same answers
([ByteRover](https://www.byterover.dev/blog/benchmark-ai-agent-memory)).

**Determinism.** All LLM calls use temperature 0.0. Results are deterministic
within a model version but not across model updates. LLM outputs at temperature
0 can still vary slightly across API calls due to batching and quantization.

### 3.5 Known Limitations

**Ground truth errors.** An independent audit by Penfield Labs identified 99
score-corrupting errors across the 1,540 non-adversarial questions (6.4% error
rate), including hallucinated facts in the answer key, incorrect temporal
reasoning, and speaker attribution errors. The audit did not cover the 446
adversarial questions (category 5). Assuming adversarial ground truth is
reliable (the expected answer is simply a refusal), the 99 errors across the
full 1,986 questions yield a **5.0% error rate** and a **practical accuracy
ceiling around 95%**. For reference, Northcutt et al. (NeurIPS 2021) found an
average 3.3% label error rate across 10 major ML benchmarks.

> Penfield Labs. "We Audited LoCoMo: 6.4% of the Answer Key Is Wrong, and the
> Judge Accepts Up to 63% of Intentionally Wrong Answers."
> [penfieldlabs.substack.com](https://penfieldlabs.substack.com/p/we-audited-locomo-64-of-the-answer)

**LLM judge vulnerability.** When tested with `gpt-4o-mini` as judge against
intentionally wrong but topically related answers, 62.81% of wrong answers were
accepted overall. Specific factual errors (wrong names/dates) were caught ~89%
of the time, but vague, topically adjacent answers missing specific details
passed ~63% of the time (Penfield Labs, ibid.).

**Ingestion context limits.** The ingestion window is limited to the last 20
conversation turns and last 30 stored memories. For very long conversations,
early context may be unavailable to the ingestion model when processing later
turns.

**max_tokens truncation.** All LLM calls use `max_tokens = 1024`. Long answers
or complex multi-hop reasoning chains may be truncated.

**Model version dependency.** Results depend on specific model versions
(`gemini-2.5-flash`, `gemini-2.5-flash-lite`) which are not version-pinned beyond
their model identifiers. Model updates from providers may change results.

**Open-domain category.** 841 of 1,986 questions (42.3%) are open-domain,
requiring inference from facts rather than direct retrieval. Term-overlap-based
retrieval diagnostics are misleading for this category.

**Conversation diversity.** 10 conversations may not generalize to all
conversational domains, relationship types, or cultural contexts.

**Embedding dilution.** Embeddings are generated from the concatenation of
summary + full_text + tags. Long full_text fields may dilute the semantic signal
from the summary.

## 4. Reproducibility

### Requirements

- Rust toolchain (see `rust-toolchain.toml`)
- One of the following LLM backends:
  - **Gemini on Vertex AI** (default): `GEMINI_VERTEX_PROJECT_ID` + gcloud auth.
    Optional: `GEMINI_VERTEX_REGION` (default: `us-central1`)
  - **Claude on Vertex AI**: `CLAUDE_CODE_USE_VERTEX=1` + `CLOUD_ML_REGION` +
    `ANTHROPIC_VERTEX_PROJECT_ID`
  - **Anthropic API**: `ANTHROPIC_API_KEY`
  - **OpenAI-compatible**: `--llm-url http://host:port`

### Command

```bash
cargo run --features bench --release -- bench locomo
```

### Configurable parameters

| Flag                | Default                       | Description |
|---------------------|-------------------------------|-------------|
| `--data`            | `src/bench/data/locomo10.json`| Path to dataset file |
| `--top-k`           | `15`                          | Retrieved memories per question |
| `--model`           | `gemini-2.5-flash`            | Answer generation and query construction model |
| `--ingest-model`    | `gemini-2.5-flash`            | Conversation ingestion model |
| `--judge-model`     | `gemini-2.5-flash-lite`       | Answer judging model |
| `--llm-url`         | *(none)*                      | OpenAI-compatible LLM server URL |
| `--parallel`        | `2`                           | Number of conversations to evaluate concurrently |
| `--qa-parallel`     | `4`                           | Number of QA pairs to evaluate concurrently per conversation |
| `--skip-adversarial`| `false`                       | Exclude category 5 questions |
| `--diagnose`        | `false`                       | Run retrieval diagnostics only (no LLM calls for QA) |
| `--format`          | `human`                       | Output format (`human` or `json`) |

### Data integrity

- Gold answers are never visible to the ingestion, retrieval, or answer
  generation stages
- The gold answer is only visible to the judge model
- The answer model and judge model are separate instances with different models
  by default
- Prompts contain no LoCoMo-specific examples or dataset knowledge
- Each conversation gets a fresh, empty recalld instance

## 5. Results

Results are stored in `benchmarks/<run-name>/` with full JSON output
(`bench_results.json`) and per-question debug logs (`bench_debug.log`).

All results include adversarial questions (all 5 categories, 1,986 total
questions).

### Run: Claude Sonnet 4 (unified prompt, top-k=15, all categories)

**Configuration:**
- Model (ingestion + answer gen): `claude-sonnet-4-6`
- Judge: `claude-haiku-4-5`
- Top-k: 15
- Prompt: unified (no category-specific instructions)
- Categories: all 5 (including adversarial)
- Results: `benchmarks/claude-sonnet-4-6-top15-unified-full/`

**Overall accuracy: 83.0%** (1,649/1,986)

Wilson 95% CI: [81.3%, 84.6%]

### Per-category breakdown

| Category    | Correct | Total | Accuracy |
|-------------|---------|-------|----------|
| Single-hop  | 225     | 282   | 79.8%    |
| Temporal    | 276     | 321   | 86.0%    |
| Multi-hop   | 71      | 96    | 74.0%    |
| Open-domain | 689     | 841   | 81.9%    |
| Adversarial | 388     | 446   | 87.0%    |
| **Overall (1--5)** | **1,649** | **1,986** | **83.0%** |

### Run: Claude Sonnet 4 (unified prompt, top-k=20, all categories)

**Configuration:**
- Model (ingestion + answer gen): `claude-sonnet-4-6`
- Judge: `claude-haiku-4-5`
- Top-k: 20
- Prompt: unified (no category-specific instructions)
- Categories: all 5 (including adversarial)
- Results: `benchmarks/claude-sonnet-4-6-top20-unified-full/`

**Overall accuracy: 83.1%** (1,650/1,986)

Wilson 95% CI: [81.4%, 84.7%]

| Category    | Correct | Total | Accuracy |
|-------------|---------|-------|----------|
| Single-hop  | 232     | 282   | 82.3%    |
| Temporal    | 274     | 321   | 85.4%    |
| Multi-hop   | 70      | 96    | 72.9%    |
| Open-domain | 688     | 841   | 81.8%    |
| Adversarial | 386     | 446   | 86.5%    |
| **Overall (1--5)** | **1,650** | **1,986** | **83.1%** |

### Run: Gemini 2.5 Flash (unified prompt, top-k=15, all categories)

**Configuration:**
- Model (all stages): `gemini-2.5-flash`
- Judge: `gemini-2.5-flash-lite`
- Top-k: 15
- Prompt: unified (no category-specific instructions)
- Categories: all 5 (including adversarial)
- Results: `benchmarks/gemini-2.5-flash-top15-unified-full/`

**Overall accuracy: 73.9%** (1,467/1,986)

Wilson 95% CI: [71.9%, 75.8%]

| Category    | Correct | Total | Accuracy |
|-------------|---------|-------|----------|
| Single-hop  | 191     | 282   | 67.7%    |
| Temporal    | 227     | 321   | 70.7%    |
| Multi-hop   | 41      | 96    | 42.7%    |
| Open-domain | 642     | 841   | 76.3%    |
| Adversarial | 366     | 446   | 82.1%    |
| **Overall (1--5)** | **1,467** | **1,986** | **73.9%** |

### Run: Gemini 2.5 Flash (unified prompt, top-k=20, all categories)

**Configuration:**
- Model (all stages): `gemini-2.5-flash`
- Judge: `gemini-2.5-flash-lite`
- Top-k: 20
- Prompt: unified (no category-specific instructions)
- Categories: all 5 (including adversarial)
- Results: `benchmarks/gemini-2.5-flash-top20-unified-full/`

**Overall accuracy: 74.4%** (1,477/1,986)

Wilson 95% CI: [72.4%, 76.3%]

| Category    | Correct | Total | Accuracy |
|-------------|---------|-------|----------|
| Single-hop  | 196     | 282   | 69.5%    |
| Temporal    | 220     | 321   | 68.5%    |
| Multi-hop   | 47      | 96    | 49.0%    |
| Open-domain | 651     | 841   | 77.4%    |
| Adversarial | 363     | 446   | 81.4%    |
| **Overall (1--5)** | **1,477** | **1,986** | **74.4%** |

## 6. Stress Test

The standard LoCoMo evaluation creates an isolated memory store per
conversation. In production, a memory system accumulates knowledge across
many conversations, projects, and contexts. The stress test evaluates
retrieval accuracy at scale by ingesting all 10 conversations into a
**single shared memory store**, then running all 1,986 questions against it.

This is a harder test: the retrieval pipeline must find the right memories
among thousands of candidates, many of which share similar entities, topics,
and phrasing from unrelated conversations.

### Run: Gemini 2.5 Flash stress test (top-k=15, all categories)

**Configuration:**
- Model (all stages): `gemini-2.5-flash`
- Judge: `gemini-2.5-flash-lite`
- Top-k: 15
- Total memories in store: 2,293 (from all 10 conversations)
- Categories: all 5 (including adversarial)
- Results: `benchmarks/gemini-2.5-flash-top15-stress-test/`

**Overall accuracy: 73.2%** (1,453/1,986)

| Category    | Correct | Total | Accuracy |
|-------------|---------|-------|----------|
| Single-hop  | 181     | 282   | 64.2%    |
| Temporal    | 219     | 321   | 68.2%    |
| Multi-hop   | 45      | 96    | 46.9%    |
| Open-domain | 641     | 841   | 76.2%    |
| Adversarial | 367     | 446   | 82.3%    |
| **Overall (1--5)** | **1,453** | **1,986** | **73.2%** |

### Accuracy retention

| Mode | Memories | Accuracy | Delta |
|------|----------|----------|-------|
| Isolated (per-conversation) | ~200--500 each | 73.9% | -- |
| Stress test (shared store) | 2,293 total | 73.2% | -0.7 pts |

With 5--10x more memories in the store, accuracy dropped by less than
1 percentage point. Retrieval precision is maintained at scale through
entity filtering, topic tags, and graph-based disambiguation.

## 7. Comparison

The following table is provided for context. Due to the methodological
differences described in Section 3.1, these numbers are **not directly
comparable**. Different models, prompts, judge configurations, and scoring
criteria were used by each system.

| System              | Reported accuracy | Categories | Scoring method | Notes |
|---------------------|-------------------|-----------|----------------|-------|
| recalld             | 83.0%             | 1--5 (all) | LLM-as-judge, binary | Claude Sonnet 4; unified prompt; top-k=15 |
| Mem0 (April 2026)   | 92.5%             | 1--4 only | LLM-as-judge   | Adversarial excluded |
| ByteRover 2.0       | 92.2%             | 1--4 only | LLM-as-judge   | Adversarial excluded |
| Human (original paper) | 87.9% F1       | 1--5 (all) | Token-level F1 | Different metric |
| Accuracy ceiling     | ~95%             | -- | --              | Limited by ground truth errors (Penfield Labs) |

**Note:** recalld evaluates on all 5 categories including adversarial (1,986
questions). The "Categories" column indicates which categories each system
includes in their reported score. Scores across different category sets are
not directly comparable.
