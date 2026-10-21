# Memory ceiling

On Linux the worker runs inside a systemd user scope with a memory limit, so a job that grows
(an `sd-cli` that offloads a 14 GiB model to the CPU holds 21 to 27 GB) can never take the
machine down with it: the kernel reclaims and, at worst, kills inside the scope, not the desktop.

## Terms

| Term | Meaning |
| --- | --- |
| memory ceiling | the most memory the worker's processes may hold together: the config's `memory_max_gb` |
| scope | the transient systemd user unit `studio-worker.scope` the tray UI starts in |
| `MemoryMax` | the scope's hard limit: the ceiling |
| `MemoryHigh` | the scope's throttle point, 90 % of the ceiling (`MEMORY_HIGH_FRACTION`): above it the kernel reclaims hard before anything is killed |
| scope launcher | the argv prefix `systemd-run --user --scope --unit=studio-worker --collect --quiet -p MemoryHigh=<h> -p MemoryMax=<m>` |
| effective limit | what the daemon's own cgroup says (`memory.max`, `memory.high`), whatever the config says |

## Configuration

- `memory_max_gb` in `config.toml`: whole GB, default 32 (`DEFAULT_MEMORY_MAX_GB`).
- Valid: 4 (`MIN_MEMORY_MAX_GB`, the daemon plus the smallest engine) to 1024 (`MAX_MEMORY_MAX_GB`).
- The Config page shows it in the Worker card (**Memory ceiling (GB)**); `PUT /daemon/config` refuses a value
  outside the range (`memoryMaxGb: must be a whole number of GB from 4 to 1024`).
- A hand-edited value outside the range is not fatal: the worker logs a warning (`op="memory_ceiling"`)
  and uses the default.
- `MemoryMax=<n>G`; `MemoryHigh=<floor(n × 1024 × 0.9)>M`. For 32: `MemoryHigh=29491M`, `MemoryMax=32G`.

## Where the ceiling comes from

Everything the worker runs descends from the tray UI, and a child stays in its parent's cgroup:

| Process | Started by | In the scope because |
| --- | --- | --- |
| tray UI | login entry, `setup` | the scope launcher starts it |
| daemon | the tray UI (`ProcessStarter`) | child of the tray UI |
| `sd-cli`, `llama-cli`, `nvidia-smi`, … | the daemon | children of the daemon |
| installer (auto-update) | the daemon | child of the daemon |
| daemon after an update | `restart_self` | `exec`: the same process |
| tray UI after an update or a display wait | itself | `exec`: the same process |
| tray UI relaunched by the watchdog | the daemon | child of the daemon |

### The login entry

- Linux, scope launcher usable: `Exec=systemd-run --user --scope --unit=studio-worker --collect --quiet
  -p MemoryHigh=<h> -p MemoryMax=<m> <exe> ui`.
- Linux without it (no systemd user manager, `systemd-run` missing): `Exec=<exe> ui`, as before.
- macOS, Windows: unchanged; there is no ceiling there.
- The launcher is usable when `systemd-run --user --scope --quiet --collect -p MemoryMax=<m> true` exits 0
  (the launcher probe). Its outcome is logged (`op="memory_ceiling"`).
- The tray UI rewrites the entry at start and after every successful Config save, so a new ceiling reaches
  the next login.

### `setup`

- `setup` starts the tray UI through the scope launcher when it is usable and `studio-worker.scope` is not
  active.
- When the scope is active, a worker already runs in it: `setup` starts the tray UI plainly; it hands over to
  the running one (or, when only the daemon is left in the scope, becomes the tray, which is small).

### Live changes

- At start, and after every Config save that changes `memory_max_gb`, a daemon inside `studio-worker.scope`
  sets the scope's limits to the configured ones: `systemctl --user set-property --runtime studio-worker.scope
  MemoryHigh=<h> MemoryMax=<m>`.
- A daemon outside the scope changes nothing; the next start through the login entry applies the ceiling.

### Effective limit

At start the daemon reads `/proc/self/cgroup` and its cgroup's `memory.max` and `memory.high`, and logs them
(`op="memory_ceiling"`):

- info `memory ceiling in effect` with `cgroup`, `memory_max`, `memory_high`, `configured_gb`;
- warn `no memory ceiling in effect` with the reason: not in `studio-worker.scope` (started outside the
  login entry, or without systemd), or the cgroup has no memory limit (memory controller not delegated);
- info `no memory ceiling on this platform` off Linux.

## Admission of host memory

Device memory alone does not say whether a job fits: a job whose engine keeps its weights in host memory
needs host memory too.

- **Evidence** (verified in `daemon.log`): `qwen-image-2512` (`offloadToCpu: true`, no `mmap`, 13.85 GiB of
  weights) was admitted three times on device memory alone (`needed_gib=12`, `free_gib` 13.4 to 19.97), and
  `sd-cli` died by a signal each time (`exit=None` after 188 s, 40 s, 11 s) while holding 21 to 27 GB.
- **Why the ceiling is not enough** (inferred): the ceiling bounds the worker, not the machine. A job
  started while other programs already hold most of the memory still pushes the host into its OOM killer,
  inside the ceiling or not.
- **Decision**: admission also checks host memory, for the jobs where the need is known.

### Which jobs

- A job whose `ModelSource` has `cliDefaults.offloadToCpu: true` and not `mmap: true`: its engine copies every
  weight into host memory.
- Needed: the sum of the source's `files[].approxBytes`, in GiB (a file without `approxBytes` counts 0, so the
  need is a lower bound).
- `mmap: true` jobs are not checked: their weights are file pages the kernel can reclaim, and a model larger
  than the ceiling (Qwen-Image-2.1, 29 GiB) must still run, slower.
- Every other job is not checked: no engine states its host memory need.

### The check

- Available: the smaller of the host's `MemAvailable` (`/proc/meminfo`) and, when the daemon's cgroup has a
  `memory.max`, that limit minus the cgroup's anonymous memory (`anon` in `memory.stat`; file pages are
  reclaimable).
- Admitted when needed ≤ available − 4 GiB (`HOST_RAM_MARGIN_GIB`: what the job's runtime holds beyond its
  weights, and the desktop's breathing room).
- Unknown available memory (off Linux, unreadable `/proc`): admitted unmeasured, logged.
- Device memory is checked first; a job must pass both.

### Outcome

- `op="admit"` "transient job admitted" carries `host_needed_gib` and `host_available_gib` when host memory
  was checked.
- A refusal is "transient job refused" with the error `insufficient host memory: needs <n> GiB, <a> GiB
  available (<m> GiB kept in reserve)`; a device refusal says `insufficient device memory: …`.
- The studio receives a `reject` with code `insufficient_memory`, as for device memory.
- A local API request answers `409` with the refusal as its message, which names the memory.

## Considered and not chosen

- **The tray UI moves itself into the scope** (re-exec through `systemd-run` when it starts outside it):
  covers a hand-started `studio-worker ui` too, but hides the launcher inside the process, and a failed
  `systemd-run` after the `exec` leaves no worker and no log. The login entry states the ceiling where the
  operator can read it.
- **A memory limit per job** (`systemd-run --scope` around each `sd-cli`): bounds one engine, not the daemon
  and its in-process models, and multiplies scopes.

## Limits

- One scope name per user: a second worker with another `--config` started by `setup` while the scope is
  active runs without a ceiling.
- A tray UI started by hand from a terminal runs without a ceiling, and so does the daemon it starts.
- At login, when `studio-worker.scope` is still active from an earlier session, the launcher fails and the
  daemon's watchdog relaunches the tray UI inside the scope.
