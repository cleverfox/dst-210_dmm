# DST-210 DMM internal UART protocol

Reverse-engineered from the DMM's internal UART: the link from the
isolated multimeter side to the display MCU, crossing the isolation
barrier (π122U31 digital isolator).
Reference decoder: `dst210_read.py`.

Status legend: **[confirmed]** - verified against captures,
**[guess]** - hypothesis, needs more captures.

## Physical layer

- UART, 9600 baud, 8N1 [confirmed 9600; framing assumed 8N1]
- Logic level 3.3 V on both sides of the isolator [confirmed]
- Two directions:
  - **multimeter -> display**: readings, header `5A A5`, streamed
    continuously (sections "Packet format" .. "Bytes 10..11").
  - **display -> multimeter**: mode-select commands, header `AA 55`
    (section "Display -> multimeter: mode commands").
- WARNING: PCB test points RX, TX, GND, 3V3 are on the multimeter side of
  the isolation barrier (at probe potential). If you connect a UART there,
  do not measure mains or other hazardous voltages unless the UART link is
  isolated. See README.md, "Safety".

## Packet format (multimeter -> display)

Fixed length, 12 bytes:

| Offset | Size | Content |
|---|---|---|
| 0 | 2 | Header `5A A5` [confirmed] |
| 2 | 5 | LCD segment bitmap: sign, leading "1", 4 digits, decimal points |
| 7 | 3 | Annunciator / flag bits (partially decoded) |
| 10 | 2 | AC frequency in Hz, big-endian (V AC); other meaning elsewhere [confirmed] |

There is no checksum.

Example: `5A A5 86 FF 0B 4A 0E 00 02 00 01 1B` = `3.014 V DC`

The display is 4.5 digits: an optional minus sign, a half digit that can
only show "1" or blank, then 4 full 7-segment digits (D1..D4).

## Display bytes (offset 2..6)

The data is the raw LCD segment map, not a binary number.

Name the 5 display bytes `d[0]..d[4]` (`d[0]` = packet byte 2).
Full digit `Dk` (k = 1..4) is made of the **high nibble of `d[k-1]`** and
the **low nibble of `d[k]`**:

```
packet byte:   2      3      4      5      6
             d[0]   d[1]   d[2]   d[3]   d[4]
nibble:     hi lo  hi lo  hi lo  hi lo  hi lo
            |  |    \/     \/     \/    |
            |  |    D1     D2     D3    D4 lo
            |  +--- lo: leading "1" + flags
            +------ hi: D1 hi
```

i.e. D1 = d[0].hi + d[1].lo, D2 = d[1].hi + d[2].lo,
D3 = d[2].hi + d[3].lo, D4 = d[3].hi + d[4].lo.

### Segment bits within a digit [confirmed]

| Nibble | bit3 | bit2 | bit1 | bit0 |
|---|---|---|---|---|
| high (`d[k-1] >> 4`) | a | f | e | DP before this digit (see below) |
| low (`d[k] & 0x0F`) | b | g | c | d |

Standard segment naming:

```
  aaa
 f   b
  ggg
 e   c
  ddd
```

The a/d order is confirmed by a "7" (a lit, d not lit) in `-0.7051`.

### Special bits

| Bit | Meaning | Status |
|---|---|---|
| byte 2 bit4 (D1 hi bit0) | Minus sign (D1 has no DP of its own) | [confirmed] |
| byte 2 bit3 | Leading half digit "1" | [confirmed] |
| byte 8 bit7 | DP right after the leading half digit (`1.xxxx`) | [confirmed] |
| byte 3 bit4 (D2 hi bit0) | DP before D2 (`1x.xxx`) | [confirmed] |
| byte 4 bit4 (D3 hi bit0) | DP before D3 (`1xx.xx`) | [confirmed: 10.13 kHz] |
| byte 5 bit4 (D4 hi bit0) | DP before D4 (`1xxx.x`) | [confirmed: 357.9 uF] |
| byte 6 bit7 | Diode symbol (see flags) | [likely] |
| byte 6 bit5 | "n" prefix (see flags) | [confirmed] |
| byte 6 bit4 | "u" prefix (see flags) | [confirmed] |
| byte 6 bit6 | "k" prefix (see flags) | [confirmed] |

If the half digit is blank and byte 8 bit7 is set, the display shows e.g.
`-.7051`; the meter's LCD reads `-0.7051`. Treat a missing leading digit as 0.

### Glyph table

Digits [confirmed: all 0..9 seen]:

| Glyph | Segments |
|---|---|
| 0 / O | abcdef |
| 1 | bc |
| 2 | abdeg |
| 3 | abcdg |
| 4 | bcfg |
| 5 | acdfg |
| 6 | acdefg |
| 7 | abc |
| 8 | abcdefg |
| 9 | abcdfg |
| blank | (none) |

Letters: `L` = def [confirmed, in "OL"]. Others are standard 7-segment
shapes, not yet seen: E adefg, F aefg, H bcefg, P abefg, A abcefg, C adef,
U bcdef, b cdefg, d bcdeg, n ceg, o cdeg, r eg, t defg, u cde, - g.

### Status words

Some modes show text instead of a number. On 7-segment "i" has the same
pattern as "1" and "v" the same as "u", so decode by matching the whole
word:

| Decoded | Shown | Meaning |
|---|---|---|
| `L1uE` | LIvE | Live-wire mode: live wire detected |
| ` 0L ` (no DP, no flags) | OL | Live-wire mode: nothing detected (idle) |

Live-wire mode sets no unit/mode flags at all (no AUTO, no units). That
distinguishes its idle `OL` from resistance overload (` .0L ` with DP and
Ohm + M + AUTO flags).

### Overload

Ohm mode open circuit: display ` .OL ` (D1 blank, DP before D2, D2 = O,
D3 = L, D4 blank). Any display matching `OL` after removing DPs and blanks
is treated as overload.

## Flag bits (bytes 2, 7..10)

### Identified annunciators

| Bit | Name | Status |
|---|---|---|
| byte 2 bit1 | DC | [confirmed: on in V DC and mA DC, off elsewhere] |
| byte 2 bit0 | AC | [confirmed: on in V AC and mA AC only] |
| byte 8 bit1 | V | [confirmed: on in V DC, V AC, diode; off in Ohm] |
| byte 6 bit7 | Diode symbol | [likely: set only in diode mode, 1 capture] |
| byte 7 bit4 | F (farad) | [confirmed: set in uF and nF] |
| byte 6 bit4 | u (micro) prefix | [confirmed: 357.9 uF] |
| byte 6 bit5 | n (nano) prefix | [confirmed: 9.890 nF] |
| byte 8 bit0 | A (ampere) | [confirmed: set in mA and 10 A ranges] |
| byte 7 bit5 | Ohm | [confirmed: set in all resistance captures only] |
| byte 6 bit6 | k (kilo) prefix | [confirmed: kHz and kOhm] |
| byte 8 bit2 | Hz | [confirmed: kHz yes, kOhm no] |
| byte 7 bit2 | M (mega) prefix | [confirmed: 0.2354 MOhm, Ohm OL] |
| byte 7 bit3 | Auto mode (meter auto-detects the function) | [confirmed: same 3.23 Ohm reading, set in auto mode, clear in resistance mode] |
| byte 7 bit0 | m (milli) prefix | [confirmed: set in mA, clear in 10 A] |
| byte 8 bit5 | Temperature (value always in deg C) | [likely: set only in temperature mode, 1 capture] |

### Display-side features (not on the UART)

The UART is the link from the measurement chip to the display MCU. These
features are implemented entirely in the display MCU; using them changes
the LCD but not the UART stream:

| Feature | Effect on UART | Status |
|---|---|---|
| deg F | none, value stays in deg C | [confirmed] |
| HOLD | none, live readings keep coming | [confirmed] |
| REL | none, absolute readings keep coming | [confirmed] |
| MAX/MIN | none, live readings keep coming | [confirmed] |
| Low battery | battery is on the display side, not expected | [assumed] |

### Values seen so far

| Mode | byte2 lo bits 2..0 | B7 | B8 (bit7 masked) | B9 | B10 | B11 |
|---|---|---|---|---|---|---|
| V DC | `110` | `00` | `02` | `00` | `01` | `19`..`1B` |
| V AC | `101` | `00` | `02` | `00` | `00` | `00` |
| Ohm (OL) | `100` | `24` | `00` | `00` | `01` | `18` |
| Ohm (3.32 Ohm, auto mode) | `100` | `28` | `00` | `00` | `01` | `26` |
| Ohm (3.23 Ohm, auto mode) | `100` | `28` | `00` | `00` | `01` | `27` |
| Ohm (3.23 Ohm, resistance mode) | `100` | `20` | `00` | `00` | `01` | `27` |
| Ohm (0.7042 kOhm) | `100` | `20` | `00` | `00` | `01` | `27` |
| Ohm (0.2354 MOhm) | `100` | `24` | `00` | `00` | `01` | `29` |
| Diode | `000` | `00` | `02` | `00` | `01` | `1D` |
| Capacitance uF | `100` | `10` | `00` | `00` | `01` | `1F` |
| Capacitance nF | `100` | `10` | `00` | `00` | `01` | `1F` |
| Frequency kHz | `100` | `00` | `04` | `00` | `01` | `1C` |
| Temperature C | `000` | `00` | `20` | `00` | `01` | `1D` |
| Current mA DC | `110` | `01` | `01` | `00` | `01` | `20` |
| Current mA AC | `101` | `01` | `01` | `00` | `00` | `00` |
| Current A DC (10 A) | `110` | `00` | `01` | `00` | `01` | `23`, `24` |

(Byte 6 high nibble: diode `8`, uF `1`, nF `2`, k (kHz, kOhm) `4`,
others `0`.)

### Unidentified

| Bit | V DC | V AC | Ohm | Diode | Cap | kHz | Temp | mA DC | mA AC | Guess |
|---|---|---|---|---|---|---|---|---|---|---|
| byte 2 bit2 | 1 | 1 | 1 | 0 | 1 | 1 | 0 | 1 | 1 | auto-range (off only in fixed-range diode/temp; set in both auto mode and resistance mode) |

All other bits of bytes 7..9 have always been 0. Byte 10 is the high
byte of the frequency (see "Bytes 10..11").

The display firmware (V0.1.0) never reads byte 6 bit7, byte 7 bits 7, 6, 1,
byte 8 bits 6..0 (including the unit bits) or byte 9: it derives the unit
from the selected mode plus the prefix bits. So these bits cannot be
identified from the firmware; they are probably LCD annunciators of the
measurement chip that the display side ignores.

## Bytes 10..11: AC frequency

In AC modes, bytes 10..11 are the signal frequency in Hz as a 16-bit
big-endian number: `(B10 << 8) | B11`.

- [confirmed] Mains: `5A A5 A5 0D 8A FA 07 00 02 00 00 31` = 217.6 V AC,
  `00 31` = 49 Hz.
- Display firmware: the readings decoder stores `(B10 << 8) | B11` only
  while AC is selected, and the UI prints it with `"%d Hz"` as a secondary
  readout in V AC (and in auto mode when it detects AC voltage). It is not
  shown in AC current.
- The two earlier AC captures (0.2916 V AC, 0.183 mA AC) had `00 00`:
  0 Hz, no countable signal.

In all other modes the display firmware ignores bytes 10..11. There they
hold a slowly drifting value around `0x0118`..`0x012A` (280..298); its
meaning is unknown. The notes below are about that non-AC value
(byte 11 alone; byte 10 was always `01`).

### Byte 11 outside AC [unknown]

Not a checksum: no sum/xor variant matches, and it is identical (`19`) for
two packets with completely different display bytes. Not a plain range
code either: two V DC readings on the same 2 V range gave `19` and `1A`.

| B11 | Binary | Seen in |
|---|---|---|
| `1B` | 00011011 | 3.014 V DC (20 V range) |
| `19` | 00011001 | 1.3094 V DC, -0.7051 V DC (2 V range) |
| `1A` | 00011010 | -0.0004 V DC (2 V range) |
| `18` | 00011000 | Ohm, OL |
| `1C` | 00011100 | 10.13 kHz frequency |
| `1D` | 00011101 | 0.228 V diode, 28 deg C temperature |
| `1F` | 00011111 | 357.9 uF, 9.890 nF, 0.031 nF capacitance |
| `20` | 00100000 | 176.25 mA DC |
| `23` | 00100011 | 0.1957 A DC (10 A range) |
| `24` | 00100100 | 2.210 A DC (10 A range) |
| `26` | 00100110 | 3.32 Ohm |
| `27` | 00100111 | 3.23 Ohm (both auto mode and resistance mode), 0.7042 kOhm |
| `29` | 00101001 | 0.2354 MOhm, 1.0115 MOhm |
| `2A` | 00101010 | LIvE (live-wire mode) |

Observations:
- In AC modes bytes 10..11 are the frequency instead (see above).
- `0x18` set in V DC, Ohm OL, diode, capacitance, frequency, temperature;
  clear in AC, current DC and 3.32 Ohm (which have `0x20` instead). Upper
  bits change within one mode too (Ohm: `18` vs `26`), so byte 11 looks
  like a value (6 bits?), not flags.
- Same 3.23 Ohm measurement in auto mode and resistance mode gives the
  same `27`, but so does 0.7042 kOhm. Byte 11 does not simply follow the
  reading either. Across the session it mostly grows (`18`..`1F` early,
  `20`..`2A` later, AC excluded). NOT a timer: in live-wire mode it went
  `29` -> `27` with no display change. NOT battery: the battery is on the
  display side. Most likely a slow, noisy analog value (measurement chip
  internal temperature or reference?).
- bits 2..0: values seen 0..5, 7. Constant `7` across three capacitance
  readings (uF and nF ranges) but varied within V DC (3, 1, 1, 2) and
  within the 10 A range (3, 4). Not a unique function code: diode and
  temperature both give `5`.

## Display -> multimeter: mode commands

The display MCU tells the multimeter side which function to measure.

### Packet format

Fixed length, 10 bytes:

| Offset | Size | Content |
|---|---|---|
| 0 | 2 | Header `AA 55` [confirmed] |
| 2 | 1 | `05` in all captures: command type or length? [guess] |
| 3 | 1 | Mode code (see below) [confirmed] |
| 4 | 5 | `00` in all captures |
| 9 | 1 | Checksum: sum of bytes 2..8, mod 256 [confirmed, all 12 captures] |

Example: `AA 55 05 0C 00 00 00 00 00 11` = select DC V
(`05 + 0C = 11`).

Unlike the readings stream, this direction has a checksum.

### Behaviour [confirmed]

- A command is sent **once**, on mode change only. It is not repeated.
- The multimeter side sends **no reply or acknowledge**. The only feedback
  is the readings stream switching to the new mode.

Consequence for a passive bridge: the current mode is known only if the
bridge saw the last mode change. After a bridge restart, infer the mode
from the reading flags instead.

### Mode codes

| Code | Mode |
|---|---|
| `0A` | Capacitance |
| `0B` | Resistance (Ohm) |
| `0C` | DC V |
| `0D` | AC V |
| `0E` | Diode |
| `0F` | Frequency |
| `10` | DC A (10 A input) |
| `11` | DC mA |
| `12` | Temperature |
| `13` | Live-wire detection |
| `14` | Auto mode (function auto-detect) |
| `15` | AC A (10 A input) |
| `16` | AC mA |
| `17` | Continuity |

Codes are contiguous from `0A` to `17`. AC and DC current have separate
codes, while voltage has separate DC V / AC V codes too. The meter
officially has no hFE or NCV modes.

### Open questions (commands)

- Meaning of byte 2 (`05`) and bytes 4..8 (always `00`): manual range?
  (AC/DC current is not it: separate mode codes. REL is display-side.)
- Anything below `0A` or above `17`?

### Raw command captures

```
AA 55 05 14 00 00 00 00 00 19   auto mode
AA 55 05 13 00 00 00 00 00 18   live-wire mode
AA 55 05 0C 00 00 00 00 00 11   DC V
AA 55 05 0D 00 00 00 00 00 12   AC V
AA 55 05 0B 00 00 00 00 00 10   Ohm
AA 55 05 17 00 00 00 00 00 1C   continuity
AA 55 05 0E 00 00 00 00 00 13   diode
AA 55 05 0A 00 00 00 00 00 0F   capacitance
AA 55 05 0F 00 00 00 00 00 14   frequency
AA 55 05 12 00 00 00 00 00 17   temperature
AA 55 05 11 00 00 00 00 00 16   DC mA
AA 55 05 10 00 00 00 00 00 15   DC A
AA 55 05 16 00 00 00 00 00 1B   AC mA
AA 55 05 15 00 00 00 00 00 1A   AC A
```

## Raw captures

Full packets (multimeter -> display):

```
5A A5 86 FF 0B 4A 0E 00 02 00 01 1B   3.014 V DC
5A A5 8E EF CB 4F 0E 00 82 00 01 19   1.3094 V DC
5A A5 96 EA CB 07 0A 00 82 00 01 19   -0.7051 V DC
5A A5 04 F0 6B 01 00 24 00 00 01 18   OL, Ohm
5A A5 A5 CD 0F EA 07 00 82 00 00 00   0.2916 V AC
5A A5 A5 0D 8A FA 07 00 02 00 00 31   217.6 V AC, mains, 49 Hz
5A A5 F6 EB EB 4B 0E 00 82 00 01 1A   -0.0004 V DC
5A A5 E0 BB AD ED 8F 00 02 00 01 1D   0.228 V, diode mode
5A A5 84 CF 87 DA 1F 10 00 00 01 1F   357.9 uF, capacitance
5A A5 C4 FF CF EF 2B 10 00 00 01 1F   9.890 nF, capacitance
5A A5 E4 FB 8B 0F 2A 10 00 00 01 1F   0.031 nF (31 pF), capacitance
5A A5 04 EA 1B 8A 4F 00 04 00 01 1C   10.13 kHz, frequency
5A A5 00 00 A0 ED 0F 00 20 00 01 1D   28 deg C, temperature
5A A5 8E EA B7 CD 07 01 01 00 01 20   176.25 mA DC
5A A5 E5 1B EA 8F 0F 01 01 00 00 00   0.183 mA AC
5A A5 06 CA CF 87 0A 00 81 00 01 23   0.1957 A DC (195.7 mA in 10 A mode)
5A A5 A6 BD 0D EA 0B 00 01 00 01 24   2.210 A DC (10 A mode)
5A A5 04 80 9F AF 0D 28 00 00 01 26   3.32 Ohm, auto mode
5A A5 04 80 BF 8D 0F 28 00 00 01 27   3.23 Ohm, auto mode
5A A5 04 80 BF 8D 0F 20 00 00 01 27   3.23 Ohm, resistance mode
5A A5 84 EA 4B AE 4D 20 80 00 01 27   0.7042 kOhm
5A A5 A4 8D CF 47 0E 24 80 00 01 29   0.2354 MOhm
5A A5 EC 0B 0A CA 07 24 80 00 01 29   1.0115 MOhm (real 1 MOhm resistor)
5A A5 60 01 2A E3 05 00 00 00 01 2A   "LIvE", live-wire mode, live detected
5A A5 00 E0 6B 01 00 00 00 00 01 29   " OL ", live-wire mode, idle
5A A5 00 E0 6B 01 00 00 00 00 01 27   " OL ", live-wire mode, idle (B11 drifted)
5A A5 60 01 2A E3 05 00 00 00 01 27   "LIvE", live-wire mode, live detected
```

## Open questions / captures wanted

- Manual range (RANGE button), if the meter has it: confirm byte 2 bit2
  is AUTO.
- Byte 10 bit0 / byte 11: log a stable DC input for a while, see if B11
  changes; sweep a DC input slowly to see if B11 tracks the value.
- uA range: "u" for current (same bit as uF?). mV: does "m" use byte 7
  bit0 for voltage too?
- Which earlier captures were taken in auto mode? Only Ohm captures have
  shown byte 7 bit3 so far.
- Capacitance in mF range (if the meter has it): "m" prefix.
- Frequency in MHz range if available (same "M" bit as MOhm?).
- Continuity readings (mode command `17` known, reading packets not yet
  captured).

## Notes for the SCPI bridge

- Sync on `5A A5`, take 12 bytes; no checksum available, so validate by
  decoding: every digit must map to a known glyph.
- Value = decoded display string as a float (blank half digit -> 0);
  `OL` -> overload (SCPI convention: `9.9E37`).
- Live-wire mode (no flags at all): report as a status, not a value
  (`LIvE` = live detected, `OL` = nothing).
- Unit and scale come from the prefix/unit flag bits; the display value
  alone does not carry k/M/m prefixes. Byte 11 is not needed.
- Temperature is always deg C on the UART, regardless of the meter's
  C/F display setting. Convert in the bridge if deg F is wanted.
- HOLD / REL / MAX/MIN on the meter do not affect the UART: the bridge
  always sees live absolute readings. Implement these in the bridge if
  needed (e.g. SCPI CALC:NULL, CALC:AVER).
- Mode commands (`AA 55`) could in principle let the bridge select the
  function (SCPI CONF:VOLT:DC etc.), but that means injecting packets into
  the display MCU -> multimeter line. Out of scope for a read-only bridge.
  A passive bridge can still listen to this line to learn the selected
  mode, e.g. to tell live-wire `OL` from resistance `OL`. Commands are sent
  only once per mode change, so the reading flags remain the primary mode
  source and the command line is only a hint.
