//! 小蒙同学 —— 树莓派 4 离线语音助手（可执行入口）。

use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

use xiaomeng::audio::capture::Capture;
use xiaomeng::config::Config;
use xiaomeng::{agent, asr, audio, fsm, memory, tts};

struct Args {
    config: PathBuf,
    check: bool,
    dev: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let args = parse_args();
    init_tracing(args.dev);

    let mut cfg = Config::load(&args.config)?;
    if args.dev {
        apply_dev(&mut cfg);
    }

    if args.check {
        cfg.validate(true)?;
        println!("配置检查通过。\n\n{}", cfg.summary());
        return Ok(());
    }

    cfg.validate(false)?;
    if cfg.llm.api_key.is_empty() {
        tracing::warn!("未设置 DEEPSEEK_API_KEY，所有对话都会失败");
    }
    if cfg.search.provider == "tavily" && cfg.search.api_key.is_empty() {
        tracing::warn!("未设置 TAVILY_API_KEY，搜索工具会失败");
    }

    let cfg = Arc::new(cfg);

    let store = Arc::new(memory::Store::open(&cfg.storage.path)?);
    tracing::info!(
        db = %cfg.storage.path.display(),
        messages = store.message_count().await.unwrap_or(0),
        "数据库就绪"
    );

    let agent = Arc::new(agent::Agent::new(&cfg, store.clone())?);
    let tts = Arc::new(tts::Tts::from_config(&cfg.tts)?);
    tracing::info!(
        asr = %cfg.asr.engine,
        tts = %tts.describe(),
        search = %cfg.search.provider,
        "引擎就绪"
    );

    let (tx, rx) = mpsc::channel(32);
    let mut capture: Option<Capture> = None;

    match cfg.audio.backend.as_str() {
        "stdin" => {
            eprintln!("[dev] 输入文本回车；Ctrl-C / Ctrl-D / /quit 退出");
            let tx = tx.clone();
            tokio::spawn(async move {
                if let Err(e) = audio::stdin_source::run(tx).await {
                    tracing::error!("标准输入结束: {e:#}");
                }
            });
        }
        _ => {
            let (pcm_tx, pcm_rx) = mpsc::unbounded_channel();
            capture = Some(Capture::start(cfg.audio.input_device.clone(), pcm_tx)?);

            let (seg_tx, seg_rx) = mpsc::channel(4);
            let vad_cfg = cfg.audio.vad.clone();
            let vad_events = tx.clone();
            tokio::spawn(async move {
                audio::vad::run(pcm_rx, seg_tx, vad_events, vad_cfg).await;
            });

            let asr_engine = asr::Asr::from_config(&cfg.asr)?;
            tracing::info!(engine = %asr_engine.describe(), "ASR 就绪");
            let events = tx.clone();
            tokio::spawn(async move {
                asr::run(asr_engine, seg_rx, events).await;
            });
        }
    }

    let controller = fsm::Controller::new(cfg.clone(), store, agent, tts, tx, rx);

    let result = tokio::select! {
        r = controller.run() => r,
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("收到退出信号");
            Ok(())
        }
    };

    drop(capture);
    result
}

fn apply_dev(cfg: &mut Config) {
    tracing::warn!("开发模式：标准输入 + mock ASR/TTS（不依赖麦克风与模型）");
    cfg.audio.backend = "stdin".into();
    cfg.asr.engine = "mock".into();
    cfg.tts.engine = "mock".into();
    // 手动打字比说话慢，开发模式把 Active 窗口放宽，避免刚唤醒就超时。
    if cfg.assistant.active_timeout_secs < 60 {
        cfg.assistant.active_timeout_secs = 60;
        tracing::info!("开发模式：Active 窗口放宽到 60 秒");
    }
}

fn init_tracing(dev: bool) {
    use tracing_subscriber::EnvFilter;
    let default = if dev { "debug" } else { "info" };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();
}

fn parse_args() -> Args {
    let mut args = Args {
        config: PathBuf::from("config.toml"),
        check: false,
        dev: false,
    };

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-c" | "--config" => {
                if let Some(v) = it.next() {
                    args.config = PathBuf::from(v);
                } else {
                    eprintln!("--config 需要一个路径");
                    std::process::exit(2);
                }
            }
            "--check" => args.check = true,
            "--dev" => args.dev = true,
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            other => {
                eprintln!("未知参数: {other}（--help 查看用法）");
                std::process::exit(2);
            }
        }
    }
    args
}

fn print_help() {
    println!(
        "小蒙同学 v{}\n\n\
         用法: xiaomeng [选项]\n\n\
         选项:\n  \
           -c, --config <PATH>  配置文件路径（默认 ./config.toml）\n  \
               --check          只校验配置与模型文件，然后退出\n  \
               --dev            开发模式：标准输入 + mock ASR/TTS\n  \
           -h, --help           显示帮助\n\n\
         环境变量:\n  \
           DEEPSEEK_API_KEY     DeepSeek 密钥（必需）\n  \
           TAVILY_API_KEY       Tavily 搜索密钥（provider=tavily 时必需）\n  \
           RUST_LOG             日志级别，例如 info,debug\n",
        env!("CARGO_PKG_VERSION")
    );
}
