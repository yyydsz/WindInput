//! 卡片面板：把打过的**词组**各显示为一张小卡片，支持**单击取一张**与**长按滑动多选**。
//!
//! # 手势（用户原话：「可以点选，也可以长按滑动覆盖的卡片达到多选连续的效果」）
//!
//! 两种手势靠**按下时长**区分，判定点是「是否曾进入拖动」而不是抬手时长：
//!
//! | 序列 | 结果 |
//! |---|---|
//! | `Down`(命中卡 i) → 未超阈值 → `Up` | **单击** ⇒ [`CardPanelEvent::Click(i)`] |
//! | `Down`(命中卡 i) → 超过阈值 → `Drag`/`Move` 滑过若干卡 → `Up` | **多选** ⇒ [`CardPanelEvent::MultiSelect`] |
//! | `Down` 未命中任何卡 → `Up` | 无事件（面板空白处按下不产生取用） |
//!
//! 「超过阈值」由**外部传入的 `now_ms`** 判定，而不是读时钟——这让整条状态机成为纯函数，
//! 可直接用事件序列单测，不需要真窗口（同 `soft_keyboard.rs` 把拖动算术留在纯 Rust 里的理由）。
//!
//! # 选中集合
//!
//! [`CardPanel::selected`] 保持**有序、无重复**：按滑过的先后追加，重复命中同一张不重复入列。
//! 顺序即拼接顺序，调用方按它拼句子。
//!
//! # 本模块的边界
//!
//! 本模块**只做面板的状态与几何**：卡片数据、网格布局、命中判定、手势状态机、选中态查询、
//! 以及向外抛出的事件队列（[`CardPanel::drain_events`]）。**窗口创建、渲染与主题消费不在
//! 本模块**——那部分照 `soft_keyboard.rs` 的 `view` 盒模型路径接线，由调用方完成。
//!
//! 事件用**队列 + drain** 而不是回调 trait：`PanelMouse::on_mouse` 的签名里没有宿主参数，
//! 队列让本模块既不依赖宿主、又能在纯单测里断言「产生了什么意图」。

/// 长按阈值（毫秒）：按下超过它才进入拖动（多选）态，未超过即按单击处置。
///
/// 350ms 是「比双击间隔长、又短到不觉得卡」的常用取值；它只影响**进入拖动**的时机，
/// 不影响抬手时对单击/多选的判定（判定看的是「是否曾进入拖动」）。
pub const LONG_PRESS_MS: u64 = 350;

/// 卡片尺寸与间距（设备像素）。逻辑尺寸由调用方按 DPI 缩放后传入。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CardMetrics {
    pub card_w: u32,
    pub card_h: u32,
    pub gap_x: u32,
    pub gap_y: u32,
    /// 面板四周留白。
    pub pad: u32,
}

impl Default for CardMetrics {
    fn default() -> Self {
        Self {
            card_w: 96,
            card_h: 32,
            gap_x: 8,
            gap_y: 8,
            pad: 8,
        }
    }
}

/// 一张卡片。文本即用户打过的词组。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardItem {
    pub text: String,
}

impl CardItem {
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

/// 矩形（面板客户区坐标，原点**左上**，设备像素）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    /// 左闭右开区间判定：右/下边界上的点**不算**命中。
    ///
    /// 右开是必须的——相邻两卡之间只隔 `gap_x`，若两边都闭，落在共享边界上的坐标会
    /// 同时命中两张卡，命中结果取决于遍历顺序，表现为「贴着边滑时选中跳张」。
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x as f32
            && y >= self.y as f32
            && x < (i64::from(self.x) + i64::from(self.w)) as f32
            && y < (i64::from(self.y) + i64::from(self.h)) as f32
    }
}

/// 按可用宽度把 `n` 张卡排成若干行。
///
/// 返回 `(每张卡的矩形, 面板宽, 面板高)`。`n == 0` 时返回空向量与零尺寸。
///
/// **每行张数**由「可用内宽能放下几张」算出，至少 1 张（宽度再窄也不除零）；
/// 面板尺寸**按实际用到的列数**算而不是按 `avail_w`——否则卡片少时会画出一大片空白。
pub fn layout_cards(n: usize, avail_w: u32, m: CardMetrics) -> (Vec<Rect>, u32, u32) {
    if n == 0 {
        return (Vec::new(), 0, 0);
    }
    let inner = avail_w.saturating_sub(m.pad.saturating_mul(2));
    let step = m.card_w.saturating_add(m.gap_x).max(1);
    let per_row = (inner.saturating_add(m.gap_x) / step).max(1) as usize;

    let mut rects = Vec::with_capacity(n);
    for i in 0..n {
        let r = (i / per_row) as u32;
        let c = (i % per_row) as u32;
        rects.push(Rect {
            x: (m.pad + c * step) as i32,
            y: (m.pad + r * (m.card_h + m.gap_y)) as i32,
            w: m.card_w,
            h: m.card_h,
        });
    }

    let rows = n.div_ceil(per_row) as u32;
    let cols = per_row.min(n) as u32;
    let w = m.pad * 2 + cols * m.card_w + cols.saturating_sub(1) * m.gap_x;
    let h = m.pad * 2 + rows * m.card_h + rows.saturating_sub(1) * m.gap_y;
    (rects, w, h)
}

/// 一个手势阶段。判定点记在这里，抬手时只看它——**不看时长**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gesture {
    /// 没有按下的指针。
    Idle,
    /// 已按下、尚未判定为单击还是拖动。
    Pending { index: usize },
    /// 已进入拖动（多选）态。
    Dragging,
}

/// 面板产生的意图。由调用方在事件处理之后 `drain_events()` 取出并执行。
///
/// 用队列而不是回调 trait：`PanelMouse::on_mouse` 签名里没有宿主参数，队列让本模块
/// 既不依赖宿主、又能在纯单测里断言意图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CardPanelEvent {
    /// 单击第 `index` 张卡 ⇒ 取用它。
    Click(usize),
    /// 长按滑动多选后取用，**按滑过顺序**给出。
    MultiSelect(Vec<usize>),
    /// 请求关闭面板。
    Close,
}

/// 鼠标事件种类。与平台侧 [`crate::window`] / `mac_panel` 的消息一一对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelMouseKind {
    /// 光标移动（未按住）。
    Move,
    /// 左键按下。
    Down,
    /// 左键按住并移动。
    Drag,
    /// 左键抬起。
    Up,
    /// 光标离开面板。
    Leave,
}

/// 一次鼠标事件。坐标是**面板客户区设备像素、原点左上**。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PanelPointer {
    pub kind: PanelMouseKind,
    pub x: f32,
    pub y: f32,
}

impl PanelPointer {
    pub fn new(kind: PanelMouseKind, x: f32, y: f32) -> Self {
        Self { kind, x, y }
    }
}

/// 卡片面板的状态。
pub struct CardPanel {
    cards: Vec<CardItem>,
    metrics: CardMetrics,
    rects: Vec<Rect>,
    size: (u32, u32),
    gesture: Gesture,
    selected: Vec<usize>,
    /// `Down` 的时刻（毫秒），用于长按阈值判定。
    pressed_at_ms: u64,
    events: Vec<CardPanelEvent>,
    visible: bool,
}

impl CardPanel {
    /// 用卡片与可用宽度建面板。卡片顺序由调用方给（**时间倒序**），本模块原样保持。
    pub fn new(cards: Vec<CardItem>, avail_w: u32, metrics: CardMetrics) -> Self {
        let (rects, w, h) = layout_cards(cards.len(), avail_w, metrics);
        Self {
            cards,
            metrics,
            rects,
            size: (w, h),
            gesture: Gesture::Idle,
            selected: Vec::new(),
            pressed_at_ms: 0,
            events: Vec::new(),
            visible: false,
        }
    }

    pub fn cards(&self) -> &[CardItem] {
        &self.cards
    }

    pub fn rects(&self) -> &[Rect] {
        &self.rects
    }

    pub fn metrics(&self) -> CardMetrics {
        self.metrics
    }

    /// 面板尺寸（宽, 高）。零卡片时为 `(0, 0)`。
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    pub fn gesture(&self) -> Gesture {
        self.gesture
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// 选中了下标 `i` 的卡片吗（渲染选中态用）。
    pub fn is_selected(&self, i: usize) -> bool {
        self.selected.contains(&i)
    }

    /// 当前选中集合：**有序、无重复**，顺序即滑过的先后（也就是拼接顺序）。
    pub fn selected(&self) -> &[usize] {
        &self.selected
    }

    /// 坐标 → 卡片下标。未命中返回 `None`。
    pub fn hit_test(&self, x: f32, y: f32) -> Option<usize> {
        self.rects.iter().position(|r| r.contains(x, y))
    }

    /// 按选中集合取出待拼接的文本（顺序同 [`Self::selected`]）。
    pub fn selected_texts(&self) -> Vec<&str> {
        self.selected
            .iter()
            .filter_map(|&i| self.cards.get(i))
            .map(|c| c.text.as_str())
            .collect()
    }

    /// 显示面板（清空上一次的选中与手势残留）。
    ///
    /// ⚠️ **必须清残留**：上一次拖动留下的选中集合若不清，下次打开面板会带着旧选中态，
    /// 用户看到「还没点就高亮着几张卡」。
    pub fn show(&mut self) {
        self.visible = true;
        self.gesture = Gesture::Idle;
        self.selected.clear();
        self.events.clear();
    }

    /// 隐藏面板并清残留。
    pub fn hide(&mut self) {
        self.visible = false;
        self.gesture = Gesture::Idle;
        self.selected.clear();
    }

    /// 取走累计的意图。
    pub fn drain_events(&mut self) -> Vec<CardPanelEvent> {
        std::mem::take(&mut self.events)
    }

    /// 请求关闭：抛出一个 [`CardPanelEvent::Close`]。
    pub fn request_close(&mut self) {
        self.events.push(CardPanelEvent::Close);
    }

    /// 处理一次指针事件。`now_ms` 是**单调时钟毫秒**，由调用方提供（见模块头）。
    pub fn on_pointer_at(&mut self, ev: PanelPointer, now_ms: u64) {
        match ev.kind {
            PanelMouseKind::Down => {
                self.selected.clear();
                self.gesture = match self.hit_test(ev.x, ev.y) {
                    Some(i) => {
                        self.pressed_at_ms = now_ms;
                        Gesture::Pending { index: i }
                    }
                    // 空白处按下：不进 Pending，抬手也不产生事件。
                    None => Gesture::Idle,
                };
            }

            // 按住移动。`Pending` 期间越过阈值即**升格**为拖动，并把按下那张纳入选中；
            // 之后每次命中新卡都追加（重复命中同一张不重复入列）。
            PanelMouseKind::Drag | PanelMouseKind::Move => {
                self.advance_drag(ev, now_ms);
            }

            PanelMouseKind::Up => {
                match self.gesture {
                    // 从未进入拖动 ⇒ **单击**：取按下那张。
                    Gesture::Pending { index } => {
                        self.events.push(CardPanelEvent::Click(index));
                    }
                    // 进入过拖动 ⇒ **多选**：取整个选中集合（即使只滑过一张）。
                    Gesture::Dragging => {
                        if !self.selected.is_empty() {
                            self.events
                                .push(CardPanelEvent::MultiSelect(self.selected.clone()));
                        }
                    }
                    Gesture::Idle => {}
                }
                self.gesture = Gesture::Idle;
            }

            // 划出面板**不清选中**：用户可能只是划过了边界又划回来，此刻清掉会让
            // 已经选好的几张凭空消失。手势保持，等 `Up` 正常提交。
            PanelMouseKind::Leave => {}
        }
    }

    /// 拖动推进的公共部分（`Drag` 与按住态的 `Move` 都走它）。
    fn advance_drag(&mut self, ev: PanelPointer, now_ms: u64) {
        if let Gesture::Pending { index } = self.gesture {
            let long_enough = now_ms.saturating_sub(self.pressed_at_ms) >= LONG_PRESS_MS;
            if long_enough {
                self.gesture = Gesture::Dragging;
                self.push_selected(index);
            } else {
                return;
            }
        }
        if self.gesture == Gesture::Dragging {
            if let Some(i) = self.hit_test(ev.x, ev.y) {
                self.push_selected(i);
            }
        }
    }

    /// 追加选中：**已存在则不重复入列**（重复即会让拼接结果多出一段）。
    fn push_selected(&mut self, i: usize) {
        if !self.selected.contains(&i) {
            self.selected.push(i);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn panel(n: usize) -> CardPanel {
        let cards = (0..n).map(|i| CardItem::new(format!("卡{i}"))).collect();
        CardPanel::new(cards, 400, CardMetrics::default())
    }

    fn p(kind: PanelMouseKind, x: f32, y: f32) -> PanelPointer {
        PanelPointer::new(kind, x, y)
    }

    /// 第 i 张卡的中心点（用于构造「命中第 i 张」的坐标）。
    fn center(c: &CardPanel, i: usize) -> (f32, f32) {
        let r = c.rects()[i];
        (r.x as f32 + r.w as f32 / 2.0, r.y as f32 + r.h as f32 / 2.0)
    }

    #[test]
    fn layout_wraps_and_sizes_to_used_columns() {
        let m = CardMetrics::default();
        // avail=400 ⇒ inner=384，step=104 ⇒ 每行 4 张（3*104+96=408 > 384，故 3 张）
        let (rects, w, h) = layout_cards(8, 400, m);
        assert_eq!(rects.len(), 8);
        let per_row = (384 + 8) / 104;
        assert_eq!(per_row, 3, "每行张数由内宽算出");
        // 第 4 张换行：y 增大、x 回到左起
        assert_eq!(rects[3].x, rects[0].x, "换行后 x 回到行首");
        assert!(rects[3].y > rects[0].y, "换行后 y 下移");
        // 尺寸按实际列数，不按 avail_w
        assert_eq!(w, m.pad * 2 + 3 * m.card_w + 2 * m.gap_x);
        let rows = 8usize.div_ceil(3) as u32;
        assert_eq!(h, m.pad * 2 + rows * m.card_h + (rows - 1) * m.gap_y);
    }

    #[test]
    fn layout_empty_is_zero_sized() {
        let (rects, w, h) = layout_cards(0, 400, CardMetrics::default());
        assert!(rects.is_empty());
        assert_eq!((w, h), (0, 0));
    }

    #[test]
    fn layout_never_divides_by_zero_on_narrow_width() {
        // 宽度小到放不下一张：每行至少 1 张，不能除零、不能空布局
        let (rects, w, _h) = layout_cards(3, 1, CardMetrics::default());
        assert_eq!(rects.len(), 3);
        assert!(w >= CardMetrics::default().card_w);
    }

    #[test]
    fn hit_test_hits_inside_and_misses_outside() {
        let c = panel(4);
        let (cx, cy) = center(&c, 1);
        assert_eq!(c.hit_test(cx, cy), Some(1));
        // 所有卡的中心都应命中自己
        for i in 0..4 {
            let (x, y) = center(&c, i);
            assert_eq!(c.hit_test(x, y), Some(i), "第 {i} 张中心应命中自己");
        }
        // 左上留白里 ⇒ 不命中
        assert_eq!(c.hit_test(1.0, 1.0), None);
    }

    #[test]
    fn hit_test_right_and_bottom_edges_are_exclusive() {
        let c = panel(2);
        let r = c.rects()[0];
        let right = (r.x + r.w as i32) as f32;
        let bottom = (r.y + r.h as i32) as f32;
        assert!(
            c.hit_test(right - 0.5, r.y as f32 + 1.0).is_some(),
            "内侧应命中"
        );
        // 右/下边界本身不算命中（否则相邻卡共享边界会双命中）
        assert_eq!(c.hit_test(right, r.y as f32 + 1.0), None, "右边界右开");
        assert_eq!(c.hit_test(r.x as f32 + 1.0, bottom), None, "下边界右开");
    }

    #[test]
    fn quick_press_and_release_is_a_single_click() {
        let mut c = panel(4);
        let (x, y) = center(&c, 2);
        c.on_pointer_at(p(PanelMouseKind::Down, x, y), 1000);
        assert_eq!(c.gesture(), Gesture::Pending { index: 2 });
        // 未超阈值就抬手 ⇒ 单击
        c.on_pointer_at(p(PanelMouseKind::Up, x, y), 1010);
        assert_eq!(c.drain_events(), vec![CardPanelEvent::Click(2)]);
        assert_eq!(c.gesture(), Gesture::Idle);
        assert!(c.selected().is_empty(), "单击不产生多选集合");
    }

    #[test]
    fn long_press_then_release_is_a_multi_select() {
        let mut c = panel(4);
        let (x, y) = center(&c, 1);
        c.on_pointer_at(p(PanelMouseKind::Down, x, y), 0);
        // 越过阈值后的一次拖动 ⇒ 升格为拖动，并把按下那张纳入
        c.on_pointer_at(p(PanelMouseKind::Drag, x, y), LONG_PRESS_MS);
        assert_eq!(c.gesture(), Gesture::Dragging);
        assert_eq!(c.selected(), &[1], "按住那张先进选中");
        c.on_pointer_at(p(PanelMouseKind::Up, x, y), LONG_PRESS_MS + 10);
        assert_eq!(c.drain_events(), vec![CardPanelEvent::MultiSelect(vec![1])]);
    }

    #[test]
    fn drag_below_threshold_stays_pending_and_ends_as_click() {
        let mut c = panel(4);
        let (x1, y1) = center(&c, 0);
        let (x2, y2) = center(&c, 2);
        c.on_pointer_at(p(PanelMouseKind::Down, x1, y1), 0);
        // 阈值内划过别的卡：只算移动，不选、不升格
        c.on_pointer_at(p(PanelMouseKind::Drag, x2, y2), LONG_PRESS_MS - 1);
        assert_eq!(c.gesture(), Gesture::Pending { index: 0 });
        assert!(c.selected().is_empty(), "未超阈值不得开始累积");
        c.on_pointer_at(p(PanelMouseKind::Up, x2, y2), LONG_PRESS_MS - 1);
        assert_eq!(
            c.drain_events(),
            vec![CardPanelEvent::Click(0)],
            "仍按单击取按下那张"
        );
    }

    #[test]
    fn drag_accumulates_in_order_without_duplicates() {
        let mut c = panel(6);
        let pts: Vec<(f32, f32)> = (0..6).map(|i| center(&c, i)).collect();
        c.on_pointer_at(p(PanelMouseKind::Down, pts[2].0, pts[2].1), 0);
        c.on_pointer_at(p(PanelMouseKind::Drag, pts[2].0, pts[2].1), LONG_PRESS_MS);
        // 滑过 3、4，再折回 3、2（重复不得再入列）
        for i in [3usize, 4, 3, 2] {
            c.on_pointer_at(
                p(PanelMouseKind::Drag, pts[i].0, pts[i].1),
                LONG_PRESS_MS + 5,
            );
        }
        assert_eq!(c.selected(), &[2, 3, 4], "有序、无重复，顺序即滑过先后");
        c.on_pointer_at(p(PanelMouseKind::Up, pts[2].0, pts[2].1), LONG_PRESS_MS + 9);
        assert_eq!(
            c.drain_events(),
            vec![CardPanelEvent::MultiSelect(vec![2, 3, 4])]
        );
    }

    #[test]
    fn drag_over_gap_keeps_previous_selection() {
        let mut c = panel(3);
        let pts: Vec<(f32, f32)> = (0..3).map(|i| center(&c, i)).collect();
        c.on_pointer_at(p(PanelMouseKind::Down, pts[0].0, pts[0].1), 0);
        c.on_pointer_at(p(PanelMouseKind::Drag, pts[0].0, pts[0].1), LONG_PRESS_MS);
        // 落在卡片之间的空隙（不命中任何卡）：已选中的不得被清掉
        let gap_x = (c.rects()[0].x + c.rects()[0].w as i32) as f32 + 2.0;
        c.on_pointer_at(p(PanelMouseKind::Drag, gap_x, pts[0].1), LONG_PRESS_MS + 3);
        assert_eq!(c.selected(), &[0], "空隙处不改变选中集合");
        c.on_pointer_at(
            p(PanelMouseKind::Drag, pts[1].0, pts[1].1),
            LONG_PRESS_MS + 6,
        );
        assert_eq!(c.selected(), &[0, 1]);
    }

    #[test]
    fn leave_does_not_clear_selection_and_up_still_commits() {
        let mut c = panel(4);
        let pts: Vec<(f32, f32)> = (0..4).map(|i| center(&c, i)).collect();
        c.on_pointer_at(p(PanelMouseKind::Down, pts[0].0, pts[0].1), 0);
        c.on_pointer_at(p(PanelMouseKind::Drag, pts[0].0, pts[0].1), LONG_PRESS_MS);
        c.on_pointer_at(
            p(PanelMouseKind::Drag, pts[1].0, pts[1].1),
            LONG_PRESS_MS + 2,
        );
        assert_eq!(c.selected(), &[0, 1]);
        // 划出面板：手势与选中都保持（用户可能又划回来）
        c.on_pointer_at(p(PanelMouseKind::Leave, -50.0, -50.0), LONG_PRESS_MS + 4);
        assert_eq!(c.gesture(), Gesture::Dragging, "Leave 不结束拖动");
        assert_eq!(c.selected(), &[0, 1], "Leave 不清选中");
        // 面板外抬手：仍按多选提交已选中的
        c.on_pointer_at(p(PanelMouseKind::Up, -50.0, -50.0), LONG_PRESS_MS + 8);
        assert_eq!(
            c.drain_events(),
            vec![CardPanelEvent::MultiSelect(vec![0, 1])]
        );
        assert_eq!(c.gesture(), Gesture::Idle);
    }

    #[test]
    fn press_on_blank_area_produces_nothing() {
        let mut c = panel(2);
        c.on_pointer_at(p(PanelMouseKind::Down, 1.0, 1.0), 0);
        assert_eq!(c.gesture(), Gesture::Idle, "空白处按下不进 Pending");
        c.on_pointer_at(p(PanelMouseKind::Up, 1.0, 1.0), 5);
        assert!(c.drain_events().is_empty(), "空白处抬手不产生意图");
    }

    #[test]
    fn hover_move_without_press_selects_nothing() {
        let mut c = panel(3);
        let (x, y) = center(&c, 1);
        // 未按下时的 Move 不得开始任何手势
        c.on_pointer_at(p(PanelMouseKind::Move, x, y), 0);
        assert_eq!(c.gesture(), Gesture::Idle);
        assert!(c.selected().is_empty());
        assert!(c.drain_events().is_empty());
    }

    #[test]
    fn show_clears_stale_selection_from_previous_open() {
        let mut c = panel(3);
        let pts: Vec<(f32, f32)> = (0..3).map(|i| center(&c, i)).collect();
        c.on_pointer_at(p(PanelMouseKind::Down, pts[0].0, pts[0].1), 0);
        c.on_pointer_at(p(PanelMouseKind::Drag, pts[0].0, pts[0].1), LONG_PRESS_MS);
        assert_eq!(c.selected(), &[0]);

        c.hide();
        c.show();
        assert!(c.is_visible());
        assert!(c.selected().is_empty(), "重开不得带着上次的选中态");
        assert_eq!(c.gesture(), Gesture::Idle);
        assert!(c.drain_events().is_empty(), "重开不得带出上次的意图");
    }

    #[test]
    fn close_request_comes_out_of_the_queue() {
        let mut c = panel(1);
        c.request_close();
        assert_eq!(c.drain_events(), vec![CardPanelEvent::Close]);
        assert!(c.drain_events().is_empty(), "drain 是取走语义");
    }

    #[test]
    fn selected_texts_follow_selection_order() {
        let mut c = panel(3);
        let pts: Vec<(f32, f32)> = (0..3).map(|i| center(&c, i)).collect();
        c.on_pointer_at(p(PanelMouseKind::Down, pts[1].0, pts[1].1), 0);
        c.on_pointer_at(p(PanelMouseKind::Drag, pts[1].0, pts[1].1), LONG_PRESS_MS);
        c.on_pointer_at(
            p(PanelMouseKind::Drag, pts[0].0, pts[0].1),
            LONG_PRESS_MS + 1,
        );
        assert_eq!(c.selected(), &[1, 0], "顺序是滑过先后，不是下标序");
        assert_eq!(c.selected_texts(), vec!["卡1", "卡0"]);
    }
}
