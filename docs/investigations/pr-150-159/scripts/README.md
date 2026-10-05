# Reproducing the native checks

These are independent hosts of the actual Erika binaries. They complement the
baseline/head regression logs; they do not claim OHOS emulator execution or
Apple coverage.

## Windows

Build this branch's `erika_capi` using the native dependencies described in
`docs/building.zh.md`, MSVC, LLVM, and the Windows SDK. Install Python 3.
Place the resulting DLL at `scripts/target/debug/erika_capi.dll`, and download
the BBB sample described in `../review.zh.md` to `scripts/bbb-10s.mp4`.
Then, from PowerShell:

```powershell
python .\windows-live-capi.py C:\path\to\Erika
```

The host creates a Win32 HWND, attaches the production D3D11 renderer, plays
local and HTTP media, and exercises pause, EOF seek, resume, rate, MP3 probe,
and malformed inputs. It does not record a visible window or listen to audio.

## Linux

The recorded environment was Ubuntu 26.04 in the existing
`erika-performance-ci:20260930` image, Rust 1.93.1, FFmpeg 8, patched libass,
Xvfb, PulseAudio, and Mesa llvmpipe. Build the branch with libass and wgpu:

```sh
export CARGO_TARGET_DIR=/target
cargo build --locked -p erika_capi -p linux_native_demo
cc examples/linux_native_demo/capi_smoke.c -I crates/erika_capi/include \
  -L /target/debug -Wl,-rpath,/target/debug -lerika_capi -lX11 \
  -o /target/debug/linux_capi_smoke
```

The scripts assume the checkout at `/work`, binaries at `/target/debug`, and
scripts/media/output at `/review-logs`. Use a non-root account, a writable
`/tmp/erika-pr-desktop` directory with mode 0700, and a null PulseAudio sink.

```sh
xvfb-run -a bash /review-logs/native-desktop.sh
xvfb-run -a bash /review-logs/linux-live-subtitle.sh whitespace-separators.srt
```

Create the subtitle sidecar as two SRT cues, `First` from 1–2 seconds and
`Second` from 3–4 seconds, with `space + tab + space` on their separator line.
The host captures a paused frame after a short resumed seek; inspect the
`whitespace-separators-second.png` output for the visible `Second` cue. The
pixel count also includes any video refresh caused by track switching and is
not a measure of the subtitle's area.

The Python host runs the 1-second attached-cover MP3 for 4 seconds and asserts
actual audio consumption. It consumed 48000 frames with an empty final queue
and zero audio errors. A shorter 1.5-second observation initially measured no
consumption and was insufficient to judge the output. The separate native demo
consumed audio on a 5-second MP3, but its generic smoke exit check requires video
and exits 1 for audio-only files. `linux-cover-audio.sh` preserves that result.

Generate its two samples using the repository's attached-cover MP3:

```sh
ffmpeg -y -stream_loop -1 -i /work/crates/erika/testdata/software/mp3-attached-pic.mp3 \
  -t 5 -map 0:a -map 0:v -c:a libmp3lame -c:v copy \
  -disposition:v attached_pic /review-logs/cover-5s.mp3
ffmpeg -y -i /review-logs/cover-5s.mp3 -map 0:a -c copy /review-logs/no-cover-5s.mp3
xvfb-run -a bash /review-logs/linux-cover-audio.sh
```

## OHOS SDK build

Official public archive:

<https://repo.huaweicloud.com/openharmony/os/5.1.0-Release/ohos-sdk-windows_linux-public.tar.gz>

SHA256: `8c1f97df34812e45fa4f0e3275987b261e80622abb7c51392bb063f853451344`.

Extract the Linux native/toolchains/previewer archives to
`/opt/erika-ohos-sdk-5.1.0/linux`, then:

```sh
bash /review-logs/ohos-native-build.sh
```

This builds the full modified Flutter and ArkTS bridges for ARM64 against
API 18, links the SDK libraries, and links the checksum-pinned v0.2.1 C API
release. It does not rebuild this branch's Rust runtime or run an emulator.

## OHOS emulator: required next stage

The official DevEco download entry currently requires a Huawei login. The
public SDK archive has a previewer, not a complete phone OS emulator image.
After obtaining DevEco and a supported local image:

1. Verify the image ABI before choosing the runtime. The package and xtask
   currently support ARM64. An x86_64 emulator needs a matching C API build and
   FFmpeg dependencies; renaming the ARM64 `.so` is not sufficient.
2. Build/install a real HAP with the modified ArkTS bridge and attach an
   XComponent. Push the BBB and audio/video fixtures via `hdc`.
3. Observe actual decoded/rendered/audio frames, zero error counters, pause,
   EOF seek, resume, rate, subtitle output, and surface resize/recreation.
4. Keep an old native ID, destroy its player, create a new one, then execute
   late invoke/poll/destroy calls against the old ID. The new player's media,
   surface, state, and audio must remain unaffected. Repeat this on both
   bridges. Host allocation-reuse checks are not a substitute for this stage.
5. Save `hilog`, `hdc` install/start results, emulator image/ABI/version, and
   screenshots of actual playback before marking the platform verified.

These steps are pending; no emulator result is claimed in this report.
