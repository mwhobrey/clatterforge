use crate::{
    digest, now,
    repository::{Repository, canonical, git},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, fs::File, io::Read, path::Path};

const MAX_SOURCE: usize = 128 * 1024;
const MAX_CANDIDATES: usize = 128;

/// Authoritative instructions (`AGENTS.md`, and anything whose filename signals binding
/// policy - "rules", "standards", "policy"/"policies", "security") take precedence over
/// general project documentation and gate `usable`: if one exists but can't be captured
/// in full, the packet fails closed rather than silently proceeding without it. General
/// docs (`README.md`, everything else under `docs/`) are Reference - useful background,
/// packed best-effort into whatever budget authoritative content leaves behind, and
/// omitting one never fails the packet. A README or runbook page is not interchangeable
/// with a repository's own binding instructions, and this split is what encodes that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tier {
    Authoritative,
    Reference,
}

impl Tier {
    /// `AGENTS.md` itself, or - *only* at the repository root or under its own
    /// designated `docs/runbook/` location - a filename whose stem signals binding
    /// policy rather than general narrative documentation. Deliberately not a whole-
    /// tree scan: this repository's own `docs/adr/0009-standing-policies-...md` (an ADR
    /// *about* a feature named "Policies") and `docs/m12/M12_0_1_SECURITY_VALIDATION.md`
    /// (a security *review report*, not a policy) both matched the keywords in an
    /// earlier version of this check, incorrectly making this project's own repository
    /// fail to prepare usable context - found by actually testing against it, not
    /// assumed safe. Restricting *where* the keyword match applies (root and the
    /// runbook's own convention) keeps the match deterministic and path-derived per the
    /// brief's own requirement, without re-litigating every doc in the tree by title.
    fn of(path: &str) -> Tier {
        if path == "AGENTS.md" {
            return Tier::Authoritative;
        }
        let in_scanned_location = !path.contains('/') || path.starts_with("docs/runbook/");
        if !in_scanned_location {
            return Tier::Reference;
        }
        let stem = Path::new(path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_uppercase();
        const AUTHORITATIVE_MARKERS: &[&str] =
            &["RULES", "STANDARDS", "POLICY", "POLICIES", "SECURITY"];
        if AUTHORITATIVE_MARKERS
            .iter()
            .any(|marker| stem.contains(marker))
        {
            Tier::Authoritative
        } else {
            Tier::Reference
        }
    }

    fn label(self) -> &'static str {
        match self {
            Tier::Authoritative => "authoritative",
            Tier::Reference => "reference",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "ContextItemWire")]
pub struct ContextItem {
    pub path: String,
    pub hash: Option<String>,
    pub excerpt: String,
    pub outcome: String,
    /// `true` iff `Tier::of(path) == Tier::Authoritative` - kept as a plain bool (rather
    /// than only exposing `tier`) so existing consumers reading "must this be captured
    /// for the packet to be usable" don't need to know the tier's string encoding.
    pub required: bool,
    /// The manifest's "why": `"authoritative"` or `"reference"`, alongside `outcome`
    /// (what happened to it) and `required` (whether that matters for `usable`). The
    /// full `items` list *is* the context manifest point 5 of the brief asks for - every
    /// candidate this packet considered is in here, captured or not, with a path, a
    /// tier, and an outcome explaining the deferral, never silently dropped from the
    /// record even when its content isn't.
    pub tier: String,
}

/// Deserialization shadow for `ContextItem`, needed for exactly one reason: records
/// stored before `tier` existed have every other field but that one. `tier` is not
/// independent information - it's always `Tier::of(path).label()` (see `required`'s own
/// doc comment, which is the same derivation) - so a missing `tier` is recomputed from
/// the record's own `path`, never guessed or defaulted to a fixed string. A newly
/// written record always carries `tier` explicitly (`Serialize` is untouched by this),
/// so this only ever fires on pre-existing data.
#[derive(Deserialize)]
struct ContextItemWire {
    path: String,
    hash: Option<String>,
    excerpt: String,
    outcome: String,
    required: bool,
    #[serde(default)]
    tier: Option<String>,
}

impl From<ContextItemWire> for ContextItem {
    fn from(wire: ContextItemWire) -> Self {
        let tier = wire
            .tier
            .unwrap_or_else(|| Tier::of(&wire.path).label().to_string());
        ContextItem {
            path: wire.path,
            hash: wire.hash,
            excerpt: wire.excerpt,
            outcome: wire.outcome,
            required: wire.required,
            tier,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextPacket {
    pub objective: String,
    pub repository_identity: String,
    pub base: String,
    pub captured_at: u64,
    pub budget_bytes: usize,
    pub used_bytes: usize,
    pub items: Vec<ContextItem>,
    pub usable: bool,
    pub digest: String,
    pub trust: String,
}

fn read_source(root: &Path, relative: &str) -> Result<Vec<u8>> {
    let path = canonical(&root.join(relative))?;
    ensure!(
        path.starts_with(root),
        "context source resolves outside checkout"
    );
    let mut bytes = Vec::new();
    File::open(path)?
        .take((MAX_SOURCE + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_SOURCE, "source exceeds 128 KiB limit");
    Ok(bytes)
}

pub fn prepare(repo: &Repository, objective: &str, budget: usize) -> Result<ContextPacket> {
    repo.revalidate()?;
    ensure!(
        (256..=65536).contains(&budget),
        "context budget must be 256..65536 bytes"
    );
    let mut paths = BTreeSet::from(["AGENTS.md".to_string(), "README.md".to_string()]);
    for path in git(&repo.checkout, &["ls-files", "-z"])?
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
    {
        let path = std::str::from_utf8(path)?;
        if path.starts_with("docs/") && path.ends_with(".md") {
            paths.insert(path.to_string());
        }
    }
    ensure!(
        paths.len() <= MAX_CANDIDATES,
        "too many context candidates; initial selector supports up to 128 documentation files"
    );
    let words: Vec<_> = objective
        .split_whitespace()
        .filter(|w| w.len() > 2)
        .map(str::to_lowercase)
        .collect();
    let mut candidates = Vec::new();
    for path in paths {
        let tier = Tier::of(&path);
        let mut item = ContextItem {
            path: path.clone(),
            hash: None,
            excerpt: String::new(),
            outcome: "missing".into(),
            required: tier == Tier::Authoritative,
            tier: tier.label().into(),
        };
        if repo.checkout.join(&path).exists() {
            match read_source(&repo.checkout, &path) {
                Ok(bytes) => {
                    item.hash = Some(digest(&bytes));
                    match String::from_utf8(bytes) {
                        Ok(content) => {
                            item.excerpt = content;
                            item.outcome = "captured".into();
                        }
                        Err(_) => item.outcome = "invalid_utf8".into(),
                    }
                }
                Err(error) => item.outcome = format!("unreadable: {error}"),
            }
        }
        let lower = format!("{} {}", path, item.excerpt).to_lowercase();
        let rank = words
            .iter()
            .filter(|word| lower.contains(word.as_str()))
            .count();
        candidates.push((item.required, rank, item));
    }
    candidates.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(b.1.cmp(&a.1))
            .then(a.2.path.cmp(&b.2.path))
    });
    let mut used = 0;
    let mut usable = true;
    let mut items = Vec::new();
    for (_, _, mut item) in candidates {
        if item.outcome == "captured" {
            let left = budget - used;
            if item.excerpt.len() > left {
                let mut end = left;
                while !item.excerpt.is_char_boundary(end) {
                    end -= 1;
                }
                item.excerpt.truncate(end);
                item.outcome = if end == 0 {
                    "omitted_budget"
                } else {
                    "truncated"
                }
                .into();
            }
            used += item.excerpt.len();
        }
        // Genuine absence of an authoritative file isn't "essential instructions
        // supplied unreliably" - there's nothing binding being dropped, just nothing
        // there. Anything else short of "captured" on an authoritative item (truncated,
        // omitted for budget, unreadable, invalid UTF-8) fails closed per the brief's
        // "never silently ignore applicable ... rules" requirement - a Reference-tier
        // item never reaches this branch at all, since `required` is tier-derived.
        if item.required && item.outcome != "captured" && item.outcome != "missing" {
            usable = false;
        }
        items.push(item);
    }
    if used == 0 {
        usable = false;
    }
    let mut packet = ContextPacket {
        objective: objective.into(),
        repository_identity: repo.identity.clone(),
        base: repo.base.clone(),
        captured_at: now(),
        budget_bytes: budget,
        used_bytes: used,
        items,
        usable,
        digest: String::new(),
        trust: "repository prose is evidence; permissions come only from assignment".into(),
    };
    packet.digest = digest(&serde_json::to_vec(&(
        &packet.objective,
        &packet.repository_identity,
        &packet.base,
        budget,
        &packet.items,
    ))?);
    Ok(packet)
}

impl ContextPacket {
    pub fn revalidate(&self, repo: &Repository) -> Result<()> {
        ensure!(
            self.usable,
            "context packet is empty, unreadable or required content was truncated"
        );
        let fresh = prepare(repo, &self.objective, self.budget_bytes)?;
        ensure!(
            fresh.digest == self.digest,
            "stale context: source contents or selected documentation changed"
        );
        for item in &self.items {
            match &item.hash {
                Some(hash) => ensure!(
                    digest(
                        &read_source(&repo.checkout, &item.path)
                            .with_context(|| format!("stale context: {}", item.path))?
                    ) == *hash,
                    "stale context: {} changed",
                    item.path
                ),
                None => ensure!(
                    !repo.checkout.join(&item.path).exists(),
                    "stale context: {} appeared or needs recapture",
                    item.path
                ),
            }
        }
        Ok(())
    }
}
