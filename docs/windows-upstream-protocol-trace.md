# Windows upstream protocol trace: Razer Naga V2 HyperSpeed

This document traces the **Windows implementation in the upstream Sidearm project**. It is the protocol reference for Linux work in this fork; Linux input events and the fork's Linux implementation are not used as the authority for Razer vendor-protocol identifiers.

## Scope and provenance

- Upstream repository: [danscMax/Sidearm](https://github.com/danscMax/Sidearm).
- The fork's README points to that repository for the original project source/build instructions: [fork README, build instructions](https://github.com/bossman22-dxf/Sidearm-cachyOS/blob/feat/open-naga-diff-onboard-writes/README.md#L100-L108).
- The upstream README identifies Windows 10/11 as its platform and gives the same clone URL: [upstream README, requirements/building](https://github.com/danscMax/Sidearm/blob/41e750032de13cf6081d046913f4100390c009d9/README.md#L94-L108).
- Source snapshot traced here: upstream commit [`41e750032de13cf6081d046913f4100390c009d9`](https://github.com/danscMax/Sidearm/commit/41e750032de13cf6081d046913f4100390c009d9). The source links below are pinned to this immutable revision so later upstream edits do not silently change this record.

**Historical fork-point caveat:** this establishes the upstream repository identified by the fork README and traces its Windows code at the pinned snapshot. It does not, by itself, prove that this exact commit was the historical point at which the Linux fork diverged. That fork-point SHA has not been independently established in this investigation.

The implementation evidence is code and tests in the upstream repository. The upstream source comments state that wire details were reverse-engineered from a Synapse USB capture named `sidearm_map2.pcap` and that the code's fixtures contain captured reports. I found the capture filename and embedded fixtures in the source, but have not located the raw `.pcap` itself in the repository. Accordingly, this is a trace of the source's documented/captured findings, not an independent inspection of that raw capture.

## Keep these identifiers separate

There are several unrelated identifier spaces:

1. **Sidearm control IDs** are application names such as `thumb_01`, `top_aux_01`, and `wheel_click`.
2. **Razer vendor button IDs** are values in the onboard-memory protocol, such as `0x40`, `0x0b`, and `0x34`.
3. **Vendor memory slots** are storage locations for the slot-based report path. They are not button IDs and are not sequential.
4. **HID usages** describe the action stored in a record. For example, keyboard usage `0x68` is F13.
5. **HID report IDs** select a report format, such as `0x03` for the vendor feature report or `0x02` for the consumer-signal input report.
6. **Linux input events** such as `KEY_F13` or `REL_HWHEEL` are the Linux input subsystem's representation. They are not Razer button IDs or report IDs.

Do not copy a number from one namespace into another.

## Windows device selection and feature-report transport

The upstream Naga V2 HyperSpeed model entry specifies:

- USB VID:PID `1532:00B4` (receiver).
- Windows HID interface `MI_00`.
- Vendor command transaction byte `0x1f`.

See [`device_catalog.rs`, Naga V2 HyperSpeed entry](https://github.com/danscMax/Sidearm/blob/41e750032de13cf6081d046913f4100390c009d9/src-tauri/src/device_catalog.rs#L115-L172).

The Windows implementation discovers HID paths through the Windows DeviceClasses registry, filters for the model VID/PID and interface number, then opens the vendor collection with `CreateFileW`. The source requests zero access rights and shares read/write because its comment records that Razer's driver holds the collection and that requesting generic read/write access fails. See [`mouse_memory.rs`, device open](https://github.com/danscMax/Sidearm/blob/41e750032de13cf6081d046913f4100390c009d9/src-tauri/src/mouse_memory.rs#L503-L538) and [interface path selection](https://github.com/danscMax/Sidearm/blob/41e750032de13cf6081d046913f4100390c009d9/src-tauri/src/mouse_memory.rs#L1128-L1145).

The vendor feature-report transport is:

- Report ID: `0x03`.
- Payload: 90 bytes.
- Windows HID feature buffer: 91 bytes total, with report ID in byte 0 and the 90-byte payload after it.
- Write: `HidD_SetFeature`.
- Read: `HidD_GetFeature`.

See [report constants and protocol notes](https://github.com/danscMax/Sidearm/blob/41e750032de13cf6081d046913f4100390c009d9/src-tauri/src/mouse_memory.rs#L1-L36) and [feature-report read/write wrappers](https://github.com/danscMax/Sidearm/blob/41e750032de13cf6081d046913f4100390c009d9/src-tauri/src/mouse_memory.rs#L953-L985).

The upstream model's `interface: 0` therefore means Windows interface `MI_00`. It must not be confused with a Linux HID instance suffix such as `.0009` or a Linux event node such as `event12`.

## Vendor command packet layout

The shared command builder constructs a 90-byte payload with these fields:

| Payload offset | Meaning |
|---:|---|
| 0 | Status/command byte; initialized to zero for a request |
| 1 | Transaction ID (`0x1f` for this model) |
| 5 | Argument length |
| 6 | Command class |
| 7 | Command |
| 8 onward | Command arguments |
| 88 | XOR checksum of payload bytes 2 through 87 inclusive |

The checksum deliberately excludes payload bytes 0 and 1. The command function sends the feature report, waits for a reply, and checks that the reply echoes the same class and command before accepting it. It retries busy replies and ignores a successful reply that belongs to a different request, because the Razer driver can share the report with Sidearm.

See [`build_command`, `reply_matches`, and checksum](https://github.com/danscMax/Sidearm/blob/41e750032de13cf6081d046913f4100390c009d9/src-tauri/src/mouse_memory.rs#L164-L167) and [command exchange and retry logic](https://github.com/danscMax/Sidearm/blob/41e750032de13cf6081d046913f4100390c009d9/src-tauri/src/mouse_memory.rs#L925-L985).

The command class/command pairs relevant to button records are:

| Operation | Class | Command | Meaning in upstream code |
|---|---:|---:|---|
| Read button record | `0x02` | `0x8c` | Get one button's record for a profile/button/layer |
| Write button record | `0x02` | `0x0c` | Set one button's record for a profile/button/layer |

The read arguments begin with profile, button ID, and layer. The write arguments begin with profile, button ID, layer, followed by category, value length, and value bytes. In the Naga V2 HyperSpeed code the profile argument is `0x01`.

The source reads the ten record bytes from reply offsets 8 through 17. See [button read path](https://github.com/danscMax/Sidearm/blob/41e750032de13cf6081d046913f4100390c009d9/src-tauri/src/mouse_memory.rs#L744-L752), [button command constants](https://github.com/danscMax/Sidearm/blob/41e750032de13cf6081d046913f4100390c009d9/src-tauri/src/mouse_memory.rs#L76-L108), and [per-button write path](https://github.com/danscMax/Sidearm/blob/41e750032de13cf6081d046913f4100390c009d9/src-tauri/src/mouse_memory.rs#L573-L583).

## Upstream Naga V2 HyperSpeed control-to-button map

The following mappings are present in the upstream model catalogue. They are **vendor button IDs**, not Linux key codes.

| Sidearm control ID | Razer vendor button ID | Upstream classification |
|---|---:|---|
| `thumb_01` | `0x40` | Key-capable button; slot-backed |
| `thumb_02` | `0x41` | Key-capable button; slot-backed |
| `thumb_03` | `0x42` | Key-capable button; slot-backed |
| `thumb_04` | `0x43` | Key-capable button; slot-backed |
| `thumb_05` | `0x44` | Key-capable button; slot-backed |
| `thumb_06` | `0x45` | Key-capable button; slot-backed |
| `thumb_07` | `0x46` | Key-capable button; slot-backed |
| `thumb_08` | `0x47` | Key-capable button; slot-backed |
| `thumb_09` | `0x48` | Key-capable button; slot-backed |
| `thumb_10` | `0x49` | Key-capable button; slot-backed |
| `thumb_11` | `0x4a` | Key-capable button; slot-backed |
| `thumb_12` | `0x4b` | Key-capable button; slot-backed |
| `top_aux_01` | `0x0b` | Key-capable button; no slot, per-button command path |
| `top_aux_02` | `0x0c` | Key-capable button; no slot, per-button command path |
| `mouse_left` | `0x01` | Native mouse-button record, value 1 |
| `mouse_right` | `0x02` | Native mouse-button record, value 2 |
| `wheel_click` | `0x03` | Native mouse-button record, value 3 |
| `wheel_up` | `0x09` | Native wheel record, value `0x09` |
| `wheel_down` | `0x0a` | Native wheel record, value `0x0a` |
| `mouse_4` | `0x34` | Native record, value 4; upstream comment identifies this pair as wheel tilt |
| `mouse_5` | `0x35` | Native record, value 5; upstream comment identifies this pair as wheel tilt |

Source: [`device_catalog.rs`, Naga V2 HyperSpeed key-buttons and native-buttons tables](https://github.com/danscMax/Sidearm/blob/41e750032de13cf6081d046913f4100390c009d9/src-tauri/src/device_catalog.rs#L115-L152).

The upstream source labels `0x34`/`0x35` as wheel tilt (Synapse `ScrollLeft`/`ScrollRight`) and gives their native values as 4 and 5. It does not provide a separate physical-position diagram in this source file. Do not assign a left-versus-right physical direction beyond what a future controlled report test establishes.

Likewise, `top_aux_01` and `top_aux_02` are source-level control names mapped to `0x0b` and `0x0c`; this source table alone does not establish a precise physical position for those labels.

## Thumb button storage slots and recorded key usages

The twelve thumb buttons use a **non-linear** vendor slot allocation:

| Control | Button ID | Slot | HID keyboard usage in upstream captured fixture |
|---|---:|---:|---:|
| `thumb_01` | `0x40` | `0x07` | `0x68` (F13) |
| `thumb_02` | `0x41` | `0x1c` | `0x69` (F14) |
| `thumb_03` | `0x42` | `0x1d` | `0x6a` (F15) |
| `thumb_04` | `0x43` | `0x1e` | `0x6b` (F16) |
| `thumb_05` | `0x44` | `0x00` | `0x6c` (F17) |
| `thumb_06` | `0x45` | `0x01` | `0x6d` (F18) |
| `thumb_07` | `0x46` | `0x02` | `0x6e` (F19) |
| `thumb_08` | `0x47` | `0x03` | `0x6f` (F20) |
| `thumb_09` | `0x48` | `0x08` | `0x70` (F21) |
| `thumb_10` | `0x49` | `0x04` | `0x71` (F22) |
| `thumb_11` | `0x4a` | `0x05` | `0x72` (F23) |
| `thumb_12` | `0x4b` | `0x06` | `0x73` (F24) |

The table is specified in the upstream `NAGA_V2_HYPERSPEED.slots` entry and repeated by the upstream test `thumb_controls_resolve_onto_the_captured_slots_and_usages`. The embedded fixtures are described by the source as slot payloads captured from Synapse `GET_REPORT` replies. There is also a spare duplicate slot `0x1b` for button `0x4b`; the catalogue comment says that slot is never written.

Sources: [model's button and slot tables](https://github.com/danscMax/Sidearm/blob/41e750032de13cf6081d046913f4100390c009d9/src-tauri/src/device_catalog.rs#L123-L152), [captured slot fixtures and checksum tests](https://github.com/danscMax/Sidearm/blob/41e750032de13cf6081d046913f4100390c009d9/src-tauri/src/mouse_memory.rs#L1266-L1305), and [thumb-to-button/slot/usage test](https://github.com/danscMax/Sidearm/blob/41e750032de13cf6081d046913f4100390c009d9/src-tauri/src/mouse_memory.rs#L1318-L1344).

## How the Windows writer uses the two button-record paths

### Slot-backed thumb buttons

For a button with a slot in the model catalogue, `write_button` builds the 90-byte slot payload and writes it with `HidD_SetFeature`. The record marker is `0a 02 0c 01`; the record then contains the button ID, layer, category, length, modifier, and HID usage. The slot writer sets the layer byte to `0x00`. It verifies the write by reading the slot back and comparing the record bytes and checksum; the code allows a retry because the reply can lag.

See [`write_button`](https://github.com/danscMax/Sidearm/blob/41e750032de13cf6081d046913f4100390c009d9/src-tauri/src/mouse_memory.rs#L553-L571) and [slot record builder and comparison](https://github.com/danscMax/Sidearm/blob/41e750032de13cf6081d046913f4100390c009d9/src-tauri/src/mouse_memory.rs#L1065-L1084).

### Buttons without a vendor slot

If the model has no slot for the button, `write_button` falls through to `write_key_command`. This writes class `0x02` / command `0x0c` with arguments `[profile=1, button_id, layer=0, category=0x02, length=2, modifier, usage, ...]`, then reads the button record back to confirm it. The upstream source specifically identifies the two top auxiliary buttons as having no vendor slot.

See [no-slot command path](https://github.com/danscMax/Sidearm/blob/41e750032de13cf6081d046913f4100390c009d9/src-tauri/src/mouse_memory.rs#L555-L583) and [upstream top-button catalogue comments](https://github.com/danscMax/Sidearm/blob/41e750032de13cf6081d046913f4100390c009d9/src-tauri/src/device_catalog.rs#L123-L135).

### A slot read is not a cold read of old memory

The upstream source warns that the slot GET_REPORT path does not independently reveal a slot's pre-existing record: after selecting a slot, it echoes what the last SET_REPORT stored. For actual stored state, the Windows source uses the command-class button query `0x02/0x8c`. Consequently, Sidearm's slot write verification is a write-and-echo check, not a non-destructive read of the old slot contents.

This distinction is important for future backup/restore work. Do not implement a supposed backup by merely selecting every slot and reading it. See [upstream protocol notes](https://github.com/danscMax/Sidearm/blob/41e750032de13cf6081d046913f4100390c009d9/src-tauri/src/mouse_memory.rs#L15-L31), [per-button read implementation](https://github.com/danscMax/Sidearm/blob/41e750032de13cf6081d046913f4100390c009d9/src-tauri/src/mouse_memory.rs#L744-L752), and [upstream model-adding guide](https://github.com/danscMax/Sidearm/blob/41e750032de13cf6081d046913f4100390c009d9/docs/ADDING-A-MODEL.md#L1-L42).

## Separate Windows consumer-signal input path

The Windows source defines an additional signal mode that must not be confused with the vendor feature-report channel:

- The Naga V2 HyperSpeed model defines a consumer input path on interface `MI_01`, top-level collection `Col03`, report ID `0x02`.
- Each key-capable control is assigned a reserved consumer-page usage. The source's comments say Windows ignores these reserved usages; Sidearm's helper reads them directly from the HID collection.
- The reader expects the report ID first, followed by little-endian 16-bit usage values. The consumer report is an **input** signal; it is not the vendor feature report used to write onboard records.
- The source initializes consumer-signal mode to enabled by default. A config-side file named `onboard-key-signals` switches the key buttons back to F13–F24 signals.
- In consumer mode, a button record uses category `0x0a` and carries the consumer usage; Sidearm reads the corresponding input report and translates the usage to the configured key action.

Sources: [model consumer interface/report/usages](https://github.com/danscMax/Sidearm/blob/41e750032de13cf6081d046913f4100390c009d9/src-tauri/src/device_catalog.rs#L153-L172), [consumer mode default and binding selection](https://github.com/danscMax/Sidearm/blob/41e750032de13cf6081d046913f4100390c009d9/src-tauri/src/mouse_memory.rs#L96-L108) and [consumer signal selection](https://github.com/danscMax/Sidearm/blob/41e750032de13cf6081d046913f4100390c009d9/src-tauri/src/mouse_memory.rs#L214-L276), [report parsing](https://github.com/danscMax/Sidearm/blob/41e750032de13cf6081d046913f4100390c009d9/src-tauri/src/capture_backend/consumer.rs#L24-L38), and [Windows consumer collection reader](https://github.com/danscMax/Sidearm/blob/41e750032de13cf6081d046913f4100390c009d9/src-tauri/src/capture_backend/consumer.rs#L194-L248).

This explains why the Windows project has two relevant interfaces:
- `MI_00` is the vendor feature-report channel used for onboard memory and device commands.
- `MI_01&Col03` is the separate consumer signal input collection used by Sidearm's Windows capture helper in consumer-signal mode.

## Comparison with the Linux fork's current table

At the time this note was written, the fork's `src-tauri/src/linux_onboard_backup.rs` contains a 21-entry `BUTTONS` table with the same control-to-vendor-ID pairs listed above: thumbs `0x40`–`0x4b`, top auxiliary IDs `0x0b`/`0x0c`, standard buttons `0x01`–`0x03`, wheel `0x09)/`0x0a`, and `mouse_4)/`mouse_5` as `0x34)/`0x35`. This is a source-table parity check; it does not prove the Linux report transport is equivalent to the Windows one.

Reference: [fork's Linux onboard table](https://github.com/bossman22-dxf/Sidearm-cachyOS/blob/3a7638c44c6a15d8814f9382e00b4107d6c28c64/src-tauri/src/linux_onboard_backup.rs#L664-L676).

## Reconciliation with the user's Linux observations

These points deliberately keep the previously recorded physical observations separate from source-derived conclusions:

- The user's device identity is VID:PID `1532:00B4`, matching the upstream Naga V2 HyperSpeed model entry.
- The user's sysfs/udev observations identify the main mouse event node on USB interface `00`, matching the upstream Windows model's `MI_00` vendor interface selection at the interface-number level.
- The user's event13 capture reports the 12 side-grid controls as Linux `KEY_F13`–`KEY_F24`, with HID keyboard usages `0x68`–`0x73`. Those values align with the F13–F24 usages in the upstream captured slot fixtures, but the Linux key codes, HID usages, and Razer button IDs remain separate namespaces. The exact physical grid position for each entry still depends on the user's documented press order.
- The user's event12 wheel tilt capture reports `REL_HWHEEL` movement rather than ordinary mouse-button clicks. The upstream Windows catalogue independently labels vendor IDs `0x34)/`0x35` as wheel tilt controls. The Linux event capture does not itself reveal those vendor IDs or establish the left/right-to-ID direction.
- The user's event14 test produced no evtest events. The Windows source's separate input path is specifically `MI_01&Col03`, report ID `0x02`; a lack of evtest events from that node is not enough to prove that the HID collection cannot carry Windows' raw consumer reports. A Linux raw-HID capture would be needed to verify the equivalent path.

The user's original observations remain recorded in [`linux-naga-v2-hyperspeed-hardware-notes.md`](linux-naga-v2-hyperspeed-hardware-notes.md).

## What this trace does not establish

- It does not map every named control to an exact physical position on the mouse body.
- It does not independently replay or inspect the raw `sidearm_map2.pcap` capture.
- It does not prove the user's receiver firmware, firmware state, or active onboard profile is identical to the upstream author's test device.
- It does not prove that Windows' `HidD_SetFeature`/`HidD_GetFeature` calls can be translated one-for-one to Linux hidraw/ioctl operations.
- It does not justify sending experimental writes to the mouse. Any Linux write path must be built from the upstream packet layouts and verified safely, with saved original records and an explicit restore plan.

## Suggested investigation order for future work

1. Keep this Windows trace as the protocol reference and preserve its immutable upstream links.
2. Inspect the fork's Linux writer against the slot and command paths separately; do not treat them as one generic feature report.
3. Establish the Linux raw-HID report descriptor and report IDs for the exact `MI_00` vendor collection and `MI_01&Col03` consumer collection.
4. Compare read-only Linux reports with the upstream layouts before attempting any write.
5. Only after report transport and read-back behaviour are established, design a controlled write/restore test. Record the exact physical control pressed whenever translating vendor IDs into a labelled hardware diagram.
