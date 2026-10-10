//! Resolving a model's real context window.
//!
//! The configured window is a single number for every model, so it is often
//! wrong in one of two directions: a small model is allowed to be overfilled,
//! or a large one is capped far below what it can hold. This module asks two
//! sources, in order, before the caller falls back to the configured default:
//!
//! 1. The configured endpoint's `/models` listing — the same authority the
//!    daemon already talks to, and the only source that knows what *this*
//!    deployment actually serves.
//! 2. [models.dev](https://models.dev) — a public catalogue, consulted only when
//!    the endpoint says nothing. Fetched with a short timeout and cached on disk
//!    for a day so a request never waits on it twice.
//!
//! The catalogue read here is `models.json`, the provider-agnostic listing: one
//! flat object keyed by a namespaced model id (`zhipuai/glm-5.3-flash`, window at
//! `limit.context`). `api.json` is deliberately *not* used — its provider-nested
//! shape is only needed by the catalogue UI, which keeps its own copy.
//!
//! Every failure is soft. A missing field, an unreachable host or an unparsable
//! payload all resolve to `None`, which leaves the caller on its existing
//! default rather than failing a request.

use serde_json::Value;
use std::path::Path;

/// The public models.dev catalogue, provider-agnostic listing.
///
/// Keys are namespaced (`zhipuai/glm-5.3-flash`, `deepseek/deepseek-v4.1-flash`)
/// and the context window lives under `limit.context`.
pub const MODELS_DEV_URL: &str = "https://models.dev/models.json";

/// How long a cached models.dev payload is trusted, in seconds.
pub const MODELS_DEV_CACHE_TTL_SECS: u64 = 24 * 60 * 60;

/// Smallest value accepted as a context window. Anything below this is a typo
/// or a per-response output limit, not a context.
pub const CONTEXT_WINDOW_MIN: usize = 1_024;

/// Largest value accepted as a context window, guarding against a byte count or
/// a timestamp that happens to sit in the field.
pub const CONTEXT_WINDOW_MAX: usize = 10_000_000;

/// Timeout for each metadata request. These are advisory lookups: they must
/// never hold a completion request open.
const METADATA_REQUEST_TIMEOUT_SECS: u64 = 10;

/// Field names an OpenAI-compatible `/models` entry might use for its window.
const CONTEXT_WINDOW_KEYS: [&str; 6] = [
    "context_length",
    "context_window",
    "max_context_window",
    "context",
    "max_input_tokens",
    "supported_input_tokens",
];

/// Keys inside a nested `limit` object, as models.dev publishes them.
const LIMIT_KEYS: [&str; 2] = ["context", "output"];

/// Whether a number is plausible as a context window.
pub fn is_plausible_context_window(tokens: usize) -> bool {
    (CONTEXT_WINDOW_MIN..=CONTEXT_WINDOW_MAX).contains(&tokens)
}

/// Read a number out of a JSON value that may be numeric or a numeric string.
fn as_tokens(value: &Value) -> Option<usize> {
    if let Some(number) = value.as_u64() {
        return usize::try_from(number).ok();
    }
    value
        .as_str()
        .and_then(|text| text.trim().parse::<usize>().ok())
}

/// Read a context window out of one model entry.
///
/// Tries the flat candidate keys first, then a nested `limit` object, ignoring
/// anything implausible so a junk field cannot win over a good one.
pub fn context_window_from_model_entry(entry: &Value) -> Option<usize> {
    let object = entry.as_object()?;

    for key in CONTEXT_WINDOW_KEYS {
        if let Some(tokens) = object.get(key).and_then(as_tokens)
            && is_plausible_context_window(tokens)
        {
            return Some(tokens);
        }
    }

    if let Some(limit) = object.get("limit").and_then(Value::as_object) {
        for key in LIMIT_KEYS {
            if let Some(tokens) = limit.get(key).and_then(as_tokens)
                && is_plausible_context_window(tokens)
            {
                return Some(tokens);
            }
        }
    }

    None
}

/// Which rung of the id-mapping ladder produced a match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdMatch {
    /// The ids are equal, ignoring case.
    Exact,
    /// The candidate equaled a known id once its `:qualifier` was stripped.
    ColonSuffix,
    /// The ids matched once `-` and `.` were treated as the same separator.
    SeparatorSwap,
    /// The ids matched once a leading `vendor/` was removed from either side.
    VendorPrefix,
}

impl IdMatch {
    /// A short label for logs.
    pub fn as_str(self) -> &'static str {
        match self {
            IdMatch::Exact => "exact",
            IdMatch::ColonSuffix => "colon-suffix",
            IdMatch::SeparatorSwap => "separator-swap",
            IdMatch::VendorPrefix => "vendor-prefix",
        }
    }
}

/// Strip a trailing `:qualifier`, e.g. `glm-5.3:cloudflare` -> `glm-5.3`, `:free`.
///
/// A qualifier names where or how the *same* model is served, so removing it
/// cannot land on a different model. That is why this is stripped while a
/// `-variant` suffix is not — see [`map_model_id`] for the full reasoning.
fn strip_colon_suffix(id: &str) -> &str {
    match id.split_once(':') {
        Some((head, _)) if !head.is_empty() => head,
        _ => id,
    }
}

/// Strip a leading `vendor/` prefix, e.g. `zai/glm-5.3` -> `glm-5.3`.
fn strip_vendor_prefix(id: &str) -> &str {
    match id.split_once('/') {
        Some((_, rest)) if !rest.is_empty() => rest,
        _ => id,
    }
}

/// Flip the separator that sits between two digits — the version segment.
///
/// `glm-5-3-flash` -> `glm-5.3-flash` and `glm-5.3-flash` -> `glm-5-3-flash`.
/// A separator that is not between two digits is left alone, so the dash before
/// `flash` in `qwen3.8-flash` survives and only the `3.8`/`3-8` part flips.
fn swap_version_separators(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut swapped = String::with_capacity(name.len());
    for (index, ch) in chars.iter().enumerate() {
        let between_digits = index > 0
            && index + 1 < chars.len()
            && chars[index - 1].is_ascii_digit()
            && chars[index + 1].is_ascii_digit();
        match (ch, between_digits) {
            ('.', true) => swapped.push('-'),
            ('-', true) => swapped.push('.'),
            _ => swapped.push(*ch),
        }
    }
    swapped
}

/// Separator variants of a model name, most faithful first, lower-cased and
/// deduplicated.
///
/// Providers and the catalogue disagree about `-` and `.` in the *same* id
/// (`glm-5-3-flash` vs `glm-5.3-flash`), and real ids mix both conventions
/// (`glm-5.3-flash` has a dot inside the version and dashes elsewhere). So the
/// separator is flipped in both directions across the whole id, *and* only where
/// it sits between two digits.
///
/// Every variant keeps the whole model name, so callers can still demand exact
/// equality against one — this is not a prefix or similarity match.
fn separator_variants(name: &str) -> Vec<String> {
    let mut variants: Vec<String> = Vec::new();
    for variant in [
        name.to_owned(),
        name.replace('.', "-"),
        name.replace('-', "."),
        swap_version_separators(name),
    ] {
        let variant = variant.to_ascii_lowercase();
        if !variants.contains(&variant) {
            variants.push(variant);
        }
    }
    variants
}

/// Map a provider-flavoured model id onto one of `known` ids.
///
/// Provider ids are aggregator-flavoured and rarely match a catalogue verbatim,
/// so the ladder tries, in order: exact, `:qualifier`-stripped, separator-swapped
/// (`-` <-> `.`, both directions), and vendor-namespace-stripped. The rung that
/// matched is returned with the id so the caller can log what it did. All four
/// rungs compare case-insensitively.
///
/// The catalogue used here keys entries by a namespaced id, so a bare provider id
/// is matched against the *model segment* of the key (`zhipuai/glm-5.3-flash` for
/// `glm-5-3-flash`), which is what the vendor-namespace rung does.
///
/// Every comparison is exact equality against a whole generated variant: no rung
/// matches on a prefix, on a substring or on similarity.
///
/// Every rung keeps the full model name intact, and there is deliberately **no
/// fuzzy matching**: when nothing matches, the caller falls through to its
/// configured default. Two suffixes are treated differently, on purpose:
///
/// * A trailing `:qualifier` (`glm-5.3:cloudflare`, `glm-5.3:free`) *is*
///   stripped, because it names how the same model is deployed or routed.
/// * A `-variant` (`-flash`, `-pro`, `-max`, `-lite`, `-preview`, `-thinking`,
///   `-r1`) is *not*, because a variant is a different model that can carry a
///   different context window. So `glm-5.3` never resolves to `glm-5.3-flash`,
///   and `glm-5.3-flash` never falls back to `glm-5.3`, even when only one of
///   the two appears in the catalogue. Guessing between variants would silently
///   hand a model a window that is not its own, which is worse than the default.
pub fn map_model_id(candidate: &str, known: &[String]) -> Option<(String, IdMatch)> {
    let candidate_trimmed = candidate.trim();
    if candidate_trimmed.is_empty() {
        return None;
    }

    // (a) exact, case-insensitive.
    if let Some(found) = known
        .iter()
        .find(|id| id.eq_ignore_ascii_case(candidate_trimmed))
    {
        return Some((found.clone(), IdMatch::Exact));
    }

    // (b) the candidate with its `:suffix` stripped.
    let descoped = strip_colon_suffix(candidate_trimmed);
    if descoped != candidate_trimmed
        && let Some(found) = known.iter().find(|id| id.eq_ignore_ascii_case(descoped))
    {
        return Some((found.clone(), IdMatch::ColonSuffix));
    }

    // (c) separator variants, vendor namespace left in place on both sides.
    let variants = separator_variants(descoped);
    if let Some(found) = known.iter().find(|id| {
        let known_variants = separator_variants(strip_colon_suffix(id));
        variants
            .iter()
            .any(|variant| known_variants.contains(variant))
    }) {
        return Some((found.clone(), IdMatch::SeparatorSwap));
    }

    // (d) with a leading `vendor/` removed from either side, then the same
    // separator variants — this is the rung that matches a bare provider id
    // against the model segment of a namespaced catalogue key.
    let bare = separator_variants(strip_vendor_prefix(descoped));
    if let Some(found) = known.iter().find(|id| {
        let known_bare =
            separator_variants(strip_vendor_prefix(strip_colon_suffix(id)));
        bare.iter().any(|variant| known_bare.contains(variant))
    }) {
        return Some((found.clone(), IdMatch::VendorPrefix));
    }

    // No further rungs, deliberately: a variant suffix (`-flash`, `-pro`, `-max`,
    // `-lite`, `-preview`, `-thinking`, `-r1`) names a different model that can
    // carry a different context window, so nothing here may match on a prefix.
    // Falling through leaves the caller on its configured default.
    None
}

/// Find the context window for `model_id` in a decoded `/models` response.
///
/// Accepts either `{ "data": [ { "id": ..., ... } ] }` (OpenAI shape) or a bare
/// array, and falls back to the id-mapping ladder when the exact id is absent
/// because the endpoint advertises a namespaced variant.
pub fn context_window_from_models_response(body: &Value, model_id: &str) -> Option<usize> {
    let entries = body
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| body.as_array())?;

    let mut ids: Vec<String> = Vec::new();
    let mut windows: Vec<(String, usize)> = Vec::new();
    for entry in entries {
        let Some(id) = entry.get("id").and_then(Value::as_str) else {
            continue;
        };
        ids.push(id.to_string());
        if let Some(window) = context_window_from_model_entry(entry) {
            windows.push((id.to_string(), window));
        }
    }

    if let Some((_, window)) = windows
        .iter()
        .find(|(id, _)| id.eq_ignore_ascii_case(model_id))
    {
        return Some(*window);
    }

    let (matched, _) = map_model_id(model_id, &ids)?;
    windows
        .iter()
        .find(|(id, _)| id == &matched)
        .map(|(_, window)| *window)
}

/// Every `(model id, context window)` pair in a decoded models.dev payload.
///
/// The source used here, `models.json`, is a flat `{ "<namespaced id>": { … } }`
/// object, so the key *is* the id (`zhipuai/glm-5.3-flash`). The provider-nested
/// `api.json` shape — `{ "<provider>": { "models": { … } } }` — is also accepted,
/// since an older cached payload may still be in that form.
pub fn collect_models_dev_entries(data: &Value) -> Vec<(String, usize)> {
    let mut found = Vec::new();
    let Some(root) = data.as_object() else {
        return found;
    };

    for (key, value) in root {
        if let Some(models) = value.get("models").and_then(Value::as_object) {
            for (id, entry) in models {
                if let Some(window) = context_window_from_model_entry(entry) {
                    found.push((id.clone(), window));
                }
            }
        } else if let Some(window) = context_window_from_model_entry(value) {
            found.push((key.clone(), window));
        }
    }

    found
}

/// Resolve `candidate` against a decoded models.dev payload.
pub fn resolve_from_models_dev(candidate: &str, data: &Value) -> Option<(String, IdMatch, usize)> {
    let entries = collect_models_dev_entries(data);
    if entries.is_empty() {
        return None;
    }
    let ids: Vec<String> = entries.iter().map(|(id, _)| id.clone()).collect();
    let (matched, step) = map_model_id(candidate, &ids)?;
    let window = entries
        .iter()
        .find(|(id, _)| id == &matched)
        .map(|(_, window)| *window)?;
    Some((matched, step, window))
}

/// Ask an endpoint's `/models` listing what it knows about `model_id`.
///
/// Returns `None` on any failure — transport, status, parse or absence — since
/// this is an advisory lookup.
pub async fn fetch_context_window_from_endpoint(
    client: &reqwest::Client,
    models_url: &str,
    api_key: Option<&str>,
    model_id: &str,
) -> Option<usize> {
    let mut request = client
        .get(models_url)
        .header("accept", "application/json")
        .timeout(std::time::Duration::from_secs(
            METADATA_REQUEST_TIMEOUT_SECS,
        ));
    if let Some(key) = api_key.filter(|key| !key.trim().is_empty()) {
        request = request.header("authorization", format!("Bearer {key}"));
    }

    let response = request.send().await.ok()?;
    if !response.status().is_success() {
        tracing::debug!(
            status = response.status().as_u16(),
            %models_url,
            "models listing did not answer, skipping context-window lookup"
        );
        return None;
    }

    let body: Value = response.json().await.ok()?;
    context_window_from_models_response(&body, model_id)
}

/// Load the models.dev payload, preferring a fresh on-disk cache.
///
/// A stale cache is still better than nothing when the network is down, so it is
/// returned as a last resort rather than discarded.
pub async fn load_models_dev(client: &reqwest::Client, cache_path: &Path) -> Option<Value> {
    if let Some(cached) = read_cached_models_dev(cache_path) {
        return Some(cached);
    }

    let fetched = fetch_models_dev(client).await;
    match fetched {
        Some(data) => {
            if let Err(error) = write_cached_models_dev(cache_path, &data).await {
                tracing::debug!(%error, "could not cache models.dev payload");
            }
            Some(data)
        }
        None => {
            tracing::debug!("models.dev unreachable, falling back to the configured window");
            None
        }
    }
}

/// Read the cache when it exists and is within its TTL.
fn read_cached_models_dev(cache_path: &Path) -> Option<Value> {
    let metadata = std::fs::metadata(cache_path).ok()?;
    let modified = metadata.modified().ok()?;
    let age = modified.elapsed().ok()?;
    if age.as_secs() >= MODELS_DEV_CACHE_TTL_SECS {
        tracing::trace!("models.dev cache is stale, refetching");
        return None;
    }
    let text = std::fs::read_to_string(cache_path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Persist a fetched payload next to the other agent state.
async fn write_cached_models_dev(cache_path: &Path, data: &Value) -> std::io::Result<()> {
    if let Some(parent) = cache_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let text = serde_json::to_string(data).unwrap_or_else(|_| "{}".to_string());
    tokio::fs::write(cache_path, text).await
}

/// Fetch the models.dev catalogue with a short timeout.
async fn fetch_models_dev(client: &reqwest::Client) -> Option<Value> {
    let response = client
        .get(MODELS_DEV_URL)
        .header("accept", "application/json")
        .timeout(std::time::Duration::from_secs(
            METADATA_REQUEST_TIMEOUT_SECS,
        ))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json::<Value>().await.ok()
}

/// Resolve a model's context window, endpoint first, models.dev second.
///
/// Returns the window and a label naming the source that supplied it, or `None`
/// when neither knows — in which case the caller keeps its configured default.
pub async fn resolve_context_window(
    client: &reqwest::Client,
    models_url: &str,
    api_key: Option<&str>,
    models_dev_cache_path: &Path,
    model_id: &str,
) -> Option<(usize, &'static str)> {
    if let Some(window) =
        fetch_context_window_from_endpoint(client, models_url, api_key, model_id).await
    {
        tracing::debug!(model = %model_id, window, "context window from endpoint /models");
        return Some((window, "endpoint /models"));
    }

    let data = load_models_dev(client, models_dev_cache_path).await?;
    let (matched, step, window) = resolve_from_models_dev(model_id, &data)?;
    tracing::debug!(
        model = %model_id,
        matched = %matched,
        step = step.as_str(),
        window,
        "context window from models.dev"
    );
    Some((window, "models.dev"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn known(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| (*id).to_string()).collect()
    }

    #[test]
    fn context_window_reads_every_candidate_key() {
        for key in CONTEXT_WINDOW_KEYS {
            let entry = json!({ key: 200_000 });
            assert_eq!(
                context_window_from_model_entry(&entry),
                Some(200_000),
                "key {key} should be read"
            );
        }
        // Numeric strings are accepted too.
        assert_eq!(
            context_window_from_model_entry(&json!({ "context_length": "131072" })),
            Some(131_072)
        );
    }

    #[test]
    fn context_window_reads_a_nested_limit_object() {
        assert_eq!(
            context_window_from_model_entry(
                &json!({ "limit": { "context": 128_000, "output": 8_192 } })
            ),
            Some(128_000)
        );
    }

    #[test]
    fn context_window_ignores_implausible_values() {
        // Too small: an output cap accidentally sitting in a context field.
        assert_eq!(
            context_window_from_model_entry(
                &json!({ "context_length": 512, "context_window": 64_000 })
            ),
            Some(64_000)
        );
        // Too large: a byte count.
        assert_eq!(
            context_window_from_model_entry(&json!({ "context_length": 40_000_000 })),
            None
        );
        assert_eq!(
            context_window_from_model_entry(&json!({ "id": "glm-5.3" })),
            None
        );
    }

    #[test]
    fn id_mapping_exact_match() {
        let ids = known(&["glm-5.3-flash", "glm-5.3"]);
        assert_eq!(
            map_model_id("glm-5.3", &ids),
            Some(("glm-5.3".to_string(), IdMatch::Exact))
        );
        // Case-insensitive.
        assert_eq!(
            map_model_id("GLM-5.3", &ids),
            Some(("glm-5.3".to_string(), IdMatch::Exact))
        );
    }

    #[test]
    fn id_mapping_strips_a_colon_suffix() {
        let ids = known(&["glm-5.3"]);
        assert_eq!(
            map_model_id("glm-5.3:cloudflare", &ids),
            Some(("glm-5.3".to_string(), IdMatch::ColonSuffix))
        );
    }

    #[test]
    fn separator_variants_are_bidirectional_and_deduplicated() {
        // Dashes -> dot: the direction a dash-named provider id needs.
        assert!(separator_variants("glm-5-3-flash").contains(&"glm-5.3-flash".to_string()));
        // Dot -> dash: the reverse direction.
        assert!(separator_variants("glm-5.3-flash").contains(&"glm-5-3-flash".to_string()));
        // The id itself comes first and duplicates are dropped.
        let variants = separator_variants("glm-5.3-flash");
        assert_eq!(variants.first().map(String::as_str), Some("glm-5.3-flash"));
        let mut unique = variants.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), variants.len(), "variants must be deduplicated");
    }

    #[test]
    fn version_segment_swap_flips_only_the_separator_between_digits() {
        assert_eq!(swap_version_separators("glm-5-3-flash"), "glm-5.3-flash");
        assert_eq!(swap_version_separators("glm-5.3-flash"), "glm-5-3-flash");
        assert_eq!(swap_version_separators("qwen3.8-flash"), "qwen3-8-flash");
        assert_eq!(swap_version_separators("qwen3-8-flash"), "qwen3.8-flash");
        // A separator that is not between two digits is left where it is.
        assert_eq!(
            swap_version_separators("claude-3-5-sonnet"),
            "claude-3.5-sonnet"
        );
    }

    #[test]
    fn id_mapping_swaps_dash_and_dot_in_both_directions() {
        // Provider uses dashes, catalogue uses a dot.
        let ids = known(&["glm-5.3-flash"]);
        assert_eq!(
            map_model_id("glm-5-3-flash", &ids),
            Some(("glm-5.3-flash".to_string(), IdMatch::SeparatorSwap))
        );

        // And the reverse direction: provider uses a dot, catalogue uses dashes.
        let ids = known(&["glm-5-3-flash"]);
        assert_eq!(
            map_model_id("glm-5.3-flash", &ids),
            Some(("glm-5-3-flash".to_string(), IdMatch::SeparatorSwap))
        );

        // Every dot flipped to a dash, the aggregator form of a dotted id.
        let ids = known(&["deepseek-v4-1-flash"]);
        assert_eq!(
            map_model_id("deepseek.v4.1.flash", &ids),
            Some(("deepseek-v4-1-flash".to_string(), IdMatch::SeparatorSwap))
        );
    }

    #[test]
    fn id_mapping_matches_the_model_segment_of_a_namespaced_catalogue_key() {
        // The real catalogue shape: `models.json` keys carry a vendor namespace.
        let ids = known(&["zhipuai/glm-5.3-flash", "zhipuai/glm-5.3"]);
        assert_eq!(
            map_model_id("glm-5-3-flash", &ids),
            Some(("zhipuai/glm-5.3-flash".to_string(), IdMatch::VendorPrefix))
        );
        // The shorter name must not borrow the longer entry, and vice versa.
        assert_eq!(
            map_model_id("glm-5-3", &ids),
            Some(("zhipuai/glm-5.3".to_string(), IdMatch::VendorPrefix))
        );
        // The reverse separator direction, still against the namespaced key.
        let ids = known(&["zhipuai/glm-5-3-flash"]);
        assert_eq!(
            map_model_id("glm-5.3-flash", &ids),
            Some(("zhipuai/glm-5-3-flash".to_string(), IdMatch::VendorPrefix))
        );
        // A routed qualifier on top of the namespace is stripped first.
        assert_eq!(
            map_model_id("glm-5-3-flash:cloudflare", &ids),
            Some(("zhipuai/glm-5-3-flash".to_string(), IdMatch::VendorPrefix))
        );
    }

    #[test]
    fn id_mapping_ignores_a_vendor_prefix() {
        let ids = known(&["glm-5.3"]);
        assert_eq!(
            map_model_id("zai/glm-5.3", &ids),
            Some(("glm-5.3".to_string(), IdMatch::VendorPrefix))
        );
        let ids = known(&["zai/glm-5.3"]);
        assert_eq!(
            map_model_id("glm-5.3", &ids),
            Some(("zai/glm-5.3".to_string(), IdMatch::VendorPrefix))
        );
    }

    #[test]
    fn id_mapping_never_matches_a_variant_of_the_same_family() {
        // A shorter catalogue id must not be borrowed for a longer candidate:
        // `-flash` is a different model with its own context window.
        let ids = known(&["glm-5.3-flash"]);
        assert_eq!(map_model_id("glm-5.3", &ids), None);

        // And the reverse: a longer catalogue id must not answer for a shorter one.
        let ids = known(&["glm-5.3"]);
        assert_eq!(map_model_id("glm-5.3-flash", &ids), None);

        // Every other variant suffix is rejected the same way.
        let ids = known(&["glm-5.3-pro", "glm-5.3-max", "glm-5.3-thinking"]);
        assert_eq!(map_model_id("glm-5.3", &ids), None);
    }

    #[test]
    fn id_mapping_prefers_the_exact_name_over_a_longer_neighbour() {
        let ids = known(&["glm-5.3-flash", "glm-5.3"]);
        assert_eq!(
            map_model_id("glm-5.3", &ids),
            Some(("glm-5.3".to_string(), IdMatch::Exact))
        );
        // The qualifier form lands on the exact name too, never on the variant.
        assert_eq!(
            map_model_id("glm-5.3:free", &ids),
            Some(("glm-5.3".to_string(), IdMatch::ColonSuffix))
        );
    }

    #[test]
    fn id_mapping_reports_no_match_for_an_unrelated_id() {
        let ids = known(&["glm-5.3-flash", "deepseek-v3"]);
        assert_eq!(map_model_id("totally-different-model", &ids), None);
        assert_eq!(map_model_id("", &ids), None);
    }

    #[test]
    fn models_dev_entries_are_collected_across_providers() {
        let data = json!({
            "zai": { "models": { "glm-5.3": { "limit": { "context": 200_000 } } } },
            "deepseek": { "models": { "deepseek-v3": { "context_length": 65_536 } } }
        });
        let mut entries = collect_models_dev_entries(&data);
        entries.sort();
        assert_eq!(
            entries,
            vec![
                ("deepseek-v3".to_string(), 65_536),
                ("glm-5.3".to_string(), 200_000),
            ]
        );
    }

    #[test]
    fn models_dev_resolution_uses_the_mapping_ladder() {
        let data = json!({
            "zai": { "models": { "glm-5.3-flash": { "limit": { "context": 200_000 } } } }
        });
        // A variant of the same family is not a match: fall through to the default.
        assert_eq!(resolve_from_models_dev("glm-5.3", &data), None);
        // The vendor-namespaced form of the full name still matches.
        assert_eq!(
            resolve_from_models_dev("zai/glm-5.3-flash", &data),
            Some(("glm-5.3-flash".to_string(), IdMatch::VendorPrefix, 200_000))
        );
        assert_eq!(resolve_from_models_dev("nope-1-nothing", &data), None);
    }

    #[test]
    fn flat_models_json_entries_are_collected() {
        // `models.json` is a flat object whose keys are the namespaced ids, as
        // opposed to the provider-nested `api.json` shape.
        let data = json!({
            "zhipuai/glm-5.3-flash": {
                "id": "zhipuai/glm-5.3-flash",
                "limit": { "context": 1_000_000, "output": 131_072 }
            },
            "baai/bge-m3": { "id": "baai/bge-m3", "limit": { "context": 8_192 } }
        });
        let mut entries = collect_models_dev_entries(&data);
        entries.sort();
        assert_eq!(
            entries,
            vec![
                ("baai/bge-m3".to_string(), 8_192),
                ("zhipuai/glm-5.3-flash".to_string(), 1_000_000),
            ]
        );
    }

    #[test]
    fn real_catalogue_ids_resolve_against_the_flat_namespaced_listing() {
        // Ids and windows copied from https://models.dev/models.json so the shape
        // and the naming convention are both the real ones: flat keys, namespaced,
        // windows under `limit.context`.
        let data = json!({
            "zhipuai/glm-5.3": { "limit": { "context": 1_000_000, "output": 131_072 } },
            "zhipuai/glm-5.3-flash": { "limit": { "context": 1_000_000, "output": 131_072 } },
            "deepseek/deepseek-v4.1-flash": { "limit": { "context": 1_000_000, "output": 384_000 } },
            "alibaba/qwen3.8-flash": { "limit": { "context": 1_000_000, "output": 131_072 } },
            "moonshotai/kimi-k3": { "limit": { "context": 1_048_576, "output": 131_072 } },
            "deepseek/deepseek-v4-flash": { "limit": { "context": 1_000_000, "output": 384_000 } }
        });

        // Provider id in dashes, catalogue key in dots and namespaced.
        assert_eq!(
            resolve_from_models_dev("glm-5-3-flash", &data),
            Some((
                "zhipuai/glm-5.3-flash".to_string(),
                IdMatch::VendorPrefix,
                1_000_000
            ))
        );
        // Dot in the provider id, dash in the catalogue key, namespaced.
        assert_eq!(
            resolve_from_models_dev("glm-5.3", &data),
            Some(("zhipuai/glm-5.3".to_string(), IdMatch::VendorPrefix, 1_000_000))
        );
        assert_eq!(
            resolve_from_models_dev("deepseek-v4.1-flash", &data),
            Some((
                "deepseek/deepseek-v4.1-flash".to_string(),
                IdMatch::VendorPrefix,
                1_000_000
            ))
        );
        // A dotted id must not borrow the dotted-less sibling.
        assert_eq!(
            resolve_from_models_dev("deepseek-v4-1-flash", &data),
            Some((
                "deepseek/deepseek-v4.1-flash".to_string(),
                IdMatch::VendorPrefix,
                1_000_000
            ))
        );
        assert_eq!(
            resolve_from_models_dev("qwen3.8-flash", &data),
            Some((
                "alibaba/qwen3.8-flash".to_string(),
                IdMatch::VendorPrefix,
                1_000_000
            ))
        );
        assert_eq!(
            resolve_from_models_dev("kimi-k3", &data),
            Some((
                "moonshotai/kimi-k3".to_string(),
                IdMatch::VendorPrefix,
                1_048_576
            ))
        );
        // Nothing in the catalogue is a variant of this one, so the default stands.
        assert_eq!(resolve_from_models_dev("kimi-k9", &data), None);
    }

    #[test]
    fn endpoint_models_response_is_parsed_in_both_shapes() {
        let openai = json!({
            "data": [
                { "id": "other", "context_length": 8_192 },
                { "id": "glm-5.3", "context_length": 131_072 }
            ]
        });
        assert_eq!(
            context_window_from_models_response(&openai, "glm-5.3"),
            Some(131_072)
        );

        let bare = json!([{ "id": "glm-5.3", "context_window": 200_000 }]);
        assert_eq!(
            context_window_from_models_response(&bare, "glm-5.3"),
            Some(200_000)
        );

        // Absent id, namespace variant present: the ladder still finds it.
        let namespaced =
            json!({ "data": [{ "id": "zai/glm-5.3:cloudflare", "context_length": 99_000 }] });
        assert_eq!(
            context_window_from_models_response(&namespaced, "glm-5.3"),
            Some(99_000)
        );
    }
}
