package main

import (
	"io"
	"os"
	"strings"
	"sync/atomic"
	"syscall"
)

type localeInput struct{ fallback, file string }

var localeSource atomic.Pointer[localeInput]

func configureLocale(language, file string) {
	if language != "en" {
		language = "zh-CN"
	}
	localeSource.Store(&localeInput{language, file})
}

func cachedLanguage(path string) string {
	fd, err := syscall.Open(path, syscall.O_RDONLY|syscall.O_NONBLOCK|syscall.O_NOFOLLOW, 0)
	if err != nil {
		return ""
	}
	file := os.NewFile(uintptr(fd), path)
	defer file.Close()
	info, err := file.Stat()
	if err != nil || !info.Mode().IsRegular() || info.Size() > 16 {
		return ""
	}
	bytes, err := io.ReadAll(io.LimitReader(file, 16))
	if err != nil {
		return ""
	}
	language := strings.TrimSpace(string(bytes))
	if language == "en" || language == "zh-CN" {
		return language
	}
	return ""
}

func currentLanguage() string {
	source := localeSource.Load()
	if source == nil {
		return "zh-CN"
	}
	if source.file != "" {
		if language := cachedLanguage(source.file); language != "" {
			return language
		}
	}
	return source.fallback
}

func localizedIn(language, key string) string {
	if value, ok := localeMessages[language][key]; ok {
		return value
	}
	if value, ok := localeMessages["en"][key]; ok {
		return value
	}
	return "[" + key + "]"
}

func localized(key string) string { return localizedIn(currentLanguage(), key) }
