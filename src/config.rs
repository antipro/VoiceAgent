//! 配置加载：TOML 文件 + 环境变量覆盖。
//!
//! 密钥只从环境变量读取（可配合 `.env`），不写入配置文件，避免误提交。

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::fmt;
use std::path::{Path, PathBuf};

/// 包一层密钥，`Debug` 输出 `***`，防止日志泄漏。
#[derive(Clone, Default, Deserialize)]
pub struct Secret(pub String);

impl Secret {
    pub fn is_empty(&self) -> bool {
        self.0.trim().is_empty()
    }

    pub fn expose(&self) -> &str {
        self.0.trim()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            f.write_str("Secret(<empty>)")
        } else {
            f.write_str("Secret(***)")
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub assistant: AssistantConfig,
    pub wake: WakeConfig,
    pub audio: AudioConfig,
    pub asr: AsrConfig,
    pub tts: TtsConfig,
    pub llm: LlmConfig,
    pub search: SearchConfig,
    pub storage: StorageConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AssistantConfig {
    /// 助手名字，仅用于日志与系统提示。
    pub name: String,
    /// 唤醒后立即播报的内容。
    pub greeting: String,
    /// 模型判断结束后若没有给出告别语，用这句兜底。
    pub farewell: String,
    /// Active 状态等待用户说话的窗口（秒）。
    pub active_timeout_secs: u64,
    /// 送给大模型的系统提示词。
    pub system_prompt: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WakeConfig {
    /// 是否启用唤醒词。
    pub enabled: bool,
    /// 唤醒词列表，命中任意一个即视为唤醒。
    pub words: Vec<String>,
    /// 拼音匹配时把后鼻音 -ng 折成前鼻音 -n（蒙/门、明/民）。
    pub fold_nasal_n: bool,
    /// 允许的编辑距离（容错，针对同音误识别）。
    pub fuzzy_max_distance: usize,
    /// 短于该字数的文本不做模糊匹配，避免误唤醒。
    pub fuzzy_max_len: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    /// `cpal` 走真实声卡；`stdin` 用于在开发机上无麦克风调试。
    pub backend: String,
    /// 输入设备名（模糊匹配，留空用系统默认）。
    pub input_device: Option<String>,
    /// 输出设备名（模糊匹配，留空用系统默认）。
    pub output_device: Option<String>,
    /// 播放音量倍率。
    pub playback_volume: f32,
    pub vad: VadConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct VadConfig {
    /// 分析帧长，毫秒。
    pub frame_ms: u32,
    /// 连续多少帧语音后判定说话开始。
    pub start_frames: u32,
    /// 尾部静音多少毫秒后判定说话结束。
    pub end_silence_ms: u32,
    /// 语音开始前保留的音频，防止吞掉首字。
    pub pre_roll_ms: u32,
    /// 过短的片段直接丢弃。
    pub min_speech_ms: u32,
    /// 单句上限，超过强制切分。
    pub max_utterance_ms: u32,
    /// 绝对静音阈值（RMS，0~1）。
    pub min_rms: f32,
    /// 动态阈值：高于噪声底该倍数才视为语音。
    pub noise_multiplier: f32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AsrConfig {
    /// `whisper` 调用外部 whisper.cpp 可执行文件；`mock` 仅用于测试。
    pub engine: String,
    /// whisper.cpp 可执行文件路径，例如 `whisper-cli`。
    pub command: String,
    /// ggml 模型路径。
    pub model: PathBuf,
    /// 识别语言，中文用 `zh`。
    pub language: String,
    /// 推理线程数，树莓派 4 建议 4。
    pub threads: u32,
    /// 解码宽度：1 = 贪心（最快），5 = beam search（更准但明显更慢）。
    pub beam_size: u32,
    /// 编码窗口（mel 帧，1500 = 30 秒）。省略则按音频长度自动计算。
    pub audio_ctx: Option<u32>,
    /// 追加给 whisper.cpp 的参数。
    pub extra_args: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TtsConfig {
    /// `piper` 调用外部 piper 可执行文件；`mock` 仅打印文本。
    pub engine: String,
    pub command: String,
    pub model: PathBuf,
    /// piper 的 `.onnx.json`，留空则用同名文件。
    pub config: Option<PathBuf>,
    pub speaker: Option<i32>,
    /// 语速：小于 1 更快，大于 1 更慢。
    pub length_scale: f32,
    pub extra_args: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    pub base_url: String,
    pub model: String,
    /// 从环境变量 `DEEPSEEK_API_KEY` 读取。
    pub api_key: Secret,
    pub temperature: f32,
    pub max_tokens: u32,
    /// 送入上下文的历史消息条数。
    pub history_limit: usize,
    /// 工具调用最大轮数，防止模型死循环。
    pub max_tool_rounds: usize,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SearchConfig {
    /// `tavily` / `searxng` / `none`。
    pub provider: String,
    /// 从环境变量 `TAVILY_API_KEY` 读取。
    pub api_key: Secret,
    /// SearXNG 实例地址，例如 `http://127.0.0.1:8080`。
    pub base_url: String,
    pub max_results: u32,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    pub path: PathBuf,
}

// ---------------------------------------------------------------- defaults

impl Default for AssistantConfig {
    fn default() -> Self {
        Self {
            name: "小蒙同学".into(),
            greeting: "我在".into(),
            farewell: "好的，再见。".into(),
            active_timeout_secs: 10,
            system_prompt: default_system_prompt(),
        }
    }
}

impl Default for WakeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            // 四字比「小蒙」更不容易误唤醒（“笑蒙了”“小蒙圈”发音相同）。
            // 同音写法（小萌/筱蒙/晓蒙）由拼音匹配自动覆盖，不必逐个列出。
            words: vec![
                "小蒙同学".into(),
                "小明同学".into(),
                // whisper 常输出繁体，这里保留一份，双保险。
                "小蒙同學".into(),
                "小明同學".into(),
            ],
            // 拼音匹配已经覆盖同音字；字符级模糊匹配反而会把
            // “小明的同学”判成“小明同学”，默认关掉（0）。
            fold_nasal_n: true,
            fuzzy_max_distance: 0,
            fuzzy_max_len: 8,
        }
    }
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            backend: "cpal".into(),
            input_device: None,
            output_device: None,
            playback_volume: 1.0,
            vad: VadConfig::default(),
        }
    }
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            frame_ms: 20,
            start_frames: 3,
            end_silence_ms: 600,
            pre_roll_ms: 300,
            min_speech_ms: 350,
            max_utterance_ms: 15_000,
            min_rms: 0.006,
            noise_multiplier: 2.5,
        }
    }
}

impl Default for AsrConfig {
    fn default() -> Self {
        Self {
            engine: "whisper".into(),
            command: "whisper-cli".into(),
            model: PathBuf::from("./models/ggml-base.bin"),
            language: "zh".into(),
            threads: 4,
            beam_size: 1,
            audio_ctx: None,
            extra_args: vec!["--no-prints".into()],
        }
    }
}

impl Default for TtsConfig {
    fn default() -> Self {
        Self {
            engine: "piper".into(),
            command: "piper".into(),
            model: PathBuf::from("./models/zh_CN-huayan-medium.onnx"),
            config: None,
            speaker: None,
            length_scale: 0.95,
            extra_args: Vec::new(),
        }
    }
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.deepseek.com".into(),
            model: "deepseek-chat".into(),
            api_key: Secret::default(),
            temperature: 0.7,
            max_tokens: 512,
            history_limit: 12,
            max_tool_rounds: 3,
            timeout_secs: 45,
        }
    }
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            provider: "deepseek".into(),
            api_key: Secret::default(),
            base_url: "http://127.0.0.1:8080".into(),
            max_results: 4,
            timeout_secs: 20,
        }
    }
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            path: PathBuf::from("./data/assistant_sessions.db"),
        }
    }
}

fn default_system_prompt() -> String {
    "你是运行在树莓派上的离线语音助手，名叫“小蒙同学”。\n\
     你通过语音与用户交流，所以回答必须口语化、简短（通常不超过 80 个字）。\n\
     不要使用 Markdown、代码块、列表、表情符号或括号注释，因为回答会被直接朗读出来。\n\
     需要实时资讯、天气、新闻、价格等你不确定的信息时，调用 web_search 工具再作答。\n\
     如果搜索没有给出可靠结果，就如实说明，不要编造。\n\
     当用户明确表示要结束对话（例如说再见、拜拜、没事了、先这样）时，调用 end_conversation 工具，并用一句话告别。"
        .into()
}

// ---------------------------------------------------------------- loading

impl Config {
    /// 从 `path` 读取配置；文件不存在时使用内置默认值。
    pub fn load(path: &Path) -> Result<Self> {
        let mut cfg = if path.exists() {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("读取配置文件失败: {}", path.display()))?;
            toml::from_str(&raw).with_context(|| format!("解析配置文件失败: {}", path.display()))?
        } else {
            tracing::warn!(path = %path.display(), "配置文件不存在，使用默认配置");
            Self::default()
        };

        cfg.apply_env();
        Ok(cfg)
    }

    fn apply_env(&mut self) {
        if let Ok(v) = std::env::var("DEEPSEEK_API_KEY") {
            self.llm.api_key = Secret(v);
        }
        if let Ok(v) = std::env::var("DEEPSEEK_BASE_URL") {
            self.llm.base_url = v;
        }
        if let Ok(v) = std::env::var("DEEPSEEK_MODEL") {
            self.llm.model = v;
        }
        if let Ok(v) = std::env::var("TAVILY_API_KEY") {
            self.search.api_key = Secret(v);
        }
        if let Ok(v) = std::env::var("SEARXNG_URL") {
            self.search.base_url = v;
        }
        if let Ok(v) = std::env::var("SEARCH_PROVIDER") {
            self.search.provider = v;
        }
        if let Ok(v) = std::env::var("WHISPER_BIN") {
            self.asr.command = v;
        }
        if let Ok(v) = std::env::var("WHISPER_MODEL") {
            self.asr.model = PathBuf::from(v);
        }
        if let Ok(v) = std::env::var("PIPER_BIN") {
            self.tts.command = v;
        }
        if let Ok(v) = std::env::var("PIPER_MODEL") {
            self.tts.model = PathBuf::from(v);
        }
        if let Ok(v) = std::env::var("XIAOMENG_DB") {
            self.storage.path = PathBuf::from(v);
        }
    }

    /// 启动前检查。`strict` 为真时缺少密钥会直接失败。
    pub fn validate(&self, strict: bool) -> Result<()> {
        if !matches!(self.audio.backend.as_str(), "cpal" | "stdin") {
            bail!(
                "audio.backend 只能是 cpal 或 stdin，当前为 {}",
                self.audio.backend
            );
        }
        if self.wake.enabled && self.wake.words.is_empty() {
            bail!("wake.enabled 为真但 wake.words 为空");
        }
        if self.asr.engine == "whisper" && !self.asr.model.exists() {
            bail!(
                "ASR 模型不存在: {}（请先运行 scripts/fetch-models.sh）",
                self.asr.model.display()
            );
        }
        if self.tts.engine == "piper" && !self.tts.model.exists() {
            bail!(
                "TTS 模型不存在: {}（请先运行 scripts/fetch-models.sh）",
                self.tts.model.display()
            );
        }
        if strict {
            if self.llm.api_key.is_empty() {
                bail!("缺少 DEEPSEEK_API_KEY");
            }
            if self.search.provider == "tavily" && self.search.api_key.is_empty() {
                bail!("search.provider=tavily 但缺少 TAVILY_API_KEY");
            }
            if !matches!(
                self.search.provider.as_str(),
                "deepseek" | "tavily" | "searxng" | "none" | "disabled"
            ) {
                bail!(
                    "未知的 search.provider: {}（可选 deepseek / tavily / searxng / none）",
                    self.search.provider
                );
            }
        }
        Ok(())
    }

    /// 生成一份脱敏的摘要，供 `--check` 打印。
    pub fn summary(&self) -> String {
        format!(
            "assistant={}  wake={:?}\n\
             audio.backend={} in={:?} out={:?}\n\
             asr={} model={}\n\
             tts={} model={}\n\
             llm={} @ {} key={:?}\n\
             search={} max_results={}\n\
             db={}",
            self.assistant.name,
            self.wake.words,
            self.audio.backend,
            self.audio.input_device,
            self.audio.output_device,
            self.asr.engine,
            self.asr.model.display(),
            self.tts.engine,
            self.tts.model.display(),
            self.llm.model,
            self.llm.base_url,
            self.llm.api_key,
            self.search.provider,
            self.search.max_results,
            self.storage.path.display(),
        )
    }
}
