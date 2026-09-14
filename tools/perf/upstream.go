// Concurrent fake LLM upstream for the ATG#5 experiment (std lib only).
//
//	go build -o upstream upstream.go
//	upstream -port 19889 -delay-ms 5
package main

import (
	"flag"
	"fmt"
	"net/http"
	"time"
)

func main() {
	port := flag.Int("port", 19889, "listen port")
	delayMs := flag.Int("delay-ms", 5, "per-request delay before responding")
	flag.Parse()

	body := []byte(`{"id":"msg_fake","type":"message","role":"assistant","model":"m",` +
		`"content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn",` +
		`"usage":{"input_tokens":3,"output_tokens":1}}`)

	http.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		if *delayMs > 0 {
			time.Sleep(time.Duration(*delayMs) * time.Millisecond)
		}
		w.Header().Set("content-type", "application/json")
		w.WriteHeader(200)
		_, _ = w.Write(body)
	})
	addr := fmt.Sprintf("127.0.0.1:%d", *port)
	fmt.Printf("fake upstream (concurrent, delay %dms) on http://%s\n", *delayMs, addr)
	if err := http.ListenAndServe(addr, nil); err != nil {
		panic(err)
	}
}
