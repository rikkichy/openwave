# Wave XLR USB Protocol — Complete Findings

## Architecture (from binary reverse engineering)

The Wave Link app uses a layered C++/Swift architecture for USB (namespace `LWT::`, developed by Lewitt Audio, USB driver by Thesycon):

1. **Swift UI** → `WaveXLRController` / `WaveXLRMk2Controller`
2. **Session API** (`WaveAPI.framework`) → `LWT::SessionAPI` — manages parameter trees, serializes to `LWT::IMessage`
3. **Vendor USB Backend** → `LWT::VendorUSBLewittDeviceBackend` with `MK2VendorUSBBackendStrategy` (current devices)
4. **Thesycon macOS Backend** → `LWT::LewittDeviceThesyconMacBackend` — actual IOKit USB interface

## Device Info

- **Vendor ID**: `0x0FD9` (Elgato)
- **Product ID**: `0x007D` (Wave XLR normal mode), `0x007E` (DFU mode)
- **Control Interface**: Interface 3, Vendor Specific Class (`0xFF`, SubClass `0xF0`), no endpoints
- **Communication**: USB Control Transfers on EP0
- **Audio**: Interfaces 0-2, standard USB Audio Class 1.0 (mono capture, stereo playback, 24-bit, 48/96kHz)

## USB Protocol (confirmed via Frida runtime capture)

All communication uses **vendor-specific control transfers** with:
- **wIndex**: `0x3300` (always — encodes interface 3)

### Transfer Types

| Direction | bmRequestType | bRequest | wValue | Length | Purpose |
|-----------|--------------|----------|--------|--------|---------|
| IN (read) | `0xA1` (Class, Interface) | `0x85` | `0x0000` | 34 | Read config struct (all settings) |
| IN (read) | `0xA1` (Class, Interface) | `0x85` | `0x0001` | 10 | Read metering (input levels) |
| OUT (write) | `0x21` (Class, Interface) | `0x05` | `0x0000` | 34 | Write config struct (change settings) |

**IMPORTANT**: These are **Class** requests (type bits = 01), NOT Vendor requests (type bits = 10). The Frida capture confirmed `isVendor=0`. The Thesycon backend's 7-param function passes the interface number (3) as a separate parameter.

The device is polled continuously (~10 Hz). The host reads the full config + metering on every tick, and writes the full 34-byte config struct when any setting changes.

## Config Struct — 34 Bytes (wValue=0x0)

### Decoded Byte Map

| Offset | Size | Type | Field | Values |
|--------|------|------|-------|--------|
| 0-1 | 2 | uint16 LE | `input_gain` | See gain encoding below |
| 2 | 1 | uint8 | `unknown_02` | `0x00` observed |
| 3 | 1 | uint8 | `unknown_03` | `0xEC` observed |
| **4** | 1 | uint8 | **`input_mute`** | `0x00` = unmuted, `0x01` = muted |
| 5-7 | 3 | — | reserved | `0x00 0x00 0x00` |
| 8 | 1 | uint8 | `unknown_08` | `0x00` observed |
| 9-10 | 2 | int16 LE | **`headphone_volume`** | Signed Q8.8 dB (see below) |
| 11-13 | 3 | — | reserved | `0x00 0x00 0x00` |
| **14** | 1 | uint8 | **`volume_select`** | `0x01` = knob→gain, `0x02` = knob→HP |
| 15 | 1 | uint8 | `unknown_15` | `0xFF` — likely clipguard or lowcut |
| 16-17 | 2 | — | unknown | `0x00 0x00` |
| 18-26 | 9 | — | unknown | all `0xFF` — likely DSP enable flags |
| 27 | 1 | uint8 | `unknown_27` | `0x01` |
| 28 | 1 | uint8 | `unknown_28` | `0x01` |
| 29 | 1 | uint8 | `unknown_29` | `0xFF` |
| 30 | 1 | uint8 | `unknown_30` | `0x37` (55 decimal) |
| 31 | 1 | uint8 | `unknown_31` | `0x00` |
| 32 | 1 | uint8 | `unknown_32` | `0x01` |
| **33** | 1 | uint8 | **`low_impedance`** | `0x00` = disabled, `0x01` = enabled |

### Input Gain (bytes 0-1)

Stored as **little-endian uint16**. The value is NOT direct dB — it appears to be an internal representation, possibly a DAC/PGA register value.

Observed sweep (hardware knob turned up, then down):

| USB LE | Byte[0] Byte[1] | Approx position |
|--------|-----------------|-----------------|
| `0x2200` | `00 22` | ~31 dB (initial) |
| `0x24C0` | `C0 24` | gaining... |
| `0x2940` | `40 29` | ~41 dB |
| `0x3240` | `40 32` | ~50 dB |
| `0x3B40` | `40 3B` | ~59 dB |
| `0x4440` | `40 44` | ~68 dB |
| `0x48C0` | `C0 48` | ~73 dB (near max) |

WebSocket API reports gain range 0-75 dB, with initial value 0.41333 (normalized) = 31 dB at USB value 0x2200. The USB value appears to scale linearly with dB.

### Headphone Volume (bytes 9-10)

Stored as **little-endian signed int16** in **Q8.8 fixed-point dB**. Divide by 256 to get dB.

| USB LE | Signed int16 | dB |
|--------|-------------|-----|
| `0xE180` | -7808 | **-30.50** |
| `0xE89A` | -5990 | -23.40 |
| `0xF100` | -3840 | -15.00 |
| `0xF833` | -1997 | -7.80 |
| `0xFBCD` | -1075 | -4.20 |
| `0xFE33` | -461 | **-1.80** |

Range: approximately **-30.5 dB** to **0 dB**.

### Mute (byte 4)

- `0x00` = unmuted
- `0x01` = muted

Controlled by hardware button or software.

### Volume Select (byte 14)

Indicates which parameter the physical knob currently controls:
- `0x01` — Mic gain
- `0x02` — Headphone volume

Changes automatically when user interacts with the knob ring.

### Low Impedance Mode (byte 33)

- `0x00` = high impedance (disabled)
- `0x01` = low impedance (enabled)

Changed via software only (Wave Link UI).

## Metering Block — 10 Bytes (wValue=0x1)

| Offset | Size | Type | Field |
|--------|------|------|-------|
| 0-3 | 4 | uint32 LE | `input_level_left` |
| 4-7 | 4 | uint32 LE | `input_level_right` |
| 8-9 | 2 | uint16 LE | `flag` (always `0x0001`) |

Both levels are always identical (mono mic). Fluctuates with audio input, observed range ~0xB0-0xE8.

## Device Info Block — 51 Bytes (wValue=0xA)

Read on startup to identify the device. First read returns 2 bytes (short version), second returns full 51 bytes.

```
# Short version check (2 bytes)
bmRequestType=0xA1  bRequest=0x85  wValue=0x000A  wIndex=0x3300  wLength=2
Response: 01 03  (API version 1.3)

# Full device info (51 bytes)
bmRequestType=0xA1  bRequest=0x85  wValue=0x000A  wIndex=0x3300  wLength=51
```

Decoded (51 bytes):
```
Offset  Data                  Meaning
0-1     01 03                 API version (1.3)
2-3     00 00                 unknown
4       07                    unknown
5       00                    unknown
6-8     03 07 03              Firmware version (3.7.3?)
9-26    00...                 Reserved/padding
20-21   01 03                 Repeated API version?
22-23   01 01                 unknown
24-26   00 00 00              padding
27-46   "9898fa1dDS26M2A02307" Device UID + serial number (ASCII)
47-50   84 03 dc 05           Hardware revision or checksum
```

## Polling Pattern (confirmed via Frida timing analysis)

Wave Link polls at exactly **10 Hz** (100ms interval):
1. Read config (wVal=0x0, 34 bytes) — dt~0ms after previous meter read
2. Read meters (wVal=0x1, 10 bytes) — dt~0ms after config read
3. Sleep ~100ms
4. Repeat

On **startup/reconnect**, the init sequence is:
1. Read version (wVal=0xA, 2 bytes)
2. Read config (wVal=0x0, 34 bytes)
3. Read meters (wVal=0x1, 10 bytes)
4. Read full device info (wVal=0xA, 51 bytes)
5. Read config again
6. **Write config** (wVal=0x0, 34 bytes) — restores saved settings from host
7. Begin steady-state polling

No device-push notifications — the device is purely host-polled.

## Write Protocol

To change any setting:
1. Read current config: `bmRequestType=0xC1, bRequest=0x85, wValue=0x0000, wIndex=0x3300, wLength=34`
2. Modify desired byte(s) in the 34-byte buffer
3. Write entire struct back: `bmRequestType=0x41, bRequest=0x05, wValue=0x0000, wIndex=0x3300, wLength=34`

## Parameter Tree (from binary strings analysis)

These are all the parameters found in the Wave Link binary. The ones marked **CONFIRMED** have been mapped to specific byte offsets.

### Config Parameters (`/config/`)
- `input_gain` — **CONFIRMED: bytes 0-1**
- `input_mute` — **CONFIRMED: byte 4**
- `headphone_volume` — **CONFIRMED: bytes 9-10**
- `volume_select` — **CONFIRMED: byte 14**
- `low_impedance_enabled` — **CONFIRMED: byte 33**
- `clipguard_enable` — NOT YET MAPPED (likely in bytes 15-32)
- `lowcut_enable` — NOT YET MAPPED
- `headphone_mute` — NOT YET MAPPED
- `direct_monitor` — NOT YET MAPPED
- `p48_enable` — phantom power (48V), NOT YET MAPPED
- `gain_lock` — NOT YET MAPPED
- `leds_flip` — flip LEDs for upside-down mounting
- `brightnessRed`, `brightnessWhite` — LED brightness
- `headphone_color_rgb`, `microphone_color_rgb`, `mix_color_rgb`, `mute_color_rgb` — LED colors
- `gr_enabled`, `gr_color` — gain reduction meter
- `lowcut1_enabled`, `lowcut2_enabled` — two-stage low-cut

### DSP FX Parameters (`/dspfx/`)

These likely live in a **separate wValue block** (not wValue=0x0). They were not observed in this capture because no DSP effects were toggled.

**Compressor:**
- `compressor/threshold_dB` (float)
- `compressor/ratio` (float)
- `compressor/attack_ms` (float)
- `compressor/release_ms` (float)
- `compressor/makeup_gain_dB` (float)

**Equalizer (4-band parametric, bands 0-3):**
- `equalizer/band/{0-3}/enabled` (int32_t)
- `equalizer/band/{0-3}/gain_dB` (float)
- `equalizer/band/{0-3}/frequency_Hz` (float)
- `equalizer/band/{0-3}/quality_log10` (float) — Q as log10
- `equalizer/band/{0-3}/type` (int32_t)

**Expander:**
- `expander/threshold_dB` (float)
- `expander/ratio` (float)
- `expander/attack_ms` (float)
- `expander/release_ms` (float)
- `expander/max_reduction_dB` (float)

**Bettermaker:**
- `bettermaker/knob` (float)

### DSP Enable Flags
- `compressor_enabled`, `equalizer_enabled`, `expander_enabled`
- `lowcut_enabled`, `bettermaker_enabled`

### Metering (read-only from device)
- `compressor/attenuation_dB`, `compressor/key_level_dB`, `compressor/out_level_dB`
- `expander/attenuation_dB`, `expander/key_level_dB`, `expander/out_level_dB`
- `hw_input/level/{0,1}`, `post_dsp/level/{0,1}`, `post_vst/level/{0,1}`

### Version/Identity
- `version/api/major`, `version/api/minor`
- `version/fw/major`, `version/fw/minor`, `version/fw/patch`
- `version/serial`

### Special
- `/writeflash` — persist settings to flash
- `/setting_store_pending` — pending flash write flag
- `/sample_rate_hz`

## Wave Link WebSocket API (for reference)

Wave Link 3.x exposes a JSON-RPC WebSocket at `ws://127.0.0.1:1884` (port range 1884-1893).

**Required header**: `Origin: streamdeck://`

Key methods:
- `getApplicationInfo` — app version info
- `getInputDevices` — input devices with DSP state
- `getOutputDevices` — output devices with volume/mute
- `getChannels` / `getMixes` — mixer channels
- `setInputDevice` / `setOutputDevice` — change settings

## What Still Needs Capture

To map the remaining unknown bytes and discover DSP transfer blocks:
1. Toggle **clipguard** on/off
2. Toggle **lowcut** on/off
3. Enable/disable **compressor** and change its parameters
4. Enable/disable **EQ** and change band gains
5. Enable/disable **expander**
6. Toggle **phantom power** (if available on XLR model)
7. Change **direct monitor** level
8. Read device on **fresh Wave Link startup** (to see initialization sequence)

## Quick Reference for Linux GTK App

```python
import usb.core, usb.util

# Find device
dev = usb.core.find(idVendor=0x0FD9, idProduct=0x007D)

# Claim interface 3 (detach kernel driver if needed)
try:
    if dev.is_kernel_driver_active(3):
        dev.detach_kernel_driver(3)
except: pass
usb.util.claim_interface(dev, 3)

# Read config (34 bytes) — Class request, NOT Vendor!
# bmRequestType 0xA1 = IN, Class, Interface
config = bytearray(dev.ctrl_transfer(0xA1, 0x85, 0x0000, 0x3300, 34))

# Read meters (10 bytes)
meters = dev.ctrl_transfer(0xA1, 0x85, 0x0001, 0x3300, 10)

# Write config (read-modify-write)
# bmRequestType 0x21 = OUT, Class, Interface
config[4] = 0x01  # mute ON
dev.ctrl_transfer(0x21, 0x05, 0x0000, 0x3300, bytes(config))

# CONFIRMED WORKING — tested mute/unmute successfully
```
