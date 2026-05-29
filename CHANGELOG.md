# Changelog

## [0.2.2](https://github.com/webbertakken/studio-worker/compare/studio-worker-v0.2.1...studio-worker-v0.2.2) (2026-05-29)


### Features

* per-game image config + no-fallback policy ([#22](https://github.com/webbertakken/studio-worker/issues/22)) ([e7564af](https://github.com/webbertakken/studio-worker/commit/e7564af8185686c65c29252db98a242dc5b2353c))

## [0.2.1](https://github.com/webbertakken/studio-worker/compare/studio-worker-v0.2.0...studio-worker-v0.2.1) (2026-05-28)


### Features

* real image inference via stable-diffusion.cpp ([#17](https://github.com/webbertakken/studio-worker/issues/17)) ([3db534f](https://github.com/webbertakken/studio-worker/commit/3db534fc4d1a772590b33318a2f1de513bd241d2))


### Bug Fixes

* **types:** resolved_task fallback leaves width/height/steps at 0 ([#19](https://github.com/webbertakken/studio-worker/issues/19)) ([71052aa](https://github.com/webbertakken/studio-worker/commit/71052aa6c1a7dd0350ea5b5657ca70d971adf8ab))
* **ws:** send the full prompt on complete, not the 200-char preview ([#20](https://github.com/webbertakken/studio-worker/issues/20)) ([95a1240](https://github.com/webbertakken/studio-worker/commit/95a12403b9209cc58292969257332c118c55311d))

## [0.2.0](https://github.com/webbertakken/studio-worker/compare/studio-worker-v0.1.2...studio-worker-v0.2.0) (2026-05-25)


### ⚠ BREAKING CHANGES

* `bootstrap_token` and the `POST /workers/register` HTTP route are gone.  Existing configs round-trip (the field is ignored on load and dropped on next save).

### Features

* auto-register with operator approval ([#11](https://github.com/webbertakken/studio-worker/issues/11)) ([b5f2155](https://github.com/webbertakken/studio-worker/commit/b5f2155f77a0ebe4e55d6a862dae0333fba97785))
* **ws:** worker channel, replace HTTP polling ([#8](https://github.com/webbertakken/studio-worker/issues/8)) ([ba43424](https://github.com/webbertakken/studio-worker/commit/ba43424658091a479124fdf33e738b48c7cf6fcf))

## [0.1.2](https://github.com/webbertakken/studio-worker/compare/studio-worker-v0.1.1...studio-worker-v0.1.2) (2026-05-25)


### Features

* friendly hint when register can't reach the api ([#3](https://github.com/webbertakken/studio-worker/issues/3)) ([6ab122e](https://github.com/webbertakken/studio-worker/commit/6ab122ea18ec1979c91c1c407ecae9724a5b017f))
* opt-in sentry + structured tracing pass ([#5](https://github.com/webbertakken/studio-worker/issues/5)) ([cd08513](https://github.com/webbertakken/studio-worker/commit/cd0851393c3bc10fa36a296f67fffb168527928b))
* **ui:** native egui desktop UI + tray + autostart ([#7](https://github.com/webbertakken/studio-worker/issues/7)) ([4ae6c08](https://github.com/webbertakken/studio-worker/commit/4ae6c08af37c2d83bda10b9fe8f23f7423269105))

## [0.1.1](https://github.com/webbertakken/studio-worker/compare/studio-worker-v0.1.0...studio-worker-v0.1.1) (2026-05-16)


### Features

* initial release of studio-worker ([9da42a0](https://github.com/webbertakken/studio-worker/commit/9da42a01b453b4dee4db7fb627d8704ef6545844))
* multi engine composes per-modality backends ([d5c2034](https://github.com/webbertakken/studio-worker/commit/d5c203470f20ad54615383e948f13ad4f2f0dcc3))
* multi-modal tasks, auto-update, 93% coverage ([9be920c](https://github.com/webbertakken/studio-worker/commit/9be920cd4494d59ae6b7f8b8fec4db174dd53283))
* real engines (llama, whisper, candle SD, video GIF, TTS) ([d0e32af](https://github.com/webbertakken/studio-worker/commit/d0e32afa645b354bdf41d7cc203afc5b711043a9))

## Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
