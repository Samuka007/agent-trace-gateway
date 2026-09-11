#!/usr/bin/env python3
"""R11 离线分析：解析 downstream raw，与 upstream mock 落盘配对，产出差异分析 JSON。

- 下游 wire（agent→atcd）按连接解析出 HTTP 请求（请求行/头序/body）
- 上游 wire = mock 落盘（headers.json + body.bin）
- 配对：按 content-plane 摘要（model+instructions+input 规范化）匹配，兜底按序
- 输出：每 agent 的 evidence/<agent>/analysis.json + 终端摘要
"""
import hashlib
import json
import os
import sys

EV = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "evidence")


def parse_downstream(path):
    """从 raw 连接字节里解析出全部 HTTP 请求。"""
    data = open(path, "rb").read()
    reqs = []
    pos = 0
    while pos < len(data):
        head_end = data.find(b"\r\n\r\n", pos)
        if head_end < 0:
            break
        head = data[pos:head_end].decode("iso-8859-1")
        lines = head.split("\r\n")
        request_line = lines[0]
        headers = []
        for ln in lines[1:]:
            if ":" in ln:
                k, v = ln.split(":", 1)
                headers.append((k.strip(), v.strip()))
        hl = {k.lower(): v for k, v in headers}
        n = int(hl.get("content-length", "0"))
        body = data[head_end + 4 : head_end + 4 + n]
        reqs.append(
            {
                "request_line": request_line,
                "headers": headers,
                "body": body,
                "consumed": head_end + 4 + n,
            }
        )
        pos = head_end + 4 + n
    return reqs


def content_digest(body_bytes):
    """内容平面摘要：instructions+input 的规范化 JSON（身份字段剔除）。"""
    try:
        b = json.loads(body_bytes)
    except Exception:
        return "unparseable:" + hashlib.sha256(body_bytes).hexdigest()[:12]
    plane = {k: b.get(k) for k in ("model", "instructions", "input", "tools", "tool_choice")}
    blob = json.dumps(plane, sort_keys=True, ensure_ascii=False).encode()
    return hashlib.sha256(blob).hexdigest()[:16]


def load_upstream(agent):
    d = os.path.join(EV, agent, "upstream")
    out = []
    for fn in sorted(os.listdir(d)):
        if fn.endswith(".upstream.headers.json"):
            tag = fn.split(".")[0]
            rec = json.load(open(os.path.join(d, fn)))
            body = open(os.path.join(d, tag + ".upstream.body.bin"), "rb").read()
            out.append({"tag": tag, **rec, "body": body})
    return out


def load_downstream(agent):
    d = os.path.join(EV, agent, "downstream")
    out = []
    for fn in sorted(os.listdir(d)):
        if fn.endswith(".downstream.raw"):
            reqs = parse_downstream(os.path.join(d, fn))
            for i, r in enumerate(reqs):
                out.append({"conn": fn[:7], "idx": i, **r})
    return out


def header_map(headers):
    if isinstance(headers, dict):
        return {k.lower(): v for k, v in headers.items()}
    return {k.lower(): v for k, v in headers}


def diff_headers(down_headers, up_headers):
    d = header_map(down_headers)
    u = header_map(up_headers)
    keys = sorted(set(d) | set(u))
    changed, same = [], []
    for k in keys:
        if k in d and k not in u:
            changed.append({"header": k, "change": "removed-upstream", "downstream": d[k]})
        elif k not in d and k in u:
            changed.append({"header": k, "change": "added-upstream", "upstream": u[k]})
        elif d[k] != u[k]:
            changed.append({"header": k, "change": "replaced", "downstream": d[k], "upstream": u[k]})
        else:
            same.append(k)
    return {"changed_or_added_or_removed": changed, "kept_identical": same}


def deep_body_diff(down_body, up_body):
    d = json.loads(down_body)
    u = json.loads(up_body)
    keys = sorted(set(d) | set(u))
    result = {}
    identity_plane = {}
    for k in keys:
        if k not in d:
            result[k] = {"verdict": "added-upstream"}
            identity_plane[k] = u.get(k)
        elif k not in u:
            result[k] = {"verdict": "dropped-upstream", "downstream": _summ(d[k])}
        else:
            if d[k] == u[k]:
                result[k] = {"verdict": "byte-identical"}
            else:
                result[k] = {"verdict": "changed", "downstream": _summ(d[k]), "upstream": _summ(u[k])}
                identity_plane[k] = {"downstream": d[k], "upstream": u[k]}
    # 内容平面逐字节判定（json 语义等价 + 序列化等价两种口径都给）
    content = {}
    for k in ("model", "instructions", "input", "tools", "tool_choice", "parallel_tool_calls",
              "store", "stream", "include", "reasoning", "text", "max_output_tokens"):
        if k in d and k in u:
            content[k] = d[k] == u[k]
    return {"per_key": result, "content_plane_equal": content,
            "identity_plane_changed": identity_plane}


def _summ(v):
    s = json.dumps(v, ensure_ascii=False)
    if len(s) > 220:
        s = s[:200] + f"...({len(s)} chars)"
    return s


def analyze(agent):
    ups = load_upstream(agent)
    downs = [r for r in load_downstream(agent) if r["body"]]
    pairs, unmatched = [], []
    up_pool = list(ups)
    for dn in downs:
        dig = content_digest(dn["body"])
        m = next((u for u in up_pool if content_digest(u["body"]) == dig), None)
        if m is None and up_pool:
            m = up_pool[0]
        if m is None:
            unmatched.append(dn)
            continue
        up_pool.remove(m)
        pairs.append((dn, m))
    report = {"agent": agent, "pairs": [], "unmatched_downstream": [r["conn"] for r in unmatched],
              "upstream_without_pair": [u["tag"] for u in up_pool]}
    for dn, up in pairs:
        h = diff_headers(dn["headers"], up["headers"])
        b = deep_body_diff(dn["body"], up["body"])
        report["pairs"].append({
            "downstream": {"conn": dn["conn"], "request_line": dn["request_line"],
                            "bytes": len(dn["body"]), "headers": dict(dn["headers"]),
                            "body_json": json.loads(dn["body"])},
            "upstream": {"tag": up["tag"], "request_line": up["request_line"],
                          "bytes": len(up["body"]), "headers": up["headers"],
                          "body_json": json.loads(up["body"])},
            "header_diff": h,
            "body_diff": b,
            "content_digest_match": content_digest(dn["body"]) == content_digest(up["body"]),
            "prompt_cache_key": {"bare_downstream": json.loads(dn["body"]).get("prompt_cache_key"),
                                   "rewritten_upstream": json.loads(up["body"]).get("prompt_cache_key")},
        })
    out = os.path.join(EV, agent, "analysis.json")
    with open(out, "w") as f:
        json.dump(report, f, indent=1, ensure_ascii=False)
    return report


if __name__ == "__main__":
    for agent in sys.argv[1:]:
        rep = analyze(agent)
        print(f"════ {agent}: {len(rep['pairs'])} pair(s), unmatched_down={rep['unmatched_downstream']}, orphan_up={rep['upstream_without_pair']}")
        for p in rep["pairs"]:
            pck = p["prompt_cache_key"]
            print(f"  {p['downstream']['conn']} ↔ {p['upstream']['tag']} content_digest_match={p['content_digest_match']} pck bare={pck['bare_downstream']} up={pck['rewritten_upstream']}")
            for c in p["header_diff"]["changed_or_added_or_removed"]:
                print(f"    H {c['change']:<16} {c['header']}: down={str(c.get('downstream'))[:80]} up={str(c.get('upstream'))[:80]}")
