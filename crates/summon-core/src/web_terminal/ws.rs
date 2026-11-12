//! WebSocket 帧(RFC 6455),只实现终端服务端用得到的部分:解浏览器发来的掩码帧,
//! 发不带掩码的服务端帧。握手的 SHA-1 在 summon 侧算(summon-core 不依赖 ring)。
//!
//! 浏览器发来的按键与控制消息按**帧类型**分开,从不看内容:二进制帧是可操作页的按键字节,文本帧
//! 只可能是控制消息(JSON),认不出就整条丢掉。只读页能发的只有控制消息,按内容去猜的话,一段
//! 长得像控制消息的按键、或一条写坏了的控制消息,就有机会被当成另一种东西。

/// RFC 6455 固定的握手 GUID。
pub const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
/// 单条消息上限。终端输入是按键与一行文字,64KB 远超所需,超了就是不该接的东西。
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientMessage {
    /// 二进制帧:可操作页的按键字节。
    Data(Vec<u8>),
    /// 文本帧里认得出的控制消息。
    Control(Control),
    Ping(Vec<u8>),
    Close,
}

/// 页面发来的控制消息。字节由服务端生成,页面只给参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// `{"scroll": n}`:翻 n 格,负数往上(更早的内容)、正数往下;n 不为 0。
    Scroll(i64),
}

/// 控制消息的线上格式。只认这一种形状:多一个字段、类型不对、格数为 0 都不认。
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct WireControl {
    scroll: i64,
}

/// 解一条文本帧。认不出为 None,调用方把整条丢掉。
fn parse_control(payload: &[u8]) -> Option<Control> {
    // 只认 JSON 对象:serde 派生的结构体也收 `[1]` 这种按位置写的数组。
    let first = payload.iter().find(|b| !b" \t\r\n".contains(b));
    if first != Some(&b'{') {
        return None;
    }
    let wire: WireControl = serde_json::from_slice(payload).ok()?;
    (wire.scroll != 0).then_some(Control::Scroll(wire.scroll))
}

/// 一条消息攒齐之后按首帧的类型产出:二进制是按键,文本是控制消息(认不出就没有)。
fn complete(opcode: u8, payload: Vec<u8>) -> Option<ClientMessage> {
    if opcode == 0x2 {
        Some(ClientMessage::Data(payload))
    } else {
        parse_control(&payload).map(ClientMessage::Control)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WsError {
    #[error("{}", summon_i18n::text!("r322e9db267"))]
    Unmasked,
    #[error("{}", summon_i18n::format!("rd89e19c13b", _0 = .0))]
    Protocol(u8),
    #[error("{}", summon_i18n::text!("r008d1eb2ce"))]
    TooLarge,
}

/// `Sec-WebSocket-Accept` 的 SHA-1 输入。
pub fn accept_key_source(key: &str) -> String {
    format!("{}{WS_GUID}", key.trim())
}

#[derive(Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
    /// 分片中的消息:首帧的类型与攒到的载荷。
    fragments: Option<(u8, Vec<u8>)>,
}

struct Frame {
    fin: bool,
    opcode: u8,
    payload: Vec<u8>,
}

impl FrameDecoder {
    /// 喂进一段字节,吐出其中已经完整的消息;不完整的尾巴留到下次。出错后连接应当关闭。
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<ClientMessage>, WsError> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some((frame, used)) = parse_frame(&self.buf)? {
            self.buf.drain(..used);
            match frame.opcode {
                0x0 => {
                    let (_, acc) = self.fragments.as_mut().ok_or(WsError::Protocol(0x0))?;
                    acc.extend(frame.payload);
                    if acc.len() > MAX_MESSAGE_BYTES {
                        return Err(WsError::TooLarge);
                    }
                    if frame.fin {
                        if let Some((opcode, payload)) = self.fragments.take() {
                            out.extend(complete(opcode, payload));
                        }
                    }
                }
                0x1 | 0x2 => {
                    if self.fragments.is_some() {
                        return Err(WsError::Protocol(frame.opcode));
                    }
                    if frame.fin {
                        out.extend(complete(frame.opcode, frame.payload));
                    } else {
                        self.fragments = Some((frame.opcode, frame.payload));
                    }
                }
                0x8 => out.push(ClientMessage::Close),
                0x9 => out.push(ClientMessage::Ping(frame.payload)),
                0xA => {}
                other => return Err(WsError::Protocol(other)),
            }
        }
        Ok(out)
    }
}

fn parse_frame(buf: &[u8]) -> Result<Option<(Frame, usize)>, WsError> {
    if buf.len() < 2 {
        return Ok(None);
    }
    let (b0, b1) = (buf[0], buf[1]);
    let opcode = b0 & 0x0f;
    if b0 & 0x70 != 0 {
        return Err(WsError::Protocol(opcode));
    }
    let fin = b0 & 0x80 != 0;
    if b1 & 0x80 == 0 {
        return Err(WsError::Unmasked);
    }
    let (len, mut pos) = match b1 & 0x7f {
        126 => {
            if buf.len() < 4 {
                return Ok(None);
            }
            (u16::from_be_bytes([buf[2], buf[3]]) as usize, 4)
        }
        127 => {
            if buf.len() < 10 {
                return Ok(None);
            }
            let mut n = [0u8; 8];
            n.copy_from_slice(&buf[2..10]);
            let n = u64::from_be_bytes(n);
            if n > MAX_MESSAGE_BYTES as u64 {
                return Err(WsError::TooLarge);
            }
            (n as usize, 10)
        }
        n => (n as usize, 2),
    };
    // 控制帧不许分片、载荷不超过 125(RFC 6455 §5.5)。
    if opcode >= 0x8 && (!fin || len > 125) {
        return Err(WsError::Protocol(opcode));
    }
    if len > MAX_MESSAGE_BYTES {
        return Err(WsError::TooLarge);
    }
    if buf.len() < pos + 4 + len {
        return Ok(None);
    }
    let mask = [buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]];
    pos += 4;
    let payload = buf[pos..pos + len]
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ mask[i % 4])
        .collect();
    Ok(Some((
        Frame {
            fin,
            opcode,
            payload,
        },
        pos + len,
    )))
}

pub fn encode_binary(payload: &[u8]) -> Vec<u8> {
    encode(0x2, payload)
}

pub fn encode_text(text: &str) -> Vec<u8> {
    encode(0x1, text.as_bytes())
}

pub fn encode_pong(payload: &[u8]) -> Vec<u8> {
    encode(0xA, payload)
}

/// 服务端探活用的 ping。浏览器收到会自动回 pong,不需要页面脚本参与。
pub fn encode_ping(payload: &[u8]) -> Vec<u8> {
    encode(0x9, payload)
}

pub fn encode_close() -> Vec<u8> {
    encode(0x8, &[])
}

fn encode(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 10);
    out.push(0x80 | opcode);
    match payload.len() {
        n if n < 126 => out.push(n as u8),
        n if n <= u16::MAX as usize => {
            out.push(126);
            out.extend((n as u16).to_be_bytes());
        }
        n => {
            out.push(127);
            out.extend((n as u64).to_be_bytes());
        }
    }
    out.extend_from_slice(payload);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 按浏览器的样子造一帧:客户端帧必须带掩码。
    fn client_frame(fin: bool, opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mask = [0x11, 0x22, 0x33, 0x44];
        let mut out = vec![if fin { 0x80 } else { 0 } | opcode];
        match payload.len() {
            n if n < 126 => out.push(0x80 | n as u8),
            n if n <= u16::MAX as usize => {
                out.push(0x80 | 126);
                out.extend((n as u16).to_be_bytes());
            }
            n => {
                out.push(0x80 | 127);
                out.extend((n as u64).to_be_bytes());
            }
        }
        out.extend(mask);
        out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        out
    }

    #[test]
    fn a_binary_frame_split_across_reads_decodes_once_complete() {
        let frame = client_frame(true, 0x2, b"ls\r");
        let mut d = FrameDecoder::default();
        assert!(d.push(&frame[..3]).unwrap().is_empty());
        assert_eq!(
            d.push(&frame[3..]).unwrap(),
            vec![ClientMessage::Data(b"ls\r".to_vec())]
        );
    }

    #[test]
    fn fragments_are_joined_and_control_frames_pass_through_between_them() {
        let mut bytes = client_frame(false, 0x2, b"he");
        bytes.extend(client_frame(true, 0x9, b"p"));
        bytes.extend(client_frame(true, 0x0, b"llo"));
        bytes.extend(client_frame(false, 0x1, br#"{"scr"#));
        bytes.extend(client_frame(true, 0x0, br#"oll":-2}"#));
        bytes.extend(client_frame(true, 0x8, b""));
        let mut d = FrameDecoder::default();
        assert_eq!(
            d.push(&bytes).unwrap(),
            vec![
                ClientMessage::Ping(b"p".to_vec()),
                ClientMessage::Data(b"hello".to_vec()),
                ClientMessage::Control(Control::Scroll(-2)),
                ClientMessage::Close,
            ]
        );
    }

    /// 按键与控制按帧类型分开,从不看内容:二进制帧是按键,文本帧只可能是控制消息。
    #[test]
    fn keystrokes_come_only_from_binary_frames_and_control_only_from_text_frames() {
        let mut d = FrameDecoder::default();
        let mut bytes = client_frame(true, 0x1, br#"{"scroll":-2}"#);
        bytes.extend(client_frame(true, 0x1, br#"{"scroll": 3}"#));
        bytes.extend(client_frame(true, 0x2, br#"{"scroll":-2}"#));
        assert_eq!(
            d.push(&bytes).unwrap(),
            vec![
                ClientMessage::Control(Control::Scroll(-2)),
                ClientMessage::Control(Control::Scroll(3)),
                ClientMessage::Data(br#"{"scroll":-2}"#.to_vec()),
            ],
            "二进制帧里长得像控制消息的也是按键"
        );
        assert_eq!(
            d.push(&client_frame(true, 0x1, b"echo hi\r")).unwrap(),
            vec![],
            "文本帧里的按键丢掉,不当按键"
        );
    }

    /// 认不出的控制消息整条丢掉,既不当按键也不断开连接:后面的帧照常解。
    #[test]
    fn malformed_control_messages_are_dropped_and_never_become_input() {
        let mut d = FrameDecoder::default();
        for text in [
            &br#"{"scroll":0}"#[..],
            br#"{"scroll":-0}"#,
            br#"{"scroll":1.5}"#,
            br#"{"scroll":1.0}"#,
            br#"{"scroll":1e2}"#,
            br#"{"scroll":"1"}"#,
            br#"{"scroll":null}"#,
            br#"{"scroll":true}"#,
            br#"{"scroll":99999999999999999999}"#,
            br#"{}"#,
            br#"{"scrol":1}"#,
            br#"{"scroll":1,"x":1,"y":1}"#,
            br#"{"scroll":1,"scroll":2}"#,
            br#"[1]"#,
            b"null",
            b"1",
            b"",
            b"\x1b[A",
            b"\xff\xfe",
        ] {
            assert_eq!(
                d.push(&client_frame(true, 0x1, text)).unwrap(),
                vec![],
                "{:?}",
                String::from_utf8_lossy(text)
            );
        }
        assert_eq!(
            d.push(&client_frame(true, 0x2, b"k")).unwrap(),
            vec![ClientMessage::Data(b"k".to_vec())]
        );
    }

    /// 上限本身是 spec 冻结的数值:上面那些用例的边界都跟着它算,改了取值它们照样绿。
    #[test]
    fn the_message_cap_keeps_the_value_the_spec_froze() {
        assert_eq!(MAX_MESSAGE_BYTES, 64 * 1024);
    }

    /// 分片消息也受上限管:不带 FIN 的续帧一路攒下去,攒过上限就断开这条连接。单帧那条路由
    /// `protocol_violations_are_errors` 守着——攒出来的这条另有一道检查,只有这里守得住。
    #[test]
    fn fragments_that_add_up_past_the_cap_are_rejected() {
        let half = vec![b'a'; MAX_MESSAGE_BYTES / 2 + 1];
        let mut over = client_frame(false, 0x1, &half);
        over.extend(client_frame(false, 0x0, &half));
        assert_eq!(FrameDecoder::default().push(&over), Err(WsError::TooLarge));

        // 正好到上限的分片照常拼成一条消息。
        let part = vec![b'b'; MAX_MESSAGE_BYTES / 2];
        let mut exact = client_frame(false, 0x2, &part);
        exact.extend(client_frame(true, 0x0, &part));
        assert_eq!(
            FrameDecoder::default().push(&exact).unwrap(),
            vec![ClientMessage::Data(vec![b'b'; MAX_MESSAGE_BYTES])]
        );
    }

    #[test]
    fn protocol_violations_are_errors() {
        let mut unmasked = client_frame(true, 0x1, b"x");
        unmasked[1] &= 0x7f;
        assert_eq!(
            FrameDecoder::default().push(&unmasked),
            Err(WsError::Unmasked)
        );

        let big = vec![b'a'; MAX_MESSAGE_BYTES + 1];
        assert_eq!(
            FrameDecoder::default().push(&client_frame(true, 0x2, &big)),
            Err(WsError::TooLarge)
        );

        let fragmented_ping = client_frame(false, 0x9, b"");
        assert!(matches!(
            FrameDecoder::default().push(&fragmented_ping),
            Err(WsError::Protocol(_))
        ));

        let stray_continuation = client_frame(true, 0x0, b"x");
        assert!(matches!(
            FrameDecoder::default().push(&stray_continuation),
            Err(WsError::Protocol(_))
        ));
    }

    #[test]
    fn server_frames_use_the_shortest_length_form_and_no_mask() {
        assert_eq!(encode_binary(&[7; 125])[..2], [0x82, 125]);
        assert_eq!(encode_binary(&[7; 126])[..4], [0x82, 126, 0, 126]);
        let long = encode_binary(&vec![7; 70_000]);
        assert_eq!(long[..2], [0x82, 127]);
        assert_eq!(u64::from_be_bytes(long[2..10].try_into().unwrap()), 70_000);
        assert_eq!(encode_text("ok"), vec![0x81, 2, b'o', b'k']);
        assert_eq!(encode_close(), vec![0x88, 0]);
        assert_eq!(encode_pong(b"p"), vec![0x8a, 1, b'p']);
        assert_eq!(encode_ping(b""), vec![0x89, 0]);
    }

    #[test]
    fn accept_key_source_appends_the_rfc_guid() {
        assert_eq!(
            accept_key_source("dGhlIHNhbXBsZSBub25jZQ=="),
            "dGhlIHNhbXBsZSBub25jZQ==258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
        );
    }
}
