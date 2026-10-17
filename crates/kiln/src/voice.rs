//! Speech to text for the console's microphone, where the browser can't do it itself (Firefox, and
//! any browser without speech recognition): the recording is sent on to a speech-to-text service
//! that speaks OpenAI's transcription API (`POST /v1/audio/transcriptions`, multipart), such as
//! whisper.cpp's server, faster-whisper-server or LocalAI on a machine of yours, or OpenAI itself.
//! Set it as `[voice] transcribe_url`; without it there is no server-side transcription.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Largest recording sent on: OpenAI's own limit for a transcription.
pub const MAX_AUDIO: usize = 25 << 20;
/// Longest a transcription may take.
const TIMEOUT: Duration = Duration::from_secs(120);
/// Largest answer read.
const MAX_ANSWER: usize = 1 << 20;

/// `[voice]`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Voice {
    /// The service (`http://whisper.local:8080`, `https://api.openai.com/v1`, or the whole
    /// transcription URL); empty: no server-side transcription.
    pub transcribe_url: String,
    /// The model it is asked for (`whisper-1` for OpenAI; a local server may ignore it).
    pub model: String,
    /// A file holding the service's API key, sent as a bearer token (a hosted service).
    pub api_key_file: Option<PathBuf>,
    /// The spoken language (`en`), or empty for the service to tell.
    pub language: String,
}

impl Default for Voice {
    fn default() -> Self {
        Self { transcribe_url: String::new(), model: "whisper-1".to_owned(), api_key_file: None, language: String::new() }
    }
}

impl Voice {
    pub fn enabled(&self) -> bool {
        !self.transcribe_url.trim().is_empty()
    }

    /// Where recordings are posted.
    fn endpoint(&self) -> String {
        let base = self.transcribe_url.trim().trim_end_matches('/');
        if base.ends_with("/audio/transcriptions") {
            base.to_owned()
        } else if base.ends_with("/v1") {
            format!("{base}/audio/transcriptions")
        } else {
            format!("{base}/v1/audio/transcriptions")
        }
    }
}

/// A recording's file name, as the service reads its kind from it.
fn file_name(content_type: &str) -> &'static str {
    match content_type.split(';').next().unwrap_or("").trim() {
        "audio/ogg" => "audio.ogg",
        "audio/mp4" | "audio/x-m4a" | "audio/aac" => "audio.m4a",
        "audio/wav" | "audio/x-wav" | "audio/wave" => "audio.wav",
        "audio/mpeg" | "audio/mp3" => "audio.mp3",
        "audio/flac" => "audio.flac",
        _ => "audio.webm",
    }
}

/// A `multipart/form-data` body: the text fields, then the recording.
fn multipart(boundary: &str, fields: &[(&str, &str)], name: &str, content_type: &str, audio: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(audio.len() + 512);
    for (field, value) in fields {
        body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"\r\n\r\n{value}\r\n").as_bytes());
    }
    body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\nContent-Type: {content_type}\r\n\r\n").as_bytes());
    body.extend_from_slice(audio);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

/// The words in a recording (`content_type` as the browser recorded it; `language` overrides the
/// configured one).
pub async fn transcribe(voice: &Voice, audio: &[u8], content_type: &str, language: Option<&str>) -> anyhow::Result<String> {
    anyhow::ensure!(voice.enabled(), "voice transcription isn't set up on this server: set [voice] transcribe_url to a speech-to-text service");
    anyhow::ensure!(content_type.starts_with("audio/"), "not a recording ({content_type})");
    anyhow::ensure!(!audio.is_empty() && audio.len() <= MAX_AUDIO, "a recording is up to {} MiB", MAX_AUDIO >> 20);
    let boundary = format!("kiln-{}", crate::util::new_id());
    let lang = language.filter(|l| !l.trim().is_empty()).unwrap_or(&voice.language).trim();
    let mut fields: Vec<(&str, &str)> = vec![("model", voice.model.as_str()), ("response_format", "json")];
    if !lang.is_empty() {
        fields.push(("language", lang));
    }
    let body = multipart(&boundary, &fields, file_name(content_type), content_type, audio);
    let client = crate::http::client::Client::public()?;
    let mut req = client.post(&voice.endpoint()).header("content-type", &format!("multipart/form-data; boundary={boundary}")).timeout(TIMEOUT).body(body);
    if let Some(file) = &voice.api_key_file {
        let key = std::fs::read_to_string(file).map_err(|e| anyhow::anyhow!("voice api_key_file {}: {e}", file.display()))?;
        req = req.header("authorization", &format!("Bearer {}", key.trim()));
    }
    let resp = req.send().await?;
    let status = resp.status();
    let ok = resp.is_success();
    let bytes = resp.bytes(MAX_ANSWER).await?;
    anyhow::ensure!(ok, "the speech-to-text service answered {status}: {}", String::from_utf8_lossy(bytes.get(..bytes.len().min(300)).unwrap_or_default()).trim());
    #[derive(Deserialize)]
    struct Answer {
        text: String,
    }
    let a: Answer = serde_json::from_slice(&bytes).map_err(|e| anyhow::anyhow!("the speech-to-text service's answer isn't a transcription: {e}"))?;
    Ok(a.text.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recording_goes_as_openai_transcription_takes_it() {
        let v = Voice { transcribe_url: "http://whisper.local:8080/".into(), ..Voice::default() };
        assert_eq!(v.endpoint(), "http://whisper.local:8080/v1/audio/transcriptions");
        let full = Voice { transcribe_url: "https://api.openai.com/v1/audio/transcriptions".into(), ..Voice::default() };
        assert_eq!(full.endpoint(), "https://api.openai.com/v1/audio/transcriptions");
        let v1 = Voice { transcribe_url: "https://api.openai.com/v1".into(), ..Voice::default() };
        assert_eq!(v1.endpoint(), full.endpoint());
        assert!(!Voice::default().enabled() && v.enabled());
        assert_eq!((file_name("audio/webm;codecs=opus"), file_name("audio/mp4"), file_name("audio/x-wav")), ("audio.webm", "audio.m4a", "audio.wav"));
        let body = multipart("B", &[("model", "whisper-1")], "audio.webm", "audio/webm", b"OPUS");
        let text = String::from_utf8_lossy(&body);
        assert!(text.starts_with("--B\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nwhisper-1\r\n"));
        assert!(text.contains("name=\"file\"; filename=\"audio.webm\"\r\nContent-Type: audio/webm\r\n\r\nOPUS\r\n--B--\r\n"));
    }

    #[test]
    fn without_a_service_it_says_how_to_set_one_up() {
        let rt = futures::executor::block_on(transcribe(&Voice::default(), b"x", "audio/webm", None));
        assert!(rt.err().is_some_and(|e| e.to_string().contains("[voice] transcribe_url")));
        let on = Voice { transcribe_url: "http://127.0.0.1:9".into(), ..Voice::default() };
        let not_audio = futures::executor::block_on(transcribe(&on, b"x", "text/plain", None));
        assert!(not_audio.err().is_some_and(|e| e.to_string().contains("not a recording")));
    }
}
