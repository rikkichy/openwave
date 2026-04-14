# Wave XLR USB Protocol Specification

## Device

- **Vendor ID**: `0x0FD9` (Elgato)
- **Product ID**: `0x007D` (Wave XLR), `0x007E` (DFU mode)
- **Control Interface**: Interface 3 — Class `0xFF`, SubClass `0xF0`, 0 endpoints
- **Audio Interfaces**: 0-2 — standard USB Audio Class 1.0
- **DFU Interface**: 4

## Transport

All control communication uses **USB Class requests** on EP0:

- **Read**: `bmRequestType=0xA1` (IN, Class, Interface)
- **Write**: `bmRequestType=0x21` (OUT, Class, Interface)
- **bRequest**: `0x85` (read), `0x05` (write)
- **wIndex**: `0x3300` (always)

**NOT Vendor requests** — the type bits are Class (01), not Vendor (10).

## Commands

| bmRequestType | bRequest | wValue | wLength | Description |
|---------------|----------|--------|---------|-------------|
| `0xA1` | `0x85` | `0x0000` | 34 | Read config (all settings) |
| `0xA1` | `0x85` | `0x0001` | 10 | Read metering (input levels) |
| `0xA1` | `0x85` | `0x000A` | 2 | Read API version |
| `0xA1` | `0x85` | `0x000A` | 51 | Read device info (FW version, serial) |
| `0x21` | `0x05` | `0x0000` | 34 | Write config (read-modify-write) |

## Config Struct (34 bytes, wValue=0x0)

| Offset | Size | Type | Field | Encoding |
|--------|------|------|-------|----------|
| 0-1 | 2 | uint16 LE | **input_gain** | Internal scale, ~linear with dB. 0x0000=0dB, 0x2200≈31dB |
| 2 | 1 | uint8 | unknown | `0x00` |
| 3 | 1 | uint8 | unknown | `0xEC` |
| 4 | 1 | uint8 | **input_mute** | 0=unmuted, 1=muted |
| 5-8 | 4 | — | reserved | all `0x00` |
| 9-10 | 2 | int16 LE | **headphone_volume** | Signed Q8.8 dB. Divide by 256 for dB. Range: -30.5 to 0 dB |
| 11-13 | 3 | — | reserved | all `0x00` |
| 14 | 1 | uint8 | **volume_select** | 1=knob→gain, 2=knob→HP volume |
| 15 | 1 | uint8 | unknown | `0xFF` (likely clipguard or lowcut) |
| 16-17 | 2 | — | unknown | `0x00 0x00` |
| 18-26 | 9 | — | unknown | all `0xFF` (likely DSP enable flags) |
| 27 | 1 | uint8 | unknown | `0x01` |
| 28 | 1 | uint8 | unknown | `0x01` |
| 29 | 1 | uint8 | unknown | `0xFF` |
| 30 | 1 | uint8 | unknown | `0x37` (55) |
| 31 | 1 | uint8 | unknown | `0x00` |
| 32 | 1 | uint8 | unknown | `0x01` |
| 33 | 1 | uint8 | **low_impedance** | 0=disabled, 1=enabled |

### Gain Encoding (bytes 0-1)

Little-endian uint16, internal scale (not direct dB).

| USB Value (LE) | Approx dB |
|----------------|-----------|
| `0x0000` | 0 |
| `0x2200` | ~31 |
| `0x2980` | ~39 |
| `0x48C0` | ~73 |

WebSocket API reports range 0-75 dB.

### Headphone Volume (bytes 9-10)

Little-endian signed int16, Q8.8 fixed-point dB.

| USB Value (LE) | dB |
|----------------|----|
| `0xE180` | -30.5 |
| `0xF100` | -15.0 |
| `0xFE33` | -1.8 |
| `0x0000` | 0.0 |

## Metering Block (10 bytes, wValue=0x1)

| Offset | Size | Type | Field |
|--------|------|------|-------|
| 0-3 | 4 | uint32 LE | input_level_left |
| 4-7 | 4 | uint32 LE | input_level_right |
| 8-9 | 2 | uint16 LE | flag (always 0x0001) |

Both levels are identical (mono mic). Range: ~0x00-0xFF.

## Device Info Block (wValue=0xA)

Short read (2 bytes): returns API version, e.g. `01 03` = v1.3.

Full read (51 bytes):
```
Offset  Meaning
0-1     API version (01 03)
6-8     Firmware version (03 07 03 = v3.7.3?)
27-46   Device UID + serial number (ASCII, e.g. "9898fa1dDS26M2A02307")
47-50   Hardware info
```

## Polling Pattern

Wave Link polls at **10 Hz** (100ms interval):
1. Read config (wValue=0x0, 34 bytes)
2. Read meters (wValue=0x1, 10 bytes)
3. Sleep ~100ms
4. Repeat

No device-push notifications — purely host-polled.

## Startup / Reconnect Sequence

1. Read API version (wValue=0xA, 2 bytes)
2. Read config (wValue=0x0, 34 bytes)
3. Read meters (wValue=0x1, 10 bytes)
4. Read full device info (wValue=0xA, 51 bytes)
5. Read config again
6. **Write config** (wValue=0x0, 34 bytes) — restores saved settings
7. Begin steady-state 10 Hz polling

## Write Protocol

Read-modify-write — always read the full 34-byte config first, modify desired bytes, write back the entire struct.

## Quick Reference (Python)

```python
import usb.core, usb.util

dev = usb.core.find(idVendor=0x0FD9, idProduct=0x007D)
if dev.is_kernel_driver_active(3):
    dev.detach_kernel_driver(3)
usb.util.claim_interface(dev, 3)

# Read config
config = bytearray(dev.ctrl_transfer(0xA1, 0x85, 0x0000, 0x3300, 34))

# Read meters
meters = dev.ctrl_transfer(0xA1, 0x85, 0x0001, 0x3300, 10)

# Read device info
version = dev.ctrl_transfer(0xA1, 0x85, 0x000A, 0x3300, 2)
devinfo = dev.ctrl_transfer(0xA1, 0x85, 0x000A, 0x3300, 51)

# Write config (mute example)
config[4] = 0x01  # mute ON
dev.ctrl_transfer(0x21, 0x05, 0x0000, 0x3300, bytes(config))
```

## Unmapped Parameters (need more captures)

From binary analysis, these exist but byte offsets are unknown:
- Clipguard enable, Low-cut enable
- Headphone mute, Direct monitor level
- Phantom power (48V), Gain lock
- LED brightness/colors

DSP effects (compressor, EQ, expander) likely use additional wValue addresses.
