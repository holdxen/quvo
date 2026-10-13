#pragma once

#include "rust/cxx.h"

#include <cstdint>
#include <memory>

class QQuickItem;
class QObject;
class QVariant;

// 文本测量结果，和 QQuickText 的 implicitWidth / implicitHeight 同一口径。
//
// 定义不在这里：bridge 里的 shared struct 由 cxx 生成
// （target/.../test_dioxus_qt/src/main.cxx.h），这里只前置声明，
// 需要完整类型的 .cpp 自己 include 那个生成头。
// 重复定义会撞出 "redefinition of 'TextMeasure'"。
struct TextMeasure;

class QtHost {
public:
    QtHost();
    ~QtHost();

    void show();

    // 每个 create_* 都是「把一段 QML 组件实例化成 Qt 对象」。
    // 刚造出来的对象还没有 parent，也还没进 visual tree。
    QQuickItem* create_rectangle();

    QQuickItem* create_text(rust::Str text);

    // 一个裸 Item：只当容器，不带任何 QML Layout / anchors。
    // 它的位置和大小完全由 Rust 侧的布局器给（见 set_geometry）。
    QQuickItem* create_column();

    // 把一个对象挂到父对象下面。
    //
    // parent_id == 0 表示窗口根（window.contentItem()）。
    // 这一步才真正进入 Qt 的 visual tree。
    void mount(
        QQuickItem* parent,
        QQuickItem* child
    );

    // 布局结果写给一个 QQuickItem。
    //
    // 这是几何的唯一入口：不走 set_property 的 QString 强转，
    // 直接 setX/setY/setWidth/setHeight。
    void set_geometry(
        QQuickItem* item,
        double x,
        double y,
        double width,
        double height
    );

    // 视口尺寸由 Rust 侧来问，而不是两边各写一个数字。
    double window_width() const;

    double window_height() const;

    // 在 stacking 顺序上把 child 排到 sibling 前面。
    // 让 Qt 的子节点顺序跟 Dioxus 模板里的顺序一致。
    void stack_before(
        QQuickItem* child,
        QQuickItem* sibling
    );

    // 把 Dioxus 侧送来的属性写到 Qt 对象上。
    // 目前就是 QObject::setProperty(name, value)，
    // 属性名与 Qt 属性名同名（Dioxus 的 text -> QtQuick.Text::text）。
    void set_property(
        QObject *obj,
        rust::Str name,
        const QVariant &value
    );

    // 量一段文本需要多大。
    //
    // max_width <= 0 表示不限宽（对应 QQuickText 里 widthValid() == false
    // 时把 lineWidth 设成 FLT_MAX 那条分支）。
    TextMeasure measure_text(
        rust::Str text,
        double max_width
    ) const;

    // 读一个已存在对象的 implicit size —— 只为了做对照。
    // 读它会让 QQuickText 同步算一次尺寸（见 getImplicitWidth）。
    double implicit_width(
        QQuickItem *item
    ) const;

    double implicit_height(
        QQuickItem *item
    ) const;

private:
    QObject* object_for(
        std::uint64_t id
    ) const;

    struct Impl;
    std::unique_ptr<Impl> impl_;
};

std::unique_ptr<QtHost> new_qt_host();