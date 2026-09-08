// Behavior: protocol detection recognizes every real-world path variant.
// [Requirement: 协议解包与流式重组]
use agent_trace_gateway::trace::unpack::detect_protocol;

#[test]
fn detects_all_real_path_variants() {
    assert_eq!(
        detect_protocol("/v1/chat/completions"),
        Some("openai.chat_completions")
    );
    assert_eq!(
        detect_protocol("/compatible-mode/v1/chat/completions"),
        Some("openai.chat_completions")
    );
    assert_eq!(detect_protocol("/v1/messages"), Some("anthropic.messages"));
    assert_eq!(detect_protocol("/v1/responses"), Some("openai.responses"));
    // Real codex path when pointed at a compatible-mode upstream (RED before fix).
    assert_eq!(
        detect_protocol("/compatible-mode/v1/responses"),
        Some("openai.responses")
    );
    assert_eq!(detect_protocol("/v1/models"), None);
    assert_eq!(detect_protocol("/__atg/records"), None);
}

#[test]
fn extracts_user_input_from_codex_input_array() {
    use agent_trace_gateway::trace::unpack::extract_user_input;
    let body =
        std::fs::read("xtask/harness/fixtures/openai.responses/codex_real_toolturn.json").unwrap();
    let input = extract_user_input("openai.responses", &body);
    assert_eq!(
        input.as_deref(),
        Some("用 shell 工具执行 cat /tmp/codex_target.txt，然后告诉我文件内容"),
        "codex input array must yield the last user message text"
    );
}
