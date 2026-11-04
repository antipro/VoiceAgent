# 小蒙同学 · 树莓派离线语音助手

树莓派 4 上的中文语音助手：**本地 ASR（whisper.cpp）+ 本地 TTS（Piper）+ DeepSeek 大模型 + 可选联网搜索**。
Rust + Tokio 实现，蓝牙麦克风/音箱走 ALSA / cpal，对话记录落 SQLite。

```text
蓝牙麦克风 ──cpal──▶ 16kHz 单声道 ──VAD 切句──▶ whisper.cpp ──文本──┐
                                                                  │
                        ┌─────────────────────────────────────────┘
                        ▼
                 FSM（Idle / Active / Busy）◀── 唤醒词随时打断
                        │
                        ▼
                  DeepSeek（tool_calls）──▶ web_search（Tavily / SearXNG）
                        │
                        ▼
                 Piper TTS ──WAV──▶ cpal ──▶ 蓝牙音箱
```

---

## 1. 已实现的能力

| 设计规格 | 实现方式 |
| --- | --- |
| 唤醒词「小蒙同学」 | VAD 切句 + whisper 转写 + 归一化匹配（繁体转简体、同音容错：小萌 / 小明 / 小梦） |
| Active 10 秒窗口 | `tokio::time` 截止时间；问候语播放耗时不计入窗口 |
| 随时打断 | 唤醒代际（`watch<u64>`）广播，LLM 请求、TTS 合成、音频播放都会被立即取消 |
| Tool Calling 闭环 | 原生 OpenAI 兼容实现，最多 3 轮，搜索失败会回传给模型而不是崩溃 |
| SQLite 会话记录 | 表结构见第 8 节，使用 WAL 模式，阻塞调用走 `spawn_blocking` |
| 交叉编译 | 提供 `Cross.toml` 与 `scripts/build-pi.sh`，但**推荐在 Pi 上原生编译**（见第 6 节） |

**两个刻意的工程取舍**（与原始规格的差异）：

1. **ASR/TTS 以子进程方式调用**，而不是 FFI 链接。
   - 好处：`cargo build` 不需要交叉编译 C++ 依赖，模型/引擎可独立升级，崩溃隔离。
   - 代价：每次识别有一次进程启动开销（树莓派 4 上 tiny/base 模型约 0.1~0.3s）。
   - whisper.cpp / piper 本身就是独立可执行文件，这样部署最省心。
2. **唤醒检测走「切句 → 转写 → 文本匹配」**，而不是专用唤醒词模型。
   - 唤醒延迟约 1~2 秒（说完「小蒙同学」后要等 600ms 静音判定）。
   - 好处是零额外依赖，且同一套 ASR 同时服务唤醒与对话。
   - 如果对延迟敏感，后续可把 `asr` 换成 openWakeWord/Porcupine，FSM 不用改。

---

## 2. 目录结构

```text
src/
  main.rs          启动、装配、信号处理
  lib.rs           库入口（供测试与二次开发）
  config.rs        TOML + 环境变量配置，密钥脱敏
  fsm.rs           状态机与打断逻辑
  agent.rs         多轮对话 + 工具调用闭环
  llm.rs           OpenAI 兼容客户端（DeepSeek）
  search.rs        Tavily / SearXNG
  memory.rs        SQLite 持久化
  util.rs          文本清洗、唤醒词匹配、Markdown 去除
  audio/
    capture.rs     cpal 麦克风采集线程
    playback.rs    WAV 播放（可被原子标志打断）
    vad.rs         能量 VAD + 自适应噪声底
    resample.rs    重采样（48k→16k 抽取 / 22.05k→48k 插值）
    stdin_source.rs 开发模式输入
  asr/             whisper.cpp 子进程封装
  tts/             Piper 子进程封装
deploy/
  install.sh       树莓派一键安装（依赖、引擎、模型、systemd）
  xiaomeng.service
  asound.conf      ALSA / 蓝牙路由示例
examples/
  asr_probe.rs     单条 WAV 直连 whisper，排查识别问题
  tts_probe.rs     单句文本直连 piper，排查合成问题
scripts/
  fetch-models.sh  下载模型（Mac / Pi 通用）
  build-pi.sh      交叉编译 + scp + 远程重启
tests/
  agent_flow.rs    用假服务器验证 tool calling 闭环
```

---

## 3. 开发机快速验证（不需要麦克风和模型）

```bash
cp .env.example .env          # 填入 DEEPSEEK_API_KEY
cargo run -- --dev
```

`--dev` 会切换为「标准输入 + mock ASR/TTS」：

```text
[dev] 输入一行文本并回车，模拟"听到"一句话；Ctrl-D 或 /quit 退出。
小蒙同学          ← 唤醒（不播报，只打日志）
今天几号
```

运行测试：

```bash
cargo test
```

其中 `tests/agent_flow.rs` 会启动一个本地假 HTTP 服务，完整验证
「模型返回 tool_calls → 调搜索 → 结果回传 → 二次请求 → 最终回复」以及数据库写入。

---

## 4. 配置

```bash
cp config.example.toml config.toml
```

密钥只从环境变量读取（支持 `.env`），配置文件中不出现密钥：

| 变量 | 说明 |
| --- | --- |
| `DEEPSEEK_API_KEY` | 必填 |
| `TAVILY_API_KEY` | `search.provider = "tavily"` 时必填 |
| `SEARXNG_URL` | `search.provider = "searxng"` 时使用 |
| `WHISPER_MODEL` / `PIPER_MODEL` | 覆盖模型路径 |
| `RUST_LOG` | 例如 `info`、`debug` |

检查配置与模型文件：

```bash
cargo run -- --check
```

常用可调项：

- `audio.vad.min_rms`：环境吵就调大（0.006 → 0.012）。
- `audio.vad.end_silence_ms`：说完整句后等多久收尾，默认 600ms。
- `audio.input_device` / `output_device`：设备名片段（大小写不敏感），留空用系统默认。
- `llm.history_limit`：上下文条数，树莓派内存有限，建议 8~16。
- `tts.length_scale`：小于 1 更快。

---

## 5. 模型与引擎

### Whisper 模型

```bash
./scripts/fetch-models.sh                        # base
WHISPER_MODEL=ggml-tiny.bin ./scripts/fetch-models.sh
```

树莓派 4 实测参考（4 线程）：

| 模型 | 大小 | 10 秒语音耗时 | 中文准确率 |
| --- | --- | --- | --- |
| `ggml-tiny.bin` | 75 MB | ~1.0s | 偏低，仅适合安静环境 |
| `ggml-base.bin` | 142 MB | ~2.0s | 日常可用（默认） |
| `ggml-small.bin` | 466 MB | ~5s | 明显更好，但交互会变慢 |

### Piper 语音

`zh_CN-huayan-medium.onnx` + 同名 `.onnx.json`，`scripts/fetch-models.sh` 会一起下载。

---

## 6. 部署到树莓派

### 6.1 系统要求（重要）

- **推荐**：64 位 Raspberry Pi OS Bookworm，或 Ubuntu 22.04 / 24.04。
- 原始规格里的 **Ubuntu 18.04（glibc 2.27）已 EOL**，且无法运行 `cross` 新镜像产出的二进制
  （需要 glibc ≥ 2.36）。如果必须留在 18.04：
  - 在树莓派上直接 `cargo build --release`（最稳），或
  - 用 `aarch64-unknown-linux-musl` 静态链接（cpal/ALSA 需要额外配置，未验证）。
- 需要 `hci0` 正常、`libasound2-dev`、CMake、C++ 编译器。

### 6.2 一键安装

```bash
git clone <repo> xiaomeng && cd xiaomeng
sudo SERVICE_USER=pi ./deploy/install.sh --build
```

脚本会：装系统依赖 → 编译 whisper.cpp → 安装 Piper 与中文语音 → 下载模型 →
写 `/etc/xiaomeng/config.toml` 与 `/etc/xiaomeng/xiaomeng.env` → 安装并启用 systemd 服务。

然后在 `/etc/xiaomeng/xiaomeng.env` 填入密钥并重启：

```bash
sudo systemctl restart xiaomeng
journalctl -u xiaomeng -f
```

### 6.3 从 Mac 交叉编译

```bash
rustup target add aarch64-unknown-linux-gnu
cargo install cross          # 需要 Docker
./scripts/build-pi.sh pi@raspberrypi.local
```

> `Cross.toml` 已经为 cpal 装好了目标架构的 `libasound2-dev`。
> 再次强调 glibc 版本限制；跨不过去就直接在 Pi 上编译。

### 6.4 蓝牙与 ALSA

```bash
bluetoothctl
  power on
  agent on
  scan on
  pair AA:BB:CC:DD:EE:FF
  trust AA:BB:CC:DD:EE:FF
  connect AA:BB:CC:DD:EE:FF
```

确认系统暴露的设备：

```bash
aplay -L      # 输出设备
arecord -L    # 输入设备
speaker-test -c 2 -t wav
arecord -d 3 -f S16_LE -r 16000 -c 1 /tmp/t.wav && aplay /tmp/t.wav
```

如果用的是 BlueALSA 而不是 PulseAudio/PipeWire，参考 `deploy/asound.conf` 方案 B，
并在 `config.toml` 里把设备名写成 `bluealsa` 片段。

蓝牙固件与 `hciuart`（树莓派 4 用的是 `BCM4345C0.hcd`）：

```bash
sudo apt install -y pi-bluetooth bluez
ls -l /lib/firmware/brcm/BCM4345C0.hcd     # 固件是否存在
sudo systemctl status hciuart              # 串口服务是否在跑
hciconfig -a                               # 应看到 hci0 UP RUNNING
dmesg | grep -i -E "bluetooth|hci"         # 固件加载日志
```

Raspberry Pi OS 已内置固件，通常无需手动拷贝；若 `hci0` 不存在，先确认
`/boot/firmware/config.txt` 里没有禁用蓝牙，再 `sudo systemctl restart hciuart`。
Ubuntu 需要额外安装 `linux-firmware` 与 `bluez`。

---

## 7. 交互细节与已知限制

**状态机**

| 状态 | 行为 | 退出条件 |
| --- | --- | --- |
| `Idle` | 持续采集、切句、转写，只匹配唤醒词 | 命中唤醒词 |
| `Active` | 播报「我在」，开 10 秒窗口 | 用户说话 → `Busy`；超时 → `Idle`；再次唤醒 → 重开 |
| `Busy` | 请求模型、调工具、播报回答 | 播报结束 → `Active`；唤醒词 → 立即中断并重开 |

**一句话连唤醒带提问**：说「小蒙同学，今天天气怎么样」时，唤醒词之后的内容会直接作为本轮提问，
不再重复应答「我在」，也不用等 10 秒窗口。只有单独喊唤醒词时才会播报应答。

**打断**：唤醒词在任何状态都生效。实现上是把「会话代际」自增并通过 `watch` 广播，
所有派生任务用 `select!` 监听，旧任务直接返回；正在播放的音频通过 `AtomicBool` 静音并释放输出流。

**已知限制**

1. **没有回声消除（AEC）**。TTS 播放时麦克风仍开着（否则无法打断），
   音箱声音可能被自己听到。实践中因为需要精确命中唤醒词，误触发概率低；
   如果出现自唤醒，把音箱音量调低，或改用带 AEC 的会议麦克风。
2. **唤醒延迟 1~2 秒**，取决于模型大小和「说完到静音判定」的 600ms。
3. **串行识别**：ASR 一次只处理一句。播放期间产生的环境音会排队，
   极端情况下会延迟一次唤醒响应。
4. **Ubuntu 18.04 不可用**（见 6.1）。
5. **繁简转换是逐字表**（约 200 个高频字），不是完整 OpenCC。它只用于唤醒匹配，
   转写原文仍按 whisper 输出保存。遇到表外的字，可以在 `wake.words` 里补上对应写法。

---

## 8. 数据表

```sql
CREATE TABLE IF NOT EXISTS chat_history (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,   -- 每次唤醒生成的 UUID
    role       TEXT NOT NULL,   -- 'user' | 'assistant'
    content    TEXT NOT NULL,
    timestamp  TEXT NOT NULL    -- ISO8601（本地时区，毫秒）
);
CREATE INDEX IF NOT EXISTS idx_chat_history_session
    ON chat_history(session_id, id);
```

默认位置 `./data/assistant_sessions.db`，部署时为 `/var/lib/xiaomeng/assistant_sessions.db`。

---

## 9. 故障排查

| 现象 | 处理 |
| --- | --- |
| `没有默认输入设备` | `arecord -L` 看设备；在配置里填 `input_device` 片段；确认用户属于 `audio` 组 |
| `创建输入流失败` / `DeviceBusy` | 蓝牙耳机被其它程序占用，`pulseaudio -k` 或断开重连 |
| `无法执行 whisper-cli` | 确认 `/usr/local/bin/whisper-cli` 存在且可执行；`whisper-cli -h` |
| whisper 输出乱码/幻觉 | 换 `ggml-base.bin`；确认 `language = "zh"`；调高 `min_rms` |
| 一直不唤醒 | 看日志里是否有「识别」行；有文本但不匹配 → 把实际文本加进 `wake.words` |
| 想单独验证 ASR | `cargo run --release --example asr_probe -- /tmp/录音.wav config.toml`，会打印原文、清洗结果和是否命中唤醒词 |
| 模型请求失败 401 | 检查 `DEEPSEEK_API_KEY` 是否被 systemd 读到（`EnvironmentFile`） |
| 服务反复重启 | `journalctl -u xiaomeng -n 100 --no-pager` |
| 蓝牙音质差/断连 | 用 5GHz WiFi，或改用 USB 蓝牙 dongle；2.4GHz 与 A2DP 容易互相干扰 |

---

## 10. 后续可以做的事

- 用 **openWakeWord** 替换文本唤醒，把延迟压到 300ms 以内并降低 CPU 占用。
- whisper.cpp 常驻 `whisper-server`，避免每次识别重新加载模型。
- 接入 Home Assistant / MQTT，做设备控制类工具调用。
- 用 ALSA `dsnoop` + WebRTC AEC 解决自唤醒。
- 给 SQLite 增加跨会话摘要，实现长期记忆。
