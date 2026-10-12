#pragma once

#include "rust/cxx.h"

#include <cstdint>
#include <memory>

class QObject;

class QtHost {
public:
    QtHost();
    ~QtHost();

    void show();

    // 每个 create_* 都是「把一段 QML 组件实例化成 Qt 对象」。
    // 刚造出来的对象还没有 parent，也还没进 visual tree。
    std::uint64_t create_rectangle();

    std::uint64_t create_text(rust::Str text);

    // 把一个对象挂到父对象下面。
    //
    // parent_id == 0 表示窗口根（window.contentItem()）。
    // 这一步才真正进入 Qt 的 visual tree。
    void mount(
        std::uint64_t parent_id,
        std::uint64_t child_id
    );

    // 在 stacking 顺序上把 child 排到 sibling 前面。
    // 让 Qt 的子节点顺序跟 Dioxus 模板里的顺序一致。
    void stack_before(
        std::uint64_t child_id,
        std::uint64_t sibling_id
    );

    // 把 Dioxus 侧送来的属性写到 Qt 对象上。
    // 目前就是 QObject::setProperty(name, value)，
    // 属性名与 Qt 属性名同名（Dioxus 的 text -> QtQuick.Text::text）。
    void set_property(
        std::uint64_t id,
        rust::Str name,
        rust::Str value
    );

private:
    QObject* object_for(
        std::uint64_t id
    ) const;

    struct Impl;
    std::unique_ptr<Impl> impl_;
};

std::unique_ptr<QtHost> new_qt_host();