use serde::Deserialize;

const MUSICBRAINZ_API: &str = "https://musicbrainz.org/ws/2";
const COVER_ART_ARCHIVE: &str = "https://coverartarchive.org";
const ITUNES: &str = "https://itunes.apple.com/search";
pub const USER_AGENT: &str = concat!(
    "Kopuz/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/temidaradev/kopuz)"
);

fn build_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder().user_agent(USER_AGENT).build()
}

#[derive(Debug, Deserialize)]
struct ReleaseSearchResponse {
    releases: Option<Vec<ReleaseSearchResult>>,
}

#[derive(Debug, Deserialize)]
struct ReleaseSearchResult {
    id: String,
    score: Option<u8>,
}

pub fn cover_art_url(release_mbid: &str) -> String {
    format!("{}/release/{}/front", COVER_ART_ARCHIVE, release_mbid)
}

/// A correct, public cover URL derived straight from the track's source — no
/// guessing by artist/album (which is what produced *wrong* covers). Currently
/// YouTube: the video's own thumbnail is the artwork, is public, and Discord's
/// image proxy loads it fine. Returns `None` for sources with no public cover
/// URL (local files, Jellyfin/Subsonic servers Discord can't reach), which then
/// fall back to the metadata lookup.
pub fn direct_cover_url(track_path: &str) -> Option<String> {
    if let Some(rest) = track_path.strip_prefix("ytmusic:") {
        let vid = rest.split(':').next().unwrap_or(rest);
        let valid = !vid.is_empty()
            && vid
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if valid {
            // `mqdefault` is the native 16:9 frame with NO padding. `hqdefault`
            // /`sddefault` are 4:3 and letterbox a 16:9 source with black bars
            // top and bottom — which is exactly what showed on the Discord cover.
            return Some(format!("https://i.ytimg.com/vi/{vid}/mqdefault.jpg"));
        }
    }
    None
}

fn escape_lucene(input: &str) -> String {
    let special = [
        '\\', '+', '-', '!', '(', ')', ':', '^', '[', ']', '"', '{', '}', '~', '*', '?', '|', '&',
        '/',
    ];
    let mut out = String::with_capacity(input.len() + 8);
    for ch in input.chars() {
        if special.contains(&ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

async fn search_release_mbid(
    artist: &str,
    album: &str,
) -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>> {
    if artist.is_empty() && album.is_empty() {
        return Ok(None);
    }

    let mut query_parts: Vec<String> = Vec::new();
    if !album.is_empty() {
        query_parts.push(format!("release:\"{}\"", escape_lucene(album)));
    }
    if !artist.is_empty() {
        query_parts.push(format!("artist:\"{}\"", escape_lucene(artist)));
    }
    let query = query_parts.join(" AND ");

    let client = build_client()?;
    let resp = client
        .get(format!("{}/release/", MUSICBRAINZ_API))
        .query(&[("query", query.as_str()), ("fmt", "json"), ("limit", "1")])
        .send()
        .await?;

    if !resp.status().is_success() {
        tracing::warn!("MusicBrainz search returned HTTP {}", resp.status());
        return Ok(None);
    }

    let body: ReleaseSearchResponse = resp.json().await?;
    if let Some(releases) = body.releases
        && let Some(first) = releases.first()
    {
        let score = first.score.unwrap_or(0);
        if score >= 80 {
            tracing::info!("MusicBrainz match: release={} (score={})", first.id, score);
            return Ok(Some(first.id.clone()));
        } else {
            tracing::info!(
                "MusicBrainz top result score {} too low (need >= 80)",
                score
            );
        }
    }

    Ok(None)
}

#[derive(Debug, Deserialize)]
struct ItunesSearchResponse {
    #[serde(rename = "resultCount")]
    result_count: u32,
    results: Vec<ItunesResult>,
}

#[derive(Debug, Deserialize)]
struct ItunesResult {
    #[serde(rename = "artworkUrl100")]
    artwork_url_100: Option<String>,
}

async fn resolve_via_itunes(
    artist: &str,
    album: &str,
) -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>> {
    if artist.is_empty() && album.is_empty() {
        return Ok(None);
    }

    let term = format!("{} {}", artist, album);
    let client = build_client()?;
    let resp = client
        .get(ITUNES)
        .query(&[("term", term.as_str()), ("entity", "album"), ("limit", "1")])
        .send()
        .await?;

    if !resp.status().is_success() {
        tracing::warn!("iTunes returned HTTP {}", resp.status());
        return Ok(None);
    }

    let body: ItunesSearchResponse = resp.json().await?;
    if body.result_count == 0 {
        return Ok(None);
    }

    if let Some(result) = body.results.first()
        && let Some(url) = &result.artwork_url_100
    {
        let hires = url.replace("100x100bb", "600x600bb");
        tracing::info!("iTunes match -> {}", hires);
        return Ok(Some(hires));
    }

    Ok(None)
}

/// Resolve a release's Cover Art Archive front image to a **direct** image URL.
///
/// The obvious `coverartarchive.org/release/<mbid>/front` URL is a 307 redirect
/// to the actual image on archive.org. Discord's image proxy does NOT follow
/// that redirect — it just fails to load, which is why a cover resolved via
/// MusicBrainz showed up blank while an iTunes (direct `mzstatic`) URL worked.
/// So we follow the redirect ourselves (a light HEAD; reqwest follows redirects
/// by default) and hand Discord the final direct URL it can actually fetch.
///
/// Returns `None` if the release has no front cover (404) or the lookup fails.
async fn caa_front_direct(release_mbid: &str) -> Option<String> {
    let url = cover_art_url(release_mbid);
    let client = build_client().ok()?;
    let resp = client.head(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    // The URL after redirects is the direct image on archive.org.
    Some(resp.url().to_string())
}

pub async fn resolve_cover_art_url(
    mbid: Option<&str>,
    artist: &str,
    album: &str,
) -> Option<String> {
    if let Some(id) = mbid
        && !id.is_empty()
    {
        if let Some(url) = caa_front_direct(id).await {
            tracing::info!("Resolved via embedded MBID -> {}", url);
            return Some(url);
        }
        tracing::warn!("Embedded MBID {} has no usable front cover, falling back", id);
    }

    // Without an album we'd be guessing by artist alone, which returns whatever
    // release ranks first — the "confidently wrong cover" problem. Better to show
    // no cover than the wrong one. (YouTube tracks never reach here: the caller
    // uses the video thumbnail via `direct_cover_url`.)
    if album.trim().is_empty() {
        tracing::info!(
            "No album for artist=\"{}\" — skipping cover guess to avoid a wrong match",
            artist
        );
        return None;
    }

    match search_release_mbid(artist, album).await {
        Ok(Some(release_id)) => {
            if let Some(url) = caa_front_direct(&release_id).await {
                tracing::info!("Resolved via MusicBrainz search -> {}", url);
                return Some(url);
            }
            tracing::warn!("Release {} has no usable front cover", release_id);
        }
        Ok(None) => tracing::info!(
            "No MusicBrainz match for artist=\"{}\" album=\"{}\"",
            artist,
            album
        ),
        Err(e) => tracing::warn!("MusicBrainz search failed: {}", e),
    }

    // Fallback: iTunes
    match resolve_via_itunes(artist, album).await {
        Ok(Some(url)) => return Some(url),
        Ok(None) => tracing::info!("No iTunes match"),
        Err(e) => tracing::warn!("iTunes error: {}", e),
    }

    tracing::info!(
        "All sources exhausted for artist=\"{}\" album=\"{}\"",
        artist,
        album
    );
    None
}
