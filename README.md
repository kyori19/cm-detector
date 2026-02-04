# cm-detector

日本のテレビ放送のCM区間を検出するツール。ffmpegのsilencedetect出力を解析し、15秒/30秒単位のCMブロックを特定します。

## ビルド方法

### Dockerを使用（推奨）

```bash
docker build -t cm-detector .
```

### ローカルビルド

```bash
# musl ターゲットを追加（静的リンク用）
rustup target add x86_64-unknown-linux-musl

# ビルド
cargo build --release --target x86_64-unknown-linux-musl
```

## 使用方法

cm-detectorはffmpegのsilencedetect出力を標準入力から受け取ります。

### 基本的な使い方

```bash
ffmpeg -i video.mp4 -af "silencedetect=n=-40dB:d=0.3" -f null - 2>&1 | cm-detector
```

### Dockerを使用

```bash
ffmpeg -i video.mp4 -af "silencedetect=n=-40dB:d=0.3" -f null - 2>&1 | \
  docker run -i --rm cm-detector
```

### 出力例

```json
{
  "input_file": "stdin",
  "cm_blocks": [
    {
      "start_ms": 120000,
      "end_ms": 180000,
      "duration_sec": 60.0,
      "segments": [
        {"start_ms": 120000, "end_ms": 135000, "duration_sec": 15.0},
        {"start_ms": 135000, "end_ms": 165000, "duration_sec": 30.0},
        {"start_ms": 165000, "end_ms": 180000, "duration_sec": 15.0}
      ]
    }
  ],
  "silence_segments": [...]
}
```

JSON 出力には `start_offset_ms` も含まれます。これは録画の先頭から本編開始までのオフセットで、一般に2〜8秒程度になり、最初に検出された無音区間の中心点を返します。

## Kubernetes init container での使用例

cm-detectorをinit containerとして使用し、バイナリを共有ボリュームにコピーする例：

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: video-processor
spec:
  initContainers:
    - name: install-cm-detector
      image: ghcr.io/aitokis/cm-detector:latest
      command: ["/bin/cp", "/cm-detector", "/tools/cm-detector"]
      volumeMounts:
        - name: tools
          mountPath: /tools
  containers:
    - name: processor
      image: linuxserver/ffmpeg
      command:
        - /bin/sh
        - -c
        - |
          ffmpeg -i /input/video.mp4 -af "silencedetect=n=-40dB:d=0.3" -f null - 2>&1 | \
          /tools/cm-detector > /output/cm-blocks.json
      volumeMounts:
        - name: tools
          mountPath: /tools
        - name: input
          mountPath: /input
        - name: output
          mountPath: /output
  volumes:
    - name: tools
      emptyDir: {}
    - name: input
      # 入力ソースを指定
    - name: output
      # 出力先を指定
```

## CMカット動画のエンコード

cm-detectorの出力を使って、CM区間をカットした動画をエンコードできます。

### 本編区間の取得

JSON出力から本編区間（CMブロック以外の部分）を計算します。

```bash
# CM検出結果を保存
ffmpeg -i video.mp4 -af "silencedetect=n=-40dB:d=0.3" -f null - 2>&1 | cm-detector > cm.json

# 本編区間を確認（jqで抽出）
# cm_blocksの前後が本編区間になる
cat cm.json | jq '.cm_blocks[] | "\(.start_ms/1000) - \(.end_ms/1000)"'
```

### 単一の本編区間を切り出す

```bash
# 開始から最初のCMまで（例：0秒〜120秒が本編の場合）
ffmpeg -i video.mp4 -ss 0 -to 120 -c:v libx264 -c:a aac output_part1.mp4

# CM後の本編（例：180秒〜600秒が本編の場合）
ffmpeg -i video.mp4 -ss 180 -to 600 -c:v libx264 -c:a aac output_part2.mp4
```

### 複数の本編区間を連結してエンコード

複数の本編区間をCMをスキップして1つの動画にまとめる方法：

#### 方法1: concat demuxer（推奨・高速）

```bash
# 1. 本編区間を個別ファイルに切り出し（再エンコードなし）
ffmpeg -i video.mp4 -ss 0 -to 120 -c copy part1.mp4
ffmpeg -i video.mp4 -ss 180 -to 600 -c copy part2.mp4
ffmpeg -i video.mp4 -ss 660 -to 1500 -c copy part3.mp4

# 2. 連結リストを作成
cat > concat.txt << 'EOF'
file 'part1.mp4'
file 'part2.mp4'
file 'part3.mp4'
EOF

# 3. 連結してエンコード
ffmpeg -f concat -safe 0 -i concat.txt -c:v libx264 -c:a aac output.mp4

# 4. 一時ファイル削除
rm part1.mp4 part2.mp4 part3.mp4 concat.txt
```

#### 方法2: filter_complex（1コマンドで完結）

```bash
# 3つの本編区間を連結する例
ffmpeg -i video.mp4 -filter_complex \
  "[0:v]trim=start=0:end=120,setpts=PTS-STARTPTS[v0]; \
   [0:a]atrim=start=0:end=120,asetpts=PTS-STARTPTS[a0]; \
   [0:v]trim=start=180:end=600,setpts=PTS-STARTPTS[v1]; \
   [0:a]atrim=start=180:end=600,asetpts=PTS-STARTPTS[a1]; \
   [0:v]trim=start=660:end=1500,setpts=PTS-STARTPTS[v2]; \
   [0:a]atrim=start=660:end=1500,asetpts=PTS-STARTPTS[a2]; \
   [v0][a0][v1][a1][v2][a2]concat=n=3:v=1:a=1[outv][outa]" \
  -map "[outv]" -map "[outa]" -c:v libx264 -c:a aac output.mp4
```

### シェルスクリプト例：自動CMカット

JSON出力の `start_offset_ms` は本編開始位置の推定値であり、トリミングの基準点として使用します。この値から処理を開始することで、録画先頭の不要部分を除去できます。

```bash
#!/bin/bash
# cm-cut.sh - CM検出結果から自動的にCMカット動画を生成

INPUT="$1"
OUTPUT="${2:-output.mp4}"

# CM検出
CM_JSON=$(ffmpeg -i "$INPUT" -af "silencedetect=n=-40dB:d=0.3" -f null - 2>&1 | cm-detector)

# 動画の長さを取得（ミリ秒）
DURATION_MS=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$INPUT" | awk '{printf "%.0f", $1 * 1000}')

# start_offset_msを取得し、非負整数にクランプ
START_OFFSET=$(echo "$CM_JSON" | jq -r '.start_offset_ms // 0')
START_OFFSET=${START_OFFSET%.*}  # 小数点以下を削除
START_OFFSET=$((START_OFFSET < 0 ? 0 : START_OFFSET))

# 本編区間を計算してfilter_complexを構築
FILTER=""
CONCAT_INPUTS=""
IDX=0
PREV_END=$START_OFFSET

# CMブロックの開始・終了時刻を処理
while IFS= read -r line; do
  START_MS=$(echo "$line" | jq -r '.start_ms')
  END_MS=$(echo "$line" | jq -r '.end_ms')

  if [ "$PREV_END" -lt "$START_MS" ]; then
    START_SEC=$(echo "scale=3; $PREV_END / 1000" | bc)
    END_SEC=$(echo "scale=3; $START_MS / 1000" | bc)
    FILTER+="[0:v]trim=start=${START_SEC}:end=${END_SEC},setpts=PTS-STARTPTS[v${IDX}];"
    FILTER+="[0:a]atrim=start=${START_SEC}:end=${END_SEC},asetpts=PTS-STARTPTS[a${IDX}];"
    CONCAT_INPUTS+="[v${IDX}][a${IDX}]"
    ((IDX++))
  fi
  PREV_END=$END_MS
done < <(echo "$CM_JSON" | jq -c '.cm_blocks[]')

# 最後のCM以降の本編
if [ "$PREV_END" -lt "$DURATION_MS" ]; then
  START_SEC=$(echo "scale=3; $PREV_END / 1000" | bc)
  END_SEC=$(echo "scale=3; $DURATION_MS / 1000" | bc)
  FILTER+="[0:v]trim=start=${START_SEC}:end=${END_SEC},setpts=PTS-STARTPTS[v${IDX}];"
  FILTER+="[0:a]atrim=start=${START_SEC}:end=${END_SEC},asetpts=PTS-STARTPTS[a${IDX}];"
  CONCAT_INPUTS+="[v${IDX}][a${IDX}]"
  ((IDX++))
fi

FILTER+="${CONCAT_INPUTS}concat=n=${IDX}:v=1:a=1[outv][outa]"

ffmpeg -i "$INPUT" -filter_complex "$FILTER" \
  -map "[outv]" -map "[outa]" -c:v libx264 -c:a aac "$OUTPUT"
```

使用方法：
```bash
chmod +x cm-cut.sh
./cm-cut.sh input.mp4 output_no_cm.mp4
```

## 検出アルゴリズム

### 範囲ベース境界検出

1. ffmpegのsilencedetect出力から無音区間を範囲 `[start, end]` として抽出
2. 隣接する無音区間の間隔を評価し、以下のいずれかでチェインを継続：
   - **標準単位**: 15秒倍数（15/30/45/60/75秒 ±0.5秒）
   - **短時間単位**: 5秒または10秒（±0.5秒）
3. 90秒以上の間隔、または上記に該当しない間隔でチェインを切断

### 出力点選定

CMブロックの開始・終了位置は無音区間の中心点を使用：
- **開始点**: 最初の無音区間の中心点
- **終了点**: 最後の無音区間の中心点

### 短時間単位による後処理

検出後、以下の後処理で短時間単位（5秒/10秒）をさらに統合：

1. **ブロック間統合**: 隣接するCMブロック間に短時間単位のギャップがある場合、1つのブロックに統合
2. **境界拡張**: CMブロックの前後に短時間単位が隣接している場合、ブロックを拡張して含める

#### 例

```
入力:  program → 15s → 5s → 15s → 5s → 15s → program
検出:  1つのチェーンとして検出（短時間単位もチェーン継続）
出力:  program → [    65s CM block    ] → program
```

### 最終フィルタ（マージ後に適用）

全てのマージ・拡張処理後、以下の条件を満たすブロックのみを出力：
- 合計60秒以上
- 標準単位（15秒倍数）が2個以上
- 合計360秒以下

この順序により、短時間単位で分断されていても最終的に条件を満たせばCMとして検出される。

### 標準単位の上限

90秒以上（6単位以上）の間隔はCMとして扱わず、チェインを切断する（最大75秒 = 5単位）

## ライセンス

MIT
