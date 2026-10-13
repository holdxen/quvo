#include "qt_host.h"

#include <QtCore/QByteArray>
#include <QtCore/QDebug>
#include <QtCore/QObject>
#include <QtCore/QString>
#include <QtCore/QUrl>

#include <QtGui/QColor>

#include <QtQml/QQmlComponent>
#include <QtQml/QQmlEngine>

#include <QtQuick/QQuickItem>
#include <QtQuick/QQuickWindow>

#include <unordered_map>

namespace {

QString to_qstring(rust::Str text)
{
    return QString::fromUtf8(
        text.data(),
        static_cast<qsizetype>(text.size())
    );
}

// 共用：把一段内联 QML 实例化成 QQuickItem，登记进 objects 并返回 id。
//
// 返回 0 表示失败（Rust 侧会把 0 当成「这个节点没有 Qt 对象」，
// 于是既不挂载也不参与布局）。
std::uint64_t instantiate(
    QQmlEngine& engine,
    std::unordered_map<std::uint64_t, QObject*>& objects,
    std::uint64_t& next_id,
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

        return 0;
    }

    QObject* object = component.create();

    if (object == nullptr) {
        qWarning()
            << "QQmlComponent::create failed" << label;

        return 0;
    }

    auto* item = qobject_cast<QQuickItem*>(object);

    if (item == nullptr) {
        qWarning()
            << "created object is not QQuickItem" << label;

        delete object;

        return 0;
    }

    const std::uint64_t id = next_id++;

    objects.emplace(id, object);

    qDebug()
        << "[Qt] create" << label
        << "id =" << id
        << "but NOT mounted yet";

    return id;
}

} // namespace


struct QtHost::Impl {
    // engine 先声明，
    // 这样析构时 window 会先于 engine 销毁。
    QQmlEngine engine;
    QQuickWindow window;

    std::uint64_t next_id = 1;

    std::unordered_map<
        std::uint64_t,
        QObject*
    > objects;

    ~Impl()
    {
        // 已经 mount 的对象由 Qt parent tree 管理。
        //
        // 没 mount 的对象我们自己释放。
        for (auto const& [id, object] : objects) {
            Q_UNUSED(id);

            if (
                object != nullptr &&
                object->parent() == nullptr
            ) {
                delete object;
            }
        }
    }
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


std::uint64_t QtHost::create_rectangle() {
    // 注意这里没有 width / height：
    // 几何一律由 Rust 侧的布局器通过 set_geometry 给。
    return instantiate(
        impl_->engine,
        impl_->objects,
        impl_->next_id,
        R"QML(
import QtQuick

Rectangle {
}
)QML",
        "Rectangle"
    );
}


std::uint64_t QtHost::create_column() {
    // 一个裸 Item —— 没有 Layout，没有 anchors。
    // 它存在的唯一理由是给「容器」一个能装孩子的对象；
    // 孩子摆在哪，完全是布局器的事。
    return instantiate(
        impl_->engine,
        impl_->objects,
        impl_->next_id,
        R"QML(
import QtQuick

Item {
}
)QML",
        "Column"
    );
}


std::uint64_t
QtHost::create_text(rust::Str text)
{
    // 注意：这不是用户写的 UI，
    // 这是 renderer 对 Dioxus 文本 primitive 的 Qt 实现。
    //
    // 字体/颜色留在 QML 里 —— 那是「怎么画」；
    // 但 x / y / width / height 属于布局，只能由 set_geometry 决定。
    const std::uint64_t id = instantiate(
        impl_->engine,
        impl_->objects,
        impl_->next_id,
        R"QML(
import QtQuick

Text {
}
)QML",
        "Text"
    );

    if (id == 0) {
        return 0;
    }

    object_for(id)->setProperty(
        "text",
        to_qstring(text)
    );

    return id;
}


void
QtHost::mount(
    std::uint64_t parent_id,
    std::uint64_t child_id
)
{
    QObject* child =
        object_for(child_id);

    auto* child_item =
        qobject_cast<QQuickItem*>(
            child
        );

    if (
        child == nullptr ||
        child_item == nullptr
    ) {
        qWarning()
            << "[Qt] cannot mount object"
            << child_id;

        return;
    }


    // parent_id == 0 就是窗口根。
    QQuickItem* parent =
        parent_id == 0
            ? impl_->window.contentItem()
            : qobject_cast<QQuickItem*>(
                  object_for(parent_id)
              );

    if (parent == nullptr) {
        qWarning()
            << "[Qt] unknown parent"
            << parent_id;

        return;
    }


    // -----------------------------
    // QObject ownership tree
    // -----------------------------

    child->setParent(parent);


    // -----------------------------
    // Qt Quick visual tree
    // -----------------------------

    child_item->setParentItem(parent);


    qDebug()
        << "[Qt] mount object"
        << child_id
        << "into parent"
        << parent_id;
}


void
QtHost::stack_before(
    std::uint64_t child_id,
    std::uint64_t sibling_id
)
{
    auto* child =
        qobject_cast<QQuickItem*>(
            object_for(child_id)
        );

    auto* sibling =
        qobject_cast<QQuickItem*>(
            object_for(sibling_id)
        );

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
    std::uint64_t id,
    double x,
    double y,
    double width,
    double height
)
{
    auto* item =
        qobject_cast<QQuickItem*>(
            object_for(id)
        );

    if (item == nullptr) {
        qWarning()
            << "[Qt] set_geometry on unknown object"
            << id;

        return;
    }


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


QObject*
QtHost::object_for(
    std::uint64_t id
) const
{
    const auto it =
        impl_->objects.find(id);

    if (it == impl_->objects.end()) {
        return nullptr;
    }


    return it->second;
}


void
QtHost::set_property(
    std::uint64_t id,
    rust::Str name,
    const QVariant &value
)
{
    QObject* object =
        object_for(id);

    if (object == nullptr) {
        qWarning()
            << "unknown object id"
            << id;

        return;
    }


    const QByteArray property =
        to_qstring(name).toUtf8();


    // setProperty 返回 false 说明这个 Qt 对象上没有这个属性，
    // 或者值没法转换成属性需要的类型。
    // 例如 color 是 QColor、font.pixelSize 是嵌套属性，
    // 都不是 QString 能直接写进去的。
    if (
        !object->setProperty(
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