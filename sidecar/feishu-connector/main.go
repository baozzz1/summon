package main

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"

	larkcore "github.com/larksuite/oapi-sdk-go/v3/core"
	"github.com/larksuite/oapi-sdk-go/v3/event/dispatcher"
	callback "github.com/larksuite/oapi-sdk-go/v3/event/dispatcher/callback"
	larkdrive "github.com/larksuite/oapi-sdk-go/v3/service/drive/v1"
	larkim "github.com/larksuite/oapi-sdk-go/v3/service/im/v1"
	larkws "github.com/larksuite/oapi-sdk-go/v3/ws"
)

// 与 daemon 经 stdin 下发的配置
type appCfg struct {
	AppID     string `json:"app_id"`
	AppSecret string `json:"app_secret"`
	Domain    string `json:"domain"`
}
type config struct {
	Language     string   `json:"language"`
	LanguageFile string   `json:"language_file"`
	EventsSock   string   `json:"events_sock"`
	Apps         []appCfg `json:"apps"`
}

type mentionOut struct {
	OpenID string `json:"open_id"`
	Key    string `json:"key"`
	Name   string `json:"name"`
	// 飞书 `mentioned_type`:被 @ 的是人(user)还是机器人(bot)。事件没给就留空,daemon 当作未知。
	Kind string `json:"kind,omitempty"`
}

// 用户随消息发来的一个附件(图片/文件/语音/视频/富文本内嵌图)。daemon 据 file_key + kind
// 从飞书下载到本地、把路径喂给 agent。对齐 Rust ipc::Resource。
type resourceOut struct {
	FileKey string `json:"file_key"`
	Kind    string `json:"kind"` // image|file|audio|media
	Name    string `json:"name,omitempty"`
}

const (
	// captureInboundEnv 入站留痕开关,默认关。夹具采样是开发行为,不该在常驻进程里常开:
	// 落的是飞书消息明文,且 append-only 无人清理(实测已攒到 1.8 MB)。
	captureInboundEnv = "SUMMON_SIDECAR_CAPTURE_INBOUND"
	// captureInboundMaxBytes 单代上限,超了就轮转。只留两代,盘上占用因此恒有上界(≈16 MB)。
	captureInboundMaxBytes = 8 << 20
)

// captureMu 串行化留痕写入:多 app 的事件回调各跑一个 goroutine,轮转的 stat+rename 必须独占,
// 否则并发下会把同一代 rename 两次(丢掉上一代)。
var captureMu sync.Mutex

// inboundCapturePath 入站消息全量留痕的 jsonl 路径(~/.summon/run/inbound/messages.jsonl)。
// 飞书消息结构繁多且会演进:开着时把每条原始事件落一行 jsonl,可拿真实样本对着 API 声明做开发/测例。
func inboundCapturePath() (string, error) {
	home, err := os.UserHomeDir()
	if err != nil {
		return "", err
	}
	return filepath.Join(home, ".summon", "run", "inbound", "messages.jsonl"), nil
}

// captureInboundEnabled 留痕只在显式 opt-in 时生效。
func captureInboundEnabled() bool {
	return os.Getenv(captureInboundEnv) == "1"
}

// buildCaptureLine 把一条入站事件连同 app_id/时间戳序列化成一行 jsonl(末尾换行)。纯函数,便于单测。
func buildCaptureLine(appID string, ts int64, event interface{}) ([]byte, error) {
	raw, err := json.Marshal(event)
	if err != nil {
		return nil, err
	}
	line, err := json.Marshal(map[string]interface{}{
		"ts":     ts,
		"app_id": appID,
		"event":  json.RawMessage(raw),
	})
	if err != nil {
		return nil, err
	}
	return append(line, '\n'), nil
}

// appendCaptureLine 追加一行留痕:写前若当前代已到上限,先挪成 `.1`(单代轮转,旧的 `.1` 被覆盖)。
// 目录 0700 / 文件 0600 与 ~/.summon 的私有约定一致——里面是飞书消息明文,不能全局可读;
// 旧版本留下的 0644 文件在这里被就地收紧。
func appendCaptureLine(path string, line []byte, maxBytes int64) error {
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return err
	}
	if st, err := os.Stat(path); err == nil && st.Size()+int64(len(line)) > maxBytes {
		if err := os.Rename(path, path+".1"); err != nil && !os.IsNotExist(err) {
			return err
		}
	}
	f, err := os.OpenFile(path, os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0o600)
	if err != nil {
		return err
	}
	// 已存在的文件不会被 OpenFile 的 perm 影响,显式收紧一次。
	if err := f.Chmod(0o600); err != nil {
		_ = f.Close()
		return err
	}
	if _, err := f.Write(line); err != nil {
		_ = f.Close()
		return err
	}
	return f.Close()
}

// captureInbound 把原始入站事件追加到 jsonl。默认关(见 captureInboundEnv);
// 开着时也是 best-effort:任何失败静默忽略,绝不影响消息流。
func captureInbound(appID string, event interface{}) {
	if !captureInboundEnabled() {
		return
	}
	path, err := inboundCapturePath()
	if err != nil {
		return
	}
	line, err := buildCaptureLine(appID, time.Now().UnixMilli(), event)
	if err != nil {
		return
	}
	captureMu.Lock()
	defer captureMu.Unlock()
	// 留痕只是开发夹具,写失败不值得中断消息流,故吞掉错误。
	_ = appendCaptureLine(path, line, captureInboundMaxBytes)
}

// parseMessageContent 把一条飞书消息的 message_type + content(JSON 串)解析成「正文 + 附件列表」。
// text 类只有正文;image/file/audio/media 各出一个附件;post(富文本)抽出拼接文本 + 内嵌图片;
// interactive(卡片)抽出可读正文。
// 认不出的类型回空,daemon 侧表现为一条空消息(不致命)。纯函数,便于单测。
func parseMessageContent(msgType, content string, mentions []mentionOut) (string, []resourceOut) {
	switch msgType {
	case "text":
		var c struct {
			Text string `json:"text"`
		}
		_ = json.Unmarshal([]byte(content), &c)
		return c.Text, nil
	case "image":
		var c struct {
			ImageKey string `json:"image_key"`
		}
		_ = json.Unmarshal([]byte(content), &c)
		if c.ImageKey == "" {
			return "", nil
		}
		return "", []resourceOut{{FileKey: c.ImageKey, Kind: "image"}}
	case "file":
		var c struct {
			FileKey  string `json:"file_key"`
			FileName string `json:"file_name"`
		}
		_ = json.Unmarshal([]byte(content), &c)
		if c.FileKey == "" {
			return "", nil
		}
		return "", []resourceOut{{FileKey: c.FileKey, Kind: "file", Name: c.FileName}}
	case "audio":
		var c struct {
			FileKey string `json:"file_key"`
		}
		_ = json.Unmarshal([]byte(content), &c)
		if c.FileKey == "" {
			return "", nil
		}
		return "", []resourceOut{{FileKey: c.FileKey, Kind: "audio"}}
	case "media":
		var c struct {
			FileKey  string `json:"file_key"`
			FileName string `json:"file_name"`
		}
		_ = json.Unmarshal([]byte(content), &c)
		if c.FileKey == "" {
			return "", nil
		}
		return "", []resourceOut{{FileKey: c.FileKey, Kind: "media", Name: c.FileName}}
	case "post":
		return parsePost(content, mentions)
	case "interactive":
		// 卡片(bot 互发回复走的就是这条)。只取正文,卡片内嵌图片不做附件。
		return parseInteractiveCard(content), nil
	case "merge_forward":
		// 合并转发的 content 只是占位 "Merged and Forwarded Message",真正内容要调 API 拉。
		// 此处返回空文本 + 无附件,由调用方(OnP2MessageReceiveV1)据 message_id 调 fetchMergeForward
		// 拉嵌套消息后再合成。parseMessageContent 是纯函数、拿不到 message_id,故 merge_forward
		// 的真正处理在事件处理器里做。
		return "", nil
	default:
		// 认不得的类型也要留痕。静默返回空,agent 那边就是「用户什么都没发」——
		// 它连「有东西没读到」都不知道,更谈不上追问。
		if msgType == "" {
			return "", nil
		}
		return fmt.Sprintf(localized("gafdbda6868"), msgType), nil
	}
}

// eventMentions 把事件里的 @ 列表压成 daemon 契约的形态。
func eventMentions(src []*larkim.MentionEvent) []mentionOut {
	var out []mentionOut
	for _, m := range src {
		if m == nil {
			continue
		}
		openID := ""
		if m.Id != nil {
			openID = larkcore.StringValue(m.Id.OpenId)
		}
		out = append(out, mentionOut{
			OpenID: openID,
			Key:    larkcore.StringValue(m.Key),
			Name:   larkcore.StringValue(m.Name),
			Kind:   larkcore.StringValue(m.MentionedType),
		})
	}
	return out
}

// postMentionKey 把 post 里 at 元素的 user_id 换回 `@_user_N` 占位。
//
// 归一到与 text 消息同一形态后,daemon 那套「@_user_N + mentions → @姓名(open_id)」的渲染
// 就通吃两种消息;各渲染各的,同一个人在 post 里和在 text 里对 agent 会长得不一样。
func postMentionKey(userID string, mentions []mentionOut) string {
	if userID == "" {
		return ""
	}
	for _, m := range mentions {
		if m.OpenID == userID && m.Key != "" {
			return m.Key
		}
	}
	return ""
}

func appendPostResource(res []resourceOut, key, kind string) []resourceOut {
	if key == "" {
		return res
	}
	return append(res, resourceOut{FileKey: key, Kind: kind})
}

// parsePost 解析富文本(图文混合):拼接各段 text/a 文本,抽出 img 元素的 image_key 作图片附件。
func parsePost(content string, mentions []mentionOut) (string, []resourceOut) {
	var p struct {
		Title   string `json:"title"`
		Content [][]struct {
			Tag       string `json:"tag"`
			Text      string `json:"text"`
			Href      string `json:"href"`
			UserID    string `json:"user_id"`
			UserName  string `json:"user_name"`
			ImageKey  string `json:"image_key"`
			FileKey   string `json:"file_key"`
			EmojiType string `json:"emoji_type"`
			Language  string `json:"language"`
		} `json:"content"`
	}
	if err := json.Unmarshal([]byte(content), &p); err != nil {
		return "", nil
	}
	var sb strings.Builder
	if p.Title != "" {
		sb.WriteString(p.Title)
		sb.WriteString("\n")
	}
	var res []resourceOut
	for _, para := range p.Content {
		for _, el := range para {
			switch el.Tag {
			case "img":
				res = appendPostResource(res, el.ImageKey, "image")
			case "media":
				res = appendPostResource(res, el.FileKey, "media")
			case "a":
				// 地址本身是正文的一部分:只留锚文本,agent 就没法打开也没法引用它。
				if el.Href != "" {
					fmt.Fprintf(&sb, "[%s](%s)", el.Text, el.Href)
				} else {
					sb.WriteString(el.Text)
				}
			case "at":
				// 归一成 `@_user_N` 交给 daemon 渲染;拿不到就退到元素自带的名字。
				// 两者皆无时不写:裸 open_id 对 agent 没有可读性(同 normalizeCardMentions 的取舍)。
				if key := postMentionKey(el.UserID, mentions); key != "" {
					sb.WriteString(key)
				} else if el.UserName != "" {
					sb.WriteString("@" + el.UserName)
				}
			case "code_block":
				// 还原成围栏:代码进 prompt 后得有边界,否则和正文粘成一片。
				fmt.Fprintf(&sb, "\n```%s\n%s\n```\n", strings.ToLower(el.Language), el.Text)
			case "emotion":
				// 按飞书自己的 `:TYPE:` 留痕,不猜它对应哪个 Unicode 字符。
				if el.EmojiType != "" {
					fmt.Fprintf(&sb, ":%s:", el.EmojiType)
				}
			case "hr":
				sb.WriteString("\n---\n")
			default:
				// text / md,以及飞书将来新增的任何带 text 的元素:**只要带正文就不丢**。
				// 白名单式分派会把没见过的元素连同用户写的内容一起吞掉,且不留任何痕迹。
				sb.WriteString(el.Text)
			}
		}
		sb.WriteString("\n")
	}
	return strings.TrimSpace(sb.String()), res
}

// --- interactive(飞书卡片)正文抽取 ---

// cardTextKeys 是卡片里承载「可读文本」的字段名。schema 1.0/2.0 都把展示文本挂在 text/content 上:
// `header.title` 的 `{"tag":"plain_text","content":"标题"}`、2.0 的 `{"tag":"markdown","content":…}`、
// 1.0 的 `{"tag":"div","text":{"tag":"lark_md","content":…}}` 皆然。
// `title` 是为**降级壳**加的:那层把标题直接写成字符串(`"title":"卡片标题"`),不认就会在
// user_dsl 不可用时连标题一起丢。卡片 DSL 里的 `title` 是对象,而这里只抽字符串,故不会误伤。
// **只认这三个键**是刻意的:改成「收集所有字符串」会把 tag/template/url/按钮 callback value
// 一起拖进正文,抽出来的东西就没法看了。
var cardTextKeys = map[string]bool{"text": true, "content": true, "title": true}

// maxCardDepth 是递归抽取的深度上限,更深的子树改用非递归方式整块跳过。防的是畸形/敌意的
// 深层嵌套 JSON 把栈撑爆——真实卡片嵌套只有个位数层,64 远够用。
const maxCardDepth = 64

// parseInteractiveCard 从卡片 content JSON 里按出现顺序抽出可读正文。
//
// **为什么是防御性遍历而不是照 schema 反序列化**:飞书卡片同时存在 schema 1.0(顶层
// `header`/`elements`)与 2.0(顶层 `schema` + `body.elements`)两种结构,元素类型还在持续新增。
// 绑定任何一版结构,都等于飞书一改版正文就静默变空。2026-08-04 已经吃过一次:bot 之间互发卡片,
// 收方 parseMessageContent 落到 default 返回空串,消息"到了"却没有正文,只能翻源码才定位得出。
//
// 用 json.Decoder 逐 token 走、而不是 unmarshal 成 map:Go 的 map 迭代顺序随机,拼出来的正文
// 每次都不一样;token 流天然是文档顺序。
//
// 解析失败或结构完全认不出 → 返回空串,绝不 panic。
//
// **投递形态与发送形态不是一回事**(2026-08-04 真机样本 om_x100b000000000000000000000000004):
// 飞书投给接收方的 content 是一层「降级壳」——
//
//	{"title":"卡片标题",
//	 "elements":[[{"tag":"img",…},{"tag":"text","text":"请升级至最新版本客户端，以查看内容"}]],
//	 "user_dsl":"{…发送时那份卡片 JSON 的转义字符串…}"}
//
// `elements` 里只有给老客户端看的占位文案,真正的正文在 `user_dsl` 里。所以先剥 user_dsl,
// 剥不出来才退回外层——用「优先级」而不是「过滤占位文案」来绕开它:硬编码那句中文一旦飞书改
// 文案或换语言就失效,而 user_dsl 与占位是配套下发的,有它就一定该用它。
func parseInteractiveCard(content string) string {
	if dsl := extractCardUserDSL(content); dsl != "" {
		if text := collectCardText(dsl); text != "" {
			return text
		}
	}
	// 退回外层:自己构造的卡片、老结构、以及飞书哪天不再下发 user_dsl 时都走这条。
	return collectCardText(content)
}

// extractCardUserDSL 取出降级壳里的 `user_dsl`(其值本身是一份完整卡片 JSON 的字符串)。
// 没有该字段 / 不是合法 JSON → 空串,调用方自然退回外层解析。
func extractCardUserDSL(content string) string {
	var shell struct {
		UserDSL string `json:"user_dsl"`
	}
	if err := json.Unmarshal([]byte(content), &shell); err != nil {
		return ""
	}
	return shell.UserDSL
}

// collectCardText 对一份卡片 JSON 做递归抽取,按文档顺序拼成正文,并把 `<at>` 标签归一成
// daemon 认识的 `@_user_N` 占位。
func collectCardText(card string) string {
	dec := json.NewDecoder(strings.NewReader(card))
	var parts []string
	// 半路失败(JSON 截断/畸形)也保留已抽到的片段:残缺正文比整条空消息可用;
	// 完全认不出时 parts 为空,下面自然返回空串。故这里显式忽略错误。
	_ = walkCardValue(dec, "", 0, &parts)
	var sb strings.Builder
	for _, part := range parts {
		trimmed := strings.TrimSpace(normalizeCardMentions(part))
		if trimmed == "" {
			continue
		}
		if sb.Len() > 0 {
			sb.WriteString("\n")
		}
		sb.WriteString(trimmed)
	}
	return sb.String()
}

// cardAtTagPattern 匹配卡片 DSL 里的 at 标签,成对与自闭合两种写法都吃:
// `<at id=ou_xxx mention_key=@_user_1></at>`、`<at id="ou_xxx"/>`。
var cardAtTagPattern = regexp.MustCompile(`(?s)<at\b[^>]*>.*?</at>|<at\b[^>]*/>`)

// cardMentionKeyPattern 从 at 标签里抠出 `mention_key`(飞书下发时不带引号,自己构造的可能带)。
var cardMentionKeyPattern = regexp.MustCompile(`mention_key\s*=\s*"?(@_[A-Za-z0-9_]+)"?`)

// normalizeCardMentions 把卡片里的 `<at …></at>` 换成 `@_user_N` 占位。
//
// **为什么要归一**:普通 text 消息的 @ 本来就是 `@_user_N` + 事件里的 mentions 列表,daemon 侧
// 已有的渲染会把它变成「@姓名(open_id)」。卡片走的却是 `<at id=ou_…>` 这种标签,不归一的话
// agent 看到的是一串裸 open_id 标签,既认不出 @ 的是谁,也和文本消息的语义对不上。
// 拿不到 mention_key 的标签直接删掉——留着只是噪音,open_id 对 agent 没有可读性。
func normalizeCardMentions(s string) string {
	return cardAtTagPattern.ReplaceAllStringFunc(s, func(tag string) string {
		if m := cardMentionKeyPattern.FindStringSubmatch(tag); len(m) == 2 {
			return m[1]
		}
		return ""
	})
}

// walkCardValue 消费 dec 上的一个完整 JSON 值,把命中 cardTextKeys 的字符串按出现顺序追加进 out。
// key 是该值在父对象里的字段名(顶层为空;数组元素继承外层 key,故 `{"content":["a","b"]}` 也抽得到)。
func walkCardValue(dec *json.Decoder, key string, depth int, out *[]string) error {
	if depth > maxCardDepth {
		return skipCardValue(dec)
	}
	tok, err := dec.Token()
	if err != nil {
		return err
	}
	delim, isDelim := tok.(json.Delim)
	if !isDelim {
		// 标量。只有挂在 text/content 上的字符串算正文,数字/布尔/null 一律丢弃。
		if s, ok := tok.(string); ok && cardTextKeys[key] {
			*out = append(*out, s)
		}
		return nil
	}
	switch delim {
	case '{':
		for dec.More() {
			keyTok, err := dec.Token()
			if err != nil {
				return err
			}
			// 对象的键必然是字符串;拿不到就当空 key,该值只会被当作非正文跳过。
			field, _ := keyTok.(string)
			if err := walkCardValue(dec, field, depth+1, out); err != nil {
				return err
			}
		}
	case '[':
		for dec.More() {
			if err := walkCardValue(dec, key, depth+1, out); err != nil {
				return err
			}
		}
	default:
		// '}' / ']' 在这里出现说明 token 流错位(dec.More() 本该挡住),交给调用方兜底。
		return fmt.Errorf("unexpected delim %q", delim)
	}
	// 吃掉配对的 '}' / ']'。
	_, err = dec.Token()
	return err
}

// skipCardValue 非递归地吃掉 dec 上的一个完整 JSON 值。只给超深子树用——递归跳过同样会爆栈,
// 那就白防了。
func skipCardValue(dec *json.Decoder) error {
	depth := 0
	for {
		tok, err := dec.Token()
		if err != nil {
			return err
		}
		if delim, ok := tok.(json.Delim); ok {
			switch delim {
			case '{', '[':
				depth++
			case '}', ']':
				depth--
			}
		}
		// 标量值第一个 token 就结束;容器等配对闭合。
		if depth == 0 {
			return nil
		}
	}
}

// --- 合并转发(merge_forward)支持 ---

// mergeForwardNestedMsg 是飞书 merge_forward API 返回的单条嵌套消息。
type mergeForwardNestedMsg struct {
	MsgType string `json:"msg_type"`
	// Content 只供纯函数测试/兼容旧样本；真实 get-message API 把正文放在 body.content。
	Content string `json:"-"`
	Body    struct {
		Content string `json:"content"`
	} `json:"body"`
	CreateTime     string `json:"create_time"`
	UpperMessageID string `json:"upper_message_id"`
	Sender         struct {
		ID         string `json:"id"`
		IDType     string `json:"id_type"`
		SenderType string `json:"sender_type"`
	} `json:"sender"`
	// 嵌套消息各有自己的 @ 列表,不能共用外层的:同一个 `@_user_1` 在两条子消息里指的是两个人。
	Mentions []struct {
		ID   string `json:"id"`
		Key  string `json:"key"`
		Name string `json:"name"`
	} `json:"mentions"`
}

func (m mergeForwardNestedMsg) mentionList() []mentionOut {
	out := make([]mentionOut, 0, len(m.Mentions))
	for _, x := range m.Mentions {
		out = append(out, mentionOut{OpenID: x.ID, Key: x.Key, Name: x.Name})
	}
	return out
}

func (m mergeForwardNestedMsg) bodyContent() string {
	if m.Content != "" {
		return m.Content
	}
	return m.Body.Content
}

// feishuAPIBase 根据 app.Domain 返回 API base URL。
func feishuAPIBase(domain string) string {
	if domain == "larksuite.com" || domain == "open.larksuite.com" {
		return "https://open.larksuite.com"
	}
	return "https://open.feishu.cn"
}

const mergeForwardTimeout = 12 * time.Second

var defaultFeishuHTTPClient = &http.Client{
	Timeout: 10 * time.Second,
}

type cachedTenantToken struct {
	token     string
	expiresAt time.Time
}

type tenantTokenCache struct {
	mu     sync.Mutex
	tokens map[string]cachedTenantToken
}

func newTenantTokenCache() *tenantTokenCache {
	return &tenantTokenCache{tokens: make(map[string]cachedTenantToken)}
}

func (c *tenantTokenCache) get(base string, app appCfg) (string, bool) {
	c.mu.Lock()
	defer c.mu.Unlock()
	entry, ok := c.tokens[base+"\x00"+app.AppID]
	return entry.token, ok && entry.token != "" && time.Now().Before(entry.expiresAt)
}

func (c *tenantTokenCache) put(base string, app appCfg, token string, expiresIn time.Duration) {
	// 正常 token 为 2h；提前 5 分钟刷新。测试/异常短 TTL 则取一半，永不缓存到过期边缘。
	if expiresIn > 5*time.Minute {
		expiresIn -= 5 * time.Minute
	} else {
		expiresIn /= 2
	}
	c.mu.Lock()
	c.tokens[base+"\x00"+app.AppID] = cachedTenantToken{
		token:     token,
		expiresAt: time.Now().Add(expiresIn),
	}
	c.mu.Unlock()
}

// fetchTenantAccessToken 获取并缓存 tenant_access_token。所有网络请求必须带 ctx + client 总超时。
func fetchTenantAccessToken(
	ctx context.Context,
	client *http.Client,
	cache *tenantTokenCache,
	base string,
	app appCfg,
) (string, error) {
	if token, ok := cache.get(base, app); ok {
		return token, nil
	}
	endpoint := base + "/open-apis/auth/v3/tenant_access_token/internal"
	body, err := json.Marshal(map[string]string{"app_id": app.AppID, "app_secret": app.AppSecret})
	if err != nil {
		return "", err
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, endpoint, bytes.NewReader(body))
	if err != nil {
		return "", err
	}
	req.Header.Set("Content-Type", "application/json; charset=utf-8")
	resp, err := client.Do(req)
	if err != nil {
		return "", err
	}
	defer resp.Body.Close()
	b, err := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if err != nil {
		return "", err
	}
	var r struct {
		Code              int    `json:"code"`
		Msg               string `json:"msg"`
		TenantAccessToken string `json:"tenant_access_token"`
		Expire            int64  `json:"expire"`
	}
	if err := json.Unmarshal(b, &r); err != nil {
		return "", err
	}
	if resp.StatusCode < 200 || resp.StatusCode >= 300 || r.Code != 0 || r.TenantAccessToken == "" {
		return "", fmt.Errorf("tenant_access_token: status=%d code=%d msg=%s", resp.StatusCode, r.Code, r.Msg)
	}
	if r.Expire <= 0 {
		r.Expire = 7200
	}
	cache.put(base, app, r.TenantAccessToken, time.Duration(r.Expire)*time.Second)
	return r.TenantAccessToken, nil
}

type mergeForwardFetcher interface {
	Fetch(ctx context.Context, app appCfg, messageID string) (string, []resourceOut)
}

type mergeForwardService struct {
	http    *http.Client
	tokens  *tenantTokenCache
	baseURL string // 仅测试注入；生产留空并按 app.Domain 选择飞书/Lark 官方域名。
}

func newMergeForwardService(client *http.Client) *mergeForwardService {
	return &mergeForwardService{http: client, tokens: newTenantTokenCache()}
}

var defaultMergeForwardService = newMergeForwardService(defaultFeishuHTTPClient)

const maxConcurrentMergeForwardFetches = 8

var defaultMergeForwardSlots = make(chan struct{}, maxConcurrentMergeForwardFetches)

// Fetch 按官方“获取指定消息内容”接口拉合并转发子消息。失败返回友好占位，调用方仍会最终投事件。
func (s *mergeForwardService) Fetch(
	ctx context.Context,
	app appCfg,
	messageID string,
) (string, []resourceOut) {
	base := s.baseURL
	if base == "" {
		base = feishuAPIBase(app.Domain)
	}
	token, err := fetchTenantAccessToken(ctx, s.http, s.tokens, base, app)
	if err != nil {
		return fmt.Sprintf(localized("g179932139f"), err), nil
	}
	// merge_forward 消息自身 content 是固定占位；官方要求用 GET /messages/:message_id 获取子消息。
	endpoint := base + "/open-apis/im/v1/messages/" + url.PathEscape(messageID) + "?user_id_type=open_id"
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, endpoint, nil)
	if err != nil {
		return fmt.Sprintf(localized("g1de79394b4"), err), nil
	}
	req.Header.Set("Authorization", "Bearer "+token)
	resp, err := s.http.Do(req)
	if err != nil {
		return fmt.Sprintf(localized("g179932139f"), err), nil
	}
	defer resp.Body.Close()
	b, err := io.ReadAll(io.LimitReader(resp.Body, 8<<20))
	if err != nil {
		return fmt.Sprintf(localized("g4c694167fa"), err), nil
	}
	var r struct {
		Code int    `json:"code"`
		Msg  string `json:"msg"`
		Data struct {
			Items []mergeForwardNestedMsg `json:"items"`
		} `json:"data"`
	}
	if err := json.Unmarshal(b, &r); err != nil {
		return fmt.Sprintf(localized("gcb08332c53"), err), nil
	}
	if resp.StatusCode < 200 || resp.StatusCode >= 300 || r.Code != 0 {
		return fmt.Sprintf(localized("g801309a109"), resp.StatusCode, r.Code, r.Msg), nil
	}
	items := make([]mergeForwardNestedMsg, 0, len(r.Data.Items))
	for _, item := range r.Data.Items {
		// API 有时会把外层 merge_forward 自身也放在 items；只把真正子消息交给 agent。
		if item.MsgType == "merge_forward" && item.UpperMessageID == "" {
			continue
		}
		items = append(items, item)
	}
	return composeMergeForwardText(items)
}

// composeMergeForwardText 把嵌套消息列表合成给 agent 看的文本 + 附件。
// 每条嵌套消息用「---」分隔,文本消息直接展示、文件/图片消息标注类型。
func composeMergeForwardText(items []mergeForwardNestedMsg) (string, []resourceOut) {
	if len(items) == 0 {
		return localized("gce34f452ff"), nil
	}
	var sb strings.Builder
	fmt.Fprintf(&sb, localized("g01acb03ba9"), len(items))
	var resources []resourceOut
	for _, item := range items {
		fmt.Fprintf(&sb, "\n---\n")
		fmt.Fprintf(&sb, localized("g78cef7df57"), mergeForwardSenderLabel(item))
		content := item.bodyContent()
		text, res := parseMessageContent(item.MsgType, content, item.mentionList())
		if text != "" {
			fmt.Fprintf(&sb, "%s", text)
		} else if item.MsgType == "image" {
			fmt.Fprint(&sb, localized("ga71f445d35"))
		} else if item.MsgType == "file" {
			var c struct {
				FileName string `json:"file_name"`
			}
			_ = json.Unmarshal([]byte(content), &c)
			if c.FileName != "" {
				fmt.Fprintf(&sb, localized("g0bccf854e9"), c.FileName)
			} else {
				fmt.Fprint(&sb, localized("g54e20fcfb5"))
			}
		} else if item.MsgType == "audio" {
			fmt.Fprint(&sb, localized("gfee630ed57"))
		} else if item.MsgType == "media" {
			fmt.Fprint(&sb, localized("gda5d0399eb"))
		} else if item.MsgType == "post" {
			t, _ := parsePost(content, item.mentionList())
			if t != "" {
				fmt.Fprintf(&sb, "%s", t)
			}
		}
		resources = append(resources, res...)
	}
	return strings.TrimSpace(sb.String()), resources
}

func mergeForwardSenderLabel(item mergeForwardNestedMsg) string {
	id := strings.TrimSpace(item.Sender.ID)
	if id == "" {
		if item.Sender.SenderType == "app" {
			return localized("g63c73c4730")
		}
		return localized("g1be0cad4e9")
	}
	if item.Sender.SenderType == "app" {
		return localized("g4014c462d3") + id + ")"
	}
	return id
}

// 写回 daemon 的事件(对齐 Rust ipc::Event,内部 tag = "type")。字段并集:
// message 用 chat_type/sender/text/mentions;card_action 用 action/operator_open_id/event_id。
// 注意:Sender/Text/MessageID 不加 omitempty —— Rust Event::Message 这几个字段无 default,
// 空文本消息(图片等 text="")也必须显式带上,否则 daemon 解析报错。
type outEvent struct {
	Type          string `json:"type"`
	AppID         string `json:"app_id"`
	Protocol      int    `json:"protocol,omitempty"`
	ChatID        string `json:"chat_id"`
	ChatType      string `json:"chat_type,omitempty"`
	Sender        string `json:"sender"`
	SenderUnionID string `json:"sender_union_id,omitempty"`
	// 发送方类型:`"user"` = 真人,`"app"` = bot。daemon 据此在注入 prompt 里标注发送方身份。
	// **为什么必须透出来**:多 bot 同群互 @ 时,agent 判断「这条是不是另一个 bot 发的、要不要
	// 应答」全靠这个信号;缺了它,bot 和真人在 agent 眼里长得一模一样,防回音规则失去依据。
	SenderType     string                 `json:"sender_type,omitempty"`
	Text           string                 `json:"text"`
	MessageID      string                 `json:"message_id"`
	ParentID       string                 `json:"parent_id,omitempty"`
	ThreadID       string                 `json:"thread_id,omitempty"`
	RootID         string                 `json:"root_id,omitempty"`
	Mentions       []mentionOut           `json:"mentions,omitempty"`
	Resources      []resourceOut          `json:"resources,omitempty"`
	Value          map[string]interface{} `json:"value,omitempty"`
	OperatorOpenID string                 `json:"operator_open_id,omitempty"`
	EventID        string                 `json:"event_id,omitempty"`
	FileToken      string                 `json:"file_token,omitempty"`
	FileType       string                 `json:"file_type,omitempty"`
	CommentID      string                 `json:"comment_id,omitempty"`
	ReplyID        string                 `json:"reply_id,omitempty"`
	Recipient      string                 `json:"recipient,omitempty"`
	Detail         string                 `json:"detail,omitempty"`
	// 飞书消息创建时间(毫秒)。daemon 入站去重+时效闸用来判「抖动重连后补投的旧事件」。
	CreateTimeMs int64 `json:"create_time_ms,omitempty"`
}

type sink struct {
	mu      sync.Mutex
	conn    net.Conn
	reader  *bufio.Reader
	onFatal func(error)
}

// The handshake binds message delivery, durable card actions, and document comments.
const sidecarProtocolVersion = 3
const daemonAckTimeout = 2 * time.Second

func (s *sink) write(ev outEvent) error {
	b, err := json.Marshal(ev)
	if err != nil {
		return fmt.Errorf("encode event: %w", err)
	}
	b = append(b, '\n')
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.conn == nil {
		return fmt.Errorf("events sink is disconnected")
	}
	if _, err := io.Copy(s.conn, bytes.NewReader(b)); err != nil {
		err = fmt.Errorf("write events.sock: %w", err)
		s.fail(err)
		return err
	}
	return nil
}

type durableAck struct {
	Type     string `json:"type"`
	EventID  string `json:"event_id"`
	OK       bool   `json:"ok"`
	Status   string `json:"status"`
	Protocol int    `json:"protocol"`
}

// handshake 在任何飞书 WS 连接启动前完成。新 sidecar 连老 daemon 时等不到
// ACK 会直接退出；daemon 也会拒绝未握手的 card_action，避免跨版本假成功。
func (s *sink) handshake(appID string) error {
	return s.handshakeProtocol(appID, sidecarProtocolVersion)
}

// handshakeProtocol 保留完整生产握手读写路径，并让跨语言协议测试能发送旧版(字段缺失=0)
// 或错误版本。生产入口始终只传 sidecarProtocolVersion。
func (s *sink) handshakeProtocol(appID string, protocol int) error {
	ev := outEvent{Type: "hello", AppID: appID, Protocol: protocol}
	b, err := json.Marshal(ev)
	if err != nil {
		return fmt.Errorf("encode protocol hello: %w", err)
	}
	b = append(b, '\n')
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.conn == nil {
		return fmt.Errorf("events sink is disconnected")
	}
	if _, err := io.Copy(s.conn, bytes.NewReader(b)); err != nil {
		err = fmt.Errorf("write protocol hello: %w", err)
		s.fail(err)
		return err
	}
	if s.reader == nil {
		s.reader = bufio.NewReader(s.conn)
	}
	_ = s.conn.SetReadDeadline(time.Now().Add(daemonAckTimeout))
	line, err := s.reader.ReadBytes('\n')
	_ = s.conn.SetReadDeadline(time.Time{})
	if err != nil {
		err = fmt.Errorf("read protocol hello ACK: %w", err)
		s.fail(err)
		return err
	}
	var ack durableAck
	if err := json.Unmarshal(line, &ack); err != nil {
		err = fmt.Errorf("decode protocol hello ACK: %w", err)
		s.fail(err)
		return err
	}
	if ack.Type != "hello_ack" || !ack.OK || ack.Protocol != sidecarProtocolVersion {
		err := fmt.Errorf(
			"daemon rejected sidecar protocol: got=%d status=%s",
			ack.Protocol,
			ack.Status,
		)
		s.fail(err)
		return err
	}
	return nil
}

// card callback 只有拿到 daemon “WAL 已 durable”应用级 ACK 才能向飞书成功返回。单纯
// write(events.sock) 只证明进入内核缓冲区，daemon 随后崩溃会永久丢 stop/context。
func (s *sink) writeDurable(ev outEvent) error {
	b, err := json.Marshal(ev)
	if err != nil {
		return fmt.Errorf("encode event: %w", err)
	}
	b = append(b, '\n')
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.conn == nil {
		return fmt.Errorf("events sink is disconnected")
	}
	if _, err := io.Copy(s.conn, bytes.NewReader(b)); err != nil {
		err = fmt.Errorf("write events.sock: %w", err)
		s.fail(err)
		return err
	}
	if s.reader == nil {
		s.reader = bufio.NewReader(s.conn)
	}
	_ = s.conn.SetReadDeadline(time.Now().Add(daemonAckTimeout))
	line, err := s.reader.ReadBytes('\n')
	_ = s.conn.SetReadDeadline(time.Time{})
	if err != nil {
		err = fmt.Errorf("read durable card ACK: %w", err)
		s.fail(err)
		return err
	}
	var ack durableAck
	if err := json.Unmarshal(line, &ack); err != nil {
		err = fmt.Errorf("decode durable card ACK: %w", err)
		s.fail(err)
		return err
	}
	expected := "card_action_ack"
	if ev.Type == "document_comment" {
		expected = "document_comment_ack"
	}
	if ack.Type != expected || ack.EventID != ev.EventID || !ack.OK {
		err := fmt.Errorf("daemon rejected card callback ACK: type=%s status=%s", ack.Type, ack.Status)
		s.fail(err)
		return err
	}
	return nil
}

func (s *sink) fail(err error) {
	if s.conn != nil {
		_ = s.conn.Close()
		s.conn = nil
	}
	if s.onFatal != nil {
		s.onFatal(err)
	}
}

// mergeForwardSpool 是 merge_forward 异步 ACK 与 daemon socket 之间的本地持久层。callback
// 原始事件落盘后才 ACK；富化记录持有转交权，events.sock 写成功后由该记录移除。
type mergeForwardSpool struct {
	mu           sync.Mutex
	root         string
	transferring map[string]struct{}
}

func newMergeForwardSpool(root string) *mergeForwardSpool {
	return &mergeForwardSpool{root: root}
}

func defaultMergeForwardSpoolPath() string {
	home, err := os.UserHomeDir()
	if err != nil {
		return ""
	}
	return filepath.Join(home, ".summon", "run", "sidecar-merge-outbox")
}

var defaultMergeForwardSpoolStore = newMergeForwardSpool(defaultMergeForwardSpoolPath())

func safeSpoolComponent(value string) string {
	var b strings.Builder
	for _, r := range value {
		if r <= 127 && (r == '-' || r == '_' || r >= '0' && r <= '9' ||
			r >= 'A' && r <= 'Z' || r >= 'a' && r <= 'z') {
			b.WriteRune(r)
		} else {
			b.WriteByte('_')
		}
	}
	return b.String()
}

func (s *mergeForwardSpool) eventPath(event outEvent) (string, error) {
	if s == nil || s.root == "" {
		return "", fmt.Errorf("merge_forward spool path unavailable")
	}
	appID := safeSpoolComponent(event.AppID)
	messageID := safeSpoolComponent(event.MessageID)
	if appID == "" || messageID == "" {
		return "", fmt.Errorf("merge_forward spool requires app_id and message_id")
	}
	return filepath.Join(s.root, appID+"-"+messageID+".json"), nil
}

func writePrivateAtomic(path string, body []byte) error {
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return err
	}
	tmp := path + ".tmp"
	f, err := os.OpenFile(tmp, os.O_CREATE|os.O_TRUNC|os.O_WRONLY, 0o600)
	if err != nil {
		return err
	}
	if _, err = f.Write(body); err == nil {
		err = f.Sync()
	}
	closeErr := f.Close()
	if err == nil {
		err = closeErr
	}
	if err != nil {
		_ = os.Remove(tmp)
		return err
	}
	if err := os.Rename(tmp, path); err != nil {
		_ = os.Remove(tmp)
		return err
	}
	return nil
}

// A persisted record owns exactly the bytes observed at its path. A stale fetch or
// sender cannot overwrite or remove a newer record with the same message key.
type mergeForwardRecord struct {
	spool *mergeForwardSpool
	path  string
	event outEvent
	body  []byte
}

type pendingMergeForward struct{ record mergeForwardRecord }
type readyMergeForward struct{ record mergeForwardRecord }

var errMergeForwardBusy = errors.New("merge_forward fetch capacity exhausted")

func (s *mergeForwardSpool) begin(event outEvent, slots chan struct{}) (*pendingMergeForward, error) {
	if s == nil {
		return nil, fmt.Errorf("merge_forward spool unavailable")
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	path, err := s.eventPath(event)
	if err != nil {
		return nil, err
	}
	if existing, err := s.loadLocked(path); err == nil {
		if existing.event.AppID != event.AppID || existing.event.MessageID != event.MessageID {
			return nil, fmt.Errorf("merge_forward spool identity collision")
		}
		// A persisted callback already owns fetching or recovery; duplicates only ACK it.
		return nil, nil
	} else if !os.IsNotExist(err) {
		return nil, err
	}
	if slots != nil {
		select {
		case slots <- struct{}{}:
		default:
			return nil, errMergeForwardBusy
		}
	}
	acquired := slots != nil
	defer func() {
		if acquired {
			<-slots
		}
	}()
	body, err := json.Marshal(event)
	if err != nil {
		return nil, err
	}
	if err := writePrivateAtomic(path, body); err != nil {
		return nil, err
	}
	acquired = false
	return &pendingMergeForward{mergeForwardRecord{s, path, event, body}}, nil
}

func (s *mergeForwardSpool) loadLocked(path string) (mergeForwardRecord, error) {
	body, err := os.ReadFile(path)
	if err != nil {
		return mergeForwardRecord{}, err
	}
	var event outEvent
	if err := json.Unmarshal(body, &event); err != nil {
		return mergeForwardRecord{}, fmt.Errorf("decode merge_forward spool %s: %w", path, err)
	}
	expected, err := s.eventPath(event)
	if err != nil {
		return mergeForwardRecord{}, err
	}
	if event.Type != "message" || expected != path {
		return mergeForwardRecord{}, fmt.Errorf("merge_forward spool identity mismatch")
	}
	return mergeForwardRecord{s, path, event, body}, nil
}

func (r mergeForwardRecord) currentLocked() error {
	body, err := os.ReadFile(r.path)
	if err != nil {
		return err
	}
	if !bytes.Equal(body, r.body) {
		return fmt.Errorf("merge_forward spool owner is stale")
	}
	return nil
}

func (r mergeForwardRecord) replaceLocked(event outEvent) (mergeForwardRecord, error) {
	if err := r.currentLocked(); err != nil {
		return mergeForwardRecord{}, err
	}
	body, err := json.Marshal(event)
	if err != nil {
		return mergeForwardRecord{}, err
	}
	if err := writePrivateAtomic(r.path, body); err != nil {
		return mergeForwardRecord{}, err
	}
	return mergeForwardRecord{r.spool, r.path, event, body}, nil
}

func (p *pendingMergeForward) enrich(text string, resources []resourceOut) (*readyMergeForward, error) {
	r := p.record
	r.spool.mu.Lock()
	defer r.spool.mu.Unlock()
	event := r.event
	if text != "" {
		event.Text = text
	}
	event.Resources = append(append([]resourceOut(nil), event.Resources...), resources...)
	enriched, err := r.replaceLocked(event)
	if err != nil {
		return nil, err
	}
	return &readyMergeForward{enriched}, nil
}

// 转交证明仅限 socket write 完成；daemon 尚未返回业务持久接受证明。
// 网络等待只占用本记录的转交权，其它消息仍可落盘并向 SDK 返回。
func (r *readyMergeForward) transfer(sk *sink) error {
	s := r.record.spool
	s.mu.Lock()
	if err := r.record.currentLocked(); err != nil {
		s.mu.Unlock()
		return err
	}
	if _, busy := s.transferring[r.record.path]; busy {
		s.mu.Unlock()
		return fmt.Errorf("merge_forward record is already transferring")
	}
	if s.transferring == nil {
		s.transferring = make(map[string]struct{})
	}
	s.transferring[r.record.path] = struct{}{}
	s.mu.Unlock()
	released := false
	defer func() {
		if !released {
			s.mu.Lock()
			delete(s.transferring, r.record.path)
			s.mu.Unlock()
		}
	}()
	if err := sk.write(r.record.event); err != nil {
		return err
	}
	s.mu.Lock()
	defer func() {
		delete(s.transferring, r.record.path)
		released = true
		s.mu.Unlock()
	}()
	if err := r.record.currentLocked(); err != nil {
		return err
	}
	return os.Remove(r.record.path)
}

// drain 在重连时读取持久记录；拉取中断的原始记录转为可见降级结果。
// 网络写入期间不持有全局锁，同 message_id 的重放由 daemon 既有入站去重收口。
func (s *mergeForwardSpool) drain(sk *sink) error {
	if s == nil {
		return fmt.Errorf("merge_forward spool unavailable")
	}
	if s.root == "" {
		return fmt.Errorf("merge_forward spool path unavailable")
	}
	entries, err := os.ReadDir(s.root)
	if os.IsNotExist(err) {
		return nil
	}
	if err != nil {
		return err
	}
	sort.Slice(entries, func(i, j int) bool { return entries[i].Name() < entries[j].Name() })
	for _, entry := range entries {
		if entry.IsDir() || filepath.Ext(entry.Name()) != ".json" {
			continue
		}
		s.mu.Lock()
		record, err := s.loadLocked(filepath.Join(s.root, entry.Name()))
		if err == nil && record.event.Text == "" {
			event := record.event
			event.Text = localized("g28c4ca3c9c")
			record, err = record.replaceLocked(event)
		}
		s.mu.Unlock()
		if err != nil {
			return err
		}
		ready := readyMergeForward{record}
		if err := ready.transfer(sk); err != nil {
			return err
		}
	}
	return nil
}

func main() {
	if len(os.Args) == 2 && os.Args[1] == "protocol-version" {
		fmt.Println(sidecarProtocolVersion)
		return
	}
	var cfg config
	if err := json.NewDecoder(bufio.NewReader(os.Stdin)).Decode(&cfg); err != nil {
		fmt.Fprintf(os.Stderr, "config decode: %v\n", err)
		os.Exit(1)
	}
	configureLocale(cfg.Language, cfg.LanguageFile)
	// capture-one(onboard 联调用):连一个 app,收到第一条消息把 {open_id,chat_id,text} 打到 stdout 即退出;
	// 不连 events.sock、不碰常驻 daemon。死随父同常驻模式。超时由调用方(向导)kill 控制。
	if os.Getenv("SIDECAR_CAPTURE_ONE") == "1" {
		go func() { _, _ = io.Copy(io.Discard, os.Stdin); os.Exit(0) }()
		os.Exit(captureOne(cfg))
	}

	conn, err := net.Dial("unix", cfg.EventsSock)
	if err != nil {
		fmt.Fprintf(os.Stderr, "dial events.sock: %v\n", err)
		os.Exit(1)
	}
	defer conn.Close()
	sk := &sink{
		conn:   conn,
		reader: bufio.NewReader(conn),
		onFatal: func(err error) {
			fmt.Fprintf(os.Stderr, "events.sock protocol failed: %v\n", err)
			os.Exit(1)
		},
	}
	appID := "sidecar"
	if len(cfg.Apps) > 0 {
		appID = cfg.Apps[0].AppID
	}
	if err := sk.handshake(appID); err != nil {
		fmt.Fprintf(os.Stderr, "events.sock handshake: %v\n", err)
		os.Exit(1)
	}

	// 死随父:daemon 持有我们 stdin 的写端;它一退出(stop/crash/kill),stdin 读到 EOF → 自杀。
	// 杜绝孤儿边车(否则 daemon stop 后边车残留、抢飞书事件)。
	go func() {
		_, _ = io.Copy(io.Discard, os.Stdin)
		os.Exit(0)
	}()

	// selftest:发一条合成事件后退出(验证 bridge,无飞书)
	if os.Getenv("SIDECAR_SELFTEST") == "1" {
		appID := "stub:selftest"
		if len(cfg.Apps) > 0 {
			appID = cfg.Apps[0].AppID
		}
		if err := sk.write(outEvent{Type: "message", AppID: appID, ChatID: "selftest-chat",
			Sender: "owner", Text: "selftest ping", MessageID: "selftest-1"}); err != nil {
			fmt.Fprintf(os.Stderr, "selftest write: %v\n", err)
			os.Exit(1)
		}
		time.Sleep(300 * time.Millisecond)
		return
	}
	if err := defaultMergeForwardSpoolStore.drain(sk); err != nil {
		// 保留 spool 记录并退出，让 daemon 重启 sidecar 后重试；继续运行会让新事件越积越多。
		fmt.Fprintf(os.Stderr, "drain merge_forward spool: %v\n", err)
		os.Exit(1)
	}

	var wg sync.WaitGroup
	for _, app := range cfg.Apps {
		app := app
		wg.Add(1)
		go func() {
			defer wg.Done()
			runApp(app, sk)
		}()
	}
	wg.Wait()
}

// cardActionToast 决定点击后弹什么 toast,第二个返回值为 false 时不弹。召回卡翻页(recall_page)
// 是纯读:点完 daemon 会 PATCH 卡片,卡片变化本身就是反馈;终端链接按钮(term_auth)点了就跳去
// 浏览器,toast 会盖在正在打开的页面上。文件按钮(preview_open)的结果在私聊里,toast 在 daemon
// 判权之前就要回给飞书,只能指路、不能报结果。其余动作保留即时确认 toast。
func cardActionToast(action string) (string, bool) {
	switch action {
	case "recall_page", "term_auth":
		return "", false
	case "preview_open":
		return localized("g3f601feb41"), true
	default:
		return localized("ge2c6751f82"), true
	}
}

// cardActionString 从按钮 value 里取 action 字符串(缺/类型不符回空)。
func cardActionString(value map[string]interface{}) string {
	if v, ok := value["action"].(string); ok {
		return v
	}
	return ""
}

func documentCommentEvent(appID string, event *larkdrive.P2NoticeCommentAddV1) (outEvent, bool) {
	if event == nil || event.Event == nil || event.Event.NoticeMeta == nil || event.EventV2Base == nil || event.EventV2Base.Header == nil {
		return outEvent{}, false
	}
	notice := event.Event.NoticeMeta
	if notice.FromUserId == nil || notice.ToUserId == nil {
		return outEvent{}, false
	}
	out := outEvent{
		Type: "document_comment", AppID: appID, EventID: event.EventV2Base.Header.EventID,
		FileToken: larkcore.StringValue(notice.FileToken), FileType: larkcore.StringValue(notice.FileType),
		CommentID: larkcore.StringValue(event.Event.CommentId), ReplyID: larkcore.StringValue(event.Event.ReplyId),
		Sender: larkcore.StringValue(notice.FromUserId.OpenId), Recipient: larkcore.StringValue(notice.ToUserId.OpenId),
	}
	if out.EventID == "" || out.FileToken == "" || out.FileType == "" || out.CommentID == "" || out.ReplyID == "" || out.Sender == "" || out.Recipient == "" {
		return outEvent{}, false
	}
	return out, true
}

func durableCardAction(value map[string]interface{}) bool {
	switch cardActionString(value) {
	case "bridge_mirror", "bridge_handoff", "bridge_context", "bridge_stop", "recall_rpc_handoff", "recover", "dismiss_recovery", "doc_comment_approve", "doc_comment_dismiss", "fork_submit", "fork_choose", "fork_execute":
		return true
	default:
		return false
	}
}

func buildEventDispatcher(app appCfg, sk *sink) *dispatcher.EventDispatcher {
	return buildEventDispatcherWithMergeForward(
		app,
		sk,
		defaultMergeForwardService,
		defaultMergeForwardSlots,
		defaultMergeForwardSpoolStore,
	)
}

func buildEventDispatcherWithMergeForward(
	app appCfg,
	sk *sink,
	fetcher mergeForwardFetcher,
	slots chan struct{},
	spool *mergeForwardSpool,
) *dispatcher.EventDispatcher {
	return dispatcher.NewEventDispatcher("", "").
		OnP2NoticeCommentAddV1(func(ctx context.Context, e *larkdrive.P2NoticeCommentAddV1) error {
			ev, ok := documentCommentEvent(app.AppID, e)
			if !ok {
				return nil
			}
			return sk.writeDurable(ev)
		}).
		OnP2MessageReceiveV1(func(ctx context.Context, e *larkim.P2MessageReceiveV1) error {
			msg := e.Event.Message
			// 入站留痕:默认关,opt-in 时才落一行 jsonl(best-effort),供按真实样本迭代。
			captureInbound(app.AppID, e.Event)
			// 正文 + 附件(图片/文件/语音/视频/富文本内嵌图)一并解析,附件交给 daemon 下载。
			// 正文里的 @ 要归一成 `@_user_N`,所以 mentions 必须先于解析算出来。
			mentions := eventMentions(msg.Mentions)
			text, resources := parseMessageContent(
				larkcore.StringValue(msg.MessageType),
				larkcore.StringValue(msg.Content),
				mentions,
			)
			sender := ""
			senderUnion := ""
			senderType := ""
			if e.Event.Sender != nil {
				// sender_type 挂在 Sender 上、不在 SenderId 里,取的时候别搞混。
				senderType = larkcore.StringValue(e.Event.Sender.SenderType)
				if e.Event.Sender.SenderId != nil {
					sender = larkcore.StringValue(e.Event.Sender.SenderId.OpenId)
					senderUnion = larkcore.StringValue(e.Event.Sender.SenderId.UnionId)
				}
			}
			chatType := larkcore.StringValue(msg.ChatType)
			// 飞书消息 create_time 是毫秒时间戳字符串;解析成 int64 透给 daemon 做入站时效闸。
			// 解析失败/缺失 → 0(daemon 视为未知,跳过时效只去重)。
			createTimeMs := int64(0)
			if v := larkcore.StringValue(msg.CreateTime); v != "" {
				if n, err := strconv.ParseInt(v, 10, 64); err == nil {
					createTimeMs = n
				}
			}
			// 不在此剥 @ 占位:原样把 @_user_N + mentions(含 open_id/name)转给 daemon,
			// 由 daemon 渲染成 @姓名(open_id)。daemon 知道本 bot 身份,且能保留「@了谁」语义。
			// 先完整复制事件字段；merge_forward 的 API 拉取必须脱离 SDK callback ctx，
			// 否则 callback 返回后 ctx 取消会让后台任务稳定失败。
			out := outEvent{
				Type:          "message",
				AppID:         app.AppID,
				ChatID:        larkcore.StringValue(msg.ChatId),
				ChatType:      chatType,
				Sender:        sender,
				SenderUnionID: senderUnion,
				SenderType:    senderType,
				Text:          text,
				MessageID:     larkcore.StringValue(msg.MessageId),
				// 引用回复:飞书把被引用消息的 id 放在 parent_id;普通消息为空。
				ParentID:     larkcore.StringValue(msg.ParentId),
				ThreadID:     larkcore.StringValue(msg.ThreadId),
				RootID:       larkcore.StringValue(msg.RootId),
				Mentions:     mentions,
				Resources:    resources,
				CreateTimeMs: createTimeMs,
			}
			if larkcore.StringValue(msg.MessageType) == "merge_forward" {
				pending, err := spool.begin(out, slots)
				if errors.Is(err, errMergeForwardBusy) {
					out.Text = localized("g08d8564ba1")
					return sk.write(out)
				}
				if err != nil {
					return fmt.Errorf("persist merge_forward before ack: %w", err)
				}
				if pending == nil {
					return nil
				}
				go func(messageID string, pending *pendingMergeForward) {
					defer func() { <-slots }()
					fetchCtx, cancel := context.WithTimeout(context.Background(), mergeForwardTimeout)
					defer cancel()
					mfText, mfRes := fetcher.Fetch(fetchCtx, app, messageID)
					ready, err := pending.enrich(mfText, mfRes)
					if err != nil {
						fmt.Fprintf(os.Stderr, "persist enriched merge_forward %s: %v\n", messageID, err)
						return
					}
					if err := ready.transfer(sk); err != nil {
						fmt.Fprintf(os.Stderr, "transfer merge_forward %s: %v\n", messageID, err)
					}
				}(larkcore.StringValue(msg.MessageId), pending)
				return nil
			}
			// 普通消息快速写回即返回,不阻塞 SDK 的 ack。
			return sk.write(out)
		}).
		OnP2MessageReactionCreatedV1(func(ctx context.Context, e *larkim.P2MessageReactionCreatedV1) error {
			// Reaction events are self-noise for summon; register a no-op so the SDK
			// acknowledges them without logging "not found handler".
			return nil
		}).
		OnP2MessageReactionDeletedV1(func(ctx context.Context, e *larkim.P2MessageReactionDeletedV1) error {
			// Same as created: self-noise; no-op keeps the SDK from logging "not found handler".
			return nil
		}).
		OnP2CardActionTrigger(func(ctx context.Context, e *callback.CardActionTriggerEvent) (*callback.CardActionTriggerResponse, error) {
			var value map[string]interface{}
			chatID := ""
			messageID := ""
			operator := ""
			eventID := ""
			if e.Event != nil {
				if e.Event.Action != nil {
					value = e.Event.Action.Value // 整个按钮 value 透传,daemon 解析
					// 单选框的选择来自 option；归一到模型按钮的提交契约。
					if e.Event.Action.Tag == "select_static" &&
						(cardActionString(value) == "model" || cardActionString(value) == "model_default") {
						value["model"] = e.Event.Action.Option
						delete(value, "inherit")
					}
					// 表单容器提交(v2 form)把已填项放在 form_value,与按钮 value 分开。
					// 并进 value["form_value"],让 daemon 在同一个泛型 map 里拿到(无需改 ipc schema)。
					if e.Event.Action.FormValue != nil {
						if value == nil {
							value = map[string]interface{}{}
						}
						value["form_value"] = e.Event.Action.FormValue
					}
				}
				if e.Event.Context != nil {
					chatID = e.Event.Context.OpenChatID
					messageID = e.Event.Context.OpenMessageID
				}
				if e.Event.Operator != nil {
					operator = e.Event.Operator.OpenID
				}
			}
			if e.EventV2Base != nil && e.EventV2Base.Header != nil {
				eventID = e.EventV2Base.Header.EventID
			}
			// 注意:card.action.trigger 回调的 Operator 只带 open_id,没有 union_id。
			// daemon 侧若名单存的是 union_id,会用 contact API 把 operator open_id 解析成 union_id 再比对。
			event := outEvent{
				Type:           "card_action",
				AppID:          app.AppID,
				ChatID:         chatID,
				MessageID:      messageID,
				OperatorOpenID: operator,
				EventID:        eventID,
				Value:          value,
			}
			var err error
			if durableCardAction(value) {
				err = sk.writeDurable(event)
			} else {
				// Bridge 以外的历史卡片 handler 尚未统一 event_id 幂等；给它们启用崩溃重放会
				// 把 tui_text/gate_ask_submit 等副作用执行两次。维持原 socket 投递语义，不引入回归。
				err = sk.write(event)
			}
			if err != nil {
				return nil, err
			}
			resp := &callback.CardActionTriggerResponse{}
			if content, ok := cardActionToast(cardActionString(value)); ok {
				resp.Toast = &callback.Toast{Type: "success", Content: content}
			}
			return resp, nil
		}).
		// 群被解散:bot 已不在群,daemon 据此拆该 chat 的会话并 p2p 通知群主(operator≈群主)。
		OnP2ChatDisbandedV1(func(ctx context.Context, e *larkim.P2ChatDisbandedV1) error {
			operator := ""
			if e.Event.OperatorId != nil {
				operator = larkcore.StringValue(e.Event.OperatorId.OpenId)
			}
			return sk.write(outEvent{
				Type:           "chat_disbanded",
				AppID:          app.AppID,
				ChatID:         larkcore.StringValue(e.Event.ChatId),
				OperatorOpenID: operator,
			})
		}).
		// bot 被移出某群:同样拆会话并 p2p 通知移除者。
		OnP2ChatMemberBotDeletedV1(func(ctx context.Context, e *larkim.P2ChatMemberBotDeletedV1) error {
			operator := ""
			if e.Event.OperatorId != nil {
				operator = larkcore.StringValue(e.Event.OperatorId.OpenId)
			}
			return sk.write(outEvent{
				Type:           "bot_removed",
				AppID:          app.AppID,
				ChatID:         larkcore.StringValue(e.Event.ChatId),
				OperatorOpenID: operator,
			})
		}).
		// 用户撤回消息:daemon 据此把该 message_id 标记为 recalled,排队中的跳过处理。
		OnP2MessageRecalledV1(func(ctx context.Context, e *larkim.P2MessageRecalledV1) error {
			return sk.write(outEvent{
				Type:      "message_recalled",
				AppID:     app.AppID,
				ChatID:    larkcore.StringValue(e.Event.ChatId),
				MessageID: larkcore.StringValue(e.Event.MessageId),
			})
		})
}

// captured 是 capture-one 模式打到 stdout 的一行 JSON(向导侧解析)。
type captured struct {
	OpenID  string `json:"open_id"`
	UnionID string `json:"union_id,omitempty"`
	ChatID  string `json:"chat_id"`
	Text    string `json:"text"`
}

// captureDispatcher:收到第一条文本消息就把 {open_id,chat_id,text} 写到 out(单行 JSON)并回调 onCaptured。
// reaction 事件仍 no-op(不触发捕获)。抽出来便于单测注入 payload。
func captureDispatcher(out io.Writer, onCaptured func()) *dispatcher.EventDispatcher {
	return dispatcher.NewEventDispatcher("", "").
		OnP2MessageReceiveV1(func(ctx context.Context, e *larkim.P2MessageReceiveV1) error {
			msg := e.Event.Message
			var text string
			if larkcore.StringValue(msg.MessageType) == "text" {
				var c struct {
					Text string `json:"text"`
				}
				_ = json.Unmarshal([]byte(larkcore.StringValue(msg.Content)), &c)
				text = c.Text
			}
			sender := ""
			senderUnion := ""
			if e.Event.Sender != nil && e.Event.Sender.SenderId != nil {
				sender = larkcore.StringValue(e.Event.Sender.SenderId.OpenId)
				senderUnion = larkcore.StringValue(e.Event.Sender.SenderId.UnionId)
			}
			b, _ := json.Marshal(captured{OpenID: sender, UnionID: senderUnion, ChatID: larkcore.StringValue(msg.ChatId), Text: text})
			b = append(b, '\n')
			_, _ = out.Write(b)
			if onCaptured != nil {
				onCaptured()
			}
			return nil
		}).
		OnP2MessageReactionCreatedV1(func(ctx context.Context, e *larkim.P2MessageReactionCreatedV1) error {
			return nil
		}).
		OnP2MessageReactionDeletedV1(func(ctx context.Context, e *larkim.P2MessageReactionDeletedV1) error {
			return nil
		})
}

// captureOne 连第一个 app,收到首条消息打 stdout 后退出 0;启动失败(凭据/网络)退 1。无消息则阻塞,由调用方 kill。
func captureOne(cfg config) int {
	if len(cfg.Apps) == 0 {
		fmt.Fprintln(os.Stderr, localized("gc39dcdbf86"))
		return 1
	}
	app := cfg.Apps[0]
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan struct{})
	var once sync.Once
	handler := captureDispatcher(os.Stdout, func() {
		once.Do(func() { close(done); cancel() })
	})
	opts := []larkws.ClientOption{
		larkws.WithEventHandler(handler),
		larkws.WithLogLevel(larkcore.LogLevelError), // 日志压到 stderr,别污染 stdout 的 JSON 行
	}
	if app.Domain == "larksuite.com" || app.Domain == "open.larksuite.com" {
		opts = append(opts, larkws.WithDomain("https://open.larksuite.com"))
	}
	cli := larkws.NewClient(app.AppID, app.AppSecret, opts...)
	err := cli.Start(ctx) // 阻塞;捕获后 cancel 使其返回
	select {
	case <-done:
		return 0
	default:
		if err != nil {
			fmt.Fprintf(os.Stderr, "capture-one start: %v\n", err)
			return 1
		}
		return 0
	}
}

// 单连接监督的退避阈值。WHY(治本):larkws SDK(v3.9.4)默认 autoReconnect=true 时,连接被
// 1006 踢后每个 receiveMessageLoop goroutine 各自调 reconnect()→connect();connect() 开头
// `if c.conn != nil return` 的去重在锁外读、被并发掏空 → 多 goroutine 各拨一条**真实** WS、
// c.conn 互相覆盖 → 被覆盖的真连接仍活着但句柄丢失、cli.Close() 关不掉 → 同一 app 累积多条连
// 接互相踢飞、入站全丢。**飞书一个 app 只允许一条 WS**,故这是致命的。
// 修法是从根上杜绝并发重连:WithAutoReconnect(false) 关掉 SDK 内部重连,由本进程**串行**地
// 一次只持一条连接、断了才退避重连。永远单连接,飞书不踢,无累积、无泄漏。
const (
	// 重连退避下限。WHY:断开后别立刻重拨给飞书/网络再添堵;3s 起步。
	reconnectBackoffMin = 3 * time.Second
	// 重连退避上限。WHY:封顶 60s,既不无限拉长恢复、又不高频重试。
	reconnectBackoffMax = 60 * time.Second
	// 一条连接稳定挂够这么久,就把退避清回下限。WHY:一次性抖动不该永久抬高退避;连上后
	// 稳态≥120s 视为康复,下次再断从最小退避起步。慢速累积(分钟级)因每条连接都先稳够
	// 120s 再断,故退避不会逐次升级 —— 永远从最小退避快速重连,不退化成长时间不可用。
	reconnectStableReset = 120 * time.Second
)

// nextReconnectBackoff 给定上一条连接稳定挂了多久(connectedFor)与当前退避值,算出下一次
// 重连前的退避。纯函数,便于单测:连接稳够 reconnectStableReset 就回到下限(慢速累积不升级
// 退避);否则翻倍并封顶 max(快速反复断开才指数退避)。
// WHY 用「上条连接活了多久」而非「窗口内事件计数」作判据:真实故障是分钟级慢速累积,任何
// 短窗爆发率阈值都会漏判(且多 app 摊薄更测不到);改看每条连接的存活时长后,无论断开间隔
// 多稀疏,每次断开都会触发一次串行重连,结构上不可能"漏掉一次断开不重连"。
func nextReconnectBackoff(cur, connectedFor time.Duration) time.Duration {
	if connectedFor >= reconnectStableReset {
		return reconnectBackoffMin
	}
	next := cur * 2
	if next < reconnectBackoffMin {
		next = reconnectBackoffMin
	}
	if next > reconnectBackoffMax {
		next = reconnectBackoffMax
	}
	return next
}

// runApp 串行维护单个 app 的唯一一条 WS 连接:关掉 SDK 自动重连,每轮起一个全新 client、
// Start 一条连接;连接断开(onDisconnected)或 Start 自己返回(致命/首连失败)就 Close 收尾、
// 退避后重连。任一时刻该 app 最多一条连接,杜绝并发重连建多条的泄漏(治本)。
func runApp(app appCfg, sk *sink) {
	appID := app.AppID
	backoff := reconnectBackoffMin
	for {
		// disconnected 容量 1:SDK 的 onDisconnected 在连接关闭时回调一次(autoReconnect=false
		// 时 SDK 不会自行重连),我们据此结束本轮、退避后重连。buffered 防回调阻塞在 SDK goroutine。
		disconnected := make(chan struct{}, 1)
		handler := buildEventDispatcher(app, sk)
		opts := []larkws.ClientOption{
			larkws.WithEventHandler(handler),
			// SDK 的 Info 连接日志会打印带临时 access_key/ticket 的完整 WS URL；业务错误已
			// 由 OnError 结构化上报，生产日志只保留 Error，绝不把连接凭据写入 daemon.log。
			larkws.WithLogLevel(larkcore.LogLevelError),
			// 治本开关:关掉 SDK 内部自动重连。否则 1006 后多 goroutine 并发重连建多条真连接、
			// c.conn 互相覆盖、Close 关不掉,累积互踢。关掉后永远单连接,重连由本循环串行驱动。
			larkws.WithAutoReconnect(false),
			// 连接关闭:发一次信号让本轮退避后重连(非阻塞,容量 1 去重)。
			larkws.WithOnDisconnected(func() {
				select {
				case disconnected <- struct{}{}:
				default:
				}
			}),
			larkws.WithOnError(func(err error) {
				if writeErr := sk.write(outEvent{Type: "error", AppID: appID, Detail: err.Error()}); writeErr != nil {
					fmt.Fprintf(os.Stderr, "deliver websocket error for %s: %v\n", appID, writeErr)
				}
			}),
		}
		if app.Domain == "larksuite.com" || app.Domain == "open.larksuite.com" {
			opts = append(opts, larkws.WithDomain("https://open.larksuite.com"))
		}
		cli := larkws.NewClient(app.AppID, app.AppSecret, opts...)

		// Start 首连成功后内部是 select{} 永久阻塞(连接活着就一直 park);放 goroutine 里跑。
		// 它"自己返回"只发生在首连失败 / 凭据致命(autoReconnect=false 时不再内部重连)。
		started := time.Now()
		done := make(chan struct{})
		go func() {
			if err := cli.Start(context.Background()); err != nil {
				if writeErr := sk.write(outEvent{Type: "error", AppID: appID, Detail: err.Error()}); writeErr != nil {
					fmt.Fprintf(os.Stderr, "deliver startup error for %s: %v\n", appID, writeErr)
				}
			}
			close(done)
		}()

		// 等"连接断开"或"Start 自己返回"任一发生。Start 首连成功后永久阻塞,故它返回即从没连上。
		connected := true
		select {
		case <-disconnected:
		case <-done:
			connected = false
		}
		// 收尾:Close 幂等地断开并置 autoReconnect=false(已是 false)。注意 Start 首连成功后的
		// select{} 不会因 Close 返回,那个 goroutine 在连接断开后会因 receiveMessageLoop 退出而
		// 不再持有连接,但 select{} 仍 park(SDK 设计如此,不可达地泄漏一个空 goroutine;每 app
		// 一条连接、仅在断开时新建一轮,频率极低、可接受)。
		cli.Close()

		connectedFor := time.Since(started)
		// 断开时刻是「消息没送达」的排查锚点:这段窗口内飞书推送失败,之后按退避重投。
		// stderr 原样落进 sidecar.log、没有外层时间戳,时间得自己带上。
		now := time.Now().Format("2006/01/02 15:04:05.000000")
		if connected {
			fmt.Fprintf(os.Stderr, localized("re7e6dbe4b0"),
				now, appID, connectedFor.Round(time.Second), backoff)
		} else {
			fmt.Fprintf(os.Stderr, localized("r8adecc4682"),
				now, appID, connectedFor.Round(time.Second), backoff)
		}
		time.Sleep(backoff)
		backoff = nextReconnectBackoff(backoff, connectedFor)
	}
}
