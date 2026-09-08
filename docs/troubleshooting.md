# Troubleshooting

Start with observation, not a PipeWire restart, USB reset or raw register write. A node marked “running” or a live child process is not proof that samples reach hardware. Avoid recovery experiments during recordings, calls or broadcasts.

## Diagnostics and privacy

For a native installation:

```sh
openwave-diag -o openwave-diagnostics.txt
openwave-diag --help
```

From a source checkout, the equivalent is `python3 -m wavexlr.diag`. Native packages use a private module tree, so invoking the system Python with `-m wavexlr.diag` outside a checkout is not the installed interface.

The default report retains useful versions and USB vendor/product IDs while withholding serials, node names/descriptions, journals and configuration bodies. Filesystem paths are redacted in both modes. Reports are created with owner-only permissions and will not overwrite an existing destination.

- `--full` includes private details such as node names, config contents and journal output. It **does not open USB handles** by itself.
- `--device` explicitly permits read-only USB vendor queries. First quit OpenWave, including its tray, and other vendor-control clients. It is not required for ordinary graph diagnostics.
- Combine `--full --device` only when detailed device info/config bytes are needed and you intend to share those details after review.
- `-o FILE` / `--output FILE` selects the report destination. Nothing is uploaded automatically.

Privacy filtering is not a guarantee of anonymity: review reports, especially full reports, before attaching them to [an issue](https://github.com/rikkichy/openwave/issues). Include the symptom, exact PID, relevant software versions, expected route and whether the problem follows unplugging or an audio-server restart. Probe dumps have no equivalent redaction and are [engineer-only](protocol.md#engineer-only-probe).

## Device missing or USB reads failing

1. Check the exact PID against [hardware support](hardware-support.md). `00c7` and unlisted PIDs are not enabled.
2. Check native USB permission setup. Flatpak device access does not install host udev rules; use [host setup](install-bazzite.md).
3. Close other vendor-control clients. A probe or `diag --device` must not compete with the tray-resident GUI.
4. With several units, check the selected serial and each source's capture-node binding. Do not fix ambiguous identity by selecting an arbitrary ALSA card or applying a model-only scene to whichever unit responds first.

A firmware-unresponsive unit may require a deliberate power cycle. Lower monitoring levels and stop active use before unplugging. Repeated failed vendor reads are not a reason to detach the kernel audio driver or poke unknown registers.

## Application or mix is silent

- Check source trim, row mute, send level/mute and mix master level/mute. A zero at any stage can silence the route. Group exclusivity may have muted the source intentionally.
- A claimed application is moved to its intake. With no nonzero sends it is intentionally silent; removing the source/binding releases management of its streams.
- An explicitly selected unplugged output remains unavailable. Reconnect it or choose a different output yourself. Only Automatic permits fallback; Not monitored is correct for a capture-only mix.
- In OBS/voice applications, select the published mix input (`openwave_capture_<mix_id>`). Removing a mix removes that input; its consumers must be repointed.
- After PipeWire recreates nodes, allow reconciliation to observe the replacements, restore and confirm master levels, and re-establish links. Merely seeing a loopback process is not enough.
- For DSP routes, ensure the SWH LADSPA plugins are installed and available through `LADSPA_PATH` or the distribution's normal plugin directory. A missing plugin is not cured by raising gain. Calibration samples raw capture, not a processed mix; accepting a proposal is a separate action.

For feedback or doubled audio, remove any external duplicate path first. Never feed a voice application's return audio into its own microphone mix. Hardware direct monitoring can coexist with software monitoring and sound doubled even when the matrix owns only one application stream.

## Robotic capture, crackles and xruns

Inspect `pw-top -b -n 3` and compare the same live node's `ERR` counter across samples. The counter is cumulative; a large historical total is not evidence of an ongoing fault. Initial iterations may not yet contain useful profiler data. Node recreation also resets identity/counters.

Clock-driver choice and a quantum too small for USB hardware are possible causes, not universal diagnoses. The shipped Wave WirePlumber rule fixes the device rate at 48 kHz and disables idle pause/suspend; it does not impose driver priority or a global quantum. Check which node actually drives the graph. Small-quantum failures may improve with more buffering, but added latency is a tradeoff: 1024 samples at 48 kHz is about 21.3 ms per quantum.

Check capture mute state before treating silence/xruns as a broken microphone. A muted input or stopped source can produce misleading counters. Avoid blindly copying machine-specific quantum/headroom overrides or restarting every audio service.

## Playback marked running but physically silent

A playback node can claim to run while its ALSA PCM stops consuming. For an identified card/PCM, `/proc/asound/cardN/pcmNp/subN/status` can expose `state` and `hw_ptr`; a frozen pointer is relevant only while the corresponding sink is expected to be active. Unknown or missing observations are not a confirmed fault.

Suspending/resuming the affected sink or cycling its card profile is disruptive and can leave audio unavailable if interrupted. Do not apply those actions to a guessed card, unrelated devices, or an entire session as a first step. The opt-in recovery path below uses bounded remedies and restoration handling; some hardware faults still need a user-controlled power cycle.

## Capture keepalive and health policy

The native daemon maintains one capture pin per supported Wave input and releases pins on shutdown. The mixer separately confirms raw capture readiness before Wave playback. Owned keepalive restart is normal operation, separate from permission to cycle hardware profiles.

Health checks are **observation-only by default**. `HealthMonitor(auto_recover=False)` logs confirmed faults without permitting card-profile cycles or sink suspend/resume. Missing commands, unknown graph/mute state and incomplete observations must not be treated as permission to recover.

```sh
openwave-daemon --help
openwave-daemon --version
# openwave-daemon --auto-recover
```

`--auto-recover` is an explicit opt-in for bounded disruptive remedies on confirmed faults. Capture xruns and no-data faults share at most two card cycles per incident, separated by at least 60 seconds; five minutes of healthy observations rearm the budget. Muting, unplugging, an unknown graph or a recreated counter cannot refill it. Output suspend/resume has a separate two-attempt budget rearmed by six advancing hardware-pointer observations. This is not a service-wide restart policy or a guarantee that a failed microphone will recover. Cancellation still requires restoring the exact profile or suspension state changed by an in-flight attempt.

Use the flag on the daemon invocation you actually run; do not start an extra daemon beside the service. Inspect a systemd installation with:

```sh
systemctl --user status openwave.service
journalctl --user -u openwave.service
```

Host setup and recovery remain outside Flatpak. See [Bazzite/native installation](install-bazzite.md) for the supported boundary.
