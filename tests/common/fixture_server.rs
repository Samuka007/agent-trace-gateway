//! Fixture upstream speaking real wire protocols, embeddable as a tokio task.
//!   POST /v1/chat       -> non-streaming JSON echo (session_id extraction target)
//!   POST /v1/sse        -> text/event-stream fixed events
//!   GET  /ws            -> WebSocket upgrade scripted agent turn
//!   POST /v1/messages   -> Anthropic Messages protocol (JSON response)
//!   POST /v1/responses  -> OpenAI Responses protocol (SSE stream incl. tool call)
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http_body_util::{combinators::BoxBody, BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use sha1_smol::Sha1;
use std::convert::Infallible;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

type BoxedBody = BoxBody<Bytes, Infallible>;

fn full(s: impl Into<Bytes>) -> BoxedBody {
    Full::new(s.into()).map_err(|never| match never {}).boxed()
}

/// Serve the fixture on the given address until the task is dropped.
pub async fn serve(listen: &str) -> std::io::Result<()> {
    let listener = TcpListener::bind(listen).await?;
    eprintln!("FIXTURE: listening on {listen}");
    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let svc = service_fn(handle);
            if let Err(e) = http1::Builder::new()
                .serve_connection(io, svc)
                .with_upgrades()
                .await
            {
                eprintln!("FIXTURE: connection error: {e}");
            }
        });
    }
}

async fn handle(mut req: Request<Incoming>) -> Result<Response<BoxedBody>, Infallible> {
    match (req.method().as_str(), req.uri().path()) {
        ("POST", "/v1/chat") => {
            let body = req.collect().await.unwrap().to_bytes();
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let session = v.get("session_id").and_then(|x| x.as_str()).unwrap_or("");
            let user_text = v["messages"]
                .as_array()
                .and_then(|m| m.iter().rev().find(|m| m["role"] == "user"))
                .and_then(|m| m["content"].as_str())
                .unwrap_or("");
            let resp = serde_json::json!({
                "id": "chatcmpl-fixture",
                "object": "chat.completion",
                "choices": [{
                    "index": 0,
                    "finish_reason": "stop",
                    "message": {
                        "role": "assistant",
                        "content": format!("echo:{user_text} sess:{session}")
                    }
                }],
                "usage": {"prompt_tokens": 8, "completion_tokens": 2, "total_tokens": 10}
            });
            let mut r = Response::new(full(resp.to_string()));
            r.headers_mut().insert(
                hyper::header::CONTENT_TYPE,
                "application/json".parse().unwrap(),
            );
            Ok(r)
        }
        ("POST", "/v1/messages") => {
            let body = req.collect().await.unwrap().to_bytes();
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let user_text = v["messages"]
                .as_array()
                .and_then(|m| m.last())
                .and_then(|m| m["content"].as_str())
                .unwrap_or("");
            let stream = v.get("stream").and_then(|x| x.as_bool()).unwrap_or(false);
            if stream {
                // Real anthropic tool_use stream: content_block_start opens
                // the block, outer content_block_delta events carry
                // delta.type=input_json_delta fragments, content_block_stop
                // closes it.
                // Real anthropic usage: message_start reports the input side,
                // message_delta the output side.
                let (tx, rx) = tokio::sync::mpsc::channel::<Result<Frame<Bytes>, Infallible>>(8);
                tokio::spawn(async move {
                    let events = [
                        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_usage\",\"type\":\"message\",\"role\":\"assistant\",\"usage\":{\"input_tokens\":12,\"cache_read_input_tokens\":3,\"cache_creation_input_tokens\":4}}}\n\n",
                        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"get_weather\"}}\n\n",
                        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\":\\\"\"}}\n\n",
                        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"Tokyo\\\"}\"}}\n\n",
                        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":7}}\n\n",
                        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
                    ];
                    for ev in events {
                        let _ = tx.send(Ok(Frame::data(Bytes::from(ev)))).await;
                        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
                    }
                });
                let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
                let body = BodyExt::boxed(StreamBody::new(stream));
                let mut r = Response::new(body);
                r.headers_mut().insert(
                    hyper::header::CONTENT_TYPE,
                    "text/event-stream".parse().unwrap(),
                );
                return Ok(r);
            }
            let resp = serde_json::json!({
                "id": "msg_fixture",
                "type": "message",
                "role": "assistant",
                "model": v.get("model").cloned().unwrap_or_default(),
                "content": [{"type": "text", "text": format!("echo:{user_text}")}],
                "stop_reason": "end_turn",
                "usage": {
                    "input_tokens": 5,
                    "output_tokens": 2,
                    "cache_read_input_tokens": 3,
                    "cache_creation_input_tokens": 4
                }
            });
            let mut r = Response::new(full(resp.to_string()));
            r.headers_mut().insert(
                hyper::header::CONTENT_TYPE,
                "application/json".parse().unwrap(),
            );
            Ok(r)
        }
        // G3 probe: a protocol-detected path answering an HTTP-level
        // failure with a JSON error body (anthropic-style) — the turn must
        // export with an error marker, not as a silent success.
        ("POST", "/v1/messages/count_tokens") => {
            let _ = req.collect().await;
            let mut r = Response::new(full(
                r#"{"type":"error","error":{"type":"not_found_error","message":"unknown path"}}"#
                    .to_string(),
            ));
            *r.status_mut() = hyper::StatusCode::NOT_FOUND;
            r.headers_mut().insert(
                hyper::header::CONTENT_TYPE,
                "application/json".parse().unwrap(),
            );
            Ok(r)
        }
        // NIT-B probe: a protocol-detected path failing with a NON-JSON
        // body — the turn must still surface as a minimal errored record.
        ("POST", "/v1/responses/healthz") => {
            let _ = req.collect().await;
            let mut r = Response::new(full("bad gateway".to_string()));
            *r.status_mut() = hyper::StatusCode::BAD_GATEWAY;
            r.headers_mut()
                .insert(hyper::header::CONTENT_TYPE, "text/plain".parse().unwrap());
            Ok(r)
        }
        ("POST", "/v1/responses") => {
            let body = req.collect().await.unwrap().to_bytes();
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let input = v.get("input").and_then(|x| x.as_str()).unwrap_or("");
            let stream = v.get("stream").and_then(|x| x.as_bool()).unwrap_or(false);
            if stream {
                let (tx, rx) = tokio::sync::mpsc::channel::<Result<Frame<Bytes>, Infallible>>(8);
                tokio::spawn(async move {
                    let events = [
                        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"echo-stream:\"}\n\n",
                        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"part2\"}\n\n",
                        "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"id\":\"fc_1\",\"type\":\"function_call\",\"call_id\":\"call_sse_1\",\"name\":\"read_file\",\"arguments\":\"\"}}\n\n",
                        "event: response.function_call_arguments.delta\ndata: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"delta\":\"{\\\"path\\\":\\\"/tmp/x\\\"}\"}\n\n",
                        "event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"id\":\"fc_1\",\"type\":\"function_call\",\"call_id\":\"call_sse_1\",\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"/tmp/x\\\"}\"}}\n\n",
                        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":11,\"output_tokens\":22,\"total_tokens\":33}}}\n\n",
                    ];
                    for ev in events {
                        let _ = tx.send(Ok(Frame::data(Bytes::from(ev)))).await;
                        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
                    }
                });
                let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
                let body = BodyExt::boxed(StreamBody::new(stream));
                let mut r = Response::new(body);
                r.headers_mut().insert(
                    hyper::header::CONTENT_TYPE,
                    "text/event-stream".parse().unwrap(),
                );
                return Ok(r);
            }
            let resp = serde_json::json!({
                "id": "resp_fixture",
                "status": "completed",
                "usage": {"input_tokens": 11, "output_tokens": 22, "total_tokens": 33},
                "output": [{
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": format!("echo:{input}")}]
                }]
            });
            let mut r = Response::new(full(resp.to_string()));
            r.headers_mut().insert(
                hyper::header::CONTENT_TYPE,
                "application/json".parse().unwrap(),
            );
            Ok(r)
        }
        ("POST", "/v1/sse") => {
            let _ = req.collect().await.unwrap().to_bytes();
            let (tx, rx) = tokio::sync::mpsc::channel::<Result<Frame<Bytes>, Infallible>>(8);
            tokio::spawn(async move {
                let events = [
                    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello\"}\n\n",
                    "data: {\"type\":\"response.tool_call\",\"name\":\"read_file\"}\n\n",
                    "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
                ];
                for ev in events {
                    let _ = tx.send(Ok(Frame::data(Bytes::from(ev)))).await;
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            });
            let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
            let body = BodyExt::boxed(StreamBody::new(stream));
            let mut r = Response::new(body);
            *r.status_mut() = StatusCode::OK;
            r.headers_mut().insert(
                hyper::header::CONTENT_TYPE,
                "text/event-stream".parse().unwrap(),
            );
            Ok(r)
        }
        ("GET", "/ws") if is_ws_upgrade(&req) => {
            let accept = ws_accept_key(
                req.headers()
                    .get("sec-websocket-key")
                    .unwrap()
                    .to_str()
                    .unwrap(),
            );
            let mut resp = Response::new(full(""));
            *resp.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
            resp.headers_mut()
                .insert(hyper::header::CONNECTION, "upgrade".parse().unwrap());
            resp.headers_mut()
                .insert(hyper::header::UPGRADE, "websocket".parse().unwrap());
            resp.headers_mut()
                .insert("sec-websocket-accept", accept.parse().unwrap());
            tokio::spawn(async move {
                let upgraded = match hyper::upgrade::on(&mut req).await {
                    Ok(u) => u,
                    Err(e) => {
                        eprintln!("FIXTURE: upgrade failed: {e}");
                        return;
                    }
                };
                // Handshake is already complete (101 sent). The upgraded stream
                // carries raw WebSocket frames; do NOT re-run the handshake.
                let ws = tokio_tungstenite::WebSocketStream::from_raw_socket(
                    TokioIo::new(upgraded),
                    tokio_tungstenite::tungstenite::protocol::Role::Server,
                    None,
                )
                .await;
                run_ws_turn(ws).await;
            });
            Ok(resp)
        }
        _ => {
            let mut r = Response::new(full("not found"));
            *r.status_mut() = StatusCode::NOT_FOUND;
            Ok(r)
        }
    }
}

async fn run_ws_turn(ws: tokio_tungstenite::WebSocketStream<TokioIo<hyper::upgrade::Upgraded>>) {
    let (mut sink, mut stream) = ws.split();
    let first = stream.next().await;
    let Some(Ok(Message::Text(create))) = first else {
        eprintln!("FIXTURE: did not receive response.create");
        return;
    };
    eprintln!("FIXTURE: ws got create: {create}");
    // Real Realtime wire format: complete tool item on output_item.done.
    sink.send(Message::Text(
        "{\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call-1\",\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"/tmp/x\\\"}\"}}".into(),
    ))
    .await
    .unwrap();
    let tool_result = stream.next().await;
    let Some(Ok(Message::Text(result))) = tool_result else {
        eprintln!("FIXTURE: did not receive tool result");
        return;
    };
    eprintln!("FIXTURE: ws got tool result: {result}");
    sink.send(Message::Text(
        "{\"type\":\"response.audio_transcript.delta\",\"delta\":\"file-content\"}".into(),
    ))
    .await
    .unwrap();
    sink.send(Message::Text(
        "{\"type\":\"response.done\",\"response\":{\"status\":\"completed\",\"session_id\":\"sess-ws-1\",\"usage\":{\"input_tokens\":9,\"output_tokens\":3,\"total_tokens\":12,\"input_token_details\":{\"cached_tokens\":5}}}}".into(),
    ))
    .await
    .unwrap();
    let _ = sink.send(Message::Close(None)).await;
}

fn is_ws_upgrade(req: &Request<Incoming>) -> bool {
    req.headers()
        .get(hyper::header::CONNECTION)
        .map(|v| v.to_str().unwrap_or("").to_lowercase().contains("upgrade"))
        .unwrap_or(false)
        && req
            .headers()
            .get(hyper::header::UPGRADE)
            .map(|v| v.to_str().unwrap_or("").eq_ignore_ascii_case("websocket"))
            .unwrap_or(false)
}

fn ws_accept_key(key: &str) -> String {
    let mut s = Sha1::new();
    s.update(key.as_bytes());
    s.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(s.digest().bytes())
}

#[allow(dead_code)]
fn _assert_send() -> SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}
