//! # 这个项目在做什么
//!
//! 一句话：把 Dioxus 当成一个「只吐 UI 指令流的 diff 引擎」用，
//! 自己写一个 renderer，把指令流翻译成 Qt Quick 对象树的建/挂/改。
//!
//!
//! ## 三块东西的分工
//!
//! ```text
//!  Dioxus Core（纯 Rust，crate 依赖）
//!      app() + rsx! 编译出来的静态模板
//!      VirtualDom::rebuild / render_immediate
//!          │
//!          │  产出 Mutation 流：LoadTemplate / SetAttribute /
//!          │  AppendChildren / ReplacePlaceholder / ...
//!          ▼
//!  QtRenderer（本文件，Rust）
//!      实现 WriteMutations：把每条 Mutation 翻译成
//!      「建对象 / 挂父子 / 写属性」这三类动作，
//!      并自己维护一棵跟 Dioxus 下标一致的节点树
//!          │
//!          │  cxx bridge（本文件的 mod ffi）
//!          ▼
//!  QtHost（cpp/qt_host.h / cpp/qt_host.cpp，C++）
//!      唯一真正碰 Qt 的地方：QQuickWindow + QQmlEngine，
//!      每种控件是一段内联 QML 字符串，
//!      用 QQmlComponent::setData + create() 实例化
//! ```
//!
//!
//! ## 没有任何 QML 文件
//!
//! 没有 main.qml，没有 QQmlApplicationEngine，load() 一个 QML 文件都没有。
//! 屏幕上那个 Text 和 Rectangle 全是 Dioxus 的 Mutation 驱动着建出来的。
//! C++ 侧只提供「原子操作」，不提供 UI、不提供布局。
//!
//!
//! ## 编号有三层，别搞混
//!
//! ```text
//!  ElementId        Dioxus 的编号，只在 root/动态节点上有；
//!                   模板深处的静态元素默认没有 id（要 AssignId 才有）
//!      ↓  element_to_node
//!  node id          本 renderer 的树节点编号；占位节点也有，
//!                   因为 Dioxus 的 path 数是它在模板里的位置
//!      ↓  Node::object
//!  QQuickItem*      C++ 实例化出来的那个 Qt 对象的裸指针，
//!                   Rust 直接握着它去 mount / set_property / set_geometry
//! ```
//!
//! 以前这第三层是一个 u64 object id，C++ 侧再用 unordered_map 查回指针；
//! 现在指针本身就是句柄，少一次查找，也少一本要维护的账。
//! 代价是 Rust 这边拿的是 `*mut QQuickItem` —— 裸指针没有生命周期，
//! 「对象还活着吗」这个问题只能靠 Qt 的 parent tree 回答。
//!
//!
//! ## 阅读顺序建议
//!
//! 1. `app()` —— counter demo 那一屏，加三个按钮
//! 2. `mod ffi` —— Rust ↔ C++ 的双向接口：21 个正向（1 个造 QtHost + 20 个方法）
//!    + 1 个 C++ 自由函数 + 10 个 C++ 回调 Rust（1 个回路 poll + 1 个窗口尺寸
//!    + 8 支事件）
//! 3. `struct QtRenderer` 的字段注释 —— 树账（nodes/stack）、
//!    id 账（element_to_node/instance_paths）、布局账（tree/root_taffy）各记什么
//! 4. `impl WriteMutations` —— 指令如何变成 Qt 调用（核心）
//! 5. `main()` —— 拼起来的顺序
//!
//!
//! ## 当前边界（重要）
//!
//! 已经通到底的：
//!
//! - `WriteMutations` 五条指令全实现了（LoadTemplate / SetAttribute /
//!   AppendChildren / ReplacePlaceholder / 三条 diff 分支 / Remove / AssignId），
//!   配套的 C++ `destroy`、`place`（同父移动）也在；
//! - `assign_node_id` 按 path 找回那个静态节点，见 `instance_paths`；
//! - 回路是自己转的：Rust 的 `QtWaker` → `schedule_pump_poll`
//!   → `qt_pump_poll` 把 `wait_for_work` poll 到 Pending 为止，空闲时零轮询；
//! - 输入侧走真事件，而且是**唯一的**纸条来源：
//!   `create_event_listener` 按名字在 QQuickItem 上装 `InputFilter`，
//!   filter 认出 Qt 的那一套 QEvent → 对应那支 `qt_*_event` 回调
//!   → `runtime().handle_event(name, ...)` → listener 写 signal
//!   → wake → 渲染。以前那个每 200ms 推一格的假事件源
//!   （QTimer + DIFF_SCRIPT + send_click_to）整条删掉了，
//!   `app()` 现在就是个 counter，不点不动。
//!   「按下+在同一个元素里松开才算一次 click」这套判据照抄
//!   `QQuickMouseArea::setPressed()`，对照写在 cpp/qt_host.cpp 里
//!   InputFilter 的上方；名字→事件类型→回调那三者的对应关系是
//!   cpp 的 `EVENT_SPECS` 和 Rust 的 `SUPPORTED_EVENTS` 两张表，同一口径。
//! - 支持的八支（Qt 真给得出事实的）：鼠标全家
//!   （mousedown/mouseup/click/dblclick/mousemove/mouseover/mouseenter/
//!   mouseout/mouseleave/contextmenu）、指针全家（pointerdown/up/move/
//!   over/out/enter/leave/auxclick）、wheel、keydown/keyup/keypress、
//!   focus/blur/focusin/focusout、touchstart/move/end/cancel、
//!   resize（盯 QQuickItem 的尺寸信号）、mounted（渲染批次结束后补发）。
//!   负载是三套坐标（元素本地 / 窗口 / 屏幕）+ Qt 的按键位掩码 + 修饰键位，
//!   冒泡那个布尔查 dioxus-core-types 的 `event_bubbles()`，不自己拍。
//! - 窗口尺寸变化会重排：`QEvent::Resize` → `ResizeFilter` → `qt_window_resized`
//!   → `relayout`，这条链不经过 Dioxus（视口是布局的输入，不是应用状态）。
//!   配套的是尺寸属性现在认百分比（`"100%"`）和 `flex_grow`，
//!   `app()` 的根节点写死 100%、中间那块按钮 `flex_grow: "1"`，
//!   所以拖动窗口是屏幕上看得见的伸缩。
//! - 属性值四种形态都收：`AttributeValue::{Text, Int, Float, Bool}` 各包成
//!   对应类型的 QVariant 写给 Qt（`variant_of`），布局那条路认数字
//!   （`width: 110` 不再要引号）；`AttributeValue::None` 是**撤掉这个属性** ——
//!   布局属性从账本里摘掉（回到 auto），Qt 属性由 C++ 写回
//!   「对象刚建出来时的值」（那份抄存在对象的动态属性上，见 clear_property）。
//!   `Any` 翻译不了、`Listener` 不该走这条路，两者都在门口拒绝并打出理由。
//!
//! 还缺的：
//!
//! - 其余 13 支事件类（drag/drop、scroll、键盘之外的 form/input、
//!   clipboard、composition、media、animation/transition、visible、
//!   selection、toggle…）在 `create_event_listener` 门口被拒绝并打出理由，
//!   转换器里对应那几支保留 panic —— 缺的是 Qt 侧的事实来源或本项目的设施，
//!   静默造一份假数据比当场炸难查。理由表在下面 `unsupported_reason`。
//! - keydown 那三支要 item 能拿 active focus（注册时把 focusPolicy
//!   开成 ClickFocus），但按键的 left/right 位置 Qt 给不出，一律报 Standard；
//!   `is_composing` 要 QInputMethod 才有，报 false。
//! - 字体还是 Qt 自己的默认值，没和 `measure_item` 用同一个来源
//!   （现在两边都是「item 上那个默认字体」，所以量出来刚好对得上；
//!   一旦 font 变成 Rust 下发的样式，两处要一起改）。
//! - 因为事件源只剩鼠标/键盘这些真输入，offscreen 跑起来之后日志里
//!   不会再有渲染 —— 回路醒着但没人叫醒它。click→handle_event→渲染那一段
//!   的回归测试搬到了 `mod tests` 里（不碰 Qt，直接在 VirtualDom 上派发事件）。

use cxx::UniquePtr;

use cxx_qt_lib::QGuiApplication;

use dioxus::dioxus_core::{
    AttributeValue,
    ElementId,
    Event,
    RuntimeGuard,
    Template,
    TemplateAttribute,
    TemplateNode,
    VirtualDom,
    WriteMutations,
};

use dioxus::prelude::*;

// Signal 的写方法（set / apply / replace）都在 WritableExt 上，
// dioxus 的 prelude 不带它（要 dioxus 的 "signals" feature 才转发），
// 而本项目本来就直接依赖 dioxus-signals。
use dioxus_signals::WritableExt;

// dioxus-html 那套事件类型：本项目要自己造一个 PlatformEventData 的负载，
// 并且要装一个 HtmlEventConverter，所以这些名字都得进来。
use dioxus::html::{
    AnimationData,
    CancelData,
    ClipboardData,
    CompositionData,
    DragData,
    FocusData,
    FormData,
    HasMouseData,
    HtmlEventConverter,
    ImageData,
    InteractionElementOffset,
    InteractionLocation,
    KeyboardData,
    MediaData,
    Modifiers,
    ModifiersInteraction,
    MouseData,
    MountedData,
    PlatformEventData,
    PointerData,
    PointerInteraction,
    ResizeData,
    ScrollData,
    SelectionData,
    ToggleData,
    TouchData,
    TransitionData,
    VisibleData,
    WheelData,
    set_event_converter,
};

// 坐标 / 按键这两个子模块没在 dioxus-html 的根上 re-export
// （lib.rs:29-30 只有 `pub mod geometry;` 和 `pub mod input_data;`），
// 所以得写全路径。
use dioxus::html::geometry::{
    ClientPoint,
    ElementPoint,
    PagePoint,
    ScreenPoint,
};

use dioxus::html::input_data::{
    MouseButton,
    MouseButtonSet,
    decode_mouse_button_set,
};

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;
use std::task::{
    Context,
    Wake,
    Waker,
};


use taffy::prelude::{
    AvailableSpace,
    Dimension,
    Display,
    FlexDirection,
    NodeId,
    Size,
    Style,
    TaffyTree,
    length,
};

// 叶子节点的默认测量逻辑：把「测出来的尺寸」和 style 里的
// width / height、可用空间夹在一起，算出最终的 LayoutOutput。
use taffy::compute_leaf_layout;


// ============================================================
// Rust <-> C++ Qt bridge
// ============================================================
//
// 这十六个函数就是 renderer 能对 Qt 做的全部事情。
// 一次完整的首次渲染，调用顺序是：
//
//     new_qt_host()          建 QtHost：内部有 QQuickWindow + QQmlEngine
//     show()                 窗口先显示，之后边建边能看见
//     root_item()            问窗口根（contentItem）要指针，node 0 用它
//
//     create_text /
//     create_rectangle       每碰到一个元素就实例化一个 Qt 对象；
//     / create_column        此刻它还没有 parent，等于屏幕上不存在
//     set_property()         写属性。静态属性在建对象时就写，
//                            动态属性由 Dioxus 稍后补发
//     mount()                挂进 visual tree —— 到这一步才真的可见
//     stack_before()         调同一 parent 下的叠放顺序
//     destroy()              diff 之后把这棵子树从 Qt 里拿掉
//
//     measure_item()         布局时问 Qt「这个 item 要多大」
//                            （文本从 item 自己身上读，Rust 不另存一份）
//     is_text_item()         只有对照诊断用：这个 item 有没有可量的文字
//     implicit_width/height  只用于对照：读 Qt 对象自己算的尺寸
//     window_width/height    视口尺寸
//
// 注意「建对象」和「挂到树上」被刻意拆成两步：
// Dioxus 会先把一棵子树整个 LoadTemplate 出来（还没 parent），
// 之后才用 AppendChildren / ReplacePlaceholder 决定它挂哪、排第几。

#[cxx::bridge]
mod ffi {
    // C++ 侧那个同名 struct（cpp/qt_host.h），字段名必须一一对上。
    #[derive(Debug)]
    struct TextMeasure {
        width: f64,
        height: f64,
    }

    unsafe extern "C++" {
        include!(<QQuickItem>);
        type QQuickItem;
    }

    unsafe extern "C++" {
        include!("cxx-qt-lib/qvariant.h");
        /// QString from cxx_qt_lib
        type QVariant = cxx_qt_lib::QVariant;
    }
    unsafe extern "C++" {
        include!(
            "test_dioxus_qt/cpp/qt_host.h"
        );

        // C++ 侧那个类的 opaque 类型。
        // Rust 只能通过 UniquePtr<QtHost> 持有它，
        // 里面的 Qt 对象 Rust 完全看不见。
        type QtHost;

        // 只能在 QGuiApplication 建好之后调用（见 main()）。
        fn new_qt_host()
            -> UniquePtr<QtHost>;

        // QQuickWindow::show()。
        // 这里先 show 再渲染，是为了跑起来能肉眼看到内容一点点出现。
        fn show(
            self: Pin<&mut QtHost>
        );

        // 窗口根 = window.contentItem()。
        //
        // node 0 的 object 存的就是它。以前这里不需要问：
        // Rust 记一个约定的 id 0，C++ 那边再翻译成 contentItem。
        fn root_item(
            self: &QtHost,
        ) -> *mut QQuickItem;

        // 实例化一个 QtQuick.Text。
        // 返回值是这个 Qt 对象的裸指针（nullptr = 创建失败）。
        //
        // 两个地方会调它：
        //   - 模板里的静态文本（TemplateNode::Text）
        //   - 动态文本 / <qt-text> 元素本身
        fn create_text(
            self: Pin<&mut QtHost>,
            text: &str,
        ) -> *mut QQuickItem;

        // 实例化一个 QtQuick.Rectangle。
        // 没有属性需要从 Rust 传：颜色是画的事，
        // 而宽高是布局的事，走 set_geometry。
        fn create_rectangle(
            self: Pin<&mut QtHost>,
        ) -> *mut QQuickItem;

        // 实例化一个裸 Item 当容器（qt-column）。
        // QML 侧不带 Layout / anchors —— 孩子摆哪里全由 taffy 决定。
        fn create_column(
            self: Pin<&mut QtHost>,
        ) -> *mut QQuickItem;

        // 把 child 挂到 parent 下面，正式进 visual tree。
        //
        // parent 就是一个真实的 QQuickItem 指针：
        // 挂到窗口根时传的是 root_item() 拿到的那个 contentItem，
        // 不再有「id 0 表示窗口根」那层约定。
        unsafe fn mount(
            self: Pin<&mut QtHost>,
            parent: *mut QQuickItem,
            child: *mut QQuickItem,
        );

        // 把这个对象从两棵树上摘掉并安排释放。
        //
        // 一次调用管一整棵子树：mount 时 setParent 过，
        // 孩子是 QObject 树里的人，~QObject 递归删。
        // 所以 Rust 这边是「remove 那个节点」调一次，
        // 子树里剩下的节点只清自己的账，不再碰 Qt。
        unsafe fn destroy(
            self: Pin<&mut QtHost>,
            item: *mut QQuickItem,
        );

        // 让 child 在叠放顺序上排到 sibling 前面。
        //
        // 为什么需要：mount 只能往末尾追加，
        // 而 ReplacePlaceholder 要把新节点插回坑位原来的位置，
        // 所以挂完还要再往前挪一下。
        unsafe fn stack_before(
            self: Pin<&mut QtHost>,
            child: *mut QQuickItem,
            sibling: *mut QQuickItem,
        );

        // 把 child 放到 parent 的孩子列表里的第 index 个位置该长的那样：
        // sibling 非空就排在它前面，sibling 为空就排到末尾。
        //
        // 为什么不是 mount + stack_before：
        //   - setParentItem 对同一个父亲直接 return
        //     （qquickitem.cpp:2757-2758），移动不动；
        //   - stackBefore(nullptr) 只是 qWarning，什么也不做
        //     （qquickitem.cpp:2925-2929），「插到末尾」也没法表达。
        // keyed 列表的移动两条都撞上了 —— 见 child_order 的 ✗。
        unsafe fn place(
            self: Pin<&mut QtHost>,
            parent: *mut QQuickItem,
            child: *mut QQuickItem,
            sibling: *mut QQuickItem,
        );

        // QObject::setProperty(name, value)。
        //
        // value 现在是**带类型**的 QVariant：Text → QString、Int → qlonglong、
        // Float → double、Bool → bool（Rust 侧 variant_of 包的）。
        // 类型对不上时 Qt 自己会做数值转换（比如 Int 写进 qreal radius），
        // 但字符串往 bool 属性里塞 "false" 会转成 true，
        // 所以「把类型包对」这件事必须在 Rust 侧做完。
        //
        // 属性名直接对上 Qt 的属性名：
        // Dioxus 侧写 text: "..."，这里就落到 QtQuick.Text::text。
        // 写不进去的（这个对象上没这个属性，或者类型转不过去）
        // 会在 C++ 侧 qWarning。
        //
        // 第一次写某个属性之前，C++ 会把该属性当时的值抄存一份，
        // 供 clear_property 用 —— 见 cpp/qt_host.cpp。
        unsafe fn set_property(
            self: Pin<&mut QtHost>,
            item: *mut QQuickItem,
            name: &str,
            value: &QVariant,
        );

        // 「这个属性被撤掉了」（AttributeValue::None）。
        //
        // 语义是回到**这个对象刚建出来时的值**，靠的是上面那份抄存。
        // 不能走 QObject::resetProperty：我们用的那几个属性
        // （Rectangle::color、Text::text、Item::visible/opacity）
        // 在 Q_PROPERTY 里都没声明 RESET
        // （qquickrectangle_p.h:133、qquicktext_p.h:33、qquickitem.h:79-81），
        // 那条路对它们一律返回 false。
        // 没抄存过（从来没写过）就是无事发生，日志里会说明。
        unsafe fn clear_property(
            self: Pin<&mut QtHost>,
            item: *mut QQuickItem,
            name: &str,
        );

        // 布局结果写给 Qt：几何的唯一入口。
        //
        // 单独开一个函数而不是走 set_property，是为了让「几何」
        // 有一个明确的出口 —— 五个数字一次写完，也省掉属性名查找。
        unsafe fn set_geometry(
            self: Pin<&mut QtHost>,
            item: *mut QQuickItem,
            x: f64,
            y: f64,
            width: f64,
            height: f64,
        );

        // 视口尺寸问 Qt 要，免得 Rust 和 C++ 各写一个数字。
        fn window_width(
            self: &QtHost,
        ) -> f64;

        fn window_height(
            self: &QtHost,
        ) -> f64;

        // 量一个 item 要多大 —— 文本从 item 自己身上取（QQuickText::text()），
        // 算法和 QQuickText 同一套（QTextLayout + design metrics），
        // 见 cpp/qt_host.cpp 的 measure_layout。
        //
        // 不是文本项、或者文本是空的，返回 0x0：
        // 那种节点的尺寸本来就该由 style 里的 width / height 决定。
        //
        // max_width <= 0：不限宽，折一行，宽度 = Qt 的 implicitWidth 口径。
        // max_width >  0：按这个宽度折行，返回的是「用到的宽度」
        //                （最宽那一行，且不超过 max_width）和总高度。
        unsafe fn measure_item(
            self: &QtHost,
            item: *mut QQuickItem,
            max_width: f64,
        ) -> TextMeasure;

        // 这个 item 是不是「有内容可量的文本项」。
        // 布局不需要它（量不出来自然 0x0），只有对照诊断需要。
        unsafe fn is_text_item(
            self: &QtHost,
            item: *mut QQuickItem,
        ) -> bool;

        // 下面两个只用于对照：读 Qt 对象自己报的 implicit size。
        // 读它会触发 QQuickText 同步算一次尺寸，所以没 polish 也准。
        unsafe fn implicit_width(
            self: &QtHost,
            item: *mut QQuickItem,
        ) -> f64;

        unsafe fn implicit_height(
            self: &QtHost,
            item: *mut QQuickItem,
        ) -> f64;

        // 诊断用：读 Qt 侧 childItems 的实际顺序（对象地址）。
        // 见 cpp/qt_host.h 的注释 —— 插到中间这件事，从坐标上看不出来。
        unsafe fn child_order(
            self: &QtHost,
            parent: *mut QQuickItem,
        ) -> Vec<u64>;

        // 给一个 item 装一个事件监听器。
        //
        // name 是我们查过表、确认支持的那个事件名（已剥掉 on，比如 "click"），
        // C++ 侧的 EVENT_SPECS 按这个名字决定：吃什么 QEvent、送回来哪一支回调、
        // 以及这个 item 要打开哪些输入能力（命中测试的入场券 / hover / touch / 焦点）。
        unsafe fn add_event_listener(
            self: Pin<&mut QtHost>,
            item: *mut QQuickItem,
            name: &str,
            element: u32,
        );

        unsafe fn remove_event_listener(
            self: Pin<&mut QtHost>,
            item: *mut QQuickItem,
            name: &str,
        );
    }

    // C++ 的自由函数：把「该 poll 一次了」排队投进主线程的事件循环。
    // 谁调它：下面的 QtWaker::wake_by_ref —— 也就是 dioxus 发纸条的那一刻
    // （futures-channel: unbounded_send → queue_push_and_signal
    //  → recv_task.wake()，mpsc/mod.rs:457）。
    // 为什么它是自由函数、而且为什么只能排队：见 cpp/qt_host.h 的注释。
    unsafe extern "C++" {
        include!("test_dioxus_qt/cpp/qt_host.h");

        fn schedule_pump_poll();
    }

    // 反方向：C++ 调 Rust。
    //
    // cxx 把 `extern "Rust"` 里的自由函数在生成头里声明成**全局** C++ 函数
    // （`void qt_pump_poll() noexcept;` 这种），C++ include 生成头就能直接调。
    extern "Rust" {
        // 回路本身：poll 一次 wait_for_work，醒了才渲染。
        fn qt_pump_poll();

        // 窗口尺寸变了：几何的唯一作者是 relayout，所以重算也要有人在这里喊。
        fn qt_window_resized(
            width: f64,
            height: f64,
        );

        // 八支事件回调，一支一类负载（对应 dioxus-html 的 MouseData/
        // PointerData/WheelData/KeyboardData/FocusData/TouchData/ResizeData）。
        //
        // 前三支的坐标都是三套：元素本地、窗口内（scene）、屏幕（global）。
        // button/buttons/modifiers 一律交 Qt 的位掩码，Rust 侧 decode ——
        // Qt 那几位和浏览器 MouseEvent.buttons 同序。
        fn qt_mouse_event(
            name: &str,
            element: u32,
            local_x: f64,
            local_y: f64,
            window_x: f64,
            window_y: f64,
            screen_x: f64,
            screen_y: f64,
            button: i32,
            buttons: i32,
            modifiers: i32,
        );

        #[allow(clippy::too_many_arguments)]
        fn qt_pointer_event(
            name: &str,
            element: u32,
            local_x: f64,
            local_y: f64,
            window_x: f64,
            window_y: f64,
            screen_x: f64,
            screen_y: f64,
            button: i32,
            buttons: i32,
            modifiers: i32,
            pointer_id: i32,
            pointer_type: &str,
            is_primary: bool,
            pressure: f64,
            width: f64,
            height: f64,
        );

        #[allow(clippy::too_many_arguments)]
        fn qt_wheel_event(
            name: &str,
            element: u32,
            local_x: f64,
            local_y: f64,
            window_x: f64,
            window_y: f64,
            screen_x: f64,
            screen_y: f64,
            angle_x: i32,
            angle_y: i32,
            pixel_x: i32,
            pixel_y: i32,
            buttons: i32,
            modifiers: i32,
        );

        fn qt_key_event(
            name: &str,
            element: u32,
            key: &str,
            code: &str,
            location: i32,
            repeat: bool,
            modifiers: i32,
        );

        fn qt_focus_event(
            name: &str,
            element: u32,
        );

        // points 是步长 6 的扁平数组（local x/y、window x/y、screen x/y），
        // ids/forces 和它平行；changed 是「这一次变化的点」的下标。
        fn qt_touch_event(
            name: &str,
            element: u32,
            ids: Vec<i32>,
            points: Vec<f64>,
            forces: Vec<f64>,
            changed: Vec<i32>,
            modifiers: i32,
        );

        fn qt_resize_event(
            name: &str,
            element: u32,
            width: f64,
            height: f64,
        );
    }
}


type QQuickItemId = *mut ffi::QQuickItem;

// ============================================================
// Dioxus 侧的原语：Text
// ============================================================
//
// 先说 rsx! 怎么区分「元素」和「组件」，规则在
// dioxus-rsx-0.7.10/src/node.rs:87-94，判据是名字本身：
//
//     单个标识符 + 首字母小写 + 名字里不含下划线   ->   元素
//     其它（首字母大写 / 含下划线 / 是路径）        ->   组件
//
//     text { }       -> 元素
//     qt_text { }    -> 组件（因为有下划线）
//     Text { }       -> 组件
//     crate::Text {} -> 组件
//
// 带 `-` 的标签也算元素：标签是先按 `-` 切开再拼回去解析的
// （dioxus-rsx-0.7.10/src/element.rs:352），所以 qt-text 能用，
// 而且 renderer 拿到的 tag 字符串就是 "qt-text"。
//
// 也就是说 <Text> 这种写法在 Dioxus 眼里一定是组件，
// 它必须是作用域里真实存在的函数 —— 就是下面的 #[component]。
//
// 这两个组件的作用只是「把好看的 API 翻译成一个自定义元素」，
// 它们自己完全不认识 Qt；
// 真正建出 QtQuick.Text 的是下面 QtRenderer 里的 create_element。
//
// props 那侧也没什么魔法：
//     text: String   -> 变成元素上的一个动态属性
//     width/height   -> 也是动态属性，但只进布局账本，不写给 Qt
//     children: Element -> 变成元素里的一个动态坑位（Dynamic）
// 后两条分别对应 set_attribute 和 replace_placeholder_with_nodes。
// ============================================================

// 文本的尺寸不写在这里：由 Qt 量出来决定（见 relayout 的测量回调）。
// 想给文本定宽就写 width —— 那是布局输入，会被 taffy 当成已知宽度传给测量。
#[component]
fn Text(
    text: String,
) -> Element {
    rsx! {
        qt-text {
            text: "{text}",
        }
    }
}


#[component]
fn Rectangle(
    width: f64,
    height: f64,
) -> Element {
    rsx! {
        qt-rectangle {
            width: "{width}",
            height: "{height}",
            color: "green"
        }
    }
}


// ============================================================
// 用户的 Dioxus UI
// ============================================================

// 这十几行是「应用」，其余全是 renderer + 布局。
//
// 三件事值得注意：
//
// 1. 位置尺寸只在这里出现（width / height / gap / flex_direction），
//    而且现在只有容器和矩形写死了尺寸 —— Text 不写，
//    它的大小是 relayout() 里回头问 Qt 量出来的。
//    C++ 那边的内联 QML 已经没有任何 x / y / width / height 了，
//    几何只有一个作者：relayout()。
//
//    负向检查（已跑过）：把 relayout 里测量回调的返回值换成
//    Size { width: 333.0, height: 7.0 }，
//    日志里「布局用到」立刻变成 600.0x7.0 —— 高度跟着测量走。
//    （宽度一直是 600，因为列容器交叉轴默认 stretch 会把它拉满，
//      所以宽度不是这条检查的信号，高度才是。）
//
// 2. 静态属性和动态属性两条路都走到了：
//    <qt-column> / <qt-rectangle> 上那些写死的字面量是
//    TemplateAttribute::Static，在 create_template_node 里就记进布局账本；
//    Text 的 text、条目的 text、count 的插值是动态属性 → SetAttribute，
//    在 set_attribute 里记账。
//
// 3. 根 <qt-column> 的模板里有**三个洞**：
//    Text 组件调用、那个 `if`、`for` 循环各是一个 Dynamic 节点，
//    于是每次渲染都会收到三次 ReplacePlaceholder，而且 Dioxus 是
//    倒着发的 —— 正好是「先写完属性再装载孩子」那条不变式的活教材
//    （diff/node.rs:616-620）。
//    第三个洞一次顶上来 m 个节点（m = 当前条目数，0..=4）。
//
//    按钮那一行不是洞：它是模板里的静态子树，但它那三个 qt-rectangle
//    各带一个动态 onclick，所以每个都会先吃一条 AssignId
//    —— 没有 AssignId 就没有 ElementId，监听器根本送不到节点上。

// counter demo：唯一的真状态是一个整数，唯一的输入是真鼠标。
//
// diff 的几条分支现在全由点击自己跑出来：
//
//     点「加一」→ 列表尾部多出新 key   → InsertAfter（新建）
//                                        （diff/iterator.rs:452-469）
//     点「减一」→ 少了一个 key         → 单独的 Remove
//                                        （diff/iterator.rs:316）
//     点到 3    → 同一个坑位从空长成有 → ReplacePlaceholder
//
// 以前这三条靠 DIFF_SCRIPT（一份写死的 key 表 + 一个每 200ms 推一格的
// QTimer）跑，脚本删了；编在宽度里的那把「key 身份证」也跟着删了 ——
// 它验的是脚本给的顺序和屏幕上的顺序一致，而脚本已经没了。
// 留下来不依赖事件源的检查是每次渲染后 dump_tree 里的 child_order 交叉检查。
fn app() -> Element {
    // 唯一的真状态。读它（`count()`）就是把本 scope 订阅到这个 signal 上；
    // 写它（下面三个 onclick）之后整条链全是 dioxus 自己的机制：
    // write → 发纸条 → 我们的 waker → Qt 事件 → poll Ready → diff → SetAttribute。
    let mut count = use_signal(|| 0i64);

    let shown = count();

    println!("[app] 被渲染，count = {shown}");

    // 跟着 count 长的一排条目：最多加到 4 行然后封顶，
    // 于是往回点（-1 / reset / 条目上那个小方块）会一行行缩回去。
    let rows: Vec<i64> = (0..shown).take(4).collect();

    rsx! {
        // 没有 main.qml，也没有 QML 的 Layout / anchors ——
        // 这里的 width / height / gap / flex_direction 是布局输入，
        // 唯一作者还是 relayout()。
        //
        // 根节点用 100%：它跟着窗口伸缩，下面的东西才谈得上伸缩。
        // 视口那两个数不在这里写死，是 relayout 问 Qt 的
        // （window_width/window_height），拖动窗口 → ResizeFilter →
        // qt_window_resized → 重算，一整条链都不经过 Dioxus。
        qt-column {
            width: "100%",
            height: "100%",
            flex_direction: "column",
            gap: "12",

            Text {
                text: "counter = {shown}",
            }

            // 这一格是**条件**，不是 keyed 列表：
            // count 到 3 之前这里没有孩子，到了就长出一行文本。
            // 同一个坑位从空长成有东西 → ReplacePlaceholder；
            // 退回空 → 那条无共同 key 的 Remove / ReplaceWith 分支。
            if shown >= 3 {
                Text {
                    text: "已经点到 {shown}：下面条目上的小方块每点一下减一",
                }
            }

            // 一行三个按钮。容器还是 qt-column，只是 flex_direction 给 row。
            //
            // 按钮里的 qt-rectangle 是**内层静态元素** + 动态 onclick，
            // 所以 dioxus 必须先给它 AssignId，再发 CreateEventListener
            // （diff/node.rs:856-874 → :533）——
            // 没有 ElementId 的话监听器根本送不到节点上。
            // rsx 里写的是 onclick，送到我们手里已经是 "click"
            // （:533 把 "on" 前缀剥掉了）。
            //
            // 这一屏同时是「属性值四种形态 + 撤掉属性」的演示，
            // 每块按钮各演一种（注释就写在各自身上）：
            //     Text   —— 所有引号里写的，包括下面那些颜色字面量
            //     Int    —— 归零那块：不带引号的 110 / 48 / 12
            //     Float  —— 加一那块：opacity
            //     Bool   —— 那条奇偶条：visible
            //     None   —— 减一那块：color 在偶数次时是整个属性被撤掉
            qt-column {
                width: "100%",
                height: "48",
                flex_direction: "row",
                gap: "12",

                qt-rectangle {
                    width: "110",
                    height: "48",

                    // Option<String>：Some → AttributeValue::Text，
                    // None → AttributeValue::None（nodes.rs:1117-1124）。
                    // 所以偶数次点完之后这块不是「变成透明」也不是「变成黑色」，
                    // 而是**回到 Rectangle 刚建出来时的颜色**（白色）——
                    // C++ 那边第一次写 color 之前抄存的就是它。
                    color: if shown % 2 == 1 {
                        Some("#d9825f".to_string())
                    } else {
                        None
                    },

                    onclick: move |_| {
                        count -= 1;
                    },

                    qt-text {
                        text: "减一（偶数次时 color 被撤掉）",
                    }
                }

                // 中间这块 flex_grow=1：拖动窗口时多出来的宽度都归它，
                // 于是「伸缩」是屏幕上看得见的，不是只在日志里的数字。
                qt-rectangle {
                    width: "110",
                    height: "48",
                    flex_grow: "1",
                    color: "#5f9ed9",

                    // Float → QVariant(double) → QQuickItem::opacity（qreal）。
                    // 一次都没点过的时候它是半透明的。
                    opacity: if shown == 0 {
                        0.45
                    } else {
                        1.0
                    },

                    onclick: move |_| {
                        count += 1;
                    },

                    qt-text {
                        text: "加一（跟着窗口变宽）",
                    }
                }

                // 这块的尺寸写成**数字字面量**（不带引号）：
                // 静态模板属性只能是 &str（nodes.rs:670-677 那句注释），
                // 所以它们走的是动态属性那条路，交过来是 AttributeValue::Int，
                // 由 dimension_of 直接当成长度用 —— 不必再写 "110"。
                // radius 也演一次 Int：它不是布局属性，
                // 是真正落到 Qt 对象上的那个属性。
                qt-rectangle {
                    width: 110,
                    height: 48,
                    radius: 12,
                    color: "#8a8f98",
                    onclick: move |_| {
                        count.set(0);
                    },

                    qt-text {
                        text: "归零",
                    }
                }
            }

            // 一条奇偶指示条：Bool → QVariant(bool) → QQuickItem::visible。
            // 它不参与交互，所以关掉之后不会影响点按钮 ——
            // 用别的东西演示 visible 会顺手把命中测试也关掉（不可见的项
            // 直接被 hit test 跳过，qquickdeliveryagent.cpp:2202），
            // 那就不是一堂属性课而是一堂「为什么点不动」的课了。
            qt-rectangle {
                width: "100%",
                height: "6",
                color: "#3fbf7f",
                visible: shown % 2 == 1,
            }

            // 每个条目是「一个容器 + 两个孩子」，也就是 3 个节点的一棵小子树。
            // 于是删一个条目时 destroy 只调用一次、日志里却有 3 行 object destroyed，
            // 同时证明：孩子是 QObject 的孩子（跟着父亲递归析构）、
            // 账本整棵清干净了、没有双重释放。
            //
            // 两个孩子都写成原生元素 + 动态属性，就是为了逼出两条 AssignId
            // （路径 [0] 和 [1]）：用 Text { } 组件的话动态 text 落在
            // 它自己那个模板的**根**上，根的 id 从 LoadTemplate 就有，永远不需要 AssignId。
            for row in rows {
                // 横排 + 定宽 300：文本是自动宽度，小方块 20x20，
                // 加起来远小于 300 —— 不会触发 flex-shrink 把尺寸改写掉。
                qt-column {
                    key: "{row}",
                    width: "300",
                    height: "28",
                    flex_direction: "row",
                    gap: "8",

                    qt-text {
                        text: "第 {row} 行 · 当前 count = {shown}",
                    }

                    qt-rectangle {
                        width: "20",
                        height: "20",
                        color: "#7fa8a0",
                        onclick: move |_| {
                            count -= 1;
                        },
                    }
                }
            }
        }
    }
}

// ============================================================
// 最小 Qt renderer
// ============================================================

// 树里的一个节点。
//
//     object = Some(item)   已经在 Qt 里实体化了的对象，
//                           这里存的就是那个对象的裸指针
//     object = None         占位节点（模板里的动态位置），
//                           树里有它，Qt 里还没有东西，
//                           等着被 ReplacePlaceholder 顶掉
//
// children 的顺序就是 Dioxus 模板里的顺序，占位节点也占一格 ——
// resolve_slot 拿 path 的下标直接索引它，顺序错一个就全错。
//
// parent 是 children 的反向账。为什么非记不可：
// Dioxus 的 `Remove` 只给一个 ElementId（mutations.rs:112），
// 「它是谁的孩子」这件事 Qt 知道（parentItem）但我们不知道，
// 不记就只能在所有节点里线性找一个 —— 而 remove 是热路径。
//
// 根节点的 object 是 window.contentItem()（由 root_item() 拿到的真实指针）。
// 以前这里是约定的 id 0，C++ 那边再翻译回 contentItem；
// 现在两层是同一个东西，没有那层翻译了。

#[derive(Default)]
struct Node {
    object: Option<QQuickItemId>,

    // 父节点的 node id；根节点和「还没挂上去的模板根」是 None
    parent: Option<u64>,

    // 子节点的 node id，按模板顺序
    children: Vec<u64>,

    // 这个节点在 taffy 树里的位置。
    //
    // None 只有占位节点：它「以后可能长成东西」，
    // 但现在既没有 Qt 对象，也没有参与布局的资格。
    taffy: Option<NodeId>,

    // 布局关心的那几个属性（见 LAYOUT_PROPS）。
    //
    // 记在这里而不是立刻用：属性可能比节点晚到
    // （动态属性是 SetAttribute，静态属性在模板里），
    // 真正的输入时刻是 relayout()。
    //
    // 值是类型化的（PropValue），并且同一名字只有一条：
    // 写就是覆盖，清空（AttributeValue::None）就是把那条摘掉。
    // 两个动作的实现见 set_prop_entry / clear_prop_entry。
    style: Vec<(&'static str, PropValue)>,
}


// renderer 的全部状态就是这几个字段。
//
// 本质工作是在翻译两套编号：
//
//     Dioxus 只会说  ElementId / 模板里的 path
//     Qt   只认     QQuickItem*
//
// 所以中间必须有一棵树把两者接起来，而且要维持住 Dioxus 期望的
// 「第几个孩子」的顺序 —— 哪怕那个位置上的东西在 Qt 里根本不存在。
//
//     nodes            node id -> Node（它的 Qt 指针 + 孩子顺序）
//                      这是「按 Dioxus 的视角」建的树，占位节点也算一格
//     element_to_node  ElementId -> node id
//                      所有按 id 寻址的 mutation 都靠它落地
//     stack            Dioxus 约定的临时栈，见 WriteMutations 的注释
//     host             唯一能碰 Qt 的入口
//
// C++ 那边以前还有第四本账（QtHost::Impl::objects，id -> QObject*），
// 每次调用都要查一次表。现在 Rust 直接握着指针，那张表整个没了。
struct QtRenderer {
    host: UniquePtr<ffi::QtHost>,


    // 节点 id -> 节点
    //
    // 注意这套 node id 和 Qt 那边完全无关：
    // 占位节点没有 Qt 对象，但树里得有它的位置，
    // 而且 mutation 里的路径是按「第几个子节点」走的，
    // 所以要自己维护父子关系。
    nodes: HashMap<u64, Node>,


    // 只增不减的 node id 分配器。
    // 节点被占位符顶掉之后 id 不回收（nodes.remove 了但 id 不会重用），
    // 这样旧 id 万一被引用到，也不会撞上新节点。
    next_node: u64,


    // Dioxus 的 mutation stack。
    //
    // 里面放的是我们的节点 id。
    //
    // 0 是 renderer root：
    //
    // 初始：
    //
    // [ROOT]
    //
    // 规则散在 dioxus-core 的注释和实现里，归纳起来是：
    //     LoadTemplate / CreateTextNode / CreatePlaceholder / PushRoot
    //             → 往栈上压自己新建的节点，且个数是写死的
    //               （diff/node.rs:632-634「This creates one node on the stack」）
    //     AppendChildren / ReplacePlaceholder { m }
    //             → 弹掉栈顶 m 个（mutations.rs:53-63）
    // 所以「弹完 m 个之后的栈顶」就是这 m 个节点的锚点，
    // 下面好几个方法都靠这个性质找父节点
    // （diff/node.rs:622「This operation relies on the fact that
    //   the root node is the top node on the stack」）。
    stack: Vec<u64>,


    // Dioxus:
    //
    // ElementId
    //
    //      ↓
    //
    // 我们树里的节点 id
    //
    // 用 Vec 而不是 HashMap 是因为 ElementId 就是连续的小整数，
    // 偶尔会跳着分配，所以中间允许出现 None。
    element_to_node: Vec<Option<u64>>,

    // 最近一次 load_template 建立的「模板路径 → 我们的 node id」表。
    //
    // 路径的写法（mutations.rs:134-140）：一串 children 的下标，
    // `[]` = 这一次 load 出来的根，`[0]` = 根的第 0 个孩子，
    // `[0,1,2]` = 第 1 个孩子的第 2 个孩子的第 3 个孩子。
    // 注意 dioxus 发过来时已经把「第几个 root」那一位剥掉了（diff/node.rs:871
    // 传的是 `&path[1..]`）。
    //
    // 为什么「只留最近一次」就是对的，靠的是 dioxus 自己写死的一条顺序
    // （diff/node.rs:616-620）：
    //
    //     // !!VERY IMPORTANT!!
    //     // Write out all attributes before we load the children. Loading the
    //     // children will change paths we rely on to assign ids ...
    //
    // 也就是一个实例的所有 AssignId 一定紧跟它自己的 LoadTemplate、
    // 在任何嵌套子模板被 load 之前发完。
    instance_paths: Vec<(Vec<u8>, u64)>,


    // 布局树。
    //
    // 它不是第二棵「UI 树」：每个有 Qt 对象的节点在这里有一个对应节点，
    // 父子关系由 sync_taffy_children 在 Node 的孩子变化时同步过来，
    // 占位节点不进这棵树。
    //
    // 泛型参数是 taffy 的「节点 context」：这里放的是这个节点的 Qt 对象指针。
    // 测量回调只认它自己存的这份 context（不认识我们的 Node，也拿不到 node id），
    // 所以要让 C++ 拿着指针去量 —— 文本、以及「这是不是一行字」这两件事
    // 都由 item 自己回答（qobject_cast<QQuickText*>），Rust 这边不再存副本。
    tree: TaffyTree<Option<QQuickItemId>>,

    // 布局根 = node 0（窗口内容区）。
    // node 0 的 object 是 contentItem 那个真实指针，
    // 但几何从来不往它身上写（apply_geometry 的 DFS 从它的孩子开始），
    // 所以只有孩子会被 apply。
    root_taffy: NodeId,
}


impl QtRenderer {
    // 注意调用时机：必须等 QGuiApplication 建好，
    // 否则下面的 new_qt_host() 里构造 QQuickWindow 会炸。
    fn new() -> Self {
        let mut host =
            ffi::new_qt_host();


        assert!(
            !host.is_null(),
            "new_qt_host returned null"
        );


        host.pin_mut().show();


        let mut nodes =
            HashMap::new();


        // 布局树建根。
        let mut tree = TaffyTree::<
            Option<QQuickItemId>,
        >::new();

        let root_taffy = tree
            .new_leaf(Style::default())
            .expect("taffy: 建布局根失败");


        // renderer root：它自己没有可以「写几何」的对象，
        // 但它是所有顶层节点的挂载目标 —— 就是窗口的 contentItem。
        // 以前这里按 C++ 的约定记成 id 0，现在直接存真实指针。
        let root_item: QQuickItemId =
            host.root_item();

        // 它的样式也走「属性账本」这一条路，
        // 这样 style_for 就是全项目唯一的样式来源。
        nodes.insert(
            0,
            Node {
                object: Some(root_item),
                parent: None,
                children: Vec::new(),
                taffy: Some(root_taffy),
                style: vec![
                    (
                        "flex_direction",
                        PropValue::Text(
                            "column".to_string()
                        ),
                    ),
                ],
            },
        );


        Self {
            host,

            nodes,

            // 0 号 node 已经被 root 占了，所以从 1 开始发
            next_node: 1,

            tree,

            root_taffy,

            // Dioxus renderer root
            //
            // 栈里预置这一个元素，rebuild 结束时
            // AppendChildren 正好把它当锚点用。
            stack: vec![0],

            // ElementId(0) == renderer root
            //
            // 这不是随便挑的编号：VirtualDom::rebuild 的最后一步
            // 固定发 to.append_children(ElementId(0), m)
            // （dioxus-core-0.7.10/src/virtual_dom.rs:593），
            // 所以这里必须先把 0 号位置占住，
            // 否则第一条 AppendChildren 就找不到父节点。
            element_to_node: vec![Some(0)],
            instance_paths: Vec::new(),
        }
    }


    fn new_node(
        &mut self,
        object: Option<QQuickItemId>,
    ) -> u64 {
        let id = self.next_node;

        self.next_node += 1;


        // 有 Qt 对象的节点才有布局节点：
        // 一个连壳都没有的占位，摆在哪都没有意义。
        // 它后来被真节点顶掉时，顶上来的是已经建好的节点。
        let taffy = match object {
            Some(_) => Some(
                self.tree
                    .new_leaf(Style::default())
                    .expect("taffy: new_leaf 失败"),
            ),

            None => None,
        };


        self.nodes.insert(
            id,
            Node {
                object,
                parent: None,
                children: Vec::new(),
                taffy,
                style: Vec::new(),
            },
        );


        id
    }


    fn node(
        &self,
        id: u64,
    ) -> &Node {
        self.nodes
            .get(&id)
            .unwrap_or_else(|| {
                panic!("unknown node id {id}")
            })
    }


    // node id -> 它的 Qt 对象指针（占位节点是 None）。
    fn object_of_node(
        &self,
        id: u64,
    ) -> Option<QQuickItemId> {
        self.nodes
            .get(&id)
            .and_then(|node| node.object)
    }


    // 这个 node 的 node id 挂到 Dioxus 的 ElementId 上。
    //
    // 会被绑定的节点有两类：
    //     LoadTemplate / CreateTextNode / CreatePlaceholder 的根，
    //     以及模板深处那些带动态属性的静态元素 —— 后者由 AssignId 送进来
    //     （见 assign_node_id：靠 instance_paths 按路径找回节点）。
    fn bind(
        &mut self,
        id: ElementId,
        node: u64,
    ) {
        if self.element_to_node.len() <= id.0 {
            self.element_to_node.resize(
                id.0 + 1,
                None,
            );
        }


        self.element_to_node[id.0] =
            Some(node);
    }


    // ElementId -> node id。
    //
    // Dioxus 的 ElementId 不是连续分配的（每次 next_element() 拿一个），
    // 所以数组里会有洞，这里用 get().copied().flatten() 而不是直接索引。
    fn lookup(
        &self,
        id: ElementId,
    ) -> Option<u64> {
        self.element_to_node
            .get(id.0)
            .copied()
            .flatten()
            // 节点被替换掉之后，映射可能指向一个
            // 已经不在树里的节点，这里顺手过滤掉。
            .filter(|node| {
                self.nodes.contains_key(node)
            })
    }


    // 这个 ElementId 对应的 Qt 对象（占位节点没有）。
    //
    // 返回 None 的两种情况：这个 id 压根没绑过，
    // 或者绑的是个占位节点。调用方目前都是「那就跳过」，
    // 也就是说这类属性/文本更新会被丢掉。
    fn object_of(
        &self,
        id: ElementId,
    ) -> Option<QQuickItemId> {
        self.object_of_node(self.lookup(id)?)
    }


    // 按 Dioxus 的栈契约取走栈顶 m 个节点。
    //
    // 注意只弹 m 个：锚点（父节点 / 模板根）要留在栈上别动，
    // AppendChildren 和 ReplacePlaceholder 弹完还要用它。
    //
    // 那个 assert 值得留着：一旦这里的 m 和 Dioxus 实际压栈的数量
    // 不一致，后面每个节点都会挂错父节点，静默错下去非常难查，
    // 不如当场炸。
    fn pop_m(
        &mut self,
        m: usize,
    ) -> Vec<u64> {
        assert!(
            m <= self.stack.len(),
            "mutation stack underflow"
        );


        self.stack.split_off(
            self.stack.len() - m
        )
    }


    // --------------------------------------------------------
    // 建模板
    //
    // Dioxus 的模板是一棵静态树，节点只有三种
    // （定义在 dioxus-core-0.7.10/src/nodes.rs:420-475）：
    //
    //     Text { }        静态文本
    //     Element { }     元素
    //     Dynamic { }     动态位置（组件、if/for、children）
    //
    // 静态部分在这里一次性建好；
    // 动态位置先立一个占位节点，之后被顶掉。
    //
    // Template 本体是三块数据（nodes.rs:274-300）：
    //
    //     roots        静态树本体，rsx! 允许多个根
    //     node_paths   每个动态节点在树里的位置，深度优先排列
    //     attr_paths   每个动态属性的位置
    //
    // path 的格式是 [第几个 root, 进第几个孩子, 再进第几个孩子...]；
    // 发给 renderer 时会去掉第一位（相对栈顶那个 root），见 diff/node.rs:774。
    //
    // 于是这条递归有个硬性要求：children 的建序必须和模板完全一致，
    // Dynamic 也要老老实实建出一个占位节点，
    // 否则后面 resolve_slot 按下标找位置就会找错。
    // --------------------------------------------------------

    fn create_template_node(
        &mut self,
        node: &TemplateNode,
    ) -> u64 {
        self.create_template_node_at(node, &mut Vec::new())
    }

    // 按模板结构递归建节点，同时把「每个节点相对本次模板根的路径」记进
    // instance_paths —— assign_node_id 要靠它找回节点。
    //
    // path 里每一位都是**它在模板 children 数组里的下标**（mutations.rs:134-140），
    // 所以递归时必须老老实实按模板给的顺序建孩子、
    // 动态位置也要建出占位节点 —— 少建一个，后面所有下标就全错位。
    fn create_template_node_at(
        &mut self,
        node: &TemplateNode,
        path: &mut Vec<u8>,
    ) -> u64 {
        match node {
            TemplateNode::Element {
                tag,
                attrs,
                children,
                ..
            } => {
                let object =
                    self.create_element(tag);


                // 节点先建出来，属性才有地方记。
                let element_node =
                    self.new_node(Some(object));

                self.instance_paths
                    .push((path.clone(), element_node));


                // 模板里写死的属性（text: "字面量"）此刻就能应用；
                // 动态属性（text: "{expr}"）Dioxus 稍后会通过
                // set_attribute 送过来。
                //
                // 布局属性（width / height / gap / flex-direction / flex_grow）
                // 只记账，不写给 Qt —— 几何只有一个作者。
                //
                // 静态属性在模板里永远是 &str ——
                // `TemplateAttribute::Static` 自己就这么写着
                // （dioxus-core-0.7.10/src/nodes.rs:670-677：
                // "Currently this only accepts &str, so values, even if they're
                // known at compile time, are not known"），
                // 所以这一路只有 PropValue::Text 一种形态；
                // 类型化的值要从动态属性那条路才走得通。
                for attr in attrs.iter() {
                    if let TemplateAttribute::Static {
                        name,
                        value,
                        ..
                    } = attr
                    {
                        let value = PropValue::Text(
                            (*value).to_string(),
                        );

                        self.record_style(
                            element_node,
                            name,
                            value.clone(),
                        );

                        let variant = variant_of(&value);

                        if !is_layout_prop(name) {
                            // SAFETY: object 是 create_element 刚拿回来的
                            // QQuickItem 指针，还没被任何人释放
                            // （它挂在 Qt 的 parent tree 上，或等着被挂）。
                            unsafe {
                                self.host
                                    .pin_mut()
                                    .set_property(
                                        object,
                                        name,
                                        &variant,
                                    );
                            }
                        }
                    }
                }


                // 静态子节点递归建；
                // 动态位置建出一个占位节点。
                for (idx, child) in children.iter().enumerate() {
                    path.push(idx as u8);

                    let child_node =
                        self.create_template_node_at(child, path);

                    path.pop();

                    self.attach(
                        element_node,
                        child_node,
                    );
                }


                println!(
                    "[renderer] <{tag}> -> node \
                     {element_node} (QtObject {object:p})"
                );


                element_node
            }


            TemplateNode::Text {
                text,
            } => {
                // 静态文本：内容直接写死在模板里，
                // 建出来之后 Dioxus 永远不会再对它发 mutation，
                // 所以这一类节点根本不需要 ElementId。
                //
                // 会变的那种文本是 dynamic text，
                // 走的是 create_text_node / set_node_text。
                let object =
                    self.host
                        .pin_mut()
                        .create_text(text);

                let text_node =
                    self.new_node(Some(object));

                self.instance_paths
                    .push((path.clone(), text_node));

                text_node
            }


            // 动态位置：先占个坑，
            // 之后 ReplacePlaceholder 会拿真节点顶掉它。
            //
            // 这里被忽略的那个 id（TemplateNode::Dynamic { id }）
            // 是 dynamic_nodes 和 node_paths 里的下标。
            // 本 renderer 不需要它 ——
            // 我们是按「它在树的哪个位置」来找这个坑的，
            // 下标信息已经在父子顺序里了。
            //
            // 但路径要记：AssignId 也可能是给一个**占位节点**发 id 的
            // （mutations.rs:145 原文就是 "this element/placeholder"）。
            TemplateNode::Dynamic {
                ..
            } => {
                let placeholder =
                    self.new_node(None);

                self.instance_paths
                    .push((path.clone(), placeholder));

                placeholder
            }
        }
    }


    // Dioxus 的元素名 -> Qt 对象。
    //
    // 这就是「自定义内置控件」的落点，
    // 也是整个 renderer 唯一认识「有哪些控件」的地方。
    //
    // 加一个新控件要动三处：
    //
    //     1. 这里的 match 加一个分支（tag 就是 rsx! 里写的标签名）
    //     2. cpp/qt_host.h + .cpp 加一个 create_xxx()，
    //        里面写好该控件对应的内联 QML
    //     3. 本文件顶部 mod ffi 里声明这个函数
    //
    // 属性不用登记：set_property 走 QObject::setProperty，
    // 名字对上 Qt 的属性名就行。
    //
    // 为什么 qt-text 传空串进来：
    // text 在模板里是动态属性（text: "{text}"），
    // 建对象这一刻值还没送过来，得等随后的 SetAttribute；
    // 静态字面量那条路是在 create_template_node 里直接写上的。
    fn create_element(
        &mut self,
        tag: &str,
    ) -> QQuickItemId {
        let object = match tag {
            "qt-text" => self
                .host
                .pin_mut()
                .create_text(""),

            "qt-rectangle" => self
                .host
                .pin_mut()
                .create_rectangle(),

            "qt-column" => self
                .host
                .pin_mut()
                .create_column(),

            // 大小写规则见文件里 Text 组件上方的注释：
            // 走到这里的一定是被当成元素的标签，
            // 所以命中这条说明 rsx! 里写了个 renderer 不认识的标签名。
            other => panic!(
                "QtRenderer 不认识元素 <{other}>"
            ),
        };


        // C++ 那边实例化失败就是返回 nullptr。
        // 不拦的话这个「有节点、没对象」的东西会一路走下去：
        // 挂载、写几何全都拿空指针，症状会散成一片很难读。
        // 真正的错误信息（QML 报错）已经由 qWarning 打出去了。
        assert!(
            !object.is_null(),
            "Qt 侧实例化 <{tag}> 失败，见上面的 qWarning"
        );


        object
    }


    // 把布局属性记到节点上（不写给 Qt）。
    //
    // 同一名字只留一条，写第二次就是覆盖 —— 本体在 set_prop_entry，
    // 这里只管找节点。
    fn record_style(
        &mut self,
        node: u64,
        name: &'static str,
        value: PropValue,
    ) {
        if !is_layout_prop(name) {
            return;
        }

        let entry =
            self.nodes
                .get_mut(&node)
                .unwrap_or_else(|| {
                    panic!("unknown node id {node}")
                });

        set_prop_entry(
            &mut entry.style,
            name,
            value,
        );
    }


    /// 属性被撤（AttributeValue::None）：把账本里那一条摘掉，
    /// 于是这个属性在 style_for 眼里回到「从来没写过」。
    ///
    /// 返回是否真摘掉了什么 —— 没写过却被 clear，日志要说清楚。
    fn forget_style(
        &mut self,
        node: u64,
        name: &'static str,
    ) -> bool {
        let Some(entry) = self.nodes.get_mut(&node) else {
            return false;
        };

        clear_prop_entry(&mut entry.style, name)
    }


    // 父节点的孩子列表变了，就把布局树的 children 整体重设一遍。
    //
    // 刻意不做增量：这里的调用点是「树结构真的变了」的时刻
    // （静态孩子建完、占位被顶掉），量很小，
    // 整体覆盖比维护 diff 的边界条件可靠得多。
    fn sync_taffy_children(
        &mut self,
        parent: u64,
    ) {
        let Some(parent_taffy) =
            self.node(parent).taffy
        else {
            return;
        };


        let children: Vec<NodeId> = self
            .node(parent)
            .children
            .iter()
            .filter_map(|child| {
                self.node(*child).taffy
            })
            .collect();


        self.tree
            .set_children(parent_taffy, &children)
            .expect("taffy: set_children 失败");
    }


    // 布局的唯一计算入口：属性 → taffy → Qt。
    //
    // 调用时机是「批量边界」：现在是 rebuild 之后一次，
    // 以后接上更新循环就是每次 render_immediate 之后一次。
    // 绝对不要写成每条 mutation 算一次。
    fn relayout(
        &mut self,
    ) {
        // 1. 把每个节点记下的属性变成 taffy 的输入。
        //    先 clone 出 props 是为了让 &self 的借用当场结束。
        let ids: Vec<u64> =
            self.nodes.keys().copied().collect();

        for id in ids {
            let Some(taffy) = self.node(id).taffy
            else {
                continue;
            };

            let props = self.node(id).style.clone();
            let style = style_for(&props);

            self.tree
                .set_style(taffy, style)
                .expect("taffy: set_style 失败");

            // 把节点的 Qt 对象指针交给 taffy 的 context。
            //
            // 测量是 taffy 反过来调我们的：回调手里只有自己存的 context 和
            // style，既不认识我们的 Node，也拿不到 node id —— 所以「去哪儿读
            // 这段字」必须在算之前就放好。放指针而不是放字符串是有意的：
            // 文本的唯一存储就成了那个 item 本身，Rust 不再有一份会陈旧的副本。
            let object = self.node(id).object;

            self.tree
                .set_node_context(
                    taffy,
                    Some(object),
                )
                .expect("taffy: set_node_context 失败");
        }


        // 2. 视口问 Qt 要，不在 Rust 里再写一个 640x220。
        //    taffy 用 f32，bridge 用 f64，边界处换算一次。
        let viewport_px = Size {
            width: self.host.window_width() as f32,
            height: self.host.window_height() as f32,
        };

        let viewport = Size {
            width: AvailableSpace::Definite(
                viewport_px.width,
            ),
            height: AvailableSpace::Definite(
                viewport_px.height,
            ),
        };

        // 布局根（0 号节点 = 窗口的 contentItem）的尺寸就是视口本身，
        // 它不吃 rsx 里的属性 —— 这一步是「百分比的基准」：
        // 下面每一层的 100% 都是相对包含块算的，根如果是 auto，
        // 它就按内容收缩，整棵树跟着内容走而不是跟着窗口走。
        // 第一版正是这样：根算出 354x75（三个按钮加间距的宽度），
        // 于是拖动窗口什么也不会变。
        //
        // 每次 relayout 都重读一遍，所以 resize 那条链路改的就是这两个数。
        let mut root_style =
            style_for(&self.node(0).style.clone());

        root_style.size = Size {
            width: Dimension::length(
                viewport_px.width,
            ),
            height: Dimension::length(
                viewport_px.height,
            ),
        };

        self.tree
            .set_style(
                self.root_taffy,
                root_style,
            )
            .expect("taffy: 布局根样式写入失败");


        // 3. 算布局。
        //
        //    用的是 compute_layout_with_measure 而不是 compute_layout：
        //    叶子节点（taffy 眼里没有孩子的节点 = 文本和矩形）的尺寸
        //    taffy 自己算不出来，得回头问 Qt。
        //    compute_layout 其实就是「测量函数永远返 0」的特例。
        //
        //    借用要拆成字段级：tree 需要 &mut（它在写布局结果），
        //    host 只需要 &（measure_item 是 const 的），
        //    两者都在 self 上，整块借会互相撞上。
        let QtRenderer {
            tree,
            host,
            root_taffy,
            ..
        } = self;

        let host = &**host;

        tree
            .compute_layout_with_measure(
                *root_taffy,
                viewport,
                |inputs, _node, context, style| {
                    // 叶子的通用那半：style 的 size / min / max、
                    // padding、可用空间怎么和测量结果夹在一起。
                    compute_leaf_layout(
                        inputs,
                        style,
                        // calc 表达式本项目没用到，原样返回。
                        |_calc_ptr, value| value,
                        |known_dimensions, available_space| {
                            // 两层都要有：taffy 那层没存 context，
                            // 或者这个节点压根没有 Qt 对象 —— 都量不出东西，
                            // 尺寸就退回 style。
                            let Some(Some(object)) = context else {
                                return Size::ZERO;
                            };

                            // 只有父节点给了确定宽度才折行。
                            // MaxContent / MinContent 是「按内容要多宽有多宽」，
                            // 那时不限宽。
                            // （MinContent 严格来说该量「最长的一个词」，
                            //   这个 demo 里没有会问 min-content 的容器，
                            //   所以先按不限宽处理。）
                            let max_width = match available_space.width {
                                AvailableSpace::Definite(width) => {
                                    width as f64
                                }
                                _ => -1.0,
                            };

                            // 不是文本项 / 文本为空，C++ 那边返回 0x0，
                            // 于是 style 里的 width / height 就是答案。
                            //
                            // SAFETY: 这个指针是从 Node.object 抄进 context 的，
                            //         对象由 Qt 的 parent tree 持有；
                            //         算布局这段时间里没有任何东西会删节点
                            //         （删节点发生在 mutation 应用阶段，早于这里）。
                            let measured = unsafe {
                                host.measure_item(
                                    *object,
                                    max_width,
                                )
                            };

                            // 已经知道的轴就用知道的，别拿测量值去盖。
                            Size {
                                width: known_dimensions
                                    .width
                                    .unwrap_or(measured.width as f32),
                                height: known_dimensions
                                    .height
                                    .unwrap_or(measured.height as f32),
                            }
                        },
                    )
                },
            )
            .expect("taffy: compute_layout_with_measure 失败");


        // 4. 结果推给 Qt。
        self.apply_geometry(0);
    }


    // 深度优先把布局结果写给每个节点。
    //
    // taffy 的 location 是相对父节点的，
    // QQuickItem 的 x / y 也是相对 parentItem 的 —— 同一个口径，不用换算。
    fn apply_geometry(
        &mut self,
        node: u64,
    ) {
        let children = self.node(node).children.clone();

        for child in children {
            // 占位节点不参与布局，也没有 Qt 对象可写。
            let Some(taffy) = self.node(child).taffy
            else {
                continue;
            };

            let layout = self
                .tree
                .layout(taffy)
                .expect("taffy: 缺布局结果");

            let x = layout.location.x as f64;
            let y = layout.location.y as f64;
            let w = layout.size.width as f64;
            let h = layout.size.height as f64;

            // 每个有 Qt 对象的节点都写；
            // 占位节点上面已经被 taffy 那层过滤掉了（它没有布局节点）。
            //
            // 以前这里还要把「id 0 = 窗口根」那个约定值排除掉，
            // 现在不用：这个 DFS 从 node 0 的孩子们开始，
            // node 0 自己（contentItem）从来不是被写的那个。
            let object = self
                .node(child)
                .object;

            if let Some(object) = object {
                // SAFETY: 这个指针来自 create_* 或 root_item()，
                // 对象由 Qt 的 parent tree 持有，活着。
                unsafe {
                    self.host
                        .pin_mut()
                        .set_geometry(object, x, y, w, h);
                }
            }

            self.apply_geometry(child);
        }
    }


    // 对照检查：我们量的文本尺寸 vs Qt 自己报的 implicit size。
    //
    // 测量已经进布局了，这个函数退化成「对测量口径做体检」：
    // 两边不一致就先别信布局 —— 差 1px 会在 flex 分配里累积成可见的错位。
    //
    // 期待看到什么：
    //   - 「不限宽」那行 Δ 应该是 0.0/0.0（同一个字体、同一个算法）。
    //   - 「限宽100」的宽度应 ≤ 100、高度是多行之和（这是折行后的口径，
    //     Qt 的 implicitWidth 不看折行，所以这两行不和 implicit 比）。
    //
    // 如果 Δ 不是 0，最可能的原因是字体两边不同（现在两边都该是
    // QGuiApplication::font()），或者 useDesignMetrics 没开。
    fn verify_text_measurement(
        &mut self,
    ) {
        let mut ids: Vec<u64> =
            self.nodes.keys().copied().collect();

        ids.sort_unstable();


        println!("\n===== 文本测量对照 =====");

        for id in ids {
            let Some(object) =
                self.node(id).object
            else {
                continue;
            };

            // 只有文本项值得比 —— 判据在 C++（qobject_cast<QQuickText*> 且
            // 文本非空），这里不再自己留一份「这是不是文本」的账。
            //
            // SAFETY: object 来自 Node.object，也就是 create_* / root_item()
            //         交回来的指针；对象由 Qt 的 parent tree 持有，此刻还活着。
            //         这两个调用都只读，不改所有权。
            let is_text = unsafe {
                self.host.is_text_item(object)
            };

            if !is_text {
                continue;
            }

            let free = unsafe {
                self.host.measure_item(object, -1.0)
            };
            let wrapped = unsafe {
                self.host.measure_item(object, 100.0)
            };

            // SAFETY: 同一个对象指针，来自 create_*；
            // 读 implicit size 不改对象所有权，也不管它挂没挂。
            let implicit_w = unsafe {
                self.host.implicit_width(object)
            };
            let implicit_h = unsafe {
                self.host.implicit_height(object)
            };

            println!(
                "[measure] node {id} obj {object:p}"
            );
            println!(
                "   不限宽  我们 {:.1}x{:.1}   \
                 Qt implicit {:.1}x{:.1}   Δ{:.1}/{:.1}",
                free.width, free.height,
                implicit_w, implicit_h,
                free.width - implicit_w,
                free.height - implicit_h,
            );
            println!(
                "   限宽100 我们 {:.1}x{:.1}",
                wrapped.width, wrapped.height
            );

            // 布局最终给这个节点的尺寸 —— 也就是 set_geometry 写进 Qt 的那个数。
            //
            // 它的高度应该等于测量高度（文本只有一行）；
            // 宽度不等于量出来的 254.9，因为列容器的 align-items 默认是
            // stretch：交叉轴（这里就是水平）上会被拉满成容器的 600。
            // 所以宽度在这条日志上分不出「量了的」和「拉宽的」，
            // 能分辨的是高度 —— 已经验过：把测量回调硬返回 height 7.0，
            // 这里立刻变 600.0x7.0。
            if let Some(taffy) = self.node(id).taffy {
                if let Ok(layout) = self.tree.layout(taffy) {
                    println!(
                        "   布局用到 {:.1}x{:.1} @ ({:.1}, {:.1})",
                        layout.size.width,
                        layout.size.height,
                        layout.location.x,
                        layout.location.y,
                    );
                }
            }
        }

        println!("====================\n");
    }


    // 把 child 挂到 parent 下面。
    //
    // 树这边无条件记；Qt 那边只有一边已经有对象了才动手。
    fn attach(
        &mut self,
        parent: u64,
        child: u64,
    ) {
        self.nodes
            .get_mut(&parent)
            .unwrap_or_else(|| {
                panic!("unknown parent node {parent}")
            })
            .children
            .push(child);

        // 反向账跟上。Remove 只能拿到 ElementId，
        // 「这个孩子从哪个 vec 里摘」全靠这一行。
        self.nodes
            .get_mut(&child)
            .expect("unknown child node")
            .parent = Some(parent);


        // Qt 侧：把 child 的对象挂到 parent 的对象下。
        //
        // 有一方还没有 Qt 对象（占位节点）时先只记树。
        //
        // 这里只记树不挂载是安全的：
        // 挂不了是因为 child 那一侧根本还没有对象，
        // 而它后来变成真节点时，
        // ReplacePlaceholder / AppendChildren 会补上这次 mount。
        if let (
            Some(parent_object),
            Some(child_object),
        ) = (
            self.object_of_node(parent),
            self.object_of_node(child),
        ) {
            // SAFETY: 两个指针都来自 create_* / root_item()，
            // 且这一刻都还没被释放（挂载本身就是让它们被 parent 接管的动作）。
            unsafe {
                self.host
                    .pin_mut()
                    .mount(
                        parent_object,
                        child_object,
                    );
            }
        }


        // 布局树跟着同步。
        self.sync_taffy_children(parent);
    }


    // child 在 parent.children 里的下标。
    fn index_in_parent(
        &self,
        parent: u64,
        child: u64,
    ) -> Option<usize> {
        self.node(parent)
            .children
            .iter()
            .position(|c| *c == child)
    }


    // 把这些节点从各自现在挂着的那个父亲身上摘下来，
    // 返回被改动过的父亲列表（调用方要给它们重设布局树）。
    //
    // 为什么需要：列表「移动」不走 Remove —— dioxus 直接
    // push_all_root_nodes + insert_nodes_*（diff/iterator.rs:343-375），
    // 所以到这里的节点很可能还挂在某个父节点的孩子列表里（常常就是同一个）。
    // 不先摘就会在两个父亲的名下同时出现，children 的下标也全乱。
    fn detach_from_old_parents(
        &mut self,
        nodes: &[u64],
    ) -> Vec<u64> {
        let mut affected = Vec::new();

        for node in nodes {
            let Some(old_parent) = self.node(*node).parent
            else {
                continue;
            };

            if let Some(parent_node) =
                self.nodes.get_mut(&old_parent)
            {
                parent_node
                    .children
                    .retain(|child| *child != *node);
            }

            self.nodes
                .get_mut(node)
                .expect("unknown node")
                .parent = None;

            if !affected.contains(&old_parent) {
                affected.push(old_parent);
            }
        }


        affected
    }


    // 把 nodes 插进 parent.children 的 index 处，Qt 那边跟着对齐。
    //
    // Qt 侧为什么不能只 mount：mount 里的 setParentItem 只能往
    // 父亲的 childItems 末尾追加，插到中间还得靠 stack_before。
    // 逐个 stackBefore「同一个后邻」得到的正是 [n1, n2, 后邻] ——
    // 和 replace_placeholder_with_nodes 是同一套路，理由也相同。
    fn insert_nodes_at(
        &mut self,
        parent: u64,
        index: usize,
        nodes: &[u64],
    ) {
        if nodes.is_empty() {
            return;
        }


        // 1. 自己的账先动：children 插进 index，parent 字段改过去。
        {
            let children = &mut self
                .nodes
                .get_mut(&parent)
                .expect("unknown parent node")
                .children;

            for (offset, node) in nodes.iter().enumerate() {
                children.insert(index + offset, *node);
            }
        }

        for node in nodes {
            self.nodes
                .get_mut(node)
                .expect("unknown node")
                .parent = Some(parent);
        }


        // 2. 找一个「后邻」当叠放顺序的锚点：
        //    插在新一格右边、而且已经有 Qt 对象的那个孩子。
        //    占位节点当锚点没意义（Qt 那边没有它的位置）。
        let mut next_sibling = None;

        for child in self
            .node(parent)
            .children
            .iter()
            .skip(index + nodes.len())
        {
            if let Some(object) = self.object_of_node(*child) {
                next_sibling = Some(object);
                break;
            }
        }


        // 3. Qt：把每个对象放到它在 children 里该在的那一格。
        let parent_object = self.object_of_node(parent);

        if let Some(parent_object) = parent_object {
            for node in nodes {
                // 占位节点没有对象，挂不了 —— 记账已经做完了。
                let Some(child_object) =
                    self.object_of_node(*node)
                else {
                    continue;
                };

                // SAFETY: 三个指针都来自 create_* / root_item()，
                //            这一刻对象都还活着（destroy 只会发生在
                //            destroy_subtree 里，而那里同时会把账删干净）。
                unsafe {
                    self.host.pin_mut().place(
                        parent_object,
                        child_object,
                        next_sibling.unwrap_or(std::ptr::null_mut()),
                    );
                }
            }
        }


        // 4. 布局树跟上（父亲的孩子列表刚刚变过）。
        self.sync_taffy_children(parent);
    }


    // insert_nodes_after / insert_nodes_before 共用的那半边：
    // 找锚点、算下标、把「移动」过来的节点先从旧父亲身上摘掉。
    fn insert_relative(
        &mut self,
        anchor_id: ElementId,
        nodes: &[u64],
        after: bool,
    ) {
        if nodes.is_empty() {
            return;
        }


        let anchor =
            self.lookup(anchor_id).unwrap_or_else(|| {
                panic!(
                    "Insert 的锚点 ElementId({}) 不在树里",
                    anchor_id.0
                )
            });

        let parent =
            self.node(anchor).parent.unwrap_or_else(|| {
                panic!(
                    "Insert 的锚点 node {anchor} 没有父节点"
                )
            });


        // 先摘、再算下标：移动过来的节点很可能原本就挂在同一个父亲下面，
        // 用「摘之前」的下标插就会错一格。
        //
        // 第 5 步（[1,2,0] → [2,0,1,3]）就踩在这一格上：锚点 node 4 摘之前
        // 在下标 3、摘之后在下标 2，要插的是 3 这一格而不是 4。
        // 把 index_in_parent 提到 detach 前面去试过一次，Vec::insert 当场
        // panic「insertion index (is 4) should be <= len (is 3)」，
        // 日志见 target/negative_index2.log。
        let affected = self.detach_from_old_parents(nodes);

        let index =
            self.index_in_parent(parent, anchor).unwrap_or_else(|| {
                panic!(
                    "Insert：锚点 node {anchor} 不在 node {parent} \
                     的孩子列表里"
                )
            });

        let index = if after { index + 1 } else { index };

        self.insert_nodes_at(parent, index, nodes);

        // 被摘掉孩子的旧父亲们也要重设布局树
        //（新父亲在 insert_nodes_at 里已经同步过了）。
        for old_parent in
            affected.iter().filter(|p| **p != parent)
        {
            self.sync_taffy_children(*old_parent);
        }
    }


    // 把这棵子树整个删掉，返回子树大小（只为日志）。
    //
    // 两条从源码里抠出来的契约决定了下面的写法：
    //
    // 1. 子树里嵌套的动态节点**不会**再各发一条 Remove
    //    （diff/node.rs:285-288「We don't generate mutations for these,
    //    as they will be removed by the parent」）。
    //    所以 Qt 那边只要删掉根就够（孩子是 QObject 的孩子，
    //    ~QObject 递归删；~QQuickItem 自己只负责把 childItems 摘干净，
    //    见 qquickitem.cpp:2409），
    //    但我们自己的账必须整棵子树一起清干净。
    //
    // 2. 发完 Remove / ReplaceWith，Dioxus 立刻 `dom.reclaim(id)`
    //    （diff/node.rs:330）：这个 ElementId 会被重新发出去给别的节点用，
    //    所以映射必须清掉，否则 lookup 会把新 mutation 派给一个死节点。
    fn destroy_subtree(
        &mut self,
        node: u64,
    ) -> usize {
        // 1. 整棵子树拉平（占位节点也在里面：它没对象，
        //    但它的账和布局位置都得跟着清）。
        let mut subtree = Vec::new();
        let mut walk = vec![node];

        while let Some(current) = walk.pop() {
            subtree.push(current);

            walk.extend(
                self.node(current).children.iter().copied()
            );
        }


        // 2. 从父亲的孩子列表里摘掉这一格。
        let parent = self.node(node).parent;

        if let Some(parent) = parent {
            if let Some(parent_node) =
                self.nodes.get_mut(&parent)
            {
                parent_node
                    .children
                    .retain(|child| *child != node);
            }

            // 布局树的 children 以 Node.children 为准，整体重设一遍。
            self.sync_taffy_children(parent);
        }


        // 3. Qt：只有子树的根需要说一声，孩子跟着它一起没。
        if let Some(object) = self.node(node).object {
            // SAFETY: 这是 create_* 给出来的指针，这一刻还没被删；
            //            调用完 C++ 就把它从两棵树上摘掉了，
            //            而下面我们连自己的账都不留 —— 没有别人会再碰它。
            unsafe {
                self.host
                    .pin_mut()
                    .destroy(object);
            }
        }


        // 4. 每个节点：布局节点删掉、ElementId 清掉、Node 删掉。
        //    taffy 的 remove 只管自己（taffy_tree.rs:672-686 会把孩子
        //    的 parent 置空但不删孩子），所以子树里每个都要单独删，
        //    先父后子也没关系：孩子那一刻仍是合法 id。
        for current in subtree.iter() {
            if let Some(taffy) = self.node(*current).taffy {
                self.tree
                    .remove(taffy)
                    .expect("taffy: remove 失败");
            }

            self.forget_element_ids(*current);
            self.nodes.remove(current);
        }

        // 监听器不留账：Qt 的 filter 是这个 item 的 QObject 孩子，
        // ~QObject 递归删；C++ 的登记簿在 destroy 里也已经剔过了。

        subtree.len()
    }


    // 把当前的树打出来 —— 纯诊断，不参与任何逻辑。
    //
    // diff 的每一步之后都调一次：children 的顺序、每格的下标、
    // 每个节点最终拿到的尺寸，全在这一张表上。
    // 顺序错一格、多一格，或者移动后两个父亲名下都有同一个节点，
    // 这里都直接看得出来。
    fn dump_tree(
        &self,
    ) {
        println!("----- 当前的树 -----");

        self.dump_node(0, 0);

        println!(
            "----- 共 {} 个节点 -----",
            self.nodes.len()
        );
    }

    fn dump_node(
        &self,
        node: u64,
        depth: usize,
    ) {
        let pad = "  ".repeat(depth);

        let object = self
            .node(node)
            .object
            .map_or_else(
                || "无对象".to_string(),
                |item| format!("{item:p}"),
            );

        let layout = self
            .node(node)
            .taffy
            .and_then(|t| self.tree.layout(t).ok())
            .map(|l| format!(
                "{:.0}x{:.0} @({:.0},{:.0})",
                l.size.width,
                l.size.height,
                l.location.x,
                l.location.y,
            ))
            .unwrap_or_else(|| "不参与布局".to_string());

        println!(
            "{pad}node {node}  {object}  {layout}"
        );

        // 顺序对账：Qt 的 childItems 顺序必须跟我们记的一样。
        // 我们这边只数「有 Qt 对象的孩子」—— 占位节点在 Qt 里本来就没东西。
        if let Some(parent_object) = self.node(node).object {
            let expected: Vec<u64> = self
                .node(node)
                .children
                .iter()
                .filter_map(|child| {
                    self.object_of_node(*child)
                })
                .map(|item| item as u64)
                .collect();

            if !expected.is_empty() {
                // SAFETY: parent_object 来自 create_* / root_item()，
                //            这一刻还活着（正在被 dump 的这个节点就是证据）。
                let actual = unsafe {
                    self.host.child_order(parent_object)
                };

                if actual == expected {
                    println!(
                        "{pad}  叠放顺序 ✓ {} 个孩子",
                        expected.len()
                    );
                } else {
                    println!(
                        "{pad}  叠放顺序 ✗ 我们={expected:?} \
                         Qt={actual:?}"
                    );
                }
            }
        }

        for child in self.node(node).children.iter() {
            self.dump_node(*child, depth + 1);
        }
    }


    // 路径 -> (父节点, 在父节点里的下标)
    //
    // 路径是「第几个子节点」的序列。
    //
    // 语义见 dioxus-core-0.7.10/src/mutations.rs:59-63：
    //     []        栈顶节点自己
    //     [0]       第一个孩子
    //     [0,1,2]   第 1 个孩子的第 2 个孩子的第 3 个孩子
    //
    // 下标数的是「模板里的位置」，占位节点也算一格，
    // 这正是 nodes 里要维护那些没有 Qt 对象的节点的原因。
    //
    // 一个不太明显的约束：一次 ReplacePlaceholder 会把 1 个位置变成
    // m 个，父节点后面的下标全右移了。
    // Dioxus 侧对同一模板的多个坑位是「倒着处理」的
    // （diff/node.rs:749-761，issue 2809 就是这么修的），
    // 所以从前往后读到的 path 不会被污染。
    // 我们如果哪天自己造 mutation，这条不变式得自己守住。
    fn resolve_slot(
        &self,
        from: u64,
        path: &'static [u8],
    ) -> (u64, usize) {
        let (last, head) =
            path.split_last()
                .expect("空路径");


        let parent = head
            .iter()
            .fold(from, |node, step| {
                self.node(node).children[*step as usize]
            });


        (parent, *last as usize)
    }


    // 节点从树里拿掉之后，清掉指向它的 ElementId 映射。
    //
    // 不清的话，lookup 会命中一个不在树里的 node id ——
    // 目前 lookup 里有 contains_key 兜底，所以这里是双保险。
    //
    // 代价是每次都要线性扫一遍全部 ElementId，
    // 节点多了得换成反向索引。
    fn forget_element_ids(
        &mut self,
        node: u64,
    ) {
        for entry in
            self.element_to_node.iter_mut()
        {
            if *entry == Some(node) {
                *entry = None;
            }
        }
    }
}


// ============================================================
// 布局（taffy）
// ============================================================
//
// 分工定死了：
//
//     Qt 侧   只负责「画」和「谁是谁的孩子」
//     taffy   只负责「每个孩子多大、摆在父节点的哪个位置」
//
// 所以 C++ 那段内联 QML 里不再有 x / y / width / height，
// 几何的唯一作者是 relayout()。
//
// 检验办法（负向检查）：把 main() 里那句 relayout() 注释掉再跑，
// 屏幕应该只剩背景色 —— 因为除了窗口本身，没有任何东西有尺寸。
//
// 还剩的一件：
//   - 字体：两边现在都用 QGuiApplication::font()（C++ 那边是默认构造的
//     QFont，QML 里那个 Text 没写 font）。所以「量的」和「画的」恰好是
//     同一个字体，Δ 才是 0。一旦 font 变成 Rust 下发的样式属性，
//     measure_layout 必须收同一个 QFont，不能各拿各的 ——
//     改成从 item 量之后这条更要做，因为量的时候就该用那个 item 的字体。
//
// 窗口 resize 已经接了：C++ 的 ResizeFilter 收到 QEvent::Resize 就叫
// qt_window_resized → relayout，视口的两个数每次都是当场问 Qt 的。
// 配合根节点的 width/height: "100%" 和中间那块 flex_grow: "1"，
// 拖动窗口时屏幕上能看见伸缩（不是只有日志里的数字变了）。

// ============================================================
// 属性的值：Dioxus 那边有四种形态，第五种是「这个属性没了」
// ============================================================
//
// AttributeValue 的全体变体写死在 dioxus-core-0.7.10/src/nodes.rs:749-770：
//     Text(String) / Float(f64) / Int(i64) / Bool(bool)
//     / Listener(..) / Any(..) / None
//
// 前一版只认 Text，其余全丢在 `let AttributeValue::Text(text) = value`
// 那一步 —— 于是 rsx 里不带引号的 `width: 110` 是白写，
// `color: None`（`Option::None` 也会变成 AttributeValue::None，
// nodes.rs:1117-1124）更是什么都不会发生。
//
// 这里把能表达的四种收成 PropValue，并把 None 单独当成一个**动作**（清空）。
// 两种要走门口拒绝而不是静默吞掉的：
//   - Any：`Rc<dyn AnyValue>` 和 Qt 的属性系统之间没有对应类型，翻译不出来；
//   - Listener：它根本不该出现在这条路上 —— write_attribute 在更早的地方
//     就把它换成 CreateEventListener 了（diff/node.rs:525-538）。
//     真走到这里说明上面那层假设破了，得让它响。

#[derive(Clone, Debug, PartialEq)]
enum PropValue {
    Text(String),
    Int(i64),
    Float(f64),
    Bool(bool),
}


/// set_attribute 拿到一份值之后的三个决定之一。
#[derive(Debug, PartialEq)]
enum AttrInput {
    /// 写一个值进去
    Write(PropValue),

    /// AttributeValue::None：属性被撤了，
    /// 布局属性从账本里摘掉，Qt 属性回到对象刚建出来时的值
    Clear,

    /// 我们表达不了这种值，带理由（和 unsupported_reason 同一套路：
    /// 出声拒绝，不静默）
    Refused(&'static str),
}


fn classify_attr(
    value: &AttributeValue,
) -> AttrInput {
    match value {
        AttributeValue::Text(text) => {
            AttrInput::Write(PropValue::Text(text.clone()))
        }
        AttributeValue::Int(v) => {
            AttrInput::Write(PropValue::Int(*v))
        }
        AttributeValue::Float(v) => {
            AttrInput::Write(PropValue::Float(*v))
        }
        AttributeValue::Bool(v) => {
            AttrInput::Write(PropValue::Bool(*v))
        }

        AttributeValue::None => AttrInput::Clear,

        AttributeValue::Any(_) => {
            AttrInput::Refused(
                "Any(Rc<dyn AnyValue>) 没有对应的 Qt 属性类型，翻译不了"
            )
        }
        AttributeValue::Listener(_) => {
            AttrInput::Refused(
                "监听器不走属性这条路：它应该由 write_attribute 变成 CreateEventListener"
            )
        }
    }
}


// 把四种值包成对应类型的 QVariant —— 这是「类型化」的全部意义：
// Qt 那边 `visible` 要 bool、`opacity` 要 double、`text` 要 QString，
// 全靠 setProperty 的隐式转换的话，字符串往 bool 属性里塞 "false"
// 会转成 true（非空即真），那是「看着对其实错」的一类坑。
//
// 四种 QVariantValue 实现在 cxx-qt-lib-0.10.0/src/core/qvariant/mod.rs:210-216
// （bool / f64 / i64 / QString 都在表上），所以这里不需要为哪个类型加一支 C++。
fn variant_of(
    value: &PropValue,
) -> cxx_qt_lib::QVariant {
    match value {
        PropValue::Text(text) => {
            let text = cxx_qt_lib::QString::from(text.as_str());

            cxx_qt_lib::QVariant::from(&text)
        }

        PropValue::Int(v) => cxx_qt_lib::QVariant::from(v),
        PropValue::Float(v) => cxx_qt_lib::QVariant::from(v),
        PropValue::Bool(v) => cxx_qt_lib::QVariant::from(v),
    }
}


// ============================================================
// 属性账本：每个节点记「现在写过的属性各是什么值」
// ============================================================
//
// 两个动作抽成自由函数（而不是只写在 QtRenderer 的方法里），
// 是为了让 mod tests 能不带 Qt 断言它们 —— 尤其「清空」这一条，
// 光看日志不够，得能证明账本里真的少了那一条。
//
// 同一名字只留一份：覆盖，不是往后追加。
// 追加的话「最后一次写的是什么」没人说得清，
// 清空还要摘好几条，漏摘一条就变成账本上一个幽灵。

fn set_prop_entry(
    entries: &mut Vec<(&'static str, PropValue)>,
    name: &'static str,
    value: PropValue,
) {
    match entries
        .iter_mut()
        .find(|(key, _)| *key == name)
    {
        Some(slot) => slot.1 = value,

        None => entries.push((name, value)),
    }
}


/// 返回「本来有没有这一条」。
/// 没有就返回 false —— 调用方别在日志里谎称清掉了什么。
fn clear_prop_entry(
    entries: &mut Vec<(&'static str, PropValue)>,
    name: &'static str,
) -> bool {
    let before = entries.len();

    entries.retain(|(key, _)| *key != name);

    entries.len() != before
}


// 这些属性名不写给 Qt，只进布局。
// 名字同时接受下划线和短横两种写法：rsx! 里两种都能写。
const LAYOUT_PROPS: &[&str] = &[
    "width",
    "height",
    "gap",
    "flex_direction",
    "flex-direction",
    "flex_grow",
    "flex-grow",
];


fn is_layout_prop(
    name: &str,
) -> bool {
    LAYOUT_PROPS.contains(&name)
}


// 只接受纯数字，给 gap / flex_grow 这类「没有百分比语义」的值用。
//
// 故意不接受 "50%"：间距按百分比要走父节点的尺寸，grow 的份数更不是长度，
// 现在支持它就会让人以为整条百分比链路都通了。
fn parse_length(
    value: &str,
) -> Option<f32> {
    value.trim().parse::<f32>().ok()
}


// 尺寸属性（width / height）的两种写法：
//     "110"    → 死的 110
//     "100%"   → 跟着父节点 / 视口走，这是「拖动窗口能看到界面伸缩」的前提
// 其它一律当没写过（不猜）。
//
// 百分比这里要除以 100：taffy 的 `Dimension::percent` 收的是 [0.0, 1.0]
// 那个区间，不是 [0.0, 100.0]
// （taffy-0.15.0/src/style/dimension.rs:396-402，注释里加粗写了 NOTE）。
// 第一版没除，"100%" 直接变成 100 倍：日志里那一行
// node 4  35400x48 就是父节点 354 被乘出来的。
fn parse_size(
    value: &str,
) -> Option<Dimension> {
    let text = value.trim();

    if let Some(percent) = text.strip_suffix('%') {
        return percent
            .trim()
            .parse::<f32>()
            .ok()
            .map(|v| Dimension::percent(v / 100.0));
    }

    parse_length(text).map(Dimension::length)
}


// 类型化的值 → 长度。
//
// Int / Float 就是数，不用解析字符串；字符串走上面那套「纯数字」规则；
// 布尔不是长度（`gap: true` 应当算没写过，而不是算 1）。
fn length_of(
    value: &PropValue,
) -> Option<f32> {
    match value {
        PropValue::Int(v) => Some(*v as f32),
        PropValue::Float(v) => Some(*v as f32),
        PropValue::Text(text) => parse_length(text),
        PropValue::Bool(_) => None,
    }
}


// 类型化的值 → 尺寸。
//
// 百分比只有字符串写得出：`width: 100`（Int）的意思是 100 个像素，
// 不是 100%。这条区分是故意的，别「顺手」给数字也加百分比语义。
fn dimension_of(
    value: &PropValue,
) -> Option<Dimension> {
    match value {
        PropValue::Text(text) => parse_size(text),
        other => length_of(other).map(Dimension::length),
    }
}


// 方向只认那两个词；其它值（包括拼错的字符串和任何布尔）当没写过。
// 前一版是「不是 column 就设成 Row」，那等于把「看不懂」当成「看明白了」：
// 读的人从结果里分不出「写了 row」和「写了 colum」。
fn direction_of(
    value: &PropValue,
) -> Option<FlexDirection> {
    match value {
        PropValue::Text(text) => match text.as_str() {
            "column" => Some(FlexDirection::Column),
            "row" => Some(FlexDirection::Row),
            _ => None,
        },

        _ => None,
    }
}


// 把记下来的属性拼成 taffy 的输入。
//
// 没写 width / height 的节点就是 auto —— 被清空（AttributeValue::None）
// 之后也是 auto，因为账本里那一条已经被摘掉了（见 clear_prop_entry）。
// 对文本节点，auto 现在有人接了：relayout 的测量回调会问 Qt 要尺寸。
// 对矩形 / Item，auto 还是「按内容撑开」而内容没有尺寸来源 → 算出 0，
// 屏幕上就是空白。这是刻意的：几何不写就没有，不会被 Qt 的
// implicit size 偷偷填上。
fn style_for(
    props: &[(&'static str, PropValue)],
) -> Style {
    let mut style = Style {
        display: Display::Flex,
        ..Default::default()
    };


    for (name, value) in props {
        match *name {
            "width" => {
                if let Some(v) = dimension_of(value) {
                    style.size.width = v;
                }
            }

            "height" => {
                if let Some(v) = dimension_of(value) {
                    style.size.height = v;
                }
            }

            "gap" => {
                if let Some(v) = length_of(value) {
                    style.gap = Size {
                        width: length(v),
                        height: length(v),
                    };
                }
            }

            // grow：把父节点在主轴上剩下来的空间按份数分掉。
            // 这一支 + 「父节点用 100%」是界面上能跟着窗口伸缩的机制本身。
            "flex_grow" => {
                if let Some(v) = length_of(value) {
                    style.flex_grow = v;
                }
            }

            "flex_direction" | "flex-direction" => {
                if let Some(direction) = direction_of(value) {
                    style.flex_direction = direction;
                }
            }

            // 数字、百分比都解析不出来的（"" / "abc" / "50vw"）当没写过，不猜。
            _ => {}
        }
    }


    style
}


// ============================================================
// Dioxus -> Qt
// ============================================================
//
// 这个 impl 就是整个项目的「渲染层」，
// WriteMutations 是 Dioxus 给第三方 renderer 的全部接口
// （dioxus-core-0.7.10/src/mutations.rs:14）。
// 实现到哪条，UI 上就支持哪种写法，没实现的要么 panic 要么默默丢掉。
//
// 首次渲染（mount）实际会发出的那几条，按本项目的 app() 排一下：
//
//     LoadTemplate(<qt-text>)         id=ElementId(1)   → 压栈 1 个
//     SetAttribute(text, "Hello…")    id=1              → 写 Qt 属性
//     LoadTemplate(<qt-rectangle>)    id=ElementId(2)   → 压栈 1 个
//     ReplacePlaceholder([0], m=1)    → 弹 1 个，挂进 qt-text 的坑位
//     AppendChildren(ElementId(0), m=1) → 弹 1 个，挂进窗口根
//
// 更新（diff）走的是另外一组：SetText、ReplaceWith、Remove、
// InsertAfter/InsertBefore —— 见下面「这一版用不到」那一节的注释。

impl WriteMutations for QtRenderer {
    // ========================================================
    // 最重要 #1
    // ========================================================
    //
    // LoadTemplate 是「把一棵静态 Qt 子树建出来」的入口，
    // 也是本项目里唯一会调用 create_element 的地方
    // （换句话说：只有它认识 <qt-text> / <qt-rectangle>）。
    //
    // 契约：必须在栈上留下恰好 1 个节点。
    // Dioxus 那边是硬编码这个数量的
    // （diff/node.rs:632-634「This creates one node on the stack」），
    // 后面 AppendChildren / ReplacePlaceholder 就按这个数目去弹，
    // 所以这里的 stack.push 既不能省、也不能多。

    fn load_template(
        &mut self,
        template: Template,
        index: usize,
        id: ElementId,
    ) {
        let root =
            &template.roots[index];


        // 组件的边界在这里是「透明」的：
        //
        //     rsx! { Text { text: "..." } }
        //
        // 送过来的模板已经是组件内部那个 <qt-text> 元素，
        // 所以 renderer 完全不需要知道 Text 是个组件。
        //
        // 组件嵌套只是 VirtualDom 里的 scope 嵌套，
        // 落到指令流层面跟手写 <qt-text> 没有区别。
        //
        // index（也就是 template.roots 的下标）为什么用不上：
        // rsx! 允许多根，但我们的组件全是单根，
        // 所以每个模板只会走到 roots[0]。

        // 这张表只描述**这一个**实例，进来先清空。
        // 敢只留最近一次的依据见字段注释（diff/node.rs:616-620：
        // 一个实例的所有属性 mutation 一定排在它的孩子被 load 之前）。
        self.instance_paths.clear();

        let node =
            self.create_template_node(root);


        // 根节点自带 ElementId；模板深处那些有动态属性的节点，
        // 由紧跟其后的 AssignId 补上（见 assign_node_id）。
        self.bind(id, node);


        self.stack.push(node);


        println!(
            "[Dioxus] LoadTemplate \
             index={index} id={} -> node {node}",
            id.0
        );
    }

    // 把栈顶 m 个节点挂到 id 那个节点下面。
    //
    // 本项目里它只在最外层出现一次：VirtualDom::rebuild 收尾时固定发
    // append_children(ElementId(0), m)（virtual_dom.rs:593），
    // 也就是「把 app() 渲染出来的那 m 个根节点挂到窗口根上」。
    //
    // 注意父节点是用 id 找的，不是用栈顶找的 ——
    // 跟下面的 replace_placeholder_with_nodes 正好相反，
    // 那个是靠栈顶当锚点。
    fn append_children(
        &mut self,
        id: ElementId,
        m: usize,
    ) {
        println!(
            "[Dioxus] AppendChildren \
             parent={} m={}",
            id.0,
            m
        );


        let children =
            self.pop_m(m);


        let parent =
            self.lookup(id)
                .unwrap_or_else(|| {
                    panic!(
                        "AppendChildren 的父节点 \
                         ElementId({}) 不存在",
                        id.0
                    )
                });


        for child in children {
            self.attach(parent, child);
        }
    }

    // 动态文本第一次出现。
    //
    // 对应 rsx! 里插值出来的那种文本（"{some_value}" 独立成一个节点），
    // 不是模板里写死的静态文本 —— 静态那条在
    // create_template_node 的 TemplateNode::Text 分支里已经建好了。
    //
    // 同样要压栈：Dioxus 侧认为这个操作在栈上留 1 个节点
    // （diff/node.rs:876-890）。
    fn create_text_node(
        &mut self,
        value: &str,
        id: ElementId,
    ) {
        println!(
            "[Dioxus] CreateTextNode \
             id={} value={value:?}",
            id.0
        );


        let object =
            self.host
                .pin_mut()
                .create_text(value);


        let node =
            self.new_node(Some(object));


        self.bind(id, node);


        self.stack.push(node);
    }

    // 改一个已存在文本节点的内容 —— 这是 diff（更新）路径的活儿
    // （dioxus-core-0.7.10/src/diff/node.rs:217）。
    //
    // 实现上就是往 Qt 对象的 text 属性再写一次。
    // 注意找不到对象时直接跳过：这是「更新丢失」而不是错误，
    // 现在 app() 是静态的所以不会走到，接上 signal 之后要留意。
    fn set_node_text(
        &mut self,
        value: &str,
        id: ElementId,
    ) {
        println!(
            "[Dioxus] SetText \
             id={} value={value:?}",
            id.0
        );


        if let Some(object) =
            self.object_of(id)
        {
            let value = cxx_qt_lib::QString::from(value);
            let value = cxx_qt_lib::QVariant::from(&value);

            // SAFETY: object 来自 object_of，即某个还活在树上的节点。
            unsafe {
                self.host
                    .pin_mut()
                    .set_property(
                        object,
                        "text",
                        &value,
                    );
            }
        }
    }


    // 把一个「早就建好的节点」重新压回栈。
    //
    // 为什么需要：栈是 m 的来源。
    // 列表 diff（key 变了、if 换了分支、for 重排）要把旧节点搬个位置，
    // 旧节点不在任何新建流程里，就只能靠 PushRoot 手动放回栈上，
    // 后面的 InsertAfter / InsertBefore / ReplaceWith 才弹得到它。
    //
    // 唯一发出点是 dioxus-core-0.7.10/src/diff/iterator.rs:506 和 :511
    // （VNode::push_all_root_nodes）。
    //
    // 也就是说：这个方法目前根本不会被调用 ——
    // 它服务的那几条 Insert*/Replace* 在本文件里还是 panic。
    // 实现留在这里纯粹是因为 trait 要求它有实现。
    fn push_root(
        &mut self,
        id: ElementId,
    ) {
        let node =
            self.lookup(id)
                .unwrap_or_else(|| {
                    panic!(
                        "unknown ElementId({})",
                        id.0
                    )
                });


        self.stack.push(node);
    }


    // ========================================================
    // 更新（diff）路径上的那几条 mutation。
    //
    // 它们首次渲染都不会发，只有重渲染才会有：
    //
    //     assign_node_id      → 内层元素补 ElementId
    //                           （diff/node.rs:856-874）
    //     replace_node_with   → 旧节点整坨被换掉（diff/node.rs:326、381）
    //     remove_node         → 旧节点删除（diff/node.rs:328、383）
    //     insert_nodes_before → 列表重排（diff/iterator.rs:447）
    //     insert_nodes_after  → 列表重排（diff/iterator.rs:467）
    //
    // create_placeholder / replace_placeholder_with_nodes 属于 mount 路径，
    // 和上面这些混在这一段里只是因为写下的顺序。
    // ========================================================

    // 给模板深处的静态元素补一个 ElementId。
    //
    // path 语义跟 resolve_slot 一样（相对栈顶那个模板根）。
    //
    // 什么时候会收到它 —— dioxus 那边的原文写在 diff/node.rs:856-874
    // （assign_static_node_as_dynamic）：一个模板节点身上挂了**动态属性**，
    // 而它又不是这次 load 的根节点，dioxus 就现开一个 ElementId、
    // 先发 assign_node_id(&path[1..], id)、紧跟着一串 set_attribute(id, ...)。
    // 根节点不需要它（根的 id 从 LoadTemplate 就有了，走 :865 那条 return）。
    //
    // 所以这条 mutation 只出现在「属性写在原生元素上」的场合：
    // 用 Text { } 这类组件时，动态 prop 落在子模板的根上，永远不需要 AssignId。
    fn assign_node_id(
        &mut self,
        path: &'static [u8],
        id: ElementId,
    ) {
        let found = self.instance_paths
            .iter()
            .find(|(node_path, _)| node_path.as_slice() == path)
            .map(|(_, node)| *node);

        let node = found.unwrap_or_else(|| {
            panic!(
                "AssignId path={path:?} id={} 找不到节点 —— \
                 最近一次 LoadTemplate 的实例表里只有 {} 条路径。\
                 要么是 id 发在了孩子被 load 之后（dioxus 不会这么发，\
                 见 diff/node.rs:616-620），要么是我们的建序和模板不一致。",
                id.0,
                self.instance_paths.len(),
            )
        });

        self.bind(id, node);

        println!(
            "[Dioxus] AssignId path={path:?} id={} → node {node}",
            id.0
        );
    }


    // 建一个「以后可能长成东西」的空位。
    //
    // Dioxus 自己给的理由写在 mutations.rs:30-34：
    // 列表 diff 需要一个可重入的锚点，
    // 现在什么都没有的位置也得在真实树里留个格子，
    // 不然下次这一格变成真节点时没地方插。
    //
    // 发出点：diff/node.rs:892-905，
    // 对应 dynamic node 的值是 Placeholder 的情况
    // （组件没传 children、或者 `{None}` 这类空 Element）。
    fn create_placeholder(
        &mut self,
        id: ElementId,
    ) {
        // 占位节点：树里有位置，Qt 里还没有对象。
        //
        // 组件没传 children 时 Dioxus 会发这个，
        // 紧接着用 ReplacePlaceholder 把它塞进
        // 模板里的动态位置。
        let node =
            self.new_node(None);


        self.bind(id, node);


        self.stack.push(node);


        println!(
            "[Dioxus] CreatePlaceholder \
             id={} -> node {node}",
            id.0
        );
    }


    // 用栈顶 m 个新节点替换掉 id 那个节点（连同它的子树）。
    //
    // 什么时候会来：同一个位置换了「模板」。
    // diff_node 一看 template 不一样就整棵换掉
    // （diff/node.rs:53-58 → 先 create_children 建好 m 个，
    //  再 remove_node_inner(..., Some(m)) —— m 传进去了，
    //  于是发的是 ReplaceWith 而不是单独的 Remove）。
    //
    // m = 0 是合法的：那就是「换成空」，等于纯删除。
    fn replace_node_with(
        &mut self,
        id: ElementId,
        m: usize,
    ) {
        let new_nodes = self.pop_m(m);

        let target = self.lookup(id).unwrap_or_else(|| {
            panic!(
                "ReplaceWith 的目标 ElementId({}) 不在树里",
                id.0
            )
        });

        // 位置必须在删之前取：destroy_subtree 会把这一格从父亲那里摘掉，
        // 摘完之后「原来的下标」正好还是新节点该插进去的地方。
        let parent = self.node(target).parent.unwrap_or_else(|| {
            panic!(
                "ReplaceWith：node {target} 没有父节点，没地方插"
            )
        });

        let index =
            self.index_in_parent(parent, target).unwrap_or_else(|| {
                panic!(
                    "ReplaceWith：node {target} 不在 node {parent} \
                     的孩子列表里"
                )
            });

        let removed = self.destroy_subtree(target);

        self.insert_nodes_at(parent, index, &new_nodes);

        println!(
            "[Dioxus] ReplaceWith id={} → 删 node {target}\
             （子树 {removed} 个），换上 {} 个节点",
            id.0,
            new_nodes.len(),
        );
    }


    // ========================================================
    // 最重要 #2：模板里的动态坑位被真节点顶掉
    // ========================================================
    //
    // 这是整个 renderer 里唯一真正的「动态内容」入口，
    // 也是把 Dioxus 的 diff 能力接过来的关键一步。
    //
    // 全 dioxus-core 只有一个发出点：diff/node.rs:776，
    // 在 load_placeholders 里面 —— 换句话说这条只属于 mount 路径，
    // 更新时同一个位置走的是 ReplaceWith / InsertAfter（见上面那节）。
    //
    // 三条必须遵守的契约，都是库那边写死的：
    //
    // 1. path 相对「栈顶节点」，而栈顶此刻必须是模板根。
    //    node.rs:622 的注释直接写着
    //    "This operation relies on the fact that the root node is the
    //    top node on the stack"；
    //    node.rs:774-775 传的是 &node_paths[idx][1..]，
    //    即把「第几个 root」那一段剥掉之后的剩余部分。
    //
    // 2. m 是刚压上栈的节点个数，弹完 m 个，栈顶得回到模板根。
    //    这就是下面 anchor 敢用 stack.last() 的原因。
    //
    // 3. 同一个模板里多个坑位是「倒序」处理的（node.rs:749-761），
    //    因为一次替换会把 1 个下标变成 m 个，污染后面的 path；
    //    倒着做就只会污染已经处理完的位置。
    //    issue 2809 就是没倒序导致的，别把这条弄坏了。
    fn replace_placeholder_with_nodes(
        &mut self,
        path: &'static [u8],
        m: usize,
    ) {
        let new_nodes =
            self.pop_m(m);


        // 路径是相对「模板根」的，
        // 而模板根就是把 m 个节点弹掉之后的栈顶。
        let anchor =
            *self.stack.last().expect("空栈");


        let (parent, slot) =
            self.resolve_slot(anchor, path);


        // Qt 侧要插在「原来那个位置的下一个兄弟」前面，
        // 子节点顺序才跟 Dioxus 一致。
        //
        // 两个细节：
        //   - 从 slot+1 往后找，是因为坑位自己就在 slot；
        //   - 用 find_map 而不是直接取 slot+1，是因为后面的兄弟
        //     可能还是个占位节点，在 Qt 里没有对象，拿它当锚点没意义。
        //
        // 也正因为坑位右侧的兄弟会先被处理（契约 3 的倒序），
        // 到这里它们通常已经是真对象了，这个查找才有东西可找到。
        let next_sibling =
            self.node(parent)
                .children
                .iter()
                .skip(slot + 1)
                .find_map(|node| {
                    self.object_of_node(*node)
                });


        // Qt 侧的挂载。
        //
        // mount 只能往 parent 的孩子末尾追加（QQuickItem 后挂的排上面），
        // 所以插回原位还得再补一次 stack_before。
        //
        // 多个新节点共用同一个 sibling 也不会把顺序弄反：
        // stackBefore 的语义是「紧贴在这个 sibling 下方」，
        // 于是 [A, B] 依次 stackBefore(sib) 得到 [A, B, sib]。
        if let Some(parent_object) =
            self.object_of_node(parent)
        {
            for node in new_nodes.iter() {
                let Some(child_object) =
                    self.object_of_node(*node)
                else {
                    // 占位节点没有 Qt 对象，不用挂。
                    // 树那边照样给它留位置（下面 splice），
                    // 它以后长成真节点时由那时候的 mutation 负责挂载。
                    continue;
                };


                // SAFETY: parent_object / child_object / sibling 都是
                // create_* 返回过、且还没被删的指针。
                // 这里也解释了为什么「先 mount 再 stack_before」是安全的：
                // stackBefore 要求 child 已经在这个 parent 的 childItems 里。
                unsafe {
                    self.host
                        .pin_mut()
                        .mount(
                            parent_object,
                            child_object,
                        );


                    if let Some(sibling) = next_sibling {
                        self.host
                            .pin_mut()
                            .stack_before(
                                child_object,
                                sibling,
                            );
                    }
                }
            }
        }


        // 树里：用新节点顶掉占位节点。
        //
        // 顺序是「先 Qt 后树」：上面已经把 Qt 对象挂好了，
        // 这里才动自己的账本。
        //
        // 外面套一层 {} 不是风格问题：
        // children 是 self.nodes 的可变借用，
        // 借用没结束就没法调用 self.nodes.remove / self.forget_element_ids，
        // 所以把借用限制在这个块里，出来之后 placeholder 还是个普通 u64。
        let placeholder = {
            let children =
                &mut self.nodes
                    .get_mut(&parent)
                    .expect("unknown parent node")
                    .children;


            let placeholder = *children
                .get(slot)
                .unwrap_or_else(|| {
                    panic!(
                        "ReplacePlaceholder 的路径 \
                         指向的子节点不存在"
                    )
                });


            // 1 个位置换成 m 个 —— 父节点里 slot 之后的下标全部右移，
            // 这就是契约 3（Dioxus 倒序处理坑位）存在的原因。
            children.splice(
                slot..slot + 1,
                new_nodes.clone(),
            );


            placeholder
        };


        // 顶上来的这些孩子现在有父亲了 —— 反向账补上。
        // （借用得等上面那个块结束：children 和 nodes.get_mut 撞车。）
        for new in new_nodes.iter() {
            self.nodes
                .get_mut(new)
                .expect("unknown node")
                .parent = Some(parent);
        }


        // 坑位节点本身作废：从账本里删掉，
        // 并把指向它的 ElementId 映射清空
        // （不删的话 object_of 之类会命中一个不在树里的节点）。
        self.nodes.remove(&placeholder);
        self.forget_element_ids(placeholder);


        // 父节点的孩子列表刚刚从 1 格变成了 m 格：布局树跟上。
        self.sync_taffy_children(parent);


        println!(
            "[Dioxus] ReplacePlaceholder \
             path={path:?} m={m} \
             node {placeholder} 被顶掉"
        );
    }


    // 把栈顶 m 个节点插到 id 那个节点后面。
    fn insert_nodes_after(
        &mut self,
        id: ElementId,
        m: usize,
    ) {
        let new_nodes = self.pop_m(m);

        self.insert_relative(id, &new_nodes, true);

        println!(
            "[Dioxus] InsertAfter id={} → 插了 {} 个节点",
            id.0,
            new_nodes.len(),
        );
    }


    // 把栈顶 m 个节点插到 id 那个节点前面。
    fn insert_nodes_before(
        &mut self,
        id: ElementId,
        m: usize,
    ) {
        let new_nodes = self.pop_m(m);

        self.insert_relative(id, &new_nodes, false);

        println!(
            "[Dioxus] InsertBefore id={} → 插了 {} 个节点",
            id.0,
            new_nodes.len(),
        );
    }


    // 属性分两条路，别混：
    //
    //     模板里写死的   text: "字面量"
    //         → TemplateAttribute::Static，
    //           在 create_template_node 里建元素时就 apply 了，
    //           永远不会再发 mutation
    //
    //     模板里是插值   text: "{expr}"
    //         → TemplateAttribute::Dynamic，
    //           mount 时由 write_attrs（diff/node.rs:795 起）发 SetAttribute，
    //           之后每次值变化还会各发一次
    //
    // 本项目 app() 里 "Hello from Dioxus…" 是组件的 props，
    // 对 <qt-text> 的模板来说它就是插值，所以走这条。
    fn set_attribute(
        &mut self,
        name: &'static str,
        _ns: Option<&'static str>,
        value: &AttributeValue,
        id: ElementId,
    ) {
        // 动态属性走这里。例如组件传下来的 text：
        //
        //     模板里是 @dynamic#0
        //
        //     Dioxus:
        //
        //     SetAttribute { id: 1, value: Text("Hello ...") }
        //
        // id 就是 load_template 里那个元素的 ElementId。
        //
        // 值的四种形态（Text / Int / Float / Bool）先收成 PropValue，
        // 再按「是不是布局属性」分两条路走。两条路的分别只有一处：
        // 布局属性只记账（最终归宿是 taffy 的输入，由 relayout()
        // 通过 set_geometry 推回去；让 set_property 也写一遍就会出现两个作者），
        // Qt 属性直接落到对象上。
        //
        // **清空（AttributeValue::None）在两条路上都是一等公民**，
        // 只是清的东西不一样：
        //     布局属性 → 账本里那一条摘掉，于是它回到 auto
        //     Qt 属性   → 对象上那个属性回到「刚建出来时的值」（C++ 那份抄存）
        match classify_attr(value) {
            AttrInput::Refused(reason) => {
                println!(
                    "[renderer] 属性 {name} 被拒绝：{reason} \
                     （id={}，值={value:?}）",
                    id.0
                );
            }

            AttrInput::Clear if is_layout_prop(name) => {
                match self.lookup(id) {
                    Some(node) => {
                        let removed = self.forget_style(node, name);

                        println!(
                            "[Dioxus] SetAttribute(None) id={node} \
                             布局属性 {name} —— {}",
                            if removed {
                                "账本里那条已摘掉，尺寸回到 auto"
                            } else {
                                "本来就没写过，没什么可摘"
                            }
                        );
                    }

                    None => println!(
                        "[renderer] SetAttribute(None) id={} \
                         找不到节点，布局属性 {name} 的清空被丢弃",
                        id.0
                    ),
                }
            }

            AttrInput::Clear => {
                match self.object_of(id) {
                    Some(object) => {
                        println!(
                            "[Dioxus] SetAttribute(None) id={} {name} \
                             → 请 Qt 写回刚建出来时的值",
                            id.0
                        );

                        // SAFETY: object 是 object_of 查出来的、
                        // 某个还活在树上的节点的指针。
                        unsafe {
                            self.host
                                .pin_mut()
                                .clear_property(
                                    object,
                                    name,
                                );
                        }
                    }

                    None => println!(
                        "[renderer] SetAttribute(None) id={} \
                         还没有映射到 Qt 对象，{name} 的清空被丢弃",
                        id.0
                    ),
                }
            }

            AttrInput::Write(prop) if is_layout_prop(name) => {
                match self.lookup(id) {
                    Some(node) => {
                        println!(
                            "[Dioxus] SetAttribute 布局 id={node} \
                             {name}={prop:?}"
                        );

                        self.record_style(
                            node,
                            name,
                            prop,
                        );
                    }

                    None => println!(
                        "[renderer] SetAttribute id={} \
                         找不到节点，布局属性被丢弃",
                        id.0
                    ),
                }
            }

            AttrInput::Write(prop) => {
                let Some(object) =
                    self.object_of(id)
                else {
                    println!(
                        "[renderer] SetAttribute \
                         id={} 还没有映射到 Qt 对象",
                        id.0
                    );

                    return;
                };

                println!(
                    "[Dioxus] SetAttribute \
                     id={} {name}={prop:?}",
                    id.0
                );

                let variant = variant_of(&prop);

                // SAFETY: object 是 object_of 查出来的、
                // 某个还活在树上的节点的指针。
                unsafe {
                    self.host
                        .pin_mut()
                        .set_property(
                            object,
                            name,
                            &variant,
                        );
                }
            }
        }
    }


    // Dioxus 说：这个 ElementId 身上挂了一个事件监听器（rsx 里的 onclick）。
    //
    // name 是 Dioxus 把 rsx 属性名剥掉 `on` 之后交过来的（diff/node.rs:533），
    // 所以 onclick 到这里已经是 "click"。
    //
    // 这一道是**唯一**的准入检查：C++ 那边按名字决定吃什么 QEvent、
    // 送回来哪一支回调（EVENT_SPECS），两边同一份口径。
    // 不支持的名字必须在这里拒绝，不能照装 ——
    // 照装的后果不是「没反应」而是「反应错」：dioxus 派发时只比名字
    // （runtime.rs:435 `attr.name.get(2..) == Some(name)`），
    // 名字对得上 handler 就真跑起来，时机却是错的，而且一声不响。
    fn create_event_listener(
        &mut self,
        name: &'static str,
        id: ElementId,
    ) {
        if name == "mounted" {
            // 这一支不需要 Qt 给事实：元素建出来就是「挂上了」。
            // 但**不能在这里直接派发** —— 我们此刻正在 render_immediate 里面，
            // PUMP 的 borrow_mut 还握着，而 handle_event 会 wake，
            // wake 又要回来 poll。所以记到批处理结束后由回路补发
            // （见 drain_pending_mounted）。
            queue_mounted(id.0 as u32);

            println!(
                "[Dioxus] CreateEventListener mounted id={} \
                 排进「本轮渲染结束后补发」",
                id.0
            );

            return;
        }

        if let Some(reason) = unsupported_reason(name) {
            println!(
                "[Dioxus] CreateEventListener {name} id={} \
                 不装 filter：{reason}（这个监听器不会响）",
                id.0
            );

            return;
        }

        let Some(node) = self.lookup(id) else {
            println!(
                "[Dioxus] CreateEventListener {name} id={} \
                 还没有映射到节点，监听器装不上",
                id.0
            );

            return;
        };

        let Some(object) = self.object_of_node(node) else {
            println!(
                "[Dioxus] CreateEventListener {name} id={} \
                 是占位节点（node {node}），装不了监听器",
                id.0
            );

            return;
        };

        // SAFETY: object 来自 object_of_node，也就是某个还活在树上的节点的指针；
        //            filter 的所有权交给 Qt（parent 就是那个 item）。
        unsafe {
            self.host.pin_mut().add_event_listener(
                object,
                name,
                id.0 as u32,
            );
        }

        println!(
            "[Dioxus] CreateEventListener {name} id={} → node {node} 已装到 Qt 上",
            id.0
        );
    }


    fn remove_event_listener(
        &mut self,
        name: &'static str,
        id: ElementId,
    ) {
        if name == "mounted" {
            // 挂上这件事已经发生过了，撤无可撤。
            return;
        }

        if unsupported_reason(name).is_some() {
            // 当初就没装，这里也没有东西要撤。
            return;
        }

        let Some(node) = self.lookup(id) else {
            return;
        };

        let Some(object) = self.object_of_node(node) else {
            return;
        };

        // SAFETY: 同上 —— 指针来自还活着的节点。
        unsafe {
            self.host
                .pin_mut()
                .remove_event_listener(object, name);
        }

        println!(
            "[Dioxus] RemoveEventListener {name} id={}",
            id.0
        );
    }


    // Dioxus 说：把这个节点连同它整棵子树从 UI 里拿掉。
    //
    // 两条从源码里抠出来的契约，决定了下面为什么这么写：
    //
    // 1. 子树里嵌套的动态节点**不会**再各发一条 Remove
    //    （diff/node.rs:285-288「We don't generate mutations for these,
    //    as they will be removed by the parent」）。
    //    所以 Qt 那边只要删掉根就够（~QObject 递归删孩子），
    //    但我们自己的账必须整棵子树一起清干净 ——
    //    不然那些 Node 还挂在 nodes 里、taffy 树里也还有它们的布局节点。
    //
    // 2. 发完 Remove，Dioxus 立刻 `dom.reclaim(id)`（diff/node.rs:330）：
    //    这个 ElementId 会被重新发出去给别的节点用，
    //    所以映射必须清掉，否则 lookup 会把新 mutation 派给一个死节点。
    //
    // 顺带澄清一个容易搞错的地方：这里的 Remove 是「诀别」，不是 DOM 那种
    // 「先摘下来留着待会儿插回去」。列表重排（移动）走的是另一条路：
    // push_all_root_nodes + insert_nodes_*（diff/iterator.rs:343-375），
    // 全程不发 Remove。
    // Dioxus 说：把这个节点连同它整棵子树从 UI 里拿掉。
    //
    // 这里的 Remove 是「诀别」，不是 DOM 那种「先摘下来留着待会儿插回去」：
    // 具体契约见 destroy_subtree 上方的注释，移动走的是 insert_nodes_*。
    fn remove_node(
        &mut self,
        id: ElementId,
    ) {
        let Some(node) = self.lookup(id) else {
            println!(
                "[Dioxus] Remove id={} 找不到节点，跳过",
                id.0
            );

            return;
        };

        let size = self.destroy_subtree(node);

        println!(
            "[Dioxus] Remove id={} → node {node}，\
             连带子树 {size} 个节点",
            id.0
        );
    }
}


// ============================================================
// 回路：借 Qt 的事件循环当 executor
// ============================================================
//
// 形状就一句话：
//
//     有人发纸条 → dioxus 调我们的 waker → waker 让 Qt 排一个事件
//     → 事件的 functor 里 poll 一次 wait_for_work()
//     → Pending 就什么都不做，Ready 就 render_immediate + relayout
//
// 为什么要「poll 一次」而不是 `.await`：
// `wait_for_work` 是 `async fn`（virtual_dom.rs:445），`.await` 需要一个
// executor 去驱动它；而这里唯一的线程已经被 `app.exec()` 占着
// （`block_on` 会把纸条的来源也一起堵死 —— 纸条的源头恰恰是 Qt 事件回调）。
// 所以「等」这件事只有 Qt 的循环有资格做，Rust 这边只负责被叫起来 poll 一次。
//
// 一个关键的好性质：future **不需要跨事件存着**。
// wait_for_work 的全部状态都在 VirtualDom 自己身上：
// 纸条在 `self.rx`（virtual_dom.rs:215 那个 UnboundedReceiver），
// 脏 scope 在脏表里。future 本身只是「先看一眼，没有就等」这段流程，
// 所以每次被叫起来现场新建一个、循环 poll 到 Pending、丢掉，一条消息都不会漏。
//
// 这一版是 **wake 驱动**，不是轮询：
// poll 到 Pending 时我们把 waker 交了出去（Context 里那个），
// dioxus 下一次发纸条就会同步调它
// （futures-channel：unbounded_send → do_send_nb → queue_push_and_signal
//  → recv_task.wake()，mpsc/mod.rs:457）。
// 我们的 waker 只做一件事：让 Qt 把「再 poll 一次」排进事件队列
// （C++ 的 schedule_pump_poll）。于是空闲时一次 poll 都不会发生 ——
// 连那个每 200ms 推一格的假事件源 QTimer 也已经删了：
// 现在纸条只有一个来源，就是窗口里送进来的输入事件
// （QEvent → InputFilter → 对应那支 qt_*_event → runtime().handle_event）。

/// 纸条到了该干什么：把「再 poll 一次」排队交给 Qt 的事件循环。
///
/// 注意这里**不能**直接调 qt_pump_poll()：wake_by_ref 是在
/// `runtime.handle_event(...)` 里面被同步调进来的（listener 写 signal
/// → needs_update → unbounded_send → wake），那一刻 qt_handle_event
/// 正把 Pump `borrow_mut` 借着，当场 poll 就是重入。
/// QueuedConnection 把它推到本次回调返回之后再执行。
struct QtWaker;

impl Wake for QtWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        // 只是往 Qt 的事件队列里投一个 functor（QMetaObject::invokeMethod,
        // QueuedConnection）：不碰任何对象指针，也不当场回调 Rust，
        // 所以此刻就算 Pump 正被借着也没问题。
        // （它是 cxx 生成的**安全**函数：没有 self、没有指针参数。）
        ffi::schedule_pump_poll();
    }
}

/// 回路的两端：VirtualDom（脏表 + 纸条队列）和我们的 renderer，
/// 必须放在同一个地方才能同时拿到 `&mut` —— `wait_for_work(&mut self)`
/// 借的是 vdom，`render_immediate(&mut renderer)` 借的是 renderer。
struct Pump {
    vdom: VirtualDom,
    renderer: QtRenderer,
    waker: Waker,
}

// 只有主线程会碰它（Qt 事件队列里的 functor、Qt 的输入回调都在这条线程），
// 所以 thread_local + RefCell 就够（不需要 Sync，也不需要锁）。
// 真正的 re-entrance（渲染中又进来一次）会把 borrow_mut 打成 panic，
// 那是「回路里又跑回路」的信号，不是要悄悄吞掉的情况。
thread_local! {
    static PUMP: RefCell<Option<Pump>> = const { RefCell::new(None) };
}

/// 把 vdom + renderer 交给回路，并**先 poll 一次**。
///
/// 那一次先 poll 不是走形式：waker 只有在 future 被 poll 到 Pending 之后
/// 才算交给了 channel（`recv_task` 里存的就是它）。装好之后不 poll 一次，
/// 后面 dioxus 发纸条时手上没有 waker 可调，回路就永远醒不过来了。
fn install_pump(
    vdom: VirtualDom,
    renderer: QtRenderer,
) {
    PUMP.with(|slot| {
        *slot.borrow_mut() = Some(Pump {
            vdom,
            renderer,
            waker: Waker::from(Arc::new(QtWaker)),
        });
    });

    println!(
        "[pump] 回路装好了（wake 驱动、零轮询）：\
         纸条的唯一来源是窗口里的输入事件"
    );

    // 先把 waker 交出去。
    qt_pump_poll();
}


/// 收尾：把 Pump 从 thread_local 里摘出来，**趁 TLS 还活着**的时候释放。
///
/// 为什么非得主动拆，不能等线程退出时 TLS 自动析构：
/// Pump 里装着 `VirtualDom`，而 `VirtualDom::drop`
/// （dioxus-core-0.7.10/src/virtual_dom.rs:786）会把每个 scope 逐个 drop，
/// `ScopeState::drop`（scopes.rs:157）→ `Runtime::remove_scope`
/// → `RuntimeGuard::new` → `Runtime::push`（runtime.rs:312）——
/// 那一步要读写 **dioxus 自己的那个 thread_local**（runtime stack）。
/// 线程退出时各 TLS 析构器之间没有任何顺序保证：轮到 `PUMP` 的时候
/// runtime stack 很可能已经在销毁中了，于是当场 panic
/// （实测：点窗口关闭按钮 → exec 返回 → main 收尾，栈就是这个）——
///
///     cannot access a Thread Local Storage value during or after destruction: AccessError
///
/// 放在 exec 之后、main 返回之前，两件事同时成立：所有 thread_local
/// 都还在正常状态，而且 `qt_app` 还没析构（QGuiApplication 在场，
/// 删 QQuickWindow 那一串才安全）。
fn dismantle_pump() {
    // `take` 出来的值在 with 结束、借据释放之后才 drop：
    // drop 虚拟 dom 的过程中不管拐回哪里，都不会撞上 already borrowed。
    let taken = PUMP.with(|slot| slot.borrow_mut().take());

    if taken.is_some() {
        println!(
            "[pump] 回路已拆除：VirtualDom / QtRenderer 在 TLS 析构之前就还掉了"
        );
    } else {
        println!("[pump] 回路本来就没装上，没什么可拆");
    }
}


/// 回路本身：反复 poll `wait_for_work` 直到 Pending，中间每次 Ready 都渲染一轮。
/// 调用者只有一个：C++ 的 `schedule_pump_poll()` 排进事件队列的那个 functor。
///
/// **为什么必须循环到 Pending** —— 这不是为了效率，是为了重新挂上 waker：
/// `wait_for_work` 醒来的那条路径（virtual_dom.rs:452，脏表非空就直接 return）
/// 根本没碰 `rx.next()`，于是 channel 里 `recv_task` 存着的那个 waker 是空的；
/// 下一次 `unbounded_send` 就 wake 不到任何人。
/// 第一版就踩在这上面：只有第一条纸条渲染了，后面 7 条发出去没人醒。
/// 循环到 Pending 之后，最后那次 poll 会走到 `wait_for_event().await`
/// （virtual_dom.rs:460）→ `rx.next()` 把我们的 waker 存回 channel，重新武装。
///
/// 顺带这也修掉另一个问题：一次 wake 里可能已经堆了好几张纸条，
/// `process_events` 一次收不完 —— 循环能把它们全 drain 掉。
///
/// Pending 分支就是这条回路的「睡着」：什么都不做，waker 躺在 channel 里，
/// 下一次会不会被叫起来完全取决于**有没有人发纸条**，不取决于有没有定时器在转。
fn qt_pump_poll() {
    PUMP.with(|slot| {
        let mut guard = slot.borrow_mut();

        let Some(pump) = guard.as_mut() else {
            return;
        };

        let Pump {
            vdom,
            renderer,
            waker,
            ..
        } = pump;

        let mut rounds = 0usize;

        loop {
            let woke = {
                let mut fut = std::pin::pin!(
                    vdom.wait_for_work()
                );

                let mut cx = Context::from_waker(&*waker);

                fut.as_mut().poll(&mut cx).is_ready()
            };

            if !woke {
                println!(
                    "[pump] poll → Pending（waker 已交回 channel，等下一次 wake），\
                     这一轮渲染了 {rounds} 次"
                );

                break;
            }

            // 醒了才干活。
            rounds += 1;

            println!("[pump] poll → Ready #{rounds}，render_immediate");

            vdom.render_immediate(
                renderer
            );

            // 批量边界：这一轮 mutation 全落完了，算一次布局。
            renderer.relayout();

            renderer.dump_tree();
        }
    });

    // 出了 PUMP 的 borrow 才轮到补发 mounted：
    // 派发会 wake，而 wake 的那条路要回来 poll 这个 slot，
    // 在 borrow 里派发就是当场 panic（RefCell already borrowed）。
    drain_pending_mounted();
}


/// 窗口尺寸变了 → 重算一次布局。
///
/// 这条链和事件那条链一样是「Qt 先动」，但它**不经过 Dioxus**：
/// 视口大小是布局的输入，不是应用状态，vdom 里没有任何东西因此变脏，
/// 所以不需要 render，只需要 relayout —— 几何的作者是我们自己。
///
/// 也是因为这个，「攒一个 layout_dirty 标记、等下一轮再算」那套设计现在没必要：
/// 喊重算的只有两处（渲染批次结束、窗口 resize），两处都是当场就算，没有攒着的机会。
fn qt_window_resized(
    width: f64,
    height: f64,
) {
    PUMP.with(|slot| {
        let mut guard = slot.borrow_mut();

        let Some(pump) = guard.as_mut() else {
            // 第一次 show / 建树之前也会走到这里，那时候还没什么可算的。
            println!(
                "[layout] 窗口 {width:.0}x{height:.0}，回路还没装好，不重算"
            );

            return;
        };

        println!(
            "[layout] 窗口 {width:.0}x{height:.0} → relayout"
        );

        pump.renderer.relayout();

        // 顺手把树打一遍：这样离屏日志里能直接看到伸缩有没有真的发生
        // （dump_tree 里那行 叠放顺序 同时还是个免费的自检）。
        pump.renderer.dump_tree();
    });
}


// ============================================================
// 事件：Qt 送来的输入 → dioxus 的 listener
// ============================================================
//
// 这一节有四块：负载结构体（每一支一类）、转换器、派发出口、
// 以及 C++ 那一侧每个回调进来的入口。
//
// 三条从源码里读出来的契约，决定了下面的形状：
//
// 1. 名字交给 `handle_event` 时必须**已经剥掉 `on`**：
//    runtime.rs:435 是拿 `attr.name.get(2..) == Some(name)` 来比的，
//    比中的才是我们这个 listener（去程 diff/node.rs:533 也剥了一次）。
//
// 2. 负载一律 `Rc<PlatformEventData>`，它的内核是 `Box<dyn Any>`
//    （events/mod.rs:85-95）—— 所以每一支事件有自己的 Rust 结构体，
//    handler 里 `Event<MouseData>` 那种写法走的是全局转换器 downcast。
//
// 3. 转换器那个槽没装就 unwrap 炸（events/mod.rs:76-82），
//    trait 的 21 支又**没有默认实现**（events/generated.rs:353-357），
//    所以本项目把 Qt 真能送来的 8 支做成真的，剩下 13 支留着 panic ——
//    静默造一份假数据比当场炸难查。
//
// 冒泡那个布尔不归我们拍脑袋：dioxus-core-types 里有一份
// `event_bubbles()`（bubbles.rs:11-105，兜底是 `=> true`），
// runtime.rs:374-378 就是按它决定走 bubbling 还是 non-bubbling，
// 所以我们交同一个函数查同一张表。

use dioxus::html::geometry::{
    PixelsSize,
    WheelDelta,
};

use dioxus::html::{
    HasFocusData,
    HasKeyboardData,
    HasPointerData,
    HasResizeData,
    HasTouchData,
    HasTouchPointData,
    HasWheelData,
    RenderedElementBacking,
    ResizeResult,
    TouchPoint,
};

use dioxus_core_types::bubbles::event_bubbles;

use std::str::FromStr;

/// 三套坐标 + 修饰键 + 按键 —— 鼠标/指针/滚轮三支都要这几样，
/// 所以抽出来一份，各支自己再委托一遍（Rust 没有继承，只能转发）。
///
/// 坐标的来源和浏览器的对应关系：
///   local  = QSinglePointEvent::position()（元素本地，且派发器已按 item 本地化过，
///            util/qquickdeliveryagent.cpp:2553）→ element_coordinates()
///   window = scenePosition()（窗口内）                → client_/page_coordinates()
///   screen = globalPosition()（屏幕）                 → screen_coordinates()
/// 浏览器那三套（client/page/screen）在 Qt 这边没有对等物：
/// page 和 client 我们都拿窗口坐标顶上（本项目没有滚动容器，两者本来就重合），
/// 这不是偷懒，是「没有那个概念」的老实交代。
#[derive(Clone, Debug)]
struct QtCoords {
    local_x: f64,
    local_y: f64,
    window_x: f64,
    window_y: f64,
    screen_x: f64,
    screen_y: f64,
    /// Qt::KeyboardModifier 的位掩码（qnamespace.h:1071-1079）
    modifiers: u32,
    /// 换了状态的那个键，Qt 位掩码（QMouseEvent::button()）
    button: i32,
    /// 这一刻按着的一串，Qt 位掩码（QMouseEvent::buttons()）
    buttons: i32,
}

/// Qt 的修饰键位 → keyboard_types 的 Modifiers（三支柱都要，所以做成自由函数）。
///
/// 位值来自 qnamespace.h:1071-1079：Shift 0x02000000 / Control 0x04000000 /
/// Alt 0x08000000 / Meta 0x10000000。Qt 独有的 Keypad/GroupSwitch 直接丢，
/// 不硬凑一个 web 没有的位。
fn qt_modifiers(
    bits: u32,
) -> Modifiers {
    let mut out = Modifiers::empty();

    if bits & 0x0200_0000 != 0 {
        out |= Modifiers::SHIFT;
    }

    if bits & 0x0400_0000 != 0 {
        out |= Modifiers::CONTROL;
    }

    if bits & 0x0800_0000 != 0 {
        out |= Modifiers::ALT;
    }

    if bits & 0x1000_0000 != 0 {
        out |= Modifiers::META;
    }

    out
}

impl InteractionLocation for QtCoords {
    fn client_coordinates(&self) -> ClientPoint {
        ClientPoint::new(self.window_x, self.window_y)
    }

    fn screen_coordinates(&self) -> ScreenPoint {
        ScreenPoint::new(self.screen_x, self.screen_y)
    }

    fn page_coordinates(&self) -> PagePoint {
        PagePoint::new(self.window_x, self.window_y)
    }
}

impl InteractionElementOffset for QtCoords {
    fn element_coordinates(&self) -> ElementPoint {
        ElementPoint::new(self.local_x, self.local_y)
    }
}

impl ModifiersInteraction for QtCoords {
    fn modifiers(&self) -> Modifiers {
        qt_modifiers(self.modifiers)
    }
}

impl PointerInteraction for QtCoords {
    // Qt 那几位和浏览器 MouseEvent.buttons 的位是同一套
    // （Left 0x1 / Right 0x2 / Middle 0x4 / Back 0x8 / Forward 0x10，
    // 对照 dioxus-html input_data.rs:68-92），所以直接 decode，不换算。
    // 注意 dioxus 的 from_web_code 走的是另一套（button 序号 0/1/2，
    // 而且 1 是 auxiliary、2 是 secondary），我们交的是位掩码不是序号。
    fn trigger_button(&self) -> Option<MouseButton> {
        decode_mouse_button_set(self.button as u16)
            .iter()
            .next()
    }

    fn held_buttons(&self) -> MouseButtonSet {
        decode_mouse_button_set(self.buttons as u16)
    }
}

/// MouseData 的负载。
#[derive(Clone, Debug)]
struct QtMouseData {
    coords: QtCoords,
}

impl HasMouseData for QtMouseData {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// 坐标/按键那几支是纯转发，写一遍宏免得七处抄来抄去。
macro_rules! delegate_coords {
    ($ty:ty) => {
        impl InteractionLocation for $ty {
            fn client_coordinates(&self) -> ClientPoint {
                self.coords.client_coordinates()
            }

            fn screen_coordinates(&self) -> ScreenPoint {
                self.coords.screen_coordinates()
            }

            fn page_coordinates(&self) -> PagePoint {
                self.coords.page_coordinates()
            }
        }

        impl InteractionElementOffset for $ty {
            fn element_coordinates(&self) -> ElementPoint {
                self.coords.element_coordinates()
            }
        }

        impl ModifiersInteraction for $ty {
            fn modifiers(&self) -> Modifiers {
                self.coords.modifiers()
            }
        }

        impl PointerInteraction for $ty {
            fn trigger_button(&self) -> Option<MouseButton> {
                self.coords.trigger_button()
            }

            fn held_buttons(&self) -> MouseButtonSet {
                self.coords.held_buttons()
            }
        }
    };
}

delegate_coords!(QtMouseData);

/// PointerData 的负载。
///
/// Qt 给不出的字段按 web 的规矩补：鼠标 contact 尺寸报 1px、
/// 有键按住时 pressure 0.5（W3C Pointer Events 就是这么定义鼠标的）、
/// tangential/tilt/twist 只有笔才有意义，这里一律给中性值。
#[derive(Clone, Debug)]
struct QtPointerData {
    coords: QtCoords,
    pointer_id: i32,
    pointer_type: String,
    is_primary: bool,
    pressure: f32,
    width: f64,
    height: f64,
}

delegate_coords!(QtPointerData);

impl HasPointerData for QtPointerData {
    fn pointer_id(&self) -> i32 {
        self.pointer_id
    }

    fn width(&self) -> f64 {
        self.width
    }

    fn height(&self) -> f64 {
        self.height
    }

    fn pressure(&self) -> f32 {
        self.pressure
    }

    fn tangential_pressure(&self) -> f32 {
        -1.0
    }

    fn tilt_x(&self) -> i32 {
        0
    }

    fn tilt_y(&self) -> i32 {
        0
    }

    fn twist(&self) -> i32 {
        0
    }

    fn pointer_type(&self) -> String {
        self.pointer_type.clone()
    }

    fn is_primary(&self) -> bool {
        self.is_primary
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// WheelData 的负载。HasWheelData 的 supertrait 是 HasMouseData，
/// 所以这一支同样要坐标/按键那一整套。
#[derive(Clone, Debug)]
struct QtWheelData {
    coords: QtCoords,
    angle_x: i32,
    angle_y: i32,
    pixel_x: i32,
    pixel_y: i32,
}

delegate_coords!(QtWheelData);

impl HasMouseData for QtWheelData {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl HasWheelData for QtWheelData {
    fn delta(&self) -> WheelDelta {
        // 触控板给 pixelDelta（像素），滚轮给 angleDelta（一格 120）。
        // 符号两边相反：Qt 正 = 向上，web 正 = 向下，所以取负。
        if self.pixel_x != 0 || self.pixel_y != 0 {
            WheelDelta::pixels(
                -f64::from(self.pixel_x),
                -f64::from(self.pixel_y),
                0.0,
            )
        } else {
            WheelDelta::pixels(
                -f64::from(self.angle_x),
                -f64::from(self.angle_y),
                0.0,
            )
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// KeyboardData 的负载。`key`/`code` 的字符串在 C++ 那张表里就对好了
/// （qt_host.cpp 的 key_names()），这里只负责解析成 keyboard_types 的类型。
#[derive(Clone, Debug)]
struct QtKeyData {
    key: String,
    code: String,
    /// Qt 不区分同一键的左右位置（Key_Shift 就是一个键），只能一律 Standard。
    location: u32,
    repeat: bool,
    modifiers: u32,
}

impl ModifiersInteraction for QtKeyData {
    fn modifiers(&self) -> Modifiers {
        qt_modifiers(self.modifiers)
    }
}

impl HasKeyboardData for QtKeyData {
    fn key(&self) -> Key {
        // Key::from_str 认 web 那套名字（"enter"/"arrowleft"）和单个字符；
        // 认不出来的当字符原样交出去，空文本就是 Unidentified。
        if self.key.is_empty() {
            Key::Unidentified
        } else {
            Key::from_str(&self.key).unwrap_or_else(|_| Key::Character(self.key.clone().into()))
        }
    }

    fn code(&self) -> Code {
        Code::from_str(&self.code).unwrap_or(Code::Unidentified)
    }

    fn location(&self) -> Location {
        match self.location {
            1 => Location::Left,
            2 => Location::Right,
            3 => Location::Numpad,
            _ => Location::Standard,
        }
    }

    fn is_auto_repeating(&self) -> bool {
        self.repeat
    }

    fn is_composing(&self) -> bool {
        // 输入法合成态 Qt 要 QInputMethod 那套才给，本项目没接，如实报 false。
        false
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// FocusData 的负载 —— trait 只要 as_any()，没有别的字段要填。
#[derive(Clone, Debug)]
struct QtFocusData;

impl HasFocusData for QtFocusData {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// TouchData 里的一个点。坐标那一套和元素本地/窗口/屏幕对齐，
/// 但注意语义差别：触摸点的 local 是「相对这个元素」，
/// Qt 给的 position() 已经按 receiver 本地化过了。
#[derive(Clone, Debug)]
struct QtTouchPoint {
    id: i32,
    coords: QtCoords,
    force: f64,
}

impl InteractionLocation for QtTouchPoint {
    fn client_coordinates(&self) -> ClientPoint {
        self.coords.client_coordinates()
    }

    fn screen_coordinates(&self) -> ScreenPoint {
        self.coords.screen_coordinates()
    }

    fn page_coordinates(&self) -> PagePoint {
        self.coords.page_coordinates()
    }
}

impl HasTouchPointData for QtTouchPoint {
    fn identifier(&self) -> i32 {
        self.id
    }

    fn force(&self) -> f64 {
        self.force
    }

    fn radius(&self) -> ScreenPoint {
        // Qt 的 QEventPoint 不给接触半径（只有 ellipseSize 在私有接口里），
        // 按 web 的规矩给 1.0 而不是 0（0 在很多 handler 里等于「没有这个点」）。
        ScreenPoint::new(1.0, 1.0)
    }

    fn rotation(&self) -> f64 {
        0.0
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// TouchData 的负载。
///
/// web 那边 touches / targetTouches / changedTouches 三个列表的差别是
/// 「当前所有点」/「和这个元素同在的点」/「这一次变化的点」。
/// Qt 的 QTouchEvent::points() 给的是当前所有点，state 标出哪些在变，
/// 所以 changed 有真数据；target 那一层要「同元素」的判定，我们只有一个
/// item 的点集，就按「整份点集」交（本项目里一屏一个监听元素，等价）。
#[derive(Clone, Debug)]
struct QtTouchData {
    points: Vec<QtTouchPoint>,
    changed: Vec<usize>,
    modifiers: u32,
}

impl ModifiersInteraction for QtTouchData {
    fn modifiers(&self) -> Modifiers {
        qt_modifiers(self.modifiers)
    }
}

impl HasTouchData for QtTouchData {
    fn touches(&self) -> Vec<TouchPoint> {
        self.points.iter().map(point_of).collect()
    }

    fn touches_changed(&self) -> Vec<TouchPoint> {
        self.changed
            .iter()
            .filter_map(|i| self.points.get(*i))
            .map(point_of)
            .collect()
    }

    fn target_touches(&self) -> Vec<TouchPoint> {
        self.touches()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

fn point_of(p: &QtTouchPoint) -> TouchPoint {
    TouchPoint::new(p.clone())
}

/// ResizeData 的负载。0.7.10 这个类型除了 as_any() 之外全是默认
/// 「不支持」，能填的两个尺寸我们正好知道 —— 几何是我们自己写的。
#[derive(Clone, Debug)]
struct QtResizeData {
    width: f64,
    height: f64,
}

impl HasResizeData for QtResizeData {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn get_border_box_size(&self) -> ResizeResult<PixelsSize> {
        Ok(PixelsSize::new(self.width, self.height))
    }

    fn get_content_box_size(&self) -> ResizeResult<PixelsSize> {
        // 我们没有 padding/border 那套盒模型，内容盒 = 边框盒。
        Ok(PixelsSize::new(self.width, self.height))
    }
}

/// MountedData 的负载：trait 只强制 as_any()，
/// 其余（get_client_rect / scroll_to 那一串）默认 NotSupported。
/// 想要真的实现就得把 QQuickItem 指针塞进来并跨 await 保活，那是另一件事。
/// 元素 id 不在负载里（dioxus 那边事件对象本来就知道自己派给了谁），
/// 只在日志里报一声。
#[derive(Clone, Debug)]
struct QtMountedData;

impl RenderedElementBacking for QtMountedData {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// 剩下那些事件：统一生成「不许走」的实现。
macro_rules! unhandled_events {
    ($($method:ident => $data:ty),* $(,)?) => {
        $(
            fn $method(
                &self,
                _event: &PlatformEventData,
            ) -> $data {
                panic!(
                    "本项目不产生这类事件，{} 没有实现",
                    stringify!($method)
                )
            }
        )*
    };
}

/// 平台事件 → dioxus-html 通用事件的转换器。
struct QtEventConverter;

impl HtmlEventConverter for QtEventConverter {
    fn convert_mouse_data(
        &self,
        event: &PlatformEventData,
    ) -> MouseData {
        MouseData::new(
            downcast::<QtMouseData>(event),
        )
    }

    fn convert_pointer_data(
        &self,
        event: &PlatformEventData,
    ) -> PointerData {
        PointerData::new(
            downcast::<QtPointerData>(event),
        )
    }

    fn convert_wheel_data(
        &self,
        event: &PlatformEventData,
    ) -> WheelData {
        WheelData::new(
            downcast::<QtWheelData>(event),
        )
    }

    fn convert_keyboard_data(
        &self,
        event: &PlatformEventData,
    ) -> KeyboardData {
        KeyboardData::new(
            downcast::<QtKeyData>(event),
        )
    }

    fn convert_focus_data(
        &self,
        event: &PlatformEventData,
    ) -> FocusData {
        FocusData::new(
            downcast::<QtFocusData>(event),
        )
    }

    fn convert_touch_data(
        &self,
        event: &PlatformEventData,
    ) -> TouchData {
        TouchData::new(
            downcast::<QtTouchData>(event),
        )
    }

    fn convert_resize_data(
        &self,
        event: &PlatformEventData,
    ) -> ResizeData {
        ResizeData::new(
            downcast::<QtResizeData>(event),
        )
    }

    fn convert_mounted_data(
        &self,
        event: &PlatformEventData,
    ) -> MountedData {
        MountedData::new(
            downcast::<QtMountedData>(event),
        )
    }

    unhandled_events! {
        convert_animation_data => AnimationData,
        convert_cancel_data => CancelData,
        convert_clipboard_data => ClipboardData,
        convert_composition_data => CompositionData,
        convert_drag_data => DragData,
        convert_form_data => FormData,
        convert_image_data => ImageData,
        convert_media_data => MediaData,
        convert_scroll_data => ScrollData,
        convert_selection_data => SelectionData,
        convert_toggle_data => ToggleData,
        convert_transition_data => TransitionData,
        convert_visible_data => VisibleData,
    }
}

/// downcast 的目标类型就是我们自己塞进 PlatformEventData 的那个，
/// 两边是同一份代码在管，所以失败就是真出错，直接 panic。
fn downcast<T: Clone + 'static>(
    event: &PlatformEventData,
) -> T {
    event
        .downcast::<T>()
        .expect("PlatformEventData 里装的类型和这一支不匹配")
        .clone()
}

/// 把事件送进 runtime。
///
/// 每一支 Qt 回调最后都汇到这里：RuntimeGuard（不带就 panic
/// 「Must be called from inside a Dioxus runtime」，runtime.rs:100）、
/// 冒泡布尔查 event_bubbles、然后 handle_event。
///
/// 顺带：handle_event 里 listener 写 signal → needs_update → wake，
/// 而 wake 只能排队（schedule_pump_poll），不能当场 poll ——
/// 这条线上 PUMP 的 borrow_mut 还在我们手上。
fn dispatch_event(
    name: &str,
    element: u32,
    data: impl std::any::Any + 'static,
    detail: String,
) {
    PUMP.with(|slot| {
        let mut guard = slot.borrow_mut();

        let Some(pump) = guard.as_mut() else {
            println!(
                "[event] {name} 来了，但回路还没装好，丢掉"
            );

            return;
        };

        let runtime = pump.vdom.runtime();

        let _guard = RuntimeGuard::new(
            runtime.clone(),
        );

        let payload: std::rc::Rc<dyn std::any::Any> = std::rc::Rc::new(
            PlatformEventData::new(std::boxed::Box::new(data)),
        );

        let bubbles = event_bubbles(name);

        println!(
            "[event] handle_event({name:?}, ElementId({element})) \
             冒泡 {bubbles}{detail}"
        );

        runtime.handle_event(
            name,
            Event::new(payload, bubbles),
            ElementId(element as usize),
        );
    });
}

// ============================================================
// C++ 那一侧进来的八支回调（extern "Rust"）
// ============================================================

/// 坐标三套 + 按键 + 修饰键，每支回调的日志里都拼这一串。
#[allow(clippy::too_many_arguments)]
fn coords_log(
    local_x: f64,
    local_y: f64,
    window_x: f64,
    window_y: f64,
    screen_x: f64,
    screen_y: f64,
) -> String {
    format!(
        " 本地 ({local_x:.1}, {local_y:.1}) 窗口 ({window_x:.1}, {window_y:.1}) \
         屏幕 ({screen_x:.1}, {screen_y:.1})"
    )
}

#[allow(clippy::too_many_arguments)]
fn qt_mouse_event(
    name: &str,
    element: u32,
    local_x: f64,
    local_y: f64,
    window_x: f64,
    window_y: f64,
    screen_x: f64,
    screen_y: f64,
    button: i32,
    buttons: i32,
    modifiers: i32,
) {
    dispatch_event(
        name,
        element,
        QtMouseData {
            coords: QtCoords {
                local_x,
                local_y,
                window_x,
                window_y,
                screen_x,
                screen_y,
                modifiers: modifiers as u32,
                button,
                buttons,
            },
        },
        format!(
            "{} button 0x{button:x}",
            coords_log(local_x, local_y, window_x, window_y, screen_x, screen_y)
        ),
    );
}

#[allow(clippy::too_many_arguments)]
fn qt_pointer_event(
    name: &str,
    element: u32,
    local_x: f64,
    local_y: f64,
    window_x: f64,
    window_y: f64,
    screen_x: f64,
    screen_y: f64,
    button: i32,
    buttons: i32,
    modifiers: i32,
    pointer_id: i32,
    pointer_type: &str,
    is_primary: bool,
    pressure: f64,
    width: f64,
    height: f64,
) {
    dispatch_event(
        name,
        element,
        QtPointerData {
            coords: QtCoords {
                local_x,
                local_y,
                window_x,
                window_y,
                screen_x,
                screen_y,
                modifiers: modifiers as u32,
                button,
                buttons,
            },
            pointer_id,
            pointer_type: pointer_type.to_string(),
            is_primary,
            pressure: pressure as f32,
            width,
            height,
        },
        format!(
            " pointer {pointer_id}/{pointer_type} 压力 {pressure:.2}"
        ),
    );
}

#[allow(clippy::too_many_arguments)]
fn qt_wheel_event(
    name: &str,
    element: u32,
    local_x: f64,
    local_y: f64,
    window_x: f64,
    window_y: f64,
    screen_x: f64,
    screen_y: f64,
    angle_x: i32,
    angle_y: i32,
    pixel_x: i32,
    pixel_y: i32,
    buttons: i32,
    modifiers: i32,
) {
    dispatch_event(
        name,
        element,
        QtWheelData {
            coords: QtCoords {
                local_x,
                local_y,
                window_x,
                window_y,
                screen_x,
                screen_y,
                modifiers: modifiers as u32,
                button: 0,
                buttons,
            },
            angle_x,
            angle_y,
            pixel_x,
            pixel_y,
        },
        format!(
            " 滚量 angle ({angle_x}, {angle_y}) pixel ({pixel_x}, {pixel_y})"
        ),
    );
}

fn qt_key_event(
    name: &str,
    element: u32,
    key: &str,
    code: &str,
    location: i32,
    repeat: bool,
    modifiers: i32,
) {
    dispatch_event(
        name,
        element,
        QtKeyData {
            key: key.to_string(),
            code: code.to_string(),
            location: location as u32,
            repeat,
            modifiers: modifiers as u32,
        },
        format!(" 键 {key:?} code {code:?}"),
    );
}

fn qt_focus_event(
    name: &str,
    element: u32,
) {
    dispatch_event(
        name,
        element,
        QtFocusData,
        String::new(),
    );
}

/// points 是步长 6 的扁平数组：local x/y、window x/y、screen x/y。
fn qt_touch_event(
    name: &str,
    element: u32,
    ids: Vec<i32>,
    points: Vec<f64>,
    forces: Vec<f64>,
    changed: Vec<i32>,
    modifiers: i32,
) {
    let mut list = Vec::with_capacity(ids.len());

    for (i, id) in ids.iter().enumerate() {
        let base = i * 6;

        let get = |k: usize| points.get(base + k).copied().unwrap_or(0.0);

        list.push(QtTouchPoint {
            id: *id,
            coords: QtCoords {
                local_x: get(0),
                local_y: get(1),
                window_x: get(2),
                window_y: get(3),
                screen_x: get(4),
                screen_y: get(5),
                modifiers: modifiers as u32,
                button: 0,
                buttons: 0,
            },
            force: forces.get(i).copied().unwrap_or(0.0),
        });
    }

    let changed: Vec<usize> = changed
        .iter()
        .filter_map(|i| usize::try_from(*i).ok())
        .collect();

    let count = list.len();

    dispatch_event(
        name,
        element,
        QtTouchData {
            points: list,
            changed,
            modifiers: modifiers as u32,
        },
        format!(" 点数 {count}"),
    );
}

fn qt_resize_event(
    name: &str,
    element: u32,
    width: f64,
    height: f64,
) {
    dispatch_event(
        name,
        element,
        QtResizeData { width, height },
        format!(" 尺寸 {width:.1}x{height:.1}"),
    );
}

// ============================================================
// 名字表：哪一支支持、不支持的理由
// ============================================================
//
// C++ 的 EVENT_SPECS 是同一份口径的另一半（那边管「吃什么 QEvent、
// 送回来哪一支」，这里管「要不要装、不装的话为什么」）。
// 加一支事件要两边各改一处 —— 漏了 C++ 那一侧的话，
// add_event_listener 会打「事件名不在表里」并且不装任何东西。

/// 本项目不支持的事件名 → 缺的是什么东西。
///
/// 返回 None = 支持（交给 C++ 装 filter）；Some(理由) = 拒绝并打日志。
/// 兜底那条给的是「没有事实来源」，比让人以为漏了个分支好。
fn unsupported_reason(
    name: &str,
) -> Option<&'static str> {
    let reason = match name {
        // Qt 有这套事实，但本项目还没有对应的设施。
        "scroll" | "scrollend" =>
            "没有可滚动容器（Flickable/ScrollView），滚不动也就没有滚动量",
        "drag" | "dragstart" | "dragend" | "dragenter" | "dragleave"
        | "dragover" | "drop" =>
            "要一套 Qt 拖放（QDrag + QMimeData），本项目没有拖放源也没有放置目标",

        // Qt/web 语义对不上，硬接就是假数据。
        "animationstart" | "animationiteration" | "animationend"
        | "transitionend" =>
            "属性动画不在我们的渲染路径上（几何是 taffy 一次性写死的）",
        "load" | "error" =>
            "没有需要异步加载的子资源（图片/媒体都不在这个项目里）",
        "abort" | "canplay" | "canplaythrough" | "durationchange"
        | "emptied" | "encrypted" | "ended" | "loadeddata"
        | "loadedmetadata" | "loadstart" | "pause" | "play"
        | "playing" | "progress" | "ratechange" | "seeked"
        | "seeking" | "stalled" | "suspend" | "timeupdate"
        | "volumechange" | "waiting" =>
            "没有媒体元素",
        "change" | "input" | "invalid" | "reset" | "submit" =>
            "没有表单元素（值都在 Rust 侧的 signal 里）",
        "copy" | "cut" | "paste" =>
            "没接剪贴板 QClipboard",
        "compositionstart" | "compositionend" | "compositionupdate" =>
            "没接输入法 QInputMethod（is_composing 因此也只能报 false）",
        "select" | "selectstart" | "selectionchange" =>
            "没接文本选择（QQuickText 的选区是另一整套）",
        "toggle" | "beforetoggle" =>
            "Qt 没有 details/summary 这类会自己折叠的元素",
        "visible" =>
            "没接可见性观察：QQuickItem::isVisible 只说「在树上」，不说「在视口里」",
        "cancel" =>
            "没有可取消的异步元素（那是 closable dialog 那一类）",
        "pointerlockchange" | "pointerlockerror" =>
            "没接指针捕获 QQuickWindow::setPointerCaptureBehaviours",

        _ => "本项目没有这一类事实来源",
    };

    // 支持的都在这张表里；不在的就当成「上面那一段没列到的名字」也拒绝。
    if SUPPORTED_EVENTS.contains(&name) || name == "mounted" {
        return None;
    }

    Some(reason)
}


/// 支持的支（和 cpp/qt_host.cpp 的 EVENT_SPECS 一一对应）。
const SUPPORTED_EVENTS: &[&str] = &[
    "mousedown", "mouseup", "click", "dblclick", "doubleclick",
    "mousemove", "mouseover", "mouseout", "mouseenter", "mouseleave",
    "contextmenu",
    "pointerdown", "pointerup", "auxclick", "pointermove",
    "pointerover", "pointerout", "pointerenter", "pointerleave",
    "wheel",
    "keydown", "keyup", "keypress",
    "focus", "blur", "focusin", "focusout",
    "touchstart", "touchmove", "touchend", "touchcancel",
    "resize",
];

// ============================================================
// mounted：本轮渲染结束后补发
// ============================================================
//
// 队列放在 PUMP 之外的一个 thread_local 里，为的是能躲开重入：
// create_event_listener 是在 render_immediate 里面被调的，
// 那一刻 PUMP 的 borrow_mut 还握着，而派发要再借一次 → 当场 panic。
// 所以「记下来」放这里，回路在 borrow 之外掏空它。
thread_local! {
    static PENDING_MOUNTED: std::cell::RefCell<Vec<u32>> =
        std::cell::RefCell::new(Vec::new());
}


fn queue_mounted(
    element: u32,
) {
    PENDING_MOUNTED.with(|slot| {
        slot.borrow_mut().push(element);
    });
}


/// 把攒下的 mounted 发出去。回路在每轮 poll 结束后调一次。
fn drain_pending_mounted() {
    let pending: Vec<u32> = PENDING_MOUNTED.with(|slot| {
        let mut borrowed = slot.borrow_mut();

        std::mem::take(&mut *borrowed)
    });

    for element in pending {
        dispatch_event(
            "mounted",
            element,
            QtMountedData,
            format!(" 元素 {element} 挂上了"),
        );
    }
}

/// 把转换器装进那个全局槽。必须在任何事件进来之前调一次。
///
/// 这是个进程级的 `static RwLock<Option<Box<dyn HtmlEventConverter>>>`
/// （events/mod.rs:68），装一次就够了，不用每个窗口装一次。
fn install_event_converter() {
    set_event_converter(
        std::boxed::Box::new(QtEventConverter),
    );

    println!(
        "[event] HtmlEventConverter 装好了（8 支是真的：mouse/pointer/wheel/\
         key/focus/touch/resize/mounted，其余 13 支一被调到就 panic）"
    );
}


// ============================================================
// Main
// ============================================================
//
// 现在这个 main 只做两轮：第 4 步 mount，然后把 vdom + renderer 交给回路
// （第 5 步 install_pump），剩下的全在 Qt 的事件循环里发生。
// 从这里开始，屏幕上的每一次变化都是你点出来的 —— main 里没有任何
// 一行还会主动改状态。
//
// 顺便说明为什么 Qt 和 Dioxus 谁当主循环：
// 这里让 Qt 当（app.exec()），因为 QQuickWindow 的渲染、
// 输入、vsync 全在 Qt 的事件循环里；
// Dioxus 那边只需要被「叫醒」一次就能跑完一轮 diff，
// 所以正确的形状是：Qt 循环里找机会 poll Dioxus。

fn main() {
    // --------------------------------------------------------
    // 1. Qt application
    // --------------------------------------------------------
    //
    // 必须是第一件事：Qt 不允许在没有 QGuiApplication 的时候
    // 构造任何 QWindow / QQmlEngine。
    //
    // cxx-qt-lib 那边返回的是 UniquePtr（qguiapplication.rs:186），
    // 所以最后 exec 时是 `qt_app.as_mut()` 判空；
    // C++ 侧其实有 Q_ASSERT(ptr != nullptr)（qguiapplication.cpp:26），
    // 也就是说真起不来（没显示环境 / 平台插件缺失）时是当场 assert，
    // 而不是安静地给你一个 None。
    //
    // 顺带一个顺序上的坑：窗口是在第 2 步 QtRenderer::new() 里建的，
    // 而这里直到第 5 步才判空。也就是说如果 application 没建起来，
    // 程序在构造 QQuickWindow 那里就先出事了，
    // 这个判空实际上救不了什么。

    let mut qt_app =
        QGuiApplication::new();


    // --------------------------------------------------------
    // 2. 我们自己的 renderer
    // --------------------------------------------------------
    //
    // QtRenderer::new() 里已经 show() 了窗口，
    // 所以后面的建节点过程是能肉眼看到的。

    let mut renderer =
        QtRenderer::new();


    // --------------------------------------------------------
    // 3. Dioxus Core
    //
    // 没有：
    //
    // dioxus::launch()
    // dioxus-desktop
    // webview
    // --------------------------------------------------------
    //
    // 注意 new() 这一步并没有跑 app()：
    // 库那边写得很明白 —— "the VirtualDom is not progressed on creation"
    // （virtual_dom.rs:271），这里只是把 root scope 登记好。
    //
    // app() 第一次真正被调用是在下面 rebuild 里的
    // run_scope(ScopeId::ROOT)（virtual_dom.rs:584），
    // 所以此刻屏幕上什么都没有，Qt 那边也一个对象都还没建。

    let mut vdom =
        VirtualDom::new(app);


    // --------------------------------------------------------
    // 4. Dioxus 执行 app()
    //
    // 这一句把 app() 跑一遍，产出的 mutation 全进 QtRenderer：
    // 先 LoadTemplate 建出根容器，再给那三个按钮 AssignId + SetAttribute
    // + NewEventListener（顺序就是模板里写下的顺序），
    // 最后 rebuild 硬编码的那句 append_children(ElementId(0), m) 把整棵挂上窗口根
    // （virtual_dom.rs:579）。
    //
    // 想看不用跑窗口：`cargo test` 里 first_render_is_zero_with_three_buttons
    // 用 dioxus-core 自带的 Mutations 把同一股指令流原样录了下来，
    // 不需要 Qt 在场。
    // --------------------------------------------------------

    vdom.rebuild(
        &mut renderer
    );


    // --------------------------------------------------------
    // 4.5 布局：属性 + 文本测量 → taffy → Qt
    //
    // 这就是「批量边界」：mount 的全部 mutation 都跑完了，
    // 每个节点的 width / height / gap 记完账、文本内容也记完账了，
    // 才算一次。文本节点没有显式尺寸，尺寸是这一步里问 Qt 量的。
    //
    // 没算之前所有节点尺寸都是 0，屏幕上只有窗口背景色 ——
    // 想验证几何确实只有一个作者，就把下面这行注释掉再跑。
    // --------------------------------------------------------

    renderer.relayout();

    // 布局落完先打一次树：现在纸条只能由真鼠标产生，
    // 首屏这份 dump 就是「不点一下也能看到的」child_order 交叉检查。
    // 之后每一屏由 qt_pump_poll 里那句 dump_tree 负责。
    renderer.dump_tree();


    // --------------------------------------------------------
    // 4.6 文本测量对照（临时诊断，不是布局的一环）
    //
    // 目的：核对「我们量的」和「Qt 渲染时认为需要的」是同一个数，
    // 顺便把布局最后写给的尺寸打出来。
    // --------------------------------------------------------

    renderer.verify_text_measurement();



    // --------------------------------------------------------
    // 5. 回路装上，然后交给 Qt 的主循环
    // --------------------------------------------------------
    //
    // 原来这段是「第 4.7 步：exec 之前用 for 循环手动把 diff 推完」
    // （store → mark_all_dirty → render_immediate → relayout → dump_tree），
    // 再后来是 QTimer 每 200ms 推一格 —— 那些现在都没有了。剩下的形状是：
    //
    //     窗口里的输入事件 → InputFilter → 对应那支 qt_*_event
    //     → runtime().handle_event(...)（runtime.rs:369）
    //     → listener 写 signal → 纸条 → 我们的 waker
    //     → C++ schedule_pump_poll() → 排一个 Qt 事件
    //     → qt_pump_poll：poll wait_for_work → Ready → 渲染 + 布局
    //
    // main 到这里就把 vdom / renderer 整个交出去，运行期间再也不碰它们 ——
    // 也就是说 exec() 之后如果没有人点按钮，日志里不会再出现一行渲染。
    // 唯一还会回头碰一次的地方是 exec 返回之后的 dismantle_pump()。

    // 转换器必须比第一次点击更早装好：
    // onclick 的 listener 里那步 `From<&PlatformEventData> for MouseData`
    // 会 unwrap 一个全局 Option，没装就是一声 panic
    // （实测：dioxus-html/src/events/mod.rs:81 unwrap() on None）。
    install_event_converter();

    install_pump(
        vdom,
        renderer,
    );

    println!(
        "Qt event loop started. \
         Close the window to exit."
    );


    if let Some(app) =
        qt_app.as_mut()
    {
        app.exec();
    }

    // exec 回来 = 窗口关了（或者 quit 了）。
    // 回路必须在这里当场拆掉，不能留给线程退出时的 TLS 析构 —— 见 dismantle_pump。
    dismantle_pump();
}


// ============================================================
// 不碰 Qt 的那半段：属性 → 布局输入
// ============================================================
//
// relayout() 分三步：属性 → taffy 输入、算（叶子回头问 Qt 量）、
// 把几何推给 Qt。第一步是纯 Rust 的，所以能在这里断言，不需要窗口。
// 第二步的测量要 QGuiApplication 在场，测不了 ——
// 它的断言在运行日志里（verify_text_measurement 打的 Δ 和「布局用到」）。
//
// 每个测试都带反向断言：光「解析成功」不够，
// 还得证明非法输入不会被当成 0，以及 text / color 不该被吞掉。

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn parse_length_accepts_plain_numbers() {
        assert_eq!(parse_length("460"), Some(460.0));
        assert_eq!(parse_length("12.5"), Some(12.5));
        assert_eq!(parse_length(" 60 "), Some(60.0));
    }


    #[test]
    fn parse_length_rejects_anything_else() {
        // 关键是 None，而不是 0.0 —— 当成 0 的话屏幕上就是一个
        // 看不见又占位的盒子，比没有更难查。
        assert_eq!(parse_length(""), None);
        assert_eq!(parse_length("abc"), None);
        assert_eq!(parse_length("50%"), None);

        // 带单位的写法一律不当数：本项目没有 px/em 的换算规则，
        // 「12px」被当成 12 和当成没写，前者更难查。
        assert_eq!(parse_length("12px"), None);
    }


    #[test]
    fn parse_size_accepts_length_and_percent() {
        assert_eq!(parse_size("460"), Some(Dimension::length(460.0)));
        assert_eq!(parse_size(" 60 "), Some(Dimension::length(60.0)));

        // 百分比是伸缩那条链的前提：根节点 100% 才跟着窗口走。
        // 注意存进去的是 1.0 而不是 100.0 —— taffy 的 percent 收 [0.0, 1.0]
        // （dimension.rs:398 那句 NOTE），写成 100.0 就是 100 倍父节点，
        // 这条断言就是为了钉住这个坑。
        assert_eq!(parse_size("100%"), Some(Dimension::percent(1.0)));
        assert_eq!(parse_size(" 50 %"), Some(Dimension::percent(0.5)));
        assert_eq!(parse_size("0%"), Some(Dimension::percent(0.0)));
    }


    #[test]
    fn parse_size_rejects_anything_else() {
        assert_eq!(parse_size(""), None);
        assert_eq!(parse_size("abc"), None);
        assert_eq!(parse_size("50vw"), None);

        // 反向：% 后面没有数，不能当成 100%。
        assert_eq!(parse_size("%"), None);
    }


    #[test]
    fn only_layout_props_are_kept_from_qt() {
        assert!(is_layout_prop("width"));
        assert!(is_layout_prop("height"));
        assert!(is_layout_prop("gap"));
        assert!(is_layout_prop("flex-direction"));
        assert!(is_layout_prop("flex_direction"));
        assert!(is_layout_prop("flex-grow"));
        assert!(is_layout_prop("flex_grow"));

        // 反向：这些必须照常写给 Qt，不然文字和颜色没人画
        assert!(!is_layout_prop("text"));
        assert!(!is_layout_prop("color"));

        // 反向：短横写法认了，拼错的不能认（认了就会从 Qt 那边被吞掉，
        // 又没人画它）
        assert!(!is_layout_prop("flexdirection"));
        assert!(!is_layout_prop("grow"));
    }


    #[test]
    fn style_for_reads_every_supported_prop() {
        // 每种值形态都在这里有至少一次出场：宽度是百分比字符串、
        // 高度是 Int、gap 是 Float —— 布局不该在乎它们从哪个变体来。
        let props = vec![
            ("width", PropValue::Text("100%".to_string())),
            ("height", PropValue::Int(180)),
            ("flex_direction", PropValue::Text("column".to_string())),
            ("gap", PropValue::Float(12.0)),
            ("flex_grow", PropValue::Int(1)),
        ];

        let style = style_for(&props);

        assert_eq!(style.size.width, Dimension::percent(1.0));
        assert_eq!(style.size.height, Dimension::length(180.0));
        assert_eq!(style.flex_direction, FlexDirection::Column);
        assert_eq!(style.gap.width, length(12.0));
        assert_eq!(style.flex_grow, 1.0);
    }


    #[test]
    fn typed_numbers_are_lengths_never_percentages() {
        // `width: 100`（Int）= 100 个像素。
        // 这条断言钉的是「别顺手给数字也加百分比语义」：
        // 写成 percent(100.0) 或者 percent(1.0) 都会在这里红。
        assert_eq!(
            dimension_of(&PropValue::Int(100)),
            Some(Dimension::length(100.0))
        );
        assert_eq!(
            dimension_of(&PropValue::Float(12.5)),
            Some(Dimension::length(12.5))
        );

        // 百分比只有字符串写得出来。
        assert_eq!(
            dimension_of(&PropValue::Text("100%".to_string())),
            Some(Dimension::percent(1.0))
        );

        // 布尔不是长度也不是方向。
        assert_eq!(length_of(&PropValue::Bool(true)), None);
        assert_eq!(dimension_of(&PropValue::Bool(false)), None);
        assert_eq!(direction_of(&PropValue::Bool(true)), None);
    }


    #[test]
    fn direction_only_accepts_the_two_spellings() {
        assert_eq!(
            direction_of(&PropValue::Text("row".to_string())),
            Some(FlexDirection::Row)
        );
        assert_eq!(
            direction_of(&PropValue::Text("column".to_string())),
            Some(FlexDirection::Column)
        );

        // 看不懂的就当没写过：不能把「不是 column」直接当成 row。
        assert_eq!(
            direction_of(&PropValue::Text("colum".to_string())),
            None
        );
        assert_eq!(
            direction_of(&PropValue::Text("".to_string())),
            None
        );
    }


    #[test]
    fn classify_attr_covers_every_variant() {
        // AttributeValue 一共七个变体（nodes.rs:749-770），
        // 四个能表达、一个是「撤掉」、两个必须出声拒绝。
        assert_eq!(
            classify_attr(&AttributeValue::Text(
                "hi".to_string()
            )),
            AttrInput::Write(PropValue::Text("hi".to_string()))
        );
        assert_eq!(
            classify_attr(&AttributeValue::Int(-7)),
            AttrInput::Write(PropValue::Int(-7))
        );
        assert_eq!(
            classify_attr(&AttributeValue::Float(0.5)),
            AttrInput::Write(PropValue::Float(0.5))
        );
        assert_eq!(
            classify_attr(&AttributeValue::Bool(true)),
            AttrInput::Write(PropValue::Bool(true))
        );
        assert_eq!(
            classify_attr(&AttributeValue::None),
            AttrInput::Clear
        );

        // 反向：Any 翻译不出来 —— 这条必须拒绝，不能凭空造一个 Qt 值。
        assert!(matches!(
            classify_attr(&AttributeValue::any_value(42_i32)),
            AttrInput::Refused(_)
        ));

        // Listener 那支在这里没法构造：`AttributeValue::listener` 要拿当前
        // scope 的 owner（ListenerCallback::new → runtime.rs:222 那句 unwrap），
        // 而进 scope 的 push_scope 是私有的（runtime.rs:271）。
        // 反过来这也说明它到不了 set_attribute ——
        // write_attribute 早在 diff/node.rs:525-538 就把它换成
        // CreateEventListener 了，classify_attr 里那一支纯属门口拦一手。
    }


    #[test]
    fn prop_ledger_keeps_one_entry_per_name() {
        let mut entries: Vec<(&'static str, PropValue)> = Vec::new();

        set_prop_entry(
            &mut entries,
            "width",
            PropValue::Int(110),
        );
        set_prop_entry(
            &mut entries,
            "width",
            PropValue::Text("100%".to_string()),
        );
        set_prop_entry(
            &mut entries,
            "gap",
            PropValue::Int(12),
        );

        // 一名一条：第二次是覆盖，不是往尾巴上追加。
        assert_eq!(entries.len(), 2);
        assert_eq!(style_for(&entries).size.width, Dimension::percent(1.0));
    }


    #[test]
    fn clearing_a_layout_prop_returns_it_to_auto() {
        // 这就是 AttributeValue::None 在布局那一侧的落点。
        // 关键是「回到 auto」而不是「回到 0」：
        // 0 会在屏幕上留下一个看不见又占位的东西。
        let mut entries: Vec<(&'static str, PropValue)> = Vec::new();

        set_prop_entry(
            &mut entries,
            "width",
            PropValue::Int(300),
        );
        set_prop_entry(
            &mut entries,
            "height",
            PropValue::Int(28),
        );

        assert_eq!(
            style_for(&entries).size.width,
            Dimension::length(300.0)
        );

        assert!(clear_prop_entry(&mut entries, "width"));
        assert!(!clear_prop_entry(&mut entries, "width"));

        let style = style_for(&entries);

        assert_eq!(style.size.width, Dimension::auto());

        // 反向：摘一个不能顺带把别的也弄没。
        assert_eq!(style.size.height, Dimension::length(28.0));
        assert_eq!(entries.len(), 1);
    }


    #[test]
    fn style_for_treats_unparsable_value_as_absent() {
        // 每个支持的属性都配一条反向断言：解析不出来时必须「当没写过」，
        // 而不是写进 0 / 0% / 默认方向。
        let props = vec![
            ("width", PropValue::Text("12px".to_string())),
            ("height", PropValue::Text("abc".to_string())),
            ("gap", PropValue::Text("12px".to_string())),
            ("flex_grow", PropValue::Text("auto".to_string())),
        ];

        let style = style_for(&props);

        assert_eq!(style.size.width, Dimension::auto());
        assert_eq!(style.size.height, Dimension::auto());
        assert_eq!(style.gap.width, length(0.0));
        assert_eq!(style.flex_grow, 0.0);

        // 字符串空值也一样："" 不是 0。
        let empty =
            vec![("width", PropValue::Text(String::new()))];
        assert_eq!(
            style_for(&empty).size.width,
            Dimension::auto()
        );

        // 方向也一样：认不出的值不该把默认的 row 翻成 column。
        let only_direction =
            vec![("flex_direction", PropValue::Text("colum".to_string()))];
        assert_eq!(
            style_for(&only_direction).flex_direction,
            FlexDirection::Row
        );
    }


    #[test]
    fn numeric_literals_arrive_as_typed_attributes() {
        // rsx 里不带引号的 110 / 48 / 12，交过来是 AttributeValue::Int；
        // 0.45 是 Float，`shown % 2 == 1` 是 Bool。
        // 这一条断言的就是「四种形态真的到得了 renderer」——
        // 前一版它们全被 `let AttributeValue::Text` 那一步吞了。
        let mut vdom = VirtualDom::new(app);

        let mut out = Mutations::default();

        vdom.rebuild(&mut out);

        let writes = attr_writes(&out.edits);

        for expect in [
            ("width", "Int(110)"),
            ("height", "Int(48)"),
            ("radius", "Int(12)"),
            ("opacity", "Float(0.45)"),
            ("visible", "Bool(false)"),
        ] {
            assert!(
                writes.contains(
                    &(expect.0.to_string(), expect.1.to_string())
                ),
                "少了这一条 {:?}，实际写出的属性 = {writes:?}",
                expect
            );
        }

        // 反向：Int 110 不能顺带被当成「110%」。
        assert!(
            !writes.iter().any(
                |(_, value)| value.contains('%')
            ),
            "属性值里不该出现百分比，实际 = {writes:?}"
        );
    }


    #[test]
    fn clearing_an_attribute_reaches_the_renderer() {
        // 减一那块的颜色写成 Option：奇数次是 Some(...)，偶数次是 None。
        // 所以「点一下让它没了」在指令流里必须是一条能指认的
        // SetAttribute(color, AttributeValue::None) ——
        // 这就是 C++ 那边 clear_property 的唯一触发源。
        let mut vdom = VirtualDom::new(app);

        let mut out = Mutations::default();

        vdom.rebuild(&mut out);

        let buttons = click_listeners(&out.edits);

        assert_eq!(
            buttons.len(),
            3,
            "第一屏只有三个按钮的监听器（条目要点了才长出来）"
        );

        // 0 是偶数：首屏就该已经有一条 None（我们从来没写过它，
        // 这一条在 renderer 里落成「没什么可摘」）。
        assert!(
            attr_writes(&out.edits)
                .contains(&("color".to_string(), "None".to_string())),
            "count = 0 时 color 应该整个不存在"
        );

        // 加到 1（奇数）→ color 有了、opacity 回到 1.0、指示条显形。
        send_click(&vdom, buttons[1]);

        let at_one = render(&mut vdom);
        let writes = attr_writes(&at_one);

        assert!(
            writes.contains(
                &("color".to_string(), "Text(\"#d9825f\")".to_string())
            ),
            "点到 1 该把颜色写回来，实际 = {writes:?}"
        );
        assert!(
            writes.contains(
                &("opacity".to_string(), "Float(1.0)".to_string())
            ),
            "点到 1 那块该不透明了，实际 = {writes:?}"
        );
        assert!(
            writes.contains(
                &("visible".to_string(), "Bool(true)".to_string())
            ),
            "奇数次指示条该显形，实际 = {writes:?}"
        );

        // 减回 0（偶数）→ 这三条各自被**撤掉 / 改回去**。
        send_click(&vdom, buttons[0]);

        let back_to_zero = render(&mut vdom);
        let writes = attr_writes(&back_to_zero);

        assert!(
            writes.contains(
                &("color".to_string(), "None".to_string())
            ),
            "回到 0，color 这一条要被撤掉，实际 = {writes:?}"
        );
        assert!(
            writes.contains(
                &("opacity".to_string(), "Float(0.45)".to_string())
            ),
            "回到 0，opacity 要改回 0.45，实际 = {writes:?}"
        );

        // 反向：撤属性是 SetAttribute(value=None)，不是别的什么指令。
        // dioxus 没有 RemoveAttribute 这一支（remove_attribute 也是
        // 走 set_attribute，diff/node.rs:500-512），
        // 所以「值变成 None」就是唯一的信号。
        assert!(
            !kinds(&back_to_zero)
                .iter()
                .any(|kind| kind.contains("RemoveAttribute")),
            "要是哪天出现专门的撤属性指令，这条会红"
        );
    }


    // ============================================================
    // counter 的那半段：事件 → signal → diff，全程不碰 Qt
    // ============================================================
    //
    // 假事件源（QTimer + DIFF_SCRIPT + send_click_to）删掉之后，
    // 「点一下会发生什么」在 offscreen 日志里就没得看了 ——
    // 于是那段验证搬到这里。送进来的事件和窗口里点出来的是同一个形状：
    // `Event::new(Rc<PlatformEventData>, true)` + `handle_event("click", .., id)`，
    // 少的只有最外面那一环（ClickFilter 没被穿过），从 runtime 往里全是真的。
    //
    // 这里用 dioxus-core 自带的 `Mutations`（一个 Vec<Mutation>）当 renderer：
    // cargo test 环境里没有 QGuiApplication，碰不了 Qt。

    use dioxus::dioxus_core::{
        Mutation,
        Mutations,
    };


    /// 和 qt_handle_event 里那几步一一对应。
    fn send_click(
        vdom: &VirtualDom,
        id: ElementId,
    ) {
        // 转换器是个进程级全局槽，没装就 unwrap 炸；每个测试线程各调一次无所谓。
        set_event_converter(
            std::boxed::Box::new(QtEventConverter),
        );

        let runtime = vdom.runtime();

        let _guard = RuntimeGuard::new(
            runtime.clone(),
        );

        let payload: std::rc::Rc<dyn std::any::Any> = std::rc::Rc::new(
            PlatformEventData::new(std::boxed::Box::new(QtMouseData {
                coords: QtCoords {
                    local_x: 55.0,
                    local_y: 24.0,
                    window_x: 177.0,
                    window_y: 51.0,
                    screen_x: 177.0,
                    screen_y: 51.0,
                    modifiers: 0,
                    // 和真左键松开来的一样：0x1 == Qt::LeftButton，
                    // buttons 在松开这一刻是空的。
                    button: 0x1,
                    buttons: 0,
                },
            })),
        );

        runtime.handle_event(
            "click",
            Event::new(payload, true),
            id,
        );
    }


    /// 跑一轮渲染，把这轮的指令流拿下来。
    fn render(
        vdom: &mut VirtualDom,
    ) -> Vec<Mutation> {
        let mut out = Mutations::default();

        vdom.render_immediate(
            &mut out,
        );

        out.edits
    }


    /// 指令流里出现过的 click 监听器，按 dioxus 发出它们的顺序。
    fn click_listeners(
        edits: &[Mutation],
    ) -> Vec<ElementId> {
        edits
            .iter()
            .filter_map(|edit| match edit {
                Mutation::NewEventListener { name, id } if name == "click" => {
                    Some(*id)
                }
                _ => None,
            })
            .collect()
    }


    /// 指令流里被写出去的所有文本。
    fn texts(
        edits: &[Mutation],
    ) -> Vec<String> {
        edits
            .iter()
            .filter_map(|edit| match edit {
                Mutation::SetAttribute {
                    name,
                    value,
                    ..
                } if *name == "text" => match value {
                    AttributeValue::Text(text) => Some(text.to_string()),
                    _ => None,
                },
                Mutation::SetText { value, .. } => Some(value.clone()),
                _ => None,
            })
            .collect()
    }


    /// 值的形态，打成一小段文字。
    ///
    /// 为什么要这个而不是直接用 `{:?}`：断言里要写的是
    /// 「color 这一条变成了 None」，Debug 的整段输出会把测试改成
    /// 一改就红的字符串。
    fn describe_value(
        value: &AttributeValue,
    ) -> String {
        match value {
            AttributeValue::Text(text) => {
                format!("Text({text:?})")
            }
            AttributeValue::Int(v) => {
                format!("Int({v})")
            }
            AttributeValue::Float(v) => {
                format!("Float({v:?})")
            }
            AttributeValue::Bool(v) => {
                format!("Bool({v})")
            }
            AttributeValue::None => "None".to_string(),
            AttributeValue::Listener(_) => {
                "Listener".to_string()
            }
            AttributeValue::Any(_) => "Any".to_string(),
        }
    }


    /// 这一轮写出去的所有属性：(名字, 值的形态)。
    ///
    /// 有了这个，「撤掉一个属性」在指令流里就是可指认的一条
    /// `("color", "None")`，而不是「SetAttribute 好像多了一条」。
    fn attr_writes(
        edits: &[Mutation],
    ) -> Vec<(String, String)> {
        edits
            .iter()
            .filter_map(|edit| match edit {
                Mutation::SetAttribute {
                    name, value, ..
                } => Some((
                    name.to_string(),
                    describe_value(value),
                )),
                _ => None,
            })
            .collect()
    }


    /// 这一轮新建的模板根（LoadTemplate 给的 id）—— 一行条目就是这样一个根。
    fn loaded_roots(
        edits: &[Mutation],
    ) -> Vec<ElementId> {
        edits
            .iter()
            .filter_map(|edit| match edit {
                Mutation::LoadTemplate { id, .. } => Some(*id),
                _ => None,
            })
            .collect()
    }


    /// 这一轮新建的占位节点（模板里那些「洞」）。
    fn created_placeholder_ids(
        edits: &[Mutation],
    ) -> Vec<ElementId> {
        edits
            .iter()
            .filter_map(|edit| match edit {
                Mutation::CreatePlaceholder { id } => Some(*id),
                _ => None,
            })
            .collect()
    }


    /// 这一轮的 AssignId：(模板内路径, 分到的 ElementId)。
    fn assigned_ids(
        edits: &[Mutation],
    ) -> Vec<(Vec<u8>, ElementId)> {
        edits
            .iter()
            .filter_map(|edit| match edit {
                Mutation::AssignId { path, id } => {
                    Some((path.to_vec(), *id))
                }
                _ => None,
            })
            .collect()
    }


    /// 这一轮的 ReplaceWith：(被换掉的那个节点, 栈上交出去几个节点)。
    fn replaced(
        edits: &[Mutation],
    ) -> Vec<(ElementId, usize)> {
        edits
            .iter()
            .filter_map(|edit| match edit {
                Mutation::ReplaceWith { id, m } => Some((*id, *m)),
                _ => None,
            })
            .collect()
    }


    /// 一条指令流的「形状」：只留每条 mutation 的种类名。
    /// 断言写成这个形式，红的时候一眼看得出是哪一步的哪条指令变了。
    fn kinds(
        edits: &[Mutation],
    ) -> Vec<&'static str> {
        edits
            .iter()
            .map(|edit| match edit {
                Mutation::AppendChildren { .. } => "AppendChildren",
                Mutation::AssignId { .. } => "AssignId",
                Mutation::CreatePlaceholder { .. } => "CreatePlaceholder",
                Mutation::CreateTextNode { .. } => "CreateTextNode",
                Mutation::LoadTemplate { .. } => "LoadTemplate",
                Mutation::ReplaceWith { .. } => "ReplaceWith",
                Mutation::ReplacePlaceholder { .. } => "ReplacePlaceholder",
                Mutation::InsertAfter { .. } => "InsertAfter",
                Mutation::InsertBefore { .. } => "InsertBefore",
                Mutation::SetAttribute { .. } => "SetAttribute",
                Mutation::SetText { .. } => "SetText",
                Mutation::NewEventListener { .. } => "NewEventListener",
                Mutation::RemoveEventListener { .. } => "RemoveEventListener",
                Mutation::Remove { .. } => "Remove",
                Mutation::PushRoot { .. } => "PushRoot",
            })
            .collect()
    }


    #[test]
    fn first_render_is_zero_with_three_buttons() {
        let mut vdom = VirtualDom::new(app);

        let mut out = Mutations::default();
        vdom.rebuild(&mut out);

        let shown = texts(&out.edits);

        assert!(
            shown.contains(&"counter = 0".to_string()),
            "第一屏就该有 counter = 0，实际写出的文本 = {shown:?}"
        );

        // 三个按钮是仅有的监听器：count = 0 时 if 那格是空的、列表没有一行。
        assert_eq!(
            click_listeners(&out.edits).len(),
            3,
            "按钮三个，监听器就该三条"
        );

        assert!(
            !shown.iter().any(|t| t.starts_with("第 ")),
            "count = 0 不该有任何一行：{shown:?}"
        );
    }


    #[test]
    fn plus_adds_a_row_and_minus_replaces_it() {
        let mut vdom = VirtualDom::new(app);

        let mut out = Mutations::default();
        vdom.rebuild(&mut out);

        let buttons = click_listeners(&out.edits);

        // 模板里按钮的顺序就是 减一、加一、归零（write_attrs 按模板顺序发）。
        // 这条不用猜：下面分别点它们，count 各走各的方向就说明对上了。
        assert_eq!(buttons.len(), 3);

        send_click(&vdom, buttons[1]);

        let added = render(&mut vdom);

        let shown = texts(&added);

        assert!(
            shown.contains(&"counter = 1".to_string()),
            "点「加一」之后要看到 counter = 1，实际 = {shown:?}"
        );
        assert!(
            shown.iter().any(|t| t.starts_with("第 0 行")),
            "count 到 1 得长出一行，实际 = {shown:?}"
        );

        // 这一行是**新建**出来的：LoadTemplate 给一个根 id，
        // 然后那个洞（第一次渲染时就建好的占位节点）被它顶掉。
        let row_root = loaded_roots(&added);

        assert_eq!(
            row_root.len(),
            1,
            "只该有一个新建的模板根（那一行的容器），实际 = {row_root:?}"
        );

        let slots = created_placeholder_ids(&out.edits);

        assert_eq!(
            slots.len(),
            2,
            "第一屏就该有两个洞：if 那一格 + for 那一格"
        );

        let swapped = replaced(&added);

        assert_eq!(swapped.len(), 1);
        assert!(
            slots.contains(&swapped[0].0),
            "被顶掉的必须是那两个占位节点之一，实际 = {:?}",
            swapped[0].0
        );
        assert_eq!(swapped[0].1, 1, "一行 = 栈上一个节点");

        // 行里的两个孩子（文本 path[0]、小方块 path[1]）都得先 AssignId 才有 id，
        // 而监听器就挂在那个 id 上 —— 这条正好把 assign_node_id 和事件接起来。
        let assigns = assigned_ids(&added);

        assert_eq!(
            assigns
                .iter()
                .map(|(path, _)| path.clone())
                .collect::<Vec<_>>(),
            vec![vec![0u8], vec![1u8]],
            "行容器 path 0 和 1 各要一个 id"
        );

        let new_listeners = click_listeners(&added);

        assert_eq!(
            new_listeners,
            vec![assigns[1].1],
            "监听器挂在 path [1]（那个小方块）刚拿到的 id 上"
        );
        assert!(
            !buttons.contains(&new_listeners[0]),
            "而且它是**新**的，不是按钮那三个里的哪个"
        );

        send_click(&vdom, buttons[0]);

        let removed = render(&mut vdom);

        assert!(
            texts(&removed).contains(&"counter = 0".to_string()),
            "点「减一」要回到 0"
        );

        // 收缩到空**不是**一条 Remove：dioxus 先新建一个占位节点，
        // 再用它把那一行的根换掉（实测 1→0 的指令流 =
        // SetAttribute, CreatePlaceholder, ReplaceWith{id: 那一行的根, m: 1}）。
        assert_eq!(
            created_placeholder_ids(&removed).len(),
            1,
            "槽位要回到占位节点"
        );
        assert_eq!(
            replaced(&removed),
            vec![(row_root[0], 1)],
            "被换掉的正是刚才建出来那一行的根 —— renderer 在这里走 destroy"
        );
    }



    #[test]
    fn milestone_line_grows_at_three_and_shrinks_back_to_a_placeholder() {
        let mut vdom = VirtualDom::new(app);

        let mut out = Mutations::default();
        vdom.rebuild(&mut out);

        let buttons = click_listeners(&out.edits);

        // 1 和 2：那一格还是空的。
        for i in 1..=2 {
            send_click(&vdom, buttons[1]);

            let edits = render(&mut vdom);

            assert!(
                !texts(&edits)
                    .iter()
                    .any(|t| t.starts_with("已经点到")),
                "count = {i} 还不该有提示行，实际 = {:?}",
                texts(&edits)
            );
        }

        send_click(&vdom, buttons[1]);

        let at_three = render(&mut vdom);

        assert!(
            texts(&at_three)
                .iter()
                .any(|t| t.starts_with("已经点到 3")),
            "到 3 了，提示那一格该长出来"
        );
        assert_eq!(
            loaded_roots(&at_three).len(),
            2,
            "这一次点击建了两个模板根：提示那一格（Text 组件）+ 新长出来的第 3 行"
        );

        let swapped = replaced(&at_three);

        assert_eq!(swapped.len(), 1);
        assert!(
            created_placeholder_ids(&out.edits).contains(&swapped[0].0),
            "顶掉的正是第一屏留下的那个洞"
        );

        send_click(&vdom, buttons[0]);

        let back_to_two = render(&mut vdom);

        assert!(
            !texts(&back_to_two)
                .iter()
                .any(|t| t.starts_with("已经点到")),
            "回到 2，提示行该没了，实际 = {:?}",
            texts(&back_to_two)
        );

        // 实测 3→2 的指令流（kinds）：
        //     SetAttribute ×3, CreatePlaceholder, ReplaceWith,
        //     SetAttribute ×2, Remove
        // 前面那条 CreatePlaceholder+ReplaceWith 是提示那一格**换回占位节点**，
        // 最后那条 Remove 是行数从 3 缩到 2 —— 两件事走的是两支。
        //
        // 五条 SetAttribute 各是谁（下面按名字逐条钉住，光数条数说明不了什么）：
        //     color → None（3 是奇数、2 是偶数：这个属性被**撤掉**）
        //     visible → Bool(false)（指示条跟着奇偶关掉）
        //     text → "counter = 2"，再加两行条目的 text
        assert_eq!(
            kinds(&back_to_two),
            vec![
                "SetAttribute",
                "SetAttribute",
                "SetAttribute",
                "CreatePlaceholder",
                "ReplaceWith",
                "SetAttribute",
                "SetAttribute",
                "Remove",
            ]
        );

        let writes = attr_writes(&back_to_two);

        assert!(
            writes.contains(
                &("color".to_string(), "None".to_string())
            ),
            "3→2 该有一条 color 被撤，实际 = {writes:?}"
        );
        assert!(
            writes.contains(
                &("visible".to_string(), "Bool(false)".to_string())
            ),
            "3→2 该关掉指示条，实际 = {writes:?}"
        );

        // 反向：opacity 在 1→2→3 全程都是 1.0，没变过就不该重发
        // （diff/node.rs:449-463 那条 value_changed 判据）。
        // 哪天它变成每次重发，这条会红 —— 那也意味着我们在重写没动的属性。
        assert!(
            !writes.iter().any(|(name, _)| name == "opacity"),
            "opacity 没变，不该出现在这一轮里，实际 = {writes:?}"
        );
    }


    #[test]
    fn reset_wipes_every_row_and_the_count() {
        let mut vdom = VirtualDom::new(app);

        let mut out = Mutations::default();
        vdom.rebuild(&mut out);

        let buttons = click_listeners(&out.edits);

        // 加满：条目是 (0..count).take(4)，所以第 4 次之后行数封顶。
        let mut per_click = Vec::new();

        for _ in 0..5 {
            send_click(&vdom, buttons[1]);

            per_click.push(render(&mut vdom));
        }

        // 那三个按钮的监听器只发一次：闭包换了 dioxus 也不重发
        // （diff/node.rs:458-460 `(Listener, Listener) => false`）。
        // 注意看的是**按钮那三个 id**有没有再出现 ——
        // 新建出来的行里那个小方块确实会带一条新监听器，那是另一回事。
        for (i, edits) in per_click.iter().enumerate() {
            for id in click_listeners(edits) {
                assert!(
                    !buttons.contains(&id),
                    "第 {} 次点击的重渲染里又发了按钮的监听器 id={:?}",
                    i + 1,
                    id
                );
            }
        }

        let fifth = texts(&per_click[4]);

        assert!(
            fifth
                .iter()
                .any(|t| t.starts_with("第 3 行")),
            "第五屏该还留着第 3 行（行数封顶在 4），实际 = {fifth:?}"
        );
        assert!(
            fifth.contains(&"counter = 5".to_string()),
            "标题得跟着走，实际 = {fifth:?}"
        );

        send_click(&vdom, buttons[2]);

        let wiped = render(&mut vdom);

        let after_reset = texts(&wiped);

        assert!(
            after_reset.contains(&"counter = 0".to_string()),
            "归零之后要看到 counter = 0，实际 = {after_reset:?}"
        );
        assert!(
            !after_reset.iter().any(|t| t.starts_with("第 ")),
            "归零之后一行都不该有，实际 = {after_reset:?}"
        );

        // 实测 5→0 的指令流（kinds，跑出来的原样）：
        //     SetAttribute ×4, CreatePlaceholder, ReplaceWith,
        //     CreatePlaceholder, Remove, Remove, Remove, ReplaceWith
        // 四行条目 + 提示那一格一起消失。两个格各自**换回占位节点**
        // （两个 CreatePlaceholder + 两个 ReplaceWith），
        // 行数从 4 缩到 0 时前面三条走 Remove、最后一条被新建的占位换掉。
        // 直觉里的「五条 Remove」是不存在的 —— 收缩到空那一支不发明 Remove。
        //
        // 那四条 SetAttribute 是：color 被撤（None）、opacity 回到 0.45、
        // visible 关掉、标题写成 counter = 0。
        assert_eq!(
            kinds(&wiped),
            vec![
                "SetAttribute",
                "SetAttribute",
                "SetAttribute",
                "SetAttribute",
                "CreatePlaceholder",
                "ReplaceWith",
                "CreatePlaceholder",
                "Remove",
                "Remove",
                "Remove",
                "ReplaceWith",
            ]
        );

        let writes = attr_writes(&wiped);

        for expect in [
            ("color", "None"),
            ("opacity", "Float(0.45)"),
            ("visible", "Bool(false)"),
            ("text", "Text(\"counter = 0\")"),
        ] {
            assert!(
                writes.contains(
                    &(expect.0.to_string(), expect.1.to_string())
                ),
                "归零这一轮少了 ({0}, {1})，实际 = {writes:?}",
                expect.0,
                expect.1
            );
        }
    }
}
