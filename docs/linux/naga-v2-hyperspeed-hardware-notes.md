# Linux hardware notes: Razer Naga V2 HyperSpeed (1532:00B4)

These notes record live diagnostics from the user's CachyOS / KDE Plasma Wayland system. They are intended as durable hand-off context for future work on Sidearm's Linux onboard-button mapping.

## Device and input interfaces

All three evdev nodes identify as:

- Name: `Razer Razer Naga V2 HyperSpeed`
- USB ID: `1532:00B4`
- Bus: USB (evtest bus `0x3`)
- Kernel driver reported by udev: `usbhid`; `RAZER_DRIVER=razermouse`

udev identified the interfaces as:

| evdev node | HID device path suffix | USB interface | udev classification |
|---|---|---:|---|
| `/dev/input/event12` | `0003:1532:00B4.0009` | `00` | mouse |
| `/dev/input/event14` | `0003:1532:00B4.000A` | `01` | mouse + keyboard |
| `/dev/input/event13` | `0003:1532:00B4.000B` | `02` | keyboard |

Do not infer Sidearm onboard-memory button IDs from evdev node numbers, USB interface numbers, Linux key codes, or HID usages. They are separate namespaces.

## evtest observations

The user ran `sudo evtest --grab` on all three nodes.

### event12 — primary mouse interface (USB interface 00)

Supported events:

- `EV_KEY`: `BTN_LEFT` (272), `BTN_RIGHT` (273), `BTN_MIDDLE` (274), `BTN_SIDE` (275), `BTN_EXTRA` (276)
- `EV_REL`: `REL_X`, `REL_Y`, `REL_HWHEEL` (6), `REL_WHEEL` (8), `REL_WHEEL_HI_RES` (11), `REL_HWHEEL_HI_RES` (12)
- `EV_MSC/MSC_SCAN`

Observed presses/releases:

- Left: scan `0x90001`, `BTN_LEFT`
- Right: scan `0x90002`, `BTN_RIGHT`
- Wheel click: scan `0x90003`, `BTN_MIDDLE`
- Vertical wheel: `REL_WHEEL` +/-1 and high-resolution +/-120
- Wheel tilt: `REL_HWHEEL` +/-1 and high-resolution +/-120
- Pointer movement: `REL_X` / `REL_Y`

Important: wheel tilt is reported as relative horizontal wheel movement, not as `BTN_SIDE` / `BTN_EXTRA` button presses in this interface. Do not equate tilt with Mouse 4/5.

### event13 — keyboard interface (USB interface 02)

Supported events include the normal keyboard key set and F13-F24. During the user's sequential side-grid button test, the following pairs were observed:

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

Each key was seen with value 1 (press) and value 0 (release). The user's test sequence corresponded to the 12 side-grid buttons, but this transcript does not establish a physical top-to-bottom/left-to-right numbering convention. Ask the user to confirm the physical order before drawing a labelled layout.

### event14 — composite interface (USB interface 01)

The user ran `sudo evtest --grab /dev/input/event14`. It advertised a very broad set of keyboard, consumer, mouse, and miscellaneous key capabilities, including `BTN_0`, `BTN_LEFT`, `BTN_RIGHT`, `BTN_MIDDLE`, `BTN_SIDE`, `BTN_EXTRA`, `BTN_FORWARD`, many media/system keys, `ABS_VOLUME`, and relative wheel/pointer axes.

**No actual events were produced on event14 during the user's physical-button test.** The advertised capabilities are not evidence that each listed key is emitted by a physical control. Do not assign controls based on the capability list alone.

## HID report descriptor inspection

The user ran:

```fish
for d in /sys/bus/hid/devices/0003:1532:00B4.*
    if test -r "$d/report_descriptor"
        echo
        echo "========== $d =========="
        sudo hid-decode "$d/report_descriptor"
    end
end
```

All three descriptors decoded successfully.

### `0003:1532:00B4.0009` — USB interface 00 / event12

Descriptor contains:

- Generic Desktop / Mouse application collection.
- Button page usages 1–5 (five button bits).
- Consumer Devices usage `AC Pan` as a signed relative byte.
- Generic Desktop `Wheel` as a signed relative byte.
- Relative X/Y axes (16-bit each).
- Vendor-defined Feature report: usage page `0xFF00`, usage `0x02`, report count 90 bytes.

This descriptor is consistent with event12's primary mouse and horizontal/vertical wheel reports. The 90-byte vendor feature report is notable for later protocol investigation, but its contents/meaning are not established by this descriptor alone.

### `0003:1532:00B4.000A` — USB interface 01 / event14

Descriptor contains several application collections:

- Keyboard collection with Report ID 1, modifier bits, and 14-byte key array.
- Mouse collection with Report ID 10, six button bits, vendor-defined usage `0x40` (two signed bytes), wheel, and relative X/Y.
- Consumer Control collection with Report ID 2 and a 16-bit usage range 0–572 (maps to event14's `ABS_VOLUME` capability).
- System Control collection with Report ID 3.
- Vendor-defined input reports IDs 4, 5, 8, and 9, each 15 bytes.

The descriptor explains why this interface advertises many capabilities, but the user's event14 test produced no button events. Further investigation may need raw HID reports / USB captures rather than evdev.

### `0003:1532:00B4.000B` — USB interface 02 / event13

Descriptor is a keyboard application collection with modifier bits, a key array, and LED output bits. It has no explicit Report ID item in the displayed descriptor. The observed F13-F24 scan/key events are consistent with this interface exposing the 12 side-grid controls as keyboard usages.

## Sidearm mapping status and cautions

The current Sidearm mapping table in `src-tauri/src/linux_onboard_backup.rs` has been using these provisional onboard identifiers:

- `0x40`–`0x4b`: `thumb_01`–`thumb_12`
- `0x0b`, `0x0c`: `top_aux_01`, `top_aux_02`
- `0x01`, `0x02`, `0x03`: left, right, wheel click
- `0x09`, `0x0a`: wheel up/down
- `0x34`, `0x35`: `mouse_4`, `mouse_5`

These are code assumptions / protocol IDs, **not confirmed physical mappings from the tests above**. The Linux F13-F24 keycodes do not establish that the onboard IDs are 0x40–0x4b, even if the count matches. The top auxiliary buttons and Mouse 4/5 also remain physically unidentified by this test. Do not change the table based only on these evdev outputs.

## Useful next steps

1. Determine the physical order used when pressing the 12 side buttons and document the physical position for each F13-F24 event.
2. If needed, capture raw HID input reports from interface 00/01/02 while pressing one control at a time. evdev on event14 did not reveal the extra controls.
3. Correlate raw HID/protocol data with Sidearm's onboard configuration format and/or a trusted Razer/OpenRazer protocol implementation before declaring onboard IDs.
4. Keep wheel tilt distinct from Mouse 4/5: it is exposed as `REL_HWHEEL` on event12.

Shell context: the user uses **fish**, so diagnostic command examples should use fish loop syntax (`for ...` / `end`), not Bash's `for ...; do ...; done`.
