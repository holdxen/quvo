#pragma once

#include "rust/cxx.h"

#include <cstdint>
#include <memory>

class QtHost {
public:
    QtHost();
    ~QtHost();

    void show();

    // 创建 QtQuick.Text，但暂时不挂到窗口。
    std::uint64_t create_text(rust::Str text);

    // 真正加入 visual tree。
    void append_to_root(std::uint64_t id);

    // 为下一步动态文本更新准备。
    void set_text(std::uint64_t id, rust::Str text);

private:
    struct Impl;
    std::unique_ptr<Impl> impl_;
};

std::unique_ptr<QtHost> new_qt_host();