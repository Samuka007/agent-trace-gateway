// hoprelay.go — concurrent L4 TCP relay: the extra hop of a multi-hop
// deployment. Std lib only.
//
//	hoprelay -listen 127.0.0.1:6299 -target 127.0.0.1:6199
//
// Why an L4 relay and not caddy: this is the smallest object that reproduces
// the one property the single-hop bench lacks — the client's TCP connection
// TERMINATES at the hop, which opens its OWN connection to ATG. That means
// ATG's accept loop is a real accept, the connection has an extra process
// between it and the client, and the hop process competes with ATG for CPU.
// An HTTP proxy would add parsing semantics that are not the variable.
//
// CONCURRENT by construction (goroutine per connection): a serial relay would
// cap every arm at 1/handshake and reproduce nothing (tools/perf/README.md,
// "the fake upstream must be concurrent" — the same trap).
package main

import (
	"flag"
	"fmt"
	"io"
	"net"
	"sync/atomic"
	"time"
)

func main() {
	listen := flag.String("listen", "127.0.0.1:6299", "listen address")
	target := flag.String("target", "127.0.0.1:6199", "relay target (ATG listen)")
	flag.Parse()

	ln, err := net.Listen("tcp", *listen)
	if err != nil {
		panic(err)
	}
	fmt.Printf("hoprelay on %s -> %s\n", *listen, *target)

	var conns, active atomic.Int64
	for {
		c, err := ln.Accept()
		if err != nil {
			panic(err)
		}
		conns.Add(1)
		active.Add(1)
		go func(client net.Conn) {
			defer active.Add(-1)
			defer client.Close()
			up, err := net.DialTimeout("tcp", *target, 5*time.Second)
			if err != nil {
				return
			}
			defer up.Close()
			done := make(chan struct{}, 2)
			go func() { _, _ = io.Copy(up, client); done <- struct{}{} }()
			go func() { _, _ = io.Copy(client, up); done <- struct{}{} }()
			<-done
			// One direction finished; half-close the other so a streaming
			// response is not truncated by the request side completing.
			if tc, ok := up.(*net.TCPConn); ok {
				_ = tc.CloseWrite()
			}
			if tc, ok := client.(*net.TCPConn); ok {
				_ = tc.CloseWrite()
			}
			<-done
		}(c)
	}
}
