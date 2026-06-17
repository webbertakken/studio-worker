# Changelog

## [0.4.7](https://github.com/webbertakken/studio-worker/compare/studio-worker-v0.4.6...studio-worker-v0.4.7) (2026-06-17)


### Bug Fixes

* name sd-cli inputs by content, decode jpeg ([#103](https://github.com/webbertakken/studio-worker/issues/103)) ([1faf145](https://github.com/webbertakken/studio-worker/commit/1faf1451a952c81d094c384d0be0213d80ed24d1))

## [0.4.6](https://github.com/webbertakken/studio-worker/compare/studio-worker-v0.4.5...studio-worker-v0.4.6) (2026-06-17)


### Features

* load-dynamic onnx runtime (cross-platform) ([#101](https://github.com/webbertakken/studio-worker/issues/101)) ([e4c7c74](https://github.com/webbertakken/studio-worker/commit/e4c7c7404dd8ede53096f6a2587cd4fced57571c))
* structured reject codes + sha256 verify ([#48](https://github.com/webbertakken/studio-worker/issues/48)) ([902b278](https://github.com/webbertakken/studio-worker/commit/902b2788d0f82b12da565f423cce3c0eb9dc2111))


### Bug Fixes

* avoid deadlock regenerating register secret ([#94](https://github.com/webbertakken/studio-worker/issues/94)) ([53949a5](https://github.com/webbertakken/studio-worker/commit/53949a59ee8b42a1ed1b8aa32b6d7f3993f94d2f))
* clean exit when stopped before approval ([#91](https://github.com/webbertakken/studio-worker/issues/91)) ([c383cf3](https://github.com/webbertakken/studio-worker/commit/c383cf38d8074c62cb2dacc9c6b6b63d5c44f27b))
* detect VRAM via nvidia-smi when sysfs lacks it ([#55](https://github.com/webbertakken/studio-worker/issues/55)) ([4547cab](https://github.com/webbertakken/studio-worker/commit/4547cabd501d2c916a6eb99981a7b409e1bf79aa))
* gate coverage_attribute so nightly llvm-cov builds ([#82](https://github.com/webbertakken/studio-worker/issues/82)) ([ec420e5](https://github.com/webbertakken/studio-worker/commit/ec420e573e349b46cb5f4a8bb0f75c5d8b1fba6f))
* link onnx static lib on older glibc (__isoc23) ([#50](https://github.com/webbertakken/studio-worker/issues/50)) ([8e8957f](https://github.com/webbertakken/studio-worker/commit/8e8957f46c20302f42ee2bd818522d2e3c447409))
* log onnx removal failures at engine target ([#57](https://github.com/webbertakken/studio-worker/issues/57)) ([94ef1e0](https://github.com/webbertakken/studio-worker/commit/94ef1e085adfa7ee4618e01ed1bd893d4902c2d0))
* log operator config applies via UI ([#74](https://github.com/webbertakken/studio-worker/issues/74)) ([3807b20](https://github.com/webbertakken/studio-worker/commit/3807b20218a5147272aa2a1a149448088824438f))
* log sdcpp unsupported-kind rejection ([#58](https://github.com/webbertakken/studio-worker/issues/58)) ([277fd88](https://github.com/webbertakken/studio-worker/commit/277fd88adc7a6bb4b178da06cc75084f73102dfd))
* log spawn error on failed service step ([#69](https://github.com/webbertakken/studio-worker/issues/69)) ([5510dbd](https://github.com/webbertakken/studio-worker/commit/5510dbd52901973bfdf54d59aee54b4d6f570cef))
* serialise llama backend init ([#46](https://github.com/webbertakken/studio-worker/issues/46)) ([1e5dd75](https://github.com/webbertakken/studio-worker/commit/1e5dd75f0688fe411f70dc055c994098f4901d3f))
* share temp-file guard, plug onnx leak ([#59](https://github.com/webbertakken/studio-worker/issues/59)) ([8a61ae5](https://github.com/webbertakken/studio-worker/commit/8a61ae5f1bb735ef6394084a3a520c1a5b386d26))
* ship pause/resume log; harden probe tests ([#87](https://github.com/webbertakken/studio-worker/issues/87)) ([c7319f9](https://github.com/webbertakken/studio-worker/commit/c7319f92d8e906c5b0598ac1362a71df567468b1))
* structure auto-register log breadcrumbs ([#73](https://github.com/webbertakken/studio-worker/issues/73)) ([a5c3ab7](https://github.com/webbertakken/studio-worker/commit/a5c3ab787b2960b72f44e72d1a02864b4c1d5b23))
* surface game + asset on offer log ([#97](https://github.com/webbertakken/studio-worker/issues/97)) ([102642c](https://github.com/webbertakken/studio-worker/commit/102642c6c51f2790a14c7b22623dead8d68e7255))
* surface HTTP body on failed WS upgrade ([#75](https://github.com/webbertakken/studio-worker/issues/75)) ([1e1619c](https://github.com/webbertakken/studio-worker/commit/1e1619cd4cc25ea984d2e4b721599d2c4639be88))
* surface log fields + warn on VRAM threshold ([#100](https://github.com/webbertakken/studio-worker/issues/100)) ([33996ee](https://github.com/webbertakken/studio-worker/commit/33996eee86d86e0147dabcf6815b6e07a1d76869))
* surface session error in reconnect log ([#56](https://github.com/webbertakken/studio-worker/issues/56)) ([20f310a](https://github.com/webbertakken/studio-worker/commit/20f310a66944dd30016d13b3c31b8adca77bd215))
* surface silent provision + WS failures ([#67](https://github.com/webbertakken/studio-worker/issues/67)) ([057a2c1](https://github.com/webbertakken/studio-worker/commit/057a2c196bbe4be3292bc641458405c21d7e22fb))
* surface silently-dropped probe/update data ([#95](https://github.com/webbertakken/studio-worker/issues/95)) ([6f69e43](https://github.com/webbertakken/studio-worker/commit/6f69e431442248e420a1b457ecb42a93fed68f78))
* warn on empty sd-cli URL override ([#70](https://github.com/webbertakken/studio-worker/issues/70)) ([2206ba8](https://github.com/webbertakken/studio-worker/commit/2206ba85eed7a8f7d1bc306a7d22d882e38fcafa))
* warn on model-download failure paths ([#64](https://github.com/webbertakken/studio-worker/issues/64)) ([bf55622](https://github.com/webbertakken/studio-worker/commit/bf55622fe7f1597a8e8c146a756a331df33ba9ad))
* Windows auto-update + start minimised ([#49](https://github.com/webbertakken/studio-worker/issues/49)) ([2f36f60](https://github.com/webbertakken/studio-worker/commit/2f36f60fcb8ee483b238fb96dcdada30623cac90))
* worker robustness improvements ([#47](https://github.com/webbertakken/studio-worker/issues/47)) ([5ac9ff4](https://github.com/webbertakken/studio-worker/commit/5ac9ff41591264425afcdcd0d01f595f10c2e5b7))

## [0.4.5](https://github.com/webbertakken/studio-worker/compare/studio-worker-v0.4.4...studio-worker-v0.4.5) (2026-06-04)


### Features

* add ONNX/LaMa object-removal image engine ([#42](https://github.com/webbertakken/studio-worker/issues/42)) ([e3f1681](https://github.com/webbertakken/studio-worker/commit/e3f16812f2a782e0cf645eff2e362af61f8a6dfc))

## [0.4.4](https://github.com/webbertakken/studio-worker/compare/studio-worker-v0.4.3...studio-worker-v0.4.4) (2026-06-03)


### Features

* vulkan preflight + first-class arm/intel ([#37](https://github.com/webbertakken/studio-worker/issues/37)) ([60bc8b4](https://github.com/webbertakken/studio-worker/commit/60bc8b456834c9f70ef8fdea90b56f93f094f9db))

## [0.4.3](https://github.com/webbertakken/studio-worker/compare/studio-worker-v0.4.2...studio-worker-v0.4.3) (2026-06-02)


### Bug Fixes

* **update:** parse component-prefixed release tags ([#34](https://github.com/webbertakken/studio-worker/issues/34)) ([1d00d43](https://github.com/webbertakken/studio-worker/commit/1d00d4312fa32aeac8ede442bff3e0f7f7702733))

## [0.4.2](https://github.com/webbertakken/studio-worker/compare/studio-worker-v0.4.1...studio-worker-v0.4.2) (2026-06-02)


### Features

* auto-provision sd-cli for image gen ([#32](https://github.com/webbertakken/studio-worker/issues/32)) ([5b8e5db](https://github.com/webbertakken/studio-worker/commit/5b8e5dbdaabdbeeabe9571099f8bb16be1cbd55c))

## [0.4.1](https://github.com/webbertakken/studio-worker/compare/studio-worker-v0.4.0...studio-worker-v0.4.1) (2026-06-02)


### Bug Fixes

* **build:** gate in-process llama.cpp off Windows so the release binaries link (UI + image + media still ship on Windows; Linux/macOS keep the LLM) ([#30](https://github.com/webbertakken/studio-worker/issues/30)) ([d3e446a](https://github.com/webbertakken/studio-worker/commit/d3e446a))


### Continuous Integration

* publish to crates.io on release tags so `cargo install studio-worker` tracks the latest release ([#29](https://github.com/webbertakken/studio-worker/issues/29)) ([82f90ed](https://github.com/webbertakken/studio-worker/commit/82f90ed))

## [0.4.0](https://github.com/webbertakken/studio-worker/compare/studio-worker-v0.3.0...studio-worker-v0.4.0) (2026-06-02)


### Features

* all platforms work out of the box: UI on by default, GTK-free Linux build (rustls + ksni), packaged backends + model auto-download, real Windows autostart ([#25](https://github.com/webbertakken/studio-worker/issues/25)) ([d40600f](https://github.com/webbertakken/studio-worker/commit/d40600f))
* reference/EDIT mode for instruction editors ([#26](https://github.com/webbertakken/studio-worker/issues/26)) ([e6b6abf](https://github.com/webbertakken/studio-worker/commit/e6b6abf))


### Bug Fixes

* **http:** log upload byte size on complete ([#24](https://github.com/webbertakken/studio-worker/issues/24)) ([08846b1](https://github.com/webbertakken/studio-worker/commit/08846b1))

## [0.3.0](https://github.com/webbertakken/studio-worker/compare/studio-worker-v0.2.1...studio-worker-v0.3.0) (2026-05-29)


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
