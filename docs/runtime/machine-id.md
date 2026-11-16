# Machine id

A reinstall of the worker makes a new install, a new registration and a new worker id. The
**machine id** is what stays the same, so the studio can recognise the computer and keep its
contribution together.

Implemented in [`src/machine_id.rs`](../../src/machine_id.rs).

## What it is

- The SHA-256 of `studio-worker/machine-id/v1:` followed by the operating system's own machine
  identifier (trimmed, lower case), as its first 32 hex characters.

  | OS | Identifier |
  | --- | --- |
  | Linux | `/etc/machine-id`, else `/var/lib/dbus/machine-id` |
  | macOS | `IOPlatformUUID` from `ioreg -rd1 -c IOPlatformExpertDevice` |
  | Windows | `HKLM\SOFTWARE\Microsoft\Cryptography\MachineGuid` (64-bit view) |

- The raw identifier never leaves the machine, and the prefix makes the hash useless to any other
  application.
- It stays the same across reinstalls, updates and `register --reset`. Reinstalling the operating
  system changes it.
- It is read once per process and logged (`op="machine_id"`, target `studio_worker::machine_id`):
  info with the id, or a warning when the identifier cannot be read. The worker then sends none, and
  works as before.

## Where it goes

- `machineId` in `capabilities`, on the registration request, `hello` and every heartbeat. Absent
  when unknown.
- The studio labels a pending registration from a machine that already runs a registered worker as
  "Same machine as …". The id never approves anything; it is a label, not a credential.

## Contribution

The studio keeps a ledger of the jobs each worker completed, never pruned, tagged with the machine
id. On `hello` it answers `welcome` with `contribution: { total: { <kind>: <jobs> }, since }`: the
jobs of this worker and of every worker on the same machine.

The daemon's [job tally](daemon-and-tray.md#contribution) takes that figure as the base of its total
and adds the jobs it completes afterwards. **Today** stays the worker's own count, on its own clock.
