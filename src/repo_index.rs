//! Pre-computed per-profile repository file & symbol index stored in `state.sqlite`.
//!
//! Built during onboarding / re-onboarding (and lazily refreshed if `HEAD` moves)
//! so natural-language objectives resolve quickly against paths, defined symbols, and
//! source identifiers without external daemons or LLM tokens.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use anyhow::{Context, Result};
use rusqlite::{Connection, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::{
    now,
    onboard::{list_editable_repo_files, source_file_rank},
    open_sqlite_with_retry,
    orchestration::{MAX_ALLOWED_FILES, safe_file},
    repository::Repository,
    workflow::Profile,
};

const MAX_FILE_READ_BYTES: usize = 128 * 1024;
const MAX_INDEXED_FILES: usize = 2500;
const MAX_TERMS_PER_FILE: usize = 256;
const MAX_SYMBOLS_PER_FILE: usize = 128;

const STOPWORDS: &[&str] = &[
    "the",
    "and",
    "for",
    "with",
    "from",
    "this",
    "that",
    "into",
    "file",
    "files",
    "fix",
    "bug",
    "bugs",
    "error",
    "errors",
    "test",
    "tests",
    "add",
    "update",
    "change",
    "changes",
    "function",
    "method",
    "line",
    "lines",
    "code",
    "other",
    "only",
    "do",
    "not",
    "modify",
    "any",
    "inside",
    "when",
    "using",
    "should",
    "must",
    "need",
    "make",
    "both",
    "ensure",
    "passing",
    "pass",
    "fails",
    "failed",
    "currently",
    "instead",
    "without",
    "whenever",
    "already",
    "also",
    "then",
    "than",
    "have",
    "has",
    "are",
    "was",
    "were",
    "will",
    "can",
    "could",
    "all",
    "new",
    "old",
    "two",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SuggestedFile {
    pub path: String,
    pub score: f64,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SuggestFilesResponse {
    pub matched: Vec<SuggestedFile>,
    pub merged_scope: Vec<String>,
    pub indexed_files: usize,
    pub used_cached_index: bool,
}

#[derive(Debug, Clone)]
struct IndexedEntry {
    path: String,
    symbols: Vec<String>,
    path_tokens: Vec<String>,
    symbol_tokens: Vec<String>,
    body_terms: BTreeMap<String, u32>,
    doc_len: u32,
}

#[derive(Debug, Clone)]
pub struct IndexSummary {
    pub file_count: usize,
    pub base_commit: String,
}

fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS repo_index_meta (
            profile_name TEXT PRIMARY KEY,
            repo_checkout TEXT NOT NULL,
            base_commit TEXT NOT NULL,
            indexed_at INTEGER NOT NULL,
            file_count INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS repo_file_index (
            profile_name TEXT NOT NULL,
            path TEXT NOT NULL,
            symbols TEXT NOT NULL,
            path_tokens TEXT NOT NULL,
            symbol_tokens TEXT NOT NULL,
            body_terms TEXT NOT NULL,
            doc_len INTEGER NOT NULL,
            PRIMARY KEY (profile_name, path)
        );",
    )?;
    Ok(())
}

fn is_stopword(term: &str) -> bool {
    term.len() < 2 || STOPWORDS.contains(&term)
}

/// Splits camelCase, PascalCase, snake_case, kebab-case, and paths into lowercase tokens,
/// retaining intact compound `snake_case` tokens when at least 3 chars.
pub(crate) fn split_code_tokens(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();

    for chunk in raw.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
        let lower_chunk = chunk.trim_matches('_').to_ascii_lowercase();
        if lower_chunk.len() >= 3
            && chunk.contains('_')
            && !is_stopword(&lower_chunk)
            && seen.insert(lower_chunk.clone())
        {
            out.push(lower_chunk);
        }
        for snake_part in chunk.split('_') {
            if snake_part.is_empty() {
                continue;
            }
            let chars: Vec<char> = snake_part.chars().collect();
            let mut start = 0;
            for i in 1..chars.len() {
                let prev = chars[i - 1];
                let curr = chars[i];
                let next_lower = chars.get(i + 1).is_some_and(|c| c.is_ascii_lowercase());
                let split_here = (prev.is_ascii_lowercase() && curr.is_ascii_uppercase())
                    || (prev.is_ascii_uppercase() && curr.is_ascii_uppercase() && next_lower)
                    || (prev.is_ascii_alphabetic() && curr.is_ascii_digit())
                    || (prev.is_ascii_digit() && curr.is_ascii_alphabetic());
                if split_here {
                    let part: String = chars[start..i].iter().collect();
                    let norm = part.to_ascii_lowercase();
                    if !is_stopword(&norm) && seen.insert(norm.clone()) {
                        out.push(norm);
                    }
                    start = i;
                }
            }
            let tail: String = chars[start..].iter().collect();
            let norm = tail.to_ascii_lowercase();
            if !is_stopword(&norm) && seen.insert(norm.clone()) {
                out.push(norm);
            }
        }
    }
    out
}

fn clean_ident(raw: &str) -> Option<String> {
    let ident: String = raw
        .trim_start_matches(['*', '&'])
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '$')
        .collect();
    if ident.len() >= 2 && ident.chars().any(|c| c.is_ascii_alphabetic()) {
        Some(ident)
    } else {
        None
    }
}

/// Extracts top-level / module declaration symbol names from common Rust/JS/TS/Python/Go forms.
pub(crate) fn extract_symbols(text: &str) -> Vec<String> {
    const PREFIXES: &[&str] = &[
        "pub async fn ",
        "pub(crate) async fn ",
        "pub(super) async fn ",
        "async fn ",
        "pub fn ",
        "pub(crate) fn ",
        "pub(super) fn ",
        "fn ",
        "pub struct ",
        "pub(crate) struct ",
        "struct ",
        "pub enum ",
        "pub(crate) enum ",
        "enum ",
        "pub trait ",
        "trait ",
        "pub mod ",
        "mod ",
        "pub type ",
        "type ",
        "pub const ",
        "const ",
        "export async function ",
        "export function ",
        "async function ",
        "function ",
        "export class ",
        "class ",
        "export interface ",
        "interface ",
        "export type ",
        "export const ",
        "export enum ",
        "async def ",
        "def ",
        "func ",
    ];

    let mut symbols = Vec::new();
    let mut seen = BTreeSet::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('#') || trimmed.starts_with("/*") {
            continue;
        }
        for prefix in PREFIXES {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                let candidate = if *prefix == "func " && rest.starts_with('(') {
                    rest.split_once(')').map(|(_, after)| after.trim_start())
                } else {
                    Some(rest.trim_start())
                };
                if let Some(tok) = candidate.and_then(clean_ident)
                    && seen.insert(tok.clone())
                {
                    symbols.push(tok);
                    if symbols.len() >= MAX_SYMBOLS_PER_FILE {
                        return symbols;
                    }
                }
                break;
            }
        }
    }
    symbols
}

fn tokenize_body(text: &str) -> (BTreeMap<String, u32>, u32) {
    let mut counts: BTreeMap<String, u32> = BTreeMap::new();
    let mut doc_len: u32 = 0;

    for raw in text.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
        let trimmed = raw.trim_matches('_');
        if trimmed.len() < 2 {
            continue;
        }
        for tok in split_code_tokens(trimmed) {
            doc_len = doc_len.saturating_add(1);
            *counts.entry(tok).or_insert(0) += 1;
        }
    }

    if counts.len() > MAX_TERMS_PER_FILE {
        let mut ranked: Vec<(String, u32)> = counts.into_iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        ranked.truncate(MAX_TERMS_PER_FILE);
        counts = ranked.into_iter().collect();
    }

    (counts, doc_len)
}

fn read_lossy_bounded(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    let slice = &bytes[..bytes.len().min(MAX_FILE_READ_BYTES)];
    Some(String::from_utf8_lossy(slice).into_owned())
}

/// Scans `repo_checkout` and replaces the cached symbol & weighted token index for `profile_name`.
pub fn index_profile_repo(
    state_root: &Path,
    profile_name: &str,
    repo_checkout: &Path,
    base_commit: &str,
    excluded_files: &[String],
) -> Result<IndexSummary> {
    index_profile_repo_with_progress(
        state_root,
        profile_name,
        repo_checkout,
        base_commit,
        excluded_files,
        &mut |_, _| {},
    )
}

pub fn index_profile_repo_with_progress(
    state_root: &Path,
    profile_name: &str,
    repo_checkout: &Path,
    base_commit: &str,
    excluded_files: &[String],
    on_progress: &mut dyn FnMut(usize, usize),
) -> Result<IndexSummary> {
    let mut files = list_editable_repo_files(repo_checkout, excluded_files)?;
    files.truncate(MAX_INDEXED_FILES);
    let total = files.len();
    on_progress(0, total);

    let mut entries = Vec::with_capacity(total);
    for (idx, rel) in files.iter().enumerate() {
        let text = safe_file(repo_checkout, rel)
            .ok()
            .and_then(|p| read_lossy_bounded(&p))
            .unwrap_or_default();
        let symbols = extract_symbols(&text);
        let path_tokens = split_code_tokens(rel);
        let symbol_tokens = {
            let joined = symbols.join(" ");
            split_code_tokens(&joined)
        };
        let (body_terms, doc_len) = tokenize_body(&text);
        entries.push(IndexedEntry {
            path: rel.clone(),
            symbols,
            path_tokens,
            symbol_tokens,
            body_terms,
            doc_len,
        });
        let done = idx + 1;
        if done % 25 == 0 || done == total {
            on_progress(done, total);
        }
    }

    let mut conn = open_sqlite_with_retry(&state_root.join("state.sqlite"), 1)
        .context("open repo index sqlite")?;
    ensure_schema(&conn)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "DELETE FROM repo_file_index WHERE profile_name=?1",
        [profile_name],
    )?;
    for entry in &entries {
        tx.execute(
            "INSERT INTO repo_file_index(profile_name, path, symbols, path_tokens, symbol_tokens, body_terms, doc_len)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                profile_name,
                entry.path,
                serde_json::to_string(&entry.symbols)?,
                serde_json::to_string(&entry.path_tokens)?,
                serde_json::to_string(&entry.symbol_tokens)?,
                serde_json::to_string(&entry.body_terms)?,
                entry.doc_len as i64,
            ],
        )?;
    }
    tx.execute(
        "INSERT OR REPLACE INTO repo_index_meta(profile_name, repo_checkout, base_commit, indexed_at, file_count)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            profile_name,
            repo_checkout.to_string_lossy().as_ref(),
            base_commit,
            now() as i64,
            entries.len() as i64,
        ],
    )?;
    tx.commit()?;

    Ok(IndexSummary {
        file_count: entries.len(),
        base_commit: base_commit.to_string(),
    })
}

pub fn delete_profile_index(state_root: &Path, profile_name: &str) -> Result<()> {
    let conn = open_sqlite_with_retry(&state_root.join("state.sqlite"), 1)?;
    ensure_schema(&conn)?;
    conn.execute(
        "DELETE FROM repo_file_index WHERE profile_name=?1",
        [profile_name],
    )?;
    conn.execute(
        "DELETE FROM repo_index_meta WHERE profile_name=?1",
        [profile_name],
    )?;
    Ok(())
}

pub fn profile_indexed_file_count(state_root: &Path, profile_name: &str) -> Result<Option<usize>> {
    let conn = open_sqlite_with_retry(&state_root.join("state.sqlite"), 1)?;
    ensure_schema(&conn)?;
    let row = conn.query_row::<i64, _, _>(
        "SELECT file_count FROM repo_index_meta WHERE profile_name=?1",
        [profile_name],
        |r| r.get(0),
    );
    match row {
        Ok(n) if n >= 0 => Ok(Some(n as usize)),
        Ok(_) => Ok(None),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn load_cached_index(
    state_root: &Path,
    profile_name: &str,
    repo_checkout: &Path,
    current_head: &str,
) -> Result<Option<Vec<IndexedEntry>>> {
    let conn = open_sqlite_with_retry(&state_root.join("state.sqlite"), 1)?;
    ensure_schema(&conn)?;
    let meta = conn.query_row::<(String, String), _, _>(
        "SELECT repo_checkout, base_commit FROM repo_index_meta WHERE profile_name=?1",
        [profile_name],
        |r| Ok((r.get(0)?, r.get(1)?)),
    );
    let Ok((cached_checkout, cached_base)) = meta else {
        return Ok(None);
    };
    if cached_checkout != repo_checkout.to_string_lossy().as_ref() || cached_base != current_head {
        return Ok(None);
    }

    let mut stmt = conn.prepare(
        "SELECT path, symbols, path_tokens, symbol_tokens, body_terms, doc_len
         FROM repo_file_index
         WHERE profile_name=?1",
    )?;
    let rows = stmt.query_map([profile_name], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, i64>(5)?,
        ))
    })?;
    let mut entries = Vec::new();
    for row in rows {
        let (path, symbols_s, path_s, sym_tok_s, body_s, doc_len) = row?;
        entries.push(IndexedEntry {
            path,
            symbols: serde_json::from_str(&symbols_s).unwrap_or_default(),
            path_tokens: serde_json::from_str(&path_s).unwrap_or_default(),
            symbol_tokens: serde_json::from_str(&sym_tok_s).unwrap_or_default(),
            body_terms: serde_json::from_str(&body_s).unwrap_or_default(),
            doc_len: doc_len.max(0) as u32,
        });
    }
    Ok(Some(entries))
}

fn strip_line_col_suffix(token: &str) -> &str {
    let mut s = token;
    for _ in 0..2 {
        if let Some((head, tail)) = s.rsplit_once(':')
            && !tail.is_empty()
            && tail.bytes().all(|b| b.is_ascii_digit())
            && (head.contains('/') || head.contains('.'))
        {
            s = head;
        } else {
            break;
        }
    }
    s
}

struct ParsedQuery {
    explicit_paths: Vec<String>,
    exact_identifiers: Vec<String>,
    weighted_terms: BTreeMap<String, f64>,
    mentions_tests: bool,
}

fn parse_query(objective: &str, acceptance: &str) -> ParsedQuery {
    let mut explicit_paths = Vec::new();
    let mut exact_identifiers = Vec::new();
    let mut weighted_terms: BTreeMap<String, f64> = BTreeMap::new();

    for (text, section_weight) in [(objective, 1.0_f64), (acceptance, 0.5_f64)] {
        for raw in text.split_whitespace() {
            let trimmed = raw.trim_matches(|c: char| {
                matches!(
                    c,
                    '\'' | '"'
                        | '`'
                        | '('
                        | ')'
                        | '['
                        | ']'
                        | '{'
                        | '}'
                        | '<'
                        | '>'
                        | ','
                        | ';'
                        | ':'
                        | '.'
                        | '!'
                        | '?'
                )
            });
            let path_clean = strip_line_col_suffix(trimmed)
                .replace('\\', "/")
                .trim_start_matches("./")
                .to_string();
            if path_clean.contains('/')
                && path_clean.contains('.')
                && !explicit_paths
                    .iter()
                    .any(|p: &String| p.eq_ignore_ascii_case(&path_clean))
            {
                explicit_paths.push(path_clean.clone());
            } else if path_clean.contains('.')
                && !path_clean.contains(' ')
                && let Some((_, ext)) = path_clean.rsplit_once('.')
                && (1..=5).contains(&ext.len())
                && ext.bytes().all(|b| b.is_ascii_alphanumeric())
                && !explicit_paths
                    .iter()
                    .any(|p: &String| p.eq_ignore_ascii_case(&path_clean))
            {
                explicit_paths.push(path_clean);
            }

            let ident_clean = trimmed.trim_end_matches("()");
            let is_code_ident = ident_clean.len() >= 3
                && ident_clean
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
                && (ident_clean.contains('_')
                    || (ident_clean.chars().any(|c| c.is_ascii_uppercase())
                        && ident_clean.chars().any(|c| c.is_ascii_lowercase())));
            if is_code_ident
                && !exact_identifiers
                    .iter()
                    .any(|id: &String| id == ident_clean)
            {
                exact_identifiers.push(ident_clean.to_string());
            }

            for tok in split_code_tokens(trimmed) {
                let entry = weighted_terms.entry(tok).or_insert(0.0);
                *entry = entry.max(section_weight);
            }
        }
    }

    let combined_lower = format!("{objective} {acceptance}").to_ascii_lowercase();
    let mentions_tests = combined_lower.contains("unit test")
        || combined_lower.contains("integration test")
        || combined_lower.contains("regression test")
        || combined_lower
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|w| matches!(w, "test" | "tests" | "spec" | "specs"));

    ParsedQuery {
        explicit_paths,
        exact_identifiers,
        weighted_terms,
        mentions_tests,
    }
}

fn is_companion_test_path(source_path: &str, candidate_path: &str) -> bool {
    if source_path == candidate_path {
        return false;
    }
    let stem = source_path
        .rsplit('/')
        .next()
        .and_then(|name| name.split('.').next())
        .unwrap_or("");
    if stem.len() < 3 {
        return false;
    }
    let cand_lower = candidate_path.to_ascii_lowercase();
    let stem_lower = stem.to_ascii_lowercase();
    let is_test = cand_lower.ends_with("_test.rs")
        || cand_lower.ends_with(".test.ts")
        || cand_lower.ends_with(".test.tsx")
        || cand_lower.ends_with(".spec.ts")
        || cand_lower.ends_with(".spec.tsx")
        || cand_lower.contains("/tests/");
    is_test && cand_lower.contains(&stem_lower)
}

/// Scores indexed files for `(objective, acceptance)` and merges top matches with the profile's
/// default scope (matched files prioritized first, capped at `MAX_ALLOWED_FILES = 64`).
pub fn suggest_files_for_objective(
    state_root: &Path,
    profile: &Profile,
    objective: &str,
    acceptance: &str,
    max_matched: usize,
) -> Result<SuggestFilesResponse> {
    let checkout = &profile.repository.checkout;
    let current_head = Repository::inspect(checkout)
        .map(|r| r.base)
        .unwrap_or_else(|_| profile.repository.base.clone());
    let excluded = profile
        .workers
        .first()
        .map(|w| w.preset.verification_files.as_slice())
        .unwrap_or(&[]);

    let (entries, used_cached_index) =
        match load_cached_index(state_root, &profile.name, checkout, &current_head)? {
            Some(cached) => (cached, true),
            None => {
                index_profile_repo(state_root, &profile.name, checkout, &current_head, excluded)?;
                let loaded = load_cached_index(state_root, &profile.name, checkout, &current_head)?
                    .unwrap_or_default();
                (loaded, false)
            }
        };

    let indexed_files = entries.len();
    let profile_scope = profile.scope();
    let cap = max_matched.clamp(1, 16);
    let parsed = parse_query(objective, acceptance);

    if entries.is_empty()
        || (parsed.explicit_paths.is_empty()
            && parsed.exact_identifiers.is_empty()
            && parsed.weighted_terms.is_empty())
    {
        let mut merged_scope = profile_scope;
        merged_scope.truncate(MAX_ALLOWED_FILES);
        return Ok(SuggestFilesResponse {
            matched: Vec::new(),
            merged_scope,
            indexed_files,
            used_cached_index,
        });
    }

    let n_docs = entries.len() as f64;
    let avg_dl = (entries.iter().map(|e| e.doc_len as f64).sum::<f64>() / n_docs).max(1.0);
    let mut doc_freq: BTreeMap<String, usize> = BTreeMap::new();
    for term in parsed.weighted_terms.keys() {
        let df = entries
            .iter()
            .filter(|e| {
                e.body_terms.contains_key(term)
                    || e.path_tokens.iter().any(|t| t == term)
                    || e.symbol_tokens.iter().any(|t| t == term)
            })
            .count();
        doc_freq.insert(term.clone(), df);
    }

    let k1 = 1.2_f64;
    let b = 0.65_f64;
    let mut scored: Vec<SuggestedFile> = Vec::new();

    for entry in &entries {
        let mut score = 0.0_f64;
        let mut reasons: Vec<String> = Vec::new();
        let path_lower = entry.path.to_ascii_lowercase();
        let file_name = entry.path.rsplit('/').next().unwrap_or(&entry.path);
        let mut distinct_term_hits = 0usize;

        for exp in &parsed.explicit_paths {
            let exp_lower = exp.to_ascii_lowercase();
            if path_lower == exp_lower {
                score += 120.0;
                reasons.push("exact path".into());
            } else if path_lower.ends_with(&format!("/{exp_lower}"))
                || file_name.eq_ignore_ascii_case(&exp_lower)
            {
                score += 55.0;
                reasons.push(format!("filename: {file_name}"));
            }
        }

        for ident in &parsed.exact_identifiers {
            if entry.symbols.iter().any(|s| s == ident) {
                score += 45.0;
                reasons.push(format!("symbol: {ident}"));
            } else {
                let norm = ident.to_ascii_lowercase();
                if entry.body_terms.contains_key(&norm) {
                    score += 22.0;
                    reasons.push(format!("identifier: {ident}"));
                }
            }
        }

        let mut field_score = 0.0_f64;
        for (term, q_weight) in &parsed.weighted_terms {
            let df = doc_freq.get(term).copied().unwrap_or(0) as f64;
            if df == 0.0 {
                continue;
            }
            let idf = (((n_docs - df + 0.5) / (df + 0.5)) + 1.0).ln().max(0.2);

            let in_path = entry.path_tokens.iter().any(|t| t == term);
            let in_sym = entry.symbol_tokens.iter().any(|t| t == term);
            let body_tf = entry.body_terms.get(term).copied().unwrap_or(0) as f64;

            if in_path || in_sym || body_tf > 0.0 {
                distinct_term_hits += 1;
            }

            if in_path {
                field_score += 4.5 * idf * q_weight;
            }
            if in_sym {
                field_score += 3.5 * idf * q_weight;
            }
            if body_tf > 0.0 {
                let norm_tf = (body_tf * (k1 + 1.0))
                    / (body_tf + k1 * (1.0 - b + b * (entry.doc_len as f64 / avg_dl)));
                field_score += 1.8 * norm_tf * idf * q_weight;
            }
        }
        score += field_score;
        if distinct_term_hits >= 2 {
            score += (distinct_term_hits as f64 - 1.0) * 2.5;
            if reasons.is_empty() {
                reasons.push(format!("{distinct_term_hits} term matches"));
            }
        }

        let has_explicit = !reasons.is_empty() && score >= 20.0;
        let has_strong_bm25 = score >= 4.5 && distinct_term_hits >= 2;
        let has_rare_single = score >= 6.0 && distinct_term_hits == 1;
        if has_explicit || has_strong_bm25 || has_rare_single {
            scored.push(SuggestedFile {
                path: entry.path.clone(),
                score: (score * 100.0).round() / 100.0,
                reasons,
            });
        }
    }

    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| source_file_rank(&a.path).cmp(&source_file_rank(&b.path)))
            .then_with(|| a.path.cmp(&b.path))
    });
    scored.truncate(cap);

    if parsed.mentions_tests && !scored.is_empty() {
        let seed_paths: Vec<String> = scored.iter().take(2).map(|s| s.path.clone()).collect();
        for seed in seed_paths {
            if scored.len() >= cap {
                break;
            }
            if let Some(comp) = entries.iter().find(|e| {
                is_companion_test_path(&seed, &e.path) && !scored.iter().any(|s| s.path == e.path)
            }) {
                scored.push(SuggestedFile {
                    path: comp.path.clone(),
                    score: 3.0,
                    reasons: vec!["companion test".into()],
                });
            }
        }
    }

    // Merge matched files first, followed by the profile's existing top files, capped at MAX_ALLOWED_FILES.
    let mut merged_scope = Vec::new();
    for hit in &scored {
        if !merged_scope.iter().any(|p: &String| p == &hit.path) {
            merged_scope.push(hit.path.clone());
        }
    }
    for prof_file in profile_scope {
        if merged_scope.len() >= MAX_ALLOWED_FILES {
            break;
        }
        if !merged_scope.iter().any(|p: &String| p == &prof_file) {
            merged_scope.push(prof_file);
        }
    }
    merged_scope.truncate(MAX_ALLOWED_FILES);

    Ok(SuggestFilesResponse {
        matched: scored,
        merged_scope,
        indexed_files,
        used_cached_index,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_code_tokens_handles_camel_and_snake_case() {
        assert_eq!(
            split_code_tokens("query_session_flight_log"),
            vec![
                "query_session_flight_log",
                "query",
                "session",
                "flight",
                "log"
            ]
        );
        assert_eq!(
            split_code_tokens("src/FlightLogPanel.tsx"),
            vec!["src", "flight", "log", "panel", "tsx"]
        );
    }

    #[test]
    fn extract_symbols_finds_rust_and_ts_declarations() {
        let code = r#"
pub async fn query_session_flight_log(id: &str) {}
struct FlightLogEntry { ts: i64 }
export function renderTimelineView() {}
export class ChatDeliveryTracker {}
"#;
        let syms = extract_symbols(code);
        assert_eq!(
            syms,
            vec![
                "query_session_flight_log",
                "FlightLogEntry",
                "renderTimelineView",
                "ChatDeliveryTracker"
            ]
        );
    }
}
