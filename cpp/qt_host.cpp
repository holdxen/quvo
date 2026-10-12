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
    QQmlComponent component(&impl_->engine);

    component.setData(R"QML(
import QtQuick
Rectangle {
    width: 30
    height: 30
    color: 'red'
}
)QML", QUrl());
    if (component.isError()) {
        for (
            const auto& error :
            component.errors()
        ) {
            qWarning().noquote()
                << error.toString();
        }

        return 0;
    }

        QObject* object =
        component.create();

    if (object == nullptr) {
        qWarning()
            << "QQmlComponent::create failed";

        return 0;
    }


    auto* item =
        qobject_cast<QQuickItem*>(
            object
        );

    if (item == nullptr) {
        qWarning()
            << "created object is not QQuickItem";

        delete object;

        return 0;
    }

    const std::uint64_t id =
    impl_->next_id++;


    impl_->objects.emplace(
        id,
        object
    );


    qDebug()
    << "[Qt] create Text id ="
    << id
    << "but NOT mounted yet";

    return id;
}

std::uint64_t
QtHost::create_text(rust::Str text)
{
    QQmlComponent component(
        &impl_->engine
    );

    // 注意：
    //
    // 这不是用户写的 UI。
    //
    // 这是 renderer 对
    //
    //     Dioxus Text
    //
    // 这个 primitive 的 Qt 实现。
    component.setData(
        R"QML(
import QtQuick

Text {
    x: 36
    y: 76

    color: "#17343b"

    font.pixelSize: 32
    font.weight: Font.DemiBold
}
)QML",
        QUrl()
    );

    if (component.isError()) {
        for (
            const auto& error :
            component.errors()
        ) {
            qWarning().noquote()
                << error.toString();
        }

        return 0;
    }


    QObject* object =
        component.create();

    if (object == nullptr) {
        qWarning()
            << "QQmlComponent::create failed";

        return 0;
    }


    auto* item =
        qobject_cast<QQuickItem*>(
            object
        );

    if (item == nullptr) {
        qWarning()
            << "created object is not QQuickItem";

        delete object;

        return 0;
    }


    object->setProperty(
        "text",
        to_qstring(text)
    );


    const std::uint64_t id =
        impl_->next_id++;


    impl_->objects.emplace(
        id,
        object
    );


    qDebug()
        << "[Qt] create Text id ="
        << id
        << "but NOT mounted yet";


    return id;
}


void
QtHost::append_to_root(
    std::uint64_t id
)
{
    const auto it =
        impl_->objects.find(id);

    if (
        it == impl_->objects.end() ||
        it->second == nullptr
    ) {
        qWarning()
            << "unknown object id"
            << id;

        return;
    }


    QObject* object =
        it->second;


    auto* item =
        qobject_cast<QQuickItem*>(
            object
        );

    if (item == nullptr) {
        return;
    }


    QQuickItem* root =
        impl_->window.contentItem();


    // -----------------------------
    // QObject ownership tree
    // -----------------------------

    object->setParent(root);


    // -----------------------------
    // Qt Quick visual tree
    // -----------------------------

    item->setParentItem(root);


    qDebug()
        << "[Qt] mounted object"
        << id
        << "into window";
}


void
QtHost::set_property(
    std::uint64_t id,
    rust::Str name,
    rust::Str value
)
{
    const auto it =
        impl_->objects.find(id);

    if (
        it == impl_->objects.end() ||
        it->second == nullptr
    ) {
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
        !it->second->setProperty(
            property.constData(),
            to_qstring(value)
        )
    ) {
        qWarning()
            << "[Qt] cannot set property"
            << property
            << "="
            << to_qstring(value);
    }
}