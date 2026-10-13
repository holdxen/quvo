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
//! 1. `app()` —— 用户写的 UI 就三行
//! 2. `mod ffi` —— Rust 能让 C++ 做的全部十六件事
//! 3. `QtRenderer` 的三个字段 —— renderer 记了哪三本账
//! 4. `impl WriteMutations` —— 指令如何变成 Qt 调用（核心）
//! 5. `main()` —— 拼起来的顺序
//!
//!
//! ## 当前边界（重要）
//!
//! mount（首次渲染）整条路是通的；diff 只通了一条分支：
//!
//! - `remove_node` 实现了，配套的 C++ `destroy` 也在 ——
//!   `main()` 第 4.7 步会手动驱动一次「列表变短」来看它真的删了对象；
//! - `replace_node_with` / `insert_nodes_after` / `insert_nodes_before`
//!   在本文件里还是 panic；
//! - `assign_node_id` 是空的，所以模板深处的静态元素没有 ElementId，
//!   给它们的更新会被静丢掉；
//! - 输入侧还是 0：事件监听是空实现，`main()` 里也没有东西去 poll
//!   VirtualDom —— 第 4.7 步那次 diff 是手动推的，不是自己变的。
//!
//! 详见 `main()` 的注释。

use cxx::UniquePtr;

use cxx_qt_lib::QGuiApplication;

use dioxus::dioxus_core::{
    AttributeValue,
    ElementId,
    RuntimeGuard,
    ScopeId,
    Template,
    TemplateAttribute,
    TemplateNode,
    VirtualDom,
    WriteMutations,
};

use dioxus::prelude::*;

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{
    AtomicUsize,
    Ordering,
};
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
//     measure_text()         布局时问 Qt「这段字要多大」
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

        // QObject::setProperty(name, QVariant( QString(value)) )。
        //
        // 属性名直接对上 Qt 的属性名：
        // Dioxus 侧写 text: "..."，这里就落到 QtQuick.Text::text。
        // 写不进去的（类型不匹配，比如 color 要 QColor、
        // font.pixelSize 要 int）会在 C++ 侧 qWarning。
        unsafe fn set_property(
            self: Pin<&mut QtHost>,
            item: *mut QQuickItem,
            name: &str,
            value: &QVariant,
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

        // 量一段文本要多大 —— 走的是 QQuickText 同一套算法
        // （QTextLayout + design metrics），见 cpp/qt_host.cpp。
        //
        // max_width <= 0：不限宽，折一行，宽度 = Qt 的 implicitWidth 口径。
        // max_width >  0：按这个宽度折行，返回的是「用到的宽度」
        //                （最宽那一行，且不超过 max_width）和总高度。
        fn measure_text(
            self: &QtHost,
            text: &str,
            max_width: f64,
        ) -> TextMeasure;

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

        // 起一个 QTimer，每 interval_ms 把 Qt 事件循环叫起来一次，
        // 每次 timeout 都去调下面那个 `extern "Rust"` 的 qt_pump_tick。
        //
        // 这就是「借 Qt 的循环当 executor」的那一半：
        // Dioxus 这边不再自己转圈，只是搭 Qt 的便车。
        fn start_pump(
            self: Pin<&mut QtHost>,
            interval_ms: i32,
        );

        // 诊断用：读 Qt 侧 childItems 的实际顺序（对象地址）。
        // 见 cpp/qt_host.h 的注释 —— 插到中间这件事，从坐标上看不出来。
        unsafe fn child_order(
            self: &QtHost,
            parent: *mut QQuickItem,
        ) -> Vec<u64>;
    }

    // 反方向：C++ 调 Rust。
    //
    // cxx 会把 `extern "Rust"` 里的自由函数在生成头里声明成一个 C++ 函数，
    // C++ 那边（cpp/qt_host.cpp）include 生成头就能直接调 ——
    // QTimer::timeout 的 lambda 里就是这么把控制权交回 Rust 的。
    extern "Rust" {
        fn qt_pump_tick();
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
//    <qt-column> 上那几个是模板里写死的 → TemplateAttribute::Static，
//    在 create_template_node 里就记进布局账本；
//    Text / Rectangle 那几个是组件 props 插值 → SetAttribute，
//    在 set_attribute 里记账。
//
// 3. <qt-column> 的模板里现在有「两个洞」
//    （Text 是一个组件调用；for 循环整块是另一个 Dynamic 节点）：
//
//        node_paths = [ [0,0], [0,1] ]
//
//    所以 ReplacePlaceholder 会被调用两次，而且 Dioxus 是
//    倒着发的（先 [0,1] 再 [0,0]）—— 正好是那条不变式的活教材。
//    第二个洞一次顶上来 m 个节点（m = 当前矩形个数）。

// 手动 diff 的脚本表 —— 这不是 UI 状态的正道。
//
// 正道是 signal + 事件回路：signal 变了 → 有人 poll VirtualDom →
// render_immediate 出 diff。那条回路还没接（下一步），
// 所以这里写死一张脚本表，让 main() 能确定地把 diff 走过一遍。
//
// 为什么表里放的是「key 列表」而不是个数：只有 keyed 列表才凑得出四种不同的
// diff 结果，一条 mutation 都不漏 ——
//
//     老 key 少了一个      → 单独的 Remove        （diff/iterator.rs:316）
//     尾部多了新 key       → InsertAfter，新节点    （diff/iterator.rs:452-469）
//     key 全在、只换顺序   → InsertBefore/After，
//                           递过来的是**已有**的节点（移动全程不发 Remove，
//                           见 diff/iterator.rs:343-375）
//     老 key 一个都不留    → 先 create 新的，再把最后一个老的
//                           换成 ReplaceWith        （diff/iterator.rs:305-308）
//
// 事件回路已经接上了，这张表现在只是**假的事件源**：
// driver_send_work 每 TICKS_PER_STEP 个 tick 推一格、发一张纸条。
// 等 create_event_listener + handle_event 落地，该删的是这张表，
// 回路本身不动。
const DIFF_SCRIPT: &[&[usize]] = &[
    &[0, 1],       // 第 4 步 mount 就是这个样子
    &[0],          // Remove
    &[0, 1, 2],    // InsertAfter，两个都是新建的
    &[2, 0, 1],    // InsertBefore：移动已有节点（被移的在锚点后面 —— 下标不变）
    &[1, 2, 0],    // InsertBefore：再往回收一格
    &[2, 0, 1, 3], // InsertAfter：新建一个 + 移动一个，被移的那个在锚点前面，
                   // 而且要插到末尾 —— place 的「摘了再挂回去」那支为它而加
    &[3, 0, 1, 2], // 一步发两条：InsertAfter + InsertBefore
    &[1, 2, 3, 0], // InsertBefore 一次挪两个
    &[7, 8],       // 没有共同 key：Remove×3 + ReplaceWith
];

static DIFF_STEP: AtomicUsize = AtomicUsize::new(0);

fn diff_keys() -> Vec<usize> {
    let step = DIFF_STEP
        .load(Ordering::Relaxed)
        .min(DIFF_SCRIPT.len() - 1);

    DIFF_SCRIPT[step].to_vec()
}

fn app() -> Element {
    // 没有 main.qml，也没有 QML 的 Layout / anchors。
    let keys = diff_keys();

    println!("[app] 被渲染，列表 key = {keys:?}");

    // 容器的宽度 = 100 + 50 * key —— 这是给日志用的「身份证」。
    // dump_tree 打出来的是尺寸，所以每个位置上是哪个 key 一眼就能看出来
    //（渲染器自己不认识 key，只能把 key 编码进尺寸里）。
    let items: Vec<(usize, usize)> = keys
        .iter()
        .map(|key| (*key, 100 + key * 50))
        .collect();

    rsx! {
        qt-column {
            width: "600",
            height: "180",
            flex_direction: "column",
            gap: "12",

            Text {
                text: "Hello from Dioxus -> Qt Quick (taffy layout)",
            }

            // 每个条目刻意做成「一个容器 + 一个孩子」，也就是 2 个节点的一棵小子树。
            // 这样删一个条目时 destroy 只调用一次、日志里却有 2 行 object destroyed，
            // 同时证明：孩子是 QObject 的孩子（跟着父亲递归析构）、
            // 我们的账本整棵清干净了、没有双重释放。
            for (key, item_width) in items {
                qt-column {
                    key: "{key}",
                    width: "{item_width}",
                    height: "40",

                    Rectangle {
                        width: 100.0,
                        height: 40.0,
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
    style: Vec<(&'static str, String)>,

    // 这个节点上显示的文本（只有文本类节点会有）。
    //
    // 两个用处：
    //     1. relayout() 把它塞进 taffy 节点的 context，
    //        测量回调拿到的就是它（见 compute_layout_with_measure）；
    //     2. verify_text_measurement() 用它做「我们量的」vs「Qt 自己算的」对照。
    content: String,
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


    // 布局树。
    //
    // 它不是第二棵「UI 树」：每个有 Qt 对象的节点在这里有一个对应节点，
    // 父子关系由 sync_taffy_children 在 Node 的孩子变化时同步过来，
    // 占位节点不进这棵树。
    //
    // 泛型参数是 taffy 的「节点 context」：这里放这段文本的内容，
    // 测量回调会原样拿到它（没有文本的节点不设 context，量出来就是 0）。
    tree: TaffyTree<String>,

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
        let mut tree = TaffyTree::<String>::new();

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
                        "column".to_string(),
                    ),
                ],
                content: String::new(),
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
                content: String::new(),
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
    // 会被绑定的只有 Dioxus 「点名」过的那几个节点：
    // LoadTemplate 的根、CreateTextNode、CreatePlaceholder。
    // 模板深处的静态元素默认没有 ElementId ——
    // 想要的话得实现 assign_node_id（这一版是空的，见下面）。
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


                // 模板里写死的属性（text: "字面量"）此刻就能应用；
                // 动态属性（text: "{expr}"）Dioxus 稍后会通过
                // set_attribute 送过来。
                //
                // 布局属性（width / height / gap / flex-direction）
                // 只记账，不写给 Qt —— 几何只有一个作者。
                for attr in attrs.iter() {
                    if let TemplateAttribute::Static {
                        name,
                        value,
                        ..
                    } = attr
                    {
                        self.record_style(
                            element_node,
                            name,
                            value,
                        );

                        self.record_content(
                            element_node,
                            name,
                            value,
                        );

                        let value = cxx_qt_lib::QString::from(*value);
                        let value = cxx_qt_lib::QVariant::from(&value);

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
                                        &value,
                                    );
                            }
                        }
                    }
                }


                // 静态子节点递归建；
                // 动态位置建出一个占位节点。
                for child in children.iter() {
                    let child_node =
                        self.create_template_node(child);


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


                self.new_node(Some(object))
            }


            // 动态位置：先占个坑，
            // 之后 ReplacePlaceholder 会拿真节点顶掉它。
            //
            // 这里被忽略的那个 id（TemplateNode::Dynamic { id }）
            // 是 dynamic_nodes 和 node_paths 里的下标。
            // 本 renderer 不需要它 ——
            // 我们是按「它在树的哪个位置」来找这个坑的，
            // 下标信息已经在父子顺序里了。
            TemplateNode::Dynamic {
                ..
            } => self.new_node(None),
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
    // 同一名字重复记没关系：style_for 按顺序读，后面的覆盖前面的。
    fn record_style(
        &mut self,
        node: u64,
        name: &'static str,
        value: &str,
    ) {
        if !is_layout_prop(name) {
            return;
        }


        self.nodes
            .get_mut(&node)
            .unwrap_or_else(|| {
                panic!("unknown node id {node}")
            })
            .style
            .push((name, value.to_string()));
    }


    // 记下这个节点显示的文本。
    //
    // 这份账是布局的输入，不只是诊断用的：relayout() 会把它
    // 交给 taffy 的节点 context，测量回调量的就是它。
    fn record_content(
        &mut self,
        node: u64,
        name: &str,
        value: &str,
    ) {
        if name != "text" {
            return;
        }


        self.nodes
            .get_mut(&node)
            .unwrap_or_else(|| {
                panic!("unknown node id {node}")
            })
            .content = value.to_string();
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

            // 文本内容送进 taffy 的节点 context。
            //
            // 测量是 taffy 反过来调我们的：它回调时只递给自己存的
            // context 和 style，不认识我们的 Node，
            // 所以「这段字是什么」必须在算之前就先放进去。
            // 空文本（Rectangle / Item）不设 context ——
            // 那些叶子的尺寸本来就该来自 style 里的 width / height。
            let content = self.node(id).content.clone();

            self.tree
                .set_node_context(
                    taffy,
                    if content.is_empty() {
                        None
                    } else {
                        Some(content)
                    },
                )
                .expect("taffy: set_node_context 失败");
        }


        // 2. 视口问 Qt 要，不在 Rust 里再写一个 640x220。
        //    taffy 用 f32，bridge 用 f64，边界处换算一次。
        let viewport = Size {
            width: AvailableSpace::Definite(
                self.host.window_width() as f32,
            ),
            height: AvailableSpace::Definite(
                self.host.window_height() as f32,
            ),
        };


        // 3. 算布局。
        //
        //    用的是 compute_layout_with_measure 而不是 compute_layout：
        //    叶子节点（taffy 眼里没有孩子的节点 = 文本和矩形）的尺寸
        //    taffy 自己算不出来，得回头问 Qt。
        //    compute_layout 其实就是「测量函数永远返 0」的特例。
        //
        //    借用要拆成字段级：tree 需要 &mut（它在写布局结果），
        //    host 只需要 &（measure_text 是 const 的），
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
                            // 没有文本的叶子问不出尺寸。
                            // 它的宽高写在 style 里，
                            // compute_leaf_layout 会用 style 覆盖这里的结果。
                            let Some(content) = context else {
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

                            let measured = host.measure_text(
                                content.as_str(),
                                max_width,
                            );

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

            let content = self.node(id).content.clone();

            // 只有文本节点值得比：Rectangle / Item 的 implicit 是 0。
            if content.is_empty() {
                continue;
            }

            let free = self.host.measure_text(&content, -1.0);
            let wrapped = self.host.measure_text(&content, 100.0);

            // SAFETY: 同一个对象指针，来自 create_*；
            // 读 implicit size 不改对象所有权，也不管它挂没挂。
            let implicit_w = unsafe {
                self.host.implicit_width(object)
            };
            let implicit_h = unsafe {
                self.host.implicit_height(object)
            };

            println!(
                "[measure] node {id} obj {object:p} \
                 text={content:?}"
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
// 还没接的两件事：
//   - 字体：两边现在都用 QGuiApplication::font()（C++ 那边是默认构造的
//     QFont，QML 里那个 Text 没写 font）。所以「量的」和「画的」恰好是
//     同一个字体，Δ 才是 0。一旦 font 变成 Rust 下发的样式属性，
//     measure_text 必须收同一个 QFont，不能各拿各的。
//   - 窗口 resize：视口尺寸只在 relayout 那一刻读一次，
//     拖动窗口不会重算（连重算的触发点都还没接）。

// 这些属性名不写给 Qt，只进布局。
// 名字同时接受下划线和短横两种写法：rsx! 里两种都能写。
const LAYOUT_PROPS: &[&str] = &[
    "width",
    "height",
    "gap",
    "flex_direction",
    "flex-direction",
];


fn is_layout_prop(
    name: &str,
) -> bool {
    LAYOUT_PROPS.contains(&name)
}


// 只接受纯数字。
//
// 故意不接受 "50%"：百分比需要知道父节点尺寸，
// 现在支持它就会让人以为整条百分比链路都通了。
fn parse_length(
    value: &str,
) -> Option<f32> {
    value.trim().parse::<f32>().ok()
}


// 把记下来的属性拼成 taffy 的输入。
//
// 没写 width / height 的节点就是 auto。
// 对文本节点，auto 现在有人接了：relayout 的测量回调会问 Qt 要尺寸。
// 对矩形 / Item，auto 还是「按内容撑开」而内容没有尺寸来源 → 算出 0，
// 屏幕上就是空白。这是刻意的：几何不写就没有，不会被 Qt 的
// implicit size 偷偷填上。
fn style_for(
    props: &[(&'static str, String)],
) -> Style {
    let mut style = Style {
        display: Display::Flex,
        ..Default::default()
    };


    for (name, value) in props {
        match (*name, parse_length(value)) {
            ("width", Some(v)) => {
                style.size.width =
                    Dimension::length(v);
            }

            ("height", Some(v)) => {
                style.size.height =
                    Dimension::length(v);
            }

            ("gap", Some(v)) => {
                style.gap = Size {
                    width: length(v),
                    height: length(v),
                };
            }

            ("flex_direction", _)
            | ("flex-direction", _) => {
                style.flex_direction =
                    if value == "column" {
                        FlexDirection::Column
                    } else {
                        FlexDirection::Row
                    };
            }

            // 数字解析不出来的（比如 "" / "abc" / "50%"）
            // 当没写过，不猜。
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

        let node =
            self.create_template_node(root);


        // 只有根节点带 ElementId；
        // 模板深处的静态元素要等 AssignId 才有 id，
        // 而 assign_node_id 目前是空实现。
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
    // 下面这些这一版用不到。
    //
    // 先打个补丁：这个分组标题现在已经不太准了。
    // create_placeholder 和 replace_placeholder_with_nodes
    // 现在就在一起跑 —— Rectangle 顶掉 <qt-text> 里那个坑位
    // 走的正是这两条，它们属于 mount 路径。
    //
    // 剩下几个真正「用不到」的原因是它们只出现在 diff（更新）路径，
    // 首次渲染根本不会发：
    //
    //     assign_node_id      → AssignId，
    //                           给模板深处的元素补 id（diff/node.rs:871）
    //     replace_node_with   → 旧节点整坨被换掉（diff/node.rs:326、381）
    //     remove_node         → 旧节点删除（diff/node.rs:328、383）
    //     insert_nodes_before → 列表重排（diff/iterator.rs:447）
    //     insert_nodes_after  → 列表重排（diff/iterator.rs:467）
    //
    // 结论：光把 Qt 的事件接上、让 signal 能触发 diff 还不够，
    // panic 的那几个必须一起实现，否则第一次更新就炸。
    //
    // 下一版再真正实现。
    // ========================================================

    // 给模板深处的静态元素补一个 ElementId。
    //
    // path 语义跟 resolve_slot 一样（相对栈顶那个模板根）。
    //
    // 现在为空实现的后果：
    // 只要嵌套深一层，比如 <qt-text> 里面还有一个静态 <qt-row>，
    // 那个内层元素就永远拿不到 ElementId，
    // Dioxus 之后 SetAttribute(id=内层) 找不到节点，
    // 属性更新被 object_of 那层静默丢弃
    // （见 set_attribute 里「还没有映射到 Qt 对象」那个分支）。
    // 这一版所有属性都挂在模板根上，所以看不出问题。
    fn assign_node_id(
        &mut self,
        _path: &'static [u8],
        _id: ElementId,
    ) {
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
        // 动态属性走这里。
        //
        // 例如组件传下来的 text：
        //
        //     模板里是 @dynamic#0
        //
        //     Dioxus:
        //
        //     SetAttribute {
        //         id: 1,
        //         text: "Hello ..."
        //     }
        //
        // id 就是 load_template 里那个元素的 ElementId。

        // 这一版只处理字符串值。
        //
        // Int / Float / Bool 也会从 Dioxus 过来，
        // 但那要在 C++ 侧包成对应类型的 QVariant 才有意义，
        // 例如 color（QColor）。
        let AttributeValue::Text(text) = value
        else {
            println!(
                "[renderer] 暂不支持非字符串属性 \
                 {name}={value:?}"
            );

            return;
        };


        // 记下内容，测量对照要用（布局属性和普通属性都可能有文本）。
        if let Some(node) = self.lookup(id) {
            self.record_content(node, name, text);
        }


        // 布局属性在这里只记账，不写给 Qt：
        // width / height 之类的最终归宿是 taffy 的输入，
        // 再由 relayout() 通过 set_geometry 推回去。
        // 让 set_property 也写一遍就会出现两个作者。
        if is_layout_prop(name) {
            match self.lookup(id) {
                Some(node) => self.record_style(
                    node, name, text,
                ),

                None => println!(
                    "[renderer] SetAttribute id={} \
                     找不到节点，布局属性被丢弃",
                    id.0
                ),
            }

            return;
        }


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
             id={} {name}={text:?}",
            id.0
        );

        
        let value = cxx_qt_lib::QString::from(text);
        let value = cxx_qt_lib::QVariant::from(&value);

        // SAFETY: object 是 object_of 查出来的、某个还活在树上的节点的指针。
        unsafe {
            self.host
                .pin_mut()
                .set_property(
                    object,
                    name,
                    &value,
                );
        }
    }


    fn create_event_listener(
        &mut self,
        _name: &'static str,
        _id: ElementId,
    ) {
    }


    fn remove_event_listener(
        &mut self,
        _name: &'static str,
        _id: ElementId,
    ) {
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
//     Qt 的 QTimer 每 20ms 转一次 → 每次转都 poll 一遍 wait_for_work()
//     → Pending 就什么都不做，Ready 就 render_immediate + relayout
//
// 为什么要「poll 一次」而不是 `.await`：
// `wait_for_work` 是 `async fn`（virtual_dom.rs:445），`.await` 需要一个
// executor 去驱动它；而这里唯一的线程已经被 `app.exec()` 占着
// （`block_on` 会把纸条的来源也一起堵死）。所以把它当成一个
// 「每次事件都问一句」的同步接口来用 —— 这就是借 Qt 的循环当 executor。
//
// 一个关键的好性质：future **不需要跨 tick 存着**。
// wait_for_work 的全部状态都在 VirtualDom 自己身上：
// 纸条在 `self.rx`（virtual_dom.rs:215 那个 UnboundedReceiver），
// 脏 scope 在脏表里。future 本身只是「先看一眼，没有就等」这段流程，
// 所以每个 tick 现场新建一个、poll 一次、丢掉，一条消息都不会漏。
//
// waker 为什么可以什么都不做：真正的唤醒源是 Qt 的 timer，不是 Rust 的 waker。
// 纸条来了没人被 wake —— 下一个 tick 自己会再问一遍。
// poll 因此不是「必须有人 wake 才能醒」，而是「主动问一句有没有活」。

/// 什么都不做的 waker，见上面最后一段。
struct IdleWaker;

impl Wake for IdleWaker {
    fn wake(self: Arc<Self>) {}
    fn wake_by_ref(self: &Arc<Self>) {}
}

/// 回路的两端：VirtualDom（脏表 + 纸条队列）和我们的 renderer，
/// 必须放在同一个地方才能同时拿到 `&mut` —— `wait_for_work(&mut self)`
/// 借的是 vdom，`render_immediate(&mut renderer)` 借的是 renderer。
struct Pump {
    vdom: VirtualDom,
    renderer: QtRenderer,
    waker: Waker,
    /// 已经转过多少个 tick
    ticks: usize,
    /// DIFF_SCRIPT 走到第几步
    step: usize,
}

// Qt 的 timer 回调是这条线程上唯一碰 Pump 的地方，
// 所以 thread_local + RefCell 就够（不需要 Sync，也不需要锁）。
// 真正的 re-entrance（渲染中又触发一次 tick）会把 borrow_mut 打成 panic，
// 那是「回路里又跑回路」的信号，不是要悄悄吞掉的情况。
thread_local! {
    static PUMP: RefCell<Option<Pump>> = const { RefCell::new(None) };
}

/// 多少个 tick 喂一次假纸条（下面 driver_send_work 用）。
const TICKS_PER_STEP: usize = 3;

/// QTimer 的间隔。
const PUMP_INTERVAL_MS: i32 = 20;

/// 把 vdom + renderer 交给回路，并让 Qt 开始 tick。
fn install_pump(
    vdom: VirtualDom,
    mut renderer: QtRenderer,
) {
    // 先起 timer：这一步必须在把 renderer 交出去之前做，
    // 因为 host 藏在 renderer 里面。
    renderer
        .host
        .pin_mut()
        .start_pump(PUMP_INTERVAL_MS);

    PUMP.with(|slot| {
        *slot.borrow_mut() = Some(Pump {
            vdom,
            renderer,
            waker: Waker::from(Arc::new(IdleWaker)),
            ticks: 0,
            step: 0,
        });
    });

    println!(
        "[pump] 回路装好了：每 {PUMP_INTERVAL_MS}ms 一个 tick，\
         每 {TICKS_PER_STEP} 个 tick 喂一张纸条（共 {} 步）",
        DIFF_SCRIPT.len() - 1
    );
}


/// 假的事件源：把 DIFF_SCRIPT 的下一步推进去，然后**走 channel** 发纸条。
///
/// 为什么用 `runtime().needs_update(ScopeId::APP)` 而不是 `mark_all_dirty()`：
/// needs_update（runtime.rs:542）→ ScopeState::needs_update（scope_context.rs:135）
/// → `sender.unbounded_send(SchedulerMsg::Immediate(id))`（:142）——
/// 它是**真·纸条路径**，wait_for_work 得走一遍
/// `wait_for_event().await`（virtual_dom.rs:460）才醒得过来。
/// 而 mark_all_dirty 是直接往脏表里塞，醒都不醒就 Ready 了，
/// 那条 await 根本没被验证过。
///
/// ScopeId::APP = 3（scopes.rs:50：0 是 RootScopeWrapper、
/// 1 是默认 SuspenseBoundary、2 是 ErrorBoundary、3 才是 app()），
/// 这也正是上次 mark_dirty(ScopeId::ROOT) 毫无反应的根源：标的是 0 那个壳。
///
/// ⚠️ 调之前要把 runtime 推进 thread-local 的栈里（RuntimeGuard）。
///    不是形式主义：needs_update → ScopeState::needs_update_any
///    → `self.sender()`，而 sender() 的实现是
///    `Runtime::current().sender.clone()`（scope_context.rs:97-99）——
///    它问的是「当前线程正在跑的那个 runtime」，不是我们手里的这个。
///    第一次跑的时候没加 guard，当场 panic
///    「Must be called from inside a Dioxus runtime」（runtime.rs:100）。
fn driver_send_work(
    pump: &mut Pump,
) {
    if pump.step + 1 >= DIFF_SCRIPT.len() {
        // 脚本跑完了：静默，一个 tick 一行会淹掉日志。
        return;
    }

    pump.step += 1;

    DIFF_STEP
        .store(pump.step, Ordering::Relaxed);

    println!(
        "[driver] tick {} → 发 Immediate(ScopeId::APP) 纸条，\
         下一次渲染的 key = {:?}",
        pump.ticks,
        DIFF_SCRIPT[pump.step]
    );

    if pump.step + 1 >= DIFF_SCRIPT.len() {
        println!(
            "[driver] 这是最后一张纸条，之后回路只会 Pending，\
             一行渲染都不会再产生"
        );
    }

    let runtime = pump.vdom.runtime();

    let _guard = RuntimeGuard::new(
        runtime.clone(),
    );

    runtime.needs_update(ScopeId::APP);
}


/// 每个 Qt tick 干的事。这就是 C++ 那边 QTimer::timeout 调进来的函数。
fn qt_pump_tick() {
    PUMP.with(|slot| {
        let mut guard = slot.borrow_mut();

        let Some(pump) = guard.as_mut() else {
            return;
        };

        pump.ticks += 1;

        // 1. 定时喂一张纸条。发完就 return —— 故意不在这一次去醒：
        //    这样「发纸条」和「醒过来渲染」一定落在两个不同的 tick 上，
        //    日志里能看出因果不是同一句函数调用串起来的。
        if pump.ticks % TICKS_PER_STEP == 0 {
            driver_send_work(pump);

            return;
        }

        let Pump {
            vdom,
            renderer,
            waker,
            ticks,
            ..
        } = pump;

        // 2. poll wait_for_work 一次。
        let woke = {
            let mut fut = std::pin::pin!(
                vdom.wait_for_work()
            );

            let mut cx = Context::from_waker(&*waker);

            fut.as_mut().poll(&mut cx).is_ready()
        };

        if !woke {
            println!("[pump] tick {ticks} → Pending，这一轮什么都不做");

            return;
        }

        // 3. 醒了才干活。
        println!("[pump] tick {ticks} → Ready，render_immediate");

        vdom.render_immediate(
            renderer
        );

        // 批量边界：这一轮 mutation 全落完了，算一次布局。
        renderer.relayout();

        renderer.dump_tree();
    });
}


// ============================================================
// Main
// ============================================================
//
// 现在这个 main 只做两轮：第 4 步 mount，然后把 vdom + renderer 交给回路
// （第 5 步 install_pump），剩下的全在 Qt 的事件循环里发生。
// UI 会自己变 9 次 —— 但那 9 次的源头还是假的（DIFF_SCRIPT 喂的纸条）。
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
    // 然后产生 mutation：
    //
    // LoadTemplate
    // AppendChildren
    //
    // 这些 mutation 进入 QtRenderer。
    //
    // 精确一点，本项目实际产生的顺序是：
    //
    //     LoadTemplate(<qt-text>)       建 Text + 一个坑位
    //     SetAttribute("text", "Hello") 组件 props 是动态值
    //     LoadTemplate(<qt-rectangle>)  建 Rectangle
    //     ReplacePlaceholder([0], 1)    Rectangle 顶掉坑位
    //     AppendChildren(ElementId(0),1)整棵挂到窗口根
    //
    // rebuild 的定义就在 virtual_dom.rs:579，
    // 最后那句 append_children(ElementId(0), m) 是它硬编码的收尾。
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
    // （store → mark_all_dirty → render_immediate → relayout → dump_tree）。
    // 现在那三件事被拆进了 Qt 的循环里，而且拆成了两个不同的 tick：
    //
    //     tick 3k   → driver_send_work：needs_update(ScopeId::APP) 发纸条
    //     tick 3k+1 → poll wait_for_work → Ready → render_immediate + relayout
    //     其余 tick → Pending，一行渲染都不产生
    //
    // main 到这里就把 vdom / renderer 整个交出去，之后自己再也不碰它们。

    install_pump(
        vdom,
        renderer,
    );


    // 这条链上还剩什么是假的：纸条的**内容**。
    // 真实形态应该是 Qt 的鼠标事件 → create_event_listener
    // （那个空实现，见 impl WriteMutations）→ runtime().handle_event(...)
    // （runtime.rs:369）→ handler 里写 signal → dioxus 自己发那张纸条。
    // 现在用 DIFF_SCRIPT + 一个原子变量把它替了。
    //
    // 脚本跑完（9 步）之后 driver 不再发纸条，日志会一直停在 Pending ——
    // 那就是「回路在转、但没有空转渲染」的证据。

    println!(
        "Qt event loop started. \
         Close the window to exit."
    );


    if let Some(app) =
        qt_app.as_mut()
    {
        app.exec();
    }
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
    }


    #[test]
    fn only_layout_props_are_kept_from_qt() {
        assert!(is_layout_prop("width"));
        assert!(is_layout_prop("height"));
        assert!(is_layout_prop("gap"));
        assert!(is_layout_prop("flex-direction"));
        assert!(is_layout_prop("flex_direction"));

        // 反向：这些必须照常写给 Qt，不然文字和颜色没人画
        assert!(!is_layout_prop("text"));
        assert!(!is_layout_prop("color"));
    }


    #[test]
    fn style_for_reads_every_supported_prop() {
        let props = vec![
            ("width", "600".to_string()),
            ("height", "180".to_string()),
            ("flex_direction", "column".to_string()),
            ("gap", "12".to_string()),
        ];

        let style = style_for(&props);

        assert_eq!(style.size.width, Dimension::length(600.0));
        assert_eq!(style.size.height, Dimension::length(180.0));
        assert_eq!(style.flex_direction, FlexDirection::Column);
        assert_eq!(style.gap.width, length(12.0));
    }


    #[test]
    fn style_for_treats_unparsable_value_as_absent() {
        let props = vec![("width", "50%".to_string())];

        let style = style_for(&props);

        assert_eq!(style.size.width, Dimension::auto());
    }
}