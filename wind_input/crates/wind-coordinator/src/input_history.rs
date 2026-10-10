//! 输入历史的**采集端**接线：通用上屏收口 → 攒「输入会话」 → 异步落库。
//!
//! 纯逻辑在两处：会话累积与断段判据在 [`InputHistoryBuf`]（本文件，可独立单测），
//! 存储在 `wind_store::input_history`，召回在 `ModeKind::InputHistory`（后续节点）。
//! 本模块只做接线与 IO 调度。
//!
//! # 为什么挂在 `note_commit_action` 而不是快捷输入历史那条路
//!
//! `learn_quick_history` / `learn_mix_literal` 的全部调用点都在 `handle_mode.rs` 的
//! **快捷输入（mix）路径**里——只有用户在快捷输入模式里上屏的文本才会进那个历史。
//! 用户真正的场景（在聊天框里正常打一段中文、发送失败后想找回）走的是**通用上屏路径**，
//! 在那个历史里一个字都没有。所以记录点必须挂到全模式共用的收口上。
//!
//! [`crate::handle_addword::note_commit_action`] 正是那个收口：它由
//! `coordinator/message_handler.rs` 的 `handle_key_event_policed` 统一调用，覆盖 40+ 返回点。
//!
//! # 计入哪些 `KeyAction`（判据与造词**刻意不同源**）
//!
//! | 变体 | 计入 | 理由 |
//! |---|---|---|
//! | `InsertText` / `InsertTextWithCursor` | ✅ | 新插入的文本 |
//! | `CommitAndHoldComposition { commit_text }` | ✅ | `commit_text` 是**真落屏**的收尾段 |
//! | `CommitThenDeferComposition { commit_text }` | ✅ | 同上；**五笔满码自动上屏走这条** |
//! | `ReplaceBackward` | ❌ | 回改已上屏内容，不是新输入 |
//! | `PassThrough` / `UpdateComposition` / `HoldComposition` | ❌ | 尚未落屏 |
//!
//! ## ★ 为什么必须包含 `Commit*` 系
//!
//! 这正是 `note_commit_action` 自己注释里在纠正的那个旧缺陷：那处 `committed` 清单
//! 曾经「只认 `InsertText`/`InsertTextWithCursor` 两种，把 `ReplaceBackward` 和 `Commit*`
//! 系全漏了」。实测五笔满码自动上屏走 `CommitThenDeferComposition`（`commit=大`）。
//!
//! 漏掉它恰好打在本功能的靶心上：**自动上屏的字不会入库**，用户丢失的长消息会残缺，
//! 而「残缺」比「完全没有」更难发现——恢复出来的句子读着通顺、只是少了几个字。
//!
//! ## 为什么排除 `ReplaceBackward`（而滑窗草稿层不排除）
//!
//! 滑窗那边对它做的是 `rewind`（把被改掉的部分从流里退出去），因为它的对象是
//! 「最近落屏的文本流」，回改**改变了那段流**。本模块的对象是「用户打了什么」——回改是
//! 修正动作、不是新的输入内容；计入会把同一段文字重复记账，或把修正片段当正文留在历史里。
//! 两处判据不同源是**有意**的，别顺手合并。
//!
//! # 会话边界（断段）
//!
//! 两级记录里的「会话」= 一次连续输入。边界有两个来源：
//!
//! 1. **停顿**：相邻两次上屏间隔超过 `input.history.idle_break_ms`（默认 3000）。
//!    在按键路径上判（纯比较，无 IO）。
//! 2. **断流事件**：回车 / 焦点丢失 / IME 停用 / 模式切换 / 切换方案 / 光标移动——
//!    全部已经收口在 [`crate::handle_addword::terminate_auto_phrase`]，本模块挂在同一处，
//!    不新开散点。
//!
//! # 按键路径上只做两件事
//!
//! 累积（`String::push_str` + `Vec::push`）与入队。**写库全在后台线程**
//! （`spawn_input_history_flush`，照 `spawn_draft_flush` 的范式）。
//!
//! 这不是优化而是刚性要求：`wind-store/AGENTS.md` 明写 redb 读缓存只涨不落、
//! 绝不能进按键链路；裁剪还是**全表规模**操作。落库按**会话**成批（一次写事务落一段话），
//! 而非按按键。
//!
//! # 隐私
//!
//! 本模块处理的字符串是**用户原始输入**。除错误路径外不打任何日志，且错误信息**不带文本**
//! （根 `AGENTS.md`：INFO 级不得含用户输入）。

use crate::coordinator::Coordinator;
use wind_bridge::handler::KeyAction;
use wind_store::input_history::InputSegmentRecord;

use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, warn};

/// 待落库会话队列的硬上限。后台线程若长时间没跑起来（如 redb 被暂停），队列不能无限涨。
///
/// 丢最早的：它们离用户当前输入最远、最不可能被恢复到。与 `DRAFT_QUEUE_MAX` 同一取舍。
const INPUT_HISTORY_QUEUE_MAX: usize = 1024;

/// 单个会话累积的片段数上限。防止极端情况下（长时间不停顿地连打）一个会话的
/// 内存占用与单次写事务规模失控。达到上限即就地封段。
const INPUT_HISTORY_SEGMENTS_MAX: usize = 4096;

/// 一个**已封口**的会话，等待落库。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClosedSession {
    pub(crate) start_ms: i64,
    pub(crate) end_ms: i64,
    pub(crate) text: String,
    pub(crate) segments: Vec<InputSegmentRecord>,
}

/// 当前会话的累积缓冲（纯内存，无 IO）。
///
/// 独立成结构体而不是把几个字段摊在 `Coordinator` 上：断段与封口是**纯逻辑**，
/// 摊开之后就再也单测不到（要造一个完整 coordinator 才能验证「停顿会不会断段」）。
#[derive(Debug, Default)]
pub(crate) struct InputHistoryBuf {
    start_ms: i64,
    last_ms: i64,
    text: String,
    segments: Vec<InputSegmentRecord>,
}

impl InputHistoryBuf {
    pub(crate) fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    /// 追加一个落屏片段。返回是否需要**先封段**（调用方据此把旧会话入队）。
    ///
    /// `idle_break_ms`：与上一片段的间隔超过它即视为新的一段。判据是**严格大于**——
    /// 恰好等于阈值时仍算同一段，免得边界值的行为依赖浮点/取整细节。
    ///
    /// 达到 [`INPUT_HISTORY_SEGMENTS_MAX`] 时也返回 `true`：就地封段，
    /// 让下一次追加从新会话开始，避免单个会话无界增长。
    pub(crate) fn push(&mut self, text: &str, now_ms: i64, idle_break_ms: i64) -> bool {
        if text.is_empty() {
            return false;
        }
        let need_break = self.is_empty()
            || now_ms.saturating_sub(self.last_ms) > idle_break_ms
            || self.segments.len() >= INPUT_HISTORY_SEGMENTS_MAX;
        if need_break && !self.is_empty() {
            // 调用方先 `take()` 再 `push`，这里只报告「该断了」。
            return true;
        }
        if self.is_empty() {
            self.start_ms = now_ms;
        }
        self.last_ms = now_ms;
        self.text.push_str(text);
        self.segments.push(InputSegmentRecord {
            text: text.to_string(),
            at_ms: now_ms,
        });
        false
    }

    /// 封口并取走当前会话。空缓冲返回 `None`。
    pub(crate) fn take(&mut self) -> Option<ClosedSession> {
        if self.is_empty() {
            return None;
        }
        let s = ClosedSession {
            start_ms: self.start_ms,
            end_ms: self.last_ms,
            text: std::mem::take(&mut self.text),
            segments: std::mem::take(&mut self.segments),
        };
        self.start_ms = 0;
        self.last_ms = 0;
        Some(s)
    }

    /// **只读快照**（不取走）。给召回入口把「还没落库的那段」列出来用。
    ///
    /// 为什么需要它：会话只有在下一次上屏判过 idle、或断流事件时才封口入队、再由后台线程落库。
    /// 用户「打完一段、立刻想找回」时，那段**还在这个缓冲里**——纯读库的候选列表看不见它，
    /// 于是最需要它的时刻恰恰是空的。快照让它无需写盘就能被列出。
    pub(crate) fn snapshot(&self) -> Option<ClosedSession> {
        if self.is_empty() {
            return None;
        }
        Some(ClosedSession {
            start_ms: self.start_ms,
            end_ms: self.last_ms,
            text: self.text.clone(),
            segments: self.segments.clone(),
        })
    }
}

/// 当前 Unix 毫秒。
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl Coordinator {
    /// 输入历史是否启用。默认开（配置四处同步见 `wind-config` 的 `input.history.*`）。
    pub(crate) fn input_history_enabled(&self) -> bool {
        self.rt().config.input.history.enabled
    }

    /// 会话断点阈值（毫秒）。`0` 表示「用默认 3000」——与
    /// `schema.codetable.auto_phrase.idle_timeout_ms` 的 `0 = 用默认` 同一约定。
    pub(crate) fn input_history_idle_break_ms(&self) -> i64 {
        let ms = self.rt().config.input.history.idle_break_ms;
        if ms == 0 { 3000 } else { i64::from(ms) }
    }

    /// 上屏后把文字喂进输入历史。由 [`Coordinator::note_commit_action`] 统一调用。
    ///
    /// 判据见模块文档的表格；与同处的 `note_draft_commit` **刻意不同源**。
    pub(crate) fn note_input_history_commit(&self, action: &KeyAction) {
        if !self.input_history_enabled() {
            return;
        }
        let text = match action {
            KeyAction::InsertText { text, .. } | KeyAction::InsertTextWithCursor { text, .. } => {
                text.as_str()
            }
            KeyAction::CommitAndHoldComposition { commit_text, .. }
            | KeyAction::CommitThenDeferComposition { commit_text, .. } => commit_text.as_str(),
            // `ReplaceBackward`（回改已上屏内容）与组合态（尚未落屏）都不计入。
            _ => return,
        };
        if text.is_empty() {
            return;
        }
        let idle = self.input_history_idle_break_ms();
        let now = now_ms();
        let closed = {
            let mut buf = self
                .input_history_buf
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if buf.push(text, now, idle) {
                buf.take()
            } else {
                None
            }
        };
        // 先封旧段再记新段：`push` 报告该断时它自己没写入，故这里补一次。
        if let Some(s) = closed {
            self.enqueue_input_history(s);
            let mut buf = self
                .input_history_buf
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            buf.push(text, now, idle);
        }
        self.spawn_input_history_flush();
    }

    /// 断流：把当前会话封口入队并起 flush。挂在
    /// [`Coordinator::terminate_auto_phrase`]（回车/焦点丢失/IME 停用/模式切换/切换方案/光标移动）。
    pub(crate) fn terminate_input_history(&self, reason: &str) {
        if !self.input_history_enabled() {
            return;
        }
        let closed = {
            let mut buf = self
                .input_history_buf
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            buf.take()
        };
        let Some(s) = closed else { return };
        debug!("input-history: 断段（{reason}）");
        self.enqueue_input_history(s);
        self.spawn_input_history_flush();
    }

    fn enqueue_input_history(&self, s: ClosedSession) {
        let mut q = self
            .input_history_queue
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        q.push(s);
        if q.len() > INPUT_HISTORY_QUEUE_MAX {
            let drop = q.len() - INPUT_HISTORY_QUEUE_MAX;
            q.drain(..drop);
            warn!("input-history: 队列超上限，丢弃最早的 {drop} 条（后台线程没跟上？）");
        }
    }

    /// 把队列里的会话**同步**落库（含裁剪）。返回成功写入的条数。
    ///
    /// 生产路径走 [`Self::spawn_input_history_flush`]；本函数给两处用：
    /// ① 后台线程内的实际写入；② 单测——测试不该去赛跑一个线程，那会让失败变成偶发。
    pub(crate) fn flush_input_history_blocking(&self) -> usize {
        let batch: Vec<ClosedSession> = {
            let mut q = self
                .input_history_queue
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if q.is_empty() {
                return 0;
            }
            std::mem::take(&mut *q)
        };
        let Some(store) = self.store.as_ref() else {
            return 0;
        };
        let mut written = 0usize;
        for s in &batch {
            match store.append_input_session(s.start_ms, s.end_ms, &s.text, &s.segments) {
                Ok(Some(_)) => written += 1,
                // `None` = 空文本或超长（存储层静默忽略），不是错误。
                Ok(None) => {}
                Err(e) => debug!("input-history: 落库失败: {e}"),
            }
        }
        self.prune_input_history_if_needed(store);
        written
    }

    /// 超限才裁剪。先问 `count_input_sessions`（表元数据，O(1)），避免每次都做全表规模的事。
    fn prune_input_history_if_needed(&self, store: &std::sync::Arc<wind_store::Store>) {
        let max = self.rt().config.input.history.max_sessions as usize;
        if max == 0 {
            return;
        }
        match store.count_input_sessions() {
            Ok(n) if n > max => {
                if let Err(e) = store.prune_input_history(max) {
                    debug!("input-history: 裁剪失败: {e}");
                }
            }
            Ok(_) => {}
            Err(e) => debug!("input-history: 计数失败: {e}"),
        }
    }

    /// 起一个后台线程把队列里的会话落库。
    ///
    /// 已有线程在跑时直接返回（`input_history_flushing` 这道闸，同 `draft_flushing`）：
    /// 没有它，每次上屏都会 spawn 一个新线程去抢同一把 redb 写锁。
    /// 线程内**循环到队列空**——flush 期间用户还在打字，新会话会继续入队。
    pub(crate) fn spawn_input_history_flush(&self) {
        if !self.input_history_enabled() {
            return;
        }
        {
            let q = self
                .input_history_queue
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if q.is_empty() {
                return;
            }
        }
        if self
            .input_history_flushing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return; // 已有线程在跑，它会把队列里新增的一并带走
        }
        let Some(weak) = self.self_weak.get().cloned() else {
            // `self_weak` 由 `init_cmdbar` 在 `build` 里装配，所有构造器（含 headless）都经过它。
            // 走到这里说明装配被改坏了——**降级为同步落库**而不是丢数据：这条路径只在
            // 异常装配下出现，一次同步写可接受，丢用户输入不可接受。
            warn!("input-history: self_weak 未装配，改为同步落库");
            self.flush_input_history_blocking();
            self.input_history_flushing.store(false, Ordering::Release);
            return;
        };
        let spawned = std::thread::Builder::new()
            .name("input-history-flush".into())
            .spawn(move || {
                loop {
                    let Some(c) = weak.upgrade() else { break };
                    if c.flush_input_history_blocking() == 0 {
                        break;
                    }
                    // 队列已被取空：循环再判一次，把 flush 期间新入队的一并带走。
                    let empty = {
                        let q = c
                            .input_history_queue
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        q.is_empty()
                    };
                    if empty {
                        break;
                    }
                }
                if let Some(c) = weak.upgrade() {
                    c.input_history_flushing.store(false, Ordering::Release);
                }
            });
        if spawned.is_err() {
            // spawn 失败同样降级同步，别把已入队的会话留在内存里等丢。
            self.input_history_flushing.store(false, Ordering::Release);
            warn!("input-history: flush 线程启动失败，改为同步落库");
            self.flush_input_history_blocking();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg_texts(b: &InputHistoryBuf) -> Vec<String> {
        b.segments.iter().map(|s| s.text.clone()).collect()
    }
    #[test]
    fn accumulates_into_one_session() {
        let mut b = InputHistoryBuf::default();
        assert!(!b.push("你好", 1000, 3000));
        assert!(!b.push("世界", 1500, 3000));
        assert_eq!(b.text, "你好世界");
        assert_eq!(seg_texts(&b), vec!["你好", "世界"]);
        assert!(!b.is_empty());
    }

    /// ★ 严格大于阈值才断段：恰好等于阈值仍算同一段（边界值不依赖取整细节）。
    #[test]
    fn idle_break_is_strictly_greater_than_threshold() {
        let mut b = InputHistoryBuf::default();
        b.push("甲", 1000, 3000);
        assert!(!b.push("乙", 4000, 3000), "恰好相差 3000 不该断");
        assert!(b.push("丙", 7001, 3000), "相差 3001 该断");
    }

    #[test]
    fn take_returns_session_and_resets() {
        let mut b = InputHistoryBuf::default();
        b.push("你好", 1000, 3000);
        b.push("世界", 1200, 3000);
        let s = b.take().expect("非空应有会话");
        assert_eq!(s.start_ms, 1000);
        assert_eq!(s.end_ms, 1200);
        assert_eq!(s.text, "你好世界");
        assert_eq!(s.segments.len(), 2);
        assert!(b.is_empty(), "取走后必须复位");
        assert!(b.take().is_none(), "空缓冲再取为 None");
        // 复位后新一段从新起点开始，不残留上一段的时间戳
        b.push("新段", 9000, 3000);
        let s2 = b.take().unwrap();
        assert_eq!(s2.start_ms, 9000);
        assert_eq!(s2.text, "新段");
    }

    #[test]
    fn empty_text_is_ignored_and_does_not_start_a_session() {
        let mut b = InputHistoryBuf::default();
        assert!(!b.push("", 1000, 3000));
        assert!(b.is_empty());
        assert!(b.take().is_none());
    }

    /// ★ 轨迹上的坑：会话**非空**且该断段时，`push` 必须报告断段而**不写入**，
    /// 由调用方 `take()` 后补记——否则那个新片段会被吃进旧会话里，丢掉一次上屏。
    #[test]
    fn push_reports_break_without_writing() {
        let mut b = InputHistoryBuf::default();
        b.push("旧", 1000, 3000);
        assert!(b.push("新", 9000, 3000), "应报告断段");
        assert_eq!(b.text, "旧", "报告断段时不得写入新片段");
        let s = b.take().unwrap();
        assert_eq!(s.text, "旧");
        assert!(b.is_empty());
    }

    /// 达到片段上限即封段，防单个会话无界增长。
    #[test]
    fn segment_cap_forces_break() {
        let mut b = InputHistoryBuf::default();
        for i in 0..INPUT_HISTORY_SEGMENTS_MAX {
            assert!(!b.push("字", 1000 + i as i64, 3000), "未到上限不该断");
        }
        assert!(b.push("再来", 9999, 3000), "到上限该断");
        assert_eq!(b.segments.len(), INPUT_HISTORY_SEGMENTS_MAX);
    }

    // ─────────────────── 以下为接线级测试（走完整 Coordinator + 真实 redb） ───────────────────

    use crate::coordinator::Coordinator;
    use std::sync::Arc;
    use wind_config::Config;
    use wind_store::Store;

    /// 造一个带临时库的无头 Coordinator。`idle_ms` 可调小以便在毫秒级验证断段。
    /// 临时库落在 `std::env::temp_dir()`，**不写工作区**（仓内惯例）。
    fn unique_path(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "wind_inhist_{tag}_{}_{:?}.redb",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    /// 在**指定路径**上造 Coordinator——两次打开同一个库，用于验证「关闭开关不删已有记录」。
    fn coord_at(
        path: &std::path::Path,
        enabled: bool,
        idle_ms: u32,
        max_sessions: u32,
    ) -> Arc<Coordinator> {
        let store = Arc::new(Store::open(path).unwrap());
        let mut cfg = Config::default();
        cfg.input.history.enabled = enabled;
        cfg.input.history.idle_break_ms = idle_ms;
        cfg.input.history.max_sessions = max_sessions;
        Coordinator::new_headless_with_store(cfg, None, store)
    }

    fn coord(tag: &str, enabled: bool, idle_ms: u32, max_sessions: u32) -> Arc<Coordinator> {
        coord_at(&unique_path(tag), enabled, idle_ms, max_sessions)
    }

    /// 等后台 flush 线程收工，再兜一次同步落库——让断言不赛跑线程（否则失败会变成偶发）。
    fn settle(c: &Coordinator) {
        for _ in 0..400 {
            let empty = c
                .input_history_queue
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty();
            let busy = c.input_history_flushing.load(Ordering::Acquire);
            if empty && !busy {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        c.flush_input_history_blocking();
    }

    fn insert(text: &str) -> KeyAction {
        KeyAction::InsertText {
            text: text.to_string(),
            new_composition: None,
            mode_changed: false,
            chinese_mode: true,
            has_new_composition: false,
        }
    }

    fn commit_then_defer(commit: &str) -> KeyAction {
        KeyAction::CommitThenDeferComposition {
            commit_text: commit.to_string(),
            deferred_composition: String::new(),
            timeout_ms: 0,
        }
    }

    fn commit_and_hold(commit: &str) -> KeyAction {
        KeyAction::CommitAndHoldComposition {
            commit_text: commit.to_string(),
            hold_text: "。".to_string(),
            timeout_ms: 0,
        }
    }

    /// 取库里全部会话（时间倒序）。
    fn sessions(c: &Coordinator) -> Vec<wind_store::input_history::InputSessionRecord> {
        c.store
            .as_ref()
            .unwrap()
            .list_input_sessions(0, 0)
            .unwrap()
            .0
            .into_iter()
            .map(|(_, r)| r)
            .collect()
    }

    fn segs(c: &Coordinator, session_key: &str) -> Vec<String> {
        c.store
            .as_ref()
            .unwrap()
            .list_input_segments(session_key)
            .unwrap()
            .into_iter()
            .map(|s| s.text)
            .collect()
    }

    /// ★★ C1 的核心：**逐字比对**「实际打的字」与「记录中出现的字」。
    ///
    /// 三种计入变体各喂一段，最后断言**拼接结果与片段序列与被喂文本逐字相等**，
    /// 而不是只看「库里有没有东西」。
    #[test]
    fn typed_text_is_recorded_verbatim() {
        let c = coord("verbatim", true, 3000, 500);
        // 逐字构造「你好世界式大」：五种计入变体里挑三种各喂一部分
        c.note_commit_action(&insert("你好")); // InsertText
        c.note_commit_action(&KeyAction::InsertTextWithCursor {
            text: "世界".to_string(),
            cursor_offset: 0,
        }); // InsertTextWithCursor
        c.note_commit_action(&commit_and_hold("式")); // CommitAndHoldComposition
        c.note_commit_action(&commit_then_defer("大")); // CommitThenDeferComposition（五笔满码自动上屏走这条）
        c.terminate_input_history("test");
        settle(&c);

        let ss = sessions(&c);
        assert_eq!(ss.len(), 1, "连续上屏应归成一个会话，实际 {}", ss.len());
        let expected = "你好世界式大";
        assert_eq!(ss[0].text, expected, "★ 整段文本必须与实际打的字逐字相等");
        assert_eq!(ss[0].segments, 4, "片段数应等于上屏次数");
        let key = c
            .store
            .as_ref()
            .unwrap()
            .list_input_sessions(0, 0)
            .unwrap()
            .0[0]
            .0
            .clone();
        assert_eq!(
            segs(&c, &key),
            vec!["你好", "世界", "式", "大"],
            "★ 片段序列必须逐字相等且保持落屏先后"
        );
    }

    /// ★ 五笔满码自动上屏走 `CommitThenDeferComposition`，**必须入库**。
    /// 漏掉它恰好打在本功能靶心上：自动上屏的字不入库，恢复出的长消息会**残缺**。
    #[test]
    fn commit_then_defer_is_recorded() {
        let c = coord("defer", true, 3000, 500);
        c.note_commit_action(&commit_then_defer("大"));
        c.terminate_input_history("test");
        settle(&c);
        let ss = sessions(&c);
        assert_eq!(ss.len(), 1);
        assert_eq!(ss[0].text, "大", "自动上屏的字必须逐字入库");
    }

    /// `ReplaceBackward`（回改已上屏内容）与组合态（尚未落屏）都**不得**入库。
    #[test]
    fn replace_backward_and_composition_states_are_not_recorded() {
        let c = coord("excl", true, 3000, 500);
        c.note_commit_action(&KeyAction::ReplaceBackward {
            count: 1,
            text: "改".to_string(),
        });
        c.note_commit_action(&KeyAction::UpdateComposition {
            text: "nihao".to_string(),
            caret_pos: 0,
        });
        c.note_commit_action(&KeyAction::HoldComposition {
            text: "。".to_string(),
            timeout_ms: 0,
        });
        c.note_commit_action(&KeyAction::PassThrough);
        c.terminate_input_history("test");
        settle(&c);
        assert!(
            sessions(&c).is_empty(),
            "回改与组合态都不该产生记录，实际 {:?}",
            sessions(&c)
        );
        assert_eq!(
            c.store.as_ref().unwrap().count_input_sessions().unwrap(),
            0,
            "库里必须一条都没有"
        );
    }

    /// ★ C4 运行时半边：开关**关闭**后上屏不再产生新记录。
    #[test]
    fn disabled_switch_records_nothing() {
        let c = coord("off", false, 3000, 500);
        assert!(!c.input_history_enabled(), "前提：本次构造是关闭态");
        c.note_commit_action(&insert("不该被记录"));
        c.note_commit_action(&commit_then_defer("也不该"));
        c.terminate_input_history("test");
        settle(&c);
        assert!(
            sessions(&c).is_empty(),
            "关闭后不得产生任何记录，实际 {}",
            sessions(&c).len()
        );
    }

    /// ★ C4：关闭开关**不删除**已有记录（开关语义是「不再产生新记录」，不是销毁数据）。
    ///
    /// 在**同一个 Coordinator / 同一个库连接**上换配置位：redb 是单写者，同一文件二次
    /// `Store::open` 会因文件锁失败（那是脚手架问题，与记录逻辑无关）。
    /// 换配置走 [`Coordinator::set_config_for_test`]，它镜像生产的热重载 bundle 重建路径。
    #[test]
    fn disabling_does_not_delete_existing_records() {
        let c = coord("keep", true, 3000, 500);
        c.note_commit_action(&insert("先记下这句"));
        c.terminate_input_history("test");
        settle(&c);
        assert_eq!(sessions(&c).len(), 1, "前提：开时确实记了一条");

        c.set_config_for_test(|cfg| cfg.input.history.enabled = false);
        assert!(!c.input_history_enabled(), "前提：开关已关");

        c.note_commit_action(&insert("关掉之后打的"));
        c.note_commit_action(&insert("这句也不该进库"));
        c.terminate_input_history("test");
        settle(&c);

        let ss = sessions(&c);
        assert_eq!(ss.len(), 1, "关闭后不该新增，实际 {}", ss.len());
        assert_eq!(ss[0].text, "先记下这句", "既有记录必须原样保留");
    }

    /// ★ 断流事件（回车/焦点丢失/IME 停用/模式切换等共用的终止入口）把会话封口成新的一段。
    #[test]
    fn terminate_splits_the_session() {
        let c = coord("split", true, 3000, 500);
        c.note_commit_action(&insert("第一段"));
        c.terminate_input_history("回车");
        c.note_commit_action(&insert("第二段"));
        c.terminate_input_history("焦点丢失");
        settle(&c);
        let ss = sessions(&c);
        assert_eq!(ss.len(), 2, "断流应产生两段，实际 {}", ss.len());
        // 时间倒序：最新的在前
        assert_eq!(ss[0].text, "第二段");
        assert_eq!(ss[1].text, "第一段");
    }

    /// ★ 停顿超过 `idle_break_ms` 即断段（阈值调到 1ms，用 20ms 间隔触发）。
    #[test]
    fn idle_gap_splits_the_session() {
        let c = coord("idle", true, 1, 500);
        c.note_commit_action(&insert("停顿前"));
        std::thread::sleep(std::time::Duration::from_millis(20));
        c.note_commit_action(&insert("停顿后"));
        c.terminate_input_history("test");
        settle(&c);
        let ss = sessions(&c);
        assert_eq!(ss.len(), 2, "超阈值停顿应断成两段，实际 {}", ss.len());
        assert_eq!(ss[0].text, "停顿后");
        assert_eq!(ss[1].text, "停顿前");
    }

    /// ★ C4：超过 `max_sessions` 时**最旧的**被淘汰（按时间淘汰，不是按使用次数）。
    #[test]
    fn oldest_sessions_are_pruned() {
        let c = coord("prune", true, 3000, 2);
        for i in 0..4 {
            c.note_commit_action(&insert(&format!("第{i}段")));
            c.terminate_input_history("test");
            settle(&c);
        }
        let ss = sessions(&c);
        assert_eq!(ss.len(), 2, "上限 2，实际 {}", ss.len());
        let texts: Vec<&str> = ss.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, vec!["第3段", "第2段"], "必须留下最新的两条");
    }

    /// 默认配置下开关是**开**的（C4 的配置半边在 coordinator 侧也成立）。
    #[test]
    fn default_config_is_enabled() {
        let c = coord(
            "default",
            Config::default().input.history.enabled,
            3000,
            500,
        );
        assert!(c.input_history_enabled(), "默认必须开启");
        c.note_commit_action(&insert("默认就记"));
        c.terminate_input_history("test");
        settle(&c);
        assert_eq!(sessions(&c).len(), 1);
    }
}
