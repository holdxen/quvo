#pragma once

#include "rust/cxx.h"

#include <cstdint>
#include <memory>

class QQuickItem;
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

    // 窗口根 = window.contentItem()。
    //
    // 以前 Rust 侧用约定的 id 0 表示「挂到窗口根」；
    // id 没了之后，这里把真实指针交出去，
    // Rust 那边 node 0 的 object 就是它。
    QQuickItem* root_item() const;

    // 每个 create_* 都是「把一段 QML 组件实例化成 Qt 对象」。
    // 刚造出来的对象还没有 parent，也还没进 visual tree。
    //
    // 返回值就是这个对象的裸指针，Rust 侧直接拿它当句柄
    // （以前是一个 id，C++ 还要查 map 换回指针）。
    // 失败返回 nullptr。
    QQuickItem* create_rectangle();

    QQuickItem* create_text(rust::Str text);

    // 一个裸 Item：只当容器，不带任何 QML Layout / anchors。
    // 它的位置和大小完全由 Rust 侧的布局器给（见 set_geometry）。
    QQuickItem* create_column();

    // 把一个对象挂到父对象下面。
    //
    // 这一步才真正进入 Qt 的 visual tree。
    // 「窗口根」不再是约定值：Rust 侧挂根时用 root_item() 拿到的
    // 那个真实 contentItem 指针当 parent。
    void mount(
        QQuickItem* parent,
        QQuickItem* child
    );

    // 把这个对象放到 parent 的孩子列表里的指定位置。
    //
    // 为什么 mount 不够：mount 里的 setParentItem 只有「追加到末尾」这一种
    // 落点，而且对**同一个父亲**是直接 return（qquickitem.cpp:2757-2758）。
    // keyed 列表的「移动」走的正是这种调用 —— 对象本来就在父亲名下，
    // 只是要换个位置。只调 mount 的话 Qt 那边会原地不动，
    // 我们账本里的顺序却已经改了，两边就错开了（child_order 能看见）。
    //
    // sibling 非空：stackBefore(sibling)，插在它前面。
    // sibling 为空：放到末尾。这里不能写 stackBefore(nullptr) ——
    // Qt 对空 sibling 是 qWarning + 什么都不做（qquickitem.cpp:2925-2929），
    // 只能先摘下来再挂回去（重新 add 就是追加）。
    void place(
        QQuickItem* parent,
        QQuickItem* child,
        QQuickItem* sibling
    );

    // 把这个对象从两棵树上摘掉，并安排释放。
    //
    // 只在「这棵子树整个不要了」时调用一次 —— 不用逐个孩子调用：
    // mount 时我们 setParent 过，孩子是 QObject 树里的人，
    // ~QObject 会递归删（而 ~QQuickItem 只把 childItems 摘干净，
    //  不负责删它们，见 qquickitem.cpp:2409）。
    //
    // Rust 侧调用完就把这些节点的账全忘了，所以指针不会被人再碰。
    void destroy(
        QQuickItem* item
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

    // -----------------------------
    // 输入事件
    // -----------------------------
    //
    // 为什么用 event filter 而不是 MouseArea / PointerHandler：
    // 我们的节点是从内联 QML 实例化出来的裸 QQuickItem，
    // 它本身没有 clicked 这种信号可以 connect；
    // 而 MouseArea 得塞进 QML 里，就成了「谁的几何说话」的第二个作者，
    // 还会往 childItems 里多插一个我们账本上没有的节点
    // （child_order 的交叉检查立刻会 ✗）。
    // filter 装在 item 上：不占 visual tree，不吃事件（返回 false），
    // 只负责把 Qt 的那一套 QEvent 翻译成 dioxus 的事件送过去。
    //
    // 一个 (item, name) 一个 filter。名字决定这一支吃什么 QEvent、
    // 送哪一支 Rust 回调、以及这个 item 要打开哪些输入能力
    // （acceptedMouseButtons / acceptHoverEvents / acceptTouchEvents /
    //  focusPolicy）—— 全在 cpp 里的 EVENT_SPECS 那张表上。
    //
    // 「click」的判据照抄 QQuickMouseArea：按下记下、在同一个元素里松开才算
    // （对照见 ClickFilter/InputFilter 的注释）。
    void add_event_listener(
        QQuickItem* item,
        rust::Str name,
        std::uint32_t element
    );

    // 撤掉 add_event_listener 装的那个 filter（removeEventFilter + delete）。
    // filter 的 parent 是 item，所以节点被删时它自己跟着走；
    // 登记簿那一格由 filter 自己的 destroyed 槽摘掉（destroy 只清得到的了根，
    // 子树里孩子的得靠这个）。撤完还要重算一次这个 item 的能力开关。
    void remove_event_listener(
        QQuickItem* item,
        rust::Str name
    );

    // 把 Dioxus 侧送来的属性写到 Qt 对象上。
    // 就是 QObject::setProperty(name, value)，
    // 属性名与 Qt 属性名同名（Dioxus 的 text -> QtQuick.Text::text）。
    //
    // value 是带类型的 QVariant（QString / qlonglong / double / bool），
    // 类型在 Rust 侧按 AttributeValue 的变体包好（variant_of）。
    //
    // 第一次写某个名字之前，会把该名字此刻的值抄存成这个对象的一条
    // **动态属性**（键名 __dq_saved_<name>），给 clear_property 用。
    // 动态属性跟着 QObject 一起死，所以不需要另建一张 id -> 值 的表，
    // 也不会被 destroy 只删子树根那件事漏掉。
    void set_property(
        QQuickItem *item,
        rust::Str name,
        const QVariant &value
    );

    // 「这个属性被撤掉了」——AttributeValue::None 的落点。
    //
    // 语义：回到该对象刚建出来时的值，也就是 set_property 抄存下来的那一份。
    // 这就是 Web 那边「属性没了」的意思：不是变成 0，也不是变成空串，
    // 是回到没写过它时的那个状态。
    //
    // 为什么不能用现成的 QObject::resetProperty：它要求属性在 Q_PROPERTY 里
    // 声明 RESET，而我们用的那几个全都没有声明
    // （Rectangle::color qquickrectangle_p.h:133、Text::text qquicktext_p.h:33、
    //  Item::visible/opacity qquickitem.h:79-81），对它们 resetProperty
    // 只会 qWarning 然后返回 false。
    //
    // 没抄存过（这个名字我们从来没写过）就是无事发生，日志会说明是这种。
    void clear_property(
        QQuickItem *item,
        rust::Str name
    );

    // 量一个 item 需要多大 —— 文本的**唯一来源就是这个 item**，
    // Rust 那边不再另存一份字符串。
    //
    // 判据是「它自己有没有一段要显示的文字」：有没有一个能读成 QString 的
    // text 属性（走 meta-object，因为 brew 不发 QQuickText 的头，
    // qobject_cast 用不了）。不上手就返回 {0, 0}
    // （Rectangle / Item 的尺寸本该由 style 决定），
    // 文本为空也返回 {0, 0} —— QTextLayout 对 "" 照样吐一行高，
    // 少了这层判断，一个空标签会白占一行。
    //
    // max_width <= 0 表示不限宽（对应 QQuickText 里 widthValid() == false
    // 时把 lineWidth 设成 FLT_MAX 那条分支）。
    //
    // 注意这里**不读** implicitWidth/Height：那两个值折行用的是 item 当前的
    // width（qquicktext.cpp:834），而那个 width 正是布局要决定的东西。
    TextMeasure measure_item(
        QQuickItem *item,
        double max_width
    ) const;

    // 这个 item 是不是「有内容可量的文本项」。
    // 布局本身不需要它（量不出来自然返回 0x0），
    // 只有 verify_text_measurement 的对照需要，免得把 Rectangle 也拿去比。
    bool is_text_item(
        QQuickItem *item
    ) const;

    // 读一个已存在对象的 implicit size —— 只为了做对照。
    // 读它会让 QQuickText 同步算一次尺寸（见 getImplicitWidth）。
    double implicit_width(
        QQuickItem *item
    ) const;

    double implicit_height(
        QQuickItem *item
    ) const;

    // 诊断用：把 Qt 侧 childItems 的实际顺序（对象地址列表）交出去。
    //
    // 为什么要单独读：布局是按我们自己的 children 顺序给坐标的，
    // 而这里几个矩形互不重叠 —— stack_before 排错了，
    // 坐标和画面都看不出任何差别。
    // 想验证「插中间」真的生效，只能把 Qt 的顺序本身读出来对。
    rust::Vec<std::uint64_t> child_order(
        QQuickItem* parent
    ) const;


    void process_events(std::int32_t ms) const;

private:
    struct Impl;
    std::unique_ptr<Impl> impl_;
};

std::unique_ptr<QtHost> new_qt_host();


// 把「该 poll 一次 wait_for_work 了」塞进 Qt 的事件队列。
//
// 谁来调：Rust 的 waker（Wake::wake_by_ref）。dioxus 发纸条的那一刻就会调它
// （futures-channel：unbounded_send → do_send_nb → queue_push_and_signal
//  → recv_task.wake()，mpsc/mod.rs:457），而 recv_task 就是我们上次 poll 时
// 通过 Context 交进去的那个 waker。
//
// 为什么是全局函数而不是 QtHost 的成员：waker 手里没有 UniquePtr<QtHost> 可用
// —— 它被调用的那一刻，Pump 正被 borrow_mut 借着（host 就在 Pump 里面），
// 想拿 host 就是二次借用。自由函数不需要任何接收者，也就绕开了这件事。
//
// 为什么只能「排队」不能当场调 ::qt_pump_poll()：wake() 是在
// runtime.handle_event(...) 那一行**里面**被同步调进来的
// （listener 写 signal → needs_update → unbounded_send → wake），
// 那一刻 qt_handle_event 正借着 PUMP。
// QueuedConnection 把它推到本次回调返回之后由事件循环执行，
// 重入被语义挡掉。
//
// 于是原来的 interval 轮询整个删掉了：空闲时 Qt 阻塞在 exec() 里，
// 一次 poll 都不会发生。
void schedule_pump_poll();