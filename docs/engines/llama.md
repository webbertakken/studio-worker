# llama engine (chat models)

The `llama-cpp` engine serves every `llm` task. On Linux and macOS it runs llama.cpp through
`llama-cpp-2` (cargo feature `llama`; `cuda` offloads every layer to the GPU) in a
[llama process](#llama-process) of its own, either as a transient job (loaded for the task, freed
after) or on a loaded model's own lane.
On Windows it drives the official `llama-cli` as a subprocess (`llama_subprocess.rs`), text only.

A model's source names its weights (the `model` file) and, for a model that reads pictures, its
vision projector (the `text-encoder-vision` file). Its `cliDefaults` carry `contextSize` (8192 when
absent, capped at the model's trained context) and `chatTemplateKwargs`, merged under a request's
own (`{"enable_thinking": false}` turns Qwen3.5's reasoning off).

## Structured output

A task's `jsonSchema` (the local API's `response_format: json_schema`) becomes a llama.cpp
grammar, so the answer parses against the schema. The subprocess engine has no grammar and
refuses a task with a schema rather than answer in free text.

## Vision

A model reads images when its source carries a vision projector (llama.cpp's `mmproj`); `GET
/models` shows it as `images: true`, and the studio declares it as the `images` capability.

- **Inputs:** a task's `images` are absolute URLs (the studio's job inputs, fetched with no
  credentials over `https`, or `http` on loopback) or inline `data:image/...;base64,` URLs. PNG,
  JPEG and WebP decode to RGB; at most 16 pictures (`MAX_IMAGES`), 25 MiB each, 60 s per fetch.
- **Order:** one media marker per picture opens the last user message, so the model reads them
  in order with that message's text.
- **Refusals:** a model without a projector refuses a task with images (`reads no images`), and so
  does the subprocess engine; a picture is never dropped and answered about unseen. A picture that
  fails to fetch or decode fails the task, naming its index.
- **Logs:** `studio_worker::vision` logs each set of pictures read (count, sizes, time) and every
  refusal; the projector load logs once per model.
- **Size:** each picture costs context tokens: six pictures of about 1100×800 took 4,923 prompt
  tokens on Qwen3.5 9B, so the 8K default is tight and a 16K context leaves room.

## Llama process

llama.cpp never runs inside the daemon.
Each model it serves runs in a **llama process**: the daemon's own executable started as
`studio-worker llama-serve` (hidden from `--help`), a child of the daemon.

### Why a process

ggml ends the whole process on a CUDA error during generation: `CUDA_CHECK` calls `ggml_abort`,
which calls `abort()`.
A VRAM shortage does this: another process (an `sd-cli` run started outside the worker) takes the
memory, and ggml's memory pool grows during a decode.
Inside the daemon that abort ended every job, the local API and the studio session.
In a llama process it ends one job.

Rejected, by evidence (reproduced with a scratch process holding the VRAM):

| Design | Why not |
| --- | --- |
| Admit against the real free VRAM first | Racy: the pool grows during decode, after any check; 946 MiB free survived, 964 MiB aborted |
| Size the context to what fits | Smaller need, same abort path |
| ggml's abort callback | `ggml_abort` calls `abort()` after the callback returns |
| ggml's legacy pool (`GGML_CUDA_NO_VMM`) | Its allocation failure aborts as well |

### Lifetime

- A loaded model has one llama process for as long as it is loaded; a transient job has one for
  that job.
- The daemon downloads the model's files; the process gets their paths, loads them, and serves
  one request at a time.
- The process exits when its stdin closes: the model unloads, the transient job ends, or the
  daemon is gone.
  One that has not exited 5 s later (`STOP_GRACE`) is killed.
- On Linux the program is `/proc/self/exe`, so a binary replaced on disk by an update never
  serves a daemon of another version; elsewhere it is the daemon's executable, and the process
  reports its version, which must equal the daemon's.
- The process runs with `GGML_NO_BACKTRACE=1`: an abort does not wait about 20 s on `gdb`.

### Protocol

One JSON object per line, tagged by `type`, camelCase.
The daemon writes to the process's stdin; the process writes to its stdout.

| Direction | `type` | Fields | Meaning |
| --- | --- | --- | --- |
| daemon → process | `load` | `model`, `modelPath`, `projectorPath?`, `defaults` | the first line: load these files |
| daemon → process | `chat` | `params` (`LlmParams`) | one chat completion |
| daemon → process | `tokenize` | `text`, `addSpecial` | the model's token ids |
| daemon → process | `cancel` | | stop the request in progress |
| process → daemon | `loaded` | `version` | ready to serve |
| process → daemon | `piece` | `text` | answer text as it is generated |
| process → daemon | `done` | `completion` | the `chat.completion` JSON |
| process → daemon | `tokens` | `tokens` | the token ids |
| process → daemon | `failed` | `error`, `permanent` | the load or the request failed (`permanent`: on any worker too); the process keeps serving after a request |
| process → daemon | `log` | `level`, `target`, `message`, `fields?` | one tracing event of the process; `fields` are its other fields as `name=value` |

- A `cancel` applies to the request in progress; a new request clears it.
- A stdout line that is not a protocol line, and every stderr line, is logged by the daemon as
  the process's output (warn).

### Logs

- The process's tracing events (llama.cpp's included, filtered by `RUST_LOG` or
  `DEFAULT_LOG_FILTER` in the process) come back as `log` lines.
- The daemon logs each under `studio_worker::llama_process` with the process's `target`, on the
  thread waiting for the request, so the job's log keeps llama.cpp's lines.

### Failures

Every failure fails one request and leaves the daemon running.

- **Handled** (llama.cpp returns an error, e.g. the context cannot be created): `failed`.
  The job's error is the process's error followed by llama.cpp's own error lines for that
  request, e.g. `creating llama context (llama.cpp: cudaMalloc failed: out of memory; …)`.
- **Process ended** mid-request (an abort, a crash, a kill): the job's error names how it ended
  and llama.cpp's error lines, e.g. `out of GPU memory: the llama.cpp process ended (signal 6,
  SIGABRT): CUDA error: out of memory; …; the daemon is unaffected`.
  The daemon logs `op="process_died"` (error).
- Either error starts with `out of GPU memory: ` when llama.cpp's lines say `out of memory`.
- A loaded model whose process ended stays `loaded`; its next request starts a new process
  (`op="restart"`), loading the weights again (about 1 s for a 2B model).
- **Cancelled** (an unload, a client gone, a deadline): the daemon sends `cancel`; a process
  that has not answered 10 s later (`CANCEL_GRACE`) is killed and the request fails.
- An error quotes llama.cpp's error messages (their fields left out, the process's stderr lines
  included): the first 2 of the request (`FIRST_REASON_LINES`; llama.cpp names the cause first)
  and the last ones, 6 in all (`REASON_LINES`), each at most 300 characters
  (`REASON_LINE_CHARS`).

### Observability

Target `studio_worker::llama_process`:

| `op` | Level | When |
| --- | --- | --- |
| `start` | info | a process loaded its model (pid, model, elapsed) |
| `start` | warn | a process could not start or load |
| `restart` | info | a loaded model starts a new process after the last one ended |
| `process_died` | error | a process ended during a request (pid, exit, reason) |
| `stop` | info | a process exited after its stdin closed, or was killed |
| `cancel` | warn | a cancelled request did not answer in time; the process is killed |
| `output` | warn | a line the process wrote outside the protocol |
