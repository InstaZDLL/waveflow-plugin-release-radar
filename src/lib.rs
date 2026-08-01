//! Release Radar — a WaveFlow `waveflow:ui/v1` plugin.
//!
//! Surfaces recent releases from the artists in the user's library.
//! It reads the redacted artist list the host exposes
//! (`waveflow:host/library.list-artists` — names + counts + opaque
//! ids, no file paths), searches **MusicBrainz** for each artist's
//! recent release-groups, and renders them as a JSON *view descriptor*
//! the host draws with native components. Covers come from the
//! **Cover Art Archive** and each card links out to MusicBrainz. There
//! is no playback and no YouTube — this is a pure discovery view.
//!
//! ## Why the scan is incremental
//!
//! The host caps a single guest call at ~30 s of wall-clock (the
//! wasmtime epoch deadline) and MusicBrainz asks clients to average
//! ≤ 1 request/second. So a "scan" processes a bounded batch of
//! artists (`BATCH`), spacing requests by `SPACING_MS`, backing off
//! cleanly on a `503`/`429` (never hammering — that's what gets a
//! client blocked), and persisting a cursor + the resolved releases in
//! the plugin's scratch store. Re-opening the view is then instant, and
//! "Continue" picks up where the last batch stopped.

#[allow(warnings)]
mod bindings;

use bindings::exports::waveflow::ui::extension::{Guest, MountPoint};
use bindings::waveflow::host::log::{self, Level};
use bindings::waveflow::host::{http, library, storage};

use serde::{Deserialize, Serialize};

// ----- tunables -----------------------------------------------------------

/// Scratch-store key for the cached release list + scan cursor. The
/// `/v1` suffix lets a future format change invalidate old caches by
/// bumping the key instead of migrating in place.
const CACHE_KEY: &str = "release-radar/cache/v1";

/// MusicBrainz requires a descriptive User-Agent identifying the app +
/// a contact; an anonymous `reqwest/x` UA is answered with `403`.
const USER_AGENT: &str =
    "WaveFlow-ReleaseRadar/0.1.0 ( https://github.com/InstaZDLL/waveflow-plugin-release-radar )";

/// How many of the library's top artists (by track count — the host
/// orders the snapshot) the radar follows. Bounded so a huge library
/// doesn't imply thousands of MusicBrainz calls.
const ARTIST_SCAN_LIMIT: u32 = 60;

/// Artists processed per "scan"/"continue" click. Kept small so one
/// call stays well under the host's epoch deadline even on a slow link.
const BATCH: usize = 8;

/// A release is "recent" if its first-release date is within this many
/// days of now.
const RECENT_DAYS: i64 = 180;

/// Stop a batch early if it has already run this long, so a slow
/// network can't push a single call past the host's ~30 s deadline.
const WALL_GUARD_SECS: i64 = 22;

/// Spacing between MusicBrainz requests (their guidance is ≤ 1 req/s).
const SPACING_MS: u64 = 1100;

/// Cap on the number of releases kept in the cache — newest first.
const MAX_RELEASES: usize = 150;

// ----- persisted state ----------------------------------------------------

/// Everything the plugin remembers between calls, stored as JSON in the
/// scratch store under [`CACHE_KEY`].
#[derive(Serialize, Deserialize, Default)]
struct Cache {
    /// Resolved releases, deduped by release-group id, newest first.
    releases: Vec<Release>,
    /// Index into the artist list the next batch resumes from.
    next_index: usize,
    /// Total artists in the current scan pass (snapshot of the list
    /// length), so the UI can show `scanned / total`.
    total: usize,
    /// Epoch seconds of the last completed batch; `0` = never scanned.
    updated_at: i64,
}

/// One recent release-group.
#[derive(Serialize, Deserialize, Clone)]
struct Release {
    /// MusicBrainz release-group MBID — the id for the cover + the link.
    id: String,
    title: String,
    artist: String,
    /// First-release date as MusicBrainz reports it: `YYYY-MM-DD`,
    /// `YYYY-MM`, or `YYYY`.
    date: String,
    /// `Album` / `EP` / `Single` / … — shown as a badge.
    primary_type: String,
}

// ----- MusicBrainz response shapes ----------------------------------------

#[derive(Deserialize)]
struct MbResp {
    #[serde(rename = "release-groups", default)]
    release_groups: Vec<RgRaw>,
}

#[derive(Deserialize)]
struct RgRaw {
    id: String,
    #[serde(default)]
    title: String,
    #[serde(rename = "first-release-date", default)]
    first_release_date: String,
    #[serde(rename = "primary-type")]
    primary_type: Option<String>,
    #[serde(rename = "artist-credit", default)]
    artist_credit: Vec<AcRaw>,
}

#[derive(Deserialize)]
struct AcRaw {
    #[serde(default)]
    name: String,
}

// ----- view descriptor (host contract) ------------------------------------
//
// serde `camelCase` produces the exact field names the host's
// `parsePluginUiDescriptor` validates (schemaVersion, imageUrl,
// emptyTitle, …). Optional strings + arrays are OMITTED when
// empty/absent (never serialized as `null`) — the host treats a `null`
// array as present-but-malformed and rejects the whole descriptor.

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Descriptor {
    schema_version: u32,
    title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    subtitle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    actions: Vec<Action>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    sections: Vec<Section>,
    #[serde(skip_serializing_if = "Option::is_none")]
    empty_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    empty_hint: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Section {
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    items: Vec<Item>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Item {
    id: String,
    title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    subtitle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    image_url: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    badges: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    actions: Vec<Action>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Action {
    kind: &'static str,
    label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    event: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
}

impl Action {
    fn event(label: impl Into<String>, event: &str) -> Self {
        Action {
            kind: "event",
            label: label.into(),
            event: Some(event.to_string()),
            payload: None,
            url: None,
        }
    }

    fn open_url(label: impl Into<String>, url: String) -> Self {
        Action {
            kind: "open-url",
            label: label.into(),
            event: None,
            payload: None,
            url: Some(url),
        }
    }
}

// ----- host helpers -------------------------------------------------------

fn read_cache() -> Cache {
    match storage::read_state(CACHE_KEY) {
        Ok(Some(bytes)) => serde_json::from_slice(&bytes).unwrap_or_default(),
        _ => Cache::default(),
    }
}

fn write_cache(cache: &Cache) {
    match serde_json::to_vec(cache) {
        Ok(bytes) => {
            if let Err(e) = storage::write_state(CACHE_KEY, &bytes) {
                log::emit(Level::Warn, &format!("release-radar: cache write failed: {e}"));
            }
        }
        Err(e) => log::emit(Level::Warn, &format!("release-radar: cache encode failed: {e}")),
    }
}

fn mb_get(url: &str) -> Result<http::Response, String> {
    let req = http::Request {
        method: "GET".to_string(),
        url: url.to_string(),
        headers: vec![
            ("User-Agent".to_string(), USER_AGENT.to_string()),
            ("Accept".to_string(), "application/json".to_string()),
        ],
        body: None,
    };
    http::send(&req)
}

// ----- date + query utilities ---------------------------------------------

fn now_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Civil date (year, month, day) from a day number relative to the
/// Unix epoch. Howard Hinnant's `civil_from_days` — no `chrono`
/// dependency in the wasm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (y + if m <= 2 { 1 } else { 0 }, m, d)
}

/// `YYYY-MM-DD` for `days_back` days before today (UTC).
fn cutoff_date(days_back: i64) -> String {
    let z = (now_secs() - days_back * 86_400).div_euclid(86_400);
    let (y, m, d) = civil_from_days(z);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Normalize a partial MusicBrainz date to a comparable `YYYY-MM-DD`
/// lower bound so string comparison against the cutoff is sound
/// (`"2026"` → `"2026-01-01"`).
fn date_key(d: &str) -> String {
    match d.len() {
        4 => format!("{d}-01-01"),
        7 => format!("{d}-01"),
        _ => d.to_string(),
    }
}

fn normalize(s: &str) -> String {
    s.trim().to_lowercase()
}

/// Percent-encode a Lucene query for the `query=` URL parameter. The
/// host's HTTP client sends the URL verbatim, so anything outside the
/// unreserved set is escaped; MusicBrainz decodes it back before
/// parsing the Lucene syntax.
fn encode_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Escape a value going inside a Lucene quoted phrase (`"..."`).
fn escape_lucene_phrase(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn build_mb_url(artist: &str, cutoff: &str) -> String {
    let lucene = format!(
        "artist:\"{}\" AND firstreleasedate:[{} TO *]",
        escape_lucene_phrase(artist),
        cutoff
    );
    format!(
        "https://musicbrainz.org/ws/2/release-group?query={}&fmt=json&limit=25",
        encode_query(&lucene)
    )
}

fn caa_url(rg_id: &str) -> String {
    format!("https://coverartarchive.org/release-group/{rg_id}/front-250")
}

fn mb_rg_url(rg_id: &str) -> String {
    format!("https://musicbrainz.org/release-group/{rg_id}")
}

// ----- scan ---------------------------------------------------------------

fn parse_release_groups(body: &[u8], artist_name: &str, cutoff: &str) -> Vec<Release> {
    let resp: MbResp = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let want = normalize(artist_name);
    let mut out = Vec::new();
    for rg in resp.release_groups {
        if rg.first_release_date.is_empty() {
            continue;
        }
        if date_key(&rg.first_release_date).as_str() < cutoff {
            continue;
        }
        // The query is an `artist:"name"` phrase search, but MusicBrainz
        // can return homonyms — require a normalized credit match.
        if !rg.artist_credit.iter().any(|ac| normalize(&ac.name) == want) {
            continue;
        }
        let artist_display = rg
            .artist_credit
            .first()
            .map(|a| a.name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| artist_name.to_string());
        out.push(Release {
            id: rg.id,
            title: rg.title,
            artist: artist_display,
            date: rg.first_release_date,
            primary_type: rg.primary_type.unwrap_or_default(),
        });
    }
    out
}

fn merge_releases(existing: &mut Vec<Release>, found: Vec<Release>) {
    for r in found {
        if !existing.iter().any(|e| e.id == r.id) {
            existing.push(r);
        }
    }
}

fn sort_and_cap(releases: &mut Vec<Release>) {
    releases.sort_by(|a, b| date_key(&b.date).cmp(&date_key(&a.date)));
    releases.truncate(MAX_RELEASES);
}

/// Process one batch of artists. `reset` clears the cursor + results
/// first (a full rescan). Returns the rendered next view.
fn do_scan(reset: bool) -> String {
    let mut cache = read_cache();
    if reset {
        cache.releases.clear();
        cache.next_index = 0;
    }

    let artists = match library::list_artists(ARTIST_SCAN_LIMIT) {
        Ok(a) => a,
        Err(e) => return render_view(&cache, &format!("error: {e}")),
    };
    cache.total = artists.len();

    if artists.is_empty() {
        cache.updated_at = now_secs();
        write_cache(&cache);
        return render_view(&cache, "fresh");
    }
    if cache.next_index >= artists.len() {
        return render_view(&cache, "cached");
    }

    let cutoff = cutoff_date(RECENT_DAYS);
    let start = cache.next_index;
    let end = (start + BATCH).min(artists.len());
    let batch_start = now_secs();
    let mut status = "fresh";
    let mut i = start;

    while i < end {
        if now_secs() - batch_start > WALL_GUARD_SECS {
            status = "partial";
            break;
        }
        // Space requests to respect MusicBrainz's ≤ 1 req/s guidance;
        // no sleep before the first request of the batch.
        if i > start {
            std::thread::sleep(std::time::Duration::from_millis(SPACING_MS));
        }
        let artist = &artists[i];
        match mb_get(&build_mb_url(&artist.name, &cutoff)) {
            Ok(resp) if resp.status == 200 => {
                let found = parse_release_groups(&resp.body, &artist.name, &cutoff);
                merge_releases(&mut cache.releases, found);
            }
            Ok(resp) if resp.status == 503 || resp.status == 429 => {
                // Rate-limited: stop cleanly and leave the cursor on this
                // artist so "Continue" retries it. Never hammer through it.
                status = "rate-limited";
                break;
            }
            Ok(resp) => {
                log::emit(
                    Level::Warn,
                    &format!("release-radar: MB {} for {}", resp.status, artist.name),
                );
            }
            Err(e) => log::emit(Level::Warn, &format!("release-radar: fetch failed: {e}")),
        }
        i += 1;
    }

    cache.next_index = i;
    cache.updated_at = now_secs();
    sort_and_cap(&mut cache.releases);
    write_cache(&cache);
    render_view(&cache, status)
}

// ----- rendering ----------------------------------------------------------

fn render_view(cache: &Cache, status: &str) -> String {
    let scanned = cache.next_index.min(cache.total);
    let remaining = cache.total.saturating_sub(cache.next_index);
    let never_scanned = cache.updated_at == 0 && cache.next_index == 0;

    let mut actions = Vec::new();
    if never_scanned {
        actions.push(Action::event("Rechercher les nouveautés", "scan"));
    } else if remaining > 0 {
        actions.push(Action::event(format!("Continuer ({remaining} restants)"), "scan"));
        actions.push(Action::event("Tout rescanner", "rescan"));
    } else {
        actions.push(Action::event("Rescanner", "rescan"));
    }

    let items: Vec<Item> = cache
        .releases
        .iter()
        .map(|r| Item {
            id: r.id.clone(),
            title: r.title.clone(),
            subtitle: Some(r.artist.clone()),
            detail: Some(r.date.clone()),
            image_url: Some(caa_url(&r.id)),
            badges: if r.primary_type.is_empty() {
                Vec::new()
            } else {
                vec![r.primary_type.clone()]
            },
            actions: vec![Action::open_url("MusicBrainz", mb_rg_url(&r.id))],
        })
        .collect();

    let subtitle = if cache.total > 0 {
        Some(format!(
            "{} nouveauté(s) · {scanned}/{} artistes",
            cache.releases.len(),
            cache.total
        ))
    } else {
        Some("Découvrez les dernières sorties de vos artistes".to_string())
    };

    let (empty_title, empty_hint) = if !items.is_empty() {
        (None, None)
    } else if never_scanned {
        (
            Some("Bienvenue dans Release Radar".to_string()),
            Some(
                "Lancez une recherche pour découvrir les sorties récentes de vos artistes suivis."
                    .to_string(),
            ),
        )
    } else {
        (
            Some("Aucune nouveauté".to_string()),
            Some("Aucune sortie récente trouvée pour vos artistes sur MusicBrainz.".to_string()),
        )
    };

    let sections = if items.is_empty() {
        Vec::new()
    } else {
        vec![Section {
            title: Some("Sorties récentes".to_string()),
            items,
        }]
    };

    let descriptor = Descriptor {
        schema_version: 1,
        title: "Release Radar".to_string(),
        subtitle,
        status: Some(status.to_string()),
        actions,
        sections,
        empty_title,
        empty_hint,
    };

    serde_json::to_string(&descriptor).unwrap_or_else(|_| {
        // A serialize failure is essentially impossible for this fixed
        // shape, but the return type demands a valid descriptor either
        // way — hand back a minimal well-formed one rather than trap.
        "{\"schemaVersion\":1,\"title\":\"Release Radar\",\"status\":\"error\",\"emptyTitle\":\"Erreur\"}"
            .to_string()
    })
}

// ----- guest exports ------------------------------------------------------

struct ReleaseRadar;

impl Guest for ReleaseRadar {
    fn manifest() -> MountPoint {
        MountPoint {
            sidebar_label: "Release Radar".to_string(),
            sidebar_icon: Some("radar".to_string()),
            initial_path: "/".to_string(),
        }
    }

    fn render(_path: String) -> Result<String, String> {
        let cache = read_cache();
        let status = if cache.updated_at == 0 { "fresh" } else { "cached" };
        Ok(render_view(&cache, status))
    }

    fn on_event(event: String, _payload: String) -> Result<String, String> {
        match event.as_str() {
            "scan" => Ok(do_scan(false)),
            "rescan" => Ok(do_scan(true)),
            _ => Ok(render_view(&read_cache(), "cached")),
        }
    }
}

bindings::export!(ReleaseRadar with_types_in bindings);
