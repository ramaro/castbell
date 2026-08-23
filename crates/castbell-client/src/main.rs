use base64::Engine;
use clap::Parser;

/// Mimic a UniFi Protect alarm webhook payload and POST it to castbell.
#[derive(Parser, Debug)]
#[command(name = "castbell-client")]
struct Cli {
    /// Full endpoint URL to POST to, e.g. http://127.0.0.1:8080/doorbell-lr
    #[arg(long)]
    url: Option<String>,

    /// Event id (default: test-event-<unix-millis>)
    #[arg(long)]
    event_id: Option<String>,

    /// Local JPEG file to embed as the thumbnail data URI.
    /// If omitted, a small synthetic (non-JPEG) stand-in is used — enough to
    /// exercise the server, but not a valid image for a real Chromecast.
    #[arg(long)]
    thumbnail: Option<String>,

    /// alarm.name
    #[arg(long, default_value = "Ring - webhook")]
    name: String,

    /// trigger key
    #[arg(long, default_value = "ring")]
    key: String,

    /// Print the payload to stdout instead of sending it.
    #[arg(long)]
    print: bool,
}

fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

fn thumbnail_data_uri(path: Option<&str>) -> String {
    let bytes: Vec<u8> = match path {
        Some(p) => std::fs::read(p).unwrap_or_else(|e| {
            eprintln!("read thumbnail '{p}': {e}");
            std::process::exit(1);
        }),
        None => b"doorbell-test-thumbnail-not-a-real-jpeg".to_vec(),
    };
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    format!("data:image/jpeg;base64,{b64}")
}

fn build_payload(cli: &Cli) -> serde_json::Value {
    let ts = now_millis();
    let id = cli
        .event_id
        .clone()
        .unwrap_or_else(|| format!("test-event-{ts}"));
    let thumb = thumbnail_data_uri(cli.thumbnail.as_deref());
    let event_path = format!("/protect/events/event/{id}");
    serde_json::json!({
        "alarm": {
            "name": cli.name,
            "sources": [{"device": "TESTDEVICE", "type": "include"}],
            "conditions": [{"condition": {"type": "is", "source": "ring"}}],
            "triggers": [{
                "key": cli.key,
                "device": "TESTDEVICE",
                "eventId": id,
                "timestamp": ts,
                "sourceEvent": {
                    "camera": "6469c7a103d41003e40003ef",
                    "createdAt": "2026-08-23T15:29:58.076717150Z",
                    "device": "6469c7a103d41003e40003ef",
                    "id": id,
                    "locked": false,
                    "modelKey": "event",
                    "score": 0,
                    "smartDetectTypes": [],
                    "start": ts,
                    "type": "ring",
                    "pk": id,
                    "_pk": id
                }
            }],
            "thumbnail": thumb,
            "eventPath": event_path,
            "eventLocalLink": format!("https://192.168.1.1{event_path}")
        },
        "timestamp": ts
    })
}

fn main() {
    let cli = Cli::parse();
    let payload = build_payload(&cli);
    let body = serde_json::to_string(&payload).unwrap();

    if cli.print {
        println!("{body}");
        return;
    }

    let url = match &cli.url {
        Some(u) => u.clone(),
        None => {
            eprintln!("--url is required (or use --print)");
            std::process::exit(1);
        }
    };

    match ureq::post(&url)
        .set("Content-Type", "application/json")
        .send_string(&body)
    {
        Ok(resp) => {
            println!("{} {}", resp.status(), resp.status_text());
            match resp.into_string() {
                Ok(s) if !s.is_empty() => println!("{s}"),
                _ => {}
            }
        }
        Err(e) => {
            eprintln!("request failed: {e}");
            std::process::exit(1);
        }
    }
}
