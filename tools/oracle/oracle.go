// Command oracle exposes the Go translators over a JSONL stdin/stdout protocol so the
// Rust port can be differentially tested against the reference implementation.
//
// Input lines (one case per line):
//
//	{"kind":"request","client":"openai","upstream":"claude","model":"m","stream":true,"body":"<raw json>"}
//	{"kind":"stream","client":"openai","upstream":"claude","model":"m","original":"<raw>","translated":"<raw>","lines":["data: ...", ...]}
//	{"kind":"nonstream","client":"openai","upstream":"claude","model":"m","original":"<raw>","translated":"<raw>","body":"<raw>"}
//	{"kind":"token_count","client":"claude","upstream":"gemini","count":12}
//
// Output: one JSON line per case: {"out": ...} or {"error": "..."}.
// request/nonstream/token_count -> string|null; stream -> [[string,...] per input line].
package main

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
	"os"

	_ "github.com/router-for-me/CLIProxyAPI/v8/internal/translator"
	sdktranslator "github.com/router-for-me/CLIProxyAPI/v8/sdk/translator"
)

type tcase struct {
	Kind       string   `json:"kind"`
	Client     string   `json:"client"`
	Upstream   string   `json:"upstream"`
	Model      string   `json:"model"`
	Stream     bool     `json:"stream"`
	Body       string   `json:"body"`
	Original   string   `json:"original"`
	Translated string   `json:"translated"`
	Lines      []string `json:"lines"`
	Count      int64    `json:"count"`
}

func run(c tcase) (out any, err error) {
	defer func() {
		if r := recover(); r != nil {
			err = fmt.Errorf("panic: %v", r)
		}
	}()
	ctx := context.Background()
	client, upstream := sdktranslator.Format(c.Client), sdktranslator.Format(c.Upstream)
	switch c.Kind {
	case "request":
		return string(sdktranslator.TranslateRequest(client, upstream, c.Model, []byte(c.Body), c.Stream)), nil
	case "stream":
		var param any
		res := make([][]string, 0, len(c.Lines))
		for _, line := range c.Lines {
			chunks := sdktranslator.TranslateStream(ctx, upstream, client, c.Model, []byte(c.Original), []byte(c.Translated), []byte(line), &param)
			strs := make([]string, 0, len(chunks))
			for _, ch := range chunks {
				strs = append(strs, string(ch))
			}
			res = append(res, strs)
		}
		return res, nil
	case "nonstream":
		var param any
		b := sdktranslator.TranslateNonStream(ctx, upstream, client, c.Model, []byte(c.Original), []byte(c.Translated), []byte(c.Body), &param)
		if b == nil {
			return nil, nil
		}
		return string(b), nil
	case "token_count":
		return string(sdktranslator.TranslateTokenCount(ctx, upstream, client, c.Count, nil)), nil
	}
	return nil, fmt.Errorf("unknown kind %q", c.Kind)
}

func main() {
	in := bufio.NewScanner(os.Stdin)
	in.Buffer(make([]byte, 64<<20), 64<<20)
	w := bufio.NewWriter(os.Stdout)
	enc := json.NewEncoder(w)
	enc.SetEscapeHTML(false)
	for in.Scan() {
		var c tcase
		if err := json.Unmarshal(in.Bytes(), &c); err != nil {
			_ = enc.Encode(map[string]any{"error": "bad case: " + err.Error()})
			continue
		}
		out, err := run(c)
		if err != nil {
			_ = enc.Encode(map[string]any{"error": err.Error()})
		} else {
			_ = enc.Encode(map[string]any{"out": out})
		}
		_ = w.Flush()
	}
}
