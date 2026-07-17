# Third-party notices

InferenceHub Desktop incorporates third-party software under the licenses below.

## Onyx desktop client

The Tauri shell in `desktop/src-tauri/` derives from the Onyx desktop client
([onyx-dot-app/onyx](https://github.com/onyx-dot-app/onyx)). Per the Onyx
repository license, content outside its `ee` directories — including the
desktop client — is available under the MIT (Expat) license:

```
Copyright (c) 2023-present DanswerAI, Inc.

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## whisper.cpp

The macOS live-transcription helper (`ih-stt-helper`) statically links
[whisper.cpp](https://github.com/ggml-org/whisper.cpp), used under the MIT
license:

```
MIT License

Copyright (c) 2023-2024 The ggml authors

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## Jan (project heritage)

This project began as a fork of [Jan](https://github.com/janhq/jan)
(Copyright 2025 Menlo Research, Apache License 2.0). The current codebase no
longer contains Jan source; the credit is retained in `NOTICE`.

## Rust and JavaScript dependencies

The shipped binaries statically link Rust crates (including
[Tauri](https://tauri.app)) and bundle JavaScript packages, each under its
respective license (predominantly MIT and Apache-2.0), as declared in
`desktop/src-tauri/Cargo.toml` and `desktop/package.json`.
