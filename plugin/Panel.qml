import QtQuick
import Quickshell
import Quickshell.Io
import qs.Commons
import qs.Commons as Commons
import qs.Ui
import "Model.js" as Model

// Fans: bar widget + popup for the omarchy-fand fan-curve daemon.
//
// Reads /run/omarchy-fans/status.json (world-readable, rewritten every
// second by the daemon) and sends changes through the `omarchy-fans` CLI,
// which talks to the daemon's wheel-only control socket.
//
//   left click    open the panel
//   right click   cycle Silent -> Balanced -> Performance
Panel {
  id: root

  moduleName: "qqq.fans"
  ipcTarget: "qqq.fans"
  manageIpc: false

  readonly property string statusPath: String(setting("statusPath", "/run/omarchy-fans/status.json"))
  readonly property string cli: String(setting("command", "omarchy-fans"))
  readonly property string barMode: String(setting("barMode", "RPM"))

  property var status: null
  property real nowMs: Date.now()
  readonly property bool alive: status !== null && Model.ageSeconds(status, nowMs) < 10
  readonly property var fans: Model.connectedFans(status)
  readonly property var sensors: status && status.sensors ? status.sensors : []
  readonly property var profiles: status && status.profiles ? status.profiles : []
  readonly property var tabs: Model.curveTabs(status)
  property string tabKey: ""
  readonly property var tab: Model.findTab(tabs, tabKey)
  readonly property bool calibrating: alive && !!status.calibration
  readonly property bool biosMode: alive && status.mode === "auto"
  readonly property bool stalled: alive && Model.hasStall(status)
  readonly property bool critical: alive && Model.hasCritical(status)

  property string error: ""
  property var queue: []
  property bool confirmCalibrate: false
  property bool cursorActive: false
  property int profileIndex: 0

  readonly property color fg: bar ? bar.barForeground : Commons.Color.foreground
  readonly property color accent: Commons.Color.accent
  readonly property color urgent: bar ? bar.urgent : Commons.Color.urgent
  readonly property color dim: Qt.darker(fg, 1.4)
  readonly property string fontFamily: bar ? bar.fontFamily : Style.font.family

  // ---- data --------------------------------------------------------------

  FileView {
    id: statusFile
    path: root.statusPath
    printErrors: false
    onLoaded: {
      var st = Model.parseStatus(text())
      if (st) root.status = st
    }
    onLoadFailed: root.status = null
  }

  Timer {
    interval: root.opened ? 1000 : 3000
    running: true
    repeat: true
    triggeredOnStart: true
    onTriggered: {
      root.nowMs = Date.now()
      statusFile.reload()
    }
  }

  // Commands run one at a time, in order, so a fast series of curve drags
  // lands on the daemon in the order it was made.
  function run(args) {
    queue = queue.concat([args])
    pump()
  }

  function pump() {
    if (cmdProc.running || queue.length === 0) return
    cmdProc.command = [root.cli].concat(queue[0])
    queue = queue.slice(1)
    cmdProc.running = true
  }

  Process {
    id: cmdProc
    stderr: StdioCollector {
      waitForEnd: true
      onStreamFinished: {
        var t = String(text || "").trim()
        if (t !== "") root.error = t.replace(/^omarchy-fans: /, "")
      }
    }
    onExited: function(exitCode) {
      if (exitCode === 0) root.error = ""
      else if (root.error === "") root.error = "omarchy-fans exited with " + exitCode
      statusFile.reload()
      Qt.callLater(root.pump)
    }
  }

  function setProfile(id) {
    if (!id) return
    run(["profile", id])
  }

  function cycleProfile() { run(["cycle"]) }

  function saveCurve(points) {
    if (!tab) return
    run(["curve", tab.role, tab.sensor, Model.pointsArg(points)])
  }

  function calibrate() {
    if (!confirmCalibrate) {
      confirmCalibrate = true
      confirmTimer.restart()
      return
    }
    confirmCalibrate = false
    run(["calibrate"])
  }

  Timer { id: confirmTimer; interval: 4000; onTriggered: root.confirmCalibrate = false }

  function selectProfileByDelta(delta) {
    if (profiles.length === 0) return
    profileIndex = (profileIndex + delta + profiles.length) % profiles.length
  }

  IpcHandler {
    target: "qqq.fans"
    function open(): void { root.open() }
    function close(): void { root.close() }
    function show(): void { root.open() }
    function hide(): void { root.close() }
    function toggle(): void { root.toggle() }
    function cycle(): void { root.cycleProfile() }
    function profile(name: string): void { root.setProfile(name) }
  }

  onOpenedChanged: {
    if (!opened) return
    statusFile.reload()
    cursorActive = false
    confirmCalibrate = false
    for (var i = 0; i < profiles.length; i++) if (profiles[i].id === status.profile) profileIndex = i
  }

  // ---- bar ---------------------------------------------------------------

  implicitWidth: barMode === "Icon" ? iconButton.implicitWidth : textButton.implicitWidth
  implicitHeight: barMode === "Icon" ? iconButton.implicitHeight : textButton.implicitHeight

  function barPressed(b) {
    if (b === Qt.RightButton) root.cycleProfile()
    else root.toggle()
  }

  WidgetButton {
    id: textButton
    anchors.fill: parent
    visible: root.barMode !== "Icon"
    bar: root.bar
    text: Model.barLabel(root.status, root.alive, root.barMode)
    fontSize: Style.font.caption
    horizontalMargin: 6
    active: root.stalled || root.critical || !root.alive
    activeColor: root.urgent
    tooltipText: Model.tooltip(root.status, root.alive)
    onPressed: function(b) { root.barPressed(b) }
  }

  BarIconButton {
    id: iconButton
    anchors.fill: parent
    visible: root.barMode === "Icon"
    bar: root.bar
    text: "󰈐"
    active: root.stalled || root.critical || !root.alive
    activeColor: root.urgent
    tooltipText: Model.tooltip(root.status, root.alive)
    onPressed: function(b) { root.barPressed(b) }
  }

  // ---- popup -------------------------------------------------------------

  KeyboardPanel {
    id: panel
    anchorItem: root.barMode === "Icon" ? iconButton : textButton
    owner: root
    bar: root.bar
    open: root.opened
    focusTarget: keyCatcher
    contentWidth: panel.fittedContentWidth(Style.space(470))
    contentHeight: panel.fittedContentHeight(column.implicitHeight)

    PanelKeyCatcher {
      id: keyCatcher
      anchors.fill: parent
      onMoveRequested: function(dx, dy) {
        if (!root.cursorActive) { root.cursorActive = true; return }
        root.selectProfileByDelta(dx !== 0 ? dx : dy)
      }
      onActivateRequested: if (root.cursorActive && root.profiles[root.profileIndex]) root.setProfile(root.profiles[root.profileIndex].id)
      onCloseRequested: root.close()
      onTabRequested: function(direction) { root.switchPanel(direction) }

      Column {
        id: column
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.top: parent.top
        spacing: Style.space(14)

        // ---- hero ----
        Item {
          width: parent.width
          implicitHeight: Math.max(heroIcon.implicitHeight, heroLabels.implicitHeight, heroTemp.implicitHeight)

          Text {
            id: heroIcon
            textFormat: Text.PlainText
            text: "󰈐"
            color: root.critical || root.stalled ? root.urgent : root.fg
            font.family: root.fontFamily
            font.pixelSize: Style.font.display
            anchors.left: parent.left
            anchors.verticalCenter: parent.verticalCenter
            // Spins in step with the CPU fan; a stopped fan stops the icon.
            readonly property real rpm: root.alive ? Model.cpuFanRpm(root.status) : 0
            RotationAnimator on rotation {
              running: root.opened && heroIcon.rpm > 0
              from: 0; to: 360
              loops: Animation.Infinite
              duration: Math.max(250, Math.min(6000, 1800000 / Math.max(1, heroIcon.rpm)))
            }
          }

          Column {
            id: heroLabels
            anchors.left: heroIcon.right
            anchors.leftMargin: Style.space(14)
            anchors.right: heroTemp.left
            anchors.rightMargin: Style.space(10)
            anchors.verticalCenter: parent.verticalCenter
            spacing: Style.space(2)

            Text {
              textFormat: Text.PlainText
              text: "Fans"
              color: root.fg
              font.family: root.fontFamily
              font.pixelSize: Style.font.title
              font.bold: true
            }
            Text {
              textFormat: Text.PlainText
              width: parent.width
              elide: Text.ElideRight
              text: (root.alive ? Model.profileLabel(root.status) + " · " + Model.modeLabel(root.status, root.alive) : "Daemon offline").toUpperCase()
              color: root.dim
              font.family: root.fontFamily
              font.pixelSize: Style.font.caption
              font.bold: true
              font.letterSpacing: 1.2
            }
          }

          Text {
            id: heroTemp
            textFormat: Text.PlainText
            text: Model.formatTemp(Model.sensorTemp(root.status, "cpu"))
            color: root.critical ? root.urgent : root.fg
            font.family: root.fontFamily
            font.pixelSize: Style.font.displayLarge
            font.bold: true
            anchors.right: parent.right
            anchors.verticalCenter: parent.verticalCenter
          }
        }

        // ---- offline help ----
        Text {
          visible: !root.alive
          width: parent.width
          wrapMode: Text.Wrap
          textFormat: Text.PlainText
          text: "omarchy-fand is not running. Start it with:\n  sudo systemctl enable --now omarchy-fand"
          color: root.dim
          font.family: root.fontFamily
          font.pixelSize: Style.font.bodySmall
        }

        // ---- sensors ----
        Flow {
          visible: root.alive
          width: parent.width
          spacing: Style.space(14)
          Repeater {
            model: root.sensors
            Row {
              required property var modelData
              spacing: Style.space(5)
              Text {
                textFormat: Text.PlainText
                text: modelData.label
                color: root.dim
                font.family: root.fontFamily
                font.pixelSize: Style.font.bodySmall
              }
              Text {
                textFormat: Text.PlainText
                text: modelData.ok ? Model.formatTemp(modelData.temp) : "lost"
                color: !modelData.ok || modelData.temp >= modelData.critical ? root.urgent : root.fg
                font.family: root.fontFamily
                font.pixelSize: Style.font.bodySmall
                font.bold: true
              }
            }
          }
        }

        // ---- fans ----
        Column {
          visible: root.alive && root.fans.length > 0
          width: parent.width
          spacing: Style.space(8)

          Repeater {
            model: root.fans
            Item {
              required property var modelData
              width: parent.width
              implicitHeight: fanName.implicitHeight + Style.space(4) + Style.space(6)

              Text {
                id: fanName
                textFormat: Text.PlainText
                text: modelData.label + (modelData.responsive === false ? "  ·  BIOS (ignores PWM)"
                  : (modelData.calibrated ? "" : "  ·  uncalibrated"))
                color: root.fg
                font.family: root.fontFamily
                font.pixelSize: Style.font.bodySmall
                anchors.left: parent.left
              }
              Text {
                textFormat: Text.PlainText
                text: modelData.stalled ? "STALLED" : (modelData.rpm === null ? "—" : modelData.rpm + " rpm")
                color: modelData.stalled ? root.urgent : root.fg
                font.family: root.fontFamily
                font.pixelSize: Style.font.bodySmall
                font.bold: true
                anchors.right: parent.right
              }
              Rectangle {
                id: track
                anchors.left: parent.left
                anchors.right: parent.right
                anchors.bottom: parent.bottom
                height: Style.space(5)
                radius: height / 2
                color: Qt.rgba(root.fg.r, root.fg.g, root.fg.b, 0.12)
                Rectangle {
                  height: parent.height
                  radius: parent.radius
                  color: modelData.stalled ? root.urgent : root.accent
                  // In BIOS mode there is no commanded percent; show RPM share.
                  readonly property real share: modelData.percent !== null && modelData.percent !== undefined
                    ? modelData.percent / 100
                    : (modelData.max_rpm ? (modelData.rpm || 0) / modelData.max_rpm : 0)
                  width: Math.max(height, parent.width * Math.max(0, Math.min(1, share)))
                  Behavior on width { NumberAnimation { duration: 400; easing.type: Easing.OutCubic } }
                }
              }
            }
          }
        }

        Text {
          visible: root.alive && root.fans.length === 0
          width: parent.width
          wrapMode: Text.Wrap
          textFormat: Text.PlainText
          text: "No controllable fans found. Is the it87 module loaded?"
          color: root.dim
          font.family: root.fontFamily
          font.pixelSize: Style.font.bodySmall
        }

        // ---- profiles ----
        PanelSeparator { visible: root.alive; foreground: root.fg }

        Column {
          visible: root.alive
          width: parent.width
          spacing: Style.space(10)

          PanelSectionHeader {
            text: "PROFILE"
            foreground: root.fg
            fontFamily: root.fontFamily
          }

          Row {
            id: profileRow
            width: parent.width
            spacing: Style.space(6)
            readonly property real cellWidth: root.profiles.length > 0
              ? (width - spacing * (root.profiles.length - 1)) / root.profiles.length : 0

            Repeater {
              model: root.profiles
              Button {
                required property var modelData
                required property int index
                width: profileRow.cellWidth
                iconText: Model.PROFILE_ICONS[modelData.id] || "󰈐"
                iconSize: Style.font.title
                text: modelData.label
                fontSize: Style.font.caption
                foreground: root.fg
                fontFamily: root.fontFamily
                horizontalPadding: Style.space(4)
                verticalPadding: Style.spacing.controlPaddingY + Style.space(2)
                bordered: true
                active: !root.biosMode && root.status && root.status.profile === modelData.id
                hasCursor: root.cursorActive && root.profileIndex === index
                onClicked: root.setProfile(modelData.id)
                onHovered: function(h) {
                  if (h) { root.cursorActive = true; root.profileIndex = index }
                }
              }
            }
          }
        }

        // ---- curve editor ----
        Column {
          visible: root.alive && root.tab !== null
          width: parent.width
          spacing: Style.space(8)

          PanelSectionHeader {
            text: "CURVE"
            foreground: root.fg
            fontFamily: root.fontFamily
          }

          ButtonGroup {
            width: parent.width
            options: root.tabs.map(function(t) { return { value: t.key, label: t.label } })
            value: root.tab ? root.tab.key : ""
            foreground: root.fg
            fontFamily: root.fontFamily
            fontSize: Style.font.caption
            focusable: false
            onChanged: function(v) { root.tabKey = v }
          }

          CurveEditor {
            width: parent.width
            points: root.tab ? root.tab.points : []
            currentTemp: root.tab ? Number((Model.sensor(root.status, root.tab.sensor) || {}).held) : NaN
            foreground: root.fg
            accent: root.accent
            fontFamily: root.fontFamily
            editable: !root.calibrating
            onEdited: function(points) { root.saveCurve(points) }
          }

          Text {
            width: parent.width
            wrapMode: Text.Wrap
            textFormat: Text.PlainText
            text: root.status && root.status.profile !== "custom"
              ? "Drag a point to make a Custom profile from " + Model.profileLabel(root.status) + ". Double-click adds, right-click removes."
              : "Drag points to edit. Double-click adds a point, right-click removes one."
            color: root.dim
            font.family: root.fontFamily
            font.pixelSize: Style.font.caption
          }
        }

        // ---- actions ----
        PanelSeparator { visible: root.alive; foreground: root.fg }

        Row {
          visible: root.alive
          width: parent.width
          spacing: Style.space(6)

          Button {
            width: (parent.width - parent.spacing) / 2
            iconText: root.calibrating ? "󰑓" : "󰔛"
            iconSpinning: root.calibrating
            text: root.calibrating
              ? (root.status.calibration.step || "Calibrating")
              : (root.confirmCalibrate ? "Fans stop briefly — click again" : "Calibrate fans")
            tooltipText: "Measures each fan's stop, start and RPM response (~2 min) so curves are quiet and RPM-accurate"
            fontSize: Style.font.caption
            foreground: root.confirmCalibrate ? root.urgent : root.fg
            fontFamily: root.fontFamily
            bordered: true
            enabled: !root.calibrating
            onClicked: root.calibrate()
          }

          Button {
            width: (parent.width - parent.spacing) / 2
            iconText: root.biosMode ? "󰒓" : "󰘚"
            text: root.biosMode ? "Use fan curves" : "Hand to BIOS"
            tooltipText: root.biosMode ? "Take control with the selected profile" : "Let the motherboard's own fan curve run the fans"
            fontSize: Style.font.caption
            foreground: root.fg
            fontFamily: root.fontFamily
            bordered: true
            active: root.biosMode
            enabled: !root.calibrating
            onClicked: root.run([root.biosMode ? "resume" : "auto"])
          }
        }

        Text {
          readonly property string msg: root.error !== "" ? root.error : (root.alive ? (root.status.message || "") : "")
          visible: msg !== ""
          width: parent.width
          wrapMode: Text.Wrap
          textFormat: Text.PlainText
          text: msg
          color: root.error !== "" || msg.indexOf("failed") >= 0 ? root.urgent : root.dim
          font.family: root.fontFamily
          font.pixelSize: Style.font.caption
        }
      }
    }
  }
}
