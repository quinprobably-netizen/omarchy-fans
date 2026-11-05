const test = require("node:test")
const assert = require("node:assert/strict")
const M = require("../Model.js")

const status = {
  time: 1000, mode: "curve", profile: "balanced",
  profiles: [{ id: "silent", label: "Silent" }, { id: "balanced", label: "Balanced" }],
  curves: {
    cpu: { cpu: [[40, 10], [85, 100]] },
    case: { cpu: [[45, 10], [88, 100]], gpu: [[60, 10], [95, 100]], vram: [[70, 0], [100, 100]] },
  },
  sensors: [
    { id: "cpu", label: "CPU", temp: 45.2, held: 45, critical: 90, ok: true },
    { id: "gpu", label: "GPU", temp: 72, held: 72, critical: 97, ok: true },
  ],
  fans: [
    { id: "it8689/pwm1", label: "CPU fan", role: "cpu", connected: true, calibrated: true, rpm: 812, stalled: false },
    { id: "it8689/pwm2", label: "System fan 1", role: "case", connected: true, seen_spinning: true, rpm: 640, stalled: false },
    { id: "it8689/pwm4", label: "System fan 3", role: "case", connected: false, rpm: 0, stalled: false },
  ],
  calibration: null,
}

test("parseStatus rejects junk", () => {
  assert.equal(M.parseStatus(""), null)
  assert.equal(M.parseStatus("{nope"), null)
  assert.equal(M.parseStatus('{"a":1}'), null)
  assert.equal(M.parseStatus(JSON.stringify(status)).profile, "balanced")
})

test("staleness", () => {
  assert.ok(M.ageSeconds(status, 1005000) < 10)
  assert.ok(M.ageSeconds(status, 1020000) > 10)
  assert.equal(M.ageSeconds(null, 0), Infinity)
})

test("curve tabs skip sensors this machine lacks and roles without fans", () => {
  const tabs = M.curveTabs(status)
  assert.deepEqual(tabs.map(t => t.key), ["cpu:cpu", "case:cpu", "case:gpu"])
  assert.equal(tabs[2].label, "Case ← GPU")
  const noCase = { ...status, fans: [status.fans[0]] }
  assert.deepEqual(M.curveTabs(noCase).map(t => t.key), ["cpu:cpu"])
  assert.equal(M.findTab(tabs, "missing").key, "cpu:cpu")
})

test("movePoint keeps curves monotonic", () => {
  const pts = [[40, 10], [60, 30], [80, 100]]
  assert.deepEqual(M.movePoint(pts, 1, 90, 5), [[40, 10], [79, 10], [80, 100]])
  assert.deepEqual(M.movePoint(pts, 1, 20, 200), [[40, 10], [41, 100], [80, 100]])
  assert.deepEqual(M.movePoint(pts, 0, -10, -10), [[0, 0], [60, 30], [80, 100]])
  assert.deepEqual(pts, [[40, 10], [60, 30], [80, 100]], "input untouched")
})

test("add/remove points", () => {
  const pts = [[40, 10], [80, 100]]
  assert.deepEqual(M.addPoint(pts, 60.4, 5), [[40, 10], [60, 10], [80, 100]])
  assert.deepEqual(M.addPoint(pts, 40, 50), pts, "no duplicate temps")
  assert.deepEqual(M.removePoint(pts, 0), pts, "keeps two points")
  assert.deepEqual(M.removePoint([[1, 1], [2, 2], [3, 3]], 1), [[1, 1], [3, 3]])
  assert.equal(M.pointsArg(pts), "40:10,80:100")
})

test("labels", () => {
  assert.equal(M.barLabel(status, true, "RPM"), "󰈐 812")
  assert.equal(M.barLabel(status, true, "Temp"), "󰈐 45°")
  assert.equal(M.barLabel(status, false, "RPM"), "󰈐 —")
  assert.equal(M.modeLabel(status, true), "Curve")
  assert.equal(M.modeLabel({ ...status, mode: "auto" }, true), "BIOS")
  assert.equal(M.modeLabel({ ...status, calibration: { progress: 0.42 } }, true), "Calibrating 42%")
  assert.equal(M.profileLabel(status), "Balanced")
  assert.match(M.tooltip(status, true), /CPU fan: 812 rpm/)
  assert.doesNotMatch(M.tooltip(status, true), /System fan 3/)
  assert.equal(M.hasCritical(status), false)
  assert.equal(M.hasCritical({ ...status, sensors: [{ ...status.sensors[0], temp: 91 }] }), true)
})

test("never-spinning uncalibrated headers are hidden", () => {
  const st = { ...status, fans: [...status.fans, { id: "it8689/pwm5", label: "x", role: "case", connected: true, calibrated: false, seen_spinning: false, rpm: 0 }] }
  assert.deepEqual(M.connectedFans(st).map(f => f.id), ["it8689/pwm1", "it8689/pwm2"])
})

test("PWM-deaf fans stay listed (they still report RPM)", () => {
  const st = { ...status, fans: [{ id: "it8689/pwm2", label: "S", role: "case", connected: true, calibrated: true, responsive: false, rpm: 1149 }] }
  assert.equal(M.connectedFans(st).length, 1)
})

test("PWM-deaf fans get a tag, a fix hint and a re-test state", () => {
  const deaf = { id: "it/pwm2", label: "System fan 1", connected: true, responsive: false, calibrated: false }
  const ok = { id: "it/pwm3", connected: true, responsive: true, calibrated: true }
  const empty = { id: "it/pwm4", connected: false, responsive: true, calibrated: false }
  assert.equal(M.ignoresPwm(deaf), true)
  assert.equal(M.ignoresPwm(ok), false)
  assert.equal(M.ignoresPwm(empty), false)
  assert.equal(M.fanTag(deaf), "ignores PWM · BIOS")
  assert.equal(M.fanTag(ok), "")
  assert.equal(M.fanTag({ connected: true, calibrated: false }), "uncalibrated")
  assert.match(M.fanHint(deaf), /Voltage mode in the BIOS/)
  assert.equal(M.fanHint(ok), "")
  const st = { calibration: { kind: "retest", progress: 0.5, fans: ["it/pwm2"] } }
  assert.equal(M.retestRunning(st, "it/pwm2"), true)
  assert.equal(M.retestRunning(st, "it/pwm3"), false)
  assert.equal(M.retestRunning({ calibration: { kind: "calibrate", fans: ["it/pwm2"] } }, "it/pwm2"), false)
  assert.equal(M.modeLabel({ ...st, time: 1 }, true), "Re-testing 50%")
  assert.equal(M.modeLabel({ calibration: { progress: 0.25 } }, true), "Calibrating 25%")
})

test("GPU fan is listed at rest and gets its own curve tab", () => {
  const st = {
    curves: { cpu: { cpu: [[40, 10], [85, 100]] }, gpu: { gpu: [[55, 0], [93, 100]], vram: [[80, 0], [100, 100]] } },
    sensors: [{ id: "cpu", label: "CPU" }, { id: "gpu", label: "GPU" }, { id: "vram", label: "VRAM" }],
    fans: [
      { id: "it/pwm1", role: "cpu", connected: true, calibrated: true, rpm: 800 },
      { id: "xe/gpu", kind: "gpu", role: "gpu", label: "GPU fan", connected: true, calibrated: false, rpm: 0 },
    ],
  }
  assert.deepEqual(M.connectedFans(st).map(f => f.id), ["it/pwm1", "xe/gpu"])
  assert.deepEqual(M.curveTabs(st).map(t => t.label), ["CPU fan", "GPU fan", "GPU ← VRAM"])
  assert.equal(M.fanTag(st.fans[1]), "", "the name already says GPU")
})
