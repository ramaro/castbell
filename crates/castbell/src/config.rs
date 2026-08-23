use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use url::{Host, Url};

use crate::media::ThumbnailCache;

#[derive(Parser, Debug)]
#[command(
    name = "castbell",
    about = "Webhook server that drives Chromecast devices"
)]
pub struct Cli {
    #[arg(long, default_value = "127.0.0.1:8080")]
    pub listen: SocketAddr,

    /// Repeatable: <alias>:<ip>
    #[arg(long, required = true)]
    pub device: Vec<String>,

    /// Repeatable: <path>:<alias,alias>  (path must be >= 8 chars)
    #[arg(long, required = true)]
    pub endpoint: Vec<String>,

    /// Repeatable: <alias>:<doorbell|thumbnail|thumbnail:<dur>|livestream|sleep:<dur>|resume>
    /// `sleep:<dur>` waits between loads (e.g. `sleep:2s`). `thumbnail:<dur>`
    /// displays the snapshot then auto-clears it after the duration
    /// (e.g. `thumbnail:30s`). `resume` re-launches the Default Media Receiver and
    /// resumes the stream that was playing before the doorbell interrupted it
    /// — only valid as the last action for a device. Order is preserved.
    #[arg(long)]
    pub action: Vec<String>,

    #[arg(long)]
    pub doorbell_url: Option<Url>,

    /// Local mp3 served via --advertise-url (mutually exclusive with --doorbell-url).
    #[arg(long)]
    pub doorbell_file: Option<PathBuf>,

    #[arg(long)]
    pub livestream_url: Option<Url>,

    /// Externally-reachable base URL (needed for the `thumbnail` action).
    #[arg(long)]
    pub advertise_url: Option<Url>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    Doorbell,
    Thumbnail,
    /// Thumbnail display that auto-clears after the given duration.
    TimedThumbnail(Duration),
    Livestream,
    /// Wait before the next load.
    Sleep(Duration),
    /// After interrupting, re-launch the Default Media Receiver and resume the
    /// media that was playing before (at its last position). Only valid as the
    /// last action for a device.
    Resume,
}

#[derive(Debug)]
pub struct Config {
    pub listen: SocketAddr,
    pub advertise_url: Option<Url>,
    pub devices: HashMap<String, String>, // alias -> ip
    pub routes: HashMap<String, Vec<String>>,
    pub actions: HashMap<String, Vec<Action>>,
    pub doorbell_url: Option<String>, // effective URL passed to the cast device
    pub doorbell_bytes: Option<Vec<u8>>, // present when served locally via --doorbell-file
    pub livestream_url: Option<Url>,
    pub thumbnails: ThumbnailCache,
}

/// Parse `<alias>:<ip>` into `(alias, ip-as-string)`.
pub fn parse_device(s: &str) -> Result<(String, String), String> {
    let (alias, ip) = split(s).ok_or_else(|| format!("bad --device '{s}', want alias:ip"))?;
    let ip: IpAddr = ip
        .parse()
        .map_err(|_| format!("bad ip '{ip}' in --device '{s}'"))?;
    Ok((alias.to_string(), ip.to_string()))
}

/// Parse `<path>:<alias,alias>` validating aliases against the device registry.
pub fn parse_endpoint(
    s: &str,
    devices: &HashMap<String, String>,
) -> Result<(String, Vec<String>), String> {
    let (path, aliases) =
        split(s).ok_or_else(|| format!("bad --endpoint '{s}', want path:aliases"))?;
    if path.len() < 8 {
        return Err(format!("endpoint path '{path}' must be >= 8 chars"));
    }
    let aliases: Vec<String> = aliases
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if aliases.is_empty() {
        return Err(format!("endpoint '{path}' has no devices"));
    }
    for a in &aliases {
        if !devices.contains_key(a) {
            return Err(format!("endpoint '{path}' references unknown device '{a}'"));
        }
    }
    Ok((path.to_string(), aliases))
}

/// Pure: parse a human duration `<n>s` or `<n>ms` into `Duration`.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if let Some(n) = s.strip_suffix("ms") {
        let n: u64 = n
            .parse()
            .map_err(|_| format!("bad sleep duration '{s}', want <n>ms or <n>s"))?;
        return Ok(Duration::from_millis(n));
    }
    if let Some(n) = s.strip_suffix('s') {
        let n: u64 = n
            .parse()
            .map_err(|_| format!("bad sleep duration '{s}', want <n>ms or <n>s"))?;
        return Ok(Duration::from_secs(n));
    }
    Err(format!("bad sleep duration '{s}', want <n>ms or <n>s"))
}

/// Parse an action kind name. `sleep:<dur>` produces `Action::Sleep`,
/// `thumbnail:<dur>` produces `Action::TimedThumbnail`, and `resume` produces
/// `Action::Resume`.
pub fn parse_action_kind(s: &str) -> Result<Action, String> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("sleep:") {
        return Ok(Action::Sleep(parse_duration(rest)?));
    }
    if let Some(rest) = s.strip_prefix("thumbnail:") {
        return Ok(Action::TimedThumbnail(parse_duration(rest)?));
    }
    match s {
        "doorbell" => Ok(Action::Doorbell),
        "thumbnail" => Ok(Action::Thumbnail),
        "livestream" => Ok(Action::Livestream),
        "resume" => Ok(Action::Resume),
        other => Err(format!("unknown action '{other}'")),
    }
}

/// Parse `<alias>:<kind>` validating alias against the device registry.
pub fn parse_action(
    s: &str,
    devices: &HashMap<String, String>,
) -> Result<(String, Action), String> {
    let (alias, act) = split(s).ok_or_else(|| format!("bad --action '{s}', want alias:action"))?;
    if !devices.contains_key(alias) {
        return Err(format!("action references unknown device '{alias}'"));
    }
    Ok((alias.to_string(), parse_action_kind(act)?))
}

fn split(s: &str) -> Option<(&str, &str)> {
    s.split_once(':')
}

/// Pure: whether an advertise URL points at localhost, which a Chromecast
/// (a separate device) cannot reach. Returns true for `127.0.0.1`, `::1`,
/// and `localhost` on any port.
pub fn advertise_is_local(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(d)) => d == "localhost",
        Some(Host::Ipv4(a)) => a.is_loopback(),
        Some(Host::Ipv6(a)) => a.is_loopback(),
        None => false,
    }
}

#[derive(Debug, Clone)]
pub enum DoorbellSource {
    Url(Url),
    File(PathBuf),
}

/// Pure: pick the doorbell source from the two mutually exclusive flags.
/// Returns `Ok(None)` when neither is set (valid if no doorbell action is used).
pub fn choose_doorbell(
    url: Option<Url>,
    file: Option<PathBuf>,
) -> Result<Option<DoorbellSource>, String> {
    match (url, file) {
        (None, None) => Ok(None),
        (Some(u), None) => Ok(Some(DoorbellSource::Url(u))),
        (None, Some(f)) => Ok(Some(DoorbellSource::File(f))),
        (Some(_), Some(_)) => {
            Err("--doorbell-url and --doorbell-file are mutually exclusive".into())
        }
    }
}

/// Pure: the externally-fetchable URL for the served doorbell mp3.
pub fn doorbell_cast_url(advertise: &Url) -> String {
    let base = advertise.as_str().trim_end_matches('/');
    format!("{base}/doorbell.mp3")
}

pub fn build(cli: Cli) -> Result<Config, String> {
    let mut devices = HashMap::new();
    for d in &cli.device {
        let (alias, ip) = parse_device(d)?;
        devices.insert(alias, ip);
    }

    let mut routes = HashMap::new();
    for e in &cli.endpoint {
        let (path, aliases) = parse_endpoint(e, &devices)?;
        routes.insert(path, aliases);
    }

    let mut actions: HashMap<String, Vec<Action>> = HashMap::new();
    for a in &cli.action {
        let (alias, action) = parse_action(a, &devices)?;
        actions.entry(alias).or_default().push(action);
    }

    // `resume` is only valid as the last action for a device (it resumes the
    // stream that was playing before the doorbell interrupted it; anything
    // after it would just interrupt again).
    for (alias, acts) in &actions {
        if let Some(pos) = acts.iter().position(|a| matches!(a, Action::Resume)) {
            if pos != acts.len() - 1 {
                return Err(format!(
                    "--action {alias}:resume must be the last action for device '{alias}'"
                ));
            }
        }
    }

    let doorbell = choose_doorbell(cli.doorbell_url, cli.doorbell_file.clone())?;
    let (doorbell_url, doorbell_bytes) = match &doorbell {
        None => (None, None),
        Some(DoorbellSource::Url(u)) => (Some(u.as_str().to_string()), None),
        Some(DoorbellSource::File(p)) => {
            let adv = cli
                .advertise_url
                .as_ref()
                .ok_or_else(|| "--advertise-url required for --doorbell-file".to_string())?;
            let bytes = std::fs::read(p)
                .map_err(|e| format!("read doorbell file '{}': {e}", p.display()))?;
            (Some(doorbell_cast_url(adv)), Some(bytes))
        }
    };

    let has = |a: Action| actions.values().any(|v| v.contains(&a));
    if has(Action::Doorbell) && doorbell_url.is_none() {
        return Err("doorbell-url or doorbell-file required for doorbell actions".into());
    }
    if has(Action::Livestream) && cli.livestream_url.is_none() {
        return Err("--livestream-url required for livestream actions".into());
    }
    let needs_advertise = actions.values().any(|v| {
        v.iter()
            .any(|a| matches!(a, Action::Thumbnail | Action::TimedThumbnail(_)))
    });
    if needs_advertise && cli.advertise_url.is_none() {
        return Err("--advertise-url required for image actions".into());
    }

    Ok(Config {
        listen: cli.listen,
        advertise_url: cli.advertise_url,
        devices,
        routes,
        actions,
        doorbell_url,
        doorbell_bytes,
        livestream_url: cli.livestream_url,
        thumbnails: ThumbnailCache::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    fn devices() -> HashMap<String, String> {
        let mut d = HashMap::new();
        d.insert("lr".into(), "192.168.1.50".into());
        d.insert("kit".into(), "192.168.1.51".into());
        d
    }

    #[test]
    fn parse_device_ok() {
        assert_eq!(
            parse_device("lr:192.168.1.50").unwrap(),
            ("lr".into(), "192.168.1.50".into())
        );
    }

    #[test]
    fn parse_device_bad_ip() {
        assert!(parse_device("lr:999.1.1.1").is_err());
        assert!(parse_device("lr").is_err());
        assert!(parse_device("lr:notanip").is_err());
    }

    #[test]
    fn parse_action_kind_image_timed() {
        assert_eq!(
            parse_action_kind("thumbnail:30s").unwrap(),
            Action::TimedThumbnail(Duration::from_secs(30))
        );
        assert_eq!(
            parse_action_kind(" thumbnail:500ms ").unwrap(),
            Action::TimedThumbnail(Duration::from_millis(500))
        );
    }

    #[test]
    fn parse_action_kind_image_timed_bad() {
        assert!(parse_action_kind("thumbnail:").is_err());
        assert!(parse_action_kind("thumbnail:abc").is_err());
        assert!(parse_action_kind("thumbnail:5").is_err());
    }

    #[test]
    fn parse_action_kind_resume() {
        assert_eq!(parse_action_kind("resume").unwrap(), Action::Resume);
        assert_eq!(parse_action_kind(" resume ").unwrap(), Action::Resume);
    }

    #[test]
    fn parse_action_kind_ok() {
        assert_eq!(parse_action_kind("doorbell").unwrap(), Action::Doorbell);
        assert_eq!(parse_action_kind(" thumbnail ").unwrap(), Action::Thumbnail);
        assert_eq!(parse_action_kind("livestream").unwrap(), Action::Livestream);
    }

    #[test]
    fn parse_action_kind_sleep() {
        assert_eq!(
            parse_action_kind("sleep:2s").unwrap(),
            Action::Sleep(Duration::from_secs(2))
        );
        assert_eq!(
            parse_action_kind(" sleep:500ms ").unwrap(),
            Action::Sleep(Duration::from_millis(500))
        );
    }

    #[test]
    fn parse_action_kind_sleep_bad() {
        assert!(parse_action_kind("sleep:").is_err());
        assert!(parse_action_kind("sleep:abc").is_err());
        assert!(parse_action_kind("sleep:2").is_err());
    }

    #[test]
    fn parse_duration_units() {
        assert_eq!(parse_duration("0s").unwrap(), Duration::ZERO);
        assert_eq!(parse_duration("3s").unwrap(), Duration::from_secs(3));
        assert_eq!(parse_duration("750ms").unwrap(), Duration::from_millis(750));
    }

    #[test]
    fn parse_duration_bad() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("2").is_err());
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("2m").is_err());
    }

    #[test]
    fn parse_action_kind_unknown() {
        assert!(parse_action_kind("door").is_err());
        assert!(parse_action_kind("").is_err());
        assert!(parse_action_kind("sleep").is_err());
    }

    #[test]
    fn parse_endpoint_ok() {
        let (p, a) = parse_endpoint("/doorbell-lr:lr", &devices()).unwrap();
        assert_eq!(p, "/doorbell-lr");
        assert_eq!(a, vec!["lr".to_string()]);
    }

    #[test]
    fn parse_endpoint_multi() {
        let (_, a) = parse_endpoint("/alert-all:lr, kit", &devices()).unwrap();
        assert_eq!(a, vec!["lr".to_string(), "kit".to_string()]);
    }

    #[test]
    fn parse_endpoint_short_path() {
        assert!(parse_endpoint("/short:lr", &devices()).is_err());
    }

    #[test]
    fn parse_endpoint_unknown_device() {
        assert!(parse_endpoint("/doorbell-x:ghost", &devices()).is_err());
    }

    #[test]
    fn parse_endpoint_empty_aliases() {
        assert!(parse_endpoint("/doorbell-x:,", &devices()).is_err());
    }

    #[test]
    fn parse_action_unknown_alias() {
        assert!(parse_action("ghost:doorbell", &devices()).is_err());
    }

    fn cli(args: &[&str]) -> Cli {
        let mut full = vec!["castbell"];
        full.extend_from_slice(args);
        Cli::parse_from(full)
    }

    #[test]
    fn build_ok() {
        let cfg = build(cli(&[
            "--device",
            "lr:192.168.1.50",
            "--endpoint",
            "/doorbell-lr:lr",
            "--action",
            "lr:doorbell",
            "--action",
            "lr:thumbnail",
            "--doorbell-url",
            "https://ex/d.mp3",
            "--advertise-url",
            "http://10.0.0.1:8080",
        ]))
        .unwrap();
        assert_eq!(cfg.devices.get("lr").unwrap(), "192.168.1.50");
        assert_eq!(
            cfg.routes.get("/doorbell-lr").unwrap(),
            &vec!["lr".to_string()]
        );
        assert_eq!(
            cfg.actions.get("lr").unwrap(),
            &vec![Action::Doorbell, Action::Thumbnail]
        );
        assert_eq!(cfg.listen, "127.0.0.1:8080".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn build_missing_doorbell_source() {
        let err = build(cli(&[
            "--device",
            "lr:192.168.1.50",
            "--endpoint",
            "/doorbell-lr:lr",
            "--action",
            "lr:doorbell",
        ]))
        .expect_err("should fail");
        assert!(err.contains("doorbell"));
    }

    #[test]
    fn build_missing_advertise_url_for_thumbnail() {
        let err = build(cli(&[
            "--device",
            "lr:192.168.1.50",
            "--endpoint",
            "/doorbell-lr:lr",
            "--action",
            "lr:thumbnail",
        ]))
        .expect_err("should fail");
        assert!(err.contains("advertise-url"));
    }

    #[test]
    fn build_missing_advertise_url_for_timed_thumbnail() {
        let err = build(cli(&[
            "--device",
            "lr:192.168.1.50",
            "--endpoint",
            "/doorbell-lr:lr",
            "--action",
            "lr:thumbnail:30s",
        ]))
        .expect_err("should fail");
        assert!(err.contains("advertise-url"));
    }

    #[test]
    fn build_missing_livestream_url() {
        let err = build(cli(&[
            "--device",
            "lr:192.168.1.50",
            "--endpoint",
            "/doorbell-lr:lr",
            "--action",
            "lr:livestream",
        ]))
        .expect_err("should fail");
        assert!(err.contains("livestream-url"));
    }

    #[test]
    fn build_resume_not_last_rejected() {
        let err = build(cli(&[
            "--device",
            "lr:192.168.1.50",
            "--endpoint",
            "/doorbell-lr:lr",
            "--action",
            "lr:doorbell",
            "--action",
            "lr:resume",
            "--action",
            "lr:thumbnail",
            "--doorbell-url",
            "https://ex/d.mp3",
            "--advertise-url",
            "http://10.0.0.1:8080",
        ]))
        .expect_err("should fail");
        assert!(err.contains("resume must be the last action"));
    }

    #[test]
    fn build_resume_last_ok() {
        let cfg = build(cli(&[
            "--device",
            "lr:192.168.1.50",
            "--endpoint",
            "/doorbell-lr:lr",
            "--action",
            "lr:doorbell",
            "--action",
            "lr:sleep:2s",
            "--action",
            "lr:resume",
            "--doorbell-url",
            "https://ex/d.mp3",
        ]))
        .unwrap();
        assert_eq!(cfg.actions.get("lr").unwrap().last(), Some(&Action::Resume));
    }

    #[test]
    fn build_no_actions_ok() {
        // endpoint routes to a device with no actions: valid config.
        let cfg = build(cli(&[
            "--device",
            "lr:192.168.1.50",
            "--endpoint",
            "/doorbell-lr:lr",
        ]))
        .unwrap();
        assert!(cfg.actions.is_empty());
    }

    #[test]
    fn choose_doorbell_none() {
        assert!(choose_doorbell(None, None).unwrap().is_none());
    }

    #[test]
    fn choose_doorbell_url() {
        let u: Url = "https://ex/d.mp3".parse().unwrap();
        assert!(matches!(
            choose_doorbell(Some(u), None).unwrap(),
            Some(DoorbellSource::Url(_))
        ));
    }

    #[test]
    fn choose_doorbell_file() {
        assert!(matches!(
            choose_doorbell(None, Some(PathBuf::from("a.mp3"))).unwrap(),
            Some(DoorbellSource::File(_))
        ));
    }

    #[test]
    fn choose_doorbell_both_rejected() {
        let u: Url = "https://ex/d.mp3".parse().unwrap();
        assert!(choose_doorbell(Some(u), Some(PathBuf::from("a.mp3"))).is_err());
    }

    #[test]
    fn doorbell_cast_url_strips_slash() {
        let u: Url = "http://10.0.0.1:8080/".parse().unwrap();
        assert_eq!(doorbell_cast_url(&u), "http://10.0.0.1:8080/doorbell.mp3");
    }

    #[test]
    fn advertise_is_local_detects_loopback() {
        assert!(advertise_is_local(
            &"http://127.0.0.1:8080".parse().unwrap()
        ));
        assert!(advertise_is_local(
            &"http://localhost:8080".parse().unwrap()
        ));
        assert!(advertise_is_local(&"http://[::1]:8080".parse().unwrap()));
    }

    #[test]
    fn advertise_is_local_false_for_lan() {
        assert!(!advertise_is_local(
            &"http://192.168.1.10:8080".parse().unwrap()
        ));
        assert!(!advertise_is_local(
            &"http://10.0.0.1:8080".parse().unwrap()
        ));
    }

    #[test]
    fn build_with_doorbell_file() {
        let path = std::env::temp_dir().join("castbell_test_d.mp3");
        std::fs::write(&path, b"ID3FAKE").unwrap();
        let cfg = build(cli(&[
            "--device",
            "lr:192.168.1.50",
            "--endpoint",
            "/doorbell-lr:lr",
            "--action",
            "lr:doorbell",
            "--doorbell-file",
            path.to_str().unwrap(),
            "--advertise-url",
            "http://10.0.0.1:8080",
        ]))
        .unwrap();
        assert_eq!(
            cfg.doorbell_url.as_deref(),
            Some("http://10.0.0.1:8080/doorbell.mp3")
        );
        assert_eq!(cfg.doorbell_bytes.as_deref(), Some(b"ID3FAKE".as_slice()));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn build_doorbell_file_without_advertise_fails() {
        let path = std::env::temp_dir().join("castbell_test_d2.mp3");
        std::fs::write(&path, b"ID3FAKE").unwrap();
        let err = build(cli(&[
            "--device",
            "lr:192.168.1.50",
            "--endpoint",
            "/doorbell-lr:lr",
            "--action",
            "lr:doorbell",
            "--doorbell-file",
            path.to_str().unwrap(),
        ]))
        .expect_err("should fail");
        assert!(err.contains("advertise-url"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn build_doorbell_url_and_file_mutually_exclusive() {
        let path = std::env::temp_dir().join("castbell_test_d3.mp3");
        std::fs::write(&path, b"ID3FAKE").unwrap();
        let err = build(cli(&[
            "--device",
            "lr:192.168.1.50",
            "--endpoint",
            "/doorbell-lr:lr",
            "--action",
            "lr:doorbell",
            "--doorbell-url",
            "https://ex/d.mp3",
            "--doorbell-file",
            path.to_str().unwrap(),
            "--advertise-url",
            "http://10.0.0.1:8080",
        ]))
        .expect_err("should fail");
        assert!(err.contains("mutually exclusive"));
        let _ = std::fs::remove_file(&path);
    }
}
