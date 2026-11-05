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
                  DeepSeek（tool_calls）──▶ web_search（DeepSeek Search / Tavily / SearXNG）
                        │
                        ▼
                 Piper TTS ──WAV──▶ cpal ──▶ 蓝牙音箱
```

---

## 1. 已实现的能力

| 设计规格 | 实现方式 |
| --- | --- |
| 唤醒词「小蒙同学」 | VAD 切句 + whisper 转写 + 繁简归一 + **拼音匹配**（含前后鼻音折叠：小萌/筱蒙/晓蒙/小梦/小门 都能命中） |
| Active 10 秒窗口 | `tokio::time` 截止时间；问候语播放耗时不计入窗口；一旦检测到用户开口就重新计时 |
| 随时打断 | 唤醒代际（`watch<u64>`）广播，LLM 请求、TTS 合成、音频播放都会被立即取消 |
| Tool Calling 闭环 | 原生 OpenAI 兼容实现，最多 3 轮，搜索失败会回传给模型而不是崩溃 |
| SQLite 会话记录 | 表结构见第 8 节，使用 WAL 模式，阻塞调用走 `spawn_blocking` |
| 交叉编译 | 提供 Docker（目标系统内编译，推荐）与 `cross`（可选）两条路，见 6.3 |

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
  search.rs        DeepSeek Search / Tavily / SearXNG
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
  install.sh       树莓派一键安装（依赖、引擎、模型、systemd、可选 AEC）
  Dockerfile.build Mac 上用于产出与 Pi 一致的 aarch64 二进制
  xiaomeng.service
  asound.conf      ALSA / 蓝牙路由示例
  pipewire/        PipeWire WebRTC AEC 配置（0.3.48 语法）
examples/
  asr_probe.rs     单条 WAV 直连 whisper，排查识别问题
  tts_probe.rs     单句文本直连 piper，排查合成问题
scripts/
  fetch-models.sh  下载模型（Mac / Pi 通用）
  build-pi.sh      交叉编译 + scp + 远程重启
  setup-aec.sh     启用/检查 PipeWire 回声消除（安装后为 xiaomeng-aec）
tests/
  agent_flow.rs    用假服务器验证 tool calling 闭环
```

---

## 3. 开发机快速验证（不需要麦克风和模型）

```bash
cp .env.example .env          # 填入 DEEPSEEK_API_KEY
cargo run -- --dev
```

`--dev` 会切换为「标准输入 + mock ASR/TTS」，并把 10 秒等待窗口放宽到 60 秒（打字比说话慢）。
输入用 `rustyline` 做行编辑，中文退格按整字删除（直接读 stdin 时内核按字节处理，一个汉字要按三次退格）：

```text
你说> 小蒙同学
```

`Ctrl-C` / `Ctrl-D` / `/quit` 退出。

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
| `DEEPSEEK_API_KEY` | **必填**，对话和联网搜索都用它 |
| `TAVILY_API_KEY` | 可选，只有 `search.provider = "tavily"` 时需要 |
| `SEARXNG_URL` | 可选，`search.provider = "searxng"` 时使用 |
| `WHISPER_MODEL` / `PIPER_MODEL` | 覆盖模型路径 |
| `RUST_LOG` | 例如 `info`、`debug` |

### 4.1 联网搜索用哪个

默认 `search.provider = "deepseek"`，**复用 `DEEPSEEK_API_KEY`，不需要第二个 key**。

容易混淆的一点：DeepSeek 的 **Chat Completions** 接口没有内置 `web_search` 工具
（Responses / Anthropic 格式才有服务端搜索，Chat Completions 会静默忽略），
但它提供了独立的 **Search API**：

```http
POST https://api.deepseek.com/v1/tools/search
Authorization: Bearer <DEEPSEEK_API_KEY>

{"query": "...", "count": 5, "need_content": false}
```

按成功请求计费（$0.75 / 1000 次），返回 `title / url / summary / date` 等结构，
本项目的 `web_search` 工具就是调它。三种实现按需切换：

| provider | 需要额外的 key | 说明 |
| --- | --- | --- |
| `deepseek`（默认） | 否 | 官方搜索，按次计费 |
| `tavily` | `TAVILY_API_KEY` | 第三方，有免费额度 |
| `searxng` | 否 | 自建实例，完全免费，但结果质量看实例 |
| `none` | 否 | 关掉工具，模型只凭训练知识回答 |

检查配置与模型文件：

```bash
cargo run -- --check
```

常用可调项：

- `audio.vad.min_rms`：环境吵就调大（0.006 → 0.012）。
- `audio.vad.end_silence_ms`：说完整句后等多久收尾，默认 600ms。
- `audio.input_device` / `output_device`：设备名片段（大小写不敏感），留空用系统默认。
- `audio.sink_volume_percent`：启动时把系统输出音量设成该百分比（默认 100，0 = 不调整）。
  蓝牙音箱没有实体音量键、PipeWire 又常常默认只有 30% 左右时，靠它把音量拉满。
  超过 100 是软件增益，可能削波。
- `llm.history_limit`：上下文条数，树莓派内存有限，建议 8~16。
- `tts.length_scale`：小于 1 更快。
- `tts.comma_pause_ms` / `tts.sentence_pause_ms`：标点后的停顿，默认 120 / 350ms。
  Piper 对中文逗号只给 60~80ms，听起来像一口气念完；这里按标点拆段、逐段合成
  （`--json-input`，模型只加载一次）再拼接，停顿长度完全可控。
- `asr.beam_size`：默认 1（贪心），改成 5 更准但 Pi 4 上会慢数倍。
- `asr.audio_ctx`：默认按音频长度自动截断编码窗口；固定成 1500 等于旧行为（最慢）。

---

## 5. 模型与引擎

### Whisper 模型

```bash
./scripts/fetch-models.sh                        # base
WHISPER_MODEL=ggml-tiny.bin ./scripts/fetch-models.sh
```

树莓派 4 实测参考（4 线程）：

| 模型 | 大小 | Pi 4 短句耗时 | 中文准确率 |
| --- | --- | --- | --- |
| `ggml-tiny.bin` | 75 MB | ~2s | 偏低，仅适合安静环境 |
| `ggml-base.bin` | 142 MB | ~3s | 日常可用（默认） |
| `ggml-small.bin` | 466 MB | ~10s+ | 明显更好，但交互会变慢 |

> 这组数字来自 `--audio-ctx` 自动截断 + 贪心解码（默认配置）。
> 如果按 whisper 的默认值（30 秒编码窗口 + 5 beam），base 在 Pi 4 上要 **15~20 秒**。

### Piper 语音

`zh_CN-huayan-medium.onnx` + 同名 `.onnx.json`，`scripts/fetch-models.sh` 会一起下载。

Piper 自带的中文停顿很短（实测逗号只有 60~80ms），所以本项目会把回复按标点拆成小段，
再按逗号/句号插入 120/350ms 静音。段数上限 12，过短的片段（「嗯，」）会自动并入下一段。

**延迟优化**（Pi 4 实测）：

| 阶段 | 耗时 |
| --- | --- |
| 回复文本 → 第一段开始播放 | **~0.4s** |
| 整段 4.8s 回复播完 | ~4.5s |
| 启动时模型预热 | ~6s（后台进行，不阻塞） |

做法：piper 以 `--json-input` 常驻，模型只加载一次；播放端**边合成边播**，
第一段合成完就开始放，后面的段在播放期间合成。之前是「整段合成完再播」，首句要等 5 秒。

---

## 6. 部署到树莓派

### 6.1 系统要求（重要）

- **目标系统：Ubuntu 22.04 LTS（glibc 2.35）**，或 64 位 Raspberry Pi OS Bookworm。
- 22.04 上 **默认音频仍是 PulseAudio**，PipeWire 需要手动接管（见 6.5）。这不影响先跑起来，
  但会影响 AEC。
- 交叉编译注意：不同 `cross` 版本的镜像基于不同 Debian，glibc 可能比 2.35 新
  （打开就报 `GLIBC_2.36 not found`）。**最稳的是在 Pi 上原生编译**：
  `cargo install --path . --release`，或 `./deploy/install.sh --build`。
  若一定要交叉编译，请选与 2.35 匹配的镜像，并在 Pi 上 `ldd --version` 核对。
- 需要 `hci0` 正常、`libasound2-dev`、CMake、C++ 编译器。

### 6.2 一键安装

```bash
git clone <repo> xiaomeng && cd xiaomeng
sudo ./deploy/install.sh --build          # 不含 AEC
sudo ./deploy/install.sh --build --aec    # 含 PipeWire + WebRTC AEC
```

脚本会自动把服务用户设成 `sudo` 的原始用户（也就是你登录的账号），并校验它存在。
需要换成别的账号时显式指定：

```bash
sudo SERVICE_USER=ubuntu ./deploy/install.sh --build
```

> **关于用户名**：`pi` 是 2022 年以前 Raspberry Pi OS 的默认用户。
> 现在的 **Bookworm 及以后不再创建 `pi`**，用户名是装机时在 Imager 里自己设的；
> **Ubuntu 22.04 默认是 `ubuntu`**。用错用户名时脚本会直接报错并列出本机可用用户，
> 不会生成一个起不来的 systemd 单元。

脚本会：装系统依赖 → 编译 whisper.cpp → 安装 Piper 与中文语音 → 下载模型 →
写 `/etc/xiaomeng/config.toml` 与 `/etc/xiaomeng/xiaomeng.env` → 安装并启用 systemd 服务。
加 `--aec` 时还会安装 PipeWire 音频栈、打开 linger、写入 AEC 配置。

> systemd 单元里已经设置 `XDG_RUNTIME_DIR=/run/user/<uid>` —— 系统服务要访问用户会话的
> PipeWire，少了这一行会退回到没有音频设备。

然后在 `/etc/xiaomeng/xiaomeng.env` 填入密钥并重启：

```bash
sudo systemctl restart xiaomeng
journalctl -u xiaomeng -f
```

### 6.3 编译：Pi 上原生 vs Mac 上交叉

先澄清 `--build` 的含义：它是**在树莓派本机**执行 `cargo build --release`，
不是交叉编译。三条路对比如下：

| 方式 | 命令 | 速度 | 风险 |
| --- | --- | --- | --- |
| Docker 里用 Ubuntu 22.04/arm64 编译（推荐） | `./scripts/build-pi.sh` | Apple Silicon 上原生速度 | 无 glibc 问题 |
| Pi 上原生编译 | `sudo ./deploy/install.sh --build` | Pi 4 约 5~10 分钟 | 需要在 Pi 上装 Rust |
| `cross` 交叉编译 | `./scripts/build-pi.sh --cross` | 最快 | 镜像 glibc 可能比目标新 |

**① Docker（推荐）**

```bash
./scripts/build-pi.sh                              # 只构建
./scripts/build-pi.sh <用户名>@<树莓派地址>          # 构建 + scp + 远程重启
```

首次会构建 `deploy/Dockerfile.build`（Ubuntu 22.04 + arm64 + Rust，约 1GB）。
产物是 `target/linux-arm64/release/xiaomeng`，和 Pi 上的系统库版本完全一致。
Intel Mac 上 Docker 会走 QEMU，能编但慢，建议改用 Pi 原生编译。

**② Pi 上原生编译**

```bash
# Pi 上先装 Rust（只需一次）
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
# 然后
sudo ./deploy/install.sh --build
```

`install.sh --build` 只是调用 `cargo build --release` 并把产物装到 `/opt/xiaomeng`；
Pi 上没有 `cargo` 时脚本会直接报错并给出安装命令。

**③ `cross`（可选）**

```bash
rustup target add aarch64-unknown-linux-gnu
cargo install cross          # 需要 Docker
./scripts/build-pi.sh --cross <用户名>@<树莓派地址>
```

`Cross.toml` 已经为 cpal 装好了目标架构的 `libasound2-dev`。但 cross 镜像基于的
Debian 版本决定了产物的 glibc：**必须 ≤ 目标系统的 glibc**（Ubuntu 22.04 是 2.35）。
比目标新就会在 Pi 上报 `GLIBC_2.36 not found`，这时换 ① 或 ②，别在镜像上耗时间。

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

### 6.5 回声消除（PipeWire + WebRTC AEC）

目标是在系统层把「音箱正在放的声音」从麦克风里扣掉，应用层（本项目）读到的就是干净音频，
不需要在 Rust 里写 DSP。**方案可行，但 Ubuntu 22.04 上有四个硬性前提**：

1. **22.04 默认不是 PipeWire 音频**。`pipewire-audio` 这个元包在 jammy 里不存在（24.04 才有），
   要手动装 `pipewire pipewire-pulse pipewire-alsa wireplumber libpipewire-0.3-modules libspa-0.2-modules`，
   并让 PipeWire 取代 PulseAudio。`--aec` 会做这些。
2. **AEC 引擎必须落到 webrtc**。jammy 的 PipeWire 0.3.48 代码里默认值已经是
   `aec/libspa-aec-webrtc`（文档注释还写着 exaudio，是过时的），但显式写出 `library.name`
   最稳——旧版本或别人改过的配置里默认值不一定是它。
3. **0.3.48 不能按设备名绑定**。`capture.props`/`playback.props` + `node.target` 是新版本才有的；
   0.3.48 只会连到「当前默认」的 source/sink。所以**先连蓝牙耳机并让它成为默认设备，再加载模块**。
   耳机没连就开机，模块会绑到空设备或 HDMI——这时重跑一次 `xiaomeng-aec` 即可。
4. **应用侧要跟着改**。cpal 走 ALSA，看到的不是 PipeWire 节点名，而是 `pipewire` 这个 PCM，
   所以配置写 `input_device = "pipewire"` / `output_device = "pipewire"`，
   再由 PipeWire 的默认 source/sink 指向 echo-cancel。
   **播放必须走 echo-cancel sink**，否则 AEC 拿不到参考信号，等于没开——这是最常见的「配了没用」。

```bash
# 耳机连好之后（会写入配置、重启 PipeWire、设默认、自检）
xiaomeng-aec          # = scripts/setup-aec.sh
xiaomeng-aec --check  # 随时检查 AEC 是否真的在链路里
```

检查通过时 `pactl info` 的默认输入输出都带 `echo-cancel`。模块没加载时看：

```bash
journalctl --user -u pipewire -n 50 --no-pager
pw-cli ls Node | grep echo-cancel
```

配置示例见 `deploy/pipewire/51-xiaomeng-aec.conf`（按 0.3.48 语法写的：
`aec.args` 是**空格分隔的字符串**，写成 JSON 对象会被静默忽略）。

**蓝牙 profile 的取舍**：A2DP 只有输出，一旦开麦会切到 HFP（mSBC，16 kHz，音质下降）。
一体式耳机走 HFP 时收发是同一个设备时钟，AEC 效果最好；如果输入输出分别走不同设备，
0.3.48 又关闭了 drift compensation，残留回声会更明显。

**不想折腾 PipeWire 的替代方案**：22.04 默认的 PulseAudio 也有同一个 WebRTC AEC，
在 `/etc/pulse/default.pa` 加一行即可，稳定但功能少：

```bash
pactl load-module module-echo-cancel use_master_format=1 aec_method=webrtc \
    source_name=echoCancelSource sink_name=echoCancelSink
```

---

## 7. 交互细节与已知限制

**状态机**

| 状态 | 行为 | 退出条件 |
| --- | --- | --- |
| `Idle` | 持续采集、切句、转写，只匹配唤醒词 | 命中唤醒词 |
| `Active` | 播报「我在」，开 10 秒窗口；检测到用户开口就重新计时 | 用户说话 → `Busy`；超时 → `Idle`；再次唤醒 → 重开 |
| `Busy` | 请求模型、调工具、播报回答 | 播报结束 → `Active`；唤醒词 → 立即中断并重开 |

**一句话连唤醒带提问**：说「小蒙同学，今天天气怎么样」时，唤醒词之后的内容会直接作为本轮提问，
不再重复应答「我在」，也不用等 10 秒窗口。只有单独喊唤醒词时才会播报应答。

**结束对话**：说「再见」「拜拜」「先这样」时，模型会调用 `end_conversation` 工具，
播报告别后回到 `Idle`（仍可随时用唤醒词叫醒）。即使模型没给告别语，
也会用 `assistant.farewell`（默认「好的，再见。」）兜底。

**打断**：唤醒词在任何状态都生效。实现上是把「会话代际」自增并通过 `watch` 广播，
所有派生任务用 `select!` 监听，旧任务直接返回；正在播放的音频通过 `AtomicBool` 静音并释放输出流。

**唤醒词为什么能容忍同音字**

whisper 对同一段发音可能每次写出不同的汉字，靠枚举写法永远追不完。匹配分三层：

1. **精确**：归一化（去标点、繁转简）后直接找唤醒词。
2. **拼音**：把整句和唤醒词都转成无声调拼音再比。
   `小蒙 / 小萌 / 小梦 / 筱蒙 / 晓蒙 / 肖蒙` 都是 `xiaomeng`，全部命中。
   同时把后鼻音 `-ng` 折成前鼻音 `-n`（`wake.fold_nasal_n`，默认开），
   所以 `蒙/门`、`明/民` 这种前后鼻音不分的口音也能命中。
   命中位置会映射回原字符，所以「筱蒙同学，今天天气」仍能正确摘出后半句。
3. **字符模糊**：默认关闭（`fuzzy_max_distance = 0`）。它会把
   「小明的同学」判成「小明同学」，而拼音层已经覆盖了同音场景。

默认唤醒词只保留四字的「小蒙同学 / 小明同学」：两字的「小蒙」和
「笑蒙了」「小蒙圈」发音完全相同，误唤醒率太高。发音不同的叫法
（比如「小敏」）直接加进 `wake.words` 即可。

鼻音折叠的代价是「小敏同学」和「小明同学」会被当成同一个发音；
如果这两个名字在你的场景里必须区分，把 `fold_nasal_n` 关掉，
再用 `wake.words` 精确列出需要的写法。

**已知限制**

1. **默认没有回声消除**。TTS 播放时麦克风仍开着（否则无法打断），音箱声音可能被自己听到。
   启用 6.5 的 PipeWire AEC 后，读到的是消过回声的音频；不启用时靠「必须命中唤醒词」
   来降低误触发，实在不行就调低音量或用带 AEC 的会议麦克风。
   特别地，如果回复里出现「小蒙同学」（比如用户问「你叫什么」），没有 AEC 时可能自唤醒——
   这是启用 AEC 最实际的理由。
2. **唤醒延迟 1~2 秒**，取决于模型大小和「说完到静音判定」的 600ms。
3. **串行识别**：ASR 一次只处理一句。播放期间产生的环境音会排队，
   极端情况下会延迟一次唤醒响应。
4. **22.04 的 PipeWire 版本偏旧（0.3.48）**：AEC 不能按设备名绑定、模块只跟随默认设备，
   开机顺序不对时要手动重跑 `xiaomeng-aec`（见 6.5）。
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
| 模型请求失败 422 `tools: invalid type` | 旧版本 bug，已修复；`cargo build` 重新编译即可 |
| 搜索报 402 / 429 | DeepSeek 余额不足，或触发 Search API 的 QPS 限制（100 QPS） |
| 服务反复重启 | `journalctl -u xiaomeng -n 100 --no-pager` |
| 蓝牙音质差/断连 | 用 5GHz WiFi，或改用 USB 蓝牙 dongle；2.4GHz 与 A2DP 容易互相干扰 |
| `xiaomeng-aec --check` 报没有 echo-cancel 节点 | 耳机没连/没切到 HFP；或 `library.name` 没写；`journalctl --user -u pipewire -n 50` |
| AEC 开了但还是自唤醒 | 播放没走 echo-cancel sink；`pactl get-default-sink` 必须是 `echo-cancel-sink` |
| 系统服务里没有音频设备 | 单元缺少 `XDG_RUNTIME_DIR=/run/user/<uid>`，或没 `loginctl enable-linger <用户名>` |
| `systemctl status` 报 `Failed to determine user credentials` | `SERVICE_USER` 写成了不存在的用户（例如新系统上的 `pi`），用 `sudo SERVICE_USER=$(whoami) ./deploy/install.sh` 重来 |

---

## 10. 后续可以做的事

- 用 **openWakeWord** 替换文本唤醒，把延迟压到 300ms 以内并降低 CPU 占用。
- whisper.cpp 常驻 `whisper-server`，避免每次识别重新加载模型。
- 接入 Home Assistant / MQTT，做设备控制类工具调用。
- 用 ALSA `dsnoop` + WebRTC AEC 解决自唤醒。
- 给 SQLite 增加跨会话摘要，实现长期记忆。
