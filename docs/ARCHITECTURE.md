# OpenWave architecture

OpenWave combines a USB control panel with a PipeWire router. GTK presents cached observations and desired state; workers own hardware discovery, audio-graph discovery and mutations. There is no custom audio server.

## Native workspace and entry points

The Rust workspace has three crates:

| Crate | Ownership |
|---|---|
| [`openwave-core`](../crates/openwave-core/src/lib.rs) | Typed state and identities, exact USB profiles and protocol encoding, routing/group decisions, scene resolution, DSP rendering, calibration analysis and health policy; no GTK or process ownership |
| [`openwave-runtime`](../crates/openwave-runtime/src/lib.rs) | Controller and persistent stores; serialized USB, mixer, meter, calibration and host-integration workers; process/lease boundaries, diagnostics, probing, installation and removal |
| [`openwave-desktop`](../crates/openwave-desktop/src/lib.rs) | GTK/libadwaita presentation, GApplication actions, dialogs, icons and tray; submits typed commands and renders controller snapshots |

The public native executables are `openwave` (desktop and its CLI dispatch), `openwave-daemon` (capture pins/health), `openwave-diag` (diagnostics) and `openwave-probe` (engineer-only vendor access). Private `libexec/openwave-maintenance` owns maintenance CLI dispatch and supervised audio-child execution; it is not a second desktop runtime. `VERSION` remains canonical and `rust-toolchain.toml` pins Rust 1.98.1. Source launches require the sibling maintenance executable: build with `cargo build --locked --workspace --bins` before using `target/debug/openwave`.

## Ownership and concurrency

| Owner | Responsibilities |
|---|---|
| GTK application/window | Render cached state, collect UI input and expose GApplication actions; no direct USB or graph mutation |
| Runtime controller | Validate `AppCommand`s, own source/mix stores and scene coordination, publish immutable `AppSnapshot`s and command outcomes |
| USB discovery worker | Enumerate exact supported profiles and runtime USB locations off the GTK thread |
| One serialized queue per USB unit | Connect, poll, read-modify-write controls and disconnect that captured device |
| Mixer worker | Discover PipeWire/Pulse state, reconcile desired routes, manage owned child processes/sinks and restore streams |
| Meter/calibration workers | Capture PCM and return identity-scoped samples/results without blocking GTK |
| Native capture daemon | Maintain one capture pin per Wave input and observe health; disruptive remedies require opt-in |

[`RuntimeHandle`](../crates/openwave-runtime/src/controller.rs) admits typed `AppCommand`s and returns a `CommandId`; admission is not completion. GTK consumes `RuntimeEvent`s and `Arc<AppSnapshot>` snapshots, with coalesced snapshot notifications separate from command/shutdown/removal outcomes. The controller translates requests to `BackendCommand`s; `NativeBackend` owns the concrete workers and translates their observations and completions back to `BackendEvent`s. GTK actions and local controls use this same controller boundary.

A device operation captures its `UnitId` (profile, bus, address and connection incarnation) when queued. Changing the sidebar selection cancels unsent debounced device edits and cannot redirect queued writes. The sidebar disables new device gestures until the selected unit's snapshot is rendered, including across stale renders. [`DeviceManager`](../crates/openwave-runtime/src/device.rs) retires a unit by draining its queue before closing its handle. ALSA synchronization requires exact identity evidence; a model name or a missing serial is not permission to choose the first card. See [hardware identity](hardware-support.md#multiple-units-and-hotplug).

The mixer exposes cached stream, capture and output snapshots to the UI. GTK does not synchronously discover the graph for a dialog or fader update. Desired-state setters wake the worker; it owns graph commands during normal reconciliation. The only ownership handoff is final cleanup retry after the mixer worker has joined, still off GTK. Failed discovery preserves the last usable snapshot rather than pretending the graph is empty. Failed operations remain observable and are retried through reconciliation, not reported successful because a child still exists.

Shutdown freezes command admission and cancels unsent edits/calibration. [`ActiveWorkers::stop`](../crates/openwave-runtime/src/controller/native.rs) drains health, calibration and device workers, then stops meter readers **before final mixer cleanup**. Pending health restoration and meter drains retain ownership and are retried; final mixer cleanup cannot run while readers remain owned. `Mixer::stop` retains its reconciler after cleanup failure so a later shutdown retry can finish restoring streams and removing owned sinks. Completed worker failures remain errors rather than disappearing with a consumed join handle.

An unfinished process reaper is pending cleanup, never successful termination; only its actual joined exit status completes that obligation. `NativeBackend` releases vendor and installation leases only after every drain and owned graph cleanup succeeds. Failure keeps the controller frozen with ownership retained. Closing a frozen failed-shutdown window requests another drain even with a tray host; ordinary running-window Close may still hide into the tray.

A server restart invalidates runtime identities. Final cleanup still needs a known graph and ownership evidence: `Observation::Unknown` is never an empty graph or permission to discard cleanup obligations. Broad process-name kills or reused module IDs must not authorize deleting unrelated resources. Closing a window into the tray is not application shutdown.

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

Source, send and master volumes retain Python's **normalized `wpctl`/Pulse volume coordinates**, not linear PCM gain. The per-cell loopback receives **source trim × send** and row/cell mute; the master is a separate destination volume. [`SubprocessPipeWire`](../crates/openwave-runtime/src/mixer.rs) passes that volume directly to `wpctl` and divides Pulse integer readback by `65536`. Pulse/PipeWire's cubic boundary supplies the PCM gain: a saved source `0.5` and send `0.4` produce `(0.5 × 0.4)³ = 0.008` gain. Existing canonical JSON and scene numbers keep their audible meaning without a load-time rewrite.

Trim is not applied by turning down an intake null sink, whose monitor and stream-volume compensation would make it unreliable. Master scale and mute callbacks compose from the current sibling widget value so rapid edits cannot discard one another.

Mix masters apply to the mix destination. Saved master levels/mutes are restored to each observed runtime sink identity, then confirmed by a subsequent observation before the new sink's state can overwrite the saved values. Reusing a node name after a PipeWire restart does not prove restoration occurred. Failed writes or absent sinks do not count as restored. Once confirmed, external master changes can be observed and persisted.

Routes are reconciled from observed nodes, ports and links. New route gain is established before audio is linked. Missing links are repaired even when a child remains alive; stale targets are not treated as healthy merely because the process has not exited.

### Output safety and published inputs

Each mix independently chooses:

- **Automatic**: prefer an eligible Wave output with an exact ready paired capture. If Wave outputs exist but none is ready, stay silent rather than falling through to speakers. Without Wave outputs, use an eligible system default, then a deterministic priority-ranked physical output.
- **Not monitored**: no physical output loopback; the published capture input still exists.
- **Explicit output**: use only that sink. If unplugged, remain silent. No fallback to speakers or another person's headphones.

Personal defaults to Automatic; other mixes default to Not monitored. OpenWave virtual sinks are ineligible outputs. Wave playback is gated on capture readiness for the corresponding input. Automatic is a convenience policy, not a persistent per-unit headphone assignment; use explicit outputs when that distinction matters.

Every mix is exposed as an ordinary capture source named `openwave_capture_<mix_id>`, allowing clients that hide monitor sources to select it. Its audio path is reconciled after sink recreation too. Do not route a communication application's return audio into the same mix it uses as a microphone. External graph tools and acoustic paths can still create feedback outside these guards.

### Groups

Groups express mutually exclusive alternatives, such as two microphones used by one speaker. Unmuting a member mutes the other members of that group; another group is untouched. All members may be muted, so the invariant is **at most one open source**, not “one must always be live”. Switching advances through members in row order.

Dragging a source onto the middle of another groups it; edge drops reorder. Group changes and remote commands use [`Controller::set_source_mute`](../crates/openwave-runtime/src/controller/state/mutations.rs) and `openwave_core::routing::set_source_muted`. Hardware mute is derived from the committed bound rows: it is muted only when all are muted. Every known vendor poll is reconciled against that current aggregate, not only the previous hardware edge. Shared physical inputs can retain distinct software row mutes while another row keeps the input open. Hardware-originated transitions do not echo a write back to their originating unit.

The first known non-vendor capture observation establishes a binding/identity-scoped baseline; it does not clear a saved software mute. Later external edges use the shared group transition. Exact vendor bindings remain authoritative for vendor feedback.

Software-initiated handovers hold opening writes and routes until the required departing inputs acknowledge silence. This includes already software-muted peers whose observed hardware is still open or unknown. If another row legitimately keeps a shared physical input open, the mixer instead supplies exact capture-identity and routing-revision proof that the departing row's owned routes are silent. Failed, unknown, stale or replacement observations cannot release an opening; newer mute, rebinding, retirement and shutdown invalidate held work. External hardware changes still happen outside this coordination; there is no USB-wide or sample-atomic transaction.

A current known observation can prove that a departing capture is absent and release an explicitly selected available alternative. Unknown or ambiguous observations are not absence, and a missing opening target stays closed.

## Scenes

Scenes store source trims/mutes, send levels/mutes, per-mix output choices and masters, plus supported hardware settings: `gain_raw`, `mute`, `hp_volume_db`, `low_impedance` and `monitor_mix`. They do not redefine routing topology, create missing sources/mixes or capture effects/phantom power. Saving uses the latest serialized device-poll cache, not synchronous GTK USB reads; a just-moved physical control may not appear until its next completed poll.

Hardware matching uses exact serial identity. A legacy model-only scene entry can match only an unambiguous single connected unit of that model. Missing and ambiguous targets are reported rather than applied to another unit. Source/mix IDs must still exist for their scene entries to be usable.

[`Controller::apply_scene`](../crates/openwave-runtime/src/controller/state/scene.rs) performs ordered, best-effort recall through the same source/group rules as live controls. Explicit source mute patches take precedence over hardware mute patches; hardware-only mute fills missing bound-row patches, then the final committed group state determines hardware mute. A later recalled open group member wins, and displaced entries are reported. Superseded unsent edits are cancelled so they cannot overwrite the recalled state. Hardware operations go to captured per-device queues; completion waits for their results. Some changes can succeed while others fail. The result reports skipped/failed targets; it does not promise rollback, sample-accurate switching or a hardware-atomic transaction. Do not treat an action being accepted as proof every target has completed successfully.

## DSP and calibration

[`openwave_core::effects`](../crates/openwave-core/src/effects.rs) validates settings and renders JSON-escaped SPA configuration independently of GTK and subprocess execution. The mixer owns the resulting PipeWire filter process/lifecycle. User-visible labels must not become unescaped SPA syntax or shell commands.

SWH LADSPA provides the gate and SC4 compressor; plugins must be available at runtime, including through `LADSPA_PATH` in packaged environments. Neutral settings bypass the filter process. An enabled chain that cannot start or remain healthy silences its sends and reports the error; it never silently falls back to unprocessed audio. Stereo is preserved unless mono is explicitly requested or the capture is known to have one channel; unknown channel counts must not force a stereo source down to mono.

Calibration captures the selected **raw input**, not a mix monitor or the processed output. A cancellable `CalibrationWorker` measures three seconds of room noise and five seconds of speech, then displays a bounded DSP proposal for review. Nothing is applied until explicit acceptance. The controller's `CalibrationToken` binds the session, source, node name, exact `NodeIdentity` and channel count; the capture binding must also remain unchanged. Unknown graph state, hotplug, rebinding or changed channels expires the session rather than applying a stale proposal. Rejecting, cancelling or closing the application leaves current settings unchanged. Calibration never adjusts hardware gain or phantom power. Keep normal microphone placement and avoid clipping while collecting a sample.

[`MeterMonitor`](../crates/openwave-runtime/src/meter.rs) scopes each tap and queued event to the exact server/node identity and worker generation. Meter readers preserve Python's 8 kHz mono-downmix policy before signed-16-bit peak decoding; anti-phase stereo can therefore cancel. The `0.004` quiet floor and settled-zero coalescing are presentation policy, not proof that PCM or byte flow stopped. Calibration retains its separate channel capture. Retired events are fenced, reader exit clears readiness, and bounded shutdown retains unfinished children for retry. `CaptureReadiness` tracks actual raw reader lifetime and accepts zero-valued or incomplete PCM bytes as flow.

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

Do not edit live JSON files to control the running application: the owner may overwrite external changes on its next save. GApplication exports `org.gtk.Actions` on the session bus under `com.github.openwave`; remote commands must use the registered actions and go through the same validated controller commands, not bypass the queue with another USB process. Read-only action state can provide coherent snapshots; action activation itself is not a hardware-completion reply.

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

## Desktop launcher upgrades

Canonical installation identity remains separate from a proven stable launch path. Native launcher rendering can follow a current profile alias, including recognized root-owned Nix wrappers that target the exact current executable. Unproven old absolute launchers are never implicitly adopted as current ownership.

`openwave --migrate-launchers-from PREVIOUS_EXECUTABLE [--dry-run] [--yes]` provides an explicit old-to-current handoff before the previous installation is removed. It runs before GTK initialization, inspects only the standard user menu/autostart entries, and captures installation authority plus entry bytes, inode and parent identity before confirmation. Publication rechecks those witnesses and preserves enabled/hidden intent. Native manual receipts and immutable Nix installations are supported; package-managed entries, custom/foreign commands, unsafe links, lost authority and changed files are refused. Publication is atomic per entry; reinspection can finish a partially completed handoff.

## Host integration and health

Native setup may install exact-PID udev permissions, user audio rules and a capture service. Those are explicit host operations. The [experimental sandbox](install-bazzite.md#experimental-flatpak-boundary) does not acquire those privileges or restart host audio.

Successful setup re-queries the capture service on the integration worker and publishes that observation before setup completion. Only a confirmed running, non-failed service clears its warning; an unavailable observation remains visible rather than being treated as healthy. This refresh does not skip the replug/Continue phase or restart the audio session.

Systemd's absent-unit state does not require an `ExecStart` value. Loaded units still require proven commands and directories: an optional `!HOME` default is accepted only when the unit fragment leaves `WorkingDirectory` unset, never as an override of an explicit source working directory.

The GUI starts [`HealthMonitor::start(false, …)`](../crates/openwave-runtime/src/health.rs); the daemon also defaults to observe-only. Confirmed faults are logged; only the daemon's explicit `--auto-recover` option permits bounded card-profile/sink remedies. Unknown observations are not fault confirmation, and cancellation does not remove the obligation to restore state changed by a remedy. [Diagnostics](troubleshooting.md#diagnostics-and-privacy) is read-only: `--full` controls disclosure, while the separate `--device` flag permits vendor reads after other control clients have closed.

Recovery retains the exact original card profile or sink-resume obligation after a possibly applied mutation. Failed restoration observations do not discard it; only that original identity may repay it, without spending a new incident budget or initiating another remedy. The daemon retains its health owner, capture pins and leases through bounded restoration retries during shutdown. Replacement identities never authorize restoration.

The GUI supplies live raw-meter byte ages to its observe-only monitor. The daemon supplies its keepalive byte ages; only its explicit opt-in enables recovery. Zero-valued samples are still data. Capture xruns and no-data faults share one per-node card-recovery budget: at most two attempts, at least 60 seconds apart, rearmed after five minutes of healthy observations. Muted, absent, unknown and recreated-node baselines do not refill it. Output monitoring follows actual links from OpenWave output routes to ALSA sinks, not stale target hints; its separate suspend/resume budget rearms after six advancing hardware-pointer observations.

## Installation removal

[`openwave_runtime::installation`](../crates/openwave-runtime/src/installation.rs) records an exact, hashed native payload inventory and installation identity at install time. `RuntimePaths` uses the canonical `share/openwave` directory for an installed identity and the repository root for a source build, with a same-install maintenance helper. Runtime paths exclude `DESTDIR`; native package recipes declare their owner. Package ownership overrides manual metadata. Manual removal validates the inventory again before deleting files, rejects changed files and symlink boundaries, and removes only recorded files plus empty OpenWave-owned directories. Unrecorded content is preserved.

The application menu, first-run dialogs and `openwave --uninstall` share [`openwave_runtime::uninstall`](../crates/openwave-runtime/src/uninstall.rs). Inspection is read-only and starts no GTK, USB, audio or service workers. Explicit confirmation precedes controller freeze, worker draining, service stop and file removal; settings/scenes need separate opt-in. Service removal checks the effective systemd command or actual runit target. Package-owned definitions and USB rules remain under their manager's control; shared GTK/PipeWire dependencies are never removal targets.

Service refresh/removal also checks parent-directory symlink ownership. A matching externally managed unit may be stopped after consent, but its target definition and package-owned bytes are preserved.

The CLI uses the stateful `prepare-uninstall(s)` GApplication action to request shutdown of the same canonical installation identity. Its state is `idle`, `stopping` or `error:<details>`. This action only quiesces/exits the matching GUI; it never uninstalls files or starts a GUI automatically. The parameterless `uninstall` action opens the interactive confirmation.

An unanswered interactive removal dialog reports an immediate public preparation conflict. Closing that dialog permits a later request; it cannot leave a hidden owner waiting for the CLI deadline.

After confirmation, removal preparation validates authority and creates any required recovery bundle before the controller gives up its live runtime. Freezing may persist final preferences, but no state is saved again once removal begins, preventing deleted settings from being recreated. Joins and removal run off GTK. Failed operations retain truthful partial results and retry support. [`uninstall::recovery`](../crates/openwave-runtime/src/uninstall/recovery.rs) copies the native `openwave-maintenance` executable into a private, digest-checked recovery bundle with `plan.json`, so retry does not depend on already removed installed files. Privileged removal independently derives authority from a validated root-owned bootstrap/transaction. A user recovery record never overrides receipt, hash, package or path-boundary validation, and never grants root deletion authority.

The public CLI's cancellation token reaches bounded inventory hashing/unlink and the owned privileged cancellation channel, not merely phase boundaries. Root removal authenticates the initiating login-user process through kernel ancestry, credentials, start identity and executable identity while retaining independent owner/lease exclusion. Historical schema-1 recovery JSON may be `0644` inside a validated private `0700` bundle; it remains inert data, never execution of historical `retry.py`, and does not relax native-record privacy or installation authority checks.
