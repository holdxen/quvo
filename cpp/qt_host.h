#pragma once

#include "rust/cxx.h"

#include <cstdint>
#include <memory>

class QtHost {
public:
    QtHost();
    ~QtHost();

    void show();

    std::uint64_t create_rectangle();
    // 创建 QtQuick.Text，但暂时不挂到窗口。
    std::uint64_t create_text(rust::Str text);

    // 真正加入 visual tree。
    void append_to_root(std::uint64_t id);

    // 把 Dioxus 侧送来的属性写到 Qt 对象上。
    // 目前就是 QObject::setProperty(name, value)，
    // 属性名与 Qt 属性名同名（Dioxus 的 text -> QtQuick.Text::text）。
    void set_property(
        std::uint64_t id,
        rust::Str name,
        rust::Str value
    );

private:
    struct Impl;
    std::unique_ptr<Impl> impl_;
};

std::unique_ptr<QtHost> new_qt_host();