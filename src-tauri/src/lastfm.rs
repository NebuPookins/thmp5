use crate::models::LastFmLovedTrack;
use md5::{Digest, Md5};
use reqwest::Client;
use std::sync::OnceLock;
use tracing::{debug, info, warn};

const API_ROOT: &str = "https://ws.audioscrobbler.com/2.0/";

/// App-level Last.fm credentials loaded from environment variables.
pub struct LastFmConfig {
    pub api_key: String,
    pub shared_secret: String,
}

fn http_client() -> &'static Client {
    static HTTP_CLIENT: OnceLock<Client> = OnceLock::new();
    HTTP_CLIENT.get_or_init(|| {
        Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("failed to build shared Last.fm HTTP client")
    })
}

/// Generate an api_sig per the Last.fm signing spec:
/// Sort params alphabetically by name, concatenate as name+value for each,
/// append the shared secret, then take the MD5 hex digest.
pub fn generate_api_sig(params: &[(&str, &str)], secret: &str) -> String {
    let mut sorted: Vec<&&str> = params.iter().map(|(k, _)| k).collect();
    sorted.sort_unstable();

    let mut buf = String::new();
    for key in &sorted {
        let value = params.iter().find(|(k, _)| k == *key).unwrap().1;
        buf.push_str(key);
        buf.push_str(value);
    }
    let hash = Md5::digest(format!("{buf}{secret}").as_bytes());
    hex::encode(hash)
}

async fn api_call(
    mut params: Vec<(&str, &str)>,
    config: &LastFmConfig,
) -> Result<serde_json::Value, String> {
    params.push(("api_key", &config.api_key));

    let method = params
        .iter()
        .find(|(k, _)| *k == "method")
        .map(|(_, v)| *v)
        .unwrap_or("unknown");

    let sig = generate_api_sig(&params, &config.shared_secret);
    params.push(("api_sig", &sig));
    params.push(("format", "json"));

    debug!(method, "Calling Last.fm API");

    let resp = http_client()
        .post(API_ROOT)
        .form(&params)
        .send()
        .await
        .map_err(|e| {
            warn!(method, "Last.fm request failed: {e}");
            format!("Last.fm request failed: {e}")
        })?;

    let body: serde_json::Value = resp.json().await.map_err(|e| {
        warn!(method, "Failed to parse Last.fm response: {e}");
        format!("Failed to parse Last.fm response: {e}")
    })?;

    // Last.fm returns {"error": N, "message": "..."} on failure
    if let Some(err_code) = body.get("error").and_then(|v| v.as_i64()) {
        if err_code != 0 {
            let msg = body
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            warn!(method, err_code, "Last.fm API error: {msg}");
            return Err(format!("Last.fm API error ({err_code}): {msg}"));
        }
    }

    debug!(method, "Last.fm API call succeeded");
    Ok(body)
}

/// Call auth.getToken – returns a temporary token for the authorization flow.
pub async fn get_token(config: &LastFmConfig) -> Result<String, String> {
    info!("Last.fm: requesting auth token");
    let body = api_call(vec![("method", "auth.getToken")], config).await?;
    let token = body
        .get("token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            warn!("Last.fm did not return a token in auth.getToken response");
            "Last.fm did not return a token".to_string()
        })?;
    info!("Last.fm: obtained auth token successfully");
    Ok(token)
}

/// Call auth.getSession – exchanges an authorized token for a session key.
/// Returns (session_key, username).
pub async fn get_session(config: &LastFmConfig, token: &str) -> Result<(String, String), String> {
    info!(token = %token.chars().take(8).collect::<String>(), "Last.fm: exchanging auth token for session");
    let body = api_call(
        vec![("method", "auth.getSession"), ("token", token)],
        config,
    )
    .await?;

    let session = body.get("session").ok_or_else(|| {
        warn!("Last.fm: response missing 'session' object");
        "Last.fm did not return a session object".to_string()
    })?;

    let key = session
        .get("key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            warn!("Last.fm: session object missing 'key'");
            "Last.fm session missing 'key'".to_string()
        })?
        .to_string();

    let name = session
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            warn!("Last.fm: session object missing 'name'");
            "Last.fm session missing 'name'".to_string()
        })?
        .to_string();

    info!(username = %name, "Last.fm: session obtained successfully");
    Ok((key, name))
}

/// Call track.updateNowPlaying.
pub async fn now_playing(
    config: &LastFmConfig,
    session_key: &str,
    artist: &str,
    track: &str,
    album: Option<&str>,
) -> Result<(), String> {
    info!(artist, track, "Last.fm: updating now playing");
    let mut params = vec![
        ("method", "track.updateNowPlaying"),
        ("sk", session_key),
        ("artist", artist),
        ("track", track),
    ];
    if let Some(album) = album {
        info!(album, "Last.fm: with album");
        params.push(("album", album));
    }
    match api_call(params, config).await {
        Ok(_) => {
            info!("Last.fm: now playing updated successfully");
            Ok(())
        }
        Err(e) => {
            warn!(artist, track, "Last.fm: now playing failed: {e}");
            Err(e)
        }
    }
}

/// Call track.scrobble.
/// `timestamp` is a UNIX timestamp (seconds) when the track started playing.
pub async fn scrobble(
    config: &LastFmConfig,
    session_key: &str,
    artist: &str,
    track: &str,
    album: Option<&str>,
    timestamp: i64,
) -> Result<(), String> {
    info!(artist, track, timestamp, "Last.fm: scrobbling track");
    let timestamp_str = timestamp.to_string();
    let mut params = vec![
        ("method", "track.scrobble"),
        ("sk", session_key),
        ("artist", artist),
        ("track", track),
        ("timestamp", &timestamp_str),
    ];
    if let Some(album) = album {
        params.push(("album", album));
    }
    match api_call(params, config).await {
        Ok(_) => {
            info!("Last.fm: scrobble successful");
            Ok(())
        }
        Err(e) => {
            warn!(artist, track, "Last.fm: scrobble failed: {e}");
            Err(e)
        }
    }
}

/// Call track.getInfo and return whether the current user has loved the track.
pub async fn get_track_loved(
    config: &LastFmConfig,
    session_key: &str,
    artist: &str,
    track: &str,
) -> Result<bool, String> {
    let params = vec![
        ("method", "track.getInfo"),
        ("sk", session_key),
        ("artist", artist),
        ("track", track),
    ];
    let body = api_call(params, config).await?;
    let loved = parse_user_loved(&body).inspect_err(|e| {
        warn!(
            artist,
            track, "Last.fm: unusable track.getInfo response: {e}"
        );
    })?;
    info!(artist, track, loved, "Last.fm: checked track loved status");
    Ok(loved)
}

/// Read `track.userloved` from a track.getInfo response. Anything other than
/// "1" or "0" is an error: a missing field means "unknown", not "not loved".
fn parse_user_loved(body: &serde_json::Value) -> Result<bool, String> {
    match body.get("track").and_then(|t| t.get("userloved")) {
        Some(serde_json::Value::String(s)) if s == "1" => Ok(true),
        Some(serde_json::Value::String(s)) if s == "0" => Ok(false),
        Some(other) => Err(format!("Unexpected track.userloved value: {other}")),
        None => Err("track.getInfo response has no track.userloved".to_string()),
    }
}

/// Which loved-state change to apply. Pairs the API method name with its log
/// verb so the two can never be set inconsistently with each other.
#[derive(Clone, Copy)]
enum LovedAction {
    Love,
    Unlove,
}

impl LovedAction {
    fn method(self) -> &'static str {
        match self {
            LovedAction::Love => "track.love",
            LovedAction::Unlove => "track.unlove",
        }
    }

    fn verb(self) -> &'static str {
        match self {
            LovedAction::Love => "love",
            LovedAction::Unlove => "unlove",
        }
    }
}

/// Call track.love or track.unlove, sharing the request/response handling
/// that differs between the two only by API method name and log wording.
async fn set_track_loved(
    config: &LastFmConfig,
    session_key: &str,
    action: LovedAction,
    artist: &str,
    track: &str,
) -> Result<(), String> {
    let params = vec![
        ("method", action.method()),
        ("sk", session_key),
        ("artist", artist),
        ("track", track),
    ];
    match api_call(params, config).await {
        Ok(_) => {
            let verb = action.verb();
            info!("Last.fm: track {verb}d successfully");
            Ok(())
        }
        Err(e) => {
            let verb = action.verb();
            warn!(artist, track, "Last.fm: {verb} track failed: {e}");
            Err(e)
        }
    }
}

/// Call track.love.
pub async fn love_track(
    config: &LastFmConfig,
    session_key: &str,
    artist: &str,
    track: &str,
) -> Result<(), String> {
    info!(artist, track, "Last.fm: loving track");
    set_track_loved(config, session_key, LovedAction::Love, artist, track).await
}

/// Call track.unlove.
pub async fn unlove_track(
    config: &LastFmConfig,
    session_key: &str,
    artist: &str,
    track: &str,
) -> Result<(), String> {
    info!(artist, track, "Last.fm: unloving track");
    set_track_loved(config, session_key, LovedAction::Unlove, artist, track).await
}

/// One page of a user's loved tracks, as parsed from `user.getLovedTracks`.
#[derive(Debug, PartialEq)]
struct LovedTracksPage {
    pub tracks: Vec<LastFmLovedTrack>,
    pub total_pages: u32,
}

/// `@attr` numbers arrive as JSON strings, but tolerate real numbers too.
fn json_u32(value: &serde_json::Value) -> Option<u32> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
        .and_then(|n| u32::try_from(n).ok())
}

/// Parse a `user.getLovedTracks` response. A response without `lovedtracks` is
/// an error rather than an empty page, so a malformed reply never silently
/// yields "no hearts". `track` is an array normally, a bare object when there
/// is exactly one result, and absent or empty when there are none.
fn parse_loved_tracks_page(body: &serde_json::Value) -> Result<LovedTracksPage, String> {
    let loved = body
        .get("lovedtracks")
        .filter(|v| v.is_object())
        .ok_or("Last.fm response missing 'lovedtracks' object")?;

    let entries: Vec<&serde_json::Value> = match loved.get("track") {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(serde_json::Value::Array(items)) => items.iter().collect(),
        Some(obj @ serde_json::Value::Object(_)) => vec![obj],
        // Last.fm sometimes serialises an empty XML element as "".
        Some(serde_json::Value::String(s)) if s.is_empty() => Vec::new(),
        Some(other) => return Err(format!("Unexpected 'track' value in loved tracks: {other}")),
    };

    let tracks = entries
        .into_iter()
        .map(|entry| {
            let track = entry.get("name").and_then(|v| v.as_str());
            let artist = entry
                .get("artist")
                .and_then(|a| a.get("name"))
                .and_then(|v| v.as_str());
            match (artist, track) {
                (Some(artist), Some(track)) => Ok(LastFmLovedTrack {
                    artist: artist.to_string(),
                    track: track.to_string(),
                }),
                _ => Err(format!("Malformed loved track entry: {entry}")),
            }
        })
        .collect::<Result<Vec<_>, String>>()?;

    // Without @attr (single page responses may omit it), assume one page.
    let total_pages = loved
        .get("@attr")
        .and_then(|a| a.get("totalPages"))
        .map(|v| json_u32(v).ok_or_else(|| format!("Invalid totalPages in loved tracks: {v}")))
        .transpose()?
        .unwrap_or(1);

    Ok(LovedTracksPage {
        tracks,
        total_pages,
    })
}

/// Fetch and parse one page of `user.getLovedTracks`.
async fn fetch_loved_tracks_page(
    config: &LastFmConfig,
    username: &str,
    page: u32,
) -> Result<LovedTracksPage, String> {
    let page_str = page.to_string();
    let params = vec![
        ("method", "user.getLovedTracks"),
        ("user", username),
        ("limit", "1000"),
        ("page", &page_str),
    ];
    api_call(params, config)
        .await
        .and_then(|body| parse_loved_tracks_page(&body))
        .inspect_err(|e| warn!(username, page, "Last.fm: loved tracks fetch failed: {e}"))
}

/// Fetch every loved track of `username` via `user.getLovedTracks`, paging
/// sequentially. Fails as a whole if any page fails: no partial lists.
pub async fn get_loved_tracks(
    config: &LastFmConfig,
    username: &str,
) -> Result<Vec<LastFmLovedTrack>, String> {
    info!(username, "Last.fm: fetching loved tracks");
    let first = fetch_loved_tracks_page(config, username, 1).await?;
    let total_pages = first.total_pages;
    let mut all = first.tracks;
    for page in 2..=total_pages {
        all.extend(
            fetch_loved_tracks_page(config, username, page)
                .await?
                .tracks,
        );
    }
    info!(
        username,
        count = all.len(),
        pages = total_pages,
        "Last.fm: fetched loved tracks"
    );
    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn user_loved_one_is_true_and_zero_is_false() {
        assert_eq!(
            parse_user_loved(&json!({"track": {"userloved": "1"}})),
            Ok(true)
        );
        assert_eq!(
            parse_user_loved(&json!({"track": {"userloved": "0"}})),
            Ok(false)
        );
    }

    #[test]
    fn user_loved_missing_or_odd_is_an_error() {
        for body in [
            json!({}),
            json!({"track": {}}),
            json!({"track": {"userloved": 1}}),
            json!({"track": {"userloved": true}}),
            json!({"track": {"userloved": "yes"}}),
            json!({"track": {"userloved": ""}}),
        ] {
            assert!(parse_user_loved(&body).is_err(), "{body}");
        }
    }

    #[test]
    fn user_loved_error_names_the_offending_value() {
        let err = parse_user_loved(&json!({"track": {"userloved": "maybe"}})).unwrap_err();
        assert!(err.contains("maybe"));
    }

    #[test]
    fn parses_multiple_tracks_and_string_attrs() {
        let body = json!({"lovedtracks": {
            "track": [
                {"name": "T1", "artist": {"name": "A1", "url": "x"}},
                {"name": "T2", "artist": {"name": "A2"}}
            ],
            "@attr": {"page": "1", "totalPages": "2", "total": "1001"}
        }});
        let page = parse_loved_tracks_page(&body).unwrap();
        assert_eq!(page.total_pages, 2);
        assert_eq!(
            page.tracks,
            vec![
                LastFmLovedTrack {
                    artist: "A1".into(),
                    track: "T1".into()
                },
                LastFmLovedTrack {
                    artist: "A2".into(),
                    track: "T2".into()
                },
            ]
        );
    }

    #[test]
    fn parses_single_track_given_as_object() {
        let body = json!({"lovedtracks": {
            "track": {"name": "T", "artist": {"name": "A"}},
            "@attr": {"page": "1", "totalPages": "1", "total": "1"}
        }});
        let page = parse_loved_tracks_page(&body).unwrap();
        assert_eq!(
            page.tracks,
            vec![LastFmLovedTrack {
                artist: "A".into(),
                track: "T".into()
            }]
        );
    }

    #[test]
    fn missing_or_empty_track_means_no_tracks() {
        for track in [None, Some(json!([])), Some(json!(""))] {
            let mut loved = json!({"@attr": {"page": "1", "totalPages": "0", "total": "0"}});
            if let Some(t) = track {
                loved["track"] = t;
            }
            let page = parse_loved_tracks_page(&json!({ "lovedtracks": loved })).unwrap();
            assert!(page.tracks.is_empty());
        }
    }

    #[test]
    fn missing_lovedtracks_is_an_error() {
        assert!(parse_loved_tracks_page(&json!({})).is_err());
        assert!(parse_loved_tracks_page(&json!({"lovedtracks": "oops"})).is_err());
    }

    #[test]
    fn entry_without_artist_or_name_is_an_error() {
        let body = json!({"lovedtracks": {"track": [{"name": "T"}]}});
        assert!(parse_loved_tracks_page(&body).is_err());
    }

    #[test]
    fn invalid_total_pages_is_an_error() {
        let body = json!({"lovedtracks": {"track": [], "@attr": {"totalPages": "abc"}}});
        assert!(parse_loved_tracks_page(&body).is_err());
    }
}
