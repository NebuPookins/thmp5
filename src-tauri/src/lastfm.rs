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
    let loved = body
        .get("track")
        .and_then(|t| t.get("userloved"))
        .and_then(|v| v.as_str())
        .map(|s| s == "1")
        .unwrap_or(false);
    info!(artist, track, loved, "Last.fm: checked track loved status");
    Ok(loved)
}

/// Call track.love.
pub async fn love_track(
    config: &LastFmConfig,
    session_key: &str,
    artist: &str,
    track: &str,
) -> Result<(), String> {
    info!(artist, track, "Last.fm: loving track");
    let params = vec![
        ("method", "track.love"),
        ("sk", session_key),
        ("artist", artist),
        ("track", track),
    ];
    match api_call(params, config).await {
        Ok(_) => {
            info!("Last.fm: track loved successfully");
            Ok(())
        }
        Err(e) => {
            warn!(artist, track, "Last.fm: love track failed: {e}");
            Err(e)
        }
    }
}
