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
//! ## id 有四层，别搞混
//!
//! ```text
//!  ElementId        Dioxus 的编号，只在 root/动态节点上有；
//!                   模板深处的静态元素默认没有 id（要 AssignId 才有）
//!      ↓  element_to_node
//!  node id          本 renderer 的树节点编号；占位节点也有，
//!                   因为 Dioxus 的 path 数是它在模板里的位置
//!      ↓  Node::object
//!  object id        C++ 那边 QtHost::Impl::objects 的 key，0 = 窗口根
//!      ↓  objects
//!  QObject*         真正的 QtQuick 对象
//! ```
//!
//!
//! ## 阅读顺序建议
//!
//! 1. `app()` —— 用户写的 UI 就三行
//! 2. `mod ffi` —— Rust 能让 C++ 做的全部六件事
//! 3. `QtRenderer` 的三个字段 —— renderer 记了哪三本账
//! 4. `impl WriteMutations` —— 指令如何变成 Qt 调用（核心）
//! 5. `main()` —— 拼起来的顺序
//!
//!
//! ## 当前边界（重要）
//!
//! 只跑通了 mount（首次渲染）这一条路，diff（更新）那条路是断的：
//!
//! - 更新路径要用的 `replace_node_with` / `remove_node` /
//!   `insert_nodes_after` / `insert_nodes_before` 在本文件里还是 panic；
//! - `main()` 里没有把 Dioxus 的异步 runtime 接到 Qt 的事件循环上，
//!   signal 变了没人去唤醒 VirtualDom。
//!
//! 所以现在是「渲染一次就定住」。详见 `main()` 上方的注释。

use cxx::UniquePtr;

use cxx_qt_lib::QGuiApplication;

use dioxus::dioxus_core::{
    AttributeValue,
    ElementId,
    Template,
    TemplateAttribute,
    TemplateNode,
    VirtualDom,
    WriteMutations,
};

use dioxus::prelude::*;

use std::collections::HashMap;

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
// 这六个函数就是 renderer 能对 Qt 做的全部事情。
// 一次完整的首次渲染，调用顺序是：
//
//     new_qt_host()          建 QtHost：内部有 QQuickWindow + QQmlEngine
//     show()                 窗口先显示，之后边建边能看见
//
//     create_text /
//     create_rectangle       每碰到一个元素就实例化一个 Qt 对象；
//                            此刻它还没有 parent，等于屏幕上不存在
//     set_property()         写属性。静态属性在建对象时就写，
//                            动态属性由 Dioxus 稍后补发
//     mount()                挂进 visual tree —— 到这一步才真的可见
//     stack_before()         调同一 parent 下的叠放顺序
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

        // 实例化一个 QtQuick.Text。
        // 返回值是 C++ 侧分配的 object id（0 表示创建失败）。
        //
        // 两个地方会调它：
        //   - 模板里的静态文本（TemplateNode::Text）
        //   - 动态文本 / <qt-text> 元素本身
        fn create_text(
            self: Pin<&mut QtHost>,
            text: &str,
        ) -> u64;

        // 实例化一个 QtQuick.Rectangle。
        // 没有属性需要从 Rust 传：颜色是画的事，
        // 而宽高是布局的事，走 set_geometry。
        fn create_rectangle(
            self: Pin<&mut QtHost>,
        ) -> u64;

        // 实例化一个裸 Item 当容器（qt-column）。
        // QML 侧不带 Layout / anchors —— 孩子摆哪里全由 taffy 决定。
        fn create_column(
            self: Pin<&mut QtHost>,
        ) -> u64;

        // 把 child 挂到 parent 下面，正式进 visual tree。
        //
        // parent_id == 0 是约定好的「窗口根」，
        // C++ 那边翻译成 window.contentItem()。
        fn mount(
            self: Pin<&mut QtHost>,
            parent_id: u64,
            child_id: u64,
        );

        // 让 child 在叠放顺序上排到 sibling 前面。
        //
        // 为什么需要：mount 只能往末尾追加，
        // 而 ReplacePlaceholder 要把新节点插回坑位原来的位置，
        // 所以挂完还要再往前挪一下。
        fn stack_before(
            self: Pin<&mut QtHost>,
            child_id: u64,
            sibling_id: u64,
        );

        // QObject::setProperty(name, QVariant( QString(value)) )。
        //
        // 属性名直接对上 Qt 的属性名：
        // Dioxus 侧写 text: "..."，这里就落到 QtQuick.Text::text。
        // 写不进去的（类型不匹配，比如 color 要 QColor、
        // font.pixelSize 要 int）会在 C++ 侧 qWarning。
        fn set_property(
            self: Pin<&mut QtHost>,
            id: u64,
            name: &str,
            value: &QVariant,
        );

        // 布局结果写给 Qt：几何的唯一入口。
        //
        // 单独开一个函数而不是走 set_property，是为了让「几何」
        // 有一个明确的出口 —— 五个数字一次写完，也省掉属性名查找。
        fn set_geometry(
            self: Pin<&mut QtHost>,
            id: u64,
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
        fn implicit_width(
            self: &QtHost,
            id: u64,
        ) -> f64;

        fn implicit_height(
            self: &QtHost,
            id: u64,
        ) -> f64;
    }
}


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
//    （Text 和 Rectangle 都是组件调用 → 各是一个 Dynamic 节点）：
//
//        node_paths = [ [0,0], [0,1] ]
//
//    所以 ReplacePlaceholder 会被调用两次，而且 Dioxus 是
//    倒着发的（先 [0,1] 再 [0,0]）—— 正好是那条不变式的活教材。

fn app() -> Element {
    // 没有 main.qml，也没有 QML 的 Layout / anchors。
    rsx! {
        qt-column {
            width: "600",
            height: "180",
            flex_direction: "column",
            gap: "12",

            Text {
                text: "Hello from Dioxus -> Qt Quick (taffy layout)",
            }

            Rectangle {
                width: 200.0,
                height: 40.0,
            }
        }
    }
}


// ============================================================
// 最小 Qt renderer
// ============================================================

// 树里的一个节点。
//
//     object = Some(id)   已经在 Qt 里实体化了的对象
//     object = None       占位节点（模板里的动态位置），
//                         树里有它，Qt 里还没有东西，
//                         等着被 ReplacePlaceholder 顶掉
//
// children 的顺序就是 Dioxus 模板里的顺序，占位节点也占一格 ——
// resolve_slot 拿 path 的下标直接索引它，顺序错一个就全错。
//
// 根节点的 object 是 Some(0)：0 在 C++ 那边表示「窗口根」。

#[derive(Default)]
struct Node {
    object: Option<u64>,

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


// renderer 的全部状态就是这四个字段。
//
// 本质工作是在翻译两套编号：
//
//     Dioxus 只会说  ElementId / 模板里的 path
//     Qt   只认     object id
//
// 所以中间必须有一棵树把两者接起来，而且要维持住 Dioxus 期望的
// 「第几个孩子」的顺序 —— 哪怕那个位置上的东西在 Qt 里根本不存在。
//
//     nodes            node id -> Node（它有没有 Qt 对象 + 孩子顺序）
//                      这是「按 Dioxus 的视角」建的树，占位节点也算一格
//     element_to_node  ElementId -> node id
//                      所有按 id 寻址的 mutation 都靠它落地
//     stack            Dioxus 约定的临时栈，见 WriteMutations 的注释
//     host             唯一能碰 Qt 的入口
//
// C++ 那边还有第四本账：QtHost::Impl::objects（object id -> QObject*），
// Rust 侧看不到它，也不需要看到 —— Rust 只握着 id。
struct QtRenderer {
    host: UniquePtr<ffi::QtHost>,


    // 节点 id -> 节点
    //
    // 注意这不是 Qt 的 object id：
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
    // 它没有 Qt 对象可写几何（0 是约定的「窗口根」），
    // 只有孩子会被 apply。
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


        // renderer root：没有真的 Qt 对象，
        // 但它的目标就是窗口根，
        // 所以直接按 C++ 那边的约定记成 0。
        //
        // 它的样式也走「属性账本」这一条路，
        // 这样 style_for 就是全项目唯一的样式来源。
        nodes.insert(
            0,
            Node {
                object: Some(0),
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
        object: Option<u64>,
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


    fn object_of_node(
        &self,
        id: u64,
    ) -> Option<u64> {
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
    ) -> Option<u64> {
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
                     {element_node} (QtObject {object})"
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
    ) -> u64 {
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


        // C++ 那边实例化失败也是返回 0，而 0 在本项目里是
        // 「窗口根」这个约定值 —— 不拦的话一个建失败的节点
        // 会被当成窗口根去挂载、去写几何，症状会非常难读。
        // 真正的错误信息（QML 报错）已经由 qWarning 打出去了。
        assert_ne!(
            object, 0,
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

            // object 0 是「窗口根」那个约定值，
            // C++ 的 objects 表里没有它，写进去只会刷一条警告。
            let object = self
                .node(child)
                .object
                .filter(|id| *id != 0);

            if let Some(object) = object {
                self.host
                    .pin_mut()
                    .set_geometry(object, x, y, w, h);
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
                self.node(id).object.filter(|o| *o != 0)
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

            let implicit_w = self.host.implicit_width(object);
            let implicit_h = self.host.implicit_height(object);

            println!(
                "[measure] node {id} obj {object} \
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
            self.host
                .pin_mut()
                .mount(
                    parent_object,
                    child_object,
                );
        }


        // 布局树跟着同步。
        self.sync_taffy_children(parent);
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
            self.host
                .pin_mut()
                .set_property(
                    object,
                    "text",
                    &value,
                );
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


    fn replace_node_with(
        &mut self,
        _id: ElementId,
        _m: usize,
    ) {
        panic!(
            "replace_node_with not implemented"
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


    fn insert_nodes_after(
        &mut self,
        _id: ElementId,
        _m: usize,
    ) {
        panic!(
            "insert_nodes_after not implemented"
        );
    }


    fn insert_nodes_before(
        &mut self,
        _id: ElementId,
        _m: usize,
    ) {
        panic!(
            "insert_nodes_before not implemented"
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

        self.host
            .pin_mut()
            .set_property(
                object,
                name,
                &value,
            );
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


    fn remove_node(
        &mut self,
        _id: ElementId,
    ) {
        panic!(
            "remove_node not implemented"
        );
    }
}


// ============================================================
// Main
// ============================================================
//
// 现在这个 main 只做了一次 mount，之后就交给 Qt 了。
// 想让它「活」起来，缺的是第 5 步和第 6 步之间那条回路。
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
    // 5. Qt 接管主循环
    // --------------------------------------------------------
    //
    // ⚠️ 到这里就结束了：后面没有任何东西再去碰 vdom。
    //
    // 所以现在是「一次性渲染」：
    //     - app() 里没有 signal，UI 本来也不会变；
    //     - 就算加了 signal，也没人 poll VirtualDom；
    //     - 就算 poll 了，diff 发出的 ReplaceWith / Remove /
    //       InsertAfter 在本文件里还是 panic。
    //
    // 要接上的是这三件事，都在 Qt 的事件循环里做：
    //
    // 1) 拿一个 executor 去 poll vdom.wait_for_work()
    //    （async，virtual_dom.rs:445；它内部会 process_events，
    //      有脏 scope 就返回）。
    //    常见做法是 QAbstractEventDispatcher::processEvents 的
    //    前后挂钩子，或者干脆一个 QTimer 起来 poll 一次，
    //    千万别 block_on 把它卡死主线程。
    //
    // 2) 它返回之后调 vdom.render_immediate(&mut renderer)
    //    （virtual_dom.rs:599）走 diff，
    //    前提是把上面 panic 的那几个 WriteMutations 实现掉。
    //
    // 3) 把 Qt 的鼠标/键盘事件转成 Dioxus 的事件送进去：
    //    vdom.handle_event(name, Rc<dyn Any>, ElementId, bubbling)
    //    （virtual_dom.rs:765），
    //    送之前得先进 runtime（vdom.runtime()，virtual_dom.rs:759），
    //    不然 signal 读写找不到上下文。
    //    这也是 create_event_listener / remove_event_listener
    //    那两个空实现该被填的地方。
    //
    // 另外 Cargo.toml 里 dioxus-signals 已经引了但还没用上，
    // 第一个试验可以从 app() 里加一个 signal + 一个按钮开始。

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