package main

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// 留痕默认关:不设 SUMMON_SIDECAR_CAPTURE_INBOUND 时,一条入站事件也不该落盘。
func TestCaptureInboundStaysSilentByDefault(t *testing.T) {
	home := t.TempDir()
	t.Setenv("HOME", home)
	t.Setenv(captureInboundEnv, "")

	captureInbound("cli_x", map[string]interface{}{"message": "hi"})

	path := filepath.Join(home, ".summon", "run", "inbound", "messages.jsonl")
	if _, err := os.Stat(path); !os.IsNotExist(err) {
		t.Fatalf("默认不该产生留痕文件: stat err=%v", err)
	}
}

// 显式 opt-in 后才写,且目录 0700 / 文件 0600(内容是飞书消息明文)。
func TestCaptureInboundWritesPrivateFileWhenOptedIn(t *testing.T) {
	home := t.TempDir()
	t.Setenv("HOME", home)
	t.Setenv(captureInboundEnv, "1")

	captureInbound("cli_x", map[string]interface{}{"message": "hi"})

	path := filepath.Join(home, ".summon", "run", "inbound", "messages.jsonl")
	st, err := os.Stat(path)
	if err != nil {
		t.Fatalf("opt-in 后应有留痕文件: %v", err)
	}
	if st.Mode().Perm() != 0o600 {
		t.Fatalf("留痕文件权限应为 0600, got %v", st.Mode().Perm())
	}
	dirSt, err := os.Stat(filepath.Dir(path))
	if err != nil {
		t.Fatalf("stat 目录: %v", err)
	}
	if dirSt.Mode().Perm() != 0o700 {
		t.Fatalf("留痕目录权限应为 0700, got %v", dirSt.Mode().Perm())
	}
	body, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("读留痕: %v", err)
	}
	var got struct {
		AppID string          `json:"app_id"`
		Event json.RawMessage `json:"event"`
	}
	if err := json.Unmarshal(body, &got); err != nil {
		t.Fatalf("留痕行不是合法 json: %v (raw=%q)", err, string(body))
	}
	if got.AppID != "cli_x" || !strings.Contains(string(got.Event), "hi") {
		t.Fatalf("留痕内容错误: %+v", got)
	}
}

// 单代轮转:超上限先把当前文件挪成 .1 再写新行;第二次轮转覆盖旧的 .1,盘上恒为两代。
func TestAppendCaptureLineRotatesSingleGeneration(t *testing.T) {
	path := filepath.Join(t.TempDir(), "inbound", "messages.jsonl")
	maxBytes := int64(10)

	for _, line := range []string{"first\n", "second\n", "third\n"} {
		if err := appendCaptureLine(path, []byte(line), maxBytes); err != nil {
			t.Fatalf("append %q: %v", line, err)
		}
	}

	live, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("读当前代: %v", err)
	}
	if string(live) != "third\n" {
		t.Fatalf("当前代应只剩最后一行, got %q", string(live))
	}
	rotated, err := os.ReadFile(path + ".1")
	if err != nil {
		t.Fatalf("读上一代: %v", err)
	}
	if string(rotated) != "second\n" {
		t.Fatalf("上一代应是被挪走的那代, got %q", string(rotated))
	}
	if _, err := os.Stat(path + ".2"); !os.IsNotExist(err) {
		t.Fatalf("只保留单代,不该有 .2: stat err=%v", err)
	}
}

// 旧版本留下的 0644 文件在续写时要被收紧到 0600(否则历史明文一直是全局可读)。
func TestAppendCaptureLineTightensLegacyPermission(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "messages.jsonl")
	if err := os.WriteFile(path, []byte("legacy\n"), 0o644); err != nil {
		t.Fatalf("造旧文件: %v", err)
	}

	if err := appendCaptureLine(path, []byte("new\n"), 1<<20); err != nil {
		t.Fatalf("append: %v", err)
	}

	st, err := os.Stat(path)
	if err != nil {
		t.Fatalf("stat: %v", err)
	}
	if st.Mode().Perm() != 0o600 {
		t.Fatalf("旧文件权限应被收紧为 0600, got %v", st.Mode().Perm())
	}
}
