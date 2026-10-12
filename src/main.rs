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


// ============================================================
// Rust <-> C++ Qt bridge
// ============================================================

#[cxx::bridge]
mod ffi {
    unsafe extern "C++" {
        include!(
            "test_dioxus_qt/cpp/qt_host.h"
        );

        type QtHost;

        fn new_qt_host()
            -> UniquePtr<QtHost>;

        fn show(
            self: Pin<&mut QtHost>
        );

        fn create_text(
            self: Pin<&mut QtHost>,
            text: &str,
        ) -> u64;

        fn create_rectangle(
            self: Pin<&mut QtHost>,
        ) -> u64;

        fn mount(
            self: Pin<&mut QtHost>,
            parent_id: u64,
            child_id: u64,
        );

        fn stack_before(
            self: Pin<&mut QtHost>,
            child_id: u64,
            sibling_id: u64,
        );

        fn set_property(
            self: Pin<&mut QtHost>,
            id: u64,
            name: &str,
            value: &str,
        );
    }
}


// ============================================================
// Dioxus 侧的原语：Text
// ============================================================
//
// rsx! 的标签名规则（在 dioxus-rsx/src/node.rs）：
//
//     text { }     小写单个标识符            -> 元素
//     qt-text { }  标识符后面跟 `-`          -> 元素
//     Text { }     首字母大写                -> 组件
//
// 也就是说 "Text" 这种写法在 Dioxus 眼里一定是组件，
// 所以它必须是一个作用域里真实存在的函数。
//
// 这个组件自己不认识 Qt，它只把 props 翻译成一个自定义元素；
// 真正创建 QtQuick.Text 的是下面的 renderer。
// ============================================================

#[component]
fn Text(
    text: String,
    children: Element,
) -> Element {
    rsx! {
        qt-text {
            text: "{text}",
            {children}
        }
    }
}


#[component]
fn Rectangle() -> Element {
    rsx! {
        qt-rectangle {
        }
    }
}


// ============================================================
// 用户的 Dioxus UI
// ============================================================

fn app() -> Element {
    // 注意：
    //
    // 没有 main.qml。
    //
    // 屏幕最终出现的这句话，
    // 来源就是这个 Dioxus text node。
    rsx! {
        Text {
            text: "Hello from Dioxus -> Qt Quick >>>>>",
            Rectangle {}
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
// 根节点的 object 是 Some(0)：0 在 C++ 那边表示「窗口根」。

#[derive(Default)]
struct Node {
    object: Option<u64>,

    children: Vec<u64>,
}


struct QtRenderer {
    host: UniquePtr<ffi::QtHost>,


    // 节点 id -> 节点
    //
    // 注意这不是 Qt 的 object id：
    // 占位节点没有 Qt 对象，但树里得有它的位置，
    // 而且 mutation 里的路径是按「第几个子节点」走的，
    // 所以要自己维护父子关系。
    nodes: HashMap<u64, Node>,


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
    stack: Vec<u64>,


    // Dioxus:
    //
    // ElementId
    //
    //      ↓
    //
    // 我们树里的节点 id
    //
    element_to_node: Vec<Option<u64>>,
}


impl QtRenderer {
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


        // renderer root：没有真的 Qt 对象，
        // 但它的目标就是窗口根，
        // 所以直接按 C++ 那边的约定记成 0。
        nodes.insert(
            0,
            Node {
                object: Some(0),
                children: Vec::new(),
            },
        );


        Self {
            host,

            nodes,

            next_node: 1,

            // Dioxus renderer root
            stack: vec![0],

            // ElementId(0) == renderer root
            element_to_node: vec![Some(0)],
        }
    }


    fn new_node(
        &mut self,
        object: Option<u64>,
    ) -> u64 {
        let id = self.next_node;

        self.next_node += 1;


        self.nodes.insert(
            id,
            Node {
                object,
                children: Vec::new(),
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
    fn object_of(
        &self,
        id: ElementId,
    ) -> Option<u64> {
        self.object_of_node(self.lookup(id)?)
    }


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
    // Dioxus 的模板是一棵静态树，节点只有三种：
    //
    //     Text { }        静态文本
    //     Element { }     元素
    //     Dynamic { }     动态位置（组件、if/for、children）
    //
    // 静态部分在这里一次性建好；
    // 动态位置先立一个占位节点，之后被顶掉。
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


                // 模板里写死的属性（text: "字面量"）此刻就能应用；
                // 动态属性（text: "{expr}"）Dioxus 稍后会通过
                // set_attribute 送过来。
                for attr in attrs.iter() {
                    if let TemplateAttribute::Static {
                        name,
                        value,
                        ..
                    } = attr
                    {
                        self.host
                            .pin_mut()
                            .set_property(
                                object,
                                name,
                                value,
                            );
                    }
                }


                let element_node =
                    self.new_node(Some(object));


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
                let object =
                    self.host
                        .pin_mut()
                        .create_text(text);


                self.new_node(Some(object))
            }


            // 动态位置：先占个坑，
            // 之后 ReplacePlaceholder 会拿真节点顶掉它。
            TemplateNode::Dynamic {
                ..
            } => self.new_node(None),
        }
    }


    // Dioxus 的元素名 -> Qt 对象。
    //
    // 这就是「自定义内置控件」的落点：
    // 加一个新控件 = 这里加一个 tag，
    // 加上 C++ 那边一个 create_*。
    fn create_element(
        &mut self,
        tag: &str,
    ) -> u64 {
        match tag {
            "qt-text" => self
                .host
                .pin_mut()
                .create_text(""),

            "qt-rectangle" => self
                .host
                .pin_mut()
                .create_rectangle(),

            other => panic!(
                "QtRenderer 不认识元素 <{other}>"
            ),
        }
    }


    // 把 child 挂到 parent 下面。
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
    }


    // 路径 -> (父节点, 在父节点里的下标)
    //
    // 路径是「第几个子节点」的序列。
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
// Dioxus -> Qt
// ============================================================

impl WriteMutations for QtRenderer {
    // ========================================================
    // 最重要 #1
    // ========================================================

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

        let node =
            self.create_template_node(root);


        self.bind(id, node);


        self.stack.push(node);


        println!(
            "[Dioxus] LoadTemplate \
             index={index} id={} -> node {node}",
            id.0
        );
    }

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
            self.host
                .pin_mut()
                .set_property(
                    object,
                    "text",
                    value,
                );
        }
    }


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
    // 因为我们的 app() 只有：
    //
    //     一个 static Text
    //
    // 下一版再真正实现。
    // ========================================================

    fn assign_node_id(
        &mut self,
        _path: &'static [u8],
        _id: ElementId,
    ) {
    }


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
        let next_sibling =
            self.node(parent)
                .children
                .iter()
                .skip(slot + 1)
                .find_map(|node| {
                    self.object_of_node(*node)
                });


        if let Some(parent_object) =
            self.object_of_node(parent)
        {
            for node in new_nodes.iter() {
                let Some(child_object) =
                    self.object_of_node(*node)
                else {
                    // 占位节点没有 Qt 对象，不用挂。
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


            children.splice(
                slot..slot + 1,
                new_nodes.clone(),
            );


            placeholder
        };


        self.nodes.remove(&placeholder);
        self.forget_element_ids(placeholder);


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


        // 这一版只处理字符串值。
        //
        // Int / Float / Bool 也会从 Dioxus 过来，
        // 但那要在 C++ 侧包成对应类型的 QVariant 才有意义，
        // 例如 x / y（int）、color（QColor）。

        let AttributeValue::Text(text) = value
        else {
            println!(
                "[renderer] 暂不支持非字符串属性 \
                 {name}={value:?}"
            );

            return;
        };


        println!(
            "[Dioxus] SetAttribute \
             id={} {name}={text:?}",
            id.0
        );


        self.host
            .pin_mut()
            .set_property(
                object,
                name,
                text,
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

fn main() {
    // --------------------------------------------------------
    // 1. Qt application
    // --------------------------------------------------------

    let mut qt_app =
        QGuiApplication::new();


    // --------------------------------------------------------
    // 2. 我们自己的 renderer
    // --------------------------------------------------------

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
    // --------------------------------------------------------

    vdom.rebuild(
        &mut renderer
    );


    // --------------------------------------------------------
    // 5. Qt 接管主循环
    // --------------------------------------------------------

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