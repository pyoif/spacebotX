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
//! `limit.context`). The provider-nested `api.json` is not used anywhere any more;
//! the catalogue UI reads the same flat listing and reconstructs `provider/model`
//! routing ids from the vendor segment of each key.
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

/// Key inside a nested `limit` object carrying the *input* window, as models.dev
/// publishes it.
///
/// `output` is deliberately not a candidate here. It sat in this list once, which
/// meant an entry publishing only its per-response cap was read as a context
/// window the size of that cap — an 8k window for a model whose real one is 1M.
/// The cap is read by [`output_limit_from_model_entry`], which is the only place
/// it belongs.
const LIMIT_CONTEXT_KEY: &str = "context";

/// Key inside a nested `limit` object carrying the model's per-response output
/// cap, as models.dev publishes it.
const LIMIT_OUTPUT_KEY: &str = "output";

/// Smallest value accepted as a per-response output cap.
///
/// Unlike a context window, a small cap is a real setting — `max_tokens` values
/// in the hundreds are legal — so the floor is 1 rather than the context
/// minimum.
const OUTPUT_TOKENS_MIN: usize = 1;

/// Largest value accepted as a per-response output cap. Guards against a byte
/// count, or a context window, sitting in the field by mistake.
const OUTPUT_TOKENS_MAX: usize = 1_000_000;

/// Whether a number is plausible as a context window.
pub fn is_plausible_context_window(tokens: usize) -> bool {
    (CONTEXT_WINDOW_MIN..=CONTEXT_WINDOW_MAX).contains(&tokens)
}

/// Whether a number is plausible as a per-response output cap.
pub fn is_plausible_output_limit(tokens: usize) -> bool {
    (OUTPUT_TOKENS_MIN..=OUTPUT_TOKENS_MAX).contains(&tokens)
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

    if let Some(limit) = object.get("limit").and_then(Value::as_object)
        && let Some(tokens) = limit.get(LIMIT_CONTEXT_KEY).and_then(as_tokens)
        && is_plausible_context_window(tokens)
    {
        return Some(tokens);
    }

    None
}

/// Read a per-response output cap out of one model entry.
///
/// Only the nested `limit.output` shape, which is what models.dev publishes next
/// to `limit.context`. A flat entry that happens to carry a context window does
/// not yield an invented cap: an unknown cap leaves the provider on its own
/// default, which is what this deployment did before the field was read at all.
pub fn output_limit_from_model_entry(entry: &Value) -> Option<usize> {
    let object = entry.as_object()?;
    let limit = object.get("limit").and_then(Value::as_object)?;
    let tokens = limit.get(LIMIT_OUTPUT_KEY).and_then(as_tokens)?;
    is_plausible_output_limit(tokens).then_some(tokens)
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
        let known_bare = separator_variants(strip_vendor_prefix(strip_colon_suffix(id)));
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

/// Find a per-model value in a decoded `/models` response.
///
/// Accepts either `{ "data": [ { "id": ..., ... } ] }` (OpenAI shape) or a bare
/// array, and falls back to the id-mapping ladder when the exact id is absent
/// because the endpoint advertises a namespaced variant. `pick` reads the field
/// of interest out of one entry.
///
/// Only entries that actually carry the field take part in the ladder, so a
/// listing that describes a model without a cap cannot make that model's id
/// match an unrelated entry that has one.
fn lookup_endpoint_entry<T: Copy>(
    body: &Value,
    model_id: &str,
    pick: impl Fn(&Value) -> Option<T>,
) -> Option<T> {
    let entries = body
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| body.as_array())?;

    let mut ids: Vec<String> = Vec::new();
    let mut values: Vec<(String, T)> = Vec::new();
    for entry in entries {
        let Some(id) = entry.get("id").and_then(Value::as_str) else {
            continue;
        };
        ids.push(id.to_string());
        if let Some(value) = pick(entry) {
            values.push((id.to_string(), value));
        }
    }

    if let Some((_, value)) = values
        .iter()
        .find(|(id, _)| id.eq_ignore_ascii_case(model_id))
    {
        return Some(*value);
    }

    let (matched, _) = map_model_id(model_id, &ids)?;
    values
        .iter()
        .find(|(id, _)| id == &matched)
        .map(|(_, value)| *value)
}

/// Find the context window for `model_id` in a decoded `/models` response.
pub fn context_window_from_models_response(body: &Value, model_id: &str) -> Option<usize> {
    lookup_endpoint_entry(body, model_id, context_window_from_model_entry)
}

/// Find the per-response output cap for `model_id` in a decoded `/models`
/// response. `None` when the listing does not publish one.
pub fn output_limit_from_models_response(body: &Value, model_id: &str) -> Option<usize> {
    lookup_endpoint_entry(body, model_id, output_limit_from_model_entry)
}

/// Every `(model id, value)` pair a decoded models.dev payload carries for one
/// field, read by `pick`.
///
/// The source used here, `models.json`, is a flat `{ "<namespaced id>": { … } }`
/// object, so the key *is* the id (`zhipuai/glm-5.3-flash`). The provider-nested
/// `api.json` shape — `{ "<provider>": { "models": { … } } }` — is still accepted
/// for tolerance, but nothing writes it any more.
fn collect_models_dev_values<T: Copy>(
    data: &Value,
    pick: impl Fn(&Value) -> Option<T>,
) -> Vec<(String, T)> {
    let mut found = Vec::new();
    let Some(root) = data.as_object() else {
        return found;
    };

    for (key, value) in root {
        if let Some(models) = value.get("models").and_then(Value::as_object) {
            for (id, entry) in models {
                if let Some(limit) = pick(entry) {
                    found.push((id.clone(), limit));
                }
            }
        } else if let Some(limit) = pick(value) {
            found.push((key.clone(), limit));
        }
    }

    found
}

/// Every `(model id, context window)` pair in a decoded models.dev payload.
pub fn collect_models_dev_entries(data: &Value) -> Vec<(String, usize)> {
    collect_models_dev_values(data, context_window_from_model_entry)
}

/// Resolve one field of `candidate` against a decoded models.dev payload.
///
/// Both fields share this ladder on purpose: the id mapping is what makes a
/// provider-flavoured id find its catalogue entry, and reading the output cap by
/// a different route would let one field resolve against a different entry than
/// the other.
fn resolve_field_from_models_dev<T: Copy>(
    candidate: &str,
    data: &Value,
    pick: impl Fn(&Value) -> Option<T> + Copy,
) -> Option<(String, IdMatch, T)> {
    let entries = collect_models_dev_values(data, pick);
    if entries.is_empty() {
        return None;
    }
    let ids: Vec<String> = entries.iter().map(|(id, _)| id.clone()).collect();
    let (matched, step) = map_model_id(candidate, &ids)?;
    let value = entries
        .iter()
        .find(|(id, _)| id == &matched)
        .map(|(_, value)| *value)?;
    Some((matched, step, value))
}

/// Resolve `candidate` against a decoded models.dev payload.
pub fn resolve_from_models_dev(candidate: &str, data: &Value) -> Option<(String, IdMatch, usize)> {
    resolve_field_from_models_dev(candidate, data, context_window_from_model_entry)
}

/// Resolve a model's per-response output cap against a decoded models.dev
/// payload. `None` when the catalogue publishes no cap for it.
pub fn resolve_output_limit_from_models_dev(
    candidate: &str,
    data: &Value,
) -> Option<(String, IdMatch, usize)> {
    resolve_field_from_models_dev(candidate, data, output_limit_from_model_entry)
}

/// Which metadata source discovery consults first.
///
/// Only the order is configurable; both ladders still fall through to the other
/// source for a field the first one did not publish, so a deployment whose own
/// listing is thinner than the catalogue loses nothing by leaving this alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoverySource {
    /// The configured endpoint's own `/models` listing, then models.dev.
    #[default]
    Endpoint,
    /// models.dev first, then the endpoint's listing — for a deployment whose
    /// listing under-reports what the backend will accept.
    ModelsDev,
}

impl DiscoverySource {
    /// The rungs to try, in order.
    pub fn rungs(self) -> [DiscoverySource; 2] {
        match self {
            DiscoverySource::Endpoint => [DiscoverySource::Endpoint, DiscoverySource::ModelsDev],
            DiscoverySource::ModelsDev => [DiscoverySource::ModelsDev, DiscoverySource::Endpoint],
        }
    }
}

/// Label for a value that came from the endpoint's own listing.
pub const ENDPOINT_SOURCE: &str = "endpoint /models";

/// Label for a value that came from the models.dev catalogue.
pub const MODELS_DEV_SOURCE: &str = "models.dev";

/// What one model will accept: its input window and its per-response output cap.
///
/// The two travel together because they come from the same entry, and because a
/// deployment that published one but not the other is exactly the case the
/// per-field fallthrough exists for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModelLimits {
    /// Input window, with the source that supplied it.
    pub context: Option<(usize, &'static str)>,
    /// Per-response output cap, with the source that supplied it.
    pub output: Option<(usize, &'static str)>,
}

impl ModelLimits {
    /// Whether both fields are known, so no further source can add anything.
    pub fn is_complete(&self) -> bool {
        self.context.is_some() && self.output.is_some()
    }

    /// Fold one rung's answer in. The first source to answer a field wins it.
    ///
    /// Per field rather than per source, deliberately: an endpoint that publishes
    /// windows but no output cap has answered half the question, and the
    /// catalogue should fill the other half instead of the pair being taken
    /// wholesale from whichever source spoke first.
    fn fold(
        &mut self,
        rung: DiscoverySource,
        endpoint_body: Option<&Value>,
        models_dev_data: Option<&Value>,
        model_id: &str,
    ) {
        match rung {
            DiscoverySource::Endpoint => {
                let Some(body) = endpoint_body else { return };
                if self.context.is_none() {
                    self.context = context_window_from_models_response(body, model_id)
                        .map(|window| (window, ENDPOINT_SOURCE));
                }
                if self.output.is_none() {
                    self.output = output_limit_from_models_response(body, model_id)
                        .map(|output| (output, ENDPOINT_SOURCE));
                }
            }
            DiscoverySource::ModelsDev => {
                let Some(data) = models_dev_data else { return };
                if self.context.is_none() {
                    self.context = resolve_from_models_dev(model_id, data)
                        .map(|(_, _, window)| (window, MODELS_DEV_SOURCE));
                }
                if self.output.is_none() {
                    self.output = resolve_output_limit_from_models_dev(model_id, data)
                        .map(|(_, _, output)| (output, MODELS_DEV_SOURCE));
                }
            }
        }
    }
}

/// Fetch and decode an endpoint's `/models` listing.
///
/// Returns `None` on any failure — transport, status or parse — since this is an
/// advisory lookup that must never hold a completion request open.
async fn fetch_models_listing(
    client: &reqwest::Client,
    models_url: &str,
    api_key: Option<&str>,
) -> Option<Value> {
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
            "models listing did not answer, skipping discovery"
        );
        return None;
    }

    response.json::<Value>().await.ok()
}

/// Resolve a model's window and output cap, consulting `rungs` in order.
///
/// Each source is fetched at most once, and only when a field is still unknown,
/// so an endpoint that answers both never triggers the models.dev fetch — the
/// common case on a warm start. Every failure is soft: a source that cannot
/// answer leaves the fields it did not supply untouched, and the caller keeps
/// whatever it already had.
pub async fn resolve_model_limits(
    client: &reqwest::Client,
    models_url: &str,
    api_key: Option<&str>,
    models_dev_cache_path: &Path,
    model_id: &str,
    rungs: &[DiscoverySource],
) -> ModelLimits {
    let mut limits = ModelLimits::default();
    let mut endpoint_body: Option<Value> = None;
    let mut models_dev_data: Option<Value> = None;

    for rung in rungs {
        if limits.is_complete() {
            break;
        }
        let (endpoint, models_dev) = match rung {
            DiscoverySource::Endpoint => {
                if endpoint_body.is_none() {
                    endpoint_body = fetch_models_listing(client, models_url, api_key).await;
                }
                (endpoint_body.as_ref(), None)
            }
            DiscoverySource::ModelsDev => {
                if models_dev_data.is_none() {
                    models_dev_data = load_models_dev(client, models_dev_cache_path).await;
                }
                (None, models_dev_data.as_ref())
            }
        };
        limits.fold(*rung, endpoint, models_dev, model_id);
    }

    limits
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
        assert_eq!(
            unique.len(),
            variants.len(),
            "variants must be deduplicated"
        );
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
            Some((
                "zhipuai/glm-5.3".to_string(),
                IdMatch::VendorPrefix,
                1_000_000
            ))
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

    #[test]
    fn limit_output_is_never_read_as_a_context_window() {
        // Regression: `output` sat in the `limit` fallback list next to
        // `context`, so an entry publishing only its output cap answered with
        // that cap as the model's *window* — an 8k context for a 1M-token model.
        assert_eq!(
            context_window_from_model_entry(&json!({ "limit": { "output": 8_192 } })),
            None
        );
        // The window next to it is still read as the window.
        assert_eq!(
            context_window_from_model_entry(
                &json!({ "limit": { "context": 200_000, "output": 8_192 } })
            ),
            Some(200_000)
        );
    }

    #[test]
    fn output_limit_is_read_from_the_nested_limit_object() {
        // The real catalogue shape: both fields side by side.
        assert_eq!(
            output_limit_from_model_entry(
                &json!({ "limit": { "context": 1_000_000, "output": 131_072 } })
            ),
            Some(131_072)
        );
        // Numeric strings are accepted here too.
        assert_eq!(
            output_limit_from_model_entry(&json!({ "limit": { "output": "384000" } })),
            Some(384_000)
        );
        // A flat entry publishes no cap, and none is invented from its window.
        assert_eq!(
            output_limit_from_model_entry(&json!({ "context_window": 200_000 })),
            None
        );
        // Implausible: a byte count.
        assert_eq!(
            output_limit_from_model_entry(&json!({ "limit": { "output": 40_000_000 } })),
            None
        );
        assert_eq!(
            output_limit_from_model_entry(&json!({ "id": "glm-5.3" })),
            None
        );
    }

    #[test]
    fn endpoint_models_response_carries_the_output_cap() {
        let body = json!({
            "data": [
                { "id": "other", "limit": { "output": 4_096 } },
                { "id": "glm-5.3", "limit": { "context": 262_144, "output": 131_072 } }
            ]
        });
        assert_eq!(
            context_window_from_models_response(&body, "glm-5.3"),
            Some(262_144)
        );
        assert_eq!(
            output_limit_from_models_response(&body, "glm-5.3"),
            Some(131_072)
        );

        // The ladder reaches a namespaced variant for the cap as well.
        let namespaced =
            json!({ "data": [{ "id": "zai/glm-5.3:cloudflare", "limit": { "output": 65_536 } }] });
        assert_eq!(
            output_limit_from_models_response(&namespaced, "glm-5.3"),
            Some(65_536)
        );

        // A listing that publishes only windows yields no cap.
        let windows_only = json!({ "data": [{ "id": "glm-5.3", "context_length": 262_144 }] });
        assert_eq!(
            output_limit_from_models_response(&windows_only, "glm-5.3"),
            None
        );
    }

    #[test]
    fn models_dev_publishes_an_output_cap_next_to_the_window() {
        // Values copied from models.dev/models.json.
        let data = json!({
            "deepseek/deepseek-v4.1-flash": { "limit": { "context": 1_000_000, "output": 384_000 } }
        });
        assert_eq!(
            resolve_output_limit_from_models_dev("deepseek-v4.1-flash", &data),
            Some((
                "deepseek/deepseek-v4.1-flash".to_string(),
                IdMatch::VendorPrefix,
                384_000
            ))
        );
        // The window resolution beside it is unchanged.
        assert_eq!(
            resolve_from_models_dev("deepseek-v4.1-flash", &data),
            Some((
                "deepseek/deepseek-v4.1-flash".to_string(),
                IdMatch::VendorPrefix,
                1_000_000
            ))
        );
        // A model the catalogue knows only as a window has no cap.
        let window_only = json!({ "vendor/m": { "limit": { "context": 200_000 } } });
        assert_eq!(
            resolve_output_limit_from_models_dev("m", &window_only),
            None
        );
    }

    #[test]
    fn the_ladder_takes_the_first_source_that_answers_each_field() {
        let endpoint = json!({ "data": [{ "id": "m", "context_window": 262_144 }] });
        let models_dev = json!({
            "vendor/m": { "limit": { "context": 1_000_000, "output": 384_000 } }
        });

        // Endpoint first: its window wins, and the catalogue fills the cap the
        // endpoint did not publish.
        let mut limits = ModelLimits::default();
        for rung in DiscoverySource::Endpoint.rungs() {
            limits.fold(rung, Some(&endpoint), Some(&models_dev), "m");
        }
        assert_eq!(limits.context, Some((262_144, ENDPOINT_SOURCE)));
        assert_eq!(limits.output, Some((384_000, MODELS_DEV_SOURCE)));

        // models.dev first: it answers both, so the endpoint's window is not used.
        let mut limits = ModelLimits::default();
        for rung in DiscoverySource::ModelsDev.rungs() {
            limits.fold(rung, Some(&endpoint), Some(&models_dev), "m");
        }
        assert_eq!(limits.context, Some((1_000_000, MODELS_DEV_SOURCE)));
        assert_eq!(limits.output, Some((384_000, MODELS_DEV_SOURCE)));

        // A source that knows nothing leaves the field unknown rather than
        // falling back to a guessed number.
        let mut limits = ModelLimits::default();
        for rung in DiscoverySource::Endpoint.rungs() {
            limits.fold(rung, Some(&json!({ "data": [] })), None, "m");
        }
        assert_eq!(limits, ModelLimits::default());
    }
}
