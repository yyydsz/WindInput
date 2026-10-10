//! 输入历史恢复模式（`ModeKind::InputHistory`）：按时间倒序列出最近的输入，挑片段拼回。
//!
//! # 它解决什么
//!
//! 打完一段字、还没复制就发送失败，内容没了。采集端（`crate::input_history`）已把
//! 「什么时候打过什么」按会话 + 片段两级落进本地库，本模块是**取回来**的那一端。
//!
//! # 候选来源：**先内存、后读库**
//!
//! ⚠️ 只读库是不够的，这一点由真机实测暴露：会话只有在下一次上屏判过 idle、或断流事件
//! （回车/换焦点/切输入法/切方案）时才封口入队，再由后台线程落库。用户「打完一段、
//! 立刻想找回」时，那段**还在内存里**——纯读库的列表看不见它，于是最需要它的时刻恰恰是空的。
//!
//! 故会话层按「内存未落库的在前、库里的在后」拼接：
//! - `live` —— 正在累积的那段（`InputHistoryBuf::snapshot`，只读不取走）
//! - `queue:{start_ms}` —— 已封口、后台线程还没写进库的那段
//! - 库里的会话键 —— 走 `list_input_sessions`
//!
//! 三者互斥（封口把会话从缓冲搬到队列、flush 再从队列搬进库），故**不会重复计两条**。
//!
//! ## ⚠️ 已披露限制：在途窗口
//!
//! `spawn_input_history_flush` 的写法是「先把队列整个 `take` 走、再逐条写库」。夹在
//! 「已 take、未写完」之间的那段，**三处都看不见**（缓冲没有、队列没有、库里还没有）。
//! **可复现触发**：按回车（封口入队）后**立刻**按恢复入口键。窗口是毫秒级的，正常手速
//! 很难撞上，但它是真实存在的，故在此明写；彻底消除要把队列改成「写成功才移除」。
//!
//! 内存部分不写盘、不在按键链路做 IO，符合 `wind-store/AGENTS.md` 对按键路径的硬约束。
//!
//! # 两级列表，用一个状态位切
//!
//! | 层级 | `special_buffer` | 候选 | 选中后 |
//! |---|---|---|---|
//! | 会话 | 空 | 每个会话一条（时间倒序） | 整段插入 |
//! | 分词 | 会话键 | 该会话的各片段（按落屏先后） | 只插入那一段 |
//!
//! 复用 `special_buffer` 兼作层级位而不是给 `State` 加字段：它与「本模式当前的输入缓冲」
//! 语义同构（本模式不吃编码），且 `exit_special_mode` 已经会复位它。
//!
//! # 取用后**留在模式里**
//!
//! 这是本模式与其它所有模式最大的不同：它们选中即退出，而本模式的用法就是**连续挑几条拼回**。
//! 故取用走 `commit_then_new_composition`——把选中的文本插入宿主，同时**保持组合态**，
//! 候选窗不关、列表位置不动（用户接着挑下一条）。按 Esc 才退出。
//!
//! # 为什么不用快捷输入历史那条路
//!
//! 见 [`crate::pipeline::ModeKind::InputHistory`] 的文档：`quick_history_candidates` 在
//! 已有分步上屏段时返回空（与「连续选多条」冲突），且按次数而非时间排序（一条只打过一次的
//! 长消息会垫底）。那是为「按前缀补全一个标识符」设计的，不是为「找回上一段话」。
//!
//! # 隐私
//!
//! 候选文本是**用户原始输入**，只进候选窗，不进日志（根 `AGENTS.md`）。

use crate::coordinator::{Coordinator, State};
use crate::pipeline::ModeKind;
use tracing::debug;
use wind_bridge::handler::{KeyAction, KeyEventData};
use wind_candidate::Candidate;
use wind_keys::keymap;

/// 历史候选的 `Candidate::id` 标记。上屏出口凭它认出「这是一条输入历史」。
pub(crate) const INPUT_HISTORY_ID: &str = "input_history";

/// 一次最多给几条。会话数由 `input.history.max_sessions` 封顶，这里再截一刀是为了
/// 候选窗只显示一页——恢复场景里「上一条/上上条」就够，翻十几页找一条更慢。
pub(crate) const INPUT_HISTORY_QUOTA: usize = 20;

/// 组合区显示的固定标签（本模式不吃编码，没有缓冲可显示）。
pub(crate) const INPUT_HISTORY_LABEL: &str = "输入历史";

/// 「正在累积、尚未落库」那一段在候选 id 里的键。
pub(crate) const PENDING_LIVE_KEY: &str = "live";

/// 「已封口、还没写进库」那些会话在候选 id 里的键前缀，后面接 `start_ms`。
pub(crate) const PENDING_QUEUE_PREFIX: &str = "queue:";

impl Coordinator {
    /// 本模式能不能进：存储开关开着就行。
    ///
    /// 不给它单独的总开关——「不记就无从恢复」，存储关了本模式没有意义。
    /// 与 `reverse_mode_available` 同一条处置：门卫没过时调用方让触发键落普通输入，绝不吞键。
    pub(crate) fn input_history_mode_available(&self) -> bool {
        self.input_history_enabled()
    }

    /// 这个键是否绑了「输入历史」动词（三层链：方案 `[key_actions]` → 全局 → `z_key_action`）。
    ///
    /// ⚠️ 当前**没有调用点**，故不保留同名函数：进入本模式走的是通用动词分派
    /// （`run_bound_action` 的 `BoundAction::InputHistory` 臂），不需要单独的「是不是触发键」
    /// 判据——`reverse` 有那个判据是因为它还要给启动体检门控用（`reverse_bound_anywhere`）。
    /// 留一个没人调的函数只会腐烂，本仓对死代码的处置是删掉而不是 `allow(dead_code)`。

    /// 进入本模式。字段组与 `enter_reverse_mode` 同一组（本模式也没有宿主方案）。
    pub(crate) fn enter_input_history_mode(&self, state: &mut State, key_code: u32) -> KeyAction {
        state.input_buffer.clear();
        state.candidates.clear();
        state.active = Some(ModeKind::InputHistory);
        state.special_id = 0;
        state.overlay_spec = None;
        // 层级位：空 = 会话列表（见模块文档）
        state.special_buffer.clear();
        state.special_cursor = 0;
        state.special_prefix = keymap::vk_to_prefix_char_with_letters(key_code)
            .map(|c| c.to_string())
            .unwrap_or_default();
        state.scope_relaxed = false;
        self.update_input_history_candidates(state);
        self.notify_ui_update(state);
        let display = state.preedit.clone();
        debug!("Entered input-history mode");
        self.reverse_entry_composition(key_code, display)
    }

    /// 顶掉当前半成品并进入本模式（同 `commit_and_enter_reverse_mode`）。
    pub(crate) fn commit_and_enter_input_history_mode(
        &self,
        state: &mut State,
        key_code: u32,
    ) -> KeyAction {
        let committed = self.take_committed_with_highlight(state);
        let enter = self.enter_input_history_mode(state, key_code);
        match committed {
            Some(text) => {
                let new_comp = match &enter {
                    KeyAction::UpdateComposition { text, .. } => text.clone(),
                    _ => state.preedit.clone(),
                };
                self.commit_then_new_composition(text, new_comp)
            }
            None => enter,
        }
    }

    /// 数字/序号选词键取用第 `gi` 条（全局下标）。
    ///
    /// 与空格臂**同语义**：插入后留在模式里，列表不动，用户接着挑下一条。
    pub(crate) fn commit_input_history_selected(&self, state: &mut State, gi: usize) -> KeyAction {
        match state.candidates.get(gi) {
            Some(c) if c.id.starts_with(INPUT_HISTORY_ID) && !c.text.is_empty() => {
                let text = c.text.clone();
                let comp = state.preedit.clone();
                self.commit_then_new_composition(text, comp)
            }
            _ => KeyAction::Consumed,
        }
    }

    /// 重建候选：按 `special_buffer` 是否为空决定列会话还是列该会话的分词。
    pub(crate) fn update_input_history_candidates(&self, state: &mut State) {
        self.reset_candidate_view(state);
        state.candidates = self.input_history_candidates(state);
        // 组合区显示固定标签：本模式不吃编码，没有「缓冲」可显示；层级由候选内容自明。
        state.preedit = format!("{}{}", state.special_prefix, INPUT_HISTORY_LABEL);
        state.has_more = false;
    }

    /// **内存中尚未落库**的会话，**最新在前**：先正在累积的那段（live），再已封口待写的队列。
    ///
    /// 存在的理由见 [`crate::input_history::InputHistoryBuf::snapshot`] 的文档：用户打完立刻
    /// 想找回时，那段还没进库，**纯读库的候选列表看不见它**。两者都不写盘、不在按键链路做 IO。
    ///
    /// 键约定（`special_buffer` 兼作层级位，分词层据此取片段）：
    /// - `live` —— 正在累积的那段
    /// - `queue:{start_ms}` —— 已封口但还没写进库的那段
    /// - 其它 —— 库里的会话键（`{start_ms:020}\0{seq:010}`），走 `list_input_segments`
    pub(crate) fn pending_input_history_sessions(
        &self,
    ) -> Vec<(String, crate::input_history::ClosedSession)> {
        let mut out = Vec::new();
        {
            let buf = self
                .input_history_buf
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(s) = buf.snapshot() {
                out.push((PENDING_LIVE_KEY.to_string(), s));
            }
        }
        {
            let q = self
                .input_history_queue
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            // 队列是 push 追加 ⇒ 最新在尾部，倒序遍历即「最新在前」
            for s in q.iter().rev() {
                out.push((format!("{PENDING_QUEUE_PREFIX}{}", s.start_ms), s.clone()));
            }
        }
        out
    }

    /// 按当前层级取候选。**先内存、后读库**（内存那部分不写盘、不改视图）。
    fn input_history_candidates(&self, state: &State) -> Vec<Candidate> {
        let cand = |text: String, id: &str| Candidate {
            text,
            id: id.to_string(),
            ..Default::default()
        };

        if state.special_buffer.is_empty() {
            // 会话层：**内存里尚未落库的排在库前面**（它们更新），各自按时间倒序。
            let mut out: Vec<Candidate> = self
                .pending_input_history_sessions()
                .into_iter()
                .filter(|(_, s)| !s.text.is_empty())
                // 内存部分**也封顶**：后台 flush 若卡住，队列可长到 `INPUT_HISTORY_QUEUE_MAX`
                // （1024），而每条候选还要 clone 文本与片段。用户看的是「最近几条」，
                // 没有理由把上千条全搬进候选列表——那既撑大按键路径，也淹没真正要找的那段。
                .take(INPUT_HISTORY_QUOTA)
                .map(|(key, s)| cand(s.text, &format!("{INPUT_HISTORY_ID}\u{0}{key}")))
                .collect();
            if let Some(store) = self.store.as_ref() {
                match store.list_input_sessions(0, INPUT_HISTORY_QUOTA) {
                    Ok((rows, _)) => out.extend(rows.into_iter().map(|(key, rec)| {
                        // 片段层要用会话键，故把它挂在候选 id 上（`text` 是给用户看的整段文本）
                        cand(rec.text, &format!("{INPUT_HISTORY_ID}\u{0}{key}"))
                    })),
                    Err(e) => debug!("input-history: 会话读取失败: {e}"),
                }
            }
            out
        } else {
            // 分词层：该会话的各片段，**按落屏先后**（拼回时要顺着读，不倒序）
            let key = state.special_buffer.clone();
            if key == PENDING_LIVE_KEY || key.starts_with(PENDING_QUEUE_PREFIX) {
                // 内存会话自带片段，直接给——否则按 `→` 会没反应、看着像坏了
                return self
                    .pending_input_history_sessions()
                    .into_iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, s)| {
                        s.segments
                            .into_iter()
                            .map(|seg| cand(seg.text, INPUT_HISTORY_ID))
                            .collect()
                    })
                    .unwrap_or_default();
            }
            let Some(store) = self.store.as_ref() else {
                return Vec::new();
            };
            match store.list_input_segments(&key) {
                Ok(rows) => rows
                    .into_iter()
                    .map(|s| cand(s.text, INPUT_HISTORY_ID))
                    .collect(),
                Err(e) => {
                    debug!("input-history: 片段读取失败: {e}");
                    Vec::new()
                }
            }
        }
    }

    /// 本模式内的按键处理。
    ///
    /// 键角色：`Esc` 退一层（分词层 → 会话层；会话层 → 退出模式）、方向键 / 翻页走统一入口
    /// [`Coordinator::handle_candidate_nav`]（`handle_candidate.rs`，内部
    /// `apply_session_action`；禁在本模式另写一套）、`VK_RIGHT` 展开为分词、
    /// **空格 / 回车 / 数字键**取用当前高亮，**其余键一律吞掉**。
    ///
    /// ⚠️ 根 AGENTS.md 里点名的 `apply_nav_key` 在当前代码中已不存在（该引用已过期），
    /// 上一条的 `handle_candidate_nav` 才是实际入口。
    pub(crate) fn handle_input_history_key(
        &self,
        state: &mut State,
        data: &KeyEventData,
    ) -> KeyAction {
        use wind_keys::keymap::{VK_ESCAPE, VK_RIGHT};

        if data.key_code == VK_ESCAPE {
            if state.special_buffer.is_empty() {
                self.exit_special_mode(state);
                return KeyAction::ClearComposition;
            }
            // 分词层 → 会话层：清层级位并重建
            state.special_buffer.clear();
            self.update_input_history_candidates(state);
            self.notify_ui_update(state);
            return KeyAction::UpdateComposition {
                text: state.preedit.clone(),
                caret_pos: 0,
            };
        }

        // 展开：把高亮的会话切成它的分词列表（仅供会话层用）
        if data.key_code == VK_RIGHT && state.special_buffer.is_empty() {
            if let Some(key) = self.highlighted_session_key(state) {
                state.special_buffer = key;
                self.update_input_history_candidates(state);
                self.notify_ui_update(state);
            }
            return KeyAction::UpdateComposition {
                text: state.preedit.clone(),
                caret_pos: 0,
            };
        }

        // 翻页 / 高亮移动走**统一入口** `handle_candidate_nav`（内部 `apply_session_action`），
        // 不在本模式另写一套——那正是「同一套翻页键在某个模式里表现不同」的来源。
        // ⚠️ 根 AGENTS.md 里点名的 `apply_nav_key` 在当前代码中已不存在（该引用已过期），
        // 实际入口是 `handle_candidate_nav`。
        if let Some(act) = self.handle_candidate_nav(state, data) {
            self.notify_ui_update(state);
            return act;
        }

        // ★ 取用闸：**只有空格 / 回车 / 数字键**才把高亮项写进宿主。
        //
        // 这道闸不是装饰。`take_highlighted_input_history` 只看候选 id 前缀、**不看按键**，
        // 少了它，候选非空时任何未被上面消费的键（字母、标点、退格…）都会触发一次插入——
        // 连按即把同一段历史反复打进宿主，而历史里装的正是用户刚丢的正文。
        if !matches!(
            data.key_code,
            keymap::VK_SPACE | keymap::VK_RETURN | keymap::VK_0..=keymap::VK_9
        ) {
            // 其余键一律吞掉，别漏进主输入流（本模式不吃编码）。
            return KeyAction::Consumed;
        }

        // 取用：插入选中文本，**留在模式里**（这正是「连续选多条拼回」）
        match self.take_highlighted_input_history(state) {
            Some(text) if !text.is_empty() => {
                let comp = state.preedit.clone();
                self.commit_then_new_composition(text, comp)
            }
            // 候选为空（或高亮项不是历史）：同样是吞掉，不上屏任何东西。
            _ => KeyAction::Consumed,
        }
    }

    /// 当前高亮候选的绝对下标（无候选 → `None`）。与 `handle_aux_code::highlighted_candidate`
    /// 同一算法：页起点 + `state.selected_index`，并夹到列表末位防越界。
    fn highlighted_index(&self, state: &State) -> Option<usize> {
        if state.candidates.is_empty() {
            return None;
        }
        let (start, _) = self.page_range(state);
        Some((start + state.selected_index).min(state.candidates.len() - 1))
    }

    /// 高亮项若是会话，返回它的会话键（候选 id 首个 `\0` 之后的**全部**内容）。
    ///
    /// ⚠️ 必须取**全部剩余**，不能用 `split('\u{0}').nth(1)`：库里的会话键**自身含 `\0`**
    /// （`{start_ms:020}\0{seq:010}`），`nth(1)` 会把 `seq` 段吃掉，分词层于是拿一个截断的键
    /// 去做前缀范围扫描——同一毫秒内有两个会话时会串段。
    fn highlighted_session_key(&self, state: &State) -> Option<String> {
        let idx = self.highlighted_index(state)?;
        let cand = state.candidates.get(idx)?;
        let key = cand
            .id
            .strip_prefix(INPUT_HISTORY_ID)?
            .strip_prefix('\u{0}')?;
        (!key.is_empty()).then(|| key.to_string())
    }

    /// 高亮项若是可上屏的历史文本，返回它。
    ///
    /// 会话层与分词层的候选**都能取用**（会话层取整段、分词层取单段），故这里只看
    /// 候选是否属于本模式，不看层级。
    fn take_highlighted_input_history(&self, state: &State) -> Option<String> {
        let idx = self.highlighted_index(state)?;
        let cand = state.candidates.get(idx)?;
        cand.id
            .starts_with(INPUT_HISTORY_ID)
            .then(|| cand.text.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinator::Coordinator;
    use std::sync::Arc;
    use wind_bridge::handler::MessageHandler;
    use wind_config::Config;
    use wind_ipc::protocol::{EVENT_KEY_DOWN, MOD_SHIFT};
    use wind_store::Store;

    const VK_BACKSLASH: u32 = 0xDC;
    const VK_ESCAPE: u32 = 0x1B;
    const VK_SPACE: u32 = 0x20;
    const VK_RIGHT: u32 = 0x27;
    /// 下方向键（`keymap::VK_DOWN` 的同一值；测试内用常量便于直读）。
    const VK_DOWN: u32 = 0x28;

    fn key(c: &Coordinator, vk: u32, shift: bool) {
        c.handle_key_event(&KeyEventData {
            key_code: vk,
            scan_code: 0,
            modifiers: if shift { MOD_SHIFT } else { 0 },
            event_type: EVENT_KEY_DOWN,
            toggles: 0,
            event_seq: 0,
            prev_char: 0,
        });
    }

    /// 造一个无头 Coordinator：临时库 + 反斜杠绑「输入历史」动词。
    fn coord(tag: &str, idle_ms: u32) -> Arc<Coordinator> {
        let path = std::env::temp_dir().join(format!(
            "wind_inhistmode_{tag}_{}_{:?}.redb",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);
        let store = Arc::new(Store::open(&path).unwrap());
        let mut cfg = Config::default();
        cfg.input.history.idle_break_ms = idle_ms;
        cfg.keys
            .key_actions
            .insert("backslash".into(), "input_history".into());
        Coordinator::new_headless_with_store(cfg, None, store)
    }

    /// 往库里塞一段会话（绕过按键路径，直接走存储层）。
    fn seed(c: &Coordinator, start_ms: i64, text: &str, segs: &[&str]) {
        let segments: Vec<_> = segs
            .iter()
            .enumerate()
            .map(|(i, s)| wind_store::input_history::InputSegmentRecord {
                text: (*s).to_string(),
                at_ms: start_ms + i as i64,
            })
            .collect();
        c.store
            .as_ref()
            .unwrap()
            .append_input_session(start_ms, start_ms + segs.len() as i64, text, &segments)
            .unwrap();
    }

    /// ★ C3 前半：触发后候选**按时间倒序**列出最近输入（最新的在最前）。
    #[test]
    fn candidates_are_newest_first() {
        let c = coord("order", 3000);
        seed(&c, 1000, "最早那段", &["最早那段"]);
        seed(&c, 5000, "中间那段", &["中间那段"]);
        seed(&c, 9000, "最新那段", &["最新那段"]);
        key(&c, VK_BACKSLASH, false);
        assert_eq!(c.debug_active_mode(), Some("input_history"));
        assert_eq!(
            c.debug_all_candidate_texts(),
            ["最新那段", "中间那段", "最早那段"],
            "★ 必须是时间倒序，最新的在最前"
        );
    }

    /// ★ C3 后半：连续选中多条能拼回（每次插入后**仍在模式里**，且列表不动）。
    #[test]
    fn selecting_multiple_keeps_the_mode_and_the_list() {
        let c = coord("compose", 3000);
        seed(&c, 1000, "第二段", &["第二段"]);
        seed(&c, 9000, "第一段", &["第一段"]);
        key(&c, VK_BACKSLASH, false);
        let before = c.debug_all_candidate_texts();
        assert_eq!(before, ["第一段", "第二段"]);

        key(&c, VK_SPACE, false); // 取用最新那条
        assert_eq!(
            c.debug_active_mode(),
            Some("input_history"),
            "★ 取用后必须留在模式里，否则「连续选多条」无从谈起"
        );
        assert_eq!(
            c.debug_all_candidate_texts(),
            before,
            "取用后列表不应变化，用户接着挑下一条"
        );

        key(&c, VK_SPACE, false); // 再取第二条
        assert_eq!(c.debug_active_mode(), Some("input_history"));
    }

    /// ★ C2 的界面半边：会话层可展开为该会话的分词层，Esc 逐层退回。
    #[test]
    fn can_drill_into_segments_and_back() {
        let c = coord("drill", 3000);
        seed(&c, 1000, "你好世界", &["你好", "世界"]);
        key(&c, VK_BACKSLASH, false);
        assert_eq!(
            c.debug_all_candidate_texts(),
            ["你好世界"],
            "会话层：整段一条"
        );

        key(&c, VK_RIGHT, false); // 展开
        assert_eq!(
            c.debug_all_candidate_texts(),
            ["你好", "世界"],
            "★ 分词层：按落屏先后，不是倒序"
        );
        assert_eq!(c.debug_active_mode(), Some("input_history"));

        key(&c, VK_ESCAPE, false); // 退一层
        assert_eq!(c.debug_all_candidate_texts(), ["你好世界"], "回到会话层");
        assert_eq!(
            c.debug_active_mode(),
            Some("input_history"),
            "退一层不等于退出模式"
        );

        key(&c, VK_ESCAPE, false); // 再退 → 退出模式
        assert_eq!(c.debug_active_mode(), None, "会话层再按 Esc 才退出");
    }

    /// 存储开关关掉时进不去：触发键不吞键，落回普通输入。
    #[test]
    fn unavailable_when_storage_disabled() {
        let c = coord("off", 3000);
        c.set_config_for_test(|cfg| cfg.input.history.enabled = false);
        assert!(!c.input_history_mode_available(), "前提：开关已关");
        key(&c, VK_BACKSLASH, false);
        assert_eq!(c.debug_active_mode(), None, "关掉时不得进入本模式");
    }

    /// ★ C3 的**关键补充**：高亮移到**另一条**再取用。
    ///
    /// 前一条测试连按两次空格，两次取的都是最高那条——它证明不了「能选列表里别的条目」。
    /// 而用户的真实动作恰恰是「在候选里挑」：必须覆盖「方向键移动高亮 → 取用高亮那条」，
    /// 并把「取到的确实是另一条」断言出来（只看模式还在是不够的）。
    #[test]
    fn moving_highlight_then_selecting_picks_another_entry() {
        let c = coord("move", 3000);
        seed(&c, 1000, "第二条", &["第二条"]);
        seed(&c, 9000, "第一条", &["第一条"]);
        key(&c, VK_BACKSLASH, false);
        assert_eq!(c.debug_all_candidate_texts(), ["第一条", "第二条"]);

        key(&c, VK_DOWN, false); // 高亮下移到「第二条」

        // 取用：把返回的 KeyAction 打出来断言，而不是只看模式还在不在。
        let act = c.handle_key_event(&KeyEventData {
            key_code: VK_SPACE,
            scan_code: 0,
            modifiers: 0,
            event_type: EVENT_KEY_DOWN,
            toggles: 0,
            event_seq: 0,
            prev_char: 0,
        });
        let dbg = format!("{act:?}");
        assert!(
            dbg.contains("第二条"),
            "取用的必须是**高亮那条**（第二条），实际动作 {dbg}"
        );
        assert!(
            !dbg.contains("第一条"),
            "不得取到未高亮的第一条，实际动作 {dbg}"
        );
        assert_eq!(
            c.debug_active_mode(),
            Some("input_history"),
            "取用后仍留在模式里"
        );
    }

    /// ★★ 取用闸：候选**非空**时按字母 / 退格键**不得**把高亮的历史写进宿主。
    ///
    /// 这是「手滑一次就是一次历史误插入」的守门测试——历史里装的正是用户刚丢的正文，
    /// 误插入比不插入更糟。同时用空格做对照，证明这道闸没把正常取用路径一起堵死。
    #[test]
    fn non_take_keys_do_not_insert_history() {
        const VK_A: u32 = 0x41;
        const VK_Z: u32 = 0x5A;
        const VK_BACK: u32 = 0x08;
        let secret = "不该被插进去的一段";

        let c = coord("gate", 3000);
        seed(&c, 9000, secret, &[secret]);
        key(&c, VK_BACKSLASH, false);
        assert_eq!(
            c.debug_all_candidate_texts(),
            [secret],
            "前提：进入模式后候选非空"
        );

        let press = |vk: u32| {
            c.handle_key_event(&KeyEventData {
                key_code: vk,
                scan_code: 0,
                modifiers: 0,
                event_type: EVENT_KEY_DOWN,
                toggles: 0,
                event_seq: 0,
                prev_char: 0,
            })
        };

        for vk in [VK_A, VK_Z, VK_BACK] {
            let dbg = format!("{:?}", press(vk));
            assert!(
                !dbg.contains(secret),
                "★ 键 0x{vk:X} 不得把历史写进宿主，实际动作 {dbg}"
            );
            assert_eq!(
                c.debug_active_mode(),
                Some("input_history"),
                "键 0x{vk:X} 之后仍应留在模式里"
            );
            assert_eq!(
                c.debug_all_candidate_texts(),
                [secret],
                "键 0x{vk:X} 之后列表不应变化"
            );
        }

        // 对照：空格仍然取用（闸只拦非取用键）
        let dbg = format!("{:?}", press(VK_SPACE));
        assert!(dbg.contains(secret), "空格仍应取用高亮那条，实际动作 {dbg}");
        assert_eq!(
            c.debug_active_mode(),
            Some("input_history"),
            "取用后仍留在模式里"
        );
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

    /// ★★ 真缺陷的守门测试：**刚打完、还没落库的那段必须排在第一条**。
    ///
    /// 缺陷原状：候选纯读库，而封口只在「下一次上屏判过 idle」或断流事件时发生，
    /// 于是「打完立刻想找回」这一最常见时刻，列表里**根本没有那段**。
    #[test]
    fn unflushed_session_is_listed_first() {
        let c = coord("live", 3000);
        c.note_commit_action(&insert("刚打的那段")); // 只进内存缓冲，不封口、不落库
        assert!(
            c.store
                .as_ref()
                .unwrap()
                .list_input_sessions(0, 0)
                .unwrap()
                .0
                .is_empty(),
            "前提：库里确实还没有它"
        );

        key(&c, VK_BACKSLASH, false);
        assert_eq!(
            c.debug_all_candidate_texts(),
            ["刚打的那段"],
            "★ 未落库的那段必须出现在候选里，且排在第一条"
        );
    }

    /// ★ 不重复：会话在「缓冲 → 队列 → 库」三处**互斥**，任何时刻都不得被列成两条。
    ///
    /// ⚠️ 本用例刻意**不与后台线程赛跑**（`note_commit_action` 会 `spawn_input_history_flush`）：
    /// 后台线程 `take` 走队列、但还没写进库的那一瞬间，那段在缓冲里没有、在队列里也没有、
    /// 在库里还没有——这是**已披露的在途窗口**（见模块文档），赛跑会让本用例偶发失败。
    /// 故先等它安静，再同步兜底落库，然后只断言**与竞态无关的不变量**。
    #[test]
    fn pending_states_never_duplicate() {
        let c = coord("nopdup", 3000);
        c.note_commit_action(&insert("第一段"));
        c.terminate_input_history("test"); // 封口入队
        c.note_commit_action(&insert("第二段")); // 又开一段，留在内存

        // 等后台线程跑完 + 同步兜底，把竞态消除掉
        std::thread::sleep(std::time::Duration::from_millis(80));
        c.flush_input_history_blocking();

        key(&c, VK_BACKSLASH, false);
        let got = c.debug_all_candidate_texts();
        assert_eq!(got[0], "第二段", "★ 内存里最新那段必须排第一：{got:?}");
        assert_eq!(
            got.iter().filter(|t| t.as_str() == "第一段").count(),
            1,
            "★ 第一段（已落库）不得重复计两条：{got:?}"
        );
        assert_eq!(
            got.iter().filter(|t| t.as_str() == "第二段").count(),
            1,
            "★ 第二段（在内存）不得重复计两条：{got:?}"
        );
    }

    /// ★ 内存那条候选必须**能展开到分词**：否则按 `→` 没反应，用户以为坏了。
    #[test]
    fn pending_candidate_expands_to_segments() {
        let c = coord("liveexpand", 3000);
        c.note_commit_action(&insert("你好"));
        c.note_commit_action(&insert("世界")); // 同一会话（无 idle 间隔）⇒ 两个片段
        key(&c, VK_BACKSLASH, false);
        assert_eq!(c.debug_all_candidate_texts(), ["你好世界"], "会话层：整段一条");

        key(&c, VK_RIGHT, false);
        assert_eq!(
            c.debug_all_candidate_texts(),
            ["你好", "世界"],
            "★ 内存会话的片段必须能展开，且按落屏先后"
        );
    }

    /// ★★ 靶心动作的守门测试：**取用一条「还没落库」的候选，必须真的插进宿主**。
    ///
    /// 这是用户实测暴露的原场景（「刚打的那段」）——只断言「列出来了」不够，
    /// 必须证明它**能被取用并上屏**，以及取用后仍留在模式里。
    #[test]
    fn taking_an_unflushed_candidate_inserts_it() {
        let c = coord("takeunflushed", 3000);
        c.note_commit_action(&insert("还没落库的那段"));
        key(&c, VK_BACKSLASH, false);
        assert_eq!(c.debug_all_candidate_texts(), ["还没落库的那段"]);

        let act = c.handle_key_event(&KeyEventData {
            key_code: VK_SPACE,
            scan_code: 0,
            modifiers: 0,
            event_type: EVENT_KEY_DOWN,
            toggles: 0,
            event_seq: 0,
            prev_char: 0,
        });
        let dbg = format!("{act:?}");
        assert!(
            dbg.contains("还没落库的那段"),
            "★ 未落库的候选被取用时必须真的走 commit 通道插入宿主，实际动作 {dbg}"
        );
        assert_eq!(
            c.debug_active_mode(),
            Some("input_history"),
            "取用后仍留在模式里（可继续挑下一条）"
        );
    }

    /// ★ 边界 (i) 的直接断言：**已封口、尚未落库**（队列里）的会话也要出现在候选里，且能展开。
    ///
    /// 直接构造队列状态而**不**调 `note_commit_action`/`terminate_input_history`——那两者都会
    /// `spawn_input_history_flush`，本用例会变成跟后台线程赛跑（上一版就是这么偶发失败的）。
    #[test]
    fn queued_session_is_listed_and_expandable() {
        let c = coord("queued", 3000);
        let at = 1_700_000_000_000i64;
        c.input_history_queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(crate::input_history::ClosedSession {
                start_ms: at,
                end_ms: at + 100,
                text: "已封口未落库".to_string(),
                segments: vec![wind_store::input_history::InputSegmentRecord {
                    text: "已封口未落库".to_string(),
                    at_ms: at,
                }],
            });

        key(&c, VK_BACKSLASH, false);
        assert_eq!(
            c.debug_all_candidate_texts(),
            ["已封口未落库"],
            "★ 队列里（已封口、还没写进库）的那段必须被列出"
        );

        key(&c, VK_RIGHT, false);
        assert_eq!(
            c.debug_all_candidate_texts(),
            ["已封口未落库"],
            "★ 队列那条也要能展开到片段"
        );
    }

    /// 本模式吃编码：打进普通字母不该产生输入缓冲（没有候选时也不漏进主输入流）。
    #[test]
    fn mode_does_not_leak_keys_into_normal_input() {
        let c = coord("noleak", 3000);
        key(&c, VK_BACKSLASH, false);
        key(&c, 0x41, false); // 'A'
        assert_eq!(c.debug_input_buffer(), "", "本模式不吃编码");
        assert_eq!(c.debug_active_mode(), Some("input_history"));
    }
}
