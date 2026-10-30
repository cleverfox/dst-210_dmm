#!/usr/bin/env python3
"""Read DST-210 display -> multimeter UART stream and decode mode commands.

Packet (10 bytes):
  [0..1]  AA 55        header
  [2]     05           command type or length? (always 05 so far)
  [3]     mode code    (see MODES)
  [4..8]  00 ...       always 0 so far
  [9]     checksum     sum of bytes 2..8, mod 256

Usage: dst210_read10.py /dev/tty.usbserial-XXXX [baud]
Requires: pip install pyserial
"""
import sys

import serial

HEADER = b"\xaa\x55"
PKT_LEN = 10

MODES = {
    0x0A: "Capacitance",
    0x0B: "Resistance",
    0x0C: "DC V",
    0x0D: "AC V",
    0x0E: "Diode",
    0x0F: "Frequency",
    0x10: "DC A (10 A)",
    0x11: "DC mA",
    0x12: "Temperature",
    0x13: "Live-wire",
    0x14: "Auto mode",
    0x15: "AC A (10 A)",
    0x16: "AC mA",
    0x17: "Continuity",
}


def checksum_ok(pkt):
    return sum(pkt[2:9]) & 0xFF == pkt[9]


def decode(pkt):
    """Human-readable description of a command packet."""
    mode = MODES.get(pkt[3], f"unknown mode {pkt[3]:02X}")
    notes = []
    if not checksum_ok(pkt):
        notes.append(f"BAD CHECKSUM (expected {sum(pkt[2:9]) & 0xFF:02X})")
    if pkt[2] != 0x05:
        notes.append(f"byte2={pkt[2]:02X}")
    if any(pkt[4:9]):
        notes.append("bytes4..8=" + " ".join(f"{b:02X}" for b in pkt[4:9]))
    return mode + ("" if not notes else " | " + ", ".join(notes))


def read_packets(port):
    buf = bytearray()
    while True:
        chunk = port.read(64)
        if not chunk:
            continue
        buf += chunk
        while True:
            i = buf.find(HEADER)
            if i < 0:
                del buf[:-1]
                break
            if len(buf) - i < PKT_LEN:
                del buf[:i]
                break
            yield bytes(buf[i:i + PKT_LEN])
            del buf[:i + PKT_LEN]


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        sys.exit(1)
    baud = int(sys.argv[2]) if len(sys.argv) > 2 else 9600
    port = serial.Serial(sys.argv[1], baud, timeout=0.1)
    for pkt in read_packets(port):
        raw = " ".join(f"{b:02X}" for b in pkt)
        print(f"{raw} | {decode(pkt)}", flush=True)


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        pass
