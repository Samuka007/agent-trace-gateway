// agent-trace-gateway library: harness modules + gateway app.
pub mod harness;
pub mod trace;

pub mod gateway_app {
    use async_trait::async_trait;
    use bytes::Bytes;
    use pingora::http::ResponseHeader;
    use pingora::prelude::*;
    use pingora::proxy::{http_proxy, FailToProxy, ProxyHttp, Session};
    use pingora::upstreams::peer::HttpPeer;

    use crate::trace::session;
    use crate::trace::store::TraceStore;
    use crate::trace::unpack;

    pub struct Gateway {
        pub upstream: String,
        pub store: TraceStore,
        pub stitcher: crate::trace::prefix::PrefixStitcher,
        pub cap: crate::trace::capture::CaptureCap,
        pub exporter: crate::trace::export::Exporter,
    }

    impl Gateway {
        fn push_record(&self, record: crate::trace::store::TurnRecord) {
            self.store.push(record.clone());
            self.exporter.submit(&record);
        }
    }

    fn now_ns() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    }

    pub struct Ctx {
        pub req_buf: Vec<u8>,
        pub resp_buf: Vec<u8>,
        pub resp_content_type: String,
        pub ws_client_parser: crate::trace::ws::WsFrameParser,
        pub ws_server_parser: crate::trace::ws::WsFrameParser,
        pub ws_turn: crate::trace::ws::WsTurnState,
        /// Turn timing (unix nanoseconds). start set at request start,
        /// end set when the record is finalized.
        pub start_ns: u64,
        pub end_ns: u64,
    }

    #[async_trait]
    impl ProxyHttp for Gateway {
        type CTX = Ctx;

        fn new_ctx(&self) -> Self::CTX {
            Ctx {
                req_buf: Vec::new(),
                resp_buf: Vec::new(),
                resp_content_type: String::new(),
                ws_client_parser: crate::trace::ws::WsFrameParser::new(true),
                ws_server_parser: crate::trace::ws::WsFrameParser::new(false),
                ws_turn: crate::trace::ws::WsTurnState::default(),
                start_ns: now_ns(),
                end_ns: 0,
            }
        }

        async fn upstream_peer(
            &self,
            _session: &mut Session,
            _ctx: &mut Self::CTX,
        ) -> Result<Box<HttpPeer>> {
            let tls = self.upstream.starts_with("https://");
            let host = self
                .upstream
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .to_string();
            // ATG_SNI overrides SNI so an upstream given as a bare IP can still
            // complete TLS with the correct hostname.
            let sni = std::env::var("ATG_SNI")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| host.split(':').next().unwrap_or("").to_string());
            Ok(Box::new(HttpPeer::new(host, tls, sni)))
        }

        // Rewrite the Host header so the real upstream routes the request
        // correctly. Uses ATG_SNI when set, else the upstream host.
        async fn upstream_request_filter(
            &self,
            _session: &mut Session,
            upstream_request: &mut pingora::http::RequestHeader,
            _ctx: &mut Self::CTX,
        ) -> Result<()> {
            let default_host = self
                .upstream
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .to_string();
            let host = std::env::var("ATG_SNI")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or(default_host);
            let _ = upstream_request.insert_header(http::header::HOST, host);
            Ok(())
        }

        // Control endpoint: dump collected turn records as JSON.
        async fn request_filter(
            &self,
            session: &mut Session,
            _ctx: &mut Self::CTX,
        ) -> Result<bool> {
            if session.req_header().uri.path() == "/__atg/records" {
                let records = self.store.snapshot();
                let body = serde_json::to_vec(&records).unwrap_or_default();
                let mut resp = ResponseHeader::build(200, None)?;
                resp.insert_header("content-type", "application/json")?;
                resp.insert_header("content-length", body.len().to_string())?;
                session.write_response_header(Box::new(resp), false).await?;
                session
                    .write_response_body(Some(Bytes::from(body)), true)
                    .await?;
                return Ok(true);
            }
            if session.req_header().uri.path() == "/__atg/health" {
                let (exported, failed, dropped) = self.exporter.health.snapshot();
                let body = serde_json::json!({
                    "exported": exported,
                    "failed": failed,
                    "dropped": dropped
                })
                .to_string();
                let mut resp = ResponseHeader::build(200, None)?;
                resp.insert_header("content-type", "application/json")?;
                resp.insert_header("content-length", body.len().to_string())?;
                session.write_response_header(Box::new(resp), false).await?;
                session
                    .write_response_body(Some(Bytes::from(body)), true)
                    .await?;
                return Ok(true);
            }
            Ok(false)
        }

        async fn request_body_filter(
            &self,
            session: &mut Session,
            body: &mut Option<Bytes>,
            _end: bool,
            ctx: &mut Self::CTX,
        ) -> Result<()> {
            if session.was_upgraded() {
                if let Some(b) = body {
                    for payload in ctx.ws_client_parser.push(b) {
                        ctx.ws_turn.apply_client_frame(&payload);
                    }
                }
            } else if let Some(b) = body {
                ctx.req_buf.extend_from_slice(b);
            }
            Ok(())
        }

        async fn upstream_response_filter(
            &self,
            _session: &mut Session,
            resp: &mut pingora::http::ResponseHeader,
            ctx: &mut Self::CTX,
        ) -> Result<()> {
            if let Some(v) = resp.headers.get(http::header::CONTENT_TYPE) {
                ctx.resp_content_type = v.to_str().unwrap_or("").to_string();
            }
            Ok(())
        }

        fn response_body_filter(
            &self,
            session: &mut Session,
            body: &mut Option<Bytes>,
            _end: bool,
            ctx: &mut Self::CTX,
        ) -> Result<Option<std::time::Duration>> {
            if session.was_upgraded() {
                if let Some(b) = body {
                    for payload in ctx.ws_server_parser.push(b) {
                        if let Some(mut record) = ctx.ws_turn.apply_server_frame(&payload) {
                            ctx.end_ns = now_ns();
                            record.start_ns = ctx.start_ns;
                            record.end_ns = ctx.end_ns;
                            self.push_record(record);
                        }
                    }
                }
            } else if let Some(b) = body {
                ctx.resp_buf.extend_from_slice(b);
            }
            Ok(None)
        }

        async fn logging(&self, session: &mut Session, _e: Option<&Error>, ctx: &mut Self::CTX) {
            let path = session.req_header().uri.path();
            let Some(protocol) = unpack::detect_protocol(path) else {
                return;
            };
            let header_get = |name: &str| -> Option<String> {
                session
                    .req_header()
                    .headers
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
            };
            let mut session_id = session::extract_session_id(protocol, &ctx.req_buf, &header_get)
                .unwrap_or_default();
            let mut breakpoint = false;
            if session_id.is_empty() {
                if let Some(messages) = unpack::extract_messages(&ctx.req_buf) {
                    let scope = header_get("authorization").unwrap_or_default();
                    let (synthetic, is_bp) = self.stitcher.assign(&scope, &messages);
                    session_id = synthetic;
                    breakpoint = is_bp;
                }
            }
            let raw_request = self.cap.bound(&ctx.req_buf);
            let raw_response = self.cap.bound(&ctx.resp_buf);
            if unpack::looks_like_sse(&ctx.resp_content_type) {
                let (final_output, usage) =
                    unpack::reassemble_sse_output(protocol, &ctx.resp_buf);
                let user_input =
                    unpack::extract_user_input(protocol, &ctx.req_buf).unwrap_or_default();
                ctx.end_ns = now_ns();
                self.push_record(crate::trace::store::TurnRecord {
                    protocol: protocol.to_string(),
                    session_id,
                    user_input,
                    final_output,
                    raw_request,
                    raw_response,
                    tool_calls: unpack::extract_sse_tool_calls(protocol, &ctx.resp_buf),
                    breakpoint,
                    start_ns: ctx.start_ns,
                    end_ns: ctx.end_ns,
                    usage,
                });
                return;
            }
            if let Some(mut record) =
                unpack::unpack_nonstreaming(protocol, &ctx.req_buf, &ctx.resp_buf)
            {
                record.session_id = session_id;
                record.breakpoint = breakpoint;
                record.raw_request = raw_request;
                record.raw_response = raw_response;
                ctx.end_ns = now_ns();
                record.start_ns = ctx.start_ns;
                record.end_ns = ctx.end_ns;
                self.push_record(record);
            }
        }

        fn fail_to_connect(
            &self,
            session: &mut Session,
            peer: &HttpPeer,
            _ctx: &mut Self::CTX,
            e: Box<Error>,
        ) -> Box<Error> {
            let path = session.req_header().uri.path().to_string();
            let peer = format!("{peer:?}");
            let err = format!("{e:?}");
            eprintln!("GATEWAY fail_to_connect: path={path} peer={peer} error={err}");
            e
        }

        async fn fail_to_proxy(
            &self,
            session: &mut Session,
            e: &Error,
            _ctx: &mut Self::CTX,
        ) -> FailToProxy {
            let path = session.req_header().uri.path().to_string();
            let err = format!("{e:?}");
            eprintln!("GATEWAY fail_to_proxy: path={path} error={err}");
            let code = match e.etype() {
                pingora::HTTPStatus(code) => *code,
                _ => match e.esource() {
                    pingora::ErrorSource::Upstream => 502,
                    pingora::ErrorSource::Downstream => match e.etype() {
                        pingora::WriteError | pingora::ReadError | pingora::ConnectionClosed => 0,
                        _ => 400,
                    },
                    _ => 500,
                },
            };
            if code > 0 {
                session.respond_error(code).await.unwrap_or_else(|e| {
                    eprintln!("failed to send error response to downstream: {e}");
                });
            }
            FailToProxy {
                error_code: code,
                can_reuse_downstream: false,
            }
        }
    }

    /// Start the gateway on `listen`, forwarding to `upstream`. Blocks.
    /// `upstream` accepts "host:port", "http://host:port" or "https://host:port".
    pub fn run(listen: &str, upstream: &str) {
        let mut server = Server::new(Some(Opt::default())).unwrap();
        server.bootstrap();
        let gateway = Gateway {
            upstream: upstream.to_string(),
            store: TraceStore::new(),
            stitcher: crate::trace::prefix::PrefixStitcher::new(),
            cap: crate::trace::capture::CaptureCap::new(),
            exporter: crate::trace::export::Exporter::start(
                std::env::var("ATG_OTLP_ENDPOINT").ok(),
            ),
        };
        let mut http_proxy = http_proxy(&server.configuration, gateway);
        let mut opts = pingora::apps::HttpServerOptions::default();
        opts.h2c = true;
        http_proxy.server_options = Some(opts);
        let mut svc = pingora::services::listening::Service::new(
            "agent-trace-gateway".to_string(),
            http_proxy,
        );
        svc.add_tcp(listen);
        server.add_service(svc);
        server.run_forever();
    }
}
