# Elgato Wave control protocol

This is an engineering reference for the profiles enabled in OpenWave, derived from reverse engineering rather than a vendor specification. It describes implemented offsets, not a physical-validation certificate. See [hardware support](hardware-support.md) for the exact PID scope and cautions.

## Transport and ownership

The vendor configuration protocol uses USB Class control transfers on endpoint 0:

| Field | Read | Write |
|---|---|---|
| `bmRequestType` | `0xA1` (class/interface/IN) | `0x21` (class/interface/OUT) |
| `bRequest` | `0x85` | `0x05` |
| `wValue` | Block selector | Block selector |
| `wIndex` | `0x3303` | `0x3303` |

Wave Link's `0x3300` addresses interface 0, owned by Linux's `snd-usb-audio`. The enabled profiles use `0x3303`, retaining the protocol's `0x33` prefix while targeting interface 3. OpenWave does not detach the audio driver for these transfers. This technique is profile-specific, not a guarantee that arbitrary transfers cannot disrupt hardware.

Only one process should own vendor transfers to a unit. A competing GUI, diagnostic collector or probe can produce `-EIO`/read failures. Quit OpenWave **including its tray process** before probing or using diagnostics with `--device`. Within the GUI, each device's queue serializes polling and writes; it must not be bypassed by another USB client.

Writes are whole-block read-modify-write operations: read the profile's config, patch a supported field, write the block. Preserve all other bytes. No offset is safe merely because a similarly named product uses it.

## Blocks and encodings

All multibyte fields below are little-endian.

| Block | `wValue` | `007d` / `00a6` length | `0070` length |
|---|---|---|---|
| Config | `0x0000` | 34 bytes | 16 bytes |
| Meter | `0x0001` | 10 bytes | 8 bytes |
| Device info | `0x000A` | 51 bytes | 64 bytes |

The meter begins with two unsigned 32-bit levels. Device-info API version is at bytes 0–1. XLR-profile firmware is at 6–8 and serial at 27–46; Wave:3 firmware is at 21–23 and serial at 36–47.

### XLR profiles: `0fd9:007d` and `0fd9:00a6`

| Offset | Type | Field |
|---|---|---|
| 0 | uint16 | Gain: raw / 256 dB; maximum `0x5000` (80 dB) |
| 4 | byte | Mute: `1` muted, `0` live |
| 6 | byte | 48 V phantom: `1` on, `0` off |
| 9 | int16 | Headphone level: raw / 256 dB; zero is unity |
| 14 | byte | Knob mode: `2` selects headphone volume |
| 33 | byte | Low impedance mode: `1` on, `0` off |

The `00a6` profile uses this layout. That does not extend it to `00c7`, `00b6` or every device sold as a Dock/MK.2.

### Wave:3: `0fd9:0070`

| Offset | Type | Field |
|---|---|---|
| 0 | uint16 | Gain: raw / 256 dB; maximum `0x2800` (40 dB) |
| 4 | byte | Mute: `1` muted, `0` live |
| 7 | int16 | Headphone level: raw / 256 dB |
| 10 | uint16 | Microphone/PC mix: raw / 256 percent; maximum `0x6400` (100%) |
| 12 | byte | Dial mode: `1` gain, `2` headphones, `3` monitor mix |

No phantom or low-impedance offset is defined for Wave:3. All unlisted bytes remain unknown/reserved.

## Engineer-only probe

Use the native `openwave-probe` launcher with dependencies and USB permissions already configured. From a source checkout, the equivalent is `python3 -m wavexlr.probe`; native packages' private module tree is not automatically importable by the system Python. The probe has no per-unit selection flag: it connects to the first supported unit. **Use only one connected supported unit when investigating a specific device**, and verify the printed model/card before proceeding. Do not use it as a multi-device control interface.

Read-oriented commands:

```sh
openwave-probe dump
openwave-probe watch --interval 0.1
```

`dump` reports expected versus returned lengths for config/meter/device-info blocks. `watch` prints changed byte offsets while you move one physical control at a time; Ctrl+C stops it. Dumps can contain serials and are not privacy-redacted. Custom `dump --wvalue 0xN --len N` requests are protocol research, not a general device-health check.

**`poke` writes hardware. Even `poke --noop` sends a full config write, without a confirmation prompt.** Its unchanged payload can still trigger firmware side effects; it is not a read-only verification command. The interface is `poke --offset N --byte VALUE`, with an interactive confirmation unless `--yes` is supplied. Do not write unknown offsets, copy byte numbers across profiles, or demonstrate writes by toggling phantom power. Disconnect sensitive equipment and establish the exact target/layout before any controlled write experiment.

A successful unchanged write does not establish that unknown fields are safe or that another PID is compatible. New hardware support requires independently reviewed identity, block and field evidence before enabling writes.
