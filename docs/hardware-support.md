# Hardware support

OpenWave enables exact VID:PID profiles, not a family-wide match on the word “Wave”. All listed USB IDs use Elgato vendor ID `0fd9`. The native authority is [`openwave_core::profiles::PROFILES`](../crates/openwave-core/src/profiles.rs), selected by `profile_for_usb`; unsupported IDs have no production protocol profile.

| Product / variant | PID | Software scope |
|---|---|---|
| Wave XLR | `007d` | Enabled XLR profile: 34-byte config, 10-byte meter, 51-byte device info |
| XLR Dock / Wave XLR MK.2, `00a6` variant | `00a6` | Enabled XLR-layout profile; product naming does not cover other Dock/MK.2 PIDs |
| Wave:3 | `0070` | Enabled microphone profile: 16-byte config, 8-byte meter, 64-byte device info |
| Unverified Dock variant | `00c7` | Disabled; no assumed register layout or writes |
| Any other PID, including `00b6` | Other | Not enabled by these profiles |

“Enabled” means the implementation contains that exact profile. It does not mean physical `00a6` or multi-unit acceptance testing has been completed, nor that every firmware revision has been verified. No physical `00c7` validation is claimed. A marketing name, matching block length or successful read alone is not enough evidence to enable another PID.

## Controls

| Control | `007d` / `00a6` profile | `0070` profile |
|---|---|---|
| Microphone gain | Q8.8 dB, up to 80 dB | Q8.8 dB, up to 40 dB |
| Mute | Yes | Yes |
| Headphone volume | Signed Q8.8 dB | Signed Q8.8 dB |
| 48 V phantom power | Yes, config byte 6 | No |
| Low impedance mode | Yes | No |
| Microphone/PC monitor mix | No | Yes, 0–100% |
| Knob mode, firmware/API/serial, meters | Profile-defined | Profile-defined |

Controls absent from a profile are hidden, not emulated. Profile capabilities also govern ALSA synchronization; ALSA control names/ranges are discovered instead of assuming fixed numeric control IDs. [`openwave_core::protocol`](../crates/openwave-core/src/protocol.rs) validates profile fields and exact identity matches; [`openwave_runtime::device`](../crates/openwave-runtime/src/device.rs) owns USB access and ALSA synchronization.

**48 V is a real hardware write.** Verify the selected unit, cable and microphone manufacturer's instructions before enabling it. Some microphones and connected equipment must not receive phantom power. Reduce monitoring levels before changing power or connecting equipment. A scene deliberately cannot toggle phantom power. Do not use raw probe writes to bypass these precautions.

## Multiple units and hotplug

- Discovery distinguishes connected units by profile and USB bus/address. The selector displays serial where available, otherwise the runtime USB location. Bus/address can change after replugging and is not a durable serial substitute.
- `DeviceManager` gives each connection incarnation an independent serialized worker for connection, polling, read-modify-write controls and disconnect. An operation captures its `UnitId`; changing the selection cancels unsent debounced device edits but does not transfer queued work to another unit. Retirement drains that unit's queue before disconnect.
- ALSA pairing uses exact USB VID:PID and bus/device identity, with exact serial evidence where available. Ambiguity must not become “first Elgato card wins”. If identity cannot be established safely, hardware/ALSA synchronization is unavailable rather than aimed at an arbitrary card.
- Known unrelated USB products are filtered before requiring their ALSA bus metadata; damaged unrelated hotplug records do not select or block a valid exact target. Ambiguous or incomplete possible targets still fail closed.
- Temporarily empty/partial ALSA controls are rediscovered on the existing ALSA polling interval without reopening the USB unit. Identity is checked around discovery and before writes; complete controls stop rediscovery, and unknown controls never use guessed numeric IDs.
- The capture manager keeps one pin per Wave input. Adding or removing one input must not tear down the remaining inputs' pins.
- Saved scene hardware entries resolve by exact serial. A legacy model-only entry is eligible only when that model has exactly one connected candidate. A missing or ambiguous device is reported; it is not substituted.

Capture source rows bind to their selected PipeWire node names. Two units of the same model still need distinct source bindings; a sidebar selection is not permission to silently replace another source's hardware identity.

## Reporting an unknown device

Include the USB VID:PID, reported product name, firmware if known, kernel and PipeWire versions, and the symptom. Start with [privacy-reduced diagnostics](troubleshooting.md#diagnostics-and-privacy). Do not add an unverified PID to udev/profile lists just to make the application connect, and do not probe unknown offsets as routine troubleshooting.

Engineers can consult the [protocol reference](protocol.md#engineer-only-probe) and native [`openwave_runtime::probe`](../crates/openwave-runtime/src/probe.rs) for transport details and the limitations of the single-device probe. It selects the first supported unit in bus/address order, not a user-specified unit. Close every vendor-control client before any explicit USB diagnostic read, and connect only one supported unit when investigating a specific target.
