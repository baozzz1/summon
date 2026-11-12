//! OSC 7501 的有界增量解码。状态是展示提示，不能证明输入送达或一轮完成。
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};

pub const MAX_SEQUENCE: usize = 4096;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Idle,
    Working,
    Done,
    Blocked,
    Error,
    Clear,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Permission,
    Question,
    Auth,
}
impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Permission => summon_i18n::text!("rf163b179cf"),
            Self::Question => summon_i18n::text!("rb7d6f45459"),
            Self::Auth => summon_i18n::text!("r867eb2dc53"),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub state: State,
    pub id: Option<String>,
    pub app: Option<String>,
    pub kind: Option<Kind>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Query,
    Report(Report),
    Reset,
    ShellPrompt,
}

fn text_valid(value: &str, encoded: usize, decoded: usize) -> bool {
    if value.len() > encoded {
        return false;
    }
    let mut padded = value.to_string();
    while !padded.len().is_multiple_of(4) {
        padded.push('=');
    }
    STANDARD
        .decode(padded)
        .ok()
        .filter(|b| b.len() <= decoded)
        .and_then(|b| String::from_utf8(b).ok())
        .is_some_and(|s| !s.chars().any(char::is_control))
}
fn segment(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.+-".contains(&b))
}
fn report(body: &[u8]) -> Option<Event> {
    let raw = std::str::from_utf8(body).ok()?;
    if raw == "7501;?" {
        return Some(Event::Query);
    }
    if raw == "133;A" || raw.starts_with("133;A;") {
        return Some(Event::ShellPrompt);
    }
    let pairs = raw.strip_prefix("7501;")?;
    let mut state = None;
    let mut id = None;
    let mut app = None;
    let mut kind = None;
    for pair in pairs.split(':') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if key.len() > 16 {
            return None;
        }
        if key.is_empty()
            || !key.bytes().all(|b| b.is_ascii_lowercase())
            || !value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.,+/=-".contains(&b))
        {
            continue;
        }
        match key {
            "state" => {
                state = match value {
                    "idle" => Some(State::Idle),
                    "working" => Some(State::Working),
                    "done" => Some(State::Done),
                    "blocked" => Some(State::Blocked),
                    "error" => Some(State::Error),
                    "clear" => Some(State::Clear),
                    _ => None,
                }
            }
            "id" => {
                if value.len() > 128
                    || value.split('/').count() > 8
                    || !value.split('/').all(segment)
                {
                    return None;
                }
                id = Some(value.to_owned());
            }
            "app" => app = segment(value).then(|| value.to_owned()),
            "kind" => {
                kind = match value {
                    "permission" => Some(Kind::Permission),
                    "question" => Some(Kind::Question),
                    "auth" => Some(Kind::Auth),
                    _ => None,
                }
            }
            "msg" if !text_valid(value, 2732, 2048) => return None,
            "title" if !text_valid(value, 256, 192) => return None,
            _ => {}
        }
    }
    let state = state?;
    Some(Event::Report(Report {
        state,
        id,
        app,
        kind: if state == State::Blocked { kind } else { None },
    }))
}

/// 按字节推进，持有的缓冲区始终不超过一个协议序列。
#[derive(Default)]
pub struct Decoder {
    buffer: Vec<u8>,
    osc: bool,
    escape: bool,
    overflow: bool,
}
impl Decoder {
    pub fn feed(&mut self, bytes: &[u8], mut emit: impl FnMut(Event)) {
        for &b in bytes {
            if self.osc {
                if b == 7 || (self.escape && b == b'\\') {
                    let overhead = if b == 7 { 3 } else { 4 };
                    if !self.overflow && self.buffer.len() + overhead <= MAX_SEQUENCE {
                        if let Some(event) = report(&self.buffer) {
                            emit(event);
                        }
                    }
                    self.osc = false;
                    self.escape = false;
                    self.buffer.clear();
                    self.overflow = false;
                } else if self.escape {
                    // ESC 引入新控制序列时，未闭合 OSC 作废。
                    self.osc = b == b']';
                    self.escape = b == 27;
                    self.buffer.clear();
                    self.overflow = false;
                    if b == b'c' {
                        emit(Event::Reset);
                    }
                } else if b == 27 {
                    self.escape = true;
                } else if self.buffer.len() < MAX_SEQUENCE {
                    self.buffer.push(b);
                } else {
                    self.overflow = true;
                }
            } else if self.escape {
                self.osc = b == b']';
                self.escape = b == 27;
                if b == b'c' {
                    emit(Event::Reset);
                }
            } else {
                self.escape = b == 27;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn decode(s: &[u8]) -> Vec<Event> {
        let mut d = Decoder::default();
        let mut v = vec![];
        d.feed(s, |e| v.push(e));
        v
    }
    #[test]
    fn program_status_all_chunk_boundaries() {
        let s = b"noise\x1b]7501;state=blocked:kind=question:app=claude-code\x1b\\";
        let expected = decode(s);
        assert_eq!(expected.len(), 1);
        for split in 0..s.len() {
            let mut d = Decoder::default();
            let mut v = vec![];
            d.feed(&s[..split], |e| v.push(e));
            d.feed(&s[split..], |e| v.push(e));
            assert_eq!(v, expected);
        }
    }
    #[test]
    fn program_status_rejects_invalid_and_resynchronizes() {
        for body in [
            "state=future",
            "kind=question",
            "state=done:msg=@@",
            "state=done:msg=YQ==:title=Cg==",
            "state=done:id=a//b",
            "state=done:msg=4oA=",
        ] {
            // malformed pair characters are skipped by the protocol; valid but bad base64 is rejected.
            if body.ends_with("@@") {
                continue;
            }
            assert!(
                decode(format!("\x1b]7501;{body}\x07").as_bytes()).is_empty(),
                "{body}"
            );
        }
        let input = format!(
            "\x1b]7501;state=done:msg={}\x07\x1b]7501;?\x07",
            "A".repeat(10000)
        );
        assert_eq!(decode(input.as_bytes()), vec![Event::Query]);
    }
    #[test]
    fn program_status_replacement_and_reset() {
        assert_eq!(
            decode(b"\x1b]7501;state=working:state=idle:kind=auth\x07"),
            vec![Event::Report(Report {
                state: State::Idle,
                id: None,
                app: None,
                kind: None
            })]
        );
        assert_eq!(
            decode(b"\x1bc\x1b]133;A\x07"),
            vec![Event::Reset, Event::ShellPrompt]
        );
        assert!(decode(b"\x1b]7501;state=idle:state=unknown\x07").is_empty());
        assert_eq!(
            decode(b"\x1b]7501;state=working\x1b]7501;?\x07"),
            vec![Event::Query]
        );
    }
    #[test]
    fn program_status_text_limits_and_children() {
        assert_eq!(
            decode(b"\x1b]7501;state=blocked:id=worker/one:msg=5L2g5aW9:future=1\x07").len(),
            1
        );
        assert!(decode(
            format!(
                "\x1b]7501;state=idle:title={}\x07",
                STANDARD.encode(vec![b'a'; 193])
            )
            .as_bytes()
        )
        .is_empty());
        for text in ["a\n", "\u{0085}", "\0"] {
            assert!(decode(
                format!("\x1b]7501;state=done:msg={}\x07", STANDARD.encode(text)).as_bytes()
            )
            .is_empty());
        }
    }
}
