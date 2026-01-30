use serde::Serialize;
use std::io::{self, Read};

// 定数
const TOLERANCE_MS: i64 = 500;
const MIN_BLOCK_DURATION_SEC: f64 = 60.0;
const MAX_BLOCK_DURATION_SEC: f64 = 360.0; // 6分を超えるブロックは異常とみなす
const MIN_STANDARD_UNITS: usize = 2; // ブロックに必要な標準単位の最小数

// 無音区間を表す構造体
#[derive(Debug, Clone)]
struct SilenceSegment {
    start_ms: i64,
    end_ms: i64,
    duration_ms: i64,
}

// CM境界候補を表す構造体
#[derive(Debug, Clone)]
struct CmBoundary {
    time_ms: i64,
}

// CM候補区間を表す構造体
#[derive(Debug, Clone, Serialize)]
struct CmCandidate {
    start_ms: i64,
    end_ms: i64,
    duration_sec: f64,
    #[serde(skip)]
    is_standard: bool,
}

// CMブロックを表す構造体
#[derive(Debug, Clone, Serialize)]
struct CmBlock {
    start_ms: i64,
    end_ms: i64,
    duration_sec: f64,
    segments: Vec<CmCandidate>,
}

// JSON出力用の構造体
#[derive(Debug, Serialize)]
struct OutputJson {
    input_file: String,
    cm_blocks: Vec<CmBlock>,
    silence_segments: Vec<SilenceSegmentOutput>,
}

#[derive(Debug, Serialize)]
struct SilenceSegmentOutput {
    start_ms: i64,
    end_ms: i64,
    duration_ms: i64,
}

fn main() {
    // 無音区間を検出（標準入力からffmpeg silencedetectの出力を読み取る）
    eprintln!("Reading silence detection data from stdin...");
    let mut stdin_data = String::new();
    io::stdin()
        .read_to_string(&mut stdin_data)
        .expect("Failed to read from stdin");
    let silence_segments = parse_silence_output(&stdin_data);

    eprintln!("Found {} silence segments", silence_segments.len());

    // CM境界候補を抽出（無音区間の中心点）
    let boundaries = extract_boundaries(&silence_segments);
    eprintln!("Extracted {} CM boundaries", boundaries.len());

    // CMブロックを検出（新アルゴリズム: O(N) 線形チェイン検出）
    let blocks = detect_blocks(&boundaries);
    eprintln!("Detected {} CM blocks", blocks.len());

    // JSON出力
    let output = OutputJson {
        input_file: "stdin".to_string(),
        cm_blocks: blocks,
        silence_segments: silence_segments
            .iter()
            .map(|s| SilenceSegmentOutput {
                start_ms: s.start_ms,
                end_ms: s.end_ms,
                duration_ms: s.duration_ms,
            })
            .collect(),
    };

    let json = serde_json::to_string_pretty(&output).expect("Failed to serialize JSON");
    println!("{}", json);
}

// FFmpeg silencedetect出力から無音区間をパース
fn parse_silence_output(output: &str) -> Vec<SilenceSegment> {
    let mut segments = Vec::new();
    let mut current_start: Option<f64> = None;

    for line in output.lines() {
        if line.contains("silence_start:") {
            if let Some(start) = extract_timestamp(line, "silence_start:") {
                current_start = Some(start);
            }
        } else if line.contains("silence_end:") {
            if let (Some(start), Some(end)) = (current_start, extract_timestamp(line, "silence_end:")) {
                segments.push(SilenceSegment {
                    start_ms: (start * 1000.0) as i64,
                    end_ms: (end * 1000.0) as i64,
                    duration_ms: ((end - start) * 1000.0) as i64,
                });
                current_start = None;
            }
        }
    }

    segments
}

// タイムスタンプを抽出
fn extract_timestamp(line: &str, key: &str) -> Option<f64> {
    line.split(key)
        .nth(1)?
        .split_whitespace()
        .next()?
        .parse::<f64>()
        .ok()
}

// 無音区間の中心点をCM境界候補として抽出
fn extract_boundaries(silence_segments: &[SilenceSegment]) -> Vec<CmBoundary> {
    silence_segments
        .iter()
        .map(|seg| {
            let center_ms = (seg.start_ms + seg.end_ms) / 2;
            CmBoundary { time_ms: center_ms }
        })
        .collect()
}

// 15秒単位（15/30/45/60/75/90秒）かを判定
fn is_standard_unit(duration_sec: f64) -> bool {
    let tolerance_sec = TOLERANCE_MS as f64 / 1000.0;

    for unit in [15.0, 30.0, 45.0, 60.0, 75.0, 90.0] {
        if (duration_sec - unit).abs() <= tolerance_sec {
            return true;
        }
    }
    false
}

// 短時間単位（5/10秒）かを判定
fn is_short_unit(duration_sec: f64) -> bool {
    let tolerance_sec = TOLERANCE_MS as f64 / 1000.0;

    for unit in [5.0, 10.0] {
        if (duration_sec - unit).abs() <= tolerance_sec {
            return true;
        }
    }
    false
}

// CMブロックを検出（新アルゴリズム: O(N) 線形チェイン検出）
// 連続するCM候補のチェインを検出し、条件を満たすものをブロックとする
fn detect_blocks(boundaries: &[CmBoundary]) -> Vec<CmBlock> {
    if boundaries.len() < 2 {
        return Vec::new();
    }

    let mut blocks = Vec::new();
    let mut current_chain: Vec<CmCandidate> = Vec::new();
    let mut standard_count = 0;

    // 境界ペア (i, i+1) を順番にチェック
    for i in 0..boundaries.len() - 1 {
        let interval_ms = boundaries[i + 1].time_ms - boundaries[i].time_ms;
        let interval_sec = interval_ms as f64 / 1000.0;

        let is_standard = is_standard_unit(interval_sec);
        let is_short = is_short_unit(interval_sec);

        if is_standard || is_short {
            // CM候補としてチェインに追加
            let candidate = CmCandidate {
                start_ms: boundaries[i].time_ms,
                end_ms: boundaries[i + 1].time_ms,
                duration_sec: interval_sec,
                is_standard,
            };

            current_chain.push(candidate);

            if is_standard {
                standard_count += 1;
            }
        } else {
            // チェインが途切れた - 評価して必要ならブロック化
            if let Some(block) = try_make_block(&current_chain, standard_count) {
                blocks.push(block);
            }

            // チェインをリセット
            current_chain.clear();
            standard_count = 0;
        }
    }

    // 最後のチェインを評価
    if let Some(block) = try_make_block(&current_chain, standard_count) {
        blocks.push(block);
    }

    blocks
}

// チェインがブロック条件を満たすか評価し、満たす場合はCmBlockを生成
fn try_make_block(chain: &[CmCandidate], standard_count: usize) -> Option<CmBlock> {
    if chain.is_empty() {
        return None;
    }

    let start_ms = chain.first().unwrap().start_ms;
    let end_ms = chain.last().unwrap().end_ms;
    let total_duration_ms = end_ms - start_ms;
    let total_duration_sec = total_duration_ms as f64 / 1000.0;

    // 条件チェック:
    // 1. 合計60秒以上
    // 2. 標準単位が2個以上（短時間単位だけでは不可）
    // 3. 360秒以下（サニティチェック）
    if total_duration_sec >= MIN_BLOCK_DURATION_SEC
        && standard_count >= MIN_STANDARD_UNITS
        && total_duration_sec <= MAX_BLOCK_DURATION_SEC
    {
        // CmBlockを生成（is_standardフィールドは除外してserialize）
        let segments: Vec<CmCandidate> = chain
            .iter()
            .map(|c| CmCandidate {
                start_ms: c.start_ms,
                end_ms: c.end_ms,
                duration_sec: c.duration_sec,
                is_standard: c.is_standard,
            })
            .collect();

        Some(CmBlock {
            start_ms,
            end_ms,
            duration_sec: total_duration_sec,
            segments,
        })
    } else {
        None
    }
}
