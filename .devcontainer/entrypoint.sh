#!/bin/bash

# 0. Start D-Bus for PulseAudio
sudo -i bash <<-SHELL
mkdir -p /var/run/dbus
dbus-uuidgen > /var/lib/dbus/machine-id
dbus-daemon --config-file=/usr/share/dbus-1/system.conf --print-address
SHELL