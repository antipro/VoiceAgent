//! 有限状态机：Idle → Active → Busy，唤醒词随时可打断。
//!
//! 状态机是唯一持有状态的地方；耗时操作（LLM、TTS、播放）都在派生任务里跑，
//! 通过 `watch` 通道广播“新会话”来取消，避免任何锁。

use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use uuid::Uuid;

use crate::agent::Agent;
use crate::audio;
use crate::config::Config;
use crate::memory::{Role, Store};
use crate::tts::Tts;
use crate::util;

/// Active 到期后额外等待的时间，避免把「卡在截止点」的输入丢掉。
const ACTIVE_GRACE: Duration = Duration::from_millis(600);

#[derive(Debug)]
pub enum Event {
    /// VAD 检测到用户开口（还没出识别结果）。
    SpeechStarted,
    /// 识别出一句话。
    Heard { text: String },
    /// 一轮问答（含播报）结束。
    PipelineDone {
        generation: u64,
        /// 模型调用了 end_conversation，播报完回到 Idle。
        end_session: bool,
    },
    /// 唤醒应答播报结束。
    GreetingDone { generation: u64 },
    /// 上游音频链路结束，进程应退出。
    Shutdown,
}

#[derive(Debug, Clone)]
enum State {
    Idle,
    Active { session: String, until: Instant },
    Busy { session: String },
}

pub struct Controller {
    cfg: Arc<Config>,
    store: Arc<Store>,
    agent: Arc<Agent>,
    tts: Arc<Tts>,
    tx: mpsc::Sender<Event>,
    rx: mpsc::Receiver<Event>,
    /// 会话代际：任何变化都意味着“放弃手头的工作”。
    wake_tx: watch::Sender<u64>,
    /// 播报代际：只用来抢占上一段音频。
    audio_tx: watch::Sender<u64>,
    generation: u64,
    state: State,
}

impl Controller {
    pub fn new(
        cfg: Arc<Config>,
        store: Arc<Store>,
        agent: Arc<Agent>,
        tts: Arc<Tts>,
        tx: mpsc::Sender<Event>,
        rx: mpsc::Receiver<Event>,
    ) -> Self {
        let (wake_tx, _) = watch::channel(0u64);
        let (audio_tx, _) = watch::channel(0u64);
        Self {
            cfg,
            store,
            agent,
            tts,
            tx,
            rx,
            wake_tx,
            audio_tx,
            generation: 0,
            state: State::Idle,
        }
    }

    pub async fn run(mut self) -> Result<()> {
        tracing::info!("助手就绪，等待唤醒词");
        loop {
            let deadline = match &self.state {
                State::Active { until, .. } => Some(*until),
                _ => None,
            };

            let event = match deadline {
                Some(until) => {
                    let mut timed_out = false;
                    let ev = tokio::select! {
                        ev = self.rx.recv() => ev,
                        _ = tokio::time::sleep_until(until) => {
                            timed_out = true;
                            None
                        }
                    };
                    if timed_out {
                        // 边界保护：识别结果可能正在路上（VAD 收尾、ASR 推理、开发模式回车）。
                        match tokio::time::timeout(ACTIVE_GRACE, self.rx.recv()).await {
                            Ok(ev) => ev,
                            Err(_) => {
                                tracing::info!(
                                    secs = self.cfg.assistant.active_timeout_secs,
                                    "Active 超时（窗口内没有说话），回到待命"
                                );
                                self.state = State::Idle;
                                continue;
                            }
                        }
                    } else {
                        ev
                    }
                }
                None => self.rx.recv().await,
            };

            let Some(event) = event else { break };
            if matches!(event, Event::Shutdown) {
                tracing::info!("输入链路已结束，退出");
                break;
            }
            self.handle(event).await?;
        }
        Ok(())
    }

    async fn handle(&mut self, event: Event) -> Result<()> {
        match event {
            Event::SpeechStarted => {
                // 用户已经开口，但一句话还没说完/没识别完：重新计时，别在句子中间超时。
                if let State::Active { session, .. } = self.state.clone() {
                    self.enter_active(session);
                    tracing::debug!("检测到用户开口，等待窗口重新计时");
                }
                Ok(())
            }
            Event::Heard { text } => self.on_heard(text).await,
            Event::PipelineDone {
                generation,
                end_session,
            } => {
                if generation == self.generation {
                    if let State::Busy { session } = self.state.clone() {
                        if end_session {
                            tracing::info!("用户结束对话，回到待命");
                            self.state = State::Idle;
                        } else {
                            self.enter_active(session);
                        }
                    }
                }
                Ok(())
            }
            Event::GreetingDone { generation } => {
                if generation == self.generation {
                    if let State::Active { session, .. } = self.state.clone() {
                        // 播报占用的时间不计入 10 秒等待窗口。
                        self.enter_active(session);
                    }
                }
                Ok(())
            }
            Event::Shutdown => Ok(()),
        }
    }

    async fn on_heard(&mut self, raw: String) -> Result<()> {
        let text = util::clean_transcript(&raw);
        if text.is_empty() {
            return Ok(());
        }

        if let Some(rest) = util::strip_wake(&self.cfg.wake, &text) {
            tracing::info!(text = %text, "检测到唤醒词");
            // 「小蒙同学，今天天气怎么样」：唤醒和提问在同一句里，不必再应答「我在」。
            let rest = rest.trim().to_string();
            let has_query = rest.chars().count() >= 2;
            self.start_session(!has_query).await?;
            if has_query {
                if let State::Active { session, .. } = self.state.clone() {
                    self.start_pipeline(session, rest).await?;
                }
            }
            return Ok(());
        }

        match self.state.clone() {
            State::Idle => {
                tracing::debug!(text = %text, "待命中，忽略");
                if !self.cfg.wake.enabled {
                    self.start_session(true).await?;
                    if let State::Active { session, .. } = self.state.clone() {
                        self.start_pipeline(session, text).await?;
                    }
                }
            }
            State::Active { session, .. } => {
                self.start_pipeline(session, text).await?;
            }
            State::Busy { .. } => {
                tracing::debug!(text = %text, "忙碌中，仅响应唤醒词");
            }
        }
        Ok(())
    }

    fn enter_active(&mut self, session: String) {
        let timeout = Duration::from_secs(self.cfg.assistant.active_timeout_secs.max(1));
        self.state = State::Active {
            session,
            until: Instant::now() + timeout,
        };
    }

    fn bump_audio(&self) {
        let next = self.audio_tx.borrow().wrapping_add(1);
        self.audio_tx.send_replace(next);
    }

    /// 唤醒：作废旧会话，开一个 10 秒窗口；`greet` 为真时播报「我在」。
    async fn start_session(&mut self, greet: bool) -> Result<()> {
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;

        // 广播：中断正在进行的请求与播放。
        self.wake_tx.send_replace(generation);
        self.bump_audio();

        let session = Uuid::new_v4().to_string();
        tracing::info!(%session, "新会话");
        self.enter_active(session.clone());

        let greeting = self.cfg.assistant.greeting.trim().to_string();
        if !greet || greeting.is_empty() {
            return Ok(());
        }

        self.store
            .append(&session, Role::Assistant, &greeting)
            .await
            .ok();

        let cfg = self.cfg.clone();
        let tts = self.tts.clone();
        let tx = self.tx.clone();
        let mut wake_rx = self.wake_tx.subscribe();
        let mut audio_rx = self.audio_tx.subscribe();

        tokio::spawn(async move {
            // 用户紧接着提问时，连合成都该停掉，别白白跑 8 秒 Piper。
            if let Err(e) =
                speak_cancellable(&cfg, tts.clone(), &greeting, &mut wake_rx, &mut audio_rx).await
            {
                tracing::warn!("播报唤醒应答失败: {e:#}");
            }
            let _ = tx.send(Event::GreetingDone { generation }).await;
        });

        Ok(())
    }

    /// 一轮问答：LLM（可带工具）→ TTS → 播报 → 回到 Active。
    async fn start_pipeline(&mut self, session: String, text: String) -> Result<()> {
        tracing::info!(%session, query = %text, "处理中");
        self.state = State::Busy {
            session: session.clone(),
        };
        // 抢占唤醒应答，避免和回答叠音。
        self.bump_audio();

        let generation = self.generation;
        let cfg = self.cfg.clone();
        let agent = self.agent.clone();
        let tts = self.tts.clone();
        let tx = self.tx.clone();
        let mut wake_rx = self.wake_tx.subscribe();
        let mut audio_rx = self.audio_tx.subscribe();

        tokio::spawn(async move {
            let reply = tokio::select! {
                r = agent.respond(&session, &text) => r,
                _ = wake_rx.changed() => {
                    tracing::info!("本轮请求已中断（新唤醒或进程退出）");
                    return;
                }
            };

            let mut end_session = false;
            match reply {
                Ok(reply) => {
                    tracing::info!(reply = %reply.text, end_session = reply.end_session, "回答");
                    end_session = reply.end_session;
                    if let Err(e) = speak_cancellable(
                        &cfg,
                        tts.clone(),
                        &reply.text,
                        &mut wake_rx,
                        &mut audio_rx,
                    )
                    .await
                    {
                        tracing::warn!("播报失败: {e:#}");
                    }
                }
                Err(e) => tracing::warn!("对话失败: {e:#}"),
            }

            let _ = tx
                .send(Event::PipelineDone {
                    generation,
                    end_session,
                })
                .await;
        });

        Ok(())
    }
}

/// 播报，但允许被「新唤醒 / 新请求」打断——包括打断 TTS 合成阶段。
async fn speak_cancellable(
    cfg: &Config,
    tts: Arc<Tts>,
    text: &str,
    wake_rx: &mut watch::Receiver<u64>,
    audio_rx: &mut watch::Receiver<u64>,
) -> Result<()> {
    let mut guard_wake = wake_rx.clone();
    let mut guard_audio = audio_rx.clone();
    tokio::select! {
        r = speak(cfg, tts, text, wake_rx, audio_rx) => r,
        _ = guard_wake.changed() => {
            tracing::debug!("播报被新会话打断");
            Ok(())
        }
        _ = guard_audio.changed() => {
            tracing::debug!("播报被新请求打断");
            Ok(())
        }
    }
}

async fn speak(
    cfg: &Config,
    tts: Arc<Tts>,
    text: &str,
    wake_rx: &mut watch::Receiver<u64>,
    audio_rx: &mut watch::Receiver<u64>,
) -> Result<()> {
    let speak_started = std::time::Instant::now();
    let pieces = tts.pieces(text);
    if pieces.len() <= 1 {
        let Some(path) = tts.synthesize(text).await? else {
            return Ok(());
        };
        tracing::info!(ms = speak_started.elapsed().as_millis(), "首段开始播放");
        let outcome =
            audio::playback::play_wav(path.clone(), cfg.audio.clone(), wake_rx, audio_rx).await;
        let _ = tokio::fs::remove_file(&path).await;
        return outcome.map(|_| ());
    }

    // 边合成边播：第一段合成完就开始放，后面的段在播放期间合成。
    let (tx, mut rx) = mpsc::channel::<(PathBuf, u32)>(2);
    let producer = tts.clone();
    let list = pieces.clone();
    tokio::spawn(async move {
        for (piece, pause) in list {
            match producer.synthesize_piece(&piece).await {
                Ok(path) => {
                    if tx.send((path, pause)).await.is_err() {
                        return;
                    }
                }
                Err(e) => {
                    tracing::warn!("分段合成失败: {e:#}");
                    return;
                }
            }
        }
    });

    let mut first = true;
    while let Some((path, pause)) = rx.recv().await {
        if first {
            tracing::info!(ms = speak_started.elapsed().as_millis(), "首段开始播放");
            first = false;
        }
        let outcome =
            audio::playback::play_wav(path.clone(), cfg.audio.clone(), wake_rx, audio_rx).await;
        let _ = tokio::fs::remove_file(&path).await;
        match outcome? {
            audio::playback::PlayOutcome::Cancelled => return Ok(()),
            audio::playback::PlayOutcome::Played => {}
        }
        if pause > 0 {
            tokio::time::sleep(Duration::from_millis(pause as u64)).await;
        }
    }
    Ok(())
}
