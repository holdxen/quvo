// 只 include 生成头，不要同时 include 源码树里的 "qt_host.h"：
// cxx-qt-build 会把头文件复制到 target 下，两份路径不同、`#pragma once`
// 认不出来，于是 QtHost / TextMeasure 都会被定义两遍。
// 生成头本身就会带上复制过来的 qt_host.h，而且它才有 bridge
// shared struct（TextMeasure）的完整定义。
#include "test_dioxus_qt/src/main.cxx.h"

#include <QtCore/QByteArray>
#include <QtCore/QDebug>
#include <QtCore/QObject>
#include <QtCore/QString>
#include <QtCore/QTimer>
#include <QtCore/QUrl>


#include <QtGui/QColor>
#include <QtGui/QFont>
#include <QtGui/QTextLayout>
#include <QtGui/QTextOption>
#include <QtGui/QGuiApplication>

#include <QtQml/QQmlComponent>
#include <QtQml/QQmlEngine>

#include <QtQuick/QQuickItem>
#include <QtQuick/QQuickWindow>

// FLT_MAX —— QQuickText 不限宽时用的也是它（qquicktext.cpp:834）。
#include <cfloat>

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


struct QtHost::Impl {
    // engine 先声明，
    // 这样析构时 window 会先于 engine 销毁。
    QQmlEngine engine;
    QQuickWindow window;

    // 回路的心跳。挂在 window 上（QObject 父子树负责释放），
    // 所以 start_pump 里 new 完不用管它的死活。
    QTimer *pump_timer = nullptr;

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
    impl_->window.resize(640, 220);

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


void QtHost::show()
{
    impl_->window.show();
}


void QtHost::start_pump(
    int interval_ms
)
{
    // 挂在 window 上当 QObject 父节点：窗口析构时 timer 一起走，
    // 不需要自己 delete。
    if (impl_->pump_timer == nullptr) {
        impl_->pump_timer = new QTimer(&impl_->window);
    }

    // 直接调生成头里声明的那个全局函数（`void qt_pump_tick() noexcept;`）。
    // lambda 形式的 connect 不需要 moc，QTimer 也不是信号发射方以外的东西。
    QObject::connect(
        impl_->pump_timer,
        &QTimer::timeout,
        []() {
            qt_pump_tick();
        }
    );

    impl_->pump_timer->start(interval_ms);

    qDebug()
        << "[Qt] pump timer started," << interval_ms << "ms";
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
        item->setProperty("text", to_qstring(text));
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

TextMeasure
QtHost::measure_text(
    rust::Str text,
    double max_width
) const
{
    // 字体：默认构造的 QFont 就是 QGuiApplication::font()，
    // 和 C++ 那段裸 Text {} 用的是同一个来源。
    // 等 font 变成 Rust 下发的样式属性时，这里要改成收进来的那个 font，
    // 否则「量的」和「画的」又会分成两个作者。
    QFont font;

    QTextLayout layout(
        to_qstring(text),
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

    // 空串也会走出一行：QTextLayout 对 "" 照样给一个行高，
    // 所以量出来是 0 x lineHeight，而不是 0 x 0。
    // （负向检查时踩过这个坑：拿空串当「测不到尺寸」的探针是探不出来的。）

    // 不限宽：宽度就是 QQuickText implicitWidth 的那个口径
    // （qquicktext.cpp:1044 用的 maximumWidth()），
    // 这样 verify_text_measurement 的 Δ 才有意义。
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