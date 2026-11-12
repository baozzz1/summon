package main

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	larkdrive "github.com/larksuite/oapi-sdk-go/v3/service/drive/v1"
)

type recordingConn struct {
	mu      sync.Mutex
	writes  [][]byte
	readBuf []byte
}

func (c *recordingConn) Read(p []byte) (int, error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if len(c.readBuf) == 0 {
		return 0, io.EOF
	}
	n := copy(p, c.readBuf)
	c.readBuf = c.readBuf[n:]
	return n, nil
}
func (c *recordingConn) Close() error                       { return nil }
func (c *recordingConn) LocalAddr() net.Addr                { return nil }
func (c *recordingConn) RemoteAddr() net.Addr               { return nil }
func (c *recordingConn) SetDeadline(_ time.Time) error      { return nil }
func (c *recordingConn) SetReadDeadline(_ time.Time) error  { return nil }
func (c *recordingConn) SetWriteDeadline(_ time.Time) error { return nil }

func (c *recordingConn) Write(p []byte) (int, error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.writes = append(c.writes, append([]byte(nil), p...))
	var event outEvent
	if json.Unmarshal(p, &event) == nil {
		var ack []byte
		switch event.Type {
		case "hello":
			ack, _ = json.Marshal(durableAck{
				Type: "hello_ack", OK: true, Status: "ready", Protocol: sidecarProtocolVersion,
			})
		case "document_comment":
			ack, _ = json.Marshal(durableAck{Type: "document_comment_ack", EventID: event.EventID, OK: true, Status: "durable"})
		case "card_action":
			if durableCardAction(event.Value) {
				ack, _ = json.Marshal(durableAck{
					Type: "card_action_ack", EventID: event.EventID, OK: true, Status: "durable",
				})
			}
		}
		if len(ack) > 0 {
			c.readBuf = append(ack, '\n')
		}
	}
	return len(p), nil
}

func (c *recordingConn) writeCount() int {
	c.mu.Lock()
	defer c.mu.Unlock()
	return len(c.writes)
}

func (c *recordingConn) lastWrite() []byte {
	c.mu.Lock()
	defer c.mu.Unlock()
	if len(c.writes) == 0 {
		return nil
	}
	return append([]byte(nil), c.writes[len(c.writes)-1]...)
}

type blockingMergeFetcher struct {
	started chan struct{}
	release chan struct{}
}

func (f *blockingMergeFetcher) Fetch(
	ctx context.Context,
	_ appCfg,
	_ string,
) (string, []resourceOut) {
	close(f.started)
	select {
	case <-f.release:
		return "异步转发正文", nil
	case <-ctx.Done():
		return "📎 [转发消息:拉取超时]", nil
	}
}

func mergeForwardEventPayload() []byte {
	return []byte(`{
		"schema":"2.0",
		"header":{"event_id":"e_merge","event_type":"im.message.receive_v1","app_id":"cli_test","tenant_key":"t1","create_time":"1710000000000"},
		"event":{
			"sender":{"sender_id":{"open_id":"ou_user"},"sender_type":"user"},
			"message":{"message_id":"om_merge","chat_id":"oc_g","chat_type":"group","message_type":"merge_forward","content":"Merged and Forwarded Message","create_time":"1710000000000"}
		}
	}`)
}

func textEventPayload() []byte {
	return []byte(`{
		"schema":"2.0",
		"header":{"event_id":"e_text","event_type":"im.message.receive_v1","app_id":"cli_test","tenant_key":"t1","create_time":"1710000000000"},
		"event":{
			"sender":{"sender_id":{"open_id":"ou_user"},"sender_type":"user"},
			"message":{"message_id":"om_text","chat_id":"oc_g","chat_type":"group","message_type":"text","content":"{\"text\":\"hello\"}","create_time":"1710000000000"}
		}
	}`)
}

func TestMessageReactionCreatedIsIgnored(t *testing.T) {
	conn := &recordingConn{}
	handler := buildEventDispatcher(appCfg{AppID: "cli_test"}, &sink{conn: conn})

	payload := []byte(`{
		"schema":"2.0",
		"header":{
			"event_id":"event_1",
			"event_type":"im.message.reaction.created_v1",
			"app_id":"cli_test",
			"tenant_key":"tenant_1",
			"create_time":"1710000000000"
		},
		"event":{
			"message_id":"om_1",
			"reaction_type":{"emoji_type":"OK"},
			"operator_type":"user",
			"action_time":"1710000000000"
		}
	}`)

	if _, err := handler.Do(context.Background(), payload); err != nil {
		t.Fatalf("reaction created event should be handled silently: %v", err)
	}
	if got := conn.writeCount(); got != 0 {
		t.Fatalf("reaction created event should not be forwarded to daemon, got %d writes", got)
	}
}

func TestCaptureDispatcherEmitsOpenID(t *testing.T) {
	var buf bytes.Buffer
	captured := false
	handler := captureDispatcher(&buf, func() { captured = true })

	payload := []byte(`{
		"schema":"2.0",
		"header":{"event_id":"e1","event_type":"im.message.receive_v1","app_id":"cli_test","tenant_key":"t1","create_time":"1710000000000"},
		"event":{
			"sender":{"sender_id":{"open_id":"ou_capture_me"},"sender_type":"user"},
			"message":{"message_id":"om_1","chat_id":"oc_1","chat_type":"p2p","message_type":"text","content":"{\"text\":\"hi\"}"}
		}
	}`)

	if _, err := handler.Do(context.Background(), payload); err != nil {
		t.Fatalf("receive event should be handled: %v", err)
	}
	if !captured {
		t.Fatal("onCaptured should fire on first message")
	}
	var got captured_
	if err := json.Unmarshal(buf.Bytes(), &got); err != nil {
		t.Fatalf("stdout line should be JSON: %v (raw=%q)", err, buf.String())
	}
	if got.OpenID != "ou_capture_me" {
		t.Fatalf("open_id mismatch: got %q", got.OpenID)
	}
	if got.ChatID != "oc_1" {
		t.Fatalf("chat_id mismatch: got %q", got.ChatID)
	}
}

func TestCaptureDispatcherIgnoresReaction(t *testing.T) {
	var buf bytes.Buffer
	captured := false
	handler := captureDispatcher(&buf, func() { captured = true })

	payload := []byte(`{
		"schema":"2.0",
		"header":{"event_id":"e2","event_type":"im.message.reaction.created_v1","app_id":"cli_test","tenant_key":"t1","create_time":"1710000000000"},
		"event":{"message_id":"om_1","reaction_type":{"emoji_type":"OK"},"operator_type":"user","action_time":"1710000000000"}
	}`)

	if _, err := handler.Do(context.Background(), payload); err != nil {
		t.Fatalf("reaction event should be handled silently: %v", err)
	}
	if captured {
		t.Fatal("reaction must not trigger capture")
	}
	if buf.Len() != 0 {
		t.Fatalf("reaction must not write to stdout, got %q", buf.String())
	}
}

func TestOutEventSerializesChatTypeAndMentions(t *testing.T) {
	ev := outEvent{Type: "message", AppID: "a", ChatID: "c", ChatType: "group",
		Sender: "ou_1", Text: "hi", MessageID: "m",
		Mentions: []mentionOut{{OpenID: "ou_bot", Key: "@_user_1", Name: "助理"}}}
	b, _ := json.Marshal(ev)
	s := string(b)
	if !strings.Contains(s, `"chat_type":"group"`) || !strings.Contains(s, `"open_id":"ou_bot"`) {
		t.Fatalf("missing fields: %s", s)
	}
}

// 集成:把群消息(带 @bot 占位)喂给 dispatcher,校验**原样**转发正文 + mentions
// (不再在 sidecar 剥占位;由 daemon 渲染成 @姓名(open_id))。
func TestGroupMessageForwardsChatTypeAndMentions(t *testing.T) {
	conn := &recordingConn{}
	handler := buildEventDispatcher(appCfg{AppID: "cli_test"}, &sink{conn: conn})
	payload := []byte(`{
		"schema":"2.0",
		"header":{"event_id":"e3","event_type":"im.message.receive_v1","app_id":"cli_test","tenant_key":"t1","create_time":"1710000000000"},
		"event":{
			"sender":{"sender_id":{"open_id":"ou_user"},"sender_type":"user"},
			"message":{"message_id":"om_g","chat_id":"oc_g","chat_type":"group","message_type":"text","content":"{\"text\":\"@_user_1 帮我看下\"}","mentions":[{"key":"@_user_1","id":{"open_id":"ou_bot"},"name":"助理","mentioned_type":"bot"}]}
		}
	}`)
	if _, err := handler.Do(context.Background(), payload); err != nil {
		t.Fatalf("group receive should be handled: %v", err)
	}
	if conn.writeCount() != 1 {
		t.Fatalf("want 1 forwarded event, got %d", conn.writeCount())
	}
	var ev outEvent
	if err := json.Unmarshal(conn.writes[0], &ev); err != nil {
		t.Fatalf("bad json: %v (raw=%q)", err, string(conn.writes[0]))
	}
	if ev.ChatType != "group" {
		t.Fatalf("chat_type: %q", ev.ChatType)
	}
	if ev.Text != "@_user_1 帮我看下" {
		t.Fatalf("text should be forwarded raw, got: %q", ev.Text)
	}
	if len(ev.Mentions) != 1 || ev.Mentions[0].OpenID != "ou_bot" || ev.Mentions[0].Key != "@_user_1" {
		t.Fatalf("mentions: %+v", ev.Mentions)
	}
	if ev.Mentions[0].Kind != "bot" {
		t.Fatalf("mentioned_type 应随 @ 列表转发: %+v", ev.Mentions)
	}
}

func TestReplyMessageForwardsParentID(t *testing.T) {
	conn := &recordingConn{}
	handler := buildEventDispatcher(appCfg{AppID: "cli_test"}, &sink{conn: conn})
	// 引用回复:飞书事件 message 带 parent_id 指向被引用消息。
	payload := []byte(`{
		"schema":"2.0",
		"header":{"event_id":"e4","event_type":"im.message.receive_v1","app_id":"cli_test","tenant_key":"t1","create_time":"1710000000000"},
		"event":{
			"sender":{"sender_id":{"open_id":"ou_user"},"sender_type":"user"},
			"message":{"message_id":"om_reply","parent_id":"om_quoted","root_id":"om_quoted","thread_id":"omt_topic","chat_id":"oc_g","chat_type":"group","message_type":"text","content":"{\"text\":\"就这条\"}"}
		}
	}`)
	if _, err := handler.Do(context.Background(), payload); err != nil {
		t.Fatalf("reply receive should be handled: %v", err)
	}
	var ev outEvent
	if err := json.Unmarshal(conn.writes[0], &ev); err != nil {
		t.Fatalf("bad json: %v (raw=%q)", err, string(conn.writes[0]))
	}
	if ev.ParentID != "om_quoted" {
		t.Fatalf("parent_id should be forwarded, got: %q", ev.ParentID)
	}
	if ev.RootID != "om_quoted" {
		t.Fatalf("root_id should be forwarded, got: %q", ev.RootID)
	}
	if ev.ThreadID != "omt_topic" {
		t.Fatalf("thread_id should be forwarded, got: %q", ev.ThreadID)
	}
}

// create_time 透传此前零断言覆盖;2026-08-05 排障时 WAL 里时间为 0 一度被误判为
// 「sidecar 没解析」。钉住:message.create_time(毫秒字符串)必须原值透传成 create_time_ms。
func TestMessageForwardsCreateTimeMs(t *testing.T) {
	conn := &recordingConn{}
	handler := buildEventDispatcher(appCfg{AppID: "cli_test"}, &sink{conn: conn})
	payload := []byte(`{
		"schema":"2.0",
		"header":{"event_id":"e_ct","event_type":"im.message.receive_v1","app_id":"cli_test","tenant_key":"t1","create_time":"1755000000999"},
		"event":{
			"sender":{"sender_id":{"open_id":"ou_user"},"sender_type":"user"},
			"message":{"message_id":"om_ct","chat_id":"oc_g","chat_type":"group","message_type":"text","content":"{\"text\":\"hi\"}","create_time":"1755000000123"}
		}
	}`)
	if _, err := handler.Do(context.Background(), payload); err != nil {
		t.Fatalf("receive should be handled: %v", err)
	}
	var ev outEvent
	if err := json.Unmarshal(conn.writes[0], &ev); err != nil {
		t.Fatalf("bad json: %v (raw=%q)", err, string(conn.writes[0]))
	}
	if ev.CreateTimeMs != 1755000000123 {
		t.Fatalf("create_time_ms should carry message.create_time verbatim, got: %d", ev.CreateTimeMs)
	}
}

// 缺失/非法 create_time 按 0(未知)兜底,不能让整条消息解析失败。
func TestMessageMissingCreateTimeDefaultsToZero(t *testing.T) {
	conn := &recordingConn{}
	handler := buildEventDispatcher(appCfg{AppID: "cli_test"}, &sink{conn: conn})
	payload := []byte(`{
		"schema":"2.0",
		"header":{"event_id":"e_noct","event_type":"im.message.receive_v1","app_id":"cli_test","tenant_key":"t1","create_time":"1755000000999"},
		"event":{
			"sender":{"sender_id":{"open_id":"ou_user"},"sender_type":"user"},
			"message":{"message_id":"om_noct","chat_id":"oc_g","chat_type":"group","message_type":"text","content":"{\"text\":\"hi\"}"}
		}
	}`)
	if _, err := handler.Do(context.Background(), payload); err != nil {
		t.Fatalf("receive should be handled: %v", err)
	}
	var ev outEvent
	if err := json.Unmarshal(conn.writes[0], &ev); err != nil {
		t.Fatalf("bad json: %v (raw=%q)", err, string(conn.writes[0]))
	}
	if ev.CreateTimeMs != 0 {
		t.Fatalf("missing create_time should default to 0 (unknown), got: %d", ev.CreateTimeMs)
	}
}

func TestParseMessageContentImage(t *testing.T) {
	text, res := parseMessageContent("image", `{"image_key":"img_v3_abc"}`, nil)
	if text != "" {
		t.Fatalf("image 不应有正文, got %q", text)
	}
	if len(res) != 1 || res[0].FileKey != "img_v3_abc" || res[0].Kind != "image" {
		t.Fatalf("image 附件解析错误: %+v", res)
	}
}

func TestParseMessageContentFile(t *testing.T) {
	_, res := parseMessageContent("file", `{"file_key":"file_v3_x","file_name":"报告.pdf"}`, nil)
	if len(res) != 1 || res[0].Kind != "file" || res[0].Name != "报告.pdf" || res[0].FileKey != "file_v3_x" {
		t.Fatalf("file 附件解析错误: %+v", res)
	}
}

func TestParseMessageContentPostInterleavesTextAndImages(t *testing.T) {
	content := `{"title":"周报","content":[[{"tag":"text","text":"看这张图"},{"tag":"img","image_key":"img_1"}],[{"tag":"text","text":"还有这张"},{"tag":"img","image_key":"img_2"}]]}`
	text, res := parseMessageContent("post", content, nil)
	if !strings.Contains(text, "周报") || !strings.Contains(text, "看这张图") || !strings.Contains(text, "还有这张") {
		t.Fatalf("post 正文拼接错误: %q", text)
	}
	if len(res) != 2 || res[0].FileKey != "img_1" || res[1].FileKey != "img_2" {
		t.Fatalf("post 内嵌图片解析错误: %+v", res)
	}
}

// 富文本里承载正文的元素不止 text/a:代码块、markdown 段落、@ 提及同样是用户写下的内容。
// 漏掉它们不是"少一点格式",而是整段消失且不留痕迹 —— 用户贴了代码,agent 看到的是几个空行。
func TestParseMessageContentPostKeepsCodeBlockMarkdownAndMention(t *testing.T) {
	content := `{"content":[[{"tag":"text","text":"看这段:"}],[{"tag":"code_block","language":"RUST","text":"fn main() {}"}],[{"tag":"md","text":"**重点**"}],[{"tag":"at","user_id":"ou_x","user_name":"某人"}]]}`
	text, _ := parseMessageContent("post", content, nil)
	if !strings.Contains(text, "fn main() {}") {
		t.Fatalf("代码块正文丢失: %q", text)
	}
	if !strings.Contains(text, "```rust") {
		t.Fatalf("代码块要还原成围栏并带上语言: %q", text)
	}
	if !strings.Contains(text, "**重点**") {
		t.Fatalf("markdown 段落丢失: %q", text)
	}
	if !strings.Contains(text, "@某人") {
		t.Fatalf("@ 提及丢失: %q", text)
	}
}

// 飞书往 post 里加新 tag 时不能再静默吞掉:只要元素带 text,就该进正文。
func TestParseMessageContentPostKeepsUnknownTagText(t *testing.T) {
	text, _ := parseMessageContent("post", `{"content":[[{"tag":"未来某种元素","text":"这句必须留下"}]]}`, nil)
	if !strings.Contains(text, "这句必须留下") {
		t.Fatalf("未知 tag 的正文被静默丢弃: %q", text)
	}
}

// 飞书 post 的九种 tag(create_json 文档)在一条消息里全出现时,每一种都要留下痕迹。
// 夹具与 Rust 侧 `post_plaintext` 共用,两边行为不一致就该有一边挂。
const postAllTagsFixture = `{"title":"标题","content":[[{"tag":"text","text":"文本"},` +
	`{"tag":"a","text":"链接文字","href":"https://example.com"},` +
	`{"tag":"at","user_id":"ou_known"},` +
	`{"tag":"at","user_id":"ou_unknown","user_name":"名字兜底"},` +
	`{"tag":"img","image_key":"img_1"},` +
	`{"tag":"media","file_key":"file_1","image_key":"cover_1"},` +
	`{"tag":"emotion","emoji_type":"SMILE"},` +
	`{"tag":"code_block","language":"PYTHON","text":"print(1)"},` +
	`{"tag":"hr"},` +
	`{"tag":"md","text":"**粗体**"}]]}`

func TestParsePostCoversEveryTag(t *testing.T) {
	mentions := []mentionOut{{OpenID: "ou_known", Key: "@_user_1", Name: "某人"}}
	text, res := parsePost(postAllTagsFixture, mentions)
	for _, want := range []string{
		"标题",
		"文本",
		"[链接文字](https://example.com)", // 地址本身是正文的一部分,只留锚文本等于删了它
		"@_user_1",                    // at 归一成与 text 消息同一形态,交给 daemon 渲染成人名
		"@名字兜底",                       // mentions 对不上时退到元素自带的名字
		":SMILE:",
		"```python\nprint(1)\n```",
		"---",
		"**粗体**",
	} {
		if !strings.Contains(text, want) {
			t.Fatalf("post 正文缺 %q:\n%s", want, text)
		}
	}
	var img, media bool
	for _, r := range res {
		if r.FileKey == "img_1" && r.Kind == "image" {
			img = true
		}
		if r.FileKey == "file_1" && r.Kind == "media" {
			media = true
		}
	}
	if !img || !media {
		t.Fatalf("post 内嵌图片/视频没收成附件: %+v", res)
	}
}

// 认不得的消息类型也得留痕:静默返回空,agent 那边就是「用户什么都没发」。
func TestParseMessageContentUnknownTypeLeavesTrace(t *testing.T) {
	text, res := parseMessageContent("sticker", `{"file_key":"x"}`, nil)
	if !strings.Contains(text, "sticker") {
		t.Fatalf("未知消息类型没留痕: %q", text)
	}
	if res != nil {
		t.Fatalf("未知消息类型不该产出附件: %+v", res)
	}
	if got, _ := parseMessageContent("", `{}`, nil); got != "" {
		t.Fatalf("空类型不该造占位: %q", got)
	}
}

func TestParseMessageContentTextUnchanged(t *testing.T) {
	text, res := parseMessageContent("text", `{"text":"你好"}`, nil)
	if text != "你好" || res != nil {
		t.Fatalf("text 解析回归: text=%q res=%+v", text, res)
	}
}

// 卡片 schema 1.0:顶层 header + elements,正文挂在 div/lark_md 的嵌套 text.content 上。
func TestParseMessageContentInteractiveSchema1(t *testing.T) {
	content := `{"config":{"wide_screen_mode":true},` +
		`"header":{"template":"blue","title":{"tag":"plain_text","content":"任务完成"}},` +
		`"elements":[` +
		`{"tag":"div","text":{"tag":"lark_md","content":"**结论**:已修复"}},` +
		`{"tag":"hr"},` +
		`{"tag":"note","elements":[{"tag":"plain_text","content":"来自 remote-cursor"}]}` +
		`]}`
	text, res := parseMessageContent("interactive", content, nil)
	if text != "任务完成\n**结论**:已修复\n来自 remote-cursor" {
		t.Fatalf("schema 1.0 卡片正文抽取错误: %q", text)
	}
	if res != nil {
		t.Fatalf("卡片不产附件, got %+v", res)
	}
}

// 卡片 schema 2.0:顶层 schema + body.elements,markdown 元素正文直接挂 content。
func TestParseMessageContentInteractiveSchema2(t *testing.T) {
	content := `{"schema":"2.0","config":{"update_multi":true},` +
		`"header":{"title":{"tag":"plain_text","content":"标题"}},` +
		`"body":{"elements":[` +
		`{"tag":"markdown","content":"第一段"},` +
		`{"tag":"markdown","content":"第二段"},` +
		`{"tag":"action","actions":[{"tag":"button","text":{"tag":"plain_text","content":"批准"},` +
		`"behaviors":[{"type":"callback","value":{"action":"gate_plan_approve","gate_id":"g1"}}]}]}` +
		`]}}`
	text, _ := parseMessageContent("interactive", content, nil)
	if text != "标题\n第一段\n第二段\n批准" {
		t.Fatalf("schema 2.0 卡片正文抽取错误: %q", text)
	}
}

// 事故现场形状:summon 自己发 agent 回复用的无 header 纯 markdown 卡片(card_markdown)。
func TestParseMessageContentInteractiveAgentReplyCard(t *testing.T) {
	content := `{"config":{"wide_screen_mode":true},"elements":[{"tag":"markdown","content":"@arcadia 帮我看下这个 MR"}]}`
	text, _ := parseMessageContent("interactive", content, nil)
	if text != "@arcadia 帮我看下这个 MR" {
		t.Fatalf("agent 回复卡片正文抽取错误: %q", text)
	}
}

// 只认 text/content 两个键:tag/template/url/callback value 这些非展示字段不许混进正文。
func TestParseInteractiveCardIgnoresNonTextFields(t *testing.T) {
	content := `{"schema":"2.0","body":{"elements":[` +
		`{"tag":"img","img_key":"img_v3_x","alt":{"tag":"plain_text","content":"图说"}},` +
		`{"tag":"markdown","content":"正文","href":{"url":"https://example.com"}}` +
		`]}}`
	text := parseInteractiveCard(content)
	if strings.Contains(text, "img_v3_x") || strings.Contains(text, "https://") ||
		strings.Contains(text, "markdown") || strings.Contains(text, "2.0") {
		t.Fatalf("非展示字段被当成正文: %q", text)
	}
	if text != "图说\n正文" {
		t.Fatalf("正文抽取错误: %q", text)
	}
}

// 异常输入:一律安全返回空串,不 panic。
func TestParseInteractiveCardMalformedReturnsEmpty(t *testing.T) {
	cases := map[string]string{
		"空串":      ``,
		"非 JSON":  `not json at all`,
		"纯标量":     `42`,
		"null":    `null`,
		"顶层字符串":   `"just a string"`,
		"空对象":     `{}`,
		"空数组":     `[]`,
		"无文本键的卡片": `{"schema":"2.0","body":{"elements":[{"tag":"hr"},{"tag":"img","img_key":"k"}]}}`,
		"文本键挂了数字": `{"content":123,"text":true}`,
	}
	for name, content := range cases {
		if got := parseInteractiveCard(content); got != "" {
			t.Fatalf("%s 应回空串, got %q", name, got)
		}
	}
}

// JSON 半路截断:已抽到的片段保留(残缺正文比整条空消息可用),且不 panic。
func TestParseInteractiveCardTruncatedKeepsPrefix(t *testing.T) {
	content := `{"header":{"title":{"tag":"plain_text","content":"标题"}},"elements":[{"tag":"markdown","content":"截断`
	if got := parseInteractiveCard(content); got != "标题" {
		t.Fatalf("截断 JSON 应保留已抽到的前缀, got %q", got)
	}
}

// 超深嵌套:靠非递归跳过兜底,不爆栈;超过深度上限的文本按丢弃处理。
func TestParseInteractiveCardDeepNestingIsSafe(t *testing.T) {
	const depth = 5000
	content := `{"content":"浅层"` +
		strings.Repeat(`,"body":{`, depth) + `"content":"深层"` +
		strings.Repeat(`}`, depth) + `}`
	got := parseInteractiveCard(content)
	if got != "浅层" {
		t.Fatalf("超深嵌套应只保留深度上限内的文本, got %q", got)
	}
}

// 数组直接装字符串时也按外层 key 抽,顺序即文档顺序。
func TestParseInteractiveCardArrayOfStrings(t *testing.T) {
	if got := parseInteractiveCard(`{"content":["甲","乙"],"tag":["丙"]}`); got != "甲\n乙" {
		t.Fatalf("数组元素应继承外层 key: %q", got)
	}
}

func TestBuildCaptureLineWrapsEventWithMeta(t *testing.T) {
	line, err := buildCaptureLine("cli_x", 1710000000000, map[string]interface{}{
		"message": map[string]interface{}{"message_type": "image", "content": `{"image_key":"k"}`},
	})
	if err != nil {
		t.Fatalf("build: %v", err)
	}
	if !bytes.HasSuffix(line, []byte("\n")) {
		t.Fatal("jsonl 行应以换行结尾")
	}
	var got struct {
		Ts    int64           `json:"ts"`
		AppID string          `json:"app_id"`
		Event json.RawMessage `json:"event"`
	}
	if err := json.Unmarshal(line, &got); err != nil {
		t.Fatalf("解析失败: %v (raw=%q)", err, string(line))
	}
	if got.Ts != 1710000000000 || got.AppID != "cli_x" {
		t.Fatalf("元信息错误: %+v", got)
	}
	if !strings.Contains(string(got.Event), "image_key") {
		t.Fatalf("原始事件未保留: %s", string(got.Event))
	}
}

func TestCardActionValuePicksAction(t *testing.T) {
	value := map[string]interface{}{"action": "clean", "chat": "pdead"}
	if got := cardActionString(value); got != "clean" {
		t.Fatalf("want clean, got %q", got)
	}
	if got := cardActionString(map[string]interface{}{}); got != "" {
		t.Fatalf("want empty for missing, got %q", got)
	}
}

// 表单容器(v2 form)提交:按钮 value 携带 action,form_value 携带输入框内容;
// 二者应被并进同一个 value map 转发给 daemon(daemon 从 value.form_value.workdir 取值)。
func TestCardActionTriggerMergesFormValue(t *testing.T) {
	conn := &recordingConn{}
	handler := buildEventDispatcher(appCfg{AppID: "cli_test"}, &sink{conn: conn})
	payload := []byte(`{
		"schema":"2.0",
		"header":{"event_id":"e_card","event_type":"card.action.trigger","app_id":"cli_test","tenant_key":"t1","create_time":"1710000000000","token":"tk"},
		"event":{
			"operator":{"open_id":"ou_clicker"},
			"token":"c-tok",
			"action":{
				"value":{"action":"set_workdir","chat":"pdead"},
				"tag":"button",
				"form_value":{"workdir":"~/projects"},
				"name":"wd_submit"
			},
			"context":{"open_message_id":"om_card","open_chat_id":"oc_card"}
		}
	}`)
	if _, err := handler.Do(context.Background(), payload); err != nil {
		t.Fatalf("card action should be handled: %v", err)
	}
	if conn.writeCount() != 1 {
		t.Fatalf("want 1 forwarded event, got %d", conn.writeCount())
	}
	var ev outEvent
	if err := json.Unmarshal(conn.writes[0], &ev); err != nil {
		t.Fatalf("bad json: %v (raw=%q)", err, string(conn.writes[0]))
	}
	if ev.Type != "card_action" {
		t.Fatalf("type: %q", ev.Type)
	}
	if cardActionString(ev.Value) != "set_workdir" {
		t.Fatalf("action: %+v", ev.Value)
	}
	fv, ok := ev.Value["form_value"].(map[string]interface{})
	if !ok {
		t.Fatalf("form_value missing or wrong type: %+v", ev.Value)
	}
	if fv["workdir"] != "~/projects" {
		t.Fatalf("workdir: %+v", fv)
	}
	if ev.OperatorOpenID != "ou_clicker" {
		t.Fatalf("operator: %q", ev.OperatorOpenID)
	}
	if ev.EventID != "e_card" || strings.Contains(string(conn.writes[0]), "c-tok") {
		t.Fatalf("应透传 header event_id 且不外泄卡片更新凭证: %s", string(conn.writes[0]))
	}
}

func TestModelDropdownForwardsSelectedOption(t *testing.T) {
	for _, action := range []string{"model", "model_default"} {
		for _, selected := range []string{"model-31", ""} {
			t.Run(action+"/"+selected, func(t *testing.T) {
				conn := &recordingConn{}
				handler := buildEventDispatcher(appCfg{AppID: "cli_test"}, &sink{conn: conn})
				payload := []byte(fmt.Sprintf(`{
                    "schema":"2.0",
                    "header":{"event_id":"e_select","event_type":"card.action.trigger","app_id":"cli_test","tenant_key":"t1","create_time":"1710000000000","token":"tk"},
                    "event":{
                        "operator":{"open_id":"ou_clicker"},
                        "action":{"value":{"action":%q,"chat":"g_test","model":"stale","inherit":true},"tag":"select_static","option":%q},
                        "context":{"open_message_id":"om_select","open_chat_id":"oc_select"}
                    }
                }`, action, selected))
				if _, err := handler.Do(context.Background(), payload); err != nil {
					t.Fatal(err)
				}
				if conn.writeCount() != 1 {
					t.Fatalf("writes: %d", conn.writeCount())
				}
				var got outEvent
				if err := json.Unmarshal(conn.writes[0], &got); err != nil {
					t.Fatal(err)
				}
				if got.Value["model"] != selected || cardActionString(got.Value) != action {
					t.Fatalf("selection lost: %+v", got.Value)
				}
				if _, inherited := got.Value["inherit"]; inherited {
					t.Fatal("selection must specify its model")
				}
				if got.ChatID != "oc_select" || got.OperatorOpenID != "ou_clicker" {
					t.Fatalf("routing: %+v", got)
				}
			})
		}
	}
}

func TestBridgeCardActionPreservesOpaqueValueAndPrivateCallbackChat(t *testing.T) {
	conn := &recordingConn{}
	handler := buildEventDispatcher(appCfg{AppID: "cli_test"}, &sink{conn: conn})
	payload := []byte(`{
		"schema":"2.0",
		"header":{"event_id":"e_bridge","event_type":"card.action.trigger","app_id":"cli_test","tenant_key":"t1","create_time":"1710000000000","token":"tk"},
		"event":{
			"operator":{"open_id":"ou_owner"},
			"token":"c-tok",
			"action":{"value":{"action":"bridge_mirror","ticket":"ticket-opaque","choice":"choice-opaque"},"tag":"button"},
			"context":{"open_message_id":"om_private_card","open_chat_id":"oc_private_not_target_group"}
		}
	}`)
	if _, err := handler.Do(context.Background(), payload); err != nil {
		t.Fatalf("bridge card action should be handled: %v", err)
	}
	var ev outEvent
	if err := json.Unmarshal(conn.writes[0], &ev); err != nil {
		t.Fatalf("bad json: %v", err)
	}
	if ev.ChatID != "oc_private_not_target_group" || ev.MessageID != "om_private_card" || ev.OperatorOpenID != "ou_owner" {
		t.Fatalf("private callback metadata changed: %+v", ev)
	}
	if len(ev.Value) != 3 || ev.Value["action"] != "bridge_mirror" || ev.Value["ticket"] != "ticket-opaque" || ev.Value["choice"] != "choice-opaque" {
		t.Fatalf("opaque bridge value changed: %+v", ev.Value)
	}
	if ev.EventID != "e_bridge" || strings.Contains(string(conn.writes[0]), "c-tok") {
		t.Fatalf("bridge callback 幂等键/凭证隔离错误: %s", string(conn.writes[0]))
	}
}

// captured_ mirrors the unexported captured struct for test decoding.
type captured_ struct {
	OpenID string `json:"open_id"`
	ChatID string `json:"chat_id"`
	Text   string `json:"text"`
}

// 治本回归:慢速累积(分钟级、远超任何爆发窗口的稀疏断开)下,退避**不**逐次升级。
// WHY 这是旧 isStorm 漏判的故障模式 —— 旧逻辑按"60s 窗口内 ≥5 次"判风暴,分钟级稀疏断开
// 永远凑不满窗口、storm 一次不发、client 一次不重建(线上 rebuild=0)。新策略改看"上条连接
// 活了多久":只要每条连接都先稳过 reconnectStableReset 再断,退避就一直回到下限,永远从最小
// 退避快速重连,不会退化成长时间不可用。这条测试在旧 isStorm 语义下根本无从表达(它没有"按
// 连接存活时长决定退避"的概念),故等价于"旧实现下失败、新实现下通过"。
func TestSlowAccumulationKeepsBackoffAtMin(t *testing.T) {
	backoff := reconnectBackoffMin
	// 模拟 10 次断开,每次断开前连接都稳挂了远超 reconnectStableReset 的时长(分钟级间隔)。
	for i := 0; i < 10; i++ {
		connectedFor := 5 * time.Minute // ≥ reconnectStableReset
		backoff = nextReconnectBackoff(backoff, connectedFor)
		if backoff != reconnectBackoffMin {
			t.Fatalf("第 %d 次慢速断开后退避应保持下限 %v,got %v",
				i+1, reconnectBackoffMin, backoff)
		}
	}
}

// 快速反复断开(连接没稳够就断):退避指数翻倍并封顶 max,避免高频重连给飞书添堵。
func TestRapidChurnEscalatesBackoffToCap(t *testing.T) {
	backoff := reconnectBackoffMin
	// 连接刚连上就断(connectedFor 远小于 reconnectStableReset),退避应逐次翻倍直到封顶。
	prev := time.Duration(0)
	for i := 0; i < 8; i++ {
		backoff = nextReconnectBackoff(backoff, 100*time.Millisecond)
		if backoff < prev {
			t.Fatalf("快速 churn 退避不应回退: prev=%v now=%v", prev, backoff)
		}
		if backoff > reconnectBackoffMax {
			t.Fatalf("退避不应超过封顶 %v,got %v", reconnectBackoffMax, backoff)
		}
		prev = backoff
	}
	if backoff != reconnectBackoffMax {
		t.Fatalf("持续 churn 应最终封顶 %v,got %v", reconnectBackoffMax, backoff)
	}
}

// 退避起步:cur 小于下限时回到下限(防止从 0 起步算出 0)。
func TestNextReconnectBackoffFloorsAtMin(t *testing.T) {
	if got := nextReconnectBackoff(0, 100*time.Millisecond); got != reconnectBackoffMin {
		t.Fatalf("起步应回到下限 %v,got %v", reconnectBackoffMin, got)
	}
}

// 康复:一段快速 churn 把退避顶到高位后,只要某条连接终于稳够 reconnectStableReset,
// 退避立刻清回下限,下次再断从最小退避起步(一次性抖动不永久抬高退避)。
func TestStableConnectionResetsBackoff(t *testing.T) {
	backoff := reconnectBackoffMax // 假设已被快速 churn 顶到封顶
	backoff = nextReconnectBackoff(backoff, reconnectStableReset)
	if backoff != reconnectBackoffMin {
		t.Fatalf("稳够 %v 后退避应清回下限 %v,got %v",
			reconnectStableReset, reconnectBackoffMin, backoff)
	}
}

func TestMessageReactionDeletedIsIgnored(t *testing.T) {
	conn := &recordingConn{}
	handler := buildEventDispatcher(appCfg{AppID: "cli_test"}, &sink{conn: conn})

	payload := []byte(`{
		"schema":"2.0",
		"header":{
			"event_id":"event_2",
			"event_type":"im.message.reaction.deleted_v1",
			"app_id":"cli_test",
			"tenant_key":"tenant_1",
			"create_time":"1710000000000"
		},
		"event":{
			"message_id":"om_1",
			"reaction_type":{"emoji_type":"OK"},
			"operator_type":"user",
			"action_time":"1710000000000"
		}
	}`)

	if _, err := handler.Do(context.Background(), payload); err != nil {
		t.Fatalf("reaction deleted event should be handled silently: %v", err)
	}
	if got := conn.writeCount(); got != 0 {
		t.Fatalf("reaction deleted event should not be forwarded to daemon, got %d writes", got)
	}
}

// 合并转发:空列表返回友好占位。
func TestComposeMergeForwardEmpty(t *testing.T) {
	text, res := composeMergeForwardText(nil)
	if !strings.Contains(text, "空的转发") {
		t.Fatalf("空列表应返回空占位,got: %s", text)
	}
	if len(res) != 0 {
		t.Fatalf("空列表不应有附件,got %d", len(res))
	}
}

// 合并转发:文本消息正确拼接。
func TestComposeMergeForwardTextMessages(t *testing.T) {
	items := []mergeForwardNestedMsg{
		{MsgType: "text", Content: `{"text":"第一条"}`},
		{MsgType: "text", Content: `{"text":"第二条"}`},
	}
	items[0].Sender.ID = "ou_alice"
	items[1].Sender.ID = "ou_bob"
	text, res := composeMergeForwardText(items)
	if !strings.Contains(text, "转发了 2 条") {
		t.Fatalf("应包含条数,got: %s", text)
	}
	if !strings.Contains(text, "发送人: ou_alice\n第一条") ||
		!strings.Contains(text, "发送人: ou_bob\n第二条") {
		t.Fatalf("每条消息应与自己的发送人相邻,got: %s", text)
	}
	if len(res) != 0 {
		t.Fatalf("纯文本不应有附件,got %d", len(res))
	}
}

// 合并转发:文件消息提取 file_key。
func TestComposeMergeForwardFileMessage(t *testing.T) {
	items := []mergeForwardNestedMsg{
		{MsgType: "text", Content: `{"text":"看这个文件"}`},
		{MsgType: "file", Content: `{"file_key":"file_abc123","file_name":"report.pdf"}`},
	}
	text, res := composeMergeForwardText(items)
	if !strings.Contains(text, "report.pdf") {
		t.Fatalf("应包含文件名,got: %s", text)
	}
	if len(res) != 1 {
		t.Fatalf("应有 1 个附件,got %d", len(res))
	}
	if res[0].FileKey != "file_abc123" || res[0].Kind != "file" {
		t.Fatalf("附件应是 file_abc123/file,got %+v", res[0])
	}
}

// 合并转发:图片消息提取 image_key。
func TestComposeMergeForwardImageMessage(t *testing.T) {
	items := []mergeForwardNestedMsg{
		{MsgType: "image", Content: `{"image_key":"img_xyz"}`},
	}
	text, res := composeMergeForwardText(items)
	if !strings.Contains(text, "[图片]") {
		t.Fatalf("图片应标注,got: %s", text)
	}
	if len(res) != 1 || res[0].FileKey != "img_xyz" || res[0].Kind != "image" {
		t.Fatalf("应有 image 附件 img_xyz,got %+v", res)
	}
}

// parseMessageContent: merge_forward 类型返回空(由事件处理器另行拉取)。
func TestParseMessageContentMergeForward(t *testing.T) {
	text, res := parseMessageContent("merge_forward", "Merged and Forwarded Message", nil)
	if text != "" || len(res) != 0 {
		t.Fatalf("merge_forward 应返回空,got text=%q res=%d", text, len(res))
	}
}

func TestMergeForwardHandlerReturnsBeforeBlockingFetchCompletes(t *testing.T) {
	conn := &recordingConn{}
	spool := newMergeForwardSpool(t.TempDir())
	fetcher := &blockingMergeFetcher{
		started: make(chan struct{}),
		release: make(chan struct{}),
	}
	handler := buildEventDispatcherWithMergeForward(
		appCfg{AppID: "cli_test"},
		&sink{conn: conn},
		fetcher,
		make(chan struct{}, 1),
		spool,
	)

	releaseFetch := sync.OnceFunc(func() { close(fetcher.release) })
	defer releaseFetch()
	returned := make(chan error, 1)
	go func() {
		_, err := handler.Do(context.Background(), mergeForwardEventPayload())
		returned <- err
	}()
	select {
	case <-fetcher.started:
	case <-time.After(time.Second):
		t.Fatal("后台 fetch 未启动")
	}
	// fetch 由测试显式放行:回调先返回才证明 ACK 与 REST 完成无关。
	select {
	case err := <-returned:
		if err != nil {
			t.Fatalf("merge_forward receive should be handled: %v", err)
		}
	case <-time.After(time.Second):
		t.Fatal("fetch 阻塞期间回调必须返回")
	}
	if got := conn.writeCount(); got != 0 {
		t.Fatalf("fetch 完成前不应投递半成品事件，got %d", got)
	}
	releaseFetch()
	deadline := time.Now().Add(time.Second)
	for conn.writeCount() == 0 && time.Now().Before(deadline) {
		time.Sleep(5 * time.Millisecond)
	}
	if got := conn.writeCount(); got != 1 {
		t.Fatalf("fetch 完成后应恰好投递一次，got %d", got)
	}
	var event outEvent
	if err := json.Unmarshal(conn.lastWrite(), &event); err != nil {
		t.Fatalf("异步事件 JSON 非法: %v", err)
	}
	if event.Text != "异步转发正文" || event.MessageID != "om_merge" {
		t.Fatalf("异步事件字段丢失: %+v", event)
	}
	deadline = time.Now().Add(time.Second)
	for time.Now().Before(deadline) {
		entries, err := os.ReadDir(spool.root)
		if err != nil {
			t.Fatalf("读取 spool: %v", err)
		}
		if len(entries) == 0 {
			return
		}
		time.Sleep(5 * time.Millisecond)
	}
	t.Fatal("events.sock 成功后应删除 merge_forward spool")
}

func TestSinkWriteReturnsSocketFailure(t *testing.T) {
	client, server := net.Pipe()
	_ = server.Close()
	defer client.Close()

	err := (&sink{conn: client}).write(outEvent{Type: "message", AppID: "cli_test"})
	if err == nil {
		t.Fatal("socket 写失败必须返回 error")
	}
}

func TestSinkHandshakeCompletesBeforeFeishuAndCarriesVersion(t *testing.T) {
	conn := &recordingConn{}
	sk := &sink{conn: conn}
	if err := sk.handshake("cli_test"); err != nil {
		t.Fatalf("matching daemon handshake: %v", err)
	}
	var event outEvent
	if err := json.Unmarshal(conn.lastWrite(), &event); err != nil {
		t.Fatalf("decode hello: %v", err)
	}
	if event.Type != "hello" || event.Protocol != sidecarProtocolVersion {
		t.Fatalf("hello protocol mismatch: %+v", event)
	}
}

func TestDurableCardActionsUseCrashReplayProtocol(t *testing.T) {
	for _, action := range []string{
		"bridge_mirror",
		"bridge_handoff",
		"bridge_context",
		"bridge_stop",
		"recall_rpc_handoff",
		"recover",
		"dismiss_recovery",
		"fork_submit",
		"fork_choose",
		"fork_execute",
	} {
		if !durableCardAction(map[string]interface{}{"action": action}) {
			t.Fatalf("%s 应使用 durable callback 协议", action)
		}
	}
	for _, action := range []string{"gate_ask_submit", "reset", "tui_text", "clean", ""} {
		if durableCardAction(map[string]interface{}{"action": action}) {
			t.Fatalf("%s 尚未幂等，不得进入 crash replay", action)
		}
	}
}

func TestSinkHandshakeFailsClosedWhenOldDaemonDoesNotAck(t *testing.T) {
	client, server := net.Pipe()
	defer client.Close()
	go func() {
		_, _ = bufio.NewReader(server).ReadBytes('\n')
		_ = server.Close()
	}()
	fatal := make(chan error, 1)
	sk := &sink{
		conn: client,
		onFatal: func(err error) {
			fatal <- err
		},
	}
	if err := sk.handshake("cli_test"); err == nil {
		t.Fatal("old daemon 无 ACK 时必须拒绝启动")
	}
	select {
	case <-fatal:
	case <-time.After(time.Second):
		t.Fatal("协议失败必须触发 sidecar supervisor 自愈")
	}
}

// 由 Rust 集成测试启动。这里不伪造 daemon ACK：三次都连接 Rust 临时 Unix socket，复用
// production sink 的 JSON、deadline、ACK 校验路径。未设置 socket 时普通 `go test` 跳过。
func TestRustDaemonProtocolPeer(t *testing.T) {
	socket := os.Getenv("SUMMON_RUST_DAEMON_PROTOCOL_SOCKET")
	if socket == "" {
		t.Skip("只由 Rust daemon 跨语言协议测试启动")
	}

	dial := func(t *testing.T) *sink {
		t.Helper()
		conn, err := net.DialTimeout("unix", socket, time.Second)
		if err != nil {
			t.Fatalf("dial Rust daemon socket: %v", err)
		}
		return &sink{conn: conn, reader: bufio.NewReader(conn)}
	}

	t.Run("matching_v2_and_durable_card_ack", func(t *testing.T) {
		sk := dial(t)
		defer func() {
			if sk.conn != nil {
				_ = sk.conn.Close()
			}
		}()
		if err := sk.handshake("cross-app"); err != nil {
			t.Fatalf("v2 handshake: %v", err)
		}
		if err := sk.writeDurable(outEvent{
			Type:           "card_action",
			AppID:          "cross-app",
			ChatID:         "oc_private",
			MessageID:      "om_control",
			OperatorOpenID: "ou_owner",
			EventID:        "event-cross-language",
			Value: map[string]interface{}{
				"action": "bridge_stop",
				"ticket": "ticket-opaque",
				"choice": "choice-opaque",
			},
		}); err != nil {
			t.Fatalf("durable card ACK: %v", err)
		}
	})

	for _, tc := range []struct {
		name     string
		protocol int
	}{
		{name: "legacy_missing_protocol", protocol: 0},
		{name: "wrong_protocol", protocol: sidecarProtocolVersion + 99},
	} {
		t.Run(tc.name, func(t *testing.T) {
			sk := dial(t)
			started := time.Now()
			err := sk.handshakeProtocol("cross-app", tc.protocol)
			if err == nil {
				t.Fatalf("protocol %d 必须被 Rust daemon 拒绝", tc.protocol)
			}
			if strings.Contains(err.Error(), "timeout") || time.Since(started) >= daemonAckTimeout {
				t.Fatalf("protocol %d 应由 daemon NACK fail-fast，不应等 socket timeout: %v", tc.protocol, err)
			}
		})
	}
}

func TestSinkWriteFailureMarksConnectionFatal(t *testing.T) {
	client, server := net.Pipe()
	_ = server.Close()
	defer client.Close()
	fatal := make(chan error, 1)
	err := (&sink{
		conn: client,
		onFatal: func(err error) {
			fatal <- err
		},
	}).write(outEvent{Type: "message", AppID: "cli_test"})
	if err == nil {
		t.Fatal("socket 写失败必须返回 error")
	}
	select {
	case <-fatal:
	case <-time.After(time.Second):
		t.Fatal("socket 写失败必须触发 sidecar supervisor 自愈")
	}
}

func TestSynchronousEventDoesNotAckSocketFailure(t *testing.T) {
	client, server := net.Pipe()
	_ = server.Close()
	defer client.Close()
	handler := buildEventDispatcher(appCfg{AppID: "cli_test"}, &sink{conn: client})

	if _, err := handler.Do(context.Background(), textEventPayload()); err == nil {
		t.Fatal("同步入站写 events.sock 失败时 callback 必须返回 error，不能错误 ACK")
	}
}

func TestMergeForwardSocketFailureStaysInSpool(t *testing.T) {
	client, server := net.Pipe()
	_ = server.Close()
	defer client.Close()
	fetcher := &blockingMergeFetcher{
		started: make(chan struct{}),
		release: make(chan struct{}),
	}
	spool := newMergeForwardSpool(t.TempDir())
	handler := buildEventDispatcherWithMergeForward(
		appCfg{AppID: "cli_test"},
		&sink{conn: client},
		fetcher,
		make(chan struct{}, 1),
		spool,
	)

	if _, err := handler.Do(context.Background(), mergeForwardEventPayload()); err != nil {
		t.Fatalf("原始事件落盘后可快速 ACK: %v", err)
	}
	select {
	case <-fetcher.started:
	case <-time.After(time.Second):
		t.Fatal("后台 fetch 未启动")
	}
	close(fetcher.release)
	path, err := spool.eventPath(outEvent{AppID: "cli_test", MessageID: "om_merge"})
	if err != nil {
		t.Fatalf("spool path: %v", err)
	}
	deadline := time.Now().Add(time.Second)
	for time.Now().Before(deadline) {
		body, readErr := os.ReadFile(path)
		if readErr == nil {
			var event outEvent
			if json.Unmarshal(body, &event) == nil && event.Text == "异步转发正文" {
				return
			}
		}
		time.Sleep(5 * time.Millisecond)
	}
	t.Fatal("socket 失败后应保留已富化的 merge_forward spool")
}

func TestMergeForwardServiceCachesTenantToken(t *testing.T) {
	var tokenCalls atomic.Int32
	var messageCalls atomic.Int32
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch {
		case r.URL.Path == "/open-apis/auth/v3/tenant_access_token/internal":
			tokenCalls.Add(1)
			_, _ = w.Write([]byte(`{"code":0,"tenant_access_token":"tenant-token","expire":7200}`))
		case strings.HasPrefix(r.URL.Path, "/open-apis/im/v1/messages/"):
			messageCalls.Add(1)
			if got := r.Header.Get("Authorization"); got != "Bearer tenant-token" {
				t.Errorf("Authorization header: %q", got)
			}
			_, _ = w.Write([]byte(`{"code":0,"data":{"items":[{"msg_type":"text","body":{"content":"{\"text\":\"hello\"}"},"upper_message_id":"om_merge"}]}}`))
		default:
			http.NotFound(w, r)
		}
	}))
	defer server.Close()

	service := newMergeForwardService(server.Client())
	service.baseURL = server.URL
	app := appCfg{AppID: "cli_test", AppSecret: "secret"}
	for _, messageID := range []string{"om_1", "om_2"} {
		text, _ := service.Fetch(context.Background(), app, messageID)
		if !strings.Contains(text, "hello") {
			t.Fatalf("合并转发正文缺失: %q", text)
		}
	}
	if got := tokenCalls.Load(); got != 1 {
		t.Fatalf("tenant token 应复用缓存，只请求一次，got %d", got)
	}
	if got := messageCalls.Load(); got != 2 {
		t.Fatalf("两条消息都应各拉取一次，got %d", got)
	}
}

func TestMergeForwardServiceHonorsHTTPTimeout(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		select {
		case <-r.Context().Done():
			return
		case <-time.After(2 * time.Second):
			_, _ = w.Write([]byte(`{"code":0,"tenant_access_token":"too-late","expire":7200}`))
		}
	}))
	defer server.Close()

	client := server.Client()
	client.Timeout = 50 * time.Millisecond
	service := newMergeForwardService(client)
	service.baseURL = server.URL
	startedAt := time.Now()
	text, _ := service.Fetch(
		context.Background(),
		appCfg{AppID: "cli_test", AppSecret: "secret"},
		"om_timeout",
	)
	if elapsed := time.Since(startedAt); elapsed > 500*time.Millisecond {
		t.Fatalf("HTTP timeout 未生效，耗时 %s", elapsed)
	}
	if !strings.Contains(text, "拉取失败") {
		t.Fatalf("超时应退化成可见占位，got %q", text)
	}
}

// realDeliveredInteractiveCard 是 2026-08-04 真机抓到的**投递形态**样本
// (message om_x100b000000000000000000000000004,sample测试助手 → Arcadia,sample开发群)。
// 留作固件:飞书哪天改了投递结构,这条会先红,而不是让正文静默退化成占位文案。
const realDeliveredInteractiveCard = `{"title":"[e2e-card-20260804201858] interactive 解析验证","elements":[[{"tag":"img","image_key":"img_v3_02ad_e19fca1f-912a-450e-95de-3c229091b53g"},{"tag":"text","text":"请升级至最新版本客户端，以查看内容"},{"tag":"text","text":""}]],"user_dsl":"{\"body\":{\"elements\":[{\"content\":\"<at id=ou_44444444444444444444444444444444 mention_key=@_user_1></at> [e2e-card-20260804201858] 这是 interactive 卡片正文解析的真机探针,正文应当被 sidecar 抽出。\",\"element_id\":\"_2\",\"tag\":\"markdown\"},{\"content\":\"第二段正文:验证多元素按文档顺序拼接。\",\"element_id\":\"_3\",\"tag\":\"markdown\"}]},\"config\":{\"enable_forward_interaction\":false,\"streaming_mode\":false},\"header\":{\"title\":{\"content\":\"[e2e-card-20260804201858] interactive 解析验证\",\"tag\":\"plain_text\"}},\"schema\":\"2.0\"}"}`

func TestParseInteractiveCardPrefersUserDSLOverDegradedPlaceholder(t *testing.T) {
	text, res := parseMessageContent("interactive", realDeliveredInteractiveCard, nil)
	if len(res) != 0 {
		t.Fatalf("卡片不产附件, got %v", res)
	}
	// 事故形状:降级壳里那句占位是给老客户端看的,抽到它等于没抽到正文。
	if strings.Contains(text, "请升级至最新版本客户端") {
		t.Fatalf("不该抽到降级占位文案, got %q", text)
	}
	for _, want := range []string{
		"@_user_1",
		"这是 interactive 卡片正文解析的真机探针",
		"第二段正文:验证多元素按文档顺序拼接。",
	} {
		if !strings.Contains(text, want) {
			t.Fatalf("正文缺少 %q, got %q", want, text)
		}
	}
	// <at> 标签必须归一,不能把裸 open_id 标签塞给 agent。
	if strings.Contains(text, "<at") || strings.Contains(text, "ou_44444444444444444444444444444444") {
		t.Fatalf("at 标签未归一, got %q", text)
	}
}

func TestParseInteractiveCardFallsBackWhenNoUserDSL(t *testing.T) {
	// 自己构造的 schema 2.0 卡片(发送形态)没有 user_dsl,必须走外层解析。
	card := `{"schema":"2.0","header":{"title":{"tag":"plain_text","content":"标题"}},"body":{"elements":[{"tag":"markdown","content":"正文"}]}}`
	text, _ := parseMessageContent("interactive", card, nil)
	for _, want := range []string{"标题", "正文"} {
		if !strings.Contains(text, want) {
			t.Fatalf("外层解析缺少 %q, got %q", want, text)
		}
	}
}

func TestParseInteractiveCardFallsBackWhenUserDSLUnusable(t *testing.T) {
	// user_dsl 在但内容是坏 JSON:不能因此把整条消息吃成空,要退回外层。
	card := `{"title":"外层标题","user_dsl":"{不是合法 JSON"}`
	text, _ := parseMessageContent("interactive", card, nil)
	if !strings.Contains(text, "外层标题") {
		t.Fatalf("user_dsl 不可用时应退回外层, got %q", text)
	}
}

func TestNormalizeCardMentionsHandlesQuotedAndBareAttributes(t *testing.T) {
	got := normalizeCardMentions(`<at id=ou_abc></at> 你好 <at id="ou_def" mention_key="@_user_2"></at>`)
	if strings.Contains(got, "<at") || strings.Contains(got, "ou_abc") {
		t.Fatalf("无 mention_key 的标签应删掉, got %q", got)
	}
	if !strings.Contains(got, "@_user_2") {
		t.Fatalf("带 mention_key 的应还原成 @_user_N 占位, got %q", got)
	}
}

// 发送方类型必须透到 daemon:多 bot 互 @ 时,agent 判断「这条是不是另一个 bot 发的」全靠它。
// 2026-08-04 实测缺口:sample测试助手 @ Arcadia,agent 侧看到的只是「用户」,分不出人和 bot。
func TestMessageForwardsSenderTypeApp(t *testing.T) {
	for _, tc := range []struct {
		name string
		raw  string
		want string
	}{
		{"bot 发的(飞书实测值)", `"sender_type":"bot"`, "bot"},
		{"bot 发的(文档值)", `"sender_type":"app"`, "app"},
		{"真人发的", `"sender_type":"user"`, "user"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			conn := &recordingConn{}
			handler := buildEventDispatcher(appCfg{AppID: "cli_test"}, &sink{conn: conn})
			payload := []byte(`{
				"schema":"2.0",
				"header":{"event_id":"e_st","event_type":"im.message.receive_v1","app_id":"cli_test","tenant_key":"t1","create_time":"1710000000000"},
				"event":{
					"sender":{"sender_id":{"open_id":"ou_peer_bot"},` + tc.raw + `},
					"message":{"message_id":"om_st","chat_id":"oc_g","chat_type":"group","message_type":"text","content":"{\"text\":\"hi\"}"}
				}
			}`)
			if _, err := handler.Do(context.Background(), payload); err != nil {
				t.Fatalf("receive should be handled: %v", err)
			}
			var ev outEvent
			if err := json.Unmarshal(conn.writes[0], &ev); err != nil {
				t.Fatalf("bad json: %v", err)
			}
			if ev.SenderType != tc.want {
				t.Fatalf("sender_type: want %q, got %q", tc.want, ev.SenderType)
			}
		})
	}
}

func TestCardActionToasts(t *testing.T) {
	for _, action := range []string{"recall_page", "term_auth"} {
		if _, ok := cardActionToast(action); ok {
			t.Fatalf("%s 不该弹 toast", action)
		}
	}
	if content, ok := cardActionToast("preview_open"); !ok || content != "结果会私聊发给你" {
		t.Fatalf("preview_open 应指向私聊,实际 %q %v", content, ok)
	}
	for _, action := range []string{"term_regen", "ctx_compact", ""} {
		if content, ok := cardActionToast(action); !ok || content != "收到" {
			t.Fatalf("%s 应弹「收到」,实际 %q %v", action, content, ok)
		}
	}
}

func TestDocumentCommentIsDurableWithoutMention(t *testing.T) {
	var input larkdrive.P2NoticeCommentAddV1
	raw := []byte(`{"schema":"2.0","header":{"event_id":"evt_comment","event_type":"drive.notice.comment_add_v1","app_id":"cli_test"},"event":{"notice_meta":{"file_token":"doc_test","file_type":"docx","from_user_id":{"open_id":"ou_reader"},"to_user_id":{"open_id":"ou_bot"},"notice_type":"add_comment"},"comment_id":"100","reply_id":"200","is_mentioned":false}}`)
	if err := json.Unmarshal(raw, &input); err != nil {
		t.Fatal(err)
	}
	event, ok := documentCommentEvent("cli_test", &input)
	if !ok || event.Type != "document_comment" || event.Recipient != "ou_bot" || event.Sender != "ou_reader" || event.CommentID != "100" || event.ReplyID != "200" {
		t.Fatalf("event: %+v, %v", event, ok)
	}
	conn := &recordingConn{}
	sk := &sink{conn: conn}
	if err := sk.writeDurable(event); err != nil {
		t.Fatal(err)
	}
	if conn.writeCount() != 1 {
		t.Fatal("expected one durable event")
	}
	if _, ok := documentCommentEvent("cli_test", nil); ok {
		t.Fatal("nil accepted")
	}
	input.Event.ReplyId = nil
	if _, ok := documentCommentEvent("cli_test", &input); ok {
		t.Fatal("missing reply accepted")
	}
	for _, action := range []string{"doc_comment_approve", "doc_comment_dismiss"} {
		if !durableCardAction(map[string]interface{}{"action": action}) {
			t.Fatal(action)
		}
	}
}
