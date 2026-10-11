//! 卡片面板：触发键取数 → 下发 `ShowCardPanel`；UI 回事件 → 上屏 / 收起。
//!
//! 面板窗口（`wind-ui::card_panel_window`）与状态机（`wind-ui::card_panel`）早已就位，
//! 缺的只是**调用方**——本模块就是那个调用方，两端各自负责一件事：
//! 窗口管像素与鼠标，本模块管「取哪些文本」与「选中后往宿主里写什么」。
//!
//! ## 取数（`card_panel_cards`）
//!
//! 卡片 = 输入历史里的**分词**（`InputSegmentRecord::text`），**时间倒序**：内存里尚未
//! 落库的会话在前（同 `input_history_candidates` 的处置——刚打完就想找回时它还没进库），
//! 再按会话时间倒序读库，每个会话内片段按落屏先后。
//!
//! 边界：只取**一页**会话（`CARD_PANEL_SESSION_SCAN`）并在 `CARD_PANEL_QUOTA` 处截断，
//! 不做全表规模操作；读失败静默降级（`debug!`），面板少几张卡也比吞掉触发键好。
//!
//! ## 上屏（`commit_card_panel_indices`）
//!
//! 走**统一出口** `push_no_key_ctx_action`（`handle_candidate.rs`）——鼠标/UI 事件不在
//! `OnKeyDown` 的应答里，只能经 push 管道投递；且只有它的 `InsertText` 臂会推
//! `CMD_COMMIT_TEXT` 正文。⚠️ 不能改走 `commit_then_new_composition`：那条返回
//! `UpdateComposition`，`push_no_key_ctx_action` 对它**只推 preedit、不推正文**，
//! 实机表现是「点了像没反应」（上一轮实测的坑，见 `select_candidate_at` 的 `InputHistory` 臂）。

use crate::coordinator::{Coordinator, State};
use tracing::debug;
use wind_bridge::handler::KeyAction;
use wind_keys::keymap;
use wind_ui_types::{CardPanelUiEvent, UiCommand};

/// 面板一次最多给几张卡。
///
/// 与 `INPUT_HISTORY_QUOTA` 同量级：面板是「挑一段拼回去」的入口，不是历史浏览器，
/// 卡片铺满一屏之后再多的只能靠拖选翻，成本却要按张数付（每次下发都要 clone 文本）。
const CARD_PANEL_QUOTA: usize = 20;

/// 取分词时最多看几个会话（每个会话的片段数本身有上限）。
const CARD_PANEL_SESSION_SCAN: usize = 20;

/// 把一批卡下标拼成要上屏的文本：**按给定顺序、无分隔符、重复下标只取第一次**。
///
/// 单独抽成纯函数是为了让「顺序 / 去重 / 逐字拼接」这三条可被直接钉住（见本模块测试）。
/// 多选拖动会**反复滑过同一张卡**（`CardPanel::selected` 本身已有序去重，但协议层是
/// `Vec<usize>`，调用方不能假设它一定干净），故这里再兜一次而不是信任上游。
pub(crate) fn card_panel_text_for(cards: &[String], indices: &[usize]) -> String {
    let mut taken = vec![false; cards.len()];
    let mut out = String::new();
    for &i in indices {
        let Some(text) = cards.get(i) else {
            continue;
        };
        if std::mem::replace(&mut taken[i], true) {
            continue;
        }
        out.push_str(text);
    }
    out
}

impl Coordinator {
    /// 触发键进入输入历史时调用：取分词并下发卡片面板。
    ///
    /// 空列表不发 `ShowCardPanel`（发一屏零张卡看不出是坏了还是没历史），改发 `HideCardPanel`
    /// 把可能残留在屏上的旧面板收掉。
    pub(crate) fn show_card_panel(&self) {
        let cards = self.card_panel_cards();
        if cards.is_empty() {
            self.hide_card_panel();
            return;
        }
        // 只记张数：卡片文本是用户输入（根 AGENTS.md 日志规范）。
        debug!(cards = cards.len(), "card-panel: 下发卡片面板");
        *self
            .card_panel_cards
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = cards.clone();
        let _ = self.ui_tx.send(UiCommand::ShowCardPanel { cards });
    }

    /// 收起面板：清掉卡片快照，避免面板已不在屏上、迟到的点击却按旧下标上屏。
    pub(crate) fn hide_card_panel(&self) {
        self.card_panel_cards
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        let _ = self.ui_tx.send(UiCommand::HideCardPanel);
    }

    /// 卡片面板此刻是否在屏上（协调器侧真相：`show_card_panel` 写入、`hide_card_panel` 清空）。
    ///
    /// ⚠️ 与 `state.active` **不是**同一件事，这正是本函数存在的理由：面板挂在屏上时模式
    /// 可能已被别的出口退掉（宿主 UIElement `Abort` 走 `cancel_session`、切模式、失焦…）。
    /// 用户按触发键时看的是**面板在不在**，故「同一个键开关」的判据必须问这个，不能只问
    /// `active`——只问 `active` 的后果是「面板还在、触发键却走进入通路把它再弹一遍」，
    /// 也就是真机报的「关不掉」。
    pub(crate) fn input_history_panel_open(&self) -> bool {
        !self
            .card_panel_cards
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }

    /// 「触发键又按了一次」判据：本次键与进入本模式时的触发键同字符。
    ///
    /// 用 `special_prefix`（进入时由 `keymap::vk_to_prefix_char_with_letters` 记下）而不是
    /// 回查配置表：本模式下其余键本来就一律吞掉，这一臂只改「吞键」的结果，不放行任何键
    /// 落进主输入流；配不出字符的触发键（修饰键）返回 `false`，退面板仍走 `Esc` / 删除键。
    ///
    /// ⚠️ 取用键（空格 / 回车 / 数字）**不参与**本判据：它们在本模式里有明确含义
    /// （取当前高亮项），若触发键恰是其中之一，退面板让给 `Esc`。
    pub(crate) fn input_history_trigger_key_pressed(state: &State, key_code: u32) -> bool {
        if state.special_prefix.is_empty()
            || matches!(
                key_code,
                keymap::VK_SPACE | keymap::VK_RETURN | keymap::VK_0..=keymap::VK_9
            )
        {
            return false;
        }
        keymap::vk_to_prefix_char_with_letters(key_code)
            .is_some_and(|c| c.to_string() == state.special_prefix)
    }

    /// 取卡片文本：时间倒序的输入历史分词，一页为限。
    ///
    /// 与 `handle_input_history::input_history_candidates` **同源同序**（内存在前、库在后），
    /// 但取的是**分词**而不是会话整段——卡片的用法就是「挑几段拼回一句话」，
    /// 整段文本在一张卡里既显示不下、拖选也没有颗粒度。
    fn card_panel_cards(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        // ① 内存里尚未落库的会话（最新在前）。
        for (_, s) in self.pending_input_history_sessions() {
            // ★ 会话内部的分词按时间**升序**存储，卡片要「刚打的最前」⇒ 必须反转。
            for seg in s.segments.iter().rev() {
                if !seg.text.is_empty() {
                    out.push(seg.text.clone());
                }
            }
            if out.len() >= CARD_PANEL_QUOTA {
                out.truncate(CARD_PANEL_QUOTA);
                return out;
            }
        }
        // ② 库：会话按时间倒序，**一页一页取**（不做全表扫描），失败静默降级。
        let Some(store) = self.store.as_ref() else {
            return out;
        };
        match store.list_input_sessions(0, CARD_PANEL_SESSION_SCAN) {
            Ok((rows, _)) => {
                for (key, _rec) in rows {
                    match store.list_input_segments(&key) {
                        Ok(segs) => {
                            // ★ 同上：会话内分词升序存储，展示需倒序（刚打的最前）。
                            for s in segs.iter().rev() {
                                if !s.text.is_empty() {
                                    out.push(s.text.clone());
                                }
                            }
                        }
                        Err(e) => debug!("card-panel: 分词读取失败: {e}"),
                    }
                    if out.len() >= CARD_PANEL_QUOTA {
                        break;
                    }
                }
            }
            Err(e) => debug!("card-panel: 会话读取失败: {e}"),
        }
        out.truncate(CARD_PANEL_QUOTA);
        out
    }

    /// UI 回来的卡片事件：点击 / 拖选 → 上屏；关闭 → 收起。
    pub(crate) fn handle_card_panel_event(&self, ev: CardPanelUiEvent) {
        match ev {
            CardPanelUiEvent::Close => {
                self.hide_card_panel();
                debug!("card-panel: UI 请求关闭");
            }
            CardPanelUiEvent::Click(i) => self.commit_card_panel_indices(&[i]),
            // 拖动多选：`Vec<usize>` 的顺序即滑过先后，**按序拼接、不加分隔符**
            // （`card_panel_text_for` 的文档说明为何是「无分隔符」）。
            CardPanelUiEvent::MultiSelect(indices) => self.commit_card_panel_indices(&indices),
        }
    }

    /// 下标 → 文本 → 一次性提交。
    ///
    /// 上屏出口是 [`Self::push_no_key_ctx_action`]（理由见模块文档），提交后收起面板：
    /// 面板是「取一段就走」的一次性入口，留着它只会挡住刚才那片区域；用户再按触发键即重开。
    fn commit_card_panel_indices(&self, indices: &[usize]) {
        let cards = self
            .card_panel_cards
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if cards.is_empty() {
            // 面板已收起（或快照被清）却来了点击：静默丢弃，绝不上屏空文本。
            return;
        }
        let text = card_panel_text_for(&cards, indices);
        if text.is_empty() {
            return;
        }
        let chinese_mode = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .chinese_mode;
        // 只记张数与去向，不记文本（info! 亦不得含用户输入；此处按 UI 事件惯例用 debug!）。
        debug!(
            cards = indices.len(),
            chars = text.chars().count(),
            "card-panel: 提交选中卡片"
        );
        self.push_no_key_ctx_action(
            &KeyAction::InsertText {
                text,
                new_composition: None,
                mode_changed: false,
                chinese_mode,
                has_new_composition: false,
            },
            chinese_mode,
        );
        self.hide_card_panel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use wind_bridge::handler::{KeyEventData, MessageHandler};
    use wind_config::Config;
    use wind_ipc::protocol::EVENT_KEY_DOWN;
    use wind_store::Store;

    /// `\`（`VK_OEM_5`）：本模块测试用的触发键。
    const VK_BACKSLASH: u32 = 0xDC;

    /// 造一个无头 Coordinator（临时库；反斜杠绑「输入历史」动词——与真机
    /// `%APPDATA%\WindInput\config.toml` 的 `[keys.key_actions] backslash = "input_history"`
    /// 同形，**单键走引导键链**，不进 key_down 热键表，见 `wind-config/src/hotkey.rs:117`）。
    fn coord(tag: &str) -> Arc<Coordinator> {
        let path = std::env::temp_dir().join(format!(
            "wind_cardpanel_{tag}_{}_{:?}.redb",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);
        let store = Arc::new(Store::open(&path).unwrap());
        let mut cfg = Config::default();
        cfg.keys
            .key_actions
            .insert("backslash".into(), "input_history".into());
        Coordinator::new_headless_with_store(cfg, None, store)
    }

    /// 完整一次「按下触发键」：与真机同一条链路（`handle_key_event` 全程，不是直调收起函数）。
    fn press(c: &Coordinator, vk: u32) {
        c.handle_key_event(&KeyEventData {
            key_code: vk,
            scan_code: 0,
            modifiers: 0,
            event_type: EVENT_KEY_DOWN,
            toggles: 0,
            event_seq: 0,
            prev_char: 0,
        });
    }

    /// 往库里塞一段会话：`card_panel_cards()` 才非空，面板才有卡可显示。
    fn seed_history(c: &Coordinator) {
        let segments = vec![wind_store::input_history::InputSegmentRecord {
            text: "测试片段".to_string(),
            at_ms: 9000,
        }];
        c.store
            .as_ref()
            .unwrap()
            .append_input_session(9000, 9001, "测试片段", &segments)
            .unwrap();
    }

    fn seed_cards(c: &Coordinator, cards: &[&str]) {
        let mut slot = c.card_panel_cards.lock().unwrap();
        *slot = cards.iter().map(|s| s.to_string()).collect();
    }

    fn snapshot(c: &Coordinator) -> Vec<String> {
        c.card_panel_cards.lock().unwrap().clone()
    }

    /// 点击：取的是**被点那张**的文本（不是高亮那张、不是首张）。
    #[test]
    fn click_takes_card_text_at_index() {
        let cards = ["甲甲", "乙乙", "丙丙"];
        let owned: Vec<String> = cards.iter().map(|s| s.to_string()).collect();
        assert_eq!(card_panel_text_for(&owned, &[2]), "丙丙");
        assert_eq!(card_panel_text_for(&owned, &[0]), "甲甲");

        let c = coord("click");
        seed_cards(&c, &cards);
        c.handle_card_panel_event(CardPanelUiEvent::Click(1));
        // 上屏即收起：快照清空，迟到的点击不会再上屏任何东西。
        assert!(snapshot(&c).is_empty(), "提交后不得残留卡片快照");
    }

    /// 拖动多张：按滑过先后拼接，重复下标只取第一次。
    #[test]
    fn multi_select_keeps_order_and_dedupes() {
        let owned: Vec<String> = ["A", "B", "C"].iter().map(|s| s.to_string()).collect();
        assert_eq!(card_panel_text_for(&owned, &[2, 0, 2, 1]), "CAB");
        assert_eq!(card_panel_text_for(&owned, &[1, 1, 1]), "B");

        let c = coord("multi");
        seed_cards(&c, &["A", "B", "C"]);
        c.handle_card_panel_event(CardPanelUiEvent::MultiSelect(vec![2, 0, 2, 1]));
        assert!(snapshot(&c).is_empty(), "提交后不得残留卡片快照");
    }

    /// 拼接逐字正确：无分隔符、顺序不动、总字数等于各卡字数之和。
    #[test]
    fn join_is_char_exact_without_separator() {
        let owned: Vec<String> = ["今天", "下雨", "了"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let joined = card_panel_text_for(&owned, &[0, 1, 2]);
        assert_eq!(joined, "今天下雨了");
        assert_eq!(
            joined.chars().count(),
            owned.iter().map(|s| s.chars().count()).sum::<usize>()
        );
        // 越界下标被忽略而不是 panic / 补空串。
        assert_eq!(card_panel_text_for(&owned, &[0, 9, 2]), "今天了");
    }

    /// 关闭后不残留：快照清空 ⇒ 之后任何点击都无文本可上屏。
    #[test]
    fn close_clears_cards_and_later_click_is_noop() {
        let c = coord("close");
        seed_cards(&c, &["甲", "乙"]);
        c.handle_card_panel_event(CardPanelUiEvent::Close);
        assert!(snapshot(&c).is_empty(), "关闭后不得残留卡片快照");
        c.handle_card_panel_event(CardPanelUiEvent::Click(0));
        assert!(snapshot(&c).is_empty(), "关闭后的点击不得再写库/上屏");
    }

    /// ★★ 真机缺陷守门 A：**放弃会话（宿主 UIElement `Abort` / 取消键）必须把卡片面板一并收掉**。
    ///
    /// 真机链路（有原文可查）：`host_reads` 宿主（记事本实测日志 `uielement: … host_reads=true`）
    /// 进本模式后发 UIElement `Abort` → `handle_uielement.rs` 的 `CMD_UIELEMENT_ACTION` 臂
    /// （`self.cancel_session(&mut state)`）→ `Coordinator::cancel_session`。
    /// 修前该函数对 `InputHistory` 只调 `exit_special_mode`（`active` 归 None）并 `notify_ui_hide`
    /// ——后者收的是**候选窗**，而卡片面板是另一个 `LayeredWindow`（`wind-ui::card_panel_window`）
    /// ⇒ **模式退了、面板留在屏幕上**。
    ///
    /// 本用例先在修前代码上跑红（快照非空），修后绿——那一步就是「关不掉」的成因。
    #[test]
    fn cancel_session_also_hides_the_card_panel() {
        let c = coord("cancelpanel");
        seed_history(&c);
        press(&c, VK_BACKSLASH);
        assert!(
            !snapshot(&c).is_empty(),
            "前提：按下触发键后面板在屏上（`show_card_panel` 写了快照）"
        );

        let mut st = c.state.lock().unwrap_or_else(|e| e.into_inner());
        let _ = c.cancel_session(&mut st);
        drop(st);

        assert!(
            c.debug_active_mode().is_none(),
            "前提：放弃会话退出了模式（修前修后都成立）"
        );
        assert!(
            snapshot(&c).is_empty(),
            "★ 放弃会话后卡片面板不得留在屏上——留在屏上就是用户实测的「卡片面板关不掉」"
        );
    }

    /// ★★ 真机缺陷守门 B：**同一个键开、同一个键关**——面板在屏上时按触发键必须**收起**它，
    /// 而不是走「进入」通路把它重新弹一遍。
    ///
    /// 这一条钉的正是用户可见的形态「按 `\` 关不掉」。修前的完整序列（真机日志指纹：
    /// `卡片面板显示` 连着两条、中间没有 `卡片面板隐藏`）：
    /// ① 按 `\` → 进入模式 + 面板开；
    /// ② 模式被 `cancel_session` 静默退掉（守门 A 的那个缺陷）⇒ `active == None`、**面板还在屏上**；
    /// ③ 再按 `\` → 引导键链看到 `active == None` → `enter_bound_action` 的
    ///    `BoundAction::InputHistory` 臂 → `enter_input_history_mode` + `show_card_panel()`
    ///    ⇒ **面板被重开**，用户怎么按都关不掉。
    ///
    /// ⚠️ 本用例**只构造 ② 之后的残局**（`seed_cards` 写的就是 `show_card_panel` 留下的那份
    /// 快照），因为守门 A 修好后 ② 已不可能自然出现；判据仍是「面板在不在屏上」，与用户
    /// 看到的一致。修前跑红（快照被重新填满 + `active` 又变成 input_history），修后绿。
    #[test]
    fn trigger_key_closes_a_panel_whose_mode_was_silently_exited() {
        let c = coord("stuckpanel");
        seed_cards(&c, &["甲", "乙"]);
        assert!(
            c.debug_active_mode().is_none(),
            "前提：模式已不在（面板是上一轮遗留的）"
        );

        press(&c, VK_BACKSLASH);

        assert!(
            snapshot(&c).is_empty(),
            "★ 面板在屏上时按触发键必须收起它，不得重新进入模式再弹一次（「关了又开」）"
        );
        assert!(
            c.debug_active_mode().is_none(),
            "★ 收起后不得停在输入历史模式里"
        );
    }

    /// ★★ 完整一次按键流程的收口断言（开 → 关 → 屏上不得再有面板）。
    ///
    /// 与 `handle_input_history::tests::trigger_key_again_closes_the_panel` 的差别：
    /// 那条只断言 `debug_active_mode()`，**看不见面板**；本用例断言面板快照——「关了又开」
    /// 的表现恰恰是快照被重新填上（`show_card_panel` 写入、`hide_card_panel` 清空），
    /// 所以这条才抓得住它。
    #[test]
    fn full_trigger_sequence_never_leaves_a_panel() {
        let c = coord("panelseq");
        seed_history(&c);

        press(&c, VK_BACKSLASH);
        assert_eq!(c.debug_active_mode(), Some("input_history"), "第一次按：进模式");
        assert!(!snapshot(&c).is_empty(), "第一次按：面板开");

        press(&c, VK_BACKSLASH);
        assert_eq!(c.debug_active_mode(), None, "第二次按：退模式");
        assert!(
            snapshot(&c).is_empty(),
            "★ 第二次按：面板必须关掉且**没有被立刻重开**（重开会把快照写回来）"
        );
    }
}
