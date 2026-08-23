use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::to_bytes;
#[cfg(test)]
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::header;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use futures::future::join_all;
use url::Url;

use crate::cast;
use crate::config::{Action, Config};
use crate::payload::AlarmPayload;

pub fn build(cfg: Config) -> Router {
    let cfg = Arc::new(cfg);
    let mut r = Router::new()
        .route("/healthz", get(healthz_handler))
        .route("/media/{id}", get(media_handler))
        .route("/doorbell.mp3", get(doorbell_handler));
    for path in cfg.routes.keys() {
        r = r.route(path, post(webhook));
    }
    r.fallback(|| async {
        tracing::info!(status = 404, "not found");
        (StatusCode::NOT_FOUND, "not found")
    })
    .with_state(cfg)
}

/// Pure: does any alias in this route have an image action (timed or not)?
pub fn needs_thumbnail(aliases: &[String], actions: &HashMap<String, Vec<Action>>) -> bool {
    aliases.iter().any(|a| {
        actions.get(a).is_some_and(|v| {
            v.iter()
                .any(|x| matches!(x, Action::Thumbnail | Action::TimedThumbnail(_)))
        })
    })
}

/// Pure: build the externally-fetchable image URL for an event id.
pub fn thumbnail_url(advertise: &Url, event_id: &str) -> String {
    let base = advertise.as_str().trim_end_matches('/');
    format!("{base}/media/{event_id}")
}

/// Pure: flatten task outcomes into error strings. JoinError is normalized to
/// a String by the caller.
#[cfg(test)]
#[allow(dead_code)]
pub fn collect_errors(results: Vec<Result<Result<(), String>, String>>) -> Vec<String> {
    results
        .into_iter()
        .filter_map(|r| match r {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(e),
            Err(e) => Some(e),
        })
        .collect()
}

async fn healthz_handler() -> Response {
    (StatusCode::OK, "ok").into_response()
}

async fn doorbell_handler(State(cfg): State<Arc<Config>>) -> Response {
    tracing::info!(path = "/doorbell.mp3", "doorbell media request");
    match &cfg.doorbell_bytes {
        Some(bytes) => ([(header::CONTENT_TYPE, "audio/mpeg")], bytes.clone()).into_response(),
        None => (StatusCode::NOT_FOUND, "doorbell not configured").into_response(),
    }
}

async fn media_handler(State(cfg): State<Arc<Config>>, Path(id): Path<String>) -> Response {
    tracing::info!(path = %format!("/media/{id}"), "media request");
    match cfg.thumbnails.get(&id) {
        Some(bytes) => ([(header::CONTENT_TYPE, "image/jpeg")], bytes).into_response(),
        None => (StatusCode::NOT_FOUND, "expired or unknown").into_response(),
    }
}

async fn webhook(State(cfg): State<Arc<Config>>, req: Request) -> Response {
    let path = req.uri().path().to_string();
    let body = match to_bytes(req.into_body(), 32 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("body: {e}")).into_response(),
    };
    let aliases = match cfg.routes.get(&path) {
        Some(a) => a.clone(),
        None => {
            tracing::info!(%path, status = 404, "unknown endpoint");
            return (StatusCode::NOT_FOUND, "not found").into_response();
        }
    };
    tracing::info!(%path, len = body.len(), "webhook request");
    tracing::debug!(body = %String::from_utf8_lossy(&body), "payload");

    let payload: AlarmPayload = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(%path, err = %e, "invalid payload");
            return (StatusCode::BAD_REQUEST, format!("bad payload: {e}")).into_response();
        }
    };

    let (event_id, thumbnail_url) = if needs_thumbnail(&aliases, &cfg.actions) {
        let id = match payload.event_id() {
            Ok(id) => id,
            Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
        };
        let bytes = match payload.image_bytes() {
            Ok(b) => b,
            Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
        };
        cfg.thumbnails.insert(&id, bytes);
        let base = cfg.advertise_url.as_ref().unwrap();
        (Some(id.clone()), Some(thumbnail_url(base, &id)))
    } else {
        (None, None)
    };

    let need_thumbnail = needs_thumbnail(&aliases, &cfg.actions);

    tracing::info!(
        %path,
        devices = ?aliases,
        need_thumbnail,
        thumbnail_url = thumbnail_url.as_deref().unwrap_or(""),
        doorbell_url = cfg.doorbell_url.as_deref().unwrap_or(""),
        livestream_url = cfg.livestream_url.as_ref().map(|u| u.as_str()).unwrap_or(""),
        "dispatching"
    );

    let doorbell = cfg.doorbell_url.clone().unwrap_or_default();
    let livestream = cfg
        .livestream_url
        .as_ref()
        .map(|u| u.as_str().to_string())
        .unwrap_or_default();

    let mut tasks = Vec::new();
    for alias in &aliases {
        let acts = cfg.actions.get(alias).cloned().unwrap_or_default();
        if acts.is_empty() {
            tracing::info!(%path, alias, "device has no actions, skipping");
            continue;
        }
        let ip = match cfg.devices.get(alias) {
            Some(ip) => ip.clone(),
            None => {
                tracing::error!(%path, alias, "device alias missing from registry");
                continue;
            }
        };
        let doorbell = doorbell.clone();
        let livestream = livestream.clone();
        let thumbnail_url = thumbnail_url.clone();
        let alias = alias.clone();
        let ip_for_log = ip.clone();
        tasks.push(tokio::task::spawn_blocking(move || {
            let res = smol::block_on(async move {
                use smol_timeout::TimeoutExt;
                cast::run_device(&ip, &acts, &doorbell, &livestream, thumbnail_url.as_deref())
                    .timeout(Duration::from_secs(20))
                    .await
                    .ok_or_else(|| "cast timed out (20s)".to_string())
                    .and_then(|r| r)
            });
            (alias, ip_for_log, res)
        }));
    }

    let results = join_all(tasks).await;
    let mut errors: Vec<String> = Vec::new();
    for r in results {
        match r {
            Ok((alias, ip, Ok(()))) => {
                tracing::info!(%path, alias, ip, "device ok");
            }
            Ok((alias, ip, Err(e))) => {
                tracing::warn!(%path, alias, ip, err = %e, "device failed");
                errors.push(format!("{alias} ({ip}): {e}"));
            }
            Err(e) => {
                tracing::error!(%path, err = %e, "task join failed");
                errors.push(format!("task join: {e}"));
            }
        }
    }

    // Evict the cached image after the TTL. The cast `load` ACKs at the
    // protocol level before the receiver actually fetches the poster over
    // HTTP, so removing it immediately after join_all would race the fetch
    // and the poster would 404. Schedule removal instead.
    if let Some(id) = event_id {
        let cfg = cfg.clone();
        tokio::spawn(async move {
            tokio::time::sleep(crate::media::TTL).await;
            cfg.thumbnails.remove(&id);
            tracing::debug!(event_id = %id, "evicted cached image");
        });
    }

    if errors.is_empty() {
        tracing::info!(%path, status = 200, "ok");
        (StatusCode::OK, "ok").into_response()
    } else {
        tracing::warn!(%path, status = 500, n = errors.len(), errors = ?errors, "cast errors");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("errors: {errors:?}"),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use tower::ServiceExt;

    fn actions_map(pairs: &[(&str, &[Action])]) -> HashMap<String, Vec<Action>> {
        let mut m = HashMap::new();
        for (k, v) in pairs {
            m.insert((*k).to_string(), v.to_vec());
        }
        m
    }

    #[test]
    fn needs_thumbnail_true_for_timed() {
        let actions = actions_map(&[(
            "lr",
            &[Action::TimedThumbnail(std::time::Duration::from_secs(30))],
        )]);
        assert!(needs_thumbnail(&["lr".to_string()], &actions));
    }

    #[test]
    fn needs_thumbnail_true() {
        let actions = actions_map(&[("lr", &[Action::Thumbnail])]);
        assert!(needs_thumbnail(&["lr".to_string()], &actions));
    }

    #[test]
    fn needs_thumbnail_false_when_only_doorbell() {
        let actions = actions_map(&[("lr", &[Action::Doorbell])]);
        assert!(!needs_thumbnail(&["lr".to_string()], &actions));
    }

    #[test]
    fn needs_thumbnail_false_when_no_actions() {
        assert!(!needs_thumbnail(&["lr".to_string()], &HashMap::new()));
    }

    #[test]
    fn needs_thumbnail_any_in_route() {
        let actions = actions_map(&[("kit", &[Action::Thumbnail])]);
        assert!(needs_thumbnail(
            &["lr".to_string(), "kit".to_string()],
            &actions
        ));
    }

    #[test]
    fn thumbnail_url_strips_trailing_slash() {
        let u: Url = "http://10.0.0.1:8080/".parse().unwrap();
        assert_eq!(
            thumbnail_url(&u, "evt-1"),
            "http://10.0.0.1:8080/media/evt-1"
        );
    }

    #[test]
    fn thumbnail_url_no_trailing_slash() {
        let u: Url = "http://10.0.0.1:8080".parse().unwrap();
        assert_eq!(
            thumbnail_url(&u, "evt-1"),
            "http://10.0.0.1:8080/media/evt-1"
        );
    }

    #[test]
    fn collect_errors_all_ok() {
        let r = vec![Ok(Ok(())), Ok(Ok(()))];
        assert!(collect_errors(r).is_empty());
    }

    #[test]
    fn collect_errors_mixed() {
        let r = vec![
            Ok(Ok(())),
            Ok(Err("cast fail".into())),
            Err("join fail".into()),
        ];
        assert_eq!(collect_errors(r), vec!["cast fail", "join fail"]);
    }

    fn minimal_cfg() -> Config {
        let mut devices = HashMap::new();
        devices.insert("lr".into(), "192.168.1.50".into());
        let mut routes = HashMap::new();
        routes.insert("/doorbell-lr".to_string(), vec!["lr".to_string()]);
        Config {
            listen: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
            advertise_url: None,
            devices,
            routes,
            actions: HashMap::new(),
            doorbell_url: None,
            doorbell_bytes: None,
            livestream_url: None,
            thumbnails: crate::media::ThumbnailCache::new(),
        }
    }

    #[tokio::test]
    async fn router_unknown_endpoint_404() {
        let app = build(minimal_cfg());
        let req = Request::builder()
            .method("POST")
            .uri("/nope")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn router_healthz_200() {
        let app = build(minimal_cfg());
        let req = Request::builder()
            .method("GET")
            .uri("/healthz")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(&bytes[..], b"ok");
    }

    #[tokio::test]
    async fn router_media_missing_404() {
        let app = build(minimal_cfg());
        let req = Request::builder()
            .method("GET")
            .uri("/media/ghost")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn router_known_endpoint_no_actions_200() {
        // Endpoint exists, device has no actions -> no cast spawned -> 200.
        let app = build(minimal_cfg());
        let body = r#"{"alarm":{"thumbnail":"data:image/jpeg;base64,AAAA","triggers":[{"eventId":"e1","sourceEvent":{"id":"e1"}}]},"timestamp":1}"#;
        let req = Request::builder()
            .method("POST")
            .uri("/doorbell-lr")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn router_bad_payload_400() {
        let app = build(minimal_cfg());
        let req = Request::builder()
            .method("POST")
            .uri("/doorbell-lr")
            .header("content-type", "application/json")
            .body(Body::from("not json"))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn router_serves_cached_image() {
        let cfg = minimal_cfg();
        cfg.thumbnails.insert("evt-1", vec![0xFF, 0xD8, 0xFF]);
        let app = build(cfg);
        let req = Request::builder()
            .method("GET")
            .uri("/media/evt-1")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "image/jpeg"
        );
        let bytes = to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(bytes.to_vec(), vec![0xFF, 0xD8, 0xFF]);
    }

    #[tokio::test]
    async fn router_serves_doorbell_mp3() {
        let cfg = minimal_cfg();
        // minimal_cfg has doorbell_bytes = None -> 404
        let app = build(cfg);
        let req = Request::builder()
            .method("GET")
            .uri("/doorbell.mp3")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // now configure bytes
        let mut cfg = minimal_cfg();
        cfg.doorbell_bytes = Some(vec![0x49, 0x44, 0x33]);
        let app = build(cfg);
        let req = Request::builder()
            .method("GET")
            .uri("/doorbell.mp3")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "audio/mpeg"
        );
        let bytes = to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(bytes.to_vec(), vec![0x49, 0x44, 0x33]);
    }
}
