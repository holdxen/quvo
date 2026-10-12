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

        fn append_to_root(
            self: Pin<&mut QtHost>,
            id: u64,
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
fn Text(text: String) -> Element {
    rsx! {
        qt-text {
            text: "{text}",
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
        }
        Rectangle {  }
    }
}


// ============================================================
// 最小 Qt renderer
// ============================================================

struct QtRenderer {
    host: UniquePtr<ffi::QtHost>,


    // Dioxus 的 mutation stack。
    //
    // 0 是我们自己定义的 root sentinel。
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
    // 我们 QtHost 的 object id
    //
    objects: Vec<Option<u64>>,
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


        Self {
            host,

            // Dioxus renderer root
            stack: vec![0],

            // ElementId(0) == renderer root
            objects: vec![Some(0)],
        }
    }


    fn bind(
        &mut self,
        id: ElementId,
        object: u64,
    ) {
        if self.objects.len() <= id.0 {
            self.objects.resize(
                id.0 + 1,
                None,
            );
        }


        self.objects[id.0] =
            Some(object);
    }


    fn lookup(
        &self,
        id: ElementId,
    ) -> Option<u64> {
        self.objects
            .get(id.0)
            .copied()
            .flatten()
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
    // 从 Dioxus Template 里面找静态 Text。
    //
    // 这一版只支持一个文本，所以故意做得极简。
    // --------------------------------------------------------

    fn template_text(
        node: &TemplateNode,
    ) -> Option<&'static str> {
        match node {
            TemplateNode::Text {
                text
            } => {
                Some(*text)
            }


            TemplateNode::Element {
                children,
                ..
            } => {
                children
                    .iter()
                    .find_map(
                        Self::template_text
                    )
            }


            TemplateNode::Dynamic {
                ..
            } => {
                None
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


        // Dioxus 送来的模板告诉我们：
        //
        //     这个节点在 Qt 里应该长什么样
        //
        // 组件的边界在这里是「透明」的：
        // rsx! { Text { text: "..." } } 送过来的模板
        // 已经是组件内部那个 <qt-text> 元素，
        // 所以 renderer 完全不需要知道 Text 是个组件。
        //
        // 真正创建 QtQuick.Text 的就是下面这一段。

        let object = match root {
            // --------------------------------
            // 元素：<qt-text text="..." />
            // --------------------------------

            TemplateNode::Element {
                tag,
                attrs,
                ..
            } => {
                // 元素里写死的文本子节点，例如
                //
                //     qt-text { "hello" }
                //
                // 当作初始 text。
                // 写在属性上的值后面会覆盖它。
                let initial =
                    Self::template_text(root)
                        .unwrap_or_default();


                let object = match *tag {
                    "qt-text" => self
                        .host
                        .pin_mut()
                        .create_text(initial),

                    "qt-rectangle" => {
                        self.host.pin_mut().create_rectangle()
                    },
                    other => panic!(
                        "QtRenderer 不认识元素 \
                         <{other}>，目前只有 qt-text"
                    ),
                };


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


                println!(
                    "[Dioxus] LoadTemplate \
                     id={} <{tag}>",
                    id.0
                );


                object
            }


            // --------------------------------
            // 裸文本节点：rsx! { "hello" }
            // --------------------------------

            TemplateNode::Text {
                ..
            } => {
                let text =
                    Self::template_text(root)
                        .unwrap_or_default();


                println!(
                    "[Dioxus] LoadTemplate \
                     id={} text={text:?}",
                    id.0
                );


                self.host
                    .pin_mut()
                    .create_text(text)
            }


            // --------------------------------
            // 动态根节点：组件嵌在元素里、if/for 之类
            //
            // 这些需要 placeholder + 路径寻址，
            // 这一版还没实现。
            // --------------------------------

            TemplateNode::Dynamic {
                ..
            } => {
                panic!(
                    "模板根节点是动态节点，\
                     需要实现 create_placeholder / \
                     replace_placeholder_with_nodes"
                );
            }
        };


        assert_ne!(
            object,
            0,
            "Qt failed to create Text"
        );


        self.bind(
            id,
            object,
        );


        // 新节点还处于 detached 状态。
        //
        // stack:
        //
        // [ROOT]
        //
        //      ↓
        //
        // [ROOT, TEXT]
        //
        self.stack.push(object);


        println!(
            "[renderer] ElementId({}) \
             -> QtObject({})",
            id.0,
            object
        );
    }


    // ========================================================
    // 最重要 #2
    // ========================================================

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


        // 这一版只支持挂到 renderer root。
        assert_eq!(
            id,
            ElementId(0),
            "demo only supports root parent"
        );


        for child in children {
            if child == 0 {
                continue;
            }


            // ============================================
            // 到这一刻 QtQuick.Text 才真正进入 visual tree。
            // ============================================

            self.host
                .pin_mut()
                .append_to_root(child);
        }
    }


    // ========================================================
    // 动态 text 后面会使用这个。
    // ========================================================

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


        assert_ne!(
            object,
            0
        );


        self.bind(
            id,
            object,
        );


        self.stack.push(object);
    }


    // ========================================================
    // Signal 更新 text 时以后会进这里。
    // ========================================================

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
            self.lookup(id)
        {
            if object != 0 {
                self.host
                    .pin_mut()
                    .set_property(
                        object,
                        "text",
                        value,
                    );
            }
        }
    }


    fn push_root(
        &mut self,
        id: ElementId,
    ) {
        let object =
            self.lookup(id)
                .unwrap_or_else(|| {
                    panic!(
                        "unknown ElementId({})",
                        id.0
                    )
                });


        self.stack.push(object);
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
        _id: ElementId,
    ) {
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
        _path: &'static [u8],
        _m: usize,
    ) {
        panic!(
            "replace_placeholder_with_nodes \
             not implemented"
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
            self.lookup(id)
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