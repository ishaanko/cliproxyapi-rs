// Package zzcapture records translator inputs during go test runs (corpus harvesting only).
package zzcapture

import (
	"encoding/json"
	"fmt"
	"os"
	"sync"
)

var (
	mu   sync.Mutex
	seqs = map[*any]int{}
	next int
)

func write(rec map[string]any) {
	path := os.Getenv("CAPTURE_FILE")
	if path == "" {
		return
	}
	b, _ := json.Marshal(rec)
	f, err := os.OpenFile(path, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644)
	if err != nil {
		return
	}
	_, _ = f.Write(append(b, '\n'))
	_ = f.Close()
}

func Req(dir, fn, model string, body []byte, stream bool) {
	mu.Lock()
	defer mu.Unlock()
	write(map[string]any{"pid": os.Getpid(), "dir": dir, "fn": fn, "model": model, "body": string(body), "stream": stream})
}

func Resp(dir, fn, model string, orig, req, raw []byte, param *any) {
	mu.Lock()
	defer mu.Unlock()
	id, ok := seqs[param]
	if !ok || param == nil || *param == nil {
		next++
		id = next
		if param != nil {
			seqs[param] = id
		}
	}
	write(map[string]any{"pid": os.Getpid(), "dir": dir, "fn": fn, "model": model, "orig": string(orig), "req": string(req), "raw": string(raw), "seq": fmt.Sprint(id)})
}
