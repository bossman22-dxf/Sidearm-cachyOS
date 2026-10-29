# Linux HID permissions for Sidearm

Sidearm must run as the logged-in user, not as root. The Naga's vendor feature-report interface may need a user-session ACL.

If Sidearm reports that it found the Naga but cannot open the vendor HID interface, create this narrow udev rule:

```udev
SUBSYSTEM=="hidraw", ATTRS{idVendor}=="1532", ATTRS{idProduct}=="00b4", TAG+="uaccess"
```

Save it as `/etc/udev/rules.d/99-sidearm-naga.rules`, then reload rules and replug the receiver:

```fish
sudo udevadm control --reload-rules
sudo udevadm trigger --subsystem-match=hidraw
```

The rule grants access only to HID raw nodes belonging to the Naga V2 HyperSpeed receiver, and only through the active local user session. It does not make the node world-writable or grant Sidearm root privileges.

If access is still denied, inspect the matching node's ACL and the device's parent attributes before broadening the rule. Do not use `chmod 666`, add Sidearm to a broad hardware-access group, or run the application with `sudo`.
