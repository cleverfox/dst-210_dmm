# DST-210 bridge firmware (ESP32-C6)

Network bridge for the FNIRSI DST-210 multimeter: reads the internal
readings UART and injects mode commands, exposing the meter over SCPI
(TCP 5555) and a web UI / JSON API (HTTP 80). Derived from the `vmeter`
project (same WiFi, BLE and server code), without the display.

Protocol: [../dst-210-proto.md](../dst-210-proto.md).

## Wiring

Tap the display side of the π122U31 isolator (3.3 V logic):

| ESP32-C6 | Meter | Direction |
|---|---|---|
| GPIO4 (UART1 RX) | DMM-READINGS | multimeter -> bridge |
| GPIO5 (UART1 TX) | COMMANDS, via wired-AND | bridge -> multimeter |
| GND | GND | |

The COMMANDS line is driven by the display MCU too. UART idles high, so
combine both transmitters with a wired-AND, not a diode OR:

```
display MCU TX ---|<|---+
                        +----- isolator COMMANDS input
ESP32 GPIO5    ---|<|---+
                        |
                        +--[10k]-- 3.3 V
```

Schottky diodes, cathodes towards the transmitters, anodes joined on the
isolator input: a common-anode pair such as BAT54A fits. A
2-input AND gate (74LVC1G08) works as well. For read-only use leave GPIO5
unconnected.

Notes:
- Without any command the multimeter side starts in auto mode. The display
  MCU sends a mode command when its DMM screen is entered.
- After a bridge command the meter's own LCD keeps its selected function
  and units; the bridge always reports what the meter really measures (from
  the reading flags).

## Build and flash

```
cargo run --release          # espflash flash --monitor
```

WiFi: the device starts its own AP `dst210-bridge` (http://192.168.4.1) and
connects to the network set in the web UI, over BLE (`DST210-Bridge`, same
GATT service as vmeter), or the compile-time default from
`.cargo/config.toml` (`WIFI_SSID` / `WIFI_PASSWORD`).

The password must be empty (open network), 8..63 characters or a 64-digit
hex key; anything else is refused when saving. If the stored password is
still invalid at boot, only the AP is started.

BLE advertises only while the station is not connected: after 30 s without
a WiFi connection (also right after boot), and it stops when WiFi connects.

## SCPI (TCP port 5555, USB serial)

Values are in base units (V, A, Ohm, F, Hz, deg C), exact decimals from the
meter's display (`7.042E+02`). Overload `OL` is `9.9E+37`.

The USB port (the C6's USB Serial/JTAG, `/dev/cu.usbmodem*`) carries the log
after boot. A line starting with `*IDN?` or `*OPC?` switches it to SCPI:
logging stops and the port answers commands until the next reset. Other
input before that is ignored, so typing in a serial monitor does not switch
it. Flashing over the same port works as before.

| Command | Description |
|---|---|
| `*IDN?` | `Agilent Technologies,34405A,DST210-BRIDGE,<version>` (for sigrok) |
| `*RST`, `*CLS`, `*OPC?`, `SYST:ERR?` | `*RST` only clears errors, the meter is not touched |
| `CONF:<func> [range]` | switch function, no wait |
| `MEAS:<func>? [range]` | switch function, wait until the meter reports it, return a reading |
| `CONF?` | function, range, resolution the meter reports, e.g. `"VOLT 2E+03,1E-01"`, `"DIOD"` |
| `READ?` / `MEAS?` | next reading in the current function |
| `FETC?` | latest reading, no wait |
| `INIT`, `ABOR`, `MEAS` | accepted, no effect |
| `DATA2?` | AC frequency (V AC secondary display), Hz |
| `DMM:DET?` | `value,"unit","function","display",freq` |
| `DMM:RAW?` | latest raw packet, hex |
| `DMM:MODE <name>` / `DMM:MODE?` | raw mode command by name (`dcv`, `acv`, `r`, ...) / last sent |

`<func>`: `VOLT[:DC]`, `VOLT:AC`, `RES`, `CONT`, `DIOD`, `CAP`, `FREQ`,
`TEMP`, `CURR[:DC]`, `CURR:AC`, plus `LIVE` (live-wire detection: 1 = live,
0 = nothing) and `AUTO` (meter auto mode). For current, range > 0.6 or
`MAX` selects the 10 A input, otherwise the mA input.

```
$ nc 192.168.4.1 5555
MEAS:VOLT:AC?
2.176E+02
DATA2?
49
```

## sigrok

libsigrok's `scpi-dmm` driver only talks to models it knows, so the bridge
identifies as an Agilent 34405A and answers `CONF?` the same way. Tested
against sigrok-cli 0.7.2 / libsigrok 0.5.2 with a simulated server:

```
$ sigrok-cli -d scpi-dmm:conn=tcp-raw/192.168.4.1/5555 --continuous
P1: 704.2 V DC
$ sigrok-cli -d scpi-dmm:conn=/dev/cu.usbmodem101 --continuous
```

sigrok only reads; switch the function on the meter, in the web UI or with
`CONF:<func>` over another connection. Overload shows as `inf`. LIVE mode
has no sigrok equivalent; sigrok fails to start in it.

## HTTP API

- `GET /api/measure` latest reading as JSON
- `GET /api/status` network and link status (packets, commands sent)
- `POST /api/mode` `mode=dcv|acv|r|cont|diode|cap|freq|temp|dcma|dca|acma|aca|live|auto`
- `POST /api/wifi` `ssid=..&password=..`, JSON result

There is no login, so requests from other web pages are refused:

- every POST needs an `X-Requested-With` header (any value). A foreign page
  cannot add it without a CORS preflight, which the device never grants.
- the `Host` header, if present, must be an IP address, which blocks DNS
  rebinding. Open the UI as `http://<ip>/`, not by a host name.

```
curl -H 'X-Requested-With: curl' -d mode=acv http://192.168.4.1/api/mode
```
