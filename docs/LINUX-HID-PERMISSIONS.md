# Linux HID permissions for Sidearm

Sidearm must run as the logged-in user, not as root. The Naga's vendor feature-report interface may need a user-session ACL.

If Sidearm reports that it found the Naga but cannot open the vendor HID interface, create this narrow udev rule:

```udev
SUBSYSTEM=="hidraw", ATTRS{idVendor}=="1532", ATTRS{idProduct}=="00b4", TAG+="uaccess"
SUBSYSTEM=="input", KERNEL=="event*", ATTRS{idVendor}=="1532", ATTRS{idProduct}=="00b4", TAG+="uaccess"
SUBSYSTEM=="misc", KERNEL=="uinput", TAG+="uaccess"
```

Save it as `/etc/udev/rules.d/99-sidearm-naga.rules`, then reload rules and replug the receiver:

```fish
sudo udevadm control --reload-rules
sudo udevadm trigger --subsystem-match=hidraw
sudo udevadm trigger --subsystem-match=input
```

The first two rules grant access only to HID raw and evdev nodes belonging to the Naga V2 HyperSpeed receiver, and only through the active local user session. The evdev rule is needed for the thumb-keyboard collection; Sidearm grabs only that keyboard collection, not the pointer interface. The third rule grants the active session access to the virtual-input device used for Wayland key injection. None of these rules make nodes world-writable or grant Sidearm root privileges.

If access is still denied, inspect the matching node's ACL and the device's parent attributes before broadening the rule. Do not use `chmod 666`, add Sidearm to a broad hardware-access group, or run the application with `sudo`.


The Linux input synthesizer also needs `/dev/uinput`. Load the kernel module if the node is absent:

```fish
sudo modprobe uinput
```

Then reload and trigger the misc-device rule:

```fish
sudo udevadm control --reload-rules
sudo udevadm trigger --subsystem-match=misc
```

The `uinput` rule is session-scoped through `uaccess`; do not use `chmod 666 /dev/uinput` or run Sidearm with `sudo`.
