use base64::Engine;
use serde::Deserialize;

#[derive(Deserialize, Debug)]
pub struct AlarmPayload {
    pub alarm: Alarm,
}

#[derive(Deserialize, Debug)]
pub struct Alarm {
    #[serde(default)]
    pub thumbnail: Option<String>,
    #[serde(default)]
    pub triggers: Vec<Trigger>,
}

#[derive(Deserialize, Debug)]
pub struct Trigger {
    #[serde(default, rename = "eventId")]
    pub event_id: Option<String>,
    #[serde(default, rename = "sourceEvent")]
    pub source_event: Option<SourceEvent>,
}

#[derive(Deserialize, Debug)]
pub struct SourceEvent {
    #[serde(default)]
    pub id: Option<String>,
}

/// Resolve the event id from triggers: prefer `eventId`, fall back to `sourceEvent.id`.
pub fn resolve_event_id(triggers: &[Trigger]) -> Result<String, String> {
    let t = triggers
        .first()
        .ok_or_else(|| "payload has no triggers".to_string())?;
    if let Some(id) = &t.event_id {
        return Ok(id.clone());
    }
    if let Some(se) = &t.source_event {
        if let Some(id) = &se.id {
            return Ok(id.clone());
        }
    }
    Err("trigger has no event id".into())
}

impl AlarmPayload {
    pub fn event_id(&self) -> Result<String, String> {
        resolve_event_id(&self.alarm.triggers)
    }

    pub fn image_bytes(&self) -> Result<Vec<u8>, String> {
        let uri = self
            .alarm
            .thumbnail
            .as_ref()
            .ok_or_else(|| "payload has no alarm.thumbnail".to_string())?;
        parse_data_uri(uri)
    }
}

/// Decode a `data:image/jpeg;base64,...` URI into raw JPEG bytes.
pub fn parse_data_uri(uri: &str) -> Result<Vec<u8>, String> {
    const PREFIX: &str = "data:image/jpeg;base64,";
    let b64 = uri
        .strip_prefix(PREFIX)
        .ok_or_else(|| format!("thumbnail is not a '{PREFIX}...' data uri"))?;
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| format!("base64 decode: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_data_uri_ok() {
        // "AAAA" -> 3 zero bytes
        let bytes = parse_data_uri("data:image/jpeg;base64,AAAA").unwrap();
        assert_eq!(bytes, vec![0, 0, 0]);
    }

    #[test]
    fn parse_data_uri_wrong_prefix() {
        assert!(parse_data_uri("data:image/png;base64,AAAA").is_err());
        assert!(parse_data_uri("https://ex/a.jpg").is_err());
        assert!(parse_data_uri("").is_err());
    }

    #[test]
    fn parse_data_uri_bad_base64() {
        assert!(parse_data_uri("data:image/jpeg;base64,!@#$").is_err());
    }

    fn trig(eid: Option<&str>, sid: Option<&str>) -> Trigger {
        Trigger {
            event_id: eid.map(String::from),
            source_event: Some(SourceEvent {
                id: sid.map(String::from),
            }),
        }
    }

    #[test]
    fn resolve_event_id_prefers_event_id() {
        let ts = vec![trig(Some("evt-1"), Some("src-1"))];
        assert_eq!(resolve_event_id(&ts).unwrap(), "evt-1");
    }

    #[test]
    fn resolve_event_id_falls_back_to_source() {
        let ts = vec![trig(None, Some("src-1"))];
        assert_eq!(resolve_event_id(&ts).unwrap(), "src-1");
    }

    #[test]
    fn resolve_event_id_missing_both() {
        let ts = vec![trig(None, None)];
        assert!(resolve_event_id(&ts).is_err());
    }

    #[test]
    fn resolve_event_id_no_triggers() {
        assert!(resolve_event_id(&[]).is_err());
    }

    #[test]
    fn payload_roundtrip_from_real_shape() {
        let json = r#"{
            "alarm": {
                "name": "Ring - webhook",
                "triggers": [{
                    "key": "ring",
                    "eventId": "evt-abc-123",
                    "sourceEvent": {"id": "evt-abc-123", "type": "ring"}
                }],
                "thumbnail": "data:image/jpeg;base64,AAAA"
            },
            "timestamp": 1787498999168
        }"#;
        let p: AlarmPayload = serde_json::from_str(json).unwrap();
        assert_eq!(p.event_id().unwrap(), "evt-abc-123");
        assert_eq!(p.image_bytes().unwrap(), vec![0, 0, 0]);
    }

    #[test]
    fn payload_without_thumbnail_deserializes() {
        // Real UniFi Protect payloads may omit `thumbnail` (some event types).
        // Deserialization must not fail; image_bytes() errors only on demand.
        let json = r#"{
            "alarm": {
                "name": "Ring - webhook",
                "triggers": [{"eventId": "evt-no-thumb"}]
            },
            "timestamp": 1
        }"#;
        let p: AlarmPayload = serde_json::from_str(json).unwrap();
        assert_eq!(p.event_id().unwrap(), "evt-no-thumb");
        assert!(p.image_bytes().is_err());
    }
}
