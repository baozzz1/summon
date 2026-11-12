package main

import (
	"os"
	"path/filepath"
	"regexp"
	"slices"
	"syscall"
	"testing"
	"time"
	"unicode"
)

func TestLocaleCatalogsPreserveKeysAndFormatArguments(t *testing.T) {
	pattern := regexp.MustCompile(`%[sdv]`)
	if len(localeMessages["zh-CN"]) != len(localeMessages["en"]) {
		t.Fatal("key sets differ")
	}
	for key, chinese := range localeMessages["zh-CN"] {
		english, ok := localeMessages["en"][key]
		if !ok || english == "" {
			t.Fatalf("missing English message %s", key)
		}
		if !slices.Equal(pattern.FindAllString(chinese, -1), pattern.FindAllString(english, -1)) {
			t.Fatalf("format arguments differ for %s", key)
		}
		for _, char := range english {
			if unicode.Is(unicode.Han, char) {
				t.Fatalf("untranslated English message %s", key)
			}
		}
	}
}

func TestDisplayLanguageCacheChangesWithoutRestart(t *testing.T) {
	path := filepath.Join(t.TempDir(), "display-language")
	for _, language := range []string{"en", "zh-CN", "en"} {
		if err := os.WriteFile(path, []byte(language), 0600); err != nil {
			t.Fatal(err)
		}
		if got := cachedLanguage(path); got != language {
			t.Fatalf("got %q, want %q", got, language)
		}
	}
	for _, value := range []string{"fr", "enxxxxxxxxxxxxxxxxxxxxxxxxxxx"} {
		if err := os.WriteFile(path, []byte(value), 0600); err != nil {
			t.Fatal(err)
		}
		if got := cachedLanguage(path); got != "" {
			t.Fatalf("accepted %q", got)
		}
	}
}

func TestDisplayLanguageCacheRejectsLinksDirectoriesAndFifos(t *testing.T) {
	dir := t.TempDir()
	if got := cachedLanguage(dir); got != "" {
		t.Fatal("accepted directory")
	}
	target := filepath.Join(dir, "target")
	if err := os.WriteFile(target, []byte("en"), 0600); err != nil {
		t.Fatal(err)
	}
	link := filepath.Join(dir, "link")
	if err := os.Symlink(target, link); err != nil {
		t.Fatal(err)
	}
	if got := cachedLanguage(link); got != "" {
		t.Fatal("accepted symlink")
	}
	fifo := filepath.Join(dir, "fifo")
	if err := syscall.Mkfifo(fifo, 0600); err != nil {
		t.Fatal(err)
	}
	start := time.Now()
	if got := cachedLanguage(fifo); got != "" || time.Since(start) > time.Second {
		t.Fatal("FIFO did not fail promptly")
	}
}
