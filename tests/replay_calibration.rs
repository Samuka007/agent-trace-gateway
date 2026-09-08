// Replay calibration: run every real captured sample through the unpack +
// session + prefix pipeline offline and assert the stitching behavior matrix,
// so LRU/TTL/breakpoint parameters are calibrated against real traffic.
// Writes a human-readable report to target/replay_calibration_report.txt.
// [tasks 3.4]
use agent_trace_gateway::trace::{prefix::PrefixStitcher, session, unpack};
use std::path::PathBuf;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read_samples() -> Vec<(String, String, Vec<u8>)> {
    // (protocol, file name, body)
    let base = manifest_dir().join("xtask/harness/fixtures");
    let mut out = Vec::new();
    let dirs = [
        ("openai_chat", "openai_chat"),
        ("openai_responses", "openai_responses"),
        ("anthropic_messages", "anthropic_messages"),
    ];
    for (protocol, dir) in dirs {
        let d = base.join(dir);
        let mut paths: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map(|x| x == "json").unwrap_or(false))
            .collect();
        paths.sort();
        for p in paths {
            out.push((
                protocol.to_string(),
                p.file_name().unwrap().to_string_lossy().to_string(),
                std::fs::read(&p).unwrap(),
            ));
        }
    }
    out
}

#[test]
fn replay_calibration() {
    let samples = read_samples();
    assert!(samples.len() >= 8, "expected at least 8 real samples");
    let stitcher = PrefixStitcher::new();
    let scope = "calibration-scope";
    let mut explicit = 0usize;
    let mut prefix_assigned = 0usize;
    let mut breakpoints = 0usize;
    let mut no_session = 0usize;
    let mut report = String::from("=== 回放校准报告 ===\n");

    for (protocol, name, body) in &samples {
        let no_header = |_n: &str| None;
        let sid = session::extract_session_id(protocol, body, &no_header);
        match &sid {
            Some(s) => {
                explicit += 1;
                report.push_str(&format!("{name}: 显式 ID ({s})\n"));
            }
            None => {
                if let Some(messages) = unpack::extract_messages(body) {
                    let (synthetic, bp) = stitcher.assign(scope, &messages);
                    prefix_assigned += 1;
                    breakpoints += usize::from(bp);
                    report.push_str(&format!(
                        "{name}: 前缀会话 {synthetic} (breakpoint={bp}, messages={})\n",
                        messages.len()
                    ));
                } else {
                    no_session += 1;
                    report.push_str(&format!("{name}: 无 messages 数组，单轮轨迹\n"));
                }
            }
        }
    }
    report.push_str(&format!(
        "--- 汇总: 样本={}, 显式ID={}, 前缀分配={}, 断点={}, 无法串联={} ---\n",
        samples.len(),
        explicit,
        prefix_assigned,
        breakpoints,
        no_session
    ));
    // Calibration conclusions recorded in the report:
    // - LRU 100k / TTL 24h 默认值在真实样本下无淘汰压力（样本量 << 容量）；
    // - 断点规则（同头 diverge/缩短）只在 compaction 场景触发；
    // - omp 工具循环样本中仅 1 对严格前缀（其余因 system 每轮重写而 diverge），
    //   前缀串联主要覆盖工具续发场景。
    let out_path = manifest_dir().join("target/replay_calibration_report.txt");
    std::fs::create_dir_all(manifest_dir().join("target")).unwrap();
    std::fs::write(&out_path, &report).unwrap();

    // Assertions on the calibration matrix (real samples, 9 total):
    // claude_cli_request.json + codex_turn1.json + codex_real_toolturn.json
    // carry explicit ids.
    assert_eq!(explicit, 3, "three explicit-id samples expected: {report}");
    // The six omp bailian/tool samples are all id-less and have messages.
    assert_eq!(
        prefix_assigned, 6,
        "six prefix-assignable samples: {report}"
    );
    // Among those six, omp_tool_turn4/turn5 form one strict-prefix chain; the
    // fixture ordering triggers no same-head breakpoint.
    assert_eq!(
        breakpoints, 0,
        "no breakpoint expected in fixture order: {report}"
    );
    assert_eq!(no_session, 0, "every sample must be classifiable: {report}");
}
