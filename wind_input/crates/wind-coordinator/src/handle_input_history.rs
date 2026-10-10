//! 输入历史恢复模式（`ModeKind::InputHistory`）：按时间倒序列出最近的输入，挑片段拼回。
//!
//! # 它解决什么
//!
//! 打完一段字、还没复制就发送失败，内容没了。采集端（`crate::input_history`）已把
//! 「什么时候打过什么」按会话 + 片段两级落进本地库，本模块是**取回来**的那一端。
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

    /// 按当前层级取候选（纯读库，不改视图）。
    fn input_history_candidates(&self, state: &State) -> Vec<Candidate> {
        let Some(store) = self.store.as_ref() else {
            return Vec::new();
        };
        let cand = |text: String, id: &str| Candidate {
            text,
            id: id.to_string(),
            ..Default::default()
        };
        if state.special_buffer.is_empty() {
            // 会话层：时间倒序（store 已排好），取一页
            match store.list_input_sessions(0, INPUT_HISTORY_QUOTA) {
                Ok((rows, _)) => rows
                    .into_iter()
                    .map(|(key, rec)| {
                        // 片段层要用会话键，故把它挂在候选 id 上（`text` 是给用户看的整段文本）
                        cand(rec.text, &format!("{INPUT_HISTORY_ID}\u{0}{key}"))
                    })
                    .collect(),
                Err(e) => {
                    debug!("input-history: 会话读取失败: {e}");
                    Vec::new()
                }
            }
        } else {
            // 分词层：该会话的各片段，**按落屏先后**（拼回时要顺着读，不倒序）
            match store.list_input_segments(&state.special_buffer) {
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

    /// 高亮项若是会话，返回它的会话键（从候选 id 的后半段取）。
    fn highlighted_session_key(&self, state: &State) -> Option<String> {
        let idx = self.highlighted_index(state)?;
        let cand = state.candidates.get(idx)?;
        let key = cand.id.split('\u{0}').nth(1)?;
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
