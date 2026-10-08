import QtQuick
import QtQuick.Window
import com.example.dioxus_qml 1.0

Window {
    width: 420
    height: 220

    visible: true

    title: "Dioxus → Qt Quick"

    color: "#f5f0e8"


    // ========================================================
    // 这是 Rust 实现的 QObject
    // ========================================================

    DioxusBackend {
        id: backend
    }


    Rectangle {
        width: 260
        height: 96

        anchors.centerIn: parent

        radius: 18

        color: mouseArea.pressed
            ? "#bd6848"
            : "#d97757"


        Text {
            anchors.centerIn: parent

            // =================================================
            // Rust QObject Q_PROPERTY
            // =================================================

            text:
                "Dioxus count: "
                + backend.text

            color: "#17343b"

            font.pixelSize: 22
            font.weight: Font.DemiBold
        }


        MouseArea {
            id: mouseArea

            anchors.fill: parent

            cursorShape:
                Qt.PointingHandCursor


            // =================================================
            // Qt → Rust → Dioxus
            // =================================================

            onClicked: {
                backend.click()
            }
        }
    }
}