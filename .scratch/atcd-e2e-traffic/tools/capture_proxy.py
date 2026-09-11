#!/usr/bin/env python3
"""下游 wire 捕获代理：listen 127.0.0.1:LPORT，转发到 127.0.0.1:TPORT（atcd），
把下游方向（agent→atcd）的原始字节按连接落盘到 OUTDIR/connNNN.downstream.raw。

字节级原样捕获（含请求行、头、body、连接复用），事后离线解析成 headers/body。
"""
import os
import socket
import sys
import threading

LPORT = int(sys.argv[1])
TPORT = int(sys.argv[2])
OUTDIR = sys.argv[3]
os.makedirs(OUTDIR, exist_ok=True)
_lock = threading.Lock()
_counter = 0


def pump(src, dst, capture_path):
    f = open(capture_path, "wb") if capture_path else None
    try:
        while True:
            data = src.recv(65536)
            if not data:
                break
            if f:
                f.write(data)
                f.flush()
            dst.sendall(data)
    except OSError:
        pass
    finally:
        if f:
            f.close()
        try:
            dst.shutdown(socket.SHUT_WR)
        except OSError:
            pass


def handle(csock):
    global _counter
    with _lock:
        idx = _counter
        _counter += 1
    cap = os.path.join(OUTDIR, f"conn{idx:03d}.downstream.raw")
    try:
        usock = socket.create_connection(("127.0.0.1", TPORT), timeout=10)
    except OSError:
        csock.close()
        return
    t1 = threading.Thread(target=pump, args=(csock, usock, cap), daemon=True)
    t2 = threading.Thread(target=pump, args=(usock, csock, None), daemon=True)
    t1.start()
    t2.start()
    t1.join()
    t2.join()
    csock.close()
    usock.close()


srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", LPORT))
srv.listen(16)
while True:
    c, _ = srv.accept()
    threading.Thread(target=handle, args=(c,), daemon=True).start()
