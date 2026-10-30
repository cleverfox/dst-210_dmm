#!/usr/bin/env python3
"""Read DST-210 DMM internal UART stream and decode the LCD segment data.

Packet (12 bytes):
  [0..1]  5A A5        header
  [2..6]  display      7-segment bitmap (see decode_display)
  [6..8]  flags        annunciators (partly identified)
  [9]     ??           always 0 so far
  [10..11] frequency   AC frequency in Hz, big-endian (secondary display
                       in V AC; something else, ignored by the display
                       firmware, in other modes)

Usage: dst210_read.py /dev/tty.usbserial-XXXX [baud]
Requires: pip install pyserial
"""
import sys

import serial

HEADER = b"\x5a\xa5"
PKT_LEN = 12

SEG = {
    "0": "abcdef", "1": "bc", "2": "abdeg", "3": "abcdg", "4": "bcfg",
    "5": "acdfg", "6": "acdefg", "7": "abc", "8": "abcdefg", "9": "abcdfg",
    " ": "",
    # letters (O shares the 0 pattern)
    "L": "def", "E": "adefg", "F": "aefg", "H": "bcefg", "P": "abefg",
    "A": "abcefg", "C": "adef", "U": "bcdef", "b": "cdefg", "d": "bcdeg",
    "n": "ceg", "o": "cdeg", "r": "eg", "t": "defg", "u": "cde", "-": "g",
}
LUT = {frozenset(v): k for k, v in SEG.items()}

# Non-numeric displays, as decoded (DPs and blanks removed) -> meaning.
# 7-segment "i" decodes as "1", "v" as "u".
WORDS = {
    "0L": "OL",
    "-0L": "OL",
    "L1uE": "LIVE",  # live-wire mode, live detected (idle shows OL, no flags)
}

# Identified annunciators: (byte_index, bit, name)
MODES = [
    (2, 1, "DC"),
    (2, 0, "AC"),
    (2, 2, "AUTO?"),  # auto-range? off only in fixed-range diode/temp so far
    (7, 3, "AUTOMODE"),  # meter's auto (function auto-detect) mode
    (6, 7, "DIODE"),  # seen once, only in diode mode
]
PREFIXES = [
    (7, 2, "M"),
    (6, 6, "k"),
    (7, 0, "m"),
    (6, 4, "u"),
    (6, 5, "n"),
]
UNITS = [
    (8, 1, "V"),
    (8, 0, "A"),
    (7, 4, "F"),
    (7, 5, "Ohm"),
    (8, 2, "Hz"),
    (8, 5, "degC"),  # temperature mode; always C, F is display-side only
]
FLAGS = MODES + PREFIXES + UNITS

# Bits of the packet whose meaning is known: {byte_index: mask}.
# Everything else is reported as unidentified.
KNOWN = {
    2: 0xF8,  # hi nibble -> D1, bit3 -> leading "1"
    3: 0xFF,
    4: 0xFF,
    5: 0xFF,
    6: 0x0F,  # lo nibble -> D4
    8: 0x80,  # DP after leading "1" (1.xxxx)
    10: 0xFF,  # frequency, high byte
    11: 0xFF,  # frequency, low byte
    0: 0xFF,  # header
    1: 0xFF,
}
for _i, _bit, _ in FLAGS:
    KNOWN[_i] = KNOWN.get(_i, 0) | (1 << _bit)


def _set(pkt, table):
    return [name for i, bit, name in table if pkt[i] & (1 << bit)]


def decode_flags(pkt):
    """Mode annunciators, e.g. ["DC", "AUTO?"]."""
    return _set(pkt, MODES)


def decode_unit(pkt):
    """Prefix + unit, e.g. "mA", "nF", "V"."""
    return "".join(_set(pkt, PREFIXES)) + "".join(_set(pkt, UNITS))


def decode_secondary(pkt):
    """Secondary display: frequency in V AC (as the display firmware shows
    it), e.g. "50 Hz", or "" in other modes."""
    if pkt[2] & 0x01 and pkt[8] & 0x02:  # AC and V
        return f"{(pkt[10] << 8) | pkt[11]} Hz"
    return ""


def decode_display(pkt):
    """Display bytes are pkt[2..6]. Digit k uses hi nibble of d[k-1] and lo
    nibble of d[k].

    hi nibble: bit3=a bit2=f bit1=e bit0=DP (before this digit)
    lo nibble: bit3=b bit2=g bit1=c bit0=d
    For D1 the hi bit0 is the minus sign instead of DP.
    DP right after the leading "1" lives in pkt[8] bit7.
    """
    d = pkt[2:7]
    s = "-" if d[0] & 0x10 else ""
    s += "1" if d[0] & 0x08 else ""
    if pkt[8] & 0x80:
        s += "."
    for k in range(1, 5):
        hi, lo = d[k - 1] >> 4, d[k] & 0x0F
        segs = {n for n, m in (("a", hi & 8), ("f", hi & 4), ("e", hi & 2),
                               ("b", lo & 8), ("g", lo & 4), ("c", lo & 2),
                               ("d", lo & 1)) if m}
        if k > 1 and hi & 1:
            s += "."
        s += LUT.get(frozenset(segs), "[" + "".join(sorted(segs)) + "]")
    return s


def display_value(s):
    """Numeric value of a decoded display string, a status word, or None."""
    word = s.replace(".", "").replace(" ", "")
    if word in WORDS:
        return WORDS[word]
    try:
        return float(s.replace(" ", "0"))
    except ValueError:
        return None


def unknown_bits(pkt):
    """List of (byte_index, bit) set in the unidentified area."""
    out = []
    for i, b in enumerate(pkt):
        mask = ~KNOWN.get(i, 0) & 0xFF
        for bit in range(7, -1, -1):
            if mask & (1 << bit) and b & (1 << bit):
                out.append((i, bit))
    return out


def fmt_unknown(pkt):
    parts = []
    for i, b in enumerate(pkt):
        mask = ~KNOWN.get(i, 0) & 0xFF
        if mask:
            bits = "".join(
                ("1" if b & (1 << n) else "0") if mask & (1 << n) else "-"
                for n in range(7, -1, -1)
            )
            parts.append(f"B{i}:{bits}")
    return " ".join(parts)


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
    prev = None
    for pkt in read_packets(port):
        raw = " ".join(f"{b:02X}" for b in pkt)
        disp = decode_display(pkt)
        val = display_value(disp)
        val = "?" if val is None else val if isinstance(val, str) else f"{val:g}"
        flags = " ".join(decode_flags(pkt))
        unit = decode_unit(pkt)
        line = (f"{raw} | {disp:>8} | {val:>8} {unit:<4} | "
                f"{decode_secondary(pkt):>8} | {flags:<12} | "
                f"{fmt_unknown(pkt)}")
        if prev is not None:
            changed = [
                f"B{i}.{bit}" for i, bit in
                set(unknown_bits(pkt)) ^ set(unknown_bits(prev))
            ]
            if changed:
                line += " | changed: " + ",".join(sorted(changed))
        print(line, flush=True)
        prev = pkt


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        pass
