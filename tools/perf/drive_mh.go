// drive_mh.go — multi-hop deployment driver (ATG#1 verification).
//
//	go build -o drive_mh drive_mh.go
//	drive_mh -mode populate -url http://127.0.0.1:6299/v1/messages -n 50000 -c 32
//	drive_mh -mode load     -url http://127.0.0.1:6299/v1/messages -n 3000 -c 64 -distinct 100
//
// Two differences from tools/perf/drive.go, both required by ATG#1:
//
//  1. The request carries a MULTI-MESSAGE agent history. The single-hop rig
//     sent one message, which is below the stitcher's >=2-message gate
//     (src/lib.rs:1513): the stitcher never ran, so that rig could not see
//     the stitcher's serial point at all. Production traffic is multi-turn.
//  2. TTFB is measured at the CLIENT, through the hop proxy, and reported at
//     p50/p90/p99 — the deployment-level number ATG#1 is about.
//
// Payload shape is production-like: agent history of ~`-history-bytes` on the
// request side, and the load body is stable per conversation index so the
// stitcher's live-chain extension path (not the divergent-head path) runs.
package main

import (
	"bytes"
	"flag"
	"fmt"
	"io"
	"net/http"
	"sort"
	"strings"
	"sync"
	"sync/atomic"
	"time"
)

// body builds a request whose `messages` array has `msgs` entries and whose
// total size is about `historyBytes`. Entries repeat a fixed template with
// only a per-request marker differing, so consecutive turns of the same
// conversation form a strict prefix extension (the stitcher's steady state).
func body(conv int, msgs int, historyBytes int) []byte {
	per := historyBytes / max(1, msgs)
	if per < 32 {
		per = 32
	}
	filler := strings.Repeat("x", per)
	var b strings.Builder
	b.WriteString(`{"model":"claude-fake-b","max_tokens":8192,"stream":true,"messages":[`)
	for i := range msgs {
		if i > 0 {
			b.WriteByte(',')
		}
		role := "user"
		if i%2 == 1 {
			role = "assistant"
		}
		// The conversation marker sits on the FIRST message only: every
		// turn of conversation `conv` shares that head, which is what makes
		// the stitcher see one chain per conversation.
		if i == 0 {
			fmt.Fprintf(&b, `{"role":"%s","content":"conv-%d %s"}`, role, conv, filler)
		} else {
			fmt.Fprintf(&b, `{"role":"%s","content":"%s"}`, role, filler)
		}
	}
	b.WriteString(`]}`)
	return []byte(b.String())
}

func max(a, b int) int {
	if a > b {
		return a
	}
	return b
}

func main() {
	mode := flag.String("mode", "load", "populate | load")
	url := flag.String("url", "http://127.0.0.1:6299/v1/messages", "gateway endpoint (through the hop)")
	n := flag.Int("n", 3000, "total requests")
	c := flag.Int("c", 64, "concurrency")
	distinct := flag.Int("distinct", 100, "distinct conversations; bodies repeat every `distinct`")
	msgs := flag.Int("msgs", 8, "messages per request (>=2: the stitcher gate)")
	historyBytes := flag.Int("history-bytes", 2048, "approximate request-side history size")
	flag.Parse()

	tr := &http.Transport{
		MaxIdleConns:        4096,
		MaxIdleConnsPerHost: 4096,
		DisableKeepAlives:   false,
	}
	client := &http.Client{Transport: tr, Timeout: 120 * time.Second}

	var idx atomic.Int64
	var errs atomic.Int64
	var bytesTotal atomic.Int64
	var mu sync.Mutex
	ttfb := make([]float64, 0, *n)
	var wg sync.WaitGroup
	start := time.Now()

	for range *c {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for {
				i := int(idx.Add(1)) - 1
				if i >= *n {
					return
				}
				b := i
				if *distinct > 0 && *mode == "load" {
					b = i % *distinct
				}
				// populate: one distinct conversation per request (builds
				// the stitcher's chain table to size n).
				req, err := http.NewRequest("POST", *url, bytes.NewReader(body(b, *msgs, *historyBytes)))
				if err != nil {
					errs.Add(1)
					continue
				}
				req.Header.Set("content-type", "application/json")
				req.Header.Set("authorization", "Bearer atg-mh")
				t0 := time.Now()
				resp, err := client.Do(req)
				if err != nil {
					errs.Add(1)
					continue
				}
				buf := make([]byte, 1)
				_, rerr := io.ReadFull(resp.Body, buf)
				el := time.Since(t0).Seconds() * 1000.0
				wrote, _ := io.Copy(io.Discard, resp.Body)
				resp.Body.Close()
				if rerr != nil || resp.StatusCode != 200 {
					errs.Add(1)
					continue
				}
				bytesTotal.Add(wrote + 1)
				mu.Lock()
				ttfb = append(ttfb, el)
				mu.Unlock()
			}
		}()
	}
	wg.Wait()
	elapsed := time.Since(start)

	sort.Float64s(ttfb)
	pct := func(p float64) float64 {
		if len(ttfb) == 0 {
			return 0
		}
		k := int(float64(len(ttfb)-1) * p)
		return ttfb[k]
	}
	last := 0.0
	if len(ttfb) > 0 {
		last = ttfb[len(ttfb)-1]
	}
	mb := float64(bytesTotal.Load()) / (1 << 20)
	fmt.Printf(
		"mode=%s n=%d c=%d distinct=%d msgs=%d histB=%d ok=%d err=%d elapsed=%.2fs achieved_rps=%.1f ttfb_ms p50=%.2f p90=%.2f p99=%.2f max=%.2f MB=%.1f MBps=%.2f\n",
		*mode, *n, *c, *distinct, *msgs, *historyBytes, len(ttfb), errs.Load(),
		elapsed.Seconds(), float64(len(ttfb))/elapsed.Seconds(),
		pct(0.50), pct(0.90), pct(0.99), last, mb, mb/elapsed.Seconds())
}
