# Linux hardware investigation: Razer Naga V2 HyperSpeed

This note records observations from a physical Razer Naga V2 HyperSpeed on CachyOS/Linux.
It is intended as durable context for future contributors/agents. Do not treat Linux
input event codes, HID report IDs/usages, and Sidearm onboard-memory button IDs as
interchangeable namespaces.

## Device identity and USB/HID interfaces

- USB VID:PID: `1532:00B4`
- evtest device name: `Razer Razer Naga V2 HyperSpeed`
- USB bus: `0x3`; device version reported by evtest: `0x111`
- All three HID devices below belong to the same physical mouse.

| HID device suffix | USB interface number | Linux event node | Observed role |
|---|---:|---|---|
| `0009` | `00` | `/dev/input/event12` | Main mouse input |
| `000A` | `01` | `/dev/input/event14` | Composite keyboard/mouse/consumer/system/vendor input |
| `000B` | `02` | `/dev/input/event13` | Keyboard input; side-grid buttons |

The suffix in `0003:1532:00B4.000X` is the HID device instance, not the USB
interface number. The observed mapping above is based on sysfs DEVPATH and
udev properties.

### udev observations

- event12 DEVPATH includes `7-3.3.4:1.0/0003:1532:00B4.0009`; `ID_USB_INTERFACE_NUM=00`,
  `ID_INPUT_MOUSE=1`, `RAZER_DRIVER=razermouse`.
- event13 DEVPATH includes `7-3.3.4:1.2/0003:1532:00B4.000B`; `ID_USB_INTERFACE_NUM=02`,
  `ID_INPUT_KEY=1`, `ID_INPUT_KEYBOARD=1`, `RAZER_DRIVER=razermouse`.
- event14 DEVPATH includes `7-3.3.4:1.1/0003:1532:00B4.000A`; `ID_USB_INTERFACE_NUM=01`,
  `ID_INPUT_MOUSE=1`, `ID_INPUT_KEY=1`, `ID_INPUT_KEYBOARD=1`,
  `RAZER_DRIVER=razermouse`.

## evtest observations

Commands used: `sudo evtest --grab /dev/input/event12`, and similarly for event13
and event14. `--grab` was used while testing.

### event12 — main mouse

Supported key events:
- `BTN_LEFT` (272)
- `BTN_RIGHT` (273)
- `BTN_MIDDLE` (274)
- `BTN_SIDE` (275)
- `BTN_EXTRA` (276)

Supported relative events:
- `REL_X` (0), `REL_Y` (1)
- `REL_HWHEEL` (6), `REL_WHEEL` (8)
- `REL_WHEEL_HI_RES` (11), `REL_HWHEEL_HI_RES` (12)

Captured physical actions:
- Left click: `MSC_SCAN 90001`, `BTN_LEFT` down/up.
- Right click: `MSC_SCAN 90002`, `BTN_RIGHT` down/up.
- Wheel click: `MSC_SCAN 90003`, `BTN_MIDDLE` down/up.
- Wheel up/down: `REL_WHEEL` values -1/+1, paired with high-resolution values -120/+120.
- Wheel tilt left/right: `REL_HWHEEL` values -1/+1, paired with high-resolution values -120/+120.
- Mouse movement produces `REL_X`/`REL_Y`.

Important: tilt is exposed as horizontal wheel/pan movement, not as a pair of
ordinary mouse-button click events in this interface. Although BTN_SIDE and
BTN_EXTRA are advertised as supported capabilities, this capture did not include
presses for them. Do not assume that tilt corresponds to Sidearm's `mouse_4` /
`mouse_5` entries.

### event13 — side-grid keyboard interface

Supported events include a broad keyboard usage range. The captured sequence for
the 12 side-grid buttons was:

| MSC_SCAN | Linux key event |
|---|---|
| `0x70068` | `KEY_F13` (183) |
| `0x70069` | `KEY_F14` (184) |
| `0x7006a` | `KEY_F15` (185) |
| `0x7006b` | `KEY_F16` (186) |
| `0x7006c` | `KEY_F17` (187) |
| `0x7006d` | `KEY_F18` (188) |
| `0x7006e` | `KEY_F19` (189) |
| `0x7006f` | `KEY_F20` (190) |
| `0x70070` | `KEY_F21` (191) |
| `0x70071` | `KEY_F22` (192) |
| `0x70072` | `KEY_F23` (193) |
| `0x70073` | `KEY_F24` (194) |

Each captured key produced a press and release. The physical buttons were pressed
one by one in sequence; this establishes the observed order of the 12 keys, but
the mapping to a particular physical grid position should be documented only when
the tester's press order/position is known.

### event14 — composite interface

The node advertises an extensive keyboard key set, mouse buttons, relative wheel
and movement events, `ABS_VOLUME`, and consumer/system keys. However, running
`sudo evtest --grab /dev/input/event14` and trying the additional physical
controls produced **no events** in the recorded test.

This is a negative observation, not proof that the interface is unused. It may
require a different operating mode, report path, or a specific control action.
Do not assign the extra controls based on capabilities alone.

## HID report descriptor observations

Descriptors were decoded with:

```fish
for d in /sys/bus/hid/devices/0003:1532:00B4.*
    if test -r "$d/report_descriptor"
        echo
        echo "========== $d =========="
        sudo hid-decode "$d/report_descriptor"
    end
end
```

### `0009` — USB interface 00 / event12

Descriptor highlights:
- Mouse application collection.
- Standard Button usage range 1–5 (five buttons).
- Consumer Devices usage `AC Pan` as a signed relative byte.
- Generic Desktop `Wheel` as a signed relative byte.
- Relative X and Y as signed 16-bit values.
- A vendor-defined Feature item: Usage Page `0xFF00`, Usage `0x02`,
  Feature `Cnst,Arr,Abs`, report count 90 bytes.

The descriptor declares a 90-byte Feature item, but its Main item is
\`Feature (Cnst,Arr,Abs)\` (constant), not a normal data feature field. Do not assume
this is the writable configuration/onboard-memory channel. Its descriptor does not
reveal the meaning of any payload bytes or establish the Sidearm onboard button IDs.
The actual Razer vendor-protocol path must be established from the implementation
and/or controlled raw HID report captures.

### `000A` — USB interface 01 / event14

Descriptor highlights:
- Keyboard application collection, Report ID `0x01`, including modifiers and
  an array of keyboard usages.
- Mouse application collection, Report ID `0x0A`, six standard button usages,
  a vendor-defined usage `0x40` with two signed bytes, wheel, and relative X/Y.
- Consumer Control application collection, Report ID `0x02`, one 16-bit usage
  value with logical range 0–572.
- System Control application collection, Report ID `0x03`.
- Vendor/undefined application collections with input report IDs `0x04`,
  `0x05`, `0x08`, and `0x09`, each with 15-byte input payloads.

These are descriptor declarations, not evidence that the corresponding physical
controls generated events during the test.

### `000B` — USB interface 02 / event13

Descriptor highlights:
- Keyboard application collection, no explicit Report ID.
- Modifier bits, then a constant byte, then a keyboard usage array.
- This is consistent with the Linux keyboard interface used to emit F13–F24
  for the 12 side-grid buttons.

## Implications for Sidearm development

Sidearm's current onboard button table in
`src-tauri/src/linux_onboard_backup.rs` contains assumptions such as
`0x40`–`0x4B` for `thumb_01`–`thumb_12`, plus `0x0B`/`0x0C` for
`top_aux_01`/`top_aux_02` and `0x34`/`0x35` for `mouse_4`/`mouse_5`.
Those values are Sidearm protocol assumptions, **not confirmed by these evtest
captures or HID descriptors**.

Keep these namespaces distinct:
1. Linux input event codes (e.g. `KEY_F13`, `BTN_MIDDLE`, `REL_HWHEEL`).
2. HID usages, report IDs, and raw report payloads (e.g. report ID `0x0A`,
   Keyboard usage `0x68`).
3. Razer vendor-protocol/onboard-memory button identifiers used by Sidearm.

Next investigation should inspect the existing Razer vendor-protocol implementation
and capture/interpret relevant HID feature/output reports, especially the 90-byte
feature report declared on interface 00. Do not infer a direct numeric mapping
between the HID report IDs/usages and Sidearm's onboard button IDs. Further physical
button tests should log the exact physical position pressed alongside the emitted
event or raw report.
