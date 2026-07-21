# ffmpeg-aui2-benchmark

AviUtl2入力プラグインDLLを直接ロードし、複数動画のフレーム読み込み時間を計測するWindows向けCLIです。

## 動画の取得

ベンチ用動画はHugging Face Datasetから取得します。

```powershell
uvx --from huggingface_hub huggingface-cli download sevenc-nanashi/ffmpeg.aui2_benchmark_videos --repo-type dataset --local-dir crates/benchmark/videos
```

## 実行

```powershell
cargo run --release -p ffmpeg-aui2-benchmark -- <INPUT_PLUGIN_DLL>
```

既定では30フレームのウォームアップ後、300フレームを8入力の逐次・並列モード、順方向・逆方向で計測します。逆方向では対象範囲の末尾から先頭へ1フレームずつシークします。`FLAG_CONCURRENT`非対応プラグインでは並列モードをスキップします。動画は`videos/manifest.csv`から読み込み、結果は`results/<DLL名>.csv`へ出力します。

ローカルビルドした`ffmpeg_aui2.dll`を直接ロードする場合は、FFmpegの共有DLLを検索できるようにしてください。

```powershell
$env:PATH = (Resolve-Path .\ffmpeg\bin).Path + ";" + $env:PATH
cargo run --release -p ffmpeg-aui2-benchmark -- .\target\release\ffmpeg_aui2.dll
```

主なオプション:

- `--mode sequential|parallel|both`
- `--direction forward|reverse|both`
- `--process-priority idle|below-normal|normal|above-normal|high`（既定: `high`）
- `--thread-priority idle|lowest|below-normal|normal|above-normal|highest`（既定: `highest`）
- `--warmup <FRAMES>`
- `--frames <FRAMES>`
- `--output <CSV>`
- `--videos-dir <DIR>`
- `--video <FILE>`（複数指定するとmanifestを使わない）
- `--verify-frame <FRAME>`（複数指定すると指定順に読み込み、計測外で出力ダイジェストを表示する）

CSVは1回の入力読み込みにつき1行で、`mode,direction,frame,input_index,file,duration_ns,bytes,frame_wall_ns,process_working_set_bytes,process_private_bytes`を記録します。実効FPSは8入力すべての読み込みが完了するまでの`frame_wall_ns`から算出します。サマリーではTukey法により1.5×IQRの範囲外にあるフレーム時間を除外し、除外数を`outliers_removed`として表示します。CSVには除外前の生データを記録します。メモリはプラグインとデコーダーを含むベンチマークプロセス全体をフレーム計測後に取得し、外れ値を除外せず各モードの平均・最大値を表示します。DLLロード、入力オープン、インデックス生成はフレーム計測に含めません。
