use serde::Serialize;
use std::io::{self, Read};

// 定数
const TOLERANCE_MS: i64 = 500;
const START_OFFSET_MIN_MS: i64 = 2000;
const START_OFFSET_MAX_MS: i64 = 8000;
const MIN_BLOCK_DURATION_SEC: f64 = 60.0;
const MAX_BLOCK_DURATION_SEC: f64 = 360.0; // 6分を超えるブロックは異常とみなす
const MIN_STANDARD_UNITS: usize = 2; // ブロックに必要な標準単位の最小数
const MAX_STANDARD_UNITS: i64 = 5; // 標準単位の上限（75秒 = 5 x 15秒）
const STANDARD_UNIT_SEC: f64 = 15.0; // 標準CM単位（秒）
const SHORT_UNITS: [f64; 2] = [5.0, 10.0]; // 短時間CM単位（秒）

// 無音区間を表す構造体（範囲として扱う）
#[derive(Debug, Clone)]
struct SilenceSegment {
    start_ms: i64,
    end_ms: i64,
    duration_ms: i64,
}

// 範囲を表す構造体（境界点の候補範囲）
#[derive(Debug, Clone, Copy)]
struct Range {
    start: i64,
    end: i64,
}

impl Range {
    fn new(start: i64, end: i64) -> Self {
        Range { start, end }
    }

    /// 2つの範囲の交差を計算。交差がなければNone
    fn intersect(&self, other: &Range) -> Option<Range> {
        let start = self.start.max(other.start);
        let end = self.end.min(other.end);
        if start <= end {
            Some(Range::new(start, end))
        } else {
            None
        }
    }

    /// 範囲をオフセットする（prev_range + S で使用）
    fn offset(&self, offset_ms: i64) -> Range {
        Range::new(self.start + offset_ms, self.end + offset_ms)
    }
}

// CM候補区間を表す構造体
#[derive(Debug, Clone, Serialize)]
struct CmCandidate {
    start_ms: i64,
    end_ms: i64,
    duration_sec: f64,
    is_standard: bool, // 標準単位パスでマッチしたか（短時間単位ではない）
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
    start_offset_ms: Option<i64>,
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
    let mut raw_input = Vec::new();
    io::stdin()
        .read_to_end(&mut raw_input)
        .expect("Failed to read from stdin");
    let stdin_data = String::from_utf8_lossy(&raw_input);
    let silence_segments = parse_silence_output(&stdin_data);
    let start_offset_ms = detect_start_offset_ms(&silence_segments);

    eprintln!("Found {} silence segments", silence_segments.len());

    // CMブロックを検出（新アルゴリズム: 範囲ベース境界 + 短時間単位もチェーン継続）
    let mut blocks = detect_blocks_range_based(&silence_segments);
    eprintln!("Detected {} CM blocks (before merge)", blocks.len());

    // 短時間単位による隣接ブロック統合（後処理）
    blocks = merge_blocks_with_short_units(&blocks, &silence_segments);
    eprintln!("After between-block merge: {} CM blocks", blocks.len());

    // CMブロック境界の短時間単位を拡張（後処理）
    blocks = extend_block_boundaries_with_short_units(&blocks, &silence_segments);
    eprintln!("After boundary extension: {} CM blocks", blocks.len());

    // Debug: print pre-filter block statistics
    eprintln!("\n=== Pre-filter block analysis ===");
    eprintln!("{:<5} {:>12} {:>8} {:>10} {:>10}", "Block", "Duration(s)", "StdUnits", "Dur>=60?", "Units>=2?");
    for (i, block) in blocks.iter().enumerate() {
        let std_units = count_standard_units(block);
        let dur_ok = block.duration_sec >= MIN_BLOCK_DURATION_SEC;
        let units_ok = std_units >= MIN_STANDARD_UNITS;
        eprintln!("{:<5} {:>12.1} {:>8} {:>10} {:>10}",
            i + 1,
            block.duration_sec,
            std_units,
            if dur_ok { "YES" } else { "NO" },
            if units_ok { "YES" } else { "NO" }
        );
        // Show segment details for blocks that pass duration but fail units
        if dur_ok && !units_ok {
            eprintln!("  Block {} segments:", i + 1);
            for (j, seg) in block.segments.iter().enumerate() {
                eprintln!("    Seg {}: {:>7.2}s  is_standard={}", j + 1, seg.duration_sec, seg.is_standard);
            }
        }
    }
    eprintln!("=================================\n");

    // 最終フィルタ: 標準単位数と最小時間のチェック（マージ後に実施）
    blocks = filter_blocks_by_standard_units(blocks);
    eprintln!("Final {} CM blocks (after standard unit filter)", blocks.len());

    // JSON出力
    let output = OutputJson {
        input_file: "stdin".to_string(),
        start_offset_ms,
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

/// Check if a string contains only ASCII characters
fn is_ascii_line(line: &str) -> bool {
    line.bytes().all(|b| b.is_ascii())
}

// FFmpeg silencedetect出力から無音区間をパース
fn parse_silence_output(output: &str) -> Vec<SilenceSegment> {
    let mut segments = Vec::new();
    let mut current_start: Option<f64> = None;
    let mut skipped_lines = 0;

    for line in output.lines() {
        // Skip lines containing non-ASCII characters to avoid parsing issues
        if !is_ascii_line(line) {
            skipped_lines += 1;
            continue;
        }

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

    if skipped_lines > 0 {
        eprintln!("Skipped {} lines containing non-ASCII characters", skipped_lines);
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

fn detect_start_offset_ms(silence_segments: &[SilenceSegment]) -> Option<i64> {
    for seg in silence_segments {
        let center_ms = (seg.start_ms + seg.end_ms) / 2;
        if center_ms >= START_OFFSET_MIN_MS && center_ms <= START_OFFSET_MAX_MS {
            return Some(center_ms);
        }
    }
    None
}

/// 粗い標準単位数を決定（gap/15 を四捨五入）
/// 例: 29s → 29/15 = 1.93 → 2単位 → 30s
/// 例: 44s → 44/15 = 2.93 → 3単位 → 45s
/// 90s以上（6単位以上）はNoneを返す（CMとして扱わない）
fn coarse_unit_count(gap_ms: i64) -> Option<i64> {
    let gap_sec = gap_ms as f64 / 1000.0;
    let unit_count = (gap_sec / STANDARD_UNIT_SEC).round() as i64;
    let unit_count = unit_count.max(1); // 最低1単位
    if unit_count > MAX_STANDARD_UNITS {
        None // 75s超過はCMとして扱わない
    } else {
        Some(unit_count)
    }
}

/// 粗い標準単位から期待される間隔（ミリ秒）
/// 90s以上の場合はNoneを返す
fn expected_interval_ms(gap_ms: i64) -> Option<i64> {
    let units = coarse_unit_count(gap_ms)?;
    Some((units as f64 * STANDARD_UNIT_SEC * 1000.0) as i64)
}

/// 短時間単位（5/10秒）かを判定
fn is_short_unit(duration_sec: f64) -> bool {
    let tolerance_sec = TOLERANCE_MS as f64 / 1000.0;
    for unit in SHORT_UNITS {
        if (duration_sec - unit).abs() <= tolerance_sec {
            return true;
        }
    }
    false
}


/// CMブロックを検出（範囲ベースアルゴリズム）
/// 無音区間を範囲 [start, end] として扱い、範囲の交差で境界点を決定
/// 短時間単位（5s/10s）もチェーンに含める（標準単位チェックは後処理で実施）
fn detect_blocks_range_based(silence_segments: &[SilenceSegment]) -> Vec<CmBlock> {
    if silence_segments.len() < 2 {
        return Vec::new();
    }

    let mut blocks = Vec::new();
    // (from_idx, to_idx, is_standard) - is_standard: 標準単位パスでマッチしたか
    let mut chain_segments: Vec<(usize, usize, bool)> = Vec::new();
    let mut prev_range = Range::new(silence_segments[0].start_ms, silence_segments[0].end_ms);

    for i in 1..silence_segments.len() {
        let curr = &silence_segments[i];
        let curr_range = Range::new(curr.start_ms, curr.end_ms);

        // 前後の無音区間の間隔を粗く評価
        let prev_center = (prev_range.start + prev_range.end) / 2;
        let curr_center = (curr_range.start + curr_range.end) / 2;
        let gap_ms = curr_center - prev_center;
        let gap_sec = gap_ms as f64 / 1000.0;

        // 標準単位（15s倍数）または短時間単位（5s/10s）かをチェック
        let expected_ms = match expected_interval_ms(gap_ms) {
            Some(ms) => ms,
            None => {
                // 90s超過 - チェーンを終了して評価
                if let Some(block) = try_make_block_range_based(
                    &chain_segments,
                    silence_segments,
                ) {
                    blocks.push(block);
                }
                chain_segments.clear();
                prev_range = curr_range;
                continue;
            }
        };

        // 期待範囲を計算: prev_range をオフセットして許容範囲を作る
        let expected_range_low = prev_range.offset(expected_ms - TOLERANCE_MS);
        let expected_range_high = prev_range.offset(expected_ms + TOLERANCE_MS);
        let target_range = Range::new(expected_range_low.start, expected_range_high.end);

        // 標準単位での交差を計算
        let standard_match = curr_range.intersect(&target_range);

        // 短時間単位でのマッチもチェック
        let short_unit_match = if standard_match.is_none() && is_short_unit(gap_sec) {
            // 短時間単位の場合、実際のギャップで交差範囲を計算
            let short_expected_ms = (gap_sec * 1000.0).round() as i64;
            let short_range_low = prev_range.offset(short_expected_ms - TOLERANCE_MS);
            let short_range_high = prev_range.offset(short_expected_ms + TOLERANCE_MS);
            let short_target = Range::new(short_range_low.start, short_range_high.end);
            curr_range.intersect(&short_target)
        } else {
            None
        };

        // いずれかでマッチすればチェーン継続
        // standard_match.is_some() なら標準単位パスでマッチ
        let is_standard = standard_match.is_some();
        if let Some(valid_range) = standard_match.or(short_unit_match) {
            // 交差あり - チェーンを継続
            chain_segments.push((i - 1, i, is_standard));

            // 次イテレーションの prev_range は交差範囲
            prev_range = valid_range;
        } else {
            // 交差なし - チェーンを終了して評価
            if let Some(block) = try_make_block_range_based(
                &chain_segments,
                silence_segments,
            ) {
                blocks.push(block);
            }

            // チェーンをリセット
            chain_segments.clear();
            prev_range = curr_range;
        }
    }

    // 最後のチェーンを評価
    if let Some(block) = try_make_block_range_based(
        &chain_segments,
        silence_segments,
    ) {
        blocks.push(block);
    }

    blocks
}

/// チェインからCmBlockを生成（範囲ベース版）
/// 出力点選定: 開始点・終了点 = 無音区間の中心点
/// 注: 標準単位数・最小時間のチェックは後処理（filter_blocks_by_standard_units）で実施
fn try_make_block_range_based(
    chain_segments: &[(usize, usize, bool)], // (from_idx, to_idx, is_standard)
    silence_segments: &[SilenceSegment],
) -> Option<CmBlock> {
    if chain_segments.is_empty() {
        return None;
    }

    // チェーンの最初と最後の無音区間を取得
    let first_pair = chain_segments.first().unwrap();
    let last_pair = chain_segments.last().unwrap();

    let first_silence = &silence_segments[first_pair.0];
    let last_silence = &silence_segments[last_pair.1];

    // 出力点選定:
    // - 開始点 = 最初の無音区間の中心点
    // - 終了点 = 最後の無音区間の中心点
    let start_ms = (first_silence.start_ms + first_silence.end_ms) / 2;
    let end_ms = (last_silence.start_ms + last_silence.end_ms) / 2;

    let total_duration_ms = end_ms - start_ms;
    let total_duration_sec = total_duration_ms as f64 / 1000.0;

    // 360秒以下のサニティチェックのみ（他は後処理で確認）
    if total_duration_sec <= MAX_BLOCK_DURATION_SEC && total_duration_sec > 0.0 {
        // セグメント情報を生成
        let mut segments: Vec<CmCandidate> = Vec::new();
        for (from_idx, to_idx, is_standard) in chain_segments {
            let from_silence = &silence_segments[*from_idx];
            let to_silence = &silence_segments[*to_idx];
            // 各セグメント: from の end から to の start まで
            let seg_start = from_silence.end_ms;
            let seg_end = to_silence.start_ms;
            let duration_sec = (seg_end - seg_start) as f64 / 1000.0;

            segments.push(CmCandidate {
                start_ms: seg_start,
                end_ms: seg_end,
                duration_sec,
                is_standard: *is_standard,
            });
        }

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

/// 短時間単位による隣接ブロック統合（後処理）
/// CMブロック間に短時間単位（5/10秒）が存在する場合、ブロックを統合する
fn merge_blocks_with_short_units(
    blocks: &[CmBlock],
    silence_segments: &[SilenceSegment],
) -> Vec<CmBlock> {
    if blocks.len() < 2 {
        return blocks.to_vec();
    }

    let mut merged: Vec<CmBlock> = Vec::new();
    let mut current_block = blocks[0].clone();

    for i in 1..blocks.len() {
        let next_block = &blocks[i];

        // 現在のブロックと次のブロックの間にある無音区間を探す
        // ブロック間のギャップを計算
        let gap_start = current_block.end_ms;
        let gap_end = next_block.start_ms;

        // ギャップ内の無音区間を見つけて短時間単位チェック
        let can_merge = check_short_units_in_gap(silence_segments, gap_start, gap_end);

        if can_merge {
            // ブロックを統合
            let mut merged_segments = current_block.segments.clone();

            // ギャップ部分をセグメントとして追加（短時間単位なので is_standard: false）
            let gap_duration = (gap_end - gap_start) as f64 / 1000.0;
            merged_segments.push(CmCandidate {
                start_ms: gap_start,
                end_ms: gap_end,
                duration_sec: gap_duration,
                is_standard: false,
            });

            // 次のブロックのセグメントを追加
            merged_segments.extend(next_block.segments.clone());

            let total_duration = (next_block.end_ms - current_block.start_ms) as f64 / 1000.0;

            current_block = CmBlock {
                start_ms: current_block.start_ms,
                end_ms: next_block.end_ms,
                duration_sec: total_duration,
                segments: merged_segments,
            };
        } else {
            // 統合しない - 現在のブロックを確定
            merged.push(current_block);
            current_block = next_block.clone();
        }
    }

    // 最後のブロックを追加
    merged.push(current_block);

    merged
}

/// ギャップ内の無音区間を使って短時間単位チェーンが作れるか確認
fn check_short_units_in_gap(
    silence_segments: &[SilenceSegment],
    gap_start: i64,
    gap_end: i64,
) -> bool {
    // ギャップ内にある無音区間を収集
    let gap_silences: Vec<&SilenceSegment> = silence_segments
        .iter()
        .filter(|s| s.start_ms >= gap_start && s.end_ms <= gap_end)
        .collect();

    if gap_silences.is_empty() {
        // 無音区間がない場合、ギャップ全体が短時間単位かチェック
        let gap_sec = (gap_end - gap_start) as f64 / 1000.0;
        return is_short_unit(gap_sec);
    }

    // 無音区間がある場合、連続する短時間単位でチェーンが作れるか確認
    // 簡略化: ギャップ全体の長さで判定
    let total_gap_sec = (gap_end - gap_start) as f64 / 1000.0;

    // 短時間単位の組み合わせで表現できるかチェック（5秒または10秒の倍数±許容範囲）
    for n in 1..=6 {
        for unit in SHORT_UNITS {
            let expected = unit * n as f64;
            if (total_gap_sec - expected).abs() <= (TOLERANCE_MS as f64 / 1000.0) * n as f64 {
                return true;
            }
        }
    }

    false
}

/// CMブロックの境界にある短時間単位を拡張する（後処理）
/// program → 5s → [CM block] → 5s → program のパターンを検出し、
/// 5s単位をCMブロックに含める
fn extend_block_boundaries_with_short_units(
    blocks: &[CmBlock],
    silence_segments: &[SilenceSegment],
) -> Vec<CmBlock> {
    if blocks.is_empty() || silence_segments.is_empty() {
        return blocks.to_vec();
    }

    blocks
        .iter()
        .map(|block| extend_single_block_boundaries(block, silence_segments))
        .collect()
}

/// 単一ブロックの境界を短時間単位で拡張
fn extend_single_block_boundaries(
    block: &CmBlock,
    silence_segments: &[SilenceSegment],
) -> CmBlock {
    let mut new_start_ms = block.start_ms;
    let mut new_end_ms = block.end_ms;
    let mut prepend_segments: Vec<CmCandidate> = Vec::new();
    let mut append_segments: Vec<CmCandidate> = Vec::new();

    // ブロック開始点に対応する無音区間を探す（中心点 == block.start_ms）
    if let Some(start_idx) = silence_segments
        .iter()
        .position(|s| (s.start_ms + s.end_ms) / 2 == block.start_ms)
    {
        // 前方に短時間単位を探す
        let mut current_idx = start_idx;
        while current_idx > 0 {
            let prev_seg = &silence_segments[current_idx - 1];
            let curr_seg = &silence_segments[current_idx];
            // prev の end から curr の start までのギャップ
            let gap_ms = curr_seg.start_ms - prev_seg.end_ms;
            let gap_sec = gap_ms as f64 / 1000.0;

            if is_short_unit(gap_sec) {
                // 短時間単位を先頭に追加（is_standard: false）
                // セグメントの境界は無音区間の中心点を使用
                let seg_start = (prev_seg.start_ms + prev_seg.end_ms) / 2;
                let seg_end = (curr_seg.start_ms + curr_seg.end_ms) / 2;
                let seg_duration_sec = (seg_end - seg_start) as f64 / 1000.0;
                prepend_segments.insert(
                    0,
                    CmCandidate {
                        start_ms: seg_start,
                        end_ms: seg_end,
                        duration_sec: seg_duration_sec,
                        is_standard: false,
                    },
                );
                new_start_ms = seg_start;
                current_idx -= 1;
            } else {
                break;
            }
        }
    }

    // ブロック終了点に対応する無音区間を探す（中心点 == block.end_ms）
    if let Some(end_idx) = silence_segments
        .iter()
        .position(|s| (s.start_ms + s.end_ms) / 2 == block.end_ms)
    {
        // 後方に短時間単位を探す
        let mut current_idx = end_idx;
        while current_idx + 1 < silence_segments.len() {
            let curr_seg = &silence_segments[current_idx];
            let next_seg = &silence_segments[current_idx + 1];
            // curr の end から next の start までのギャップ
            let gap_ms = next_seg.start_ms - curr_seg.end_ms;
            let gap_sec = gap_ms as f64 / 1000.0;

            if is_short_unit(gap_sec) {
                // 短時間単位を末尾に追加（is_standard: false）
                // セグメントの境界は無音区間の中心点を使用
                let seg_start = (curr_seg.start_ms + curr_seg.end_ms) / 2;
                let seg_end = (next_seg.start_ms + next_seg.end_ms) / 2;
                let seg_duration_sec = (seg_end - seg_start) as f64 / 1000.0;
                append_segments.push(CmCandidate {
                    start_ms: seg_start,
                    end_ms: seg_end,
                    duration_sec: seg_duration_sec,
                    is_standard: false,
                });
                new_end_ms = seg_end;
                current_idx += 1;
            } else {
                break;
            }
        }
    }

    // 拡張がない場合はそのまま返す
    if prepend_segments.is_empty() && append_segments.is_empty() {
        return block.clone();
    }

    // 新しいセグメントリストを構築
    let mut new_segments = prepend_segments;
    new_segments.extend(block.segments.clone());
    new_segments.extend(append_segments);

    let new_duration_sec = (new_end_ms - new_start_ms) as f64 / 1000.0;

    CmBlock {
        start_ms: new_start_ms,
        end_ms: new_end_ms,
        duration_sec: new_duration_sec,
        segments: new_segments,
    }
}

/// ブロック内の標準単位数をカウント（is_standard フラグを使用）
fn count_standard_units(block: &CmBlock) -> usize {
    block.segments.iter().filter(|seg| seg.is_standard).count()
}

/// 最終フィルタ: 標準単位数と最小時間を満たすブロックのみを残す
/// このチェックは全てのマージ・拡張処理後に実行される
fn filter_blocks_by_standard_units(blocks: Vec<CmBlock>) -> Vec<CmBlock> {
    blocks
        .into_iter()
        .filter(|block| {
            let standard_count = count_standard_units(block);
            let meets_duration = block.duration_sec >= MIN_BLOCK_DURATION_SEC;
            let meets_standard_units = standard_count >= MIN_STANDARD_UNITS;

            meets_duration && meets_standard_units
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_start_offset_ms() {
        let segments = vec![
            SilenceSegment { start_ms: 0, end_ms: 400, duration_ms: 400 },
            SilenceSegment { start_ms: 1900, end_ms: 2100, duration_ms: 200 },
            SilenceSegment { start_ms: 9000, end_ms: 9050, duration_ms: 50 },
        ];
        assert_eq!(detect_start_offset_ms(&segments), Some(2000));
    }

    #[test]
    fn test_coarse_unit_count() {
        // 29s → 29/15 = 1.93 → 2 units
        assert_eq!(coarse_unit_count(29000), Some(2));
        // 44s → 44/15 = 2.93 → 3 units
        assert_eq!(coarse_unit_count(44000), Some(3));
        // 59s → 59/15 = 3.93 → 4 units
        assert_eq!(coarse_unit_count(59000), Some(4));
        // 15s → exactly 1 unit
        assert_eq!(coarse_unit_count(15000), Some(1));
        // 30s → exactly 2 units
        assert_eq!(coarse_unit_count(30000), Some(2));
        // 75s → exactly 5 units (max allowed)
        assert_eq!(coarse_unit_count(75000), Some(5));
        // 90s → 6 units → None (exceeds MAX_STANDARD_UNITS)
        assert_eq!(coarse_unit_count(90000), None);
        // 105s → 7 units → None
        assert_eq!(coarse_unit_count(105000), None);
    }

    #[test]
    fn test_range_intersect() {
        let r1 = Range::new(100, 200);
        let r2 = Range::new(150, 250);
        let intersection = r1.intersect(&r2);
        assert!(intersection.is_some());
        let i = intersection.unwrap();
        assert_eq!(i.start, 150);
        assert_eq!(i.end, 200);

        // No intersection
        let r3 = Range::new(300, 400);
        assert!(r1.intersect(&r3).is_none());
    }

    #[test]
    fn test_range_offset() {
        let r = Range::new(100, 200);
        let offset_r = r.offset(15000);
        assert_eq!(offset_r.start, 15100);
        assert_eq!(offset_r.end, 15200);
    }

    /// 41分付近の回帰テスト
    /// 中心点計算では A→B = 15.72s で NG になるが、
    /// 範囲ベースでは末尾を使用して 15.01s になり OK となるべき
    #[test]
    fn test_41min_regression() {
        // 実データに基づくテストケース:
        // 前区間: 2413.700–2415.670 (40:13.70–40:15.67)
        // A: 2443.11–2445.58 (40:43.11–40:45.58)
        // B: 2459.55–2460.59 (40:59.55–41:00.59)
        // C: 2474.56–2475.64 (41:14.56–41:15.64)
        //
        // 中心点計算: A→B = 15.72s (NG)
        // 末尾計算: A end → B end = 15.01s (OK)
        let segments = vec![
            // 開始境界（CMブロック開始前）
            SilenceSegment {
                start_ms: 2383700,
                end_ms: 2385670,
                duration_ms: 1970,
            },
            // 前区間
            SilenceSegment {
                start_ms: 2413700,
                end_ms: 2415670,
                duration_ms: 1970,
            },
            // A
            SilenceSegment {
                start_ms: 2443110,
                end_ms: 2445580,
                duration_ms: 2470,
            },
            // B
            SilenceSegment {
                start_ms: 2459550,
                end_ms: 2460590,
                duration_ms: 1040,
            },
            // C
            SilenceSegment {
                start_ms: 2474560,
                end_ms: 2475640,
                duration_ms: 1080,
            },
            // D (もう一つ追加して4つの15秒単位を確保)
            SilenceSegment {
                start_ms: 2489600,
                end_ms: 2490650,
                duration_ms: 1050,
            },
        ];

        let blocks = detect_blocks_range_based(&segments);

        // 範囲ベースアルゴリズムでは、チェーンが途切れずに検出されるべき
        assert!(!blocks.is_empty(), "Should detect at least one CM block");

        // ブロックが検出された場合、A, B, C, D を含むチェーンがあるべき
        let block = &blocks[0];
        assert!(block.segments.len() >= 4, "Block should have at least 4 segments");
    }

    #[test]
    fn test_basic_cm_block_detection() {
        // 15秒間隔 x 6 = 75秒のCMブロック（出力点選定後も60s以上になるよう調整）
        let segments = vec![
            SilenceSegment { start_ms: 0, end_ms: 1000, duration_ms: 1000 },
            SilenceSegment { start_ms: 14500, end_ms: 15500, duration_ms: 1000 },
            SilenceSegment { start_ms: 29500, end_ms: 30500, duration_ms: 1000 },
            SilenceSegment { start_ms: 44500, end_ms: 45500, duration_ms: 1000 },
            SilenceSegment { start_ms: 59500, end_ms: 60500, duration_ms: 1000 },
            SilenceSegment { start_ms: 74500, end_ms: 75500, duration_ms: 1000 },
        ];

        let blocks = detect_blocks_range_based(&segments);
        assert_eq!(blocks.len(), 1, "Should detect exactly one CM block");

        let block = &blocks[0];
        // 開始点 = 最初の無音区間の中心 = (0 + 1000) / 2 = 500
        assert_eq!(block.start_ms, 500);
        // 終了点 = 最後の無音区間の中心 = (74500 + 75500) / 2 = 75000
        assert_eq!(block.end_ms, 75000);
        // 5 intervals = 5 segments
        assert_eq!(block.segments.len(), 5);
    }

    #[test]
    fn test_output_point_selection() {
        // 出力点選定のテスト:
        // 開始点 = 範囲の中心点
        // 終了点 = 範囲の中心点
        // 6セグメント = 5インターバル = 75秒（出力点選定後も60s以上）
        let segments = vec![
            SilenceSegment { start_ms: 0, end_ms: 2000, duration_ms: 2000 },
            SilenceSegment { start_ms: 14000, end_ms: 16000, duration_ms: 2000 },
            SilenceSegment { start_ms: 28000, end_ms: 32000, duration_ms: 4000 },
            SilenceSegment { start_ms: 43000, end_ms: 47000, duration_ms: 4000 },
            SilenceSegment { start_ms: 58000, end_ms: 62000, duration_ms: 4000 },
            SilenceSegment { start_ms: 73000, end_ms: 77000, duration_ms: 4000 },
        ];

        let blocks = detect_blocks_range_based(&segments);
        assert_eq!(blocks.len(), 1);

        let block = &blocks[0];
        // 開始点 = 最初の無音区間の中心 = (0 + 2000) / 2 = 1000
        assert_eq!(block.start_ms, 1000);
        // 終了点 = 最後の無音区間の中心 = (73000 + 77000) / 2 = 75000
        assert_eq!(block.end_ms, 75000);
    }

    #[test]
    fn test_short_unit_in_chain() {
        // 短時間単位（5s）が15s単位の間に挟まれている場合、1つのチェーンとして検出される
        // 検出はcenter-to-centerのギャップを使用、セグメント長はend-to-start
        // 短い無音区間（100ms）を使用してこの差を最小化
        let segments = vec![
            // 15s間隔 x 5
            SilenceSegment { start_ms: 0, end_ms: 100, duration_ms: 100 },
            SilenceSegment { start_ms: 15000, end_ms: 15100, duration_ms: 100 },   // segment: 14900ms ≈ 15s
            SilenceSegment { start_ms: 30000, end_ms: 30100, duration_ms: 100 },
            SilenceSegment { start_ms: 45000, end_ms: 45100, duration_ms: 100 },
            SilenceSegment { start_ms: 60000, end_ms: 60100, duration_ms: 100 },
            SilenceSegment { start_ms: 75000, end_ms: 75100, duration_ms: 100 },
            // 5s gap
            SilenceSegment { start_ms: 80000, end_ms: 80100, duration_ms: 100 },   // segment: 4900ms ≈ 5s
            // 15s間隔 x 5
            SilenceSegment { start_ms: 95000, end_ms: 95100, duration_ms: 100 },
            SilenceSegment { start_ms: 110000, end_ms: 110100, duration_ms: 100 },
            SilenceSegment { start_ms: 125000, end_ms: 125100, duration_ms: 100 },
            SilenceSegment { start_ms: 140000, end_ms: 140100, duration_ms: 100 },
            SilenceSegment { start_ms: 155000, end_ms: 155100, duration_ms: 100 },
        ];

        let blocks = detect_blocks_range_based(&segments);

        // 短時間単位がチェーンを継続するので、1つのブロックとして検出される
        assert_eq!(blocks.len(), 1, "Short unit should continue chain, resulting in one block");

        let block = &blocks[0];
        // 11 segments total (5 + 1 + 5)
        assert_eq!(block.segments.len(), 11, "Block should have 11 segments");

        // フィルタ後も残る（標準単位 >= 2、時間 >= 60s）
        let filtered = filter_blocks_by_standard_units(vec![block.clone()]);
        assert_eq!(filtered.len(), 1, "Block should pass standard unit filter");
    }

    #[test]
    fn test_is_short_unit() {
        assert!(is_short_unit(5.0));
        assert!(is_short_unit(5.3));
        assert!(is_short_unit(4.7));
        assert!(is_short_unit(10.0));
        assert!(is_short_unit(10.4));
        assert!(!is_short_unit(7.0));
        assert!(!is_short_unit(15.0));
    }

    #[test]
    fn test_no_false_positive_on_irregular_intervals() {
        // 不規則な間隔の無音区間はCMブロックとして検出されるべきではない
        let segments = vec![
            SilenceSegment { start_ms: 0, end_ms: 1000, duration_ms: 1000 },
            SilenceSegment { start_ms: 20000, end_ms: 21000, duration_ms: 1000 },  // 20s gap
            SilenceSegment { start_ms: 55000, end_ms: 56000, duration_ms: 1000 },  // 35s gap
            SilenceSegment { start_ms: 70000, end_ms: 71000, duration_ms: 1000 },  // 15s gap
            SilenceSegment { start_ms: 120000, end_ms: 121000, duration_ms: 1000 }, // 50s gap
        ];

        let blocks = detect_blocks_range_based(&segments);
        // 検出段階では一部のブロックが生成される可能性がある
        // しかし最終フィルタで標準単位数・時間条件を満たさないものは除外される
        let filtered = filter_blocks_by_standard_units(blocks);
        assert!(filtered.is_empty(), "Should not have valid CM blocks after filter");
    }

    #[test]
    fn test_90s_gap_breaks_chain() {
        // 90秒以上のギャップはチェーンを切断すべき
        // Block1: 5 x 15s = 75s, then 90s gap, then Block2: 5 x 15s = 75s
        let segments = vec![
            // Block 1: 15s x 5 = 75s
            SilenceSegment { start_ms: 0, end_ms: 1000, duration_ms: 1000 },
            SilenceSegment { start_ms: 14500, end_ms: 15500, duration_ms: 1000 },
            SilenceSegment { start_ms: 29500, end_ms: 30500, duration_ms: 1000 },
            SilenceSegment { start_ms: 44500, end_ms: 45500, duration_ms: 1000 },
            SilenceSegment { start_ms: 59500, end_ms: 60500, duration_ms: 1000 },
            SilenceSegment { start_ms: 74500, end_ms: 75500, duration_ms: 1000 },
            // 90s gap (center to center = 90s)
            SilenceSegment { start_ms: 164500, end_ms: 165500, duration_ms: 1000 },
            // Block 2: 15s x 5 = 75s
            SilenceSegment { start_ms: 179500, end_ms: 180500, duration_ms: 1000 },
            SilenceSegment { start_ms: 194500, end_ms: 195500, duration_ms: 1000 },
            SilenceSegment { start_ms: 209500, end_ms: 210500, duration_ms: 1000 },
            SilenceSegment { start_ms: 224500, end_ms: 225500, duration_ms: 1000 },
            SilenceSegment { start_ms: 239500, end_ms: 240500, duration_ms: 1000 },
        ];

        let blocks = detect_blocks_range_based(&segments);

        // 90sギャップでチェーンが切断されるので、2つの別々のブロックになるべき
        assert_eq!(blocks.len(), 2, "90s gap should break chain into two blocks");

        // Block 1: ends around 74500
        assert!(blocks[0].end_ms < 80000, "Block 1 should end before the 90s gap");
        // Block 2: starts around 165500
        assert!(blocks[1].start_ms > 160000, "Block 2 should start after the 90s gap");
    }

    #[test]
    fn test_short_units_at_chain_boundaries_merged() {
        // CMチェーンの境界にある短時間単位（5s/10s）は
        // extend_block_boundaries_with_short_unitsによってマージされる
        //
        // 構造: program → 5s → [75s CM block] → 5s → program
        // 期待: 5s単位も含めて拡張されたブロックが検出される
        //
        // チェーン検出は center-to-center ギャップを使用（~6sは短時間単位に該当しない）
        // 境界拡張は edge-to-edge ギャップを使用（5sは短時間単位に該当）
        let segments = vec![
            // Program end / 5s CM start - center = 500
            SilenceSegment { start_ms: 0, end_ms: 1000, duration_ms: 1000 },
            // After 5s CM (boundary of main CM block) - center = 6500
            // center-to-center gap = 6000ms (not short unit)
            // edge-to-edge gap = 5000ms (short unit for extension)
            SilenceSegment { start_ms: 6000, end_ms: 7000, duration_ms: 1000 },
            // 15s x 5 = 75s CM block (5 intervals)
            SilenceSegment { start_ms: 21000, end_ms: 22000, duration_ms: 1000 },
            SilenceSegment { start_ms: 36000, end_ms: 37000, duration_ms: 1000 },
            SilenceSegment { start_ms: 51000, end_ms: 52000, duration_ms: 1000 },
            SilenceSegment { start_ms: 66000, end_ms: 67000, duration_ms: 1000 },
            SilenceSegment { start_ms: 81000, end_ms: 82000, duration_ms: 1000 },
            // After main CM block / 5s CM - center = 87500
            // center-to-center gap = 6000ms (not short unit)
            // edge-to-edge gap = 5000ms (short unit for extension)
            SilenceSegment { start_ms: 87000, end_ms: 88000, duration_ms: 1000 },
            // Program start (after trailing 5s CM)
            SilenceSegment { start_ms: 120000, end_ms: 121000, duration_ms: 1000 },
        ];

        let blocks = detect_blocks_range_based(&segments);
        assert_eq!(blocks.len(), 1, "Should detect one CM block before extension");

        // 境界拡張前のブロック（中心点ベース）
        // チェーンは segment[1] から segment[6] まで
        let block_before = &blocks[0];
        assert_eq!(block_before.start_ms, 6500, "Before extension: starts at center of [6000,7000]");
        assert_eq!(block_before.end_ms, 81500, "Before extension: ends at center of [81000,82000]");

        // 境界拡張を適用
        let extended = extend_block_boundaries_with_short_units(&blocks, &segments);
        assert_eq!(extended.len(), 1, "Should still have one CM block after extension");

        let block = &extended[0];
        // 5s単位が両端に含まれる（中心点ベース）
        // 開始点 = 500 (segment[0]の中心)
        // 終了点 = 87500 (segment[7]の中心)
        assert_eq!(block.start_ms, 500, "Block should include leading 5s unit");
        assert_eq!(block.end_ms, 87500, "Block should include trailing 5s unit");

        // セグメント数: 先頭5s + 5x15s + 末尾5s = 7セグメント
        assert_eq!(block.segments.len(), 7, "Block should have 7 segments (1 + 5 + 1)");

        // 先頭セグメント（中心点間）: 500 → 6500
        assert_eq!(block.segments[0].start_ms, 500);
        assert_eq!(block.segments[0].end_ms, 6500);
        let first_duration = block.segments[0].duration_sec;
        assert!((first_duration - 6.0).abs() < 0.1, "First segment should be ~6s (center to center)");

        // 末尾セグメント（中心点間）: 81500 → 87500
        let last = block.segments.last().unwrap();
        assert_eq!(last.start_ms, 81500);
        assert_eq!(last.end_ms, 87500);
        let last_duration = last.duration_sec;
        assert!((last_duration - 6.0).abs() < 0.1, "Last segment should be ~6s (center to center)");
    }

    #[test]
    fn test_is_standard_flag() {
        // is_standard フラグが正しく設定されることを確認
        // 標準単位パスでマッチしたセグメントは is_standard: true
        // 短時間単位パスでマッチしたセグメントは is_standard: false
        let segments = vec![
            // 15s間隔 x 3
            SilenceSegment { start_ms: 0, end_ms: 100, duration_ms: 100 },
            SilenceSegment { start_ms: 15000, end_ms: 15100, duration_ms: 100 },
            SilenceSegment { start_ms: 30000, end_ms: 30100, duration_ms: 100 },
            SilenceSegment { start_ms: 45000, end_ms: 45100, duration_ms: 100 },
            // 5s gap (短時間単位)
            SilenceSegment { start_ms: 50000, end_ms: 50100, duration_ms: 100 },
            // 15s間隔 x 3
            SilenceSegment { start_ms: 65000, end_ms: 65100, duration_ms: 100 },
            SilenceSegment { start_ms: 80000, end_ms: 80100, duration_ms: 100 },
            SilenceSegment { start_ms: 95000, end_ms: 95100, duration_ms: 100 },
        ];

        let blocks = detect_blocks_range_based(&segments);
        assert_eq!(blocks.len(), 1, "Should detect one CM block");

        let block = &blocks[0];
        // 7 segments: 3 standard + 1 short + 3 standard
        assert_eq!(block.segments.len(), 7, "Block should have 7 segments");

        // 最初の3セグメントは標準単位
        assert!(block.segments[0].is_standard, "Segment 0 should be standard");
        assert!(block.segments[1].is_standard, "Segment 1 should be standard");
        assert!(block.segments[2].is_standard, "Segment 2 should be standard");

        // 4番目のセグメントは短時間単位（5s）
        assert!(!block.segments[3].is_standard, "Segment 3 (5s) should NOT be standard");

        // 残りの3セグメントは標準単位
        assert!(block.segments[4].is_standard, "Segment 4 should be standard");
        assert!(block.segments[5].is_standard, "Segment 5 should be standard");
        assert!(block.segments[6].is_standard, "Segment 6 should be standard");

        // count_standard_units は is_standard フラグを使用
        let std_count = count_standard_units(block);
        assert_eq!(std_count, 6, "Should count 6 standard units (not counting the 5s segment)");
    }

    #[test]
    fn test_extended_segments_are_not_standard() {
        // extend_block_boundaries_with_short_units で追加されたセグメントは is_standard: false
        //
        // チェーン検出は center-to-center ギャップを使用（~6sは短時間単位に該当しない）
        // 境界拡張は edge-to-edge ギャップを使用（5sは短時間単位に該当）
        let segments = vec![
            // Program end / 5s CM start - center = 500
            SilenceSegment { start_ms: 0, end_ms: 1000, duration_ms: 1000 },
            // After 5s CM (boundary of main CM block) - center = 6500
            // center-to-center gap = 6000ms (not short unit for chain detection)
            // edge-to-edge gap = 5000ms (short unit for extension)
            SilenceSegment { start_ms: 6000, end_ms: 7000, duration_ms: 1000 },
            // 15s x 3 = 45s CM block
            SilenceSegment { start_ms: 21000, end_ms: 22000, duration_ms: 1000 },
            SilenceSegment { start_ms: 36000, end_ms: 37000, duration_ms: 1000 },
            SilenceSegment { start_ms: 51000, end_ms: 52000, duration_ms: 1000 },
            // After main CM block / 5s CM - center = 57500
            // center-to-center gap = 6000ms (not short unit for chain detection)
            // edge-to-edge gap = 5000ms (short unit for extension)
            SilenceSegment { start_ms: 57000, end_ms: 58000, duration_ms: 1000 },
            // Program start - far away so no CM chain
            SilenceSegment { start_ms: 100000, end_ms: 101000, duration_ms: 1000 },
        ];

        let blocks = detect_blocks_range_based(&segments);
        let extended = extend_block_boundaries_with_short_units(&blocks, &segments);

        assert_eq!(extended.len(), 1);
        let block = &extended[0];

        // セグメント数: 先頭5s + 3x15s + 末尾5s = 5セグメント
        assert_eq!(block.segments.len(), 5, "Block should have 5 segments");

        // 先頭の拡張セグメント（5s）は is_standard: false
        assert!(!block.segments[0].is_standard, "Extended leading segment should NOT be standard");

        // 中間の15sセグメントは is_standard: true
        assert!(block.segments[1].is_standard, "Segment 1 (15s) should be standard");
        assert!(block.segments[2].is_standard, "Segment 2 (15s) should be standard");
        assert!(block.segments[3].is_standard, "Segment 3 (15s) should be standard");

        // 末尾の拡張セグメント（5s）は is_standard: false
        assert!(!block.segments[4].is_standard, "Extended trailing segment should NOT be standard");

        // count_standard_units は3（15sセグメントのみ）
        let std_count = count_standard_units(block);
        assert_eq!(std_count, 3, "Should count 3 standard units");
    }
}
