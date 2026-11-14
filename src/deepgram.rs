//! Deepgram nova-3 (`"transcriber": "deepgram"`, the default on macOS): the recording's WAV goes
//! to Deepgram's pre-recorded API and its measured word times and punctuation come back.
//!
//! The key is `DEEPGRAM_API_KEY` from the environment, else from `~/.config/codecaine/env`
//! (`KEY=value` lines). It reaches `/usr/bin/curl` only on stdin (`--config -`), never in argv, a
//! log or an error. Every failure is a fixed reason (`Failure`), never what Deepgram answered.
//! The caller falls back to Apple Speech on any of them.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::Deserialize;

use crate::model::{TranscriptData, TranscriptSegment, WordTiming};

pub const MODEL: &str = "nova-3";
pub const TITLE: &str = "Deepgram nova-3";
const ENDPOINT: &str = "https://api.deepgram.com/v1/listen";
const KEY_NAME: &str = "DEEPGRAM_API_KEY";
const MAX_TIME: Duration = Duration::from_secs(120);
/// curl's exit code when `--max-time` runs out.
const CURL_TIMEOUT: i32 = 28;

/// Why Deepgram gave no transcript. Each one names what went wrong, never the key or the reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    NoKey,
    Timeout,
    Status(u16),
    Empty,
    /// curl could not run, or exited with this code before an answer.
    Curl(Option<i32>),
    BadReply,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoKey => write!(f, "no {KEY_NAME}"),
            Self::Timeout => write!(f, "no answer within {} s", MAX_TIME.as_secs()),
            Self::Status(code) => write!(f, "Deepgram answered HTTP {code}"),
            Self::Empty => f.write_str("Deepgram heard no words"),
            Self::Curl(Some(code)) => write!(f, "curl failed (exit {code})"),
            Self::Curl(None) => f.write_str("curl could not run"),
            Self::BadReply => f.write_str("Deepgram's answer could not be read"),
        }
    }
}

impl std::error::Error for Failure {}

/// The key from the environment, else from `~/.config/codecaine/env`.
pub fn key() -> Option<String> {
    let file = std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".config/codecaine/env"))
        .and_then(|path| std::fs::read_to_string(path).ok());
    key_from(std::env::var(KEY_NAME).ok(), file.as_deref())
}

fn key_from(env: Option<String>, file: Option<&str>) -> Option<String> {
    let usable = |value: &str| {
        let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
        (!value.is_empty()).then(|| value.to_owned())
    };
    env.as_deref().and_then(usable).or_else(|| {
        file?.lines().find_map(|line| {
            let line = line.trim();
            let line = line.strip_prefix("export ").unwrap_or(line);
            let (name, value) = line.split_once('=')?;
            (name.trim() == KEY_NAME).then(|| usable(value)).flatten()
        })
    })
}

/// The request's query: nova-3 with punctuation and utterances, and the language (`hexLanguage`):
/// a code sends `language=<code>`, `"auto"` lets Deepgram detect it.
fn query(language: &str) -> String {
    let language = match crate::hex::language_code(language) {
        Some(code) if code == "auto" => "detect_language=true".to_owned(),
        Some(code) => format!("language={code}"),
        None => format!("language={}", crate::hex::DEFAULT_LANGUAGE),
    };
    format!("model={MODEL}&smart_format=true&punctuate=true&utterances=true&{language}")
}

/// Transcribes `wav` with the configured language.
pub fn transcribe(wav: &Path, language: &str) -> Result<TranscriptData, Failure> {
    let key = key().ok_or(Failure::NoKey)?;
    post(
        &format!("{ENDPOINT}?{}", query(language)),
        &key,
        wav,
        MAX_TIME,
    )
}

/// A curl config value in double quotes.
fn quoted(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn post(url: &str, key: &str, wav: &Path, max_time: Duration) -> Result<TranscriptData, Failure> {
    // The body goes to stdout, then a newline and the HTTP code, so the code is read apart from it.
    let config = format!(
        "url = {}\nheader = {}\nheader = \"Content-Type: audio/wav\"\ndata-binary = {}\n\
         max-time = {}\nsilent\nshow-error\nwrite-out = \"\\n%{{http_code}}\"\n",
        quoted(url),
        quoted(&format!("Authorization: Token {key}")),
        quoted(&format!("@{}", wav.display())),
        max_time.as_secs().max(1),
    );
    let mut child = Command::new("/usr/bin/curl")
        .args(["--config", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| Failure::Curl(None))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(config.as_bytes())
            .map_err(|_| Failure::Curl(None))?;
    }
    let output = child.wait_with_output().map_err(|_| Failure::Curl(None))?;
    reply(output.status.code(), &output.stdout)
}

/// What curl's exit code and stdout (body, newline, HTTP code) mean.
fn reply(exit: Option<i32>, stdout: &[u8]) -> Result<TranscriptData, Failure> {
    match exit {
        Some(0) => {}
        Some(CURL_TIMEOUT) => return Err(Failure::Timeout),
        other => return Err(Failure::Curl(other)),
    }
    let split = stdout
        .iter()
        .rposition(|&b| b == b'\n')
        .ok_or(Failure::BadReply)?;
    let (body, code) = (&stdout[..split], &stdout[split + 1..]);
    let code: u16 = std::str::from_utf8(code)
        .ok()
        .and_then(|code| code.trim().parse().ok())
        .ok_or(Failure::BadReply)?;
    if code != 200 {
        return Err(Failure::Status(code));
    }
    let response: Response = serde_json::from_slice(body).map_err(|_| Failure::BadReply)?;
    transcript(response)
}

#[derive(Deserialize)]
struct Response {
    results: Results,
}

#[derive(Deserialize)]
struct Results {
    #[serde(default)]
    channels: Vec<Channel>,
    #[serde(default)]
    utterances: Vec<Utterance>,
}

#[derive(Deserialize)]
struct Channel {
    #[serde(default)]
    alternatives: Vec<Alternative>,
}

#[derive(Deserialize)]
struct Alternative {
    #[serde(default)]
    transcript: String,
    #[serde(default)]
    words: Vec<Word>,
}

#[derive(Deserialize)]
struct Word {
    word: String,
    #[serde(default)]
    punctuated_word: Option<String>,
    start: f64,
    end: f64,
}

#[derive(Deserialize)]
struct Utterance {
    start: f64,
    end: f64,
    transcript: String,
}

fn ms(seconds: f64) -> u64 {
    (seconds.max(0.0) * 1_000.0).round() as u64
}

fn transcript(response: Response) -> Result<TranscriptData, Failure> {
    let Results {
        channels,
        utterances,
    } = response.results;
    let alternative = channels
        .into_iter()
        .next()
        .and_then(|channel| channel.alternatives.into_iter().next())
        .ok_or(Failure::Empty)?;
    let text = alternative.transcript.trim().to_owned();
    let words: Vec<WordTiming> = alternative
        .words
        .into_iter()
        .map(|word| WordTiming {
            t0: ms(word.start),
            t1: ms(word.end).max(ms(word.start)),
            text: word.punctuated_word.unwrap_or(word.word),
        })
        .filter(|word| !word.text.trim().is_empty())
        .collect();
    if text.is_empty() || words.is_empty() {
        return Err(Failure::Empty);
    }
    let mut segments: Vec<TranscriptSegment> = utterances
        .into_iter()
        .filter(|u| !u.transcript.trim().is_empty())
        .map(|u| TranscriptSegment {
            t0: ms(u.start),
            t1: ms(u.end),
            text: u.transcript.trim().to_owned(),
        })
        .collect();
    if segments.is_empty() {
        segments.push(TranscriptSegment {
            t0: words[0].t0,
            t1: words[words.len() - 1].t1,
            text: text.clone(),
        });
    }
    segments.sort_by_key(|s| s.t0);
    Ok(TranscriptData {
        source: format!("deepgram:{MODEL}"),
        model: Some(MODEL.into()),
        text,
        segments,
        words,
        estimated: false,
        missing: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::io::{BufRead, BufReader, Read};
    #[cfg(unix)]
    use std::net::TcpListener;

    /// The shape of a real nova-3 pre-recorded answer (`smart_format`, `utterances`), trimmed.
    const FIXTURE: &str = r#"{
      "metadata": {"request_id": "00000000-0000-0000-0000-000000000000", "duration": 4.2,
                   "channels": 1, "models": ["2187e11a-3532-4498-b076-81fa530bdd49"],
                   "model_info": {"2187e11a-3532-4498-b076-81fa530bdd49":
                                  {"name": "general-nova-3", "version": "2025-04-17.21547", "arch": "nova-3"}}},
      "results": {
        "channels": [{
          "alternatives": [{
            "transcript": "Move this button left. Then make it red.",
            "confidence": 0.998,
            "words": [
              {"word": "move", "start": 0.24, "end": 0.48, "confidence": 0.99, "punctuated_word": "Move"},
              {"word": "this", "start": 0.48, "end": 0.64, "confidence": 0.99, "punctuated_word": "this"},
              {"word": "button", "start": 0.64, "end": 0.96, "confidence": 0.99, "punctuated_word": "button"},
              {"word": "left", "start": 0.96, "end": 1.4649999, "confidence": 0.99, "punctuated_word": "left."},
              {"word": "then", "start": 2.08, "end": 2.32, "confidence": 0.99, "punctuated_word": "Then"},
              {"word": "make", "start": 2.32, "end": 2.56, "confidence": 0.99, "punctuated_word": "make"},
              {"word": "it", "start": 2.56, "end": 2.72, "confidence": 0.99, "punctuated_word": "it"},
              {"word": "red", "start": 2.72, "end": 3.12, "confidence": 0.99, "punctuated_word": "red."}
            ],
            "paragraphs": {"transcript": "\nMove this button left. Then make it red.", "paragraphs": []}
          }],
          "detected_language": "en"
        }],
        "utterances": [
          {"start": 0.24, "end": 1.4649999, "confidence": 0.99, "channel": 0,
           "transcript": "Move this button left.", "words": [], "id": "a"},
          {"start": 2.08, "end": 3.12, "confidence": 0.99, "channel": 0,
           "transcript": "Then make it red.", "words": [], "id": "b"}
        ]
      }
    }"#;

    fn answered(code: u16, body: &str) -> Vec<u8> {
        format!("{body}\n{code}").into_bytes()
    }

    #[test]
    fn deepgram_words_carry_punctuation_and_measured_ms() {
        let t = reply(Some(0), &answered(200, FIXTURE)).unwrap();
        assert_eq!(t.source, "deepgram:nova-3");
        assert_eq!(t.text, "Move this button left. Then make it red.");
        assert!(!t.estimated);
        let words: Vec<_> = t
            .words
            .iter()
            .map(|w| (w.t0, w.t1, w.text.as_str()))
            .collect();
        assert_eq!(words[0], (240, 480, "Move"));
        assert_eq!(words[3], (960, 1_465, "left."));
        assert_eq!(words[7], (2_720, 3_120, "red."));
        assert_eq!(
            t.segments,
            vec![
                TranscriptSegment {
                    t0: 240,
                    t1: 1_465,
                    text: "Move this button left.".into()
                },
                TranscriptSegment {
                    t0: 2_080,
                    t1: 3_120,
                    text: "Then make it red.".into()
                },
            ]
        );
    }

    #[test]
    fn deepgram_without_utterances_is_one_segment() {
        let body = FIXTURE.replace("\"utterances\"", "\"ignored\"");
        let t = reply(Some(0), &answered(200, &body)).unwrap();
        assert_eq!(t.segments.len(), 1);
        assert_eq!((t.segments[0].t0, t.segments[0].t1), (240, 3_120));
    }

    #[test]
    fn deepgram_failures_are_fixed_reasons() {
        let empty = r#"{"results":{"channels":[{"alternatives":[{"transcript":"","words":[]}]}]}}"#;
        assert_eq!(reply(Some(0), &answered(200, empty)), Err(Failure::Empty));
        assert_eq!(
            reply(Some(0), &answered(200, r#"{"results":{"channels":[]}}"#)),
            Err(Failure::Empty)
        );
        let refused = answered(
            401,
            r#"{"err_code":"INVALID_AUTH","err_msg":"secret detail"}"#,
        );
        let failure = reply(Some(0), &refused).unwrap_err();
        assert_eq!(failure, Failure::Status(401));
        assert_eq!(failure.to_string(), "Deepgram answered HTTP 401");
        assert_eq!(reply(Some(28), b""), Err(Failure::Timeout));
        assert_eq!(reply(Some(6), b""), Err(Failure::Curl(Some(6))));
        assert_eq!(
            reply(Some(0), &answered(200, "not json")),
            Err(Failure::BadReply)
        );
    }

    #[test]
    fn deepgram_key_comes_from_the_environment_else_the_env_file() {
        let file = "# keys\nOTHER=1\nexport DEEPGRAM_API_KEY=\"from-file\"\n";
        assert_eq!(
            key_from(Some("from-env".into()), Some(file)).as_deref(),
            Some("from-env")
        );
        assert_eq!(
            key_from(Some("  ".into()), Some(file)).as_deref(),
            Some("from-file")
        );
        assert_eq!(
            key_from(None, Some("DEEPGRAM_API_KEY=plain\n")).as_deref(),
            Some("plain")
        );
        assert_eq!(key_from(None, Some("DEEPGRAM_API_KEY=\n")), None);
        assert_eq!(key_from(None, None), None);
    }

    #[test]
    fn deepgram_language_is_sent_or_detected() {
        assert!(query("nl").ends_with("&language=nl"));
        assert!(query("auto").ends_with("&detect_language=true"));
        assert!(query("bad value!").ends_with("&language=en"));
        assert!(
            query("en")
                .starts_with("model=nova-3&smart_format=true&punctuate=true&utterances=true")
        );
    }

    /// Answers one request on a loopback port with `status` and `body` after `delay`, and hands
    /// back the request's head (request line and headers). The `post` tests run where
    /// `/usr/bin/curl` is, and Windows file names can't hold the quote `wav` puts in one.
    #[cfg(unix)]
    fn fake_deepgram(
        status: u16,
        body: &'static str,
        delay: Duration,
    ) -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://127.0.0.1:{}/v1/listen?{}",
            listener.local_addr().unwrap().port(),
            query("en")
        );
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let (mut head, mut length) = (String::new(), 0);
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap_or(0);
                }
                head.push_str(&line);
            }
            let mut upload = vec![0; length];
            let _ = reader.read_exact(&mut upload);
            let _ = sender.send(head);
            std::thread::sleep(delay);
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        });
        (url, receiver)
    }

    /// A WAV whose path has a space and a quote, as curl's config must quote it.
    #[cfg(unix)]
    fn wav(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "scrawl-deepgram-test-{}-{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("audio \"file\".wav");
        std::fs::write(&path, b"RIFF fake wav").unwrap();
        path
    }

    #[test]
    #[cfg(unix)]
    fn deepgram_post_sends_the_key_as_a_header_and_reads_the_answer() {
        let (url, head) = fake_deepgram(200, FIXTURE, Duration::ZERO);
        let audio = wav("ok");
        let t = post(&url, "test-key", &audio, Duration::from_secs(10)).unwrap();
        let _ = std::fs::remove_dir_all(audio.parent().unwrap());
        assert_eq!(t.words.len(), 8);
        let head = head.recv().unwrap();
        assert!(head.starts_with("POST /v1/listen?model=nova-3&"));
        assert!(head.contains("Authorization: Token test-key"));
        assert!(head.contains("Content-Type: audio/wav"));
    }

    #[test]
    #[cfg(unix)]
    fn deepgram_post_reports_non_200_and_timeout() {
        let audio = wav("fail");
        let (url, _head) = fake_deepgram(402, r#"{"err_msg":"x"}"#, Duration::ZERO);
        let refused = post(&url, "k", &audio, Duration::from_secs(10));
        let (url, _head) = fake_deepgram(200, FIXTURE, Duration::from_secs(3));
        let slow = post(&url, "k", &audio, Duration::from_secs(1));
        let _ = std::fs::remove_dir_all(audio.parent().unwrap());
        assert_eq!(refused, Err(Failure::Status(402)));
        assert_eq!(slow, Err(Failure::Timeout));
    }
}
