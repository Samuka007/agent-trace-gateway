// ATG#5 load driver: concurrent POSTs against the gateway, measuring
// time-to-first-byte percentiles and achieved rate. Std lib only.
//
//	go build -o drive drive.go
//	drive -mode populate -url http://127.0.0.1:6199/v1/messages -n 50000 -c 32 -distinct 50000
//	drive -mode load     -url http://127.0.0.1:6199/v1/messages -n 20000 -c 64 -distinct 100
package main

import (
	"bytes"
	"flag"
	"fmt"
	"io"
	"net/http"
	"sort"
	"sync"
	"sync/atomic"
	"time"
)

func body(i int) []byte {
	return []byte(fmt.Sprintf(
		`{"model":"m","messages":[{"role":"system","content":"sys-%d"},{"role":"user","content":"user-%d"}]}`,
		i, i))
}

func main() {
	mode := flag.String("mode", "load", "populate | load")
	url := flag.String("url", "http://127.0.0.1:6199/v1/messages", "gateway endpoint")
	n := flag.Int("n", 20000, "total requests")
	c := flag.Int("c", 64, "concurrency")
	distinct := flag.Int("distinct", 100, "distinct conversations; bodies repeat every `distinct`")
	flag.Parse()

	tr := &http.Transport{
		MaxIdleConns:        4096,
		MaxIdleConnsPerHost: 4096,
		DisableKeepAlives:   false,
	}
	client := &http.Client{Transport: tr, Timeout: 60 * time.Second}

	var idx atomic.Int64
	var errs atomic.Int64
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
				req, err := http.NewRequest("POST", *url, bytes.NewReader(body(b)))
				if err != nil {
					errs.Add(1)
					continue
				}
				req.Header.Set("content-type", "application/json")
				req.Header.Set("authorization", "Bearer atg-perf")
				t0 := time.Now()
				resp, err := client.Do(req)
				if err != nil {
					errs.Add(1)
					continue
				}
				buf := make([]byte, 1)
				_, rerr := io.ReadFull(resp.Body, buf)
				el := time.Since(t0).Seconds() * 1000.0
				_, _ = io.Copy(io.Discard, resp.Body)
				resp.Body.Close()
				if rerr != nil || resp.StatusCode != 200 {
					errs.Add(1)
					continue
				}
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
	fmt.Printf(
		"mode=%s n=%d c=%d distinct=%d ok=%d err=%d elapsed=%.2fs achieved_rps=%.1f ttfb_ms p50=%.2f p90=%.2f p99=%.2f max=%.2f\n",
		*mode, *n, *c, *distinct, len(ttfb), errs.Load(),
		elapsed.Seconds(), float64(len(ttfb))/elapsed.Seconds(),
		pct(0.50), pct(0.90), pct(0.99), last)
}
