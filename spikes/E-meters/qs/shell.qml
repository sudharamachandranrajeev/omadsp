// Spike E harness: one PwNodePeakMonitor per node named in $E_NODES (comma separated). Every 250 ms prints the
// loudest peak each monitor reported in that window. Quickshell reports cbrt(linear peak), so dBFS = 60·log10(v).
// Test-only: the production widget must not meter in QML (guardrail: no PipeWire polling from QML).
import QtQuick
import QtQml.Models
import Quickshell
import Quickshell.Services.Pipewire

ShellRoot {
  id: root
  readonly property var names: (Quickshell.env("E_NODES") || "").split(",")

  function findNode(name) {
    const all = Pipewire.nodes.values;
    for (let i = 0; i < all.length; i++)
      if (all[i].name === name) return all[i];
    return null;
  }

  Instantiator {
    id: monitors
    model: root.names
    delegate: PwNodePeakMonitor {
      required property string modelData
      property real windowMax: 0
      property int updates: 0
      node: root.findNode(modelData)
      onPeakChanged: { updates++; if (peak > windowMax) windowMax = peak; }
    }
  }

  Timer {
    interval: 250; repeat: true; running: true
    onTriggered: {
      let row = [];
      for (let i = 0; i < monitors.count; i++) {
        const m = monitors.objectAt(i);
        const d = m.windowMax > 0 ? (60 * Math.log10(m.windowMax)).toFixed(2) : "-inf";
        row.push(m.modelData + "=" + d + "/" + (m.node ? "found" : "missing") + "/" + m.updates);
        m.windowMax = 0; m.updates = 0;
      }
      console.log("QSMETER " + Date.now() + " " + row.join(" "));
    }
  }
}
