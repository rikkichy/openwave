# OpenWave architecture

OpenWave combines a USB control panel with a PipeWire router. GTK presents cached observations and desired state; workers own hardware discovery, audio-graph discovery and mutations. There is no custom audio server.

## Ownership and concurrency

| Owner | Responsibilities |
|---|---|
| GTK application/window | UI state, validated commands, source/mix stores, scene coordination and GApplication actions |
| USB discovery worker | Enumerate exact supported profiles and runtime USB locations off the GTK thread |
| One serialized queue per USB unit | Connect, poll, read-modify-write controls and disconnect that captured device |
| Mixer worker | Discover PipeWire/Pulse state, reconcile desired routes, manage owned child processes/sinks and restore streams |
| Meter workers | Capture meter samples, report levels to GTK without blocking its event loop |
| Native capture daemon | Maintain one capture pin per Wave input and observe health; disruptive remedies require opt-in |

A device operation captures its unit when queued. Changing the sidebar selection cancels unsent debounced edits and cannot redirect queued writes. Retiring a unit drains its queue before closing its handle. ALSA synchronization requires exact identity evidence; a model name or a missing serial is not permission to choose the first card. See [hardware identity](hardware-support.md#multiple-units-and-hotplug).

The mixer exposes cached stream, capture and output snapshots to the UI. GTK does not synchronously discover the graph for a dialog or fader update. Desired-state setters wake the worker; the worker alone executes graph commands. Failed discovery preserves the last usable snapshot rather than pretending the graph is empty. Failed operations remain observable and are retried through reconciliation, not reported successful because a child still exists.

Shutdown stops new work, drains workers, terminates owned routes and attempts to restore moved streams before removing owned sinks. A server restart invalidates runtime identities. Cleanup requires ownership evidence; broad process-name kills or reused module IDs must not authorize deleting unrelated resources. Closing a window into the tray is not application shutdown.

## Sources and mixes

```text
application stream -> source intake ----+
                                       +-> mix sink -> selected output
device capture ----> optional DSP ------+      |
                                              +-> published mix input
```

A source is a row; a mix is a column. Application rows match live streams using normalized exact application/node/binary identities. Claims are deterministic: match specificity, then stable source ID breaks ties; a catch-all receives otherwise unclaimed streams. One stream has one owner, preventing two matching rows from summing duplicate copies.

An application stream is **moved**, not copied, into `openwave_src_<source_id>`. Copying while leaving original playback in a mix would make a zero send unable to silence the original. Claimed applications remain managed even with every send at zero: that means silence, not a bypass. Removing a row or binding releases its stream claims. Restoration uses the recorded original sink if available, otherwise an eligible default; it must not strand streams in a removed intake.

Device rows capture their bound hardware node directly. A missing node remains missing rather than being replaced by another microphone. OpenWave's own virtual nodes are excluded from ordinary hardware-source/output selection to avoid internal feedback paths.

Mix definitions have stable IDs independent of display names. Renaming keeps bindings/levels; removing a mix removes its sends, output route and published input. Consumers such as OBS or a voice client then need another input selected. At least one mix remains in the UI.

### Levels and masters

The per-cell loopback applies **source trim × send** and row/cell mute. Trim is not implemented by turning down an intake null sink, whose monitor and stream-volume compensation would make that control unreliable.

Mix masters apply to the mix destination. Saved master levels/mutes are restored to each observed runtime sink identity, then confirmed by a subsequent observation before the new sink's state can overwrite the saved values. Reusing a node name after a PipeWire restart does not prove restoration occurred. Failed writes or absent sinks do not count as restored. Once confirmed, external master changes can be observed and persisted.

Routes are reconciled from observed nodes, ports and links. New route gain is established before audio is linked. Missing links are repaired even when a child remains alive; stale targets are not treated as healthy merely because the process has not exited.

### Output safety and published inputs

Each mix independently chooses:

- **Automatic**: prefer the discovered Wave headphones, then an eligible system default, then a deterministic priority-ranked physical output.
- **Not monitored**: no physical output loopback; the published capture input still exists.
- **Explicit output**: use only that sink. If unplugged, remain silent. No fallback to speakers or another person's headphones.

Personal defaults to Automatic; other mixes default to Not monitored. OpenWave virtual sinks are ineligible outputs. Wave playback is gated on capture readiness for the corresponding input. Automatic is a convenience policy, not a persistent per-unit headphone assignment; use explicit outputs when that distinction matters.

Every mix is exposed as an ordinary capture source named `openwave_capture_<mix_id>`, allowing clients that hide monitor sources to select it. Its audio path is reconciled after sink recreation too. Do not route a communication application's return audio into the same mix it uses as a microphone. External graph tools and acoustic paths can still create feedback outside these guards.

### Groups

Groups express mutually exclusive alternatives, such as two microphones used by one speaker. Unmuting a member mutes the other members of that group; another group is untouched. All members may be muted, so the invariant is **at most one open source**, not “one must always be live”. Switching advances through members in row order.

Dragging a source onto the middle of another groups it; edge drops reorder. Group changes and remote commands use the same window operations. Reconciliation silences the departing/muted sends before opening the next route; failure to silence an existing route defers the handover rather than deliberately opening both. Cross-device hardware changes are still sequential operations, not a USB-wide atomic switch.

## Scenes

Scenes store source trims/mutes, send levels/mutes, per-mix output choices and masters, plus supported hardware settings: `gain_raw`, `mute`, `hp_volume_db`, `low_impedance` and `monitor_mix`. They do not redefine routing topology, create missing sources/mixes or capture effects/phantom power. Saving uses the latest serialized device-poll cache, not synchronous GTK USB reads; a just-moved physical control may not appear until its next completed poll.

Hardware matching uses exact serial identity. A legacy model-only scene entry can match only an unambiguous single connected unit of that model. Missing and ambiguous targets are reported rather than applied to another unit. Source/mix IDs must still exist for their scene entries to be usable.

Recall is an ordered, best-effort operation through the same application paths used by the UI, with hardware operations submitted to the captured per-device queues. Some changes can succeed while others fail. The result reports partial failure; it does not promise rollback, sample-accurate switching or a hardware-atomic transaction. Do not treat an action being accepted as proof every target has completed successfully.

## DSP and calibration

The effects module validates settings and renders JSON-escaped SPA configuration independently of GTK and subprocess execution. The mixer owns the resulting PipeWire filter process/lifecycle. User-visible labels must not become unescaped SPA syntax or shell commands.

SWH LADSPA provides the gate and SC4 compressor; plugins must be available at runtime, including through `LADSPA_PATH` in packaged environments. Neutral settings bypass the filter process. An enabled chain that cannot start or remain healthy silences its sends and reports the error; it never silently falls back to unprocessed audio. Stereo is preserved unless mono is explicitly requested or the capture is known to have one channel; unknown channel counts must not force a stereo source down to mono.

Calibration captures the selected **raw input**, not a mix monitor or the processed output. A cancellable worker measures three seconds of room noise and five seconds of speech, then displays a bounded DSP proposal for review. Nothing is applied until explicit acceptance; changed capture identity prevents a stale proposal from being applied after hotplug. Rejecting, cancelling or closing the application leaves current settings unchanged. Calibration never adjusts hardware gain or phantom power. Keep normal microphone placement and avoid clipping while collecting a sample.

## State and external control

Under the OpenWave configuration directory (normally `~/.config/openwave`):

| File | Purpose |
|---|---|
| `sources.json` | Stable source IDs, bindings, trim, mute, group and capture-DSP settings |
| `mixdefs.json` | Stable mix IDs, labels and sink definitions |
| `mixes.json` | Per-send levels/mutes, per-mix output choices and masters |
| `ui-state.json` | Window geometry and interface preferences |
| `scenes.json` | Named level snapshots and serial-bound hardware settings |

Scene storage is managed through the scene interface. The generated per-user PipeWire mix configuration is audio integration state, not an external control protocol. Source definitions and send levels are deliberately separate so a fader save cannot overwrite topology. Invalid persistent data is not an invitation to overwrite it with defaults.

Do not edit live JSON files to control the running application: the owner may overwrite external changes on its next save. GApplication exports `org.gtk.Actions` on the session bus under `com.github.openwave`; remote commands must use the registered actions and go through the same validated window operations, not bypass the queue with another USB process. Read-only action state can provide coherent snapshots; action activation itself is not a hardware-completion reply.

### GApplication actions

The session-bus name is `com.github.openwave`, object path `/com/github/openwave`, interface `org.gtk.Actions`. Names are stable source/mix/scene IDs unless a display name is explicitly requested below. Tuple signatures describe one GVariant tuple parameter, not multiple positional action parameters.

| Action | Parameter type | Meaning |
|---|---|---|
| `switch-group` | `s` | Group name; advance its active member |
| `set-source-level` | `(sd)` | Source ID, trim from 0 to 1 |
| `toggle-source-mute` | `s` | Source ID |
| `set-cell-level` | `(ssd)` | Source ID, mix ID, send from 0 to 1 |
| `toggle-cell-mute` | `(ss)` | Source ID, mix ID |
| `apply-scene` | `s` | Scene ID; ordered best-effort recall |
| `save-scene` | `s` | Display name for the saved scene |
| `delete-scene` | `s` | Scene ID |
| `toggle-fx` | `(ss)` | Source ID, effect key |

Read-only actions have no activation parameter: `source-groups` exposes state type `as` (string array); `snapshot`, `scenes` and `levels` expose state type `s` containing JSON. `snapshot` and `source-groups` push changes from the running window. Activate `scenes` or `levels` to refresh their state before reading. Use `Describe`/`DescribeAll` to inspect states, or subscribe to `Changed` for pushed updates. For example:

```sh
gdbus call --session --dest com.github.openwave \
  --object-path /com/github/openwave \
  --method org.gtk.Actions.DescribeAll
```

`Activate` is asynchronous from the caller's perspective and has no application-result reply. In particular, successful delivery of `apply-scene` is not confirmation that every hardware write succeeded; inspect the application's result/partial-failure reporting. Use the running GUI's actions rather than a second USB-owning helper.

## Host integration and health

Native setup may install exact-PID udev permissions, user audio rules and a capture service. Those are explicit host operations. The [experimental sandbox](install-bazzite.md#experimental-flatpak-boundary) does not acquire those privileges or restart host audio.

Health monitoring defaults to `HealthMonitor(auto_recover=False)`. Confirmed faults are logged; only the daemon's explicit `--auto-recover` option permits bounded card-profile/sink remedies. Unknown observations are not fault confirmation, and cancellation does not remove the obligation to restore state changed by a remedy. [Diagnostics](troubleshooting.md#diagnostics-and-privacy) is read-only: `--full` controls disclosure, while the separate `--device` flag permits vendor reads after other control clients have closed.

The GUI supplies live raw-meter byte ages to its observe-only monitor. The daemon supplies its keepalive byte ages; only its explicit opt-in enables recovery. Zero-valued samples are still data. Capture xruns and no-data faults share one per-node card-recovery budget: at most two attempts, at least 60 seconds apart, rearmed after five minutes of healthy observations. Muted, absent, unknown and recreated-node baselines do not refill it. Output monitoring follows actual links from OpenWave output routes to ALSA sinks, not stale target hints; its separate suspend/resume budget rearms after six advancing hardware-pointer observations.
