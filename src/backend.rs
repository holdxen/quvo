use core::pin::Pin;

use cxx_qt::CxxQtType;
use cxx_qt_lib::QString;

use dioxus::dioxus_core::{
    AttributeValue,
    ElementId,
    Event as DioxusEvent,
    Template,
    VirtualDom,
    WriteMutations,
};

use dioxus::events::MouseData;
use dioxus_html::events::SerializedMouseData;

use dioxus::prelude::*;

use std::any::Any;
use std::rc::Rc;


// ============================================================
// Qt <-> Rust bridge
// ============================================================

#[cxx_qt::bridge]
pub mod qobject {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");

        type QString = cxx_qt_lib::QString;
    }

    extern "RustQt" {
        // ----------------------------------------------------
        // 这是一个真正的 QObject。
        //
        // QML 可以：
        //
        // DioxusBackend {
        //     id: backend
        // }
        //
        // ----------------------------------------------------

        #[qobject]
        #[qml_element]
        #[qproperty(QString, text)]
        type DioxusBackend = super::DioxusBackendRust;

        // QML:
        //
        // backend.click()
        //
        #[qinvokable]
        fn click(self: Pin<&mut Self>);
    }
}


// ============================================================
// Dioxus App
// ============================================================
//
// 注意：
// 这里完全不知道 Qt 存在。
//
// 这就是普通 Dioxus component。
// ============================================================

fn app() -> Element {
    let mut count = use_signal(|| 0_i32);

    rsx! {
        button {
            onclick: move |_| {
                count += 1;
            },

            ">>>>>>{count}"
        }
    }
}


// ============================================================
// 我们的超迷你 Qt renderer
// ============================================================
//
// 正式版本以后会变成:
//
// QtRenderer
// ├── nodes: HashMap<NodeId, QtNode>
// ├── element_to_node
// ├── stack
// ├── templates
// └── ...
//
// 但现在我们只有一个 Text，
// 所以只保存:
//
// text
// text ElementId
// onclick ElementId
//
// ============================================================

#[derive(Default)]
struct MiniQtRenderer {
    // 当前 Dioxus text node 的值。
    text: String,

    // 谁是这个 text node？
    text_id: Option<ElementId>,

    // Dioxus 告诉 renderer：
    //
    // ElementId(x) 上有 click listener。
    //
    click_listener: Option<(
        &'static str,
        ElementId,
    )>,
}


// ============================================================
// Dioxus renderer interface
// ============================================================

impl WriteMutations for MiniQtRenderer {
    // --------------------------------------------------------
    // 第一版不维护真正的树，
    // 所以这些 mutation 先 ignore。
    // --------------------------------------------------------

    fn append_children(
        &mut self,
        _id: ElementId,
        _m: usize,
    ) {
    }


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


    // ========================================================
    // 重要
    // ========================================================
    //
    // 初次 render：
    //
    // "{count}"
    //
    // 会创建 dynamic text node。
    //
    // Dioxus:
    //
    // CreateTextNode {
    //     value: "0",
    //     id: ...
    // }
    //
    // ========================================================

    fn create_text_node(
        &mut self,
        value: &str,
        id: ElementId,
    ) {
        println!(
            "[QtRenderer] create_text_node \
             id={} value={value:?}",
            id.0
        );

        self.text.clear();
        self.text.push_str(value);

        self.text_id = Some(id);
    }


    fn load_template(
        &mut self,
        _template: Template,
        _index: usize,
        _id: ElementId,
    ) {
        // 正式 renderer：
        //
        // TemplateNode::Element
        //        ↓
        // QQuickItem / QObject
        //
        // 第一版先不需要。
    }


    fn replace_node_with(
        &mut self,
        _id: ElementId,
        _m: usize,
    ) {
    }


    fn replace_placeholder_with_nodes(
        &mut self,
        _path: &'static [u8],
        _m: usize,
    ) {
    }


    fn insert_nodes_after(
        &mut self,
        _id: ElementId,
        _m: usize,
    ) {
    }


    fn insert_nodes_before(
        &mut self,
        _id: ElementId,
        _m: usize,
    ) {
    }


    fn set_attribute(
        &mut self,
        _name: &'static str,
        _ns: Option<&'static str>,
        _value: &AttributeValue,
        _id: ElementId,
    ) {
        // 下一版这里会变成：
        //
        // QObject::setProperty(...)
        //
        // 例如：
        //
        // width
        // height
        // color
        // visible
    }


    // ========================================================
    // 第二个最重要的 mutation
    // ========================================================
    //
    // count:
    //
    // 0
    // ↓ click
    // 1
    //
    // Dioxus 不需要重新创建 UI。
    //
    // 只会告诉 renderer：
    //
    // SetText {
    //     id: text_id,
    //     value: "1"
    // }
    //
    // ========================================================

    fn set_node_text(
        &mut self,
        value: &str,
        id: ElementId,
    ) {
        println!(
            "[QtRenderer] set_node_text \
             id={} value={value:?}",
            id.0
        );

        if self.text_id == Some(id) {
            self.text.clear();
            self.text.push_str(value);
        }
    }


    // ========================================================
    // 第三个最重要的 mutation
    // ========================================================
    //
    // Dioxus:
    //
    // button {
    //     onclick: ...
    // }
    //
    // 会告诉 renderer：
    //
    // ElementId(x)
    //     有一个 click listener
    //
    // ========================================================

    fn create_event_listener(
        &mut self,
        name: &'static str,
        id: ElementId,
    ) {
        println!(
            "[QtRenderer] create_event_listener \
             id={} event={name}",
            id.0
        );

        self.click_listener =
            Some((name, id));
    }


    fn remove_event_listener(
        &mut self,
        name: &'static str,
        id: ElementId,
    ) {
        if self.click_listener == Some((name, id)) {
            self.click_listener = None;
        }
    }


    fn remove_node(
        &mut self,
        id: ElementId,
    ) {
        if self.text_id == Some(id) {
            self.text_id = None;
            self.text.clear();
        }

        if self
            .click_listener
            .map(|(_, listener_id)| listener_id)
            == Some(id)
        {
            self.click_listener = None;
        }
    }


    fn push_root(
        &mut self,
        _id: ElementId,
    ) {
    }
}


// ============================================================
// QObject 内部真正的 Rust state
// ============================================================

pub struct DioxusBackendRust {
    // --------------------------------------------------------
    // Q_PROPERTY
    //
    // QML 可以直接：
    //
    // Text {
    //     text: backend.text
    // }
    //
    // --------------------------------------------------------

    text: QString,


    // --------------------------------------------------------
    // Dioxus runtime
    // --------------------------------------------------------

    vdom: VirtualDom,


    // --------------------------------------------------------
    // 我们自己的 renderer
    // --------------------------------------------------------

    renderer: MiniQtRenderer,
}


// ============================================================
// QML:
//
// DioxusBackend {}
//
// 创建的时候会走这里。
// ============================================================

impl Default for DioxusBackendRust {
    fn default() -> Self {
        println!("=== create Dioxus VirtualDom ===");

        let mut vdom =
            VirtualDom::new(app);

        let mut renderer =
            MiniQtRenderer::default();


        // ====================================================
        // 第一次 Dioxus render
        // ====================================================
        //
        // Component
        //    ↓
        // VirtualDom
        //    ↓
        // WriteMutations
        //    ↓
        // MiniQtRenderer
        //
        // rebuild 不会卡住。
        //
        // ====================================================

        vdom.rebuild(&mut renderer);


        println!(
            "initial Dioxus text = {:?}",
            renderer.text
        );


        Self {
            text: QString::from(
                renderer.text.as_str()
            ),

            vdom,

            renderer,
        }
    }
}


// ============================================================
// Qt → Dioxus
// ============================================================

impl qobject::DioxusBackend {
    pub fn click(
        mut self: Pin<&mut Self>
    ) {
        println!("=== Qt clicked ===");


        // ----------------------------------------------------
        // 先进入 QObject 内部 Rust state。
        // ----------------------------------------------------

        let next_text = {
            let mut rust =
                self.as_mut().rust_mut();

            let state =
                rust.as_mut().get_mut();


            // ------------------------------------------------
            // renderer 在 rebuild 时已经记录：
            //
            // 某个 ElementId 有 onclick。
            // ------------------------------------------------

            let Some((
                event_name,
                element_id,
            )) = state.renderer.click_listener
            else {
                println!(
                    "no Dioxus click listener"
                );

                return;
            };


            println!(
                "dispatch event={event_name} \
                 to ElementId({})",
                element_id.0
            );


            // =================================================
            // QML mouse event
            //      ↓
            // Dioxus MouseData
            // =================================================

            let mouse =
                MouseData::new(
                    SerializedMouseData::default()
                );


            let data: Rc<dyn Any> =
                Rc::new(mouse);


            let event =
                DioxusEvent::new(
                    data,
                    true,
                );


            // =================================================
            // 真正把 Qt event 送回 Dioxus。
            // =================================================

            state
                .vdom
                .runtime()
                .handle_event(
                    event_name,
                    event,
                    element_id,
                );


            // =================================================
            // onclick:
            //
            // count += 1
            //
            // Signal dirty
            //
            // 现在让 Dioxus diff。
            //
            // diff 结果重新写到：
            //
            // MiniQtRenderer::set_node_text()
            //
            // =================================================

            state
                .vdom
                .render_immediate(
                    &mut state.renderer
                );


            println!(
                "new Dioxus text = {:?}",
                state.renderer.text
            );


            QString::from(
                state.renderer.text.as_str()
            )
        };


        // ====================================================
        // Dioxus renderer
        //      ↓
        // Qt Q_PROPERTY
        //      ↓
        // QML binding 自动更新
        // ====================================================

        self.set_text(next_text);
    }
}