use cast_sender::namespace::connection::Connection;
use cast_sender::namespace::media::{
    GetStatusRequestData, LoadRequestData, Media, MediaInformation, StreamType,
};
use cast_sender::namespace::receiver::Status;
use cast_sender::{App, AppId, MediaController, Receiver};

use crate::config::Action;

#[derive(Debug, Clone, PartialEq)]
pub enum PlannedLoad {
    /// Doorbell ring audio.
    Doorbell { audio_url: String },
    /// Standalone thumbnail display (full-screen Photo). Persists until the device
    /// idles or the next load replaces it.
    Thumbnail { url: String },
    /// Thumbnail display that auto-clears after `display` by stopping the receiver
    /// app (returns the device to its idle/ambient screen). When this is not
    /// the last load it behaves like `Thumbnail` (the next load replaces it).
    TimedThumbnail {
        url: String,
        display: std::time::Duration,
    },
    /// Live video stream.
    Livestream { url: String },
    /// Wait before the next load. Not a media load; just a timer.
    Sleep(std::time::Duration),
    /// Re-launch the Default Media Receiver and resume the media that was
    /// playing before we interrupted it, at its last position. Captured before
    /// our first `launch_app`; handled in `run_device`, not via `load_request_for`.
    Resume,
}

/// Pure: turn a device's action set + urls into the list of loads to perform,
/// preserving the order the actions were given on the CLI.
///
/// The default media receiver holds **one** media session: each `load`
/// replaces the previous, and the crate's `MetadataType` serializes without a
/// `metadataType` discriminator so audio artwork is not rendered. Therefore a
/// device with both `doorbell` and `image` cannot show both simultaneously.
/// The operator controls sequencing and timing via the action order and an
/// explicit `sleep:<dur>` action — e.g. `doorbell`, `sleep:2s`, `image` plays
/// the ring, waits 2s, then shows the snapshot (which persists). The SPEC's
/// per-device action subsets let operators dedicate display vs speaker devices.
pub fn plan_loads(
    actions: &[Action],
    doorbell_url: &str,
    livestream_url: &str,
    thumbnail_url: Option<&str>,
) -> Vec<PlannedLoad> {
    actions
        .iter()
        .filter_map(|a| match a {
            Action::Doorbell => Some(PlannedLoad::Doorbell {
                audio_url: doorbell_url.to_string(),
            }),
            Action::Thumbnail => thumbnail_url.map(|img| PlannedLoad::Thumbnail {
                url: img.to_string(),
            }),
            Action::TimedThumbnail(d) => thumbnail_url.map(|img| PlannedLoad::TimedThumbnail {
                url: img.to_string(),
                display: *d,
            }),
            Action::Livestream => Some(PlannedLoad::Livestream {
                url: livestream_url.to_string(),
            }),
            Action::Sleep(d) => Some(PlannedLoad::Sleep(*d)),
            Action::Resume => Some(PlannedLoad::Resume),
        })
        .collect()
}

/// Pure: build a cast `LoadRequestData` from a planned load.
pub fn load_request_for(load: &PlannedLoad) -> LoadRequestData {
    match load {
        PlannedLoad::Doorbell { audio_url } => LoadRequestData {
            media: MediaInformation {
                content_id: audio_url.clone(),
                content_type: "audio/mpeg".into(),
                stream_type: StreamType::Buffered,
                ..Default::default()
            },
            autoplay: Some(true),
            ..Default::default()
        },
        PlannedLoad::Thumbnail { url } => LoadRequestData {
            media: MediaInformation {
                content_id: url.clone(),
                content_type: "image/jpeg".into(),
                stream_type: StreamType::Buffered,
                ..Default::default()
            },
            autoplay: Some(true),
            ..Default::default()
        },
        PlannedLoad::TimedThumbnail { url, .. } => LoadRequestData {
            media: MediaInformation {
                content_id: url.clone(),
                content_type: "image/jpeg".into(),
                stream_type: StreamType::Buffered,
                ..Default::default()
            },
            autoplay: Some(true),
            ..Default::default()
        },
        PlannedLoad::Livestream { url } => LoadRequestData {
            media: MediaInformation {
                content_id: url.clone(),
                content_type: "application/x-mpegurl".into(),
                stream_type: StreamType::Live,
                ..Default::default()
            },
            autoplay: Some(true),
            ..Default::default()
        },
        // `Sleep` and `Resume` are not media loads; `run_device` handles them
        // before calling this function, so these arms are never reached.
        PlannedLoad::Sleep(_) => {
            unreachable!("load_request_for called on PlannedLoad::Sleep")
        }
        PlannedLoad::Resume => {
            unreachable!("load_request_for called on PlannedLoad::Resume")
        }
    }
}

/// Pure: from the receiver's current apps, find a running app that supports
/// the Media namespace (so we can query what's playing and resume it).
/// Returns the app to reuse as a transport. Prefers the Default Media
/// Receiver; falls back to any other Media-namespace app (e.g. a custom
/// receiver like BBC Sounds) so we can at least re-launch it.
pub fn find_media_app(apps: &[App]) -> Option<App> {
    let has_media = |app: &App| {
        app.namespaces
            .iter()
            .any(|ns| matches!(ns, cast_sender::namespace::NamespaceUrn::Media))
    };
    // Prefer Default Media Receiver when present.
    if let Some(app) = apps
        .iter()
        .find(|a| a.app_id == AppId::DefaultMediaReceiver && has_media(a))
    {
        return Some(app.clone());
    }
    apps.iter().find(|a| has_media(a)).cloned()
}

/// Pure: build a `LoadRequestData` that resumes a previously-playing media
/// item at its last position. Returns `None` if there is no media to resume
/// (e.g. the receiver was idle, or the app had no current media).
///
/// Uses a `Generic` metadata type with the original title/poster so the UI
/// reflects the resumed stream. `current_time` is the seek offset captured
/// from the prior `MediaStatus`.
pub fn build_resume_load(media: &MediaInformation, current_time: f64) -> LoadRequestData {
    LoadRequestData {
        media: media.clone(),
        current_time: Some(current_time),
        autoplay: Some(true),
        ..Default::default()
    }
}

/// Connect to one Chromecast, run its configured actions, disconnect.
/// I/O: runs on the smol runtime (caller wraps in `smol::block_on`).
pub async fn run_device(
    ip: &str,
    actions: &[Action],
    doorbell_url: &str,
    livestream_url: &str,
    thumbnail_url: Option<&str>,
) -> Result<(), String> {
    let plan = plan_loads(actions, doorbell_url, livestream_url, thumbnail_url);
    if plan.is_empty() {
        tracing::warn!(%ip, actions = ?actions, "no loads to perform");
        return Ok(());
    }

    let rx = Receiver::new();
    rx.connect(ip).await.map_err(|e| {
        let m = format!("connect {ip}: {e}");
        tracing::warn!(%ip, err = %m, "cast connect failed");
        m
    })?;
    tracing::info!(%ip, actions = ?actions, plan = ?plan, "cast connected");

    // Capture any currently-playing Default Media Receiver media so a trailing
    // `resume` action can restore it after our doorbell/image interrupts it.
    let resume_media = if plan.iter().any(|p| matches!(p, PlannedLoad::Resume)) {
        capture_resume_media(&rx, ip).await
    } else {
        None
    };

    let app = rx
        .launch_app(AppId::DefaultMediaReceiver)
        .await
        .map_err(|e| {
            let m = format!("launch app: {e}");
            tracing::warn!(%ip, err = %m, "cast launch failed");
            m
        })?;
    // Keep a clone to stop the app after a timed image; MediaController takes
    // the original by value.
    let app_handle: App = app.clone();
    let mc = MediaController::new(app, rx.clone()).map_err(|e| {
        let m = format!("media controller: {e}");
        tracing::warn!(%ip, err = %m, "cast media controller failed");
        m
    })?;

    for (i, load) in plan.iter().enumerate() {
        match load {
            PlannedLoad::Sleep(d) => {
                tracing::info!(%ip, duration = ?d, "cast sleep");
                smol::Timer::after(*d).await;
                continue;
            }
            // Inline resume: only reached when there is no TimedThumbnail before
            // it (otherwise the image's background task owns the tail).
            PlannedLoad::Resume => {
                do_resume(&rx, &resume_media, ip).await.map_err(|e| {
                    tracing::warn!(%ip, err = %e, "cast resume failed");
                    e
                })?;
                continue;
            }
            _ => {}
        }
        let req = load_request_for(load);
        tracing::info!(
            %ip,
            load = ?load,
            content_id = %req.media.content_id,
            "cast load"
        );
        mc.load(req).await.map_err(|e| {
            let m = format!("load: {e}");
            tracing::warn!(%ip, load = ?load, err = %m, "cast load failed");
            m
        })?;
        // autoplay=true on the load already starts playback (audio/livestream)
        // or displays the image; an explicit Play afterwards is rejected by the
        // receiver as "Invalid Request".

        // Timed image (at any position): load it, then hand the display window
        // + app-stop + any trailing loads (e.g. `Resume`) off to a detached
        // background task. This keeps the webhook fast and avoids the 20s
        // webhook timeout for a >20s display. The background task sleeps for
        // the display duration, stops the receiver app (clearing the image),
        // runs the remaining plan items, and disconnects. We return
        // immediately so the webhook can respond 200.
        if let PlannedLoad::TimedThumbnail { display: dur, .. } = load {
            let dur = *dur; // copy out of `plan` so the background task owns it
            let rx_bg = rx.clone();
            let app_bg = app_handle.clone();
            let ip_bg = ip.to_string();
            let remaining: Vec<PlannedLoad> = plan[i + 1..].to_vec();
            let resume_bg = resume_media.clone();
            smol::spawn(async move {
                tracing::info!(ip = %ip_bg, display = ?dur, "cast image display window");
                smol::Timer::after(dur).await;
                tracing::info!(ip = %ip_bg, "cast stopping app to clear image");
                if let Err(e) = rx_bg.stop_app(&app_bg).await {
                    tracing::warn!(ip = %ip_bg, err = %e, "cast stop_app failed (image may remain)");
                }
                // Run any trailing loads (only `Resume` is sensible here; other
                // kinds would need a fresh app launch which we do best-effort).
                for load in &remaining {
                    match load {
                        PlannedLoad::Resume => {
                            if let Err(e) = do_resume(&rx_bg, &resume_bg, &ip_bg).await {
                                tracing::warn!(ip = %ip_bg, err = %e, "cast resume failed");
                            }
                        }
                        PlannedLoad::Sleep(d) => {
                            tracing::info!(ip = %ip_bg, duration = ?d, "cast sleep (background)");
                            smol::Timer::after(*d).await;
                        }
                        other => {
                            tracing::warn!(ip = %ip_bg, load = ?other, "ignoring load after timed image in background");
                        }
                    }
                }
                rx_bg.disconnect().await;
                tracing::info!(ip = %ip_bg, "cast done (background)");
            })
            .detach();
            // Skip the inline disconnect below; the background task owns it.
            return Ok(());
        }
    }

    rx.disconnect().await;
    tracing::info!(%ip, "cast done");
    Ok(())
}

/// I/O: re-launch the app that was running before the doorbell and resume
/// the captured media at its last position. `resume_media` is
/// `(AppId, MediaInformation, current_time)` as captured by
/// `capture_resume_media`. No-op (logged) when there is nothing to resume.
///
/// For the Default Media Receiver and apps that accept generic `Media.Load`
/// (e.g. BBC Sounds), the media resumes at its last position. For apps that
/// use a proprietary protocol (e.g. YouTube) the `Media.Load` is rejected,
/// but the app is still re-launched — the user lands in the right app even
/// if the specific video doesn't resume. The load failure is non-fatal.
async fn do_resume(
    rx: &Receiver,
    resume_media: &Option<(AppId, MediaInformation, f64)>,
    ip: &str,
) -> Result<(), String> {
    tracing::info!(%ip, "cast resume");
    let Some((app_id, media, current_time)) = resume_media else {
        tracing::info!(%ip, "cast resume: nothing to resume (no prior media app)");
        return Ok(());
    };
    let req = build_resume_load(media, *current_time);
    tracing::info!(
        %ip,
        app_id = %app_id,
        content_id = %req.media.content_id,
        current_time,
        "cast resume load"
    );
    // Re-launch the original app. This always succeeds for the app foreground
    // even if the subsequent media load is rejected.
    let resume_app = rx
        .launch_app(app_id.clone())
        .await
        .map_err(|e| format!("resume launch app: {e}"))?;
    let resume_mc = match MediaController::new(resume_app, rx.clone()) {
        Ok(mc) => mc,
        Err(e) => {
            // App re-launched but doesn't support the Media namespace — that's
            // fine, the user is in the right app.
            tracing::info!(%ip, err = %e, "cast resume: app re-launched, no media namespace (OK for custom apps)");
            return Ok(());
        }
    };
    // Best-effort: try to load the captured media. Custom receivers like
    // YouTube reject this with "Invalid Request" — non-fatal, the app is open.
    if let Err(e) = resume_mc.load(req).await {
        tracing::info!(
            %ip,
            err = %e,
            app_id = %app_id,
            "cast resume: media load rejected (custom receiver protocol; app is re-launched)"
        );
    }
    Ok(())
}

/// I/O: before we interrupt the device, capture the currently-playing media
/// (if any) so a trailing `resume` action can restore it. Returns
/// `(AppId, MediaInformation, current_time)` ready to feed `do_resume`.
/// Captures from any running app that advertises the Media namespace
/// (default receiver or a custom one like BBC Sounds).
///
/// Best-effort: if the receiver is idle, no Media-namespace app is running,
/// or the media status query fails, returns `None` and logs.
async fn capture_resume_media(rx: &Receiver, ip: &str) -> Option<(AppId, MediaInformation, f64)> {
    let status: Status = match rx.status().await {
        Ok(s) => s,
        Err(e) => {
            tracing::info!(%ip, err = %e, "resume: could not read receiver status");
            return None;
        }
    };
    let apps = status.applications.unwrap_or_default();
    let Some(app) = find_media_app(&apps) else {
        tracing::info!(%ip, "resume: no prior media app running");
        return None;
    };
    tracing::info!(
        %ip,
        app_id = %app.app_id,
        display_name = %app.display_name,
        "resume: found prior media app"
    );
    // Establish a virtual connection to the (already-running) app before
    // querying it. cast-sender's `launch_app` does this for apps it launches,
    // but for a foreign app (e.g. BBC Sounds already playing) we must do it
    // ourselves or Media-namespace requests are ignored and time out.
    if let Err(e) = rx.send(&app, Connection::Connect).await {
        tracing::info!(%ip, err = %e, "resume: could not connect to prior media app");
        return None;
    }
    // Ask the running app what's playing. GetStatus with no session id returns
    // the current media status.
    let resp = rx
        .send_request(&app, Media::GetStatus(GetStatusRequestData::default()))
        .await;
    let payload = match resp {
        Ok(r) => r.payload,
        Err(e) => {
            tracing::info!(%ip, err = %e, "resume: media GetStatus failed");
            return None;
        }
    };
    let cast_sender::Payload::Media(Media::MediaStatus(data)) = payload else {
        tracing::info!(%ip, "resume: no media status returned");
        return None;
    };
    let status = data.status.first()?;
    let media = status.media.clone()?;
    tracing::info!(
        %ip,
        app_id = %app.app_id,
        display_name = %app.display_name,
        content_id = %media.content_id,
        current_time = status.current_time,
        "resume: captured prior media"
    );
    // Remember which app to re-launch for resume (not just the media).
    Some((app.app_id, media, status.current_time))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn plan_doorbell_only() {
        let p = plan_loads(&[Action::Doorbell], "https://ex/d.mp3", "x", None);
        assert_eq!(
            p,
            vec![PlannedLoad::Doorbell {
                audio_url: "https://ex/d.mp3".into()
            }]
        );
    }

    #[test]
    fn plan_image_only() {
        let p = plan_loads(&[Action::Thumbnail], "x", "x", Some("http://h/i.jpg"));
        assert_eq!(
            p,
            vec![PlannedLoad::Thumbnail {
                url: "http://h/i.jpg".into()
            }]
        );
    }

    #[test]
    fn plan_image_only_no_url_is_empty() {
        let p = plan_loads(&[Action::Thumbnail], "x", "x", None);
        assert!(p.is_empty());
    }

    #[test]
    fn plan_livestream_only() {
        let p = plan_loads(&[Action::Livestream], "x", "https://ex/s.m3u8", None);
        assert_eq!(
            p,
            vec![PlannedLoad::Livestream {
                url: "https://ex/s.m3u8".into()
            }]
        );
    }

    #[test]
    fn plan_preserves_cli_order_doorbell_then_image() {
        // Order is exactly as given on the CLI; plan_loads does not reorder.
        let p = plan_loads(
            &[Action::Doorbell, Action::Thumbnail],
            "https://ex/d.mp3",
            "x",
            Some("http://h/i.jpg"),
        );
        assert_eq!(
            p,
            vec![
                PlannedLoad::Doorbell {
                    audio_url: "https://ex/d.mp3".into()
                },
                PlannedLoad::Thumbnail {
                    url: "http://h/i.jpg".into()
                },
            ]
        );
    }

    #[test]
    fn plan_preserves_cli_order_image_then_doorbell() {
        let p = plan_loads(
            &[Action::Thumbnail, Action::Doorbell],
            "https://ex/d.mp3",
            "x",
            Some("http://h/i.jpg"),
        );
        assert_eq!(
            p,
            vec![
                PlannedLoad::Thumbnail {
                    url: "http://h/i.jpg".into()
                },
                PlannedLoad::Doorbell {
                    audio_url: "https://ex/d.mp3".into()
                },
            ]
        );
    }

    #[test]
    fn plan_with_sleep_between() {
        // --action kitchen:doorbell --action kitchen:sleep:2s --action kitchen:image
        let p = plan_loads(
            &[
                Action::Doorbell,
                Action::Sleep(std::time::Duration::from_secs(2)),
                Action::Thumbnail,
            ],
            "https://ex/d.mp3",
            "x",
            Some("http://h/i.jpg"),
        );
        assert_eq!(
            p,
            vec![
                PlannedLoad::Doorbell {
                    audio_url: "https://ex/d.mp3".into()
                },
                PlannedLoad::Sleep(std::time::Duration::from_secs(2)),
                PlannedLoad::Thumbnail {
                    url: "http://h/i.jpg".into()
                },
            ]
        );
    }

    #[test]
    fn plan_image_without_url_is_dropped() {
        let p = plan_loads(
            &[Action::Doorbell, Action::Thumbnail],
            "https://ex/d.mp3",
            "x",
            None,
        );
        assert_eq!(
            p,
            vec![PlannedLoad::Doorbell {
                audio_url: "https://ex/d.mp3".into()
            }]
        );
    }

    #[test]
    fn plan_empty_actions() {
        assert!(plan_loads(&[], "x", "x", Some("i")).is_empty());
    }

    #[test]
    fn load_request_doorbell_has_no_metadata() {
        let r = load_request_for(&PlannedLoad::Doorbell {
            audio_url: "https://ex/d.mp3".into(),
        });
        assert_eq!(r.media.content_id, "https://ex/d.mp3");
        assert_eq!(r.media.content_type, "audio/mpeg");
        assert!(matches!(r.media.stream_type, StreamType::Buffered));
        assert_eq!(r.autoplay, Some(true));
        assert!(r.media.metadata.is_none());
    }

    #[test]
    fn load_request_image() {
        let r = load_request_for(&PlannedLoad::Thumbnail {
            url: "http://h/i.jpg".into(),
        });
        assert_eq!(r.media.content_id, "http://h/i.jpg");
        assert_eq!(r.media.content_type, "image/jpeg");
        assert!(matches!(r.media.stream_type, StreamType::Buffered));
        assert!(r.media.metadata.is_none());
    }

    #[test]
    fn plan_image_timed() {
        let p = plan_loads(
            &[
                Action::Doorbell,
                Action::Sleep(Duration::from_secs(2)),
                Action::TimedThumbnail(Duration::from_secs(30)),
            ],
            "https://ex/d.mp3",
            "x",
            Some("http://h/i.jpg"),
        );
        assert_eq!(
            p,
            vec![
                PlannedLoad::Doorbell {
                    audio_url: "https://ex/d.mp3".into()
                },
                PlannedLoad::Sleep(Duration::from_secs(2)),
                PlannedLoad::TimedThumbnail {
                    url: "http://h/i.jpg".into(),
                    display: Duration::from_secs(30)
                },
            ]
        );
    }

    #[test]
    fn plan_image_timed_without_url_is_dropped() {
        let p = plan_loads(
            &[Action::TimedThumbnail(Duration::from_secs(30))],
            "x",
            "x",
            None,
        );
        assert!(p.is_empty());
    }

    #[test]
    fn load_request_image_timed() {
        let r = load_request_for(&PlannedLoad::TimedThumbnail {
            url: "http://h/i.jpg".into(),
            display: Duration::from_secs(30),
        });
        assert_eq!(r.media.content_id, "http://h/i.jpg");
        assert_eq!(r.media.content_type, "image/jpeg");
        assert!(matches!(r.media.stream_type, StreamType::Buffered));
        assert_eq!(r.autoplay, Some(true));
    }

    #[test]
    fn load_request_livestream() {
        let r = load_request_for(&PlannedLoad::Livestream {
            url: "https://ex/s.m3u8".into(),
        });
        assert_eq!(r.media.content_id, "https://ex/s.m3u8");
        assert_eq!(r.media.content_type, "application/x-mpegurl");
        assert!(matches!(r.media.stream_type, StreamType::Live));
    }

    fn app_with(app_id: AppId, namespaces: Vec<cast_sender::namespace::NamespaceUrn>) -> App {
        App {
            app_id,
            namespaces,
            ..Default::default()
        }
    }

    #[test]
    fn plan_with_resume_last() {
        let p = plan_loads(
            &[
                Action::Doorbell,
                Action::Sleep(Duration::from_secs(2)),
                Action::Resume,
            ],
            "https://ex/d.mp3",
            "x",
            None,
        );
        assert_eq!(
            p,
            vec![
                PlannedLoad::Doorbell {
                    audio_url: "https://ex/d.mp3".into()
                },
                PlannedLoad::Sleep(Duration::from_secs(2)),
                PlannedLoad::Resume,
            ]
        );
    }

    #[test]
    fn plan_image_timed_then_resume() {
        // The sequence that was broken: image must display for its duration,
        // then resume runs (handled by the image's background task).
        let p = plan_loads(
            &[
                Action::Doorbell,
                Action::Sleep(Duration::from_secs(3)),
                Action::TimedThumbnail(Duration::from_secs(20)),
                Action::Resume,
            ],
            "https://ex/d.mp3",
            "x",
            Some("http://h/i.jpg"),
        );
        assert_eq!(
            p,
            vec![
                PlannedLoad::Doorbell {
                    audio_url: "https://ex/d.mp3".into()
                },
                PlannedLoad::Sleep(Duration::from_secs(3)),
                PlannedLoad::TimedThumbnail {
                    url: "http://h/i.jpg".into(),
                    display: Duration::from_secs(20)
                },
                PlannedLoad::Resume,
            ]
        );
    }

    #[test]
    fn find_media_app_prefers_default_receiver() {
        use cast_sender::namespace::NamespaceUrn;
        let apps = vec![
            app_with(AppId::YouTube, vec![NamespaceUrn::Media]),
            app_with(AppId::DefaultMediaReceiver, vec![NamespaceUrn::Connection]),
            app_with(
                AppId::DefaultMediaReceiver,
                vec![NamespaceUrn::Connection, NamespaceUrn::Media],
            ),
        ];
        let found = find_media_app(&apps).expect("found");
        assert_eq!(found.app_id, AppId::DefaultMediaReceiver);
    }

    #[test]
    fn find_media_app_falls_back_to_custom_app() {
        // A custom receiver (e.g. BBC Sounds) with a Media namespace should be
        // captured when no Default Media Receiver is running.
        use cast_sender::namespace::NamespaceUrn;
        let apps = vec![app_with(
            AppId::Custom("BCCSounds".into()),
            vec![NamespaceUrn::Connection, NamespaceUrn::Media],
        )];
        let found = find_media_app(&apps).expect("found");
        assert_eq!(found.app_id, AppId::Custom("BCCSounds".into()));
    }

    #[test]
    fn find_media_app_none_when_no_media_namespace() {
        let apps = vec![app_with(AppId::DefaultMediaReceiver, vec![])];
        assert!(find_media_app(&apps).is_none());
    }

    #[test]
    fn find_media_app_none_when_empty() {
        assert!(find_media_app(&[]).is_none());
    }

    #[test]
    fn build_resume_load_carries_media_and_seek() {
        let media = MediaInformation {
            content_id: "https://ex/stream.m3u8".into(),
            content_type: "application/x-mpegurl".into(),
            stream_type: StreamType::Live,
            ..Default::default()
        };
        let req = build_resume_load(&media, 42.5);
        assert_eq!(req.media.content_id, "https://ex/stream.m3u8");
        assert_eq!(req.media.content_type, "application/x-mpegurl");
        assert!(matches!(req.media.stream_type, StreamType::Live));
        assert_eq!(req.current_time, Some(42.5));
        assert_eq!(req.autoplay, Some(true));
    }
}
