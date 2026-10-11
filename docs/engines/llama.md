# llama engine (chat models)

The `llama-cpp` engine serves every `llm` task. On Linux and macOS it runs llama.cpp in
process through `llama-cpp-2` (cargo feature `llama`; `cuda` offloads every layer to the GPU),
either as a transient job (loaded for the task, freed after) or on a loaded model's own lane.
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
