# pipecap

Inspect Apple Silicon display-pipe allocation and cap a monitor's EDID so that
more external displays fit.

## The problem

Apple's spec for a MacBook Pro with an M-series Pro chip says "up to three
external displays", but with a 4K 240 Hz and a 4K 160 Hz monitor attached the
third one never lights up, no matter which refresh rate you pick in System
Settings.

The reason is in the I/O Registry. The SoC has a fixed number of external display
pipes (`dispext0`..`dispext3` on the Pro chips). The display crossbar
(`AppleDisplayConnectionManager`) reserves pipes per monitor from the **highest
mode in its EDID**, not from the mode you selected:

| active pixel rate of the highest EDID mode | pipes reserved |
|---|---|
| up to ~1.27 Gpx/s (4K up to ~153 Hz, 5K up to ~86 Hz, 6K 60 Hz) | 1 |
| above that (4K 160/200/240 Hz, 5K 120 Hz, 8K 60 Hz) | 2 |

4K 240 (2 pipes) + 4K 160 (2 pipes) = 4 pipes. The third monitor sits in
`pending-dfps` forever. Apple's own numbers ("three displays up to 4K 144 Hz"
or "one 4K 240 plus one 4K 200") describe exactly this budget.

## The fix

Hide the modes above the single-pipe limit from the display coprocessor by
installing a *virtual EDID* for that monitor. macOS then reserves one pipe and
the pending monitor gets the freed one. The monitor keeps every other mode
(4K 144 Hz, HDR, VRR); only the top entries are removed.

`/Library/Displays/Contents/Resources/Overrides` is ignored on Apple Silicon, so
pipecap uses the private `IOAVServiceSetVirtualEDIDMode` API, the same one
BetterDisplay uses.

## Install

Download a release archive from the Releases page, or build from source:

```sh
cargo install --git https://github.com/a1678991/pipecap
```

Requires macOS on Apple Silicon. Run it from a normal Terminal: sandboxed
environments cannot open the IOKit user client (you get
`kIOReturnNotPermitted`).

## Usage

```sh
pipecap                    # same as `pipecap status`
pipecap status             # pipe allocation, pending displays, suggestion
pipecap list               # external outputs and their EDIDs
pipecap decode --display "4K160"
pipecap decode some.bin
pipecap dump --display "4K160" -o 4k160.bin

pipecap cap --display "4K160" --dry-run     # show what would be removed
pipecap cap --display "4K160"               # cap to the single-pipe limit and apply
pipecap cap --display "4K160" --max-hz 120  # or pick a rate yourself

pipecap reset --display "4K160"             # remove the virtual EDID
pipecap reset --all
```

`--display` accepts the EDID id (`52746001`), a unique part of the name, or the
index from `pipecap list`. When only one display is connected it can be omitted.
Add `--json` for machine readable output.

Example `status` on a MacBook Pro with four external pipes, after capping the
4K 160 Hz monitor:

```
External display pipes: 4   single-pipe limit: 1.274 Gpx/s active / 1.438 Gpx/s total
Pipe allocation (display crossbar):
  display                  addr     needs   pipes      max active       state
  MONITOR 4K240 (HDMI)     0.3.0    2       [0, 3]     1.991 Gpx/s      active, up to 240 Hz (dispext0)
  MONITOR 4K160 (DP)       0.1.0    1       [1]        1.194 Gpx/s      active, up to 144 Hz (dispext1)
  MONITOR 4K60 (dock)      0.0.1    1       [2]        0.498 Gpx/s      active, up to 60 Hz (dispext2)
Pending (waiting for a pipe): none
```

### Keeping it applied

A virtual EDID does not survive a reboot and may be dropped when the cable is
re-plugged. `pipecap watch` polls the crossbar and re-applies the cap whenever
the monitor is back at its full EDID. `pipecap agent install` wraps that in a
LaunchAgent:

```sh
pipecap agent install --display "4K160"     # writes ~/Library/LaunchAgents/io.github.pipecap.<id>.plist
pipecap agent status  --display 52746001
pipecap agent uninstall --display 52746001
```

Logs go to `~/Library/Logs/pipecap.log`.

## Safety

* The original EDID of every display you cap is saved once to
  `~/Library/Application Support/pipecap/<id>-<name>-original.bin` and never
  overwritten.
* `cap` refuses to remove the preferred timing, validates every block checksum
  before applying, and `apply` refuses a file whose manufacturer/product id does
  not match the connected display.
* Everything is reversible with `pipecap reset` or a reboot. Nothing is written
  to the monitor.
* The override uses a private API (the same one BetterDisplay relies on). It
  was verified on macOS 26.6; Apple may change or remove it in any release.

## How capping works

`pipecap cap` parses the base block, CTA-861 and DisplayID extensions and removes
every timing above the limit:

* base-block detailed timings (replaced by a dummy descriptor, except the
  preferred one),
* CTA short video descriptors (VICs) plus the YCbCr 4:2:0 video data block, with
  the 4:2:0 capability map re-indexed,
* CTA detailed timings,
* DisplayID Type I / Type VII timings.

The range-limits descriptor is lowered to match, and every touched block gets a
new checksum. The manufacturer, product code and serial bytes are untouched, so
macOS keeps your display arrangement and settings.

## Development

```sh
cargo test          # pure-Rust EDID tests on the fixtures in tests/fixtures
cargo clippy --all-targets
cargo run -- decode tests/fixtures/mon_4k240_hdmi.bin
```

CI runs fmt, clippy, tests and a release build on macOS for every push; tags
`v*` publish archives with SHA-256 checksums for `aarch64-apple-darwin` and
`x86_64-apple-darwin` (the Intel build compiles but there is no DCP to talk to).

## License

MIT
