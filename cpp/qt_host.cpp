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


// 一个「谁被按了」的转发器。
//
// 没有 Q_OBJECT 宏 —— 我们只重写 QObject::eventFilter 这个虚函数，
// 不需要信号/槽，也就不需要 moc（cxx-qt-build 没跑 moc 这一步）。
class ClickFilter : public QObject
{
public:
    ClickFilter(
        QQuickItem* item,
        const std::string& name,
        std::uint32_t element
    )
        : m_item(item)
        , m_name(name)
        , m_element(element)
    {
        // 所有权挂在 item 上：item 被 destroy（deleteLater）时
        // ~QObject 递归删孩子，这个 filter 跟着走，不会悬空。
        setParent(item);
    }

    const std::string& name() const { return m_name; }
    QQuickItem* item() const { return m_item; }

protected:
    bool eventFilter(QObject *watched, QEvent *event) override
    {
        if (event->type() != QEvent::MouseButtonPress) {
            return false;
        }

        auto *mouse = static_cast<QMouseEvent *>(event);

        // 坐标一并带上：dioxus 那边的 MouseData 要读 element/client 坐标，
        // 只传「谁被按了」是个残缺的负载。
        const QPointF local = mouse->position();

        rust::Str str(m_name.c_str(), m_name.size());

        qDebug()
            << "[Qt] click on" << watched
            << "at" << local
            << "-> dispatch" << m_name.c_str()
            << "element" << m_element;

        qt_handle_event(
            str,
            m_element,
            static_cast<double>(local.x()),
            static_cast<double>(local.y())
        );

        // false = 不吃这个事件，Qt 继续按它自己的规矩处理。
        return false;
    }

private:
    QQuickItem* m_item;
    std::string m_name;
    std::uint32_t m_element;
};


struct QtHost::Impl {
    // engine 先声明，
    // 这样析构时 window 会先于 engine 销毁。
    QQmlEngine engine;
    QQuickWindow window;

    // 我们装出去的 event filter。
    //
    // 只是「按 (item, name) 找得到、好撤」的登记簿，不管所有权 ——
    // 所有权在 QObject 父子树上（filter 的 parent 就是那个 item）。
    // item 被 destroy 时 filter 一起没了，这里的裸指针就变陈旧，
    // 所以 destroy 路径要顺手把它们剔掉（见 QtHost::destroy）。
    std::vector<ClickFilter*> listeners;

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


    // 这个 item 名下的 event filter 是它的 QObject 孩子，
    // 会跟着它一起被 ~QObject 递归删掉。
    // 这里不用主动删它们，但登记簿里那些裸指针必须剔干净 ——
    // 留着的话下一次 remove_click_listener 就会解引用野指针。
    for (auto it = impl_->listeners.begin();
         it != impl_->listeners.end(); )
    {
        if ((*it)->item() == item) {
            it = impl_->listeners.erase(it);
            continue;
        }

        ++it;
    }


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


void
QtHost::add_click_listener(
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
    // 不然重渲染把同一个监听器再挂一遍，一次点击就会派发到 Rust 两次。
    remove_click_listener(item, name);

    auto *filter = new ClickFilter(item, key, element);

    item->installEventFilter(filter);

    // 这一步才是「点得到 / 点不到」的分水岭，光装 filter 没用：
    // 真鼠标的派发要先过命中测试，而命中测试把
    // acceptedMouseButtons() == Qt::NoButton 的项**直接当不存在**
    // （qquickdeliveryagent.cpp:2258，pointerTargets 的谓词）。
    // 裸 QQuickItem / Rectangle 的默认值就是 NoButton
    // （qquickitem_p.h:1092-1096：extra 没分配、LeftMouseButtonAccepted 没置 → 空集），
    // 所以事件在到不了这个 item 之前就被丢了，filter 自然一次都没被调到。
    //
    // 之前离屏自检能通，是因为 QCoreApplication::sendEvent(item, ...) 是直接
    // 把这个对象当事件接收者，绕过了整条命中测试（QQuickItem::event()
    // 里没有任何 acceptedMouseButtons 检查，qquickitem.cpp:9326/9353）。
    item->setAcceptedMouseButtons(Qt::LeftButton);

    impl_->listeners.push_back(filter);

    qDebug()
        << "[Qt] click listener installed on" << item
        << "name" << key.c_str()
        << "element" << element
        << "acceptedMouseButtons"
        << int(item->acceptedMouseButtons());
}


void
QtHost::remove_click_listener(
    QQuickItem* item,
    rust::Str name
)
{
    if (item == nullptr) {
        return;
    }

    const std::string key(name.data(), name.size());

    for (auto it = impl_->listeners.begin();
         it != impl_->listeners.end(); )
    {
        ClickFilter *filter = *it;

        if (filter->item() == item && filter->name() == key) {
            item->removeEventFilter(filter);

            it = impl_->listeners.erase(it);

            // filter 的 parent 是 item，这里显式删也安全：
            // ~QObject 会把它从父亲的 children 里摘掉。
            delete filter;

            continue;
        }

        ++it;
    }

    // 这个 item 上一个 filter 都不剩了，就把命中测试的入场券也一起收回来 ——
    // 不然它留着 Qt::LeftButton，会当成点击目标白占一格，
    // 把后面的（比如它父亲上的）监听器挤掉。
    const bool still_watched = std::any_of(
        impl_->listeners.begin(),
        impl_->listeners.end(),
        [item](const ClickFilter *filter) {
            return filter->item() == item;
        });

    if (!still_watched) {
        item->setAcceptedMouseButtons(Qt::NoButton);
    }
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


void
QtHost::set_property(
    QQuickItem *item,
    rust::Str name,
    const QVariant &value
)
{
    const QByteArray property =
        to_qstring(name).toUtf8();


    // setProperty 返回 false 说明这个 Qt 对象上没有这个属性，
    // 或者值没法转换成属性需要的类型。
    // 例如 color 是 QColor、font.pixelSize 是嵌套属性，
    // 都不是 QString 能直接写进去的。
    if (
        !item->setProperty(
            property.constData(),
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