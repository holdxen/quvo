// 只 include 生成头，不要同时 include 源码树里的 "qt_host.h"：
// cxx-qt-build 会把头文件复制到 target 下，两份路径不同、`#pragma once`
// 认不出来，于是 QtHost / TextMeasure 都会被定义两遍。
// 生成头本身就会带上复制过来的 qt_host.h，而且它才有 bridge
// shared struct（TextMeasure）的完整定义。
#include "test_dioxus_qt/src/main.cxx.h"

#include <QtCore/QByteArray>
#include <QtCore/QDebug>
#include <QtCore/QEvent>
#include <QtCore/QMetaObject>
#include <QtCore/QObject>
#include <QtCore/QString>
#include <QtCore/QUrl>
#include <QtCore/QVariant>


#include <QtGui/QColor>
#include <QtGui/QEventPoint>
#include <QtGui/QFont>
#include <QtGui/QMouseEvent>
#include <QtGui/QTextLayout>
#include <QtGui/QTextOption>
#include <QtGui/QGuiApplication>

#include <QtQml/QQmlComponent>
#include <QtQml/QQmlEngine>

#include <QtQuick/QQuickItem>
#include <QtQuick/QQuickWindow>

// FLT_MAX —— QQuickText 不限宽时用的也是它（qquicktext.cpp:834）。
#include <cfloat>
#include <cstdio>
#include <utility>

#include <algorithm>

const QString TEXT_ITEM_NAME = QStringLiteral("qt_quick_text_item");

namespace {

QString to_qstring(rust::Str text)
{
    return QString::fromUtf8(
        text.data(),
        static_cast<qsizetype>(text.size())
    );
}

// 共用：把一段内联 QML 实例化成 QQuickItem 并把指针交出去。
//
// 返回 nullptr 表示失败。此刻对象还没有 parent，也就还不属于任何人 ——
// 是 Rust 侧随后的 mount 把它送进 Qt 的 parent tree，从此由那条树负责释放。
QQuickItem* instantiate(
    QQmlEngine& engine,
    const char* qml,
    const char* label
)
{
    QQmlComponent component(&engine);

    component.setData(
        QByteArray(qml),
        QUrl()
    );

    if (component.isError()) {
        for (const auto& error : component.errors()) {
            qWarning().noquote() << error.toString();
        }

        return nullptr;
    }

    QObject* object = component.create();

    if (object == nullptr) {
        qWarning()
            << "QQmlComponent::create failed" << label;

        return nullptr;
    }

    auto* item = qobject_cast<QQuickItem*>(object);

    if (item == nullptr) {
        qWarning()
            << "created object is not QQuickItem" << label;

        delete object;

        return nullptr;
    }

    qDebug()
        << "[Qt] create" << label
        << "at" << item
        << "but NOT mounted yet";

    // 每个建出来的对象都报一声「我死了」。
    //
    // 这不是装饰：Rust 侧现在握的是裸指针，「destroy 到底有没有真的
    // 把对象释放掉」在外面是看不见的。~QObject 会先对本对象发
    // destroyed，再递归删 QObject 孩子们（qobject.cpp），
    // 所以父先子后的三行日志同时证明了「删了」和「子树跟着没了」。
    QObject::connect(
        item,
        &QObject::destroyed,
        [label](QObject *object)
        {
            qDebug()
                << "[Qt] object destroyed"
                << label
                << object;
        }
    );

    return item;
}

} // namespace



// ============================================================
// 事件监听：名字 → 吃什么 QEvent、送哪一支 Rust 回调
// ============================================================
//
// dioxus 每注册一个监听器就给我们一个名字（已经剥掉 on 前缀），
// 这个名字决定三件事：
//   1. 这个 item 要打开哪些输入能力（acceptedMouseButtons / acceptHoverEvents /
//      acceptTouchEvents / focusPolicy），见 refresh_item_capabilities()；
//   2. filter 里哪些 QEvent::Type 要理（types 那一列）；
//   3. 理出来送哪一支回调（payload 那一列，对应 dioxus 的 MouseData /
//      PointerData / WheelData / KeyboardData / FocusData / TouchData / ResizeData）。
//
// 表里没有的名字到不了这里：Rust 侧 create_event_listener 先拦一道并打日志。
// 「mounted」也不在这里 —— 那一支不需要 Qt 给事实（元素建好就是它），
// 由 Rust 侧直接补发。

enum class Payload {
    Mouse,     // dioxus 的 MouseData
    Pointer,   // PointerData
    Wheel,     // WheelData
    Key,       // KeyboardData
    Focus,     // FocusData
    Touch,     // TouchData
    Resize,    // ResizeData：不吃 QEvent，靠尺寸变化信号
};

struct EventSpec {
    const char* name;
    Payload payload;
    int n_types;
    QEvent::Type types[4];
    bool hover;      // 需要 setAcceptHoverEvents(true)
    bool touch;      // 需要 setAcceptTouchEvents(true)
    bool key_focus;  // 需要能拿到 active focus（按键/焦点两支的前提）
};

namespace {

// DOM 的规矩：mousedown / pointerdown 任何键都发，所以鼠标类一律把
// Left|Right|Middle 开出来（MouseArea 的默认只是 Left，这里跟 DOM，
// 因为监听器的名字本来就是 DOM 那套）。
const EventSpec EVENT_SPECS[] = {
    {"mousedown",    Payload::Mouse,   1, { QEvent::MouseButtonPress   }, false, false, false},
    {"mouseup",      Payload::Mouse,   1, { QEvent::MouseButtonRelease }, false, false, false},
    {"click",        Payload::Mouse,   1, { QEvent::MouseButtonRelease }, false, false, false},
    {"dblclick",     Payload::Mouse,   1, { QEvent::MouseButtonDblClick}, false, false, false},
    {"doubleclick",  Payload::Mouse,   1, { QEvent::MouseButtonDblClick}, false, false, false},
    {"mousemove",    Payload::Mouse,   2, { QEvent::HoverMove, QEvent::MouseMove }, true, false, false},
    {"mouseover",    Payload::Mouse,   1, { QEvent::HoverEnter }, true, false, false},
    {"mouseout",     Payload::Mouse,   1, { QEvent::HoverLeave }, true, false, false},
    {"mouseenter",   Payload::Mouse,   1, { QEvent::HoverEnter }, true, false, false},
    {"mouseleave",   Payload::Mouse,   1, { QEvent::HoverLeave }, true, false, false},
    {"contextmenu",  Payload::Mouse,   1, { QEvent::ContextMenu }, false, false, false},
    {"pointerdown",  Payload::Pointer, 1, { QEvent::MouseButtonPress   }, false, false, false},
    {"pointerup",    Payload::Pointer, 1, { QEvent::MouseButtonRelease }, false, false, false},
    {"auxclick",     Payload::Pointer, 1, { QEvent::MouseButtonRelease }, false, false, false},
    {"pointermove",  Payload::Pointer, 2, { QEvent::HoverMove, QEvent::MouseMove }, true, false, false},
    {"pointerover",  Payload::Pointer, 1, { QEvent::HoverEnter }, true, false, false},
    {"pointerout",   Payload::Pointer, 1, { QEvent::HoverLeave }, true, false, false},
    {"pointerenter", Payload::Pointer, 1, { QEvent::HoverEnter }, true, false, false},
    {"pointerleave", Payload::Pointer, 1, { QEvent::HoverLeave }, true, false, false},
    {"wheel",        Payload::Wheel,   1, { QEvent::Wheel }, true, false, false},
    {"keydown",      Payload::Key,     1, { QEvent::KeyPress },   false, false, true},
    {"keyup",        Payload::Key,     1, { QEvent::KeyRelease }, false, false, true},
    {"keypress",     Payload::Key,     1, { QEvent::KeyPress },   false, false, true},
    {"focus",        Payload::Focus,   1, { QEvent::FocusIn  }, false, false, true},
    {"blur",         Payload::Focus,   1, { QEvent::FocusOut }, false, false, true},
    {"focusin",      Payload::Focus,   1, { QEvent::FocusIn  }, false, false, true},
    {"focusout",     Payload::Focus,   1, { QEvent::FocusOut }, false, false, true},
    {"touchstart",   Payload::Touch,   1, { QEvent::TouchBegin  }, false, true, false},
    {"touchmove",    Payload::Touch,   1, { QEvent::TouchUpdate }, false, true, false},
    {"touchend",     Payload::Touch,   1, { QEvent::TouchEnd    }, false, true, false},
    {"touchcancel",  Payload::Touch,   1, { QEvent::TouchCancel }, false, true, false},
    {"resize",       Payload::Resize,  0, {}, false, false, false},
};


const EventSpec* spec_for(const std::string& name)
{
    for (const EventSpec& spec : EVENT_SPECS) {
        if (name == spec.name) {
            return &spec;
        }
    }

    return nullptr;
}


bool spec_covers(const EventSpec& spec, QEvent::Type type)
{
    for (int i = 0; i < spec.n_types; ++i) {
        if (spec.types[i] == type) {
            return true;
        }
    }

    return false;
}


bool is_click_name(const std::string& name)
{
    return name == "click" || name == "auxclick";
}


bool is_up_name(const std::string& name)
{
    return name == "mouseup" || name == "pointerup";
}


// Qt 的 Key 和 web 的 code / key 两套名字都不同源，只能自己对一张表。
// code = 物理位置（"KeyA"/"Digit1"/"ArrowLeft"…），
// key  = 这次按键产出的值（"a"/"A"/"Enter"…，web 那边命名键是小写）。
struct KeyNames {
    const char* code;
    const char* key;
};


KeyNames key_names(int qt_key, const QString& text)
{
    switch (qt_key) {
    case Qt::Key_Escape:    return {"Escape",      "escape"};
    case Qt::Key_Tab:       return {"Tab",         "tab"};
    case Qt::Key_Backtab:   return {"Tab",         "tab"};
    case Qt::Key_Backspace: return {"Backspace",   "backspace"};
    case Qt::Key_Enter:
    case Qt::Key_Return:    return {"Enter",       "enter"};
    case Qt::Key_Space:     return {"Space",       " "};
    case Qt::Key_Insert:    return {"Insert",      "insert"};
    case Qt::Key_Delete:    return {"Delete",      "delete"};
    case Qt::Key_Home:      return {"Home",        "home"};
    case Qt::Key_End:       return {"End",         "end"};
    case Qt::Key_PageUp:    return {"PageUp",      "pageup"};
    case Qt::Key_PageDown:  return {"PageDown",    "pagedown"};
    case Qt::Key_Left:      return {"ArrowLeft",   "arrowleft"};
    case Qt::Key_Right:     return {"ArrowRight",  "arrowright"};
    case Qt::Key_Up:        return {"ArrowUp",     "arrowup"};
    case Qt::Key_Down:      return {"ArrowDown",   "arrowdown"};
    case Qt::Key_Shift:     return {"ShiftLeft",   "shift"};
    case Qt::Key_Control:   return {"ControlLeft", "control"};
    case Qt::Key_Alt:       return {"AltLeft",     "alt"};
    case Qt::Key_Meta:      return {"MetaLeft",    "meta"};
    case Qt::Key_CapsLock:  return {"CapsLock",    "capslock"};
    case Qt::Key_NumLock:   return {"NumLock",     "numlock"};
    case Qt::Key_ScrollLock:return {"ScrollLock",  "scrolllock"};
    case Qt::Key_Minus:     return {"Minus",       "-"};
    case Qt::Key_Equal:     return {"Equal",       "="};
    case Qt::Key_BracketLeft:  return {"BracketLeft",  "["};
    case Qt::Key_BracketRight: return {"BracketRight", "]"};
    case Qt::Key_Semicolon: return {"Semicolon",   ";"};
    case Qt::Key_Apostrophe:return {"Quote",       "'"};
    case Qt::Key_Comma:     return {"Comma",       ","};
    case Qt::Key_Period:    return {"Period",      "."};
    case Qt::Key_Slash:     return {"Slash",       "/"};
    case Qt::Key_Backslash: return {"Backslash",   "\\"};
    case Qt::Key_QuoteLeft: return {"Backquote",   "`"};
    default:
        break;
    }

    if (qt_key >= Qt::Key_A && qt_key <= Qt::Key_Z) {
        // 字母：code 是 KeyA..KeyZ，key 用 Qt 已经算好的文本（带 shift 就是大写）。
        static char code_buf[8];
        snprintf(code_buf, sizeof(code_buf), "Key%c",
                 char('A' + (qt_key - Qt::Key_A)));
        return { code_buf, nullptr };
    }

    if (qt_key >= Qt::Key_0 && qt_key <= Qt::Key_9) {
        static char code_buf[8];
        snprintf(code_buf, sizeof(code_buf), "Digit%c",
                 char('0' + (qt_key - Qt::Key_0)));
        return { code_buf, nullptr };
    }

    if (qt_key >= Qt::Key_F1 && qt_key <= Qt::Key_F12) {
        static char code_buf[8];
        snprintf(code_buf, sizeof(code_buf), "F%d",
                 int(qt_key - Qt::Key_F1) + 1);
        return { code_buf, code_buf };
    }

    // 剩下的一律报 Unidentified（keyboard_types 的 Code 有这个兜底变体）。
    if (!text.isEmpty() && text.at(0).isPrint()) {
        return { "Unidentified", nullptr };
    }

    return { "Unidentified", "unidentified" };
}

} // namespace


// 一个元素上的一个监听器 = 一个 filter。
//
// 没有 Q_OBJECT 宏 —— 只重写 QObject::eventFilter 这个虚函数，
// 不需要信号/槽，也就不需要 moc（cxx-qt-build 没跑 moc 这一步）。
//
// 「点了一下」的判据照抄 QQuickMouseArea（items/qquickmousearea.cpp）：
//
//     setPressed() :1243-1244
//         bool wasPressed = d->pressed & button;
//         bool isclick = wasPressed && p == false && dragged == false
//                        && d->hovered == true;
//     :1282-1287  只有 isclick 才 emit clicked
//     mousePressEvent :629-644   记 pressed 状态，不派发 clicked
//     mouseReleaseEvent :756-765 松开才走 setPressed(..., false)
//
// 也就是两个条件：松开的是刚才按下的那个键、而且松开那一刻指针还在元素里。
// hovered 在 MouseArea 是 mouseMoveEvent 里 setHovered(contains(...))（:660）刷的，
// 我们不等 move，直接在松开那一刻算 m_item->contains(local)：同一个判据，少一份状态。
//
// 三处有意的分歧：dragged / longPress 恒为假（本项目没 drag、没定时器）；
// mouseUngrabEvent()（:920）是派发器直接调虚函数、不是 QEvent，filter 看不见，
// 所以 pressed 位可能留着，但下一对按下/松开会覆盖它，只会多判一次不会漏发；
// 双击那一串（press/release/DblClick/release）第二次松开没有配对，被挡掉。
class InputFilter : public QObject
{
public:
    InputFilter(
        std::vector<InputFilter*>* ledger,
        QQuickItem* item,
        const std::string& name,
        std::uint32_t element
    )
        : QObject(item)
        , m_ledger(ledger)
        , m_item(item)
        , m_name(name)
        , m_element(element)
        , m_spec(spec_for(name))
    {
        // 所有权挂在 item 上（构造时直接当 parent 传进去）：
        // item 被 destroy（deleteLater）时 ~QObject 递归删孩子，filter 跟着走。

        // 死了就从登记簿里把自己摘掉。这一句是登记簿不留野指针的唯一保证：
        // destroy() 只能清理「被删的那个根」名下的 filter，
        // 而子树里孩子（比如列表行里那块会点的小方块）是 ~QObject 递归删的，
        // 没人替它们摘 —— 不摘的话下一次 add/remove 遍历登记簿就是解引用野指针。
        connect(
            this,
            &QObject::destroyed,
            [this] {
                if (m_ledger == nullptr) {
                    return;
                }

                auto& listeners = *m_ledger;

                listeners.erase(
                    std::remove(listeners.begin(), listeners.end(), this),
                    listeners.end()
                );
            }
        );

        // resize 那一支不吃 QEvent：几何只有我们在 set_geometry 时改，
        // 所以直接盯尺寸信号（Qt 会把两个信号一起发，下面按「一对」去重）。
        if (m_spec != nullptr && m_spec->payload == Payload::Resize) {
            connect(
                m_item,
                &QQuickItem::widthChanged,
                this,
                [this] { emit_resize(); }
            );

            connect(
                m_item,
                &QQuickItem::heightChanged,
                this,
                [this] { emit_resize(); }
            );
        }
    }

    const std::string& name() const { return m_name; }
    QQuickItem* item() const { return m_item; }
    const EventSpec* spec() const { return m_spec; }

protected:
    bool eventFilter(QObject *watched, QEvent *event) override
    {
        Q_UNUSED(watched)

        if (m_spec == nullptr) {
            return false;
        }

        // 按下这件事先记账，再看过不过名字那张表。
        //
        // 为什么记账要抢在 spec_covers 前面：click / auxclick 这两个名字
        // 「只吃松开」，可它们的判据是「按下+松开」这一对 ——
        // MouseArea 也是这个次序：d->pressed 在 mousePressEvent 里就写了（:644），
        // clicked 要到 setPressed(button, false) 才发（:1282-1287）。
        // 少了这一步，松开那一刻永远配不上对，click 一次都不会响。
        //
        // accept 也在这里：派发器只有在「事件被接受」之后才把按下的 item
        // 立成独占 grabber（util/qquickdeliveryagent.cpp:2550 accept、:2555 查
        // isAccepted、:2568-2569 setExclusiveGrabber），而收尾事件（松开）
        // 只发给持有 grab 的 item（:2534-2536）。裸 QQuickItem 的
        // mousePressEvent 是空实现不会自己 accept，所以这一句是前提。
        // MouseArea 干的同一件事在 :644 `event->setAccepted(setPressed(...))`。
        //
        // 一条实测边界，别当耳边风：用**没有 device 的合成 QMouseEvent**
        // 走窗口路由（离屏探针就是这么发的）时，press 能到 filter，
        // release 到不了（grab 那一步还看 point 状态，合成事件给不出真 device）。
        // 所以验证这套判据时 release 只能直送 item（绕过命中测试，
        // 只驱动状态机），命中测试那一半另外用 press 验；
        // 完整的一条链要真鼠标才有，离屏替不了。
        if (event->type() == QEvent::MouseButtonPress) {
            auto *mouse = static_cast<QMouseEvent *>(event);

            m_pressed |= mouse->button();

            mouse->accept();
        }

        // 名字不吃这一类事件，直接放行（false = 不吃，Qt 继续按自己的规矩走）。
        if (!spec_covers(*m_spec, event->type())) {
            return false;
        }

        switch (event->type())
        {
        // 按下：mousedown / pointerdown —— 按下当场就发（DOM 也是这个规矩），
        // 但一次 click 要等松开（判据见类上面）。
        case QEvent::MouseButtonPress:
        {
            auto *mouse = static_cast<QMouseEvent *>(event);

            emit_point(mouse, mouse->button(), mouse->buttons());

            return false;
        }

        case QEvent::MouseButtonRelease:
        {
            auto *mouse = static_cast<QMouseEvent *>(event);

            const Qt::MouseButton button = mouse->button();

            const bool was_pressed = (m_pressed & button) != Qt::NoButton;

            m_pressed &= ~button;

            // mouseup / pointerup：松开就发，不看位置 ——
            // 这一发能到我们手上本来就靠 press 拿到的 grab。
            if (is_up_name(m_name)) {
                emit_point(mouse, button, mouse->buttons());

                return false;
            }

            if (is_click_name(m_name)) {
                const QPointF local = mouse->position();

                if (!was_pressed || !m_item->contains(local)) {
                    return false;
                }

                // click 只认主键，auxclick 只认非主键（DOM 的分工）。
                if (m_name == "click" && button != Qt::LeftButton) {
                    return false;
                }

                if (m_name == "auxclick" && button == Qt::LeftButton) {
                    return false;
                }

                emit_point(mouse, button, mouse->buttons());

                // 这一对用掉了，accept 掉免得祖先元素再把它当自己的一次点击
                // （祖先那份由 dioxus 的冒泡管：runtime.rs:430-446）。
                mouse->accept();

                return false;
            }

            return false;
        }

        case QEvent::MouseButtonDblClick:
        {
            auto *mouse = static_cast<QMouseEvent *>(event);

            // 双击不走 click 那条配对，直接发。
            emit_point(mouse, mouse->button(), mouse->buttons());

            return false;
        }

        case QEvent::MouseMove:
        case QEvent::HoverMove:
        case QEvent::HoverEnter:
        case QEvent::HoverLeave:
        {
            auto *single = static_cast<QSinglePointEvent *>(event);

            // hover 系类没有按键信息，button 报 NoButton、buttons 带当前按住的一串。
            emit_point(single, Qt::NoButton, single->buttons());

            return false;
        }

        case QEvent::Wheel:
        {
            auto *wheel = static_cast<QWheelEvent *>(event);

            emit_wheel(wheel);

            return false;
        }

        case QEvent::KeyPress:
        case QEvent::KeyRelease:
        {
            auto *key = static_cast<QKeyEvent *>(event);

            emit_key(key);

            return false;
        }

        case QEvent::FocusIn:
        case QEvent::FocusOut:
        {
            qt_focus_event(rust_name(), m_element);

            return false;
        }

        case QEvent::TouchBegin:
        case QEvent::TouchUpdate:
        case QEvent::TouchEnd:
        case QEvent::TouchCancel:
        {
            emit_touch(static_cast<QTouchEvent *>(event));

            return false;
        }

        case QEvent::ContextMenu:
        {
            auto *menu = static_cast<QContextMenuEvent *>(event);

            // QContextMenuEvent 不是 QSinglePointEvent：只有 pos/globalPos（整型）。
            // pos() 是窗口坐标，正好当 scene；元素本地坐标自己 map 一下。
            const QPointF scene(qreal(menu->pos().x()), qreal(menu->pos().y()));
            const QPointF global(qreal(menu->globalPos().x()), qreal(menu->globalPos().y()));
            const QPointF local = m_item->mapFromScene(scene);

            qt_mouse_event(
                rust_name(),
                m_element,
                local.x(), local.y(),
                scene.x(), scene.y(),
                global.x(), global.y(),
                int(Qt::RightButton),
                int(Qt::NoButton),
                int(menu->modifiers())
            );

            return false;
        }

        default:
            return false;
        }
    }

private:
    rust::Str rust_name() const
    {
        return rust::Str(m_name.c_str(), m_name.size());
    }

    // MouseData / PointerData 两支同源：都是「一个点 + 按键 + 修饰键 + 三套坐标」。
    void emit_point(
        QSinglePointEvent *e,
        Qt::MouseButton button,
        Qt::MouseButtons buttons
    )
    {
        const QPointF local = e->position();
        const QPointF scene = e->scenePosition();
        const QPointF global = e->globalPosition();

        if (m_spec->payload == Payload::Pointer) {
            // Qt 的位掩码和浏览器 MouseEvent.buttons 的那几位同序，
            // 所以 button/buttons 原样交过去，Rust 侧直接 decode。
            // pressure/is_primary 这些 Qt 给不出的：鼠标按 web 的规矩报 0.5/1。
            qt_pointer_event(
                rust_name(),
                m_element,
                local.x(), local.y(),
                scene.x(), scene.y(),
                global.x(), global.y(),
                int(button),
                int(buttons),
                int(e->modifiers()),
                int(e->points().first().id()),
                "mouse",
                true,
                buttons == Qt::NoButton ? 0.0 : 0.5,
                1.0,
                1.0
            );

            return;
        }

        qt_mouse_event(
            rust_name(),
            m_element,
            local.x(), local.y(),
            scene.x(), scene.y(),
            global.x(), global.y(),
            int(button),
            int(buttons),
            int(e->modifiers())
        );
    }

    void emit_wheel(QWheelEvent *e)
    {
        const QPointF local = e->position();
        const QPointF scene = e->scenePosition();
        const QPointF global = e->globalPosition();

        qt_wheel_event(
            rust_name(),
            m_element,
            local.x(), local.y(),
            scene.x(), scene.y(),
            global.x(), global.y(),
            e->angleDelta().x(),
            e->angleDelta().y(),
            e->pixelDelta().x(),
            e->pixelDelta().y(),
            int(e->buttons()),
            int(e->modifiers())
        );
    }

    void emit_key(QKeyEvent *e)
    {
        const KeyNames names = key_names(e->key(), e->text());

        QString key_text = e->text();

        if (names.key != nullptr) {
            key_text = QString::fromUtf8(names.key);
        }

        const QByteArray key_utf8 = key_text.toUtf8();
        const QByteArray code_utf8 = QByteArray(names.code);

        qt_key_event(
            rust_name(),
            m_element,
            // location：Qt 不区分左右（Key_Shift 就是一个键），只能一律报 0（Standard）。
            rust::Str(key_utf8.constData(), key_utf8.size()),
            rust::Str(code_utf8.constData(), code_utf8.size()),
            0,
            e->isAutoRepeat(),
            int(e->modifiers())
        );
    }

    void emit_touch(QTouchEvent *e)
    {
        // 点数不定，所以打包成三个平行数组交过去：
        // points 的步长是 6（元素本地 x/y、scene x/y、screen x/y）。
        rust::Vec<rust::i32> ids;
        rust::Vec<rust::f64> points;
        rust::Vec<rust::f64> forces;
        rust::Vec<rust::i32> changed;

        const QList<QEventPoint> event_points = e->points();

        int index = 0;

        for (const QEventPoint &p : event_points) {
            ids.push_back(int(p.id()));

            points.push_back(p.position().x());
            points.push_back(p.position().y());
            points.push_back(p.scenePosition().x());
            points.push_back(p.scenePosition().y());
            points.push_back(p.globalPosition().x());
            points.push_back(p.globalPosition().y());

            forces.push_back(p.pressure());

            // 「本次变化的那几个点」：Qt 的状态是 Pressed/Updated/Released/...
            if (p.state() == QEventPoint::State::Pressed
                    || p.state() == QEventPoint::State::Released) {
                changed.push_back(int(index));
            }

            ++index;
        }

        qt_touch_event(
            rust_name(),
            m_element,
            std::move(ids),
            std::move(points),
            std::move(forces),
            std::move(changed),
            int(e->modifiers())
        );
    }

    void emit_resize()
    {
        const qreal width = m_item->width();
        const qreal height = m_item->height();

        // width/height 两个信号一起来，按「一对」去重，只发一次。
        if (qFuzzyCompare(width, m_last_width) && qFuzzyCompare(height, m_last_height)) {
            return;
        }

        m_last_width = width;
        m_last_height = height;

        qt_resize_event(rust_name(), m_element, width, height);
    }

private:
    std::vector<InputFilter*>* m_ledger = nullptr;
    QQuickItem* m_item;
    std::string m_name;
    std::uint32_t m_element;
    const EventSpec* m_spec = nullptr;

    // 照 MouseArea 的 d->pressed（:1245 那套位运算）：这个元素身上现在还按着哪些键。
    Qt::MouseButtons m_pressed = Qt::NoButton;

    qreal m_last_width = -1.0;
    qreal m_last_height = -1.0;
};


// 按登记簿上还活着的监听器，重算这个 item 该开哪些输入能力。
//
// 全部重算、不做增量开关：撤掉最后一个 hover 监听器要能把 hover 关回去，
// 增量的写法很容易留脏。
void refresh_item_capabilities(
    QQuickItem* item,
    const std::vector<InputFilter*>& listeners
)
{
    if (item == nullptr) {
        return;
    }

    Qt::MouseButtons buttons = Qt::NoButton;

    bool hover = false;
    bool touch = false;
    bool key_focus = false;

    for (InputFilter* filter : listeners) {
        if (filter->item() != item) {
            continue;
        }

        const EventSpec* spec = filter->spec();

        if (spec == nullptr) {
            continue;
        }

        switch (spec->payload) {
        case Payload::Mouse:
        case Payload::Pointer:
            buttons |= Qt::LeftButton | Qt::RightButton | Qt::MiddleButton;
            break;
        case Payload::Wheel:
            // 滚轮那一发也跟着「命中测试 + hover 目标」那条路走。
            buttons |= Qt::LeftButton;
            break;
        default:
            break;
        }

        hover = hover || spec->hover;
        touch = touch || spec->touch;
        key_focus = key_focus || spec->key_focus;
    }

    // 命中测试把 NoButton 的项当不存在（qquickdeliveryagent.cpp:2258），
    // 裸 QQuickItem / Rectangle 的默认就是 NoButton（qquickitem_p.h:1092-1096）
    // —— 所以这一句是「点得到 / 点不到」的分水岭。
    item->setAcceptedMouseButtons(buttons);
    item->setAcceptHoverEvents(hover);
    item->setAcceptTouchEvents(touch);

    // 按键只发给持有 active focus 的那个 item（:971-999 deliverKeyEvent，
    // 沿父链往上走到接受为止），所以要能拿焦点才可能收到 keydown。
    // ClickFocus：点一下就聚焦，和 DOM 里点按钮能收键盘的感觉一致。
    item->setFocusPolicy(key_focus ? Qt::ClickFocus : Qt::NoFocus);
}


// 窗口尺寸一变，就叫 Rust 重算一次布局。
//
// 为什么是 event filter 而不是 connect 一个信号：QWindow 没有
// widthChanged 这种信号，尺寸变化是以 QEvent::Resize 的形式进来的。
// Qt 自己的东西会跟着变（contentItem 会自动重设大小），
// 但我们这套几何的唯一作者是 relayout() —— 没人重算，屏幕上就一直是旧数字。
class ResizeFilter : public QObject
{
public:
    explicit ResizeFilter(QQuickWindow *window)
        : QObject(window)
        , m_window(window)
    {
    }

protected:
    bool eventFilter(QObject *watched, QEvent *event) override
    {
        // 装在 window 上，只看它的 Resize，不需要分辨 watched。
        Q_UNUSED(watched)

        if (event->type() != QEvent::Resize) {
            return false;
        }

        qDebug()
            << "[Qt] window resized to"
            << m_window->width() << "x" << m_window->height()
            << "-> 请 Rust 重算布局";

        // 交的是窗口自己的大小（设备无关像素），和 relayout 里
        // window_width()/window_height() 读的是同一对数 —— 
        // 这个参数只是让 Rust 那边日志里能说清是哪种变化引起的。
        qt_window_resized(
            m_window->width(),
            m_window->height()
        );

        return false;
    }

private:
    QQuickWindow* m_window;
};


struct QtHost::Impl {
    // 登记簿必须**最先声明**：声明越靠后析构越早，
    // 而 filter 死的时候（随 item 递归删、或被 remove 显式删）要回来把自己摘掉，
    // 所以这个 vector 得比 window（以及挂在 item 树上的那些 filter）活得久。
    //
    // 只是「按 (item, name) 找得到、好撤、能重算能力」的登记簿，不管所有权 ——
    // 所有权在 QObject 父子树上（filter 的 parent 就是那个 item）。
    std::vector<InputFilter*> listeners;

    // engine 晚一点声明，
    // 这样析构时 window 会先于 engine 销毁。
    QQmlEngine engine;
    QQuickWindow window;

    // 这里不再存 id -> 对象的表：指针本身就是句柄，
    // Rust 侧拿着它直接调下面的方法。
    //
    // 于是对象的生命周期只有两个去处：
    //     mount 过 → Qt 的 parent tree 负责，随窗口析构；
    //     被 destroy → deleteLater，随事件循环释放。
    // 唯一还漏的情况是「建出来但从没挂、也没被 destroy」——
    // 现在这条路径不存在（每个建出来的节点最后都会挂出去）。
    ~Impl() = default;
};


QtHost::QtHost()
    : impl_(std::make_unique<Impl>())
{
    impl_->window.resize(640, 340);

    impl_->window.setTitle(
        QStringLiteral(
            "Dioxus -> Qt Quick"
        )
    );

    impl_->window.setColor(
        QColor(
            QStringLiteral("#f6f1e8")
        )
    );

    // 尺寸变化的触发点。parent 给 window，窗口死时它跟着死。
    impl_->window.installEventFilter(
        new ResizeFilter(&impl_->window)
    );
}


QtHost::~QtHost() = default;


std::unique_ptr<QtHost> new_qt_host()
{
    return std::make_unique<QtHost>();
}

void schedule_pump_poll()
{
    QMetaObject::invokeMethod(
        qApp,
        []() {
            qt_pump_poll();
        },
        Qt::QueuedConnection
    );
}


void QtHost::show()
{
    impl_->window.show();
}


QQuickItem* QtHost::root_item() const
{
    // 窗口的内容区 —— 所有顶层节点的 parentItem。
    // 它由 window 持有，我们只是把指针交出去。
    return impl_->window.contentItem();
}


QQuickItem* QtHost::create_rectangle() {
    // 注意这里没有 width / height：
    // 几何一律由 Rust 侧的布局器通过 set_geometry 给。
    return instantiate(
        impl_->engine,
        R"QML(
import QtQuick

Rectangle {
}
)QML",
        "Rectangle"
    );
}


QQuickItem* QtHost::create_column() {
    // 一个裸 Item —— 没有 Layout，没有 anchors。
    // 它存在的唯一理由是给「容器」一个能装孩子的对象；
    // 孩子摆在哪，完全是布局器的事。
    return instantiate(
        impl_->engine,
        R"QML(
import QtQuick

Item {
}
)QML",
        "Column"
    );
}


QQuickItem *
QtHost::create_text(rust::Str text)
{
    // 注意：这不是用户写的 UI，
    // 这是 renderer 对 Dioxus 文本 primitive 的 Qt 实现。
    //
    // 字体/颜色留在 QML 里 —— 那是「怎么画」；
    // 但 x / y / width / height 属于布局，只能由 set_geometry 决定。
    QQuickItem *item = instantiate(
        impl_->engine,
        R"QML(
import QtQuick

Text {
}
)QML",
        "Text"
    );

    // 此刻对象已经有了内容，但还没进 visual tree。
    if (item != nullptr) {
        item->setObjectName(TEXT_ITEM_NAME);
        item->setProperty("text", to_qstring(text));

        // Text 的构造函数给自己设了 Qt::LeftButton（qquicktext.cpp:96），
        // 也就是「文本是点击目标」。这里的文本纯展示，而且按钮上的标签
        // 正好盖在按钮上面 —— 留着它，点标签那一下会被文本吃掉。
        // 关掉之后命中测试直接跳过它，那一下落到后面的矩形上。
        item->setAcceptedMouseButtons(Qt::NoButton);
    }

    return item;
}


void
QtHost::mount(
    QQuickItem* parent,
    QQuickItem* child
)
{
    // -----------------------------
    // QObject ownership tree
    // -----------------------------

    child->setParent(parent);


    // -----------------------------
    // Qt Quick visual tree
    // -----------------------------

    child->setParentItem(parent);


    qDebug()
        << "[Qt] mount" << child
        << "into parent" << parent;
}


void
QtHost::place(
    QQuickItem* parent,
    QQuickItem* child,
    QQuickItem* sibling
)
{
    if (parent == nullptr || child == nullptr) {
        return;
    }


    // 两棵树都要跟上：setParentItem 只管 visual tree，
    // 它不会碰 QObject 的 parent（上面 setParentItem 的函数体里
    // 只有 addChild/removeChild，没有 setParent）。
    if (child->parentItem() != parent) {
        child->setParent(parent);
        child->setParentItem(parent);
    }


    if (sibling != nullptr && sibling != child) {
        child->stackBefore(sibling);
        return;
    }


    // 目标是「末尾」。已经在末尾就什么都不做 ——
    // 别去跑下面那对 detach/attach，那是有代价的（removeChild + addChild，
    // 还会让兄弟们的 siblingOrderChanged 走一遍）。
    const QList<QQuickItem*> children = parent->childItems();

    if (!children.isEmpty() && children.last() == child) {
        return;
    }


    // Qt 没有「移到末尾」的公开接口（stackBefore(nullptr) 只是 qWarning），
    // 所以绕一圈：先从 visual tree 摘下来，再挂回去。
    // 挂回去走的是 QQuickItemPrivate::addChild，而它是
    // childItems.append(child)（qquickitem.cpp:3049）—— 落在末尾，正是要的。
    //
    // 这里不能碰 setParent(nullptr)：QObject 那一侧的父亲是谁决定了谁来删它，
    // 移动位置不该把所有权也一起挪走。
    child->setParentItem(nullptr);
    child->setParentItem(parent);
}


void
QtHost::destroy(
    QQuickItem* item
)
{
    if (item == nullptr) {
        return;
    }


    // 先立刻脱离两棵树：视觉上它当场就没了，
    // 父节点的 childItems 也不再含着它 —— 这两步不等事件循环。
    item->setParentItem(nullptr);
    item->setParent(nullptr);


    // 这里不再手动清理登记簿：filter 是 item 的 QObject 孩子，会跟着 ~QObject
    // 递归删掉，而每个 InputFilter 在自己的 destroyed 信号里把登记簿那一格摘掉
    // （见 InputFilter 构造函数）。手写循环只能清「被删的这个根」名下的，
    // 子树里孩子名下的照样留野指针 —— 那正是它当初的洞。


    // 内存的释放推给事件循环，理由（QObject::deleteLater 的文档同款）：
    // 以后这套 mutation 是在 Qt 的事件处理里跑下来的，
    // 「点了某个节点 → 重渲染正好把它删掉」这条路上当场 delete，
    // 事件还在往它身上派，就是 use-after-free。
    //
    // 于是日志里会先看到「destroy scheduled」，
    // 等 exec() 转起来才有对应的「object destroyed」。
    item->deleteLater();


    qDebug()
        << "[Qt] destroy scheduled" << item;
}


void
QtHost::stack_before(
    QQuickItem* child,
    QQuickItem* sibling
)
{
    if (
        child == nullptr ||
        sibling == nullptr
    ) {
        return;
    }


    child->stackBefore(sibling);
}


// 装一个监听器。名字决定这一支吃什么事件、送哪支回调（表在 EVENT_SPECS）。
//
// 名字必须是 Rust 那边查过表交下来的：它对着同一张表拦一道，
// 表里没有的事件名（onkeydown 之外的键盘类、drag、scroll……）根本不会到这里。
void
QtHost::add_event_listener(
    QQuickItem* item,
    rust::Str name,
    std::uint32_t element
)
{
    if (item == nullptr) {
        return;
    }

    const std::string key(name.data(), name.size());

    // 同一个 (item, name) 只留一个 filter：先撤旧的。
    // 不然重渲染把同一个监听器再挂一遍，一次操作就派发到 Rust 两次。
    remove_event_listener(item, name);

    if (spec_for(key) == nullptr) {
        qWarning()
            << "[Qt] 事件名不在表里，不装 filter：" << key.c_str();

        return;
    }

    auto *filter = new InputFilter(&impl_->listeners, item, key, element);

    item->installEventFilter(filter);

    impl_->listeners.push_back(filter);

    // 「点得到 / 点不到」的分水岭不在 filter 这一步，而在 item 的能力开关上：
    // 命中测试把 acceptedMouseButtons() == Qt::NoButton 的项**直接当不存在**
    // （qquickdeliveryagent.cpp:2258，pointerTargets 的谓词），
    // 裸 QQuickItem / Rectangle 的默认值就是 NoButton
    // （qquickitem_p.h:1092-1096）。hover / touch / focus 同理，
    // 各有一套开关，全看注册了哪些名字 —— 所以统一交给下面这个函数重算。
    //
    // 之前离屏自检能通，是因为 QCoreApplication::sendEvent(item, ...) 直接
    // 把这个对象当事件接收者，绕过了整条命中测试（QQuickItem::event()
    // 里没有任何 acceptedMouseButtons 检查，qquickitem.cpp:9326/9353）。
    refresh_item_capabilities(item, impl_->listeners);

    qDebug()
        << "[Qt] listener installed on" << item
        << "name" << key.c_str()
        << "element" << element
        << "acceptedMouseButtons" << int(item->acceptedMouseButtons())
        << "hover" << item->acceptHoverEvents()
        << "touch" << item->acceptTouchEvents()
        << "focusPolicy" << int(item->focusPolicy());
}


// 撤掉 (item, name) 那一格。
void
QtHost::remove_event_listener(
    QQuickItem* item,
    rust::Str name
)
{
    if (item == nullptr) {
        return;
    }

    const std::string key(name.data(), name.size());

    // 找一格删一格，删完从头再找：delete filter 会触发它的 destroyed，
    // 而那个槽就地把自己从 vector 里摘掉 —— 手上的迭代器当场失效，
    // 所以这里不能边遍历边 erase。
    for (;;) {
        InputFilter *match = nullptr;

        for (InputFilter* filter : impl_->listeners) {
            if (filter->item() == item && filter->name() == key) {
                match = filter;

                break;
            }
        }

        if (match == nullptr) {
            break;
        }

        item->removeEventFilter(match);

        // filter 的 parent 是 item，这里显式删也安全：
        // ~QObject 会把它从父亲的 children 里摘掉。
        delete match;   // ← 登记簿由 destroyed 槽负责清理
    }

    // 能力开关跟着重算：这个 item 的监听器都撤光的话，
    // acceptedMouseButtons 要回到 NoButton —— 不然它白占一个命中测试的目标，
    // 把后面的（比如它父亲上的）监听器挤掉。
    refresh_item_capabilities(item, impl_->listeners);
}


void
QtHost::set_geometry(
    QQuickItem* item,
    double x,
    double y,
    double width,
    double height
)
{
    // x / y 是相对 parentItem 的局部坐标，
    // 和布局器输出的 location 是同一个口径，不用换算。
    item->setX(x);
    item->setY(y);
    item->setWidth(width);
    item->setHeight(height);
}


double QtHost::window_width() const
{
    return impl_->window.width();
}


double QtHost::window_height() const
{
    return impl_->window.height();
}


namespace {

// 抄存用的动态属性名：__dq_saved_<属性名>。
//
// 为什么存在 QObject 的动态属性里，而不是 QtHost 的一张 map 里：
// 对象销毁的时候动态属性跟着一起走。我们这套里 destroy() 只被子树根调用一次，
// 孩子是随 ~QObject 递归死的（见 QtHost::destroy），
// 换成 map 就必须也在那个递归里逐个摘除 —— 漏一个就是悬空指针。
QByteArray
saved_key_of(
    rust::Str name
)
{
    return (
        QStringLiteral("__dq_saved_") + to_qstring(name)
    ).toUtf8();
}

}


void
QtHost::set_property(
    QQuickItem *item,
    rust::Str name,
    const QVariant &value
)
{
    const QString property = to_qstring(name);
    const QByteArray propertyUtf8 = property.toUtf8();


    // 第一次写之前先抄一份现值 —— 那份就是「没写过它时对象是什么样」，
    // clear_property 靠它把属性还原回去。
    //
    // 判「抄过没有」用 isValid()：我们写的这些属性都是真 Q_PROPERTY，
    // property() 读出来一定是个带类型的有效值（bool 的 false、QString 的 ""、
    // QColor 的白），所以无效 QVariant 就等于「还没有抄本」。
    const QByteArray savedKey = saved_key_of(name);

    if (!item->property(savedKey.constData()).isValid()) {
        item->setProperty(
            savedKey.constData(),
            item->property(propertyUtf8.constData())
        );
    }


    // setProperty 返回 false 说明这个 Qt 对象上没有这个属性，
    // 或者值没法转换成属性需要的类型。
    if (
        !item->setProperty(
            propertyUtf8.constData(),
            value
        )
    ) {
        qWarning()
            << "[Qt] cannot set property"
            << property
            << "="
            << value;
    }
}


void
QtHost::clear_property(
    QQuickItem *item,
    rust::Str name
)
{
    const QString property = to_qstring(name);
    const QByteArray propertyUtf8 = property.toUtf8();
    const QByteArray savedKey = saved_key_of(name);

    const QVariant saved = item->property(savedKey.constData());

    if (!saved.isValid()) {
        // 没抄本 = 这个名字我们从来没写过，对象本来就没被我们动过。
        // 如实说一句，不谎称「已还原」。
        qDebug()
            << "[Qt] clear" << property
            << "：没有抄存过（这个属性我们从来没写过），无事可做";
        return;
    }

    item->setProperty(
        propertyUtf8.constData(),
        saved
    );

    // 撤掉抄本：给动态属性写一个无效 QVariant，Qt 的行为是**删掉这条动态属性**。
    // 于是下一次写同一个名字会重新抄一遍（那时现值就是刚还原出来的那个）。
    item->setProperty(
        savedKey.constData(),
        QVariant()
    );

    // 把「对象现在自己报什么」也读回来打一遍：
    // 这一列才是证据，前面那个是「我打算写什么」。
    qDebug()
        << "[Qt] clear" << property
        << "→ 写回刚建出来时的" << saved
        << "，对象现在报"
        << item->property(propertyUtf8.constData());
}

namespace {

// 「这是不是一个会显示文字的项」的判据 —— 它有没有一个读得出 QString 的
// `text` 属性。
//
// 为什么不是 qobject_cast<QQuickText*>：brew 的 QtQuick.framework 压根不发
// QQuickText 的头文件（Headers 里只有 qquickitem.h 那批，私有头也不在包里），
// 为一个 cast 去引私有 API 不值当。走 meta-object 反而更宽：
// 以后换成自定义的、带 text 属性的文本项，这条判据照样成立。
//
// 这样读没有副作用：property("text") 调的就是 QQuickText::text()，
// 而那个 getter 是 `return d->text;`（qquicktext.cpp:1837-1841）。
QString text_of_item(
    QQuickItem *item
)
{
    if (item == nullptr) {
        return {};
    }

    const QVariant value = item->property("text");

    // 没这个属性 → 无效 QVariant（Rectangle / Item 走这里）；
    // 有但不是字符串 → 也不算文本项。
    if (!value.canConvert<QString>()) {
        return {};
    }

    return value.toString();
}


// 「量一段文本要占多大」的唯一一份实现。
//
// 只有 QQuickText 的量得出来，而且**不去读 item 的 implicitWidth/Height** ——
// 那两个值折行时用的是 item 当前的 width（qquicktext.cpp:834），
// 而那个 width 正是布局这一步要决定的东西，会绕成环。
// 折行的约束只能从参数进来（taffy 递的 available_space）。
TextMeasure measure_layout(
    const QString &text,
    double max_width
)
{
    // 字体：默认构造的 QFont 就是 QGuiApplication::font()，
    // 和 C++ 那段裸 Text {} 用的是同一个来源。
    // 等 font 变成 Rust 下发的样式属性时，这里要改成收进来的那个 font，
    // 否则「量的」和「画的」又会分成两个作者。
    QFont font;

    QTextLayout layout(
        text,
        font
    );

    layout.setCacheEnabled(true);

    QTextOption option;

    // qquicktext.cpp:818 —— shouldUseDesignMetrics =
    //     renderType != NativeRendering，而 renderType 默认是 QtRendering。
    // 所以要对齐默认渲染，这里必须用设计度量。
    option.setUseDesignMetrics(true);

    // QQuickText 的默认 wrapMode 是 Wrap（= WrapAtWordBoundaryOrAnywhere）。
    // 不限宽时靠下面 lineWidth = FLT_MAX 让它不折行，和 Qt 一样的做法。
    option.setWrapMode(
        QTextOption::WrapAtWordBoundaryOrAnywhere
    );

    layout.setTextOption(option);

    // qquicktext.cpp:833-835：宽度无效时 Qt 用的就是 FLT_MAX。
    const qreal line_width = max_width > 0.0
        ? max_width
        : qreal(FLT_MAX);

    layout.beginLayout();

    qreal height = 0.0;

    // 折行之后「真正用到」的宽度：所有行里最宽那一行的自然宽度。
    qreal widest_line = 0.0;

    for (;;) {
        QTextLine line = layout.createLine();

        // qtextlayout.h:194
        if (!line.isValid()) {
            break;
        }

        line.setLineWidth(line_width);

        // qtextlayout.cpp:1581：这一行文本的自然宽度（不做两端对齐）。
        widest_line = qMax(
            widest_line,
            line.naturalTextWidth()
        );

        // QTextLine::height() 内部会 ceil（qtextlayout.cpp:1524），
        // QQuickText 累加的也是同一个函数（qquicktext.cpp:1307）。
        height += line.height();
    }

    layout.endLayout();

    // 不限宽：宽度就是 QQuickText implicitWidth 的那个口径
    // （qquicktext.cpp:1044 用的 maximumWidth()），
    // 这样和 implicit 的对照才有意义。
    //
    // 限宽：换成折行后的实际宽度 —— maximumWidth() 量的始终是
    // 「最长那条逻辑行」，折了行它也还是 254.9，
    // 而布局要的是「这段字在这宽度下占了多宽」。
    // 再夹一次 max_width：行尾空格会让 naturalTextWidth 略微超出去。
    const qreal width = max_width > 0.0
        ? qMin(widest_line, max_width)
        : layout.maximumWidth();

    return { width, height };
}

} // namespace


bool
QtHost::is_text_item(
    QQuickItem *item
) const
{
    // 判据是「这个 item 自己有没有一段要显示的文字」，不是 Rust 记的账、
    // 也不是名字。空文本一并算 false：QTextLayout 对 "" 照样吐一行高
    // （0 x lineHeight），少了这层判断，一个空标签会白占一行。
    return item->objectName() == TEXT_ITEM_NAME;
}


TextMeasure
QtHost::measure_item(
    QQuickItem *item,
    double max_width
) const
{
    const QString text = text_of_item(item);

    // 不是文本项（Rectangle / Item）、或者没有内容 → 0x0：
    // 这种节点的尺寸本来就该由 style 里的 width / height 决定，
    // compute_leaf_layout 会把 style 盖回来。
    if (text.isEmpty()) {
        return { 0.0, 0.0 };
    }

    return measure_layout(text, max_width);
}


double
QtHost::implicit_width(
    QQuickItem *item
) const
{
    // QQuickItem::implicitWidth() -> 虚函数 getImplicitWidth()
    // -> QQuickTextPrivate::getImplicitWidth()（qquicktext.cpp:109）
    // 会强制同步跑一次 updateSize()，所以没 polish 过也读得到。
    return item ? item->implicitWidth() : 0.0;
}


double
QtHost::implicit_height(
    QQuickItem *item
) const
{
    return item ? item->implicitHeight() : 0.0;
}


rust::Vec<std::uint64_t>
QtHost::child_order(
    QQuickItem* parent
) const
{
    rust::Vec<std::uint64_t> out;

    if (parent == nullptr) {
        return out;
    }

    // childItems() 就是叠放顺序：下标小的在下面先画。
    const auto children = parent->childItems();

    for (auto* child : children) {
        out.push_back(
            reinterpret_cast<std::uint64_t>(child)
        );
    }

    return out;
}


void QtHost::process_events(std::int32_t ms) const {
    QGuiApplication::processEvents(QEventLoop::AllEvents, ms);
}