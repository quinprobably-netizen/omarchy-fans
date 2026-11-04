import QtQuick
import qs.Commons
import "Model.js" as Model

// Temperature -> fan speed chart with draggable points.
//
//   drag a point      move it (kept monotonic, same rules as the daemon)
//   double-click      add a point
//   right-click       remove a point (keeps at least two)
//
// Edits are reported through `edited(points)`; the panel sends them to the
// daemon, which forks the active profile into "custom" on the first edit.
Item {
  id: root

  property var points: []
  property real currentTemp: NaN
  property color foreground: Color.foreground
  property color accent: Color.accent
  property string fontFamily: Style.font.family
  property bool editable: true

  // While dragging (and briefly after, until the daemon echoes the new
  // curve back) the chart shows the local draft instead of `points`.
  property var draft: null
  property int dragIndex: -1
  property int hoverIndex: -1
  readonly property var shown: draft || points || []

  signal edited(var points)

  implicitHeight: Style.space(176)

  readonly property real padL: Style.space(32)
  readonly property real padR: Style.space(16)
  readonly property real padT: Style.space(10)
  readonly property real padB: Style.space(20)
  readonly property real plotW: Math.max(1, width - padL - padR)
  readonly property real plotH: Math.max(1, height - padT - padB)
  readonly property real handle: Style.space(9)

  function xOf(t) { return padL + (t - Model.T_MIN) / (Model.T_MAX - Model.T_MIN) * plotW }
  function yOf(p) { return padT + (1 - p / 100) * plotH }
  function tOf(x) { return Model.T_MIN + (x - padL) / plotW * (Model.T_MAX - Model.T_MIN) }
  function pOf(y) { return (1 - (y - padT) / plotH) * 100 }

  function hit(mx, my) {
    var best = -1, bestD = handle * 1.6
    for (var i = 0; i < shown.length; i++) {
      var d = Math.hypot(xOf(shown[i][0]) - mx, yOf(shown[i][1]) - my)
      if (d < bestD) { best = i; bestD = d }
    }
    return best
  }

  onPointsChanged: {
    // The daemon echoed our edit back (or something else changed it):
    // drop the draft unless a drag is still in progress.
    if (dragIndex < 0 && draft && JSON.stringify(draft) === JSON.stringify(points)) draft = null
    canvas.requestPaint()
  }
  onShownChanged: canvas.requestPaint()
  onCurrentTempChanged: canvas.requestPaint()
  onForegroundChanged: canvas.requestPaint()
  onAccentChanged: canvas.requestPaint()
  onWidthChanged: canvas.requestPaint()
  onHeightChanged: canvas.requestPaint()

  // Never leave a stale draft on screen if the daemon rejected the edit.
  Timer {
    id: draftTimeout
    interval: 4000
    onTriggered: if (root.dragIndex < 0) root.draft = null
  }

  Canvas {
    id: canvas
    anchors.fill: parent

    onPaint: {
      var ctx = getContext("2d")
      ctx.reset()
      var fg = root.foreground, ac = root.accent
      var pts = root.shown
      ctx.font = Math.round(Style.font.caption) + "px '" + root.fontFamily + "'"

      // grid + axis labels
      ctx.lineWidth = 1
      ctx.strokeStyle = Qt.rgba(fg.r, fg.g, fg.b, 0.10)
      ctx.fillStyle = Qt.rgba(fg.r, fg.g, fg.b, 0.55)
      ctx.textAlign = "center"
      ctx.textBaseline = "top"
      for (var t = Model.T_MIN; t <= Model.T_MAX; t += 10) {
        var gx = Math.round(root.xOf(t)) + 0.5
        ctx.beginPath(); ctx.moveTo(gx, root.padT); ctx.lineTo(gx, root.padT + root.plotH); ctx.stroke()
        if ((t - Model.T_MIN) % 20 === 0) ctx.fillText(t + "°", gx, root.padT + root.plotH + Style.space(4))
      }
      ctx.textAlign = "right"
      ctx.textBaseline = "middle"
      for (var p = 0; p <= 100; p += 25) {
        var gy = Math.round(root.yOf(p)) + 0.5
        ctx.beginPath(); ctx.moveTo(root.padL, gy); ctx.lineTo(root.padL + root.plotW, gy); ctx.stroke()
        ctx.fillText(p + "%", root.padL - Style.space(5), gy)
      }
      if (!pts || pts.length === 0) return

      // curve, extended flat to both chart edges like the daemon evaluates it
      function trace() {
        ctx.moveTo(root.xOf(Model.T_MIN), root.yOf(Model.interp(pts, Model.T_MIN)))
        for (var i = 0; i < pts.length; i++) {
          if (pts[i][0] > Model.T_MIN && pts[i][0] < Model.T_MAX) ctx.lineTo(root.xOf(pts[i][0]), root.yOf(pts[i][1]))
        }
        ctx.lineTo(root.xOf(Model.T_MAX), root.yOf(Model.interp(pts, Model.T_MAX)))
      }
      ctx.beginPath()
      trace()
      ctx.lineTo(root.xOf(Model.T_MAX), root.yOf(0))
      ctx.lineTo(root.xOf(Model.T_MIN), root.yOf(0))
      ctx.closePath()
      ctx.fillStyle = Qt.rgba(ac.r, ac.g, ac.b, 0.14)
      ctx.fill()
      ctx.beginPath()
      trace()
      ctx.lineWidth = Math.max(2, Style.space(2))
      ctx.strokeStyle = ac
      ctx.stroke()

      // live operating point
      if (isFinite(root.currentTemp)) {
        var ct = Math.max(Model.T_MIN, Math.min(Model.T_MAX, root.currentTemp))
        var cx = root.xOf(ct), cy = root.yOf(Model.interp(pts, root.currentTemp))
        ctx.setLineDash([3, 3])
        ctx.lineWidth = 1
        ctx.strokeStyle = Qt.rgba(fg.r, fg.g, fg.b, 0.5)
        ctx.beginPath(); ctx.moveTo(cx, root.padT); ctx.lineTo(cx, root.padT + root.plotH); ctx.stroke()
        ctx.setLineDash([])
        ctx.fillStyle = fg
        ctx.beginPath(); ctx.arc(cx, cy, Style.space(4), 0, Math.PI * 2); ctx.fill()
      }
    }
  }

  Repeater {
    model: root.shown.length
    Rectangle {
      required property int index
      readonly property var pt: root.shown[index] || [0, 0]
      readonly property bool hot: index === root.dragIndex || index === root.hoverIndex
      x: root.xOf(Math.max(Model.T_MIN, Math.min(Model.T_MAX, pt[0]))) - width / 2
      y: root.yOf(pt[1]) - height / 2
      width: hot ? root.handle * 1.4 : root.handle
      height: width
      radius: width / 2
      color: hot ? root.accent : Color.popups.background
      border.width: Math.max(2, Style.space(2))
      border.color: root.accent
      visible: root.editable || hot
    }
  }

  // value readout for the point under the cursor
  Rectangle {
    id: readout
    readonly property int idx: root.dragIndex >= 0 ? root.dragIndex : root.hoverIndex
    readonly property var pt: idx >= 0 ? root.shown[idx] : null
    readonly property bool has: pt !== null && pt !== undefined
    visible: has
    color: Color.tooltip.background
    border.color: Qt.rgba(root.foreground.r, root.foreground.g, root.foreground.b, 0.3)
    border.width: 1
    radius: Style.cornerRadius
    width: readoutText.implicitWidth + Style.space(10)
    height: readoutText.implicitHeight + Style.space(4)
    x: has ? Math.min(root.width - width, Math.max(0, root.xOf(pt[0]) - width / 2)) : 0
    y: has ? Math.max(0, root.yOf(pt[1]) - height - root.handle) : 0

    Text {
      id: readoutText
      anchors.centerIn: parent
      textFormat: Text.PlainText
      text: readout.pt ? readout.pt[0] + "° → " + readout.pt[1] + "%" : ""
      color: Color.tooltip.text
      font.family: root.fontFamily
      font.pixelSize: Style.font.caption
      font.bold: true
    }
  }

  MouseArea {
    anchors.fill: parent
    enabled: root.editable
    hoverEnabled: true
    acceptedButtons: Qt.LeftButton | Qt.RightButton
    cursorShape: root.dragIndex >= 0 ? Qt.ClosedHandCursor : (root.hoverIndex >= 0 ? Qt.OpenHandCursor : Qt.CrossCursor)

    onPressed: function(mouse) {
      var idx = root.hit(mouse.x, mouse.y)
      if (mouse.button === Qt.RightButton) {
        if (idx >= 0 && root.shown.length > 2) {
          root.draft = Model.removePoint(root.shown, idx)
          root.hoverIndex = -1
          draftTimeout.restart()
          root.edited(root.draft)
        }
        return
      }
      if (idx >= 0) {
        root.dragIndex = idx
        root.draft = Model.clonePoints(root.shown)
      }
    }
    onPositionChanged: function(mouse) {
      if (root.dragIndex >= 0) {
        root.draft = Model.movePoint(root.draft, root.dragIndex, root.tOf(mouse.x), root.pOf(mouse.y))
      } else {
        root.hoverIndex = root.hit(mouse.x, mouse.y)
      }
    }
    onReleased: function(mouse) {
      if (root.dragIndex < 0) return
      var changed = JSON.stringify(root.draft) !== JSON.stringify(root.points)
      root.dragIndex = -1
      if (changed) {
        draftTimeout.restart()
        root.edited(root.draft)
      } else {
        root.draft = null
      }
    }
    onDoubleClicked: function(mouse) {
      if (root.hit(mouse.x, mouse.y) >= 0 || root.shown.length >= 8) return
      var t = root.tOf(mouse.x)
      if (t < Model.T_MIN || t > Model.T_MAX) return
      root.draft = Model.addPoint(root.shown, t, root.pOf(mouse.y))
      draftTimeout.restart()
      root.edited(root.draft)
    }
    onExited: if (root.dragIndex < 0) root.hoverIndex = -1
  }
}
