//! 卡片面板的浮层窗口（窗口壳 + 渲染）。
//!
//! [`CardPanel`] 里全是纯逻辑：有哪些卡、指针落在哪张上、拖选选中了哪几张、什么时候提交。
//! 本模块只做三件事：
//!
//! 1. 持有一个 [`LayeredWindow`] 与它的 BGRA 缓冲；
//! 2. 把面板画进缓冲（底板 + 每张卡一个填充矩形，选中卡换底色）；
//! 3. 把 Win32 鼠标消息翻译成 [`PanelPointer`] 喂给 [`CardPanel::on_pointer_at`]。
//!
//! # 可见选中态
//!
//! 选中必须**看得见**——用户看不见自己选了哪几张，等于没有选中态。绘制时按
//! [`CardPanel::rects`] 逐张铺底，再按 [`CardPanel::selected`] 把选中卡换成
//! [`CARD_BG_SELECTED`]（对未选中的 [`CARD_BG`]），这就是全部的选中反馈。
//!
//! ⚠️ 卡片文本尚未绘制，见 [`CardPanelWindow::render`] 的 TODO：目前只画矩形。
//! 「哪张被选中」已经肉眼可见，但卡片内容还看不出来。
//!
//! # `SetCapture` 是承重墙
//!
//! 拖选 = 按下 → 划过多张 → 抬手提交。抬手那一下必须落回**本窗口**才收得到
//! `WM_LBUTTONUP`；而浮层是 `WS_EX_NOACTIVATE`、永远拿不到焦点，光标一旦划出窗口矩形，
//! 抬起消息就发给别的窗口了——本窗口既不知道拖选结束，`Up` 也就永远到不了
//! [`CardPanel::on_pointer_at`]，多选卡在「进行中」提交不出来。
//! 故按下即 `SetCapture(hwnd)`、抬起即 `ReleaseCapture()`（与工具栏拖动同法：这里是
//! 「按下发生在本窗口内」的用法，不是拿 capture 去偷听窗口外的点击）。
//!
//! # 日志
//!
//! 卡片文本是用户输入，**只存在于像素缓冲里**：`info!` 只记卡片张数与窗口生命周期，
//! 不记任何卡片内容（事件本身含下标，也不进 `info!`）。

use std::cell::RefCell;
use std::rc::Rc;

use tracing::{info, warn};

use crate::card_panel::{CardItem, CardMetrics, CardPanel, CardPanelEvent, Rect};
use crate::window::LayeredWindow;
#[cfg(windows)]
use crate::card_panel::{PanelMouseKind, PanelPointer};
#[cfg(windows)]
use crate::sys::{
    GetCursorPos, HWND, LPARAM, LRESULT, POINT, ReleaseCapture, SetCapture, WPARAM, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEMOVE, clamp_to_work_area,
};
// `crate::sys` 只转出窗口/输入那一族，进程运行毫秒数不在其中，故直接取。
#[cfg(windows)]
use windows::Win32::System::SystemInformation::GetTickCount64;
#[cfg(windows)]
use crate::window::WindowMouse;

/// 窗口类名，仅用于诊断日志区分是哪一个浮层。
const CLASS_NAME: &str = "WindInputCardPanel";

/// 卡片换行的可用宽度。面板实际尺寸取 [`CardPanel::size`]（贴合内容），
/// 本值只决定一行放得下几张。
const AVAIL_W: u32 = 420;

/// 建窗时的占位高度。真正的尺寸在每次 `show` 时按面板内容 `resize`。
const INITIAL_H: u32 = 64;

/// 面板底板色（**预乘** BGRA）。
const PANEL_BG: [u8; 4] = [0x1E, 0x1E, 0x1E, 0xF2];
/// 未选中卡底色（预乘 BGRA，不透明）。
const CARD_BG: [u8; 4] = [0x3A, 0x3A, 0x3A, 0xFF];
/// 选中卡底色（预乘 BGRA，不透明）——**唯一**的可见选中态。
const CARD_BG_SELECTED: [u8; 4] = [0xD9, 0x77, 0x1F, 0xFF];

/// 把矩形 `(x, y, w, h)` 铺成纯色，越界部分裁掉。
///
/// 抽成自由函数是为了能在 host 上单测：窗口本身在非 Windows 是空实现，而裁剪算错
/// 会写到缓冲外（panic）或画出半张卡，两种都只能靠这条测试挡住。
fn fill_rect(
    buf: &mut [u8],
    buf_w: u32,
    buf_h: u32,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    color: [u8; 4],
) {
    if w == 0 || h == 0 || buf_w == 0 || buf_h == 0 {
        return;
    }
    // 缓冲比声明的尺寸小（调用方传错）时直接不画，别 panic 在渲染路径上。
    if buf.len() < buf_w as usize * buf_h as usize * 4 {
        return;
    }
    let x0 = (x as i64).max(0);
    let y0 = (y as i64).max(0);
    let x1 = (x as i64 + w as i64).min(buf_w as i64);
    let y1 = (y as i64 + h as i64).min(buf_h as i64);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    for row in y0..y1 {
        let base = (row as usize * buf_w as usize) * 4;
        for col in x0..x1 {
            let i = base + col as usize * 4;
            buf[i..i + 4].copy_from_slice(&color);
        }
    }
}

/// 当前时刻（毫秒）。面板的长按/拖动判据都按毫秒比较，故直接用系统启动起的单调计数。
#[cfg(windows)]
fn now_ms() -> u64 {
    unsafe { GetTickCount64() }
}

/// 面板落在光标右下角，并钳进工作区（钳不住就会跑到屏幕外，用户再也够不着）。
#[cfg(windows)]
fn place_near_cursor(w: u32, h: u32) -> (i32, i32) {
    let mut p = POINT::default();
    let at_cursor = unsafe { GetCursorPos(&mut p) }.is_ok();
    let (x, y) = if at_cursor { (p.x + 12, p.y + 12) } else { (0, 0) };
    clamp_to_work_area(x, y, w, h)
}
#[cfg(not(windows))]
fn place_near_cursor(_w: u32, _h: u32) -> (i32, i32) {
    (0, 0)
}

/// 鼠标处理器：Win32 消息 → [`PanelPointer`] → [`CardPanel`]。
///
/// 它不持有窗口，只共享面板与出参队列（都是 `Rc`），故 `on_message` 里没有任何自引用。
#[cfg(windows)]
struct CardPanelMouse {
    panel: Rc<RefCell<CardPanel>>,
    /// 每次事件后 [`CardPanel::drain_events`] 的落点，由窗口结构在 `pump_events` 里读走。
    ///
    /// ⚠️ 必须经队列回窗口，不能就地 `hide()`：`hide()` 要同时收掉窗口，而处理器只有
    /// `hwnd`、没有窗口结构。
    pending: Rc<RefCell<Vec<CardPanelEvent>>>,
    /// 是否处于「左键按住」态：只有按住时的 `WM_MOUSEMOVE` 才算拖动。
    down: bool,
}

#[cfg(windows)]
impl CardPanelMouse {
    /// 派发一次指针事件，并把面板吐出来的事件收进队列。
    fn dispatch(&mut self, kind: PanelMouseKind, x: f32, y: f32) {
        let drained = {
            let mut panel = self.panel.borrow_mut();
            panel.on_pointer_at(PanelPointer::new(kind, x, y), now_ms());
            panel.drain_events()
        };
        if !drained.is_empty() {
            self.pending.borrow_mut().extend(drained);
        }
    }
}

#[cfg(windows)]
impl WindowMouse for CardPanelMouse {
    fn on_message(
        &mut self,
        hwnd: HWND,
        msg: u32,
        _wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<LRESULT> {
        // 客户区坐标（本窗口无边框、无子控件，与缓冲像素一一对应）。
        let x = (lparam.0 & 0xFFFF) as i16 as f32;
        let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
        match msg {
            WM_LBUTTONDOWN => {
                // ★ 必须捕获：否则划出窗口后抬手收不到 UP，多选永远提交不了。
                unsafe {
                    let _ = SetCapture(hwnd);
                }
                self.down = true;
                self.dispatch(PanelMouseKind::Down, x, y);
                Some(LRESULT(0))
            }
            // 按住即拖动。捕获在手时光标划出窗口也照样收得到，故不必依赖 `WM_MOUSELEAVE`
            // 兜底（面板也不消费 `Leave`：移出去只是不选中新卡，松手仍要能提交）。
            WM_MOUSEMOVE => {
                let kind = if self.down {
                    PanelMouseKind::Drag
                } else {
                    PanelMouseKind::Move
                };
                self.dispatch(kind, x, y);
                Some(LRESULT(0))
            }
            WM_LBUTTONUP => {
                self.dispatch(PanelMouseKind::Up, x, y);
                if self.down {
                    self.down = false;
                    unsafe {
                        let _ = ReleaseCapture();
                    }
                }
                Some(LRESULT(0))
            }
            _ => None,
        }
    }
}

/// 卡片面板浮层窗口。
pub struct CardPanelWindow {
    window: LayeredWindow,
    /// 与鼠标处理器**共享**：`show` 里整块替换（`CardPanel` 的卡片只能在 `new` 时给），
    /// 处理器的 `Rc` 必须一直指向当前那块。
    panel: Rc<RefCell<CardPanel>>,
    pending: Rc<RefCell<Vec<CardPanelEvent>>>,
    /// 最近一批事件，供外部读（见 [`CardPanelWindow::last_events`]）。
    last_events: Vec<CardPanelEvent>,
    visible: bool,
}

impl CardPanelWindow {
    /// 建窗口 + 面板，并挂上鼠标处理器。窗口此刻仍是隐藏的。
    pub fn new() -> Result<Self, String> {
        let window = LayeredWindow::create(None, AVAIL_W, INITIAL_H, CLASS_NAME)?;
        let panel = Rc::new(RefCell::new(CardPanel::new(
            Vec::new(),
            AVAIL_W,
            CardMetrics::default(),
        )));
        let pending: Rc<RefCell<Vec<CardPanelEvent>>> = Rc::new(RefCell::new(Vec::new()));
        #[cfg(windows)]
        {
            let handler: Rc<RefCell<dyn WindowMouse>> = Rc::new(RefCell::new(CardPanelMouse {
                panel: Rc::clone(&panel),
                pending: Rc::clone(&pending),
                down: false,
            }));
            window.register_mouse(handler);
        }
        Ok(Self {
            window,
            panel,
            pending,
            last_events: Vec::new(),
            visible: false,
        })
    }

    /// 显示面板：装载卡片、`CardPanel::show()`、贴内容尺寸并上屏。
    ///
    /// `cards` 按**时间倒序**给（顺序由 [`CardPanel`] 原样保持）。
    pub fn show(&mut self, cards: Vec<CardItem>) {
        let count = cards.len();
        let (w, h) = {
            let mut panel = self.panel.borrow_mut();
            // 整块替换：卡片只能在 `new` 时给，而窗口要能反复以新卡片显示。
            *panel = CardPanel::new(cards, AVAIL_W, CardMetrics::default());
            panel.show();
            panel.size()
        };
        if w > 0 && h > 0 {
            self.window.resize(w, h);
        }
        self.render();
        let (sw, sh) = self.window.size();
        let (x, y) = place_near_cursor(sw, sh);
        self.window.show(x, y);
        self.visible = true;
        info!(cards = count, "卡片面板显示");
    }

    /// 隐藏面板：面板侧 `hide()` + 窗口隐藏。
    pub fn hide(&mut self) {
        let was_visible = self.visible;
        self.panel.borrow_mut().hide();
        self.window.hide();
        self.visible = false;
        if was_visible {
            info!("卡片面板隐藏");
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// 上屏：先收事件（遇到 `Close` 即隐藏），再重绘并 `UpdateLayeredWindow`。
    /// UI 主循环每轮调一次即可。
    pub fn update(&mut self) {
        self.pump_events();
        if !self.visible {
            return;
        }
        self.render();
        if let Err(e) = self.window.update() {
            warn!(error = %e, "卡片面板上屏失败");
        }
    }

    /// 把鼠标处理器攒下的事件搬进 `last_events`；含 `Close` 就地隐藏面板。
    pub fn pump_events(&mut self) {
        let arrived: Vec<CardPanelEvent> = self.pending.borrow_mut().drain(..).collect();
        if arrived.is_empty() {
            return;
        }
        let close = arrived
            .iter()
            .any(|e| matches!(e, CardPanelEvent::Close));
        self.last_events.extend(arrived);
        if close {
            self.hide();
        }
    }

    /// 最近一批事件（`pump_events` 之后有效，供外部读）。
    pub fn last_events(&self) -> &[CardPanelEvent] {
        &self.last_events
    }

    /// 取走最近一批事件（避免调用方反复处理同一批）。
    pub fn take_events(&mut self) -> Vec<CardPanelEvent> {
        std::mem::take(&mut self.last_events)
    }

    /// 把面板画进 BGRA 缓冲：底板 + 每张卡一个填充矩形（选中卡另一种底色）。
    ///
    /// 整块重绘（清空 → 底板 → 卡片）：卡片数是个位数，重绘成本远小于一次上屏。
    ///
    /// TODO(文本): 卡片文本尚未绘制。文本要走 `text::dwrite::TextRenderer`（软键盘
    /// 用的就是它），需要按它的测量/绘制签名接线（缓冲偏移、字号、每卡裁剪），
    /// 本步只保证「哪张被选中」肉眼可见。
    fn render(&mut self) {
        let panel = self.panel.borrow();
        let rects: Vec<(usize, Rect)> = panel.rects().iter().copied().enumerate().collect();
        let selected: Vec<usize> = panel.selected().to_vec();
        drop(panel);

        self.window.clear();
        let (bw, bh) = self.window.size();
        let buf = self.window.buffer_mut();
        fill_rect(buf, bw, bh, 0, 0, bw, bh, PANEL_BG);
        for (i, r) in rects {
            let color = if selected.contains(&i) {
                CARD_BG_SELECTED
            } else {
                CARD_BG
            };
            fill_rect(buf, bw, bh, r.x, r.y, r.w, r.h, color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_rect_clips_to_buffer() {
        let (w, h) = (4u32, 2u32);
        let mut buf = vec![0u8; (w * h * 4) as usize];
        // 左上角越界的 3×3：只该留下 (0,0)..(1,1) 这 2×2。
        fill_rect(&mut buf, w, h, -1, -1, 3, 3, [1, 2, 3, 4]);
        let px = |x: u32, y: u32| {
            let i = ((y * w + x) * 4) as usize;
            [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
        };
        assert_eq!(px(0, 0), [1, 2, 3, 4]);
        assert_eq!(px(1, 1), [1, 2, 3, 4]);
        assert_eq!(px(2, 0), [0, 0, 0, 0]);
        assert_eq!(px(0, 1), [1, 2, 3, 4]);
        assert_eq!(px(3, 1), [0, 0, 0, 0]);
    }

    #[test]
    fn fill_rect_skips_empty_and_offscreen() {
        let mut buf = vec![7u8; 4 * 4 * 4];
        let before = buf.clone();
        fill_rect(&mut buf, 4, 4, 0, 0, 0, 10, [1, 1, 1, 1]); // 零宽
        fill_rect(&mut buf, 4, 4, 10, 10, 2, 2, [1, 1, 1, 1]); // 完全在右下外
        fill_rect(&mut buf, 4, 4, -5, -5, 1, 1, [1, 1, 1, 1]); // 完全在左上外
        assert_eq!(buf, before);

        // 缓冲比声明的尺寸小：宁可整块不画，也不能写到缓冲外（渲染路径上 panic）。
        let mut tiny = vec![7u8; 8];
        fill_rect(&mut tiny, 4, 4, 0, 0, 4, 4, [1, 1, 1, 1]);
        assert_eq!(tiny, vec![7u8; 8]);
    }

    #[test]
    fn panel_background_covers_buffer() {
        let (w, h) = (3u32, 3u32);
        let mut buf = vec![0u8; (w * h * 4) as usize];
        fill_rect(&mut buf, w, h, 0, 0, w, h, PANEL_BG);
        for px in buf.chunks_exact(4) {
            assert_eq!(px, &PANEL_BG);
        }
    }

    /// 分层窗口要求**预乘** alpha：任一分量超过 alpha 就会被 `UpdateLayeredWindow`
    /// 画成发光/白边。顺带钉住「选中态与常态不是同一个颜色」。
    #[test]
    fn colors_are_premultiplied_and_selection_is_distinct() {
        for c in [PANEL_BG, CARD_BG, CARD_BG_SELECTED] {
            assert!(
                c[0] <= c[3] && c[1] <= c[3] && c[2] <= c[3],
                "颜色 {c:?} 未预乘"
            );
        }
        assert_ne!(CARD_BG, CARD_BG_SELECTED, "选中态必须与常态不同色");
    }
}
