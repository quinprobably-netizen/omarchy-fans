// Pure helpers for the fans panel. No QML types here so tests/model.test.js
// can load this file under node.

var ROLE_LABELS = { cpu: "CPU fan", case: "Case", gpu: "GPU fan" }
var ROLE_SHORT = { cpu: "CPU", case: "Case", gpu: "GPU" }
var PROFILE_ICONS = { silent: "󰤄", balanced: "󰾅", performance: "󰓅", full: "󰈐", custom: "󰒓" }

// Chart range. Below 20°C nothing on a desktop is interesting; above 100°C
// everything is already at full speed.
var T_MIN = 20
var T_MAX = 100

function parseStatus(text) {
  if (!text) return null
  try {
    var st = JSON.parse(text)
    if (!st || typeof st !== "object" || !Array.isArray(st.fans)) return null
    return st
  } catch (e) {
    return null
  }
}

// Seconds since the daemon last wrote status; a stale file means it died.
function ageSeconds(st, nowMs) {
  if (!st || !isFinite(Number(st.time))) return Infinity
  return nowMs / 1000 - Number(st.time)
}

function sensor(st, id) {
  if (!st || !st.sensors) return null
  for (var i = 0; i < st.sensors.length; i++) if (st.sensors[i].id === id) return st.sensors[i]
  return null
}

function sensorTemp(st, id) {
  var s = sensor(st, id)
  return s && s.temp !== null && s.temp !== undefined ? Number(s.temp) : NaN
}

function formatTemp(value) {
  return isFinite(value) ? Math.round(value) + "°" : "—"
}

function connectedFans(st) {
  if (!st || !st.fans) return []
  // Uncalibrated headers that have never spun are almost always empty.
  // The GPU fan is always present; it rests at 0 rpm when the card is cool.
  return st.fans.filter(function(f) {
    return f.connected && (f.kind === "gpu" || f.calibrated || f.seen_spinning || f.rpm > 0)
  })
}

// One editor tab per (role, sensor) curve in the active profile, limited to
// roles that actually have a connected fan and sensors present on this box.
function curveTabs(st) {
  var tabs = []
  if (!st || !st.curves) return tabs
  var fans = connectedFans(st)
  var roles = ["cpu", "case", "gpu"]
  for (var r = 0; r < roles.length; r++) {
    var role = roles[r]
    if (!fans.some(function(f) { return f.role === role })) continue
    var curves = st.curves[role] || {}
    var sensorIds = Object.keys(curves).sort(function(a, b) { return a === "cpu" ? -1 : b === "cpu" ? 1 : a < b ? -1 : 1 })
    for (var s = 0; s < sensorIds.length; s++) {
      var sid = sensorIds[s]
      var sen = sensor(st, sid)
      if (!sen) continue
      tabs.push({
        key: role + ":" + sid,
        role: role,
        sensor: sid,
        // Compact so five tabs fit: "CPU fan", "Case ← GPU", "GPU ← VRAM".
        label: sid === role ? ROLE_LABELS[role] : ROLE_SHORT[role] + " ← " + sen.label,
        points: curves[sid]
      })
    }
  }
  return tabs
}

function findTab(tabs, key) {
  for (var i = 0; i < tabs.length; i++) if (tabs[i].key === key) return tabs[i]
  return tabs.length > 0 ? tabs[0] : null
}

function interp(points, x) {
  if (!points || points.length === 0) return 0
  if (x <= points[0][0]) return points[0][1]
  for (var i = 1; i < points.length; i++) {
    var a = points[i - 1], b = points[i]
    if (x <= b[0]) return b[0] === a[0] ? b[1] : a[1] + (b[1] - a[1]) * (x - a[0]) / (b[0] - a[0])
  }
  return points[points.length - 1][1]
}

function clonePoints(points) {
  return (points || []).map(function(p) { return [Number(p[0]), Number(p[1])] })
}

// Move point `index` to (t, p), clamped so temps stay strictly increasing
// (1°C apart) and percent never decreases — the same rules the daemon
// enforces, so a drag can never produce a curve it would reject.
function movePoint(points, index, t, p) {
  var pts = clonePoints(points)
  if (index < 0 || index >= pts.length) return pts
  var tLo = index > 0 ? pts[index - 1][0] + 1 : 0
  var tHi = index < pts.length - 1 ? pts[index + 1][0] - 1 : 110
  var pLo = index > 0 ? pts[index - 1][1] : 0
  var pHi = index < pts.length - 1 ? pts[index + 1][1] : 100
  pts[index] = [
    Math.round(Math.max(tLo, Math.min(tHi, t))),
    Math.round(Math.max(pLo, Math.min(pHi, p)))
  ]
  return pts
}

function addPoint(points, t, p) {
  var pts = clonePoints(points)
  if (pts.length >= 8) return pts
  t = Math.round(t)
  for (var i = 0; i < pts.length; i++) if (pts[i][0] === t) return pts
  var idx = 0
  while (idx < pts.length && pts[idx][0] < t) idx++
  var lo = idx > 0 ? pts[idx - 1][1] : 0
  var hi = idx < pts.length ? pts[idx][1] : 100
  pts.splice(idx, 0, [t, Math.round(Math.max(lo, Math.min(hi, p)))])
  return pts
}

function removePoint(points, index) {
  var pts = clonePoints(points)
  if (pts.length <= 2 || index < 0 || index >= pts.length) return pts
  pts.splice(index, 1)
  return pts
}

// "40:10,60:30" for the omarchy-fans CLI.
function pointsArg(points) {
  return points.map(function(p) { return p[0] + ":" + p[1] }).join(",")
}

function nearestPoint(points, t, p, maxT, maxP) {
  var best = -1, bestD = Infinity
  for (var i = 0; i < points.length; i++) {
    var dt = (points[i][0] - t) / maxT, dp = (points[i][1] - p) / maxP
    var d = dt * dt + dp * dp
    if (d < bestD && Math.abs(points[i][0] - t) <= maxT && Math.abs(points[i][1] - p) <= maxP) {
      best = i
      bestD = d
    }
  }
  return best
}

// A connected fan whose speed ignored PWM at calibration: the BIOS drives it.
function ignoresPwm(f) {
  return !!f && f.connected === true && f.responsive === false
}

function fanTag(f) {
  if (ignoresPwm(f)) return "ignores PWM · BIOS"
  if (f && f.kind === "gpu") return "card"
  return f && f.connected && !f.calibrated ? "uncalibrated" : ""
}

// How to make a PWM-deaf fan controllable. A 3-pin fan has no PWM wire, so
// only the header's Voltage (DC) mode, a BIOS setting, can slow it down.
function fanHint(f) {
  if (!ignoresPwm(f)) return ""
  return "3-pin fan: set this header to Voltage mode in the BIOS (Smart Fan → Fan Control Mode), then reboot. " +
    "It is re-tested at every start and goes onto its curve once it responds."
}

function retestRunning(st, fanId) {
  var c = st && st.calibration
  return !!c && c.kind === "retest" && (!fanId || (c.fans || []).indexOf(fanId) >= 0)
}

function modeLabel(st, alive) {
  if (!st || !alive) return "Offline"
  if (st.calibration) return (st.calibration.kind === "retest" ? "Re-testing " : "Calibrating ") + Math.round(Number(st.calibration.progress || 0) * 100) + "%"
  if (st.mode === "auto") return "BIOS"
  return "Curve"
}

function profileLabel(st) {
  if (!st || !st.profiles) return ""
  for (var i = 0; i < st.profiles.length; i++) if (st.profiles[i].id === st.profile) return st.profiles[i].label
  return st.profile || ""
}

function hasStall(st) {
  return connectedFans(st).some(function(f) { return f.stalled })
}

function hasCritical(st) {
  if (!st || !st.sensors) return false
  return st.sensors.some(function(s) { return s.temp !== null && s.temp >= s.critical })
}

function cpuFanRpm(st) {
  var fans = connectedFans(st)
  for (var i = 0; i < fans.length; i++) if (fans[i].role === "cpu" && fans[i].rpm !== null) return fans[i].rpm
  return fans.length > 0 && fans[0].rpm !== null ? fans[0].rpm : NaN
}

function barLabel(st, alive, mode) {
  if (!st || !alive) return "󰈐 —"
  if (mode === "Temp") return "󰈐 " + formatTemp(sensorTemp(st, "cpu"))
  var rpm = cpuFanRpm(st)
  return "󰈐 " + (isFinite(rpm) ? rpm : "—")
}

function tooltip(st, alive) {
  if (!st || !alive) return "Fan daemon not running"
  var lines = [profileLabel(st) + " · " + modeLabel(st, alive)]
  var temps = (st.sensors || []).filter(function(s) { return s.temp !== null })
    .map(function(s) { return s.label + " " + formatTemp(s.temp) })
  if (temps.length) lines.push(temps.join(" · "))
  connectedFans(st).forEach(function(f) {
    lines.push(f.label + ": " + (f.rpm === null ? "—" : f.rpm + " rpm") + (f.stalled ? " (stalled!)" : ""))
  })
  return lines.join("\n")
}

if (typeof module !== "undefined") {
  module.exports = {
    parseStatus: parseStatus, ageSeconds: ageSeconds, sensor: sensor, sensorTemp: sensorTemp,
    connectedFans: connectedFans, curveTabs: curveTabs, findTab: findTab, interp: interp,
    clonePoints: clonePoints, cpuFanRpm: cpuFanRpm, PROFILE_ICONS: PROFILE_ICONS,
    movePoint: movePoint, addPoint: addPoint, removePoint: removePoint, pointsArg: pointsArg,
    nearestPoint: nearestPoint, modeLabel: modeLabel, ignoresPwm: ignoresPwm, fanTag: fanTag,
    fanHint: fanHint, retestRunning: retestRunning, profileLabel: profileLabel, hasStall: hasStall,
    hasCritical: hasCritical, barLabel: barLabel, tooltip: tooltip, formatTemp: formatTemp,
    T_MIN: T_MIN, T_MAX: T_MAX
  }
}
