# User presence and "only when idle"

The worker tells the studio whether the person is using this computer, and can be asked to take
studio work only while they are away.

## Terms

| Term | Meaning |
| --- | --- |
| idle time | how long since the last keyboard or mouse input, as the operating system reports it |
| user presence | `idle` (idle time of 2 minutes or more, `IDLE_AFTER`) or `active`; unknown when the idle time cannot be read |
| only when idle | the experimental setting **Only utilise when I'm not using this computer** (config `only_when_idle`, off by default) |

## Reading the idle time

The daemon samples the idle time every 5 s (`SAMPLE_INTERVAL`) in [`src/presence.rs`](../../src/presence.rs):

| Platform | Source |
| --- | --- |
| Linux, X11 | the ScreenSaver extension (`x11rb`, pure Rust) |
| Linux, Wayland (or X11 without the extension) | D-Bus: `org.gnome.Mutter.IdleMonitor.GetIdletime` (GNOME), then `org.freedesktop.ScreenSaver.GetSessionIdleTime` (KDE Plasma) |
| Windows | `GetLastInputInfo` (`system-idle-time`) |
| macOS | `HIDIdleTime` of `IOHIDSystem`, read with `ioreg` |

The daemon runs in the operator's graphical session (the tray UI starts it), so it sees the same
display, session bus and input as the person.

- A change of presence is logged (`op="presence"`, info) with the idle time.
- A probe that fails makes the presence unknown and is logged once per distinct error (warn);
  recovery is logged (info).

## What the studio receives

Inside `capabilities` on `hello` and every heartbeat (5 s):

| Field | Value |
| --- | --- |
| `agentVersion` | the worker version |
| `autoStart` | **Start with my machine** (false in a build without the tray UI) |
| `autoUpdate` | **Update automatically** |
| `startMinimised` | **Start the window minimised** |
| `onlyWhenIdle` | **Only utilise when I'm not using this computer** |
| `userPresence` | `idle` or `active`; absent while unknown |

## Only when idle

- The studio offers no work to a worker with `onlyWhenIdle: true` unless its last heartbeat said
  `userPresence: "idle"`, so offers resume within one heartbeat of the person leaving.
- The worker holds back too: an offer that arrives while the setting is on and the person is
  active (or unknown) is turned down with a `paused` reject, which the studio requeues without
  spending an attempt. The rejection is logged with the presence.
- A job already running carries on when the person comes back.
- Local API requests and streaming speech sessions are asked for by someone on this machine or
  network, so the setting does not hold them back.
