# PS5 LAN Capture

Watch your PS5 on your PC over the local network, so you can share the game with friends on Discord. No capture card needed.

![icon](assets/icon.png)

## Why

I wanted to stream my PS5 games to friends on Discord while talking to them and hearing both them and the game on one headset.

The official PS Remote Play app was laggy and the picture quality was bad. So I built this.

## How it works

- The app connects to your PS5 using Remote Play over your home network.
- It mirrors the picture and the sound. You keep playing on the monitor connected to your PS5, and the game sound plays in your PC headset.
- You share the app window on Discord. Your friends see and hear the game, and you talk to them on the same headset.
- When a Remote Play session starts, the PS5 disconnects the controller connected to it. That is how Remote Play works. So you plug the controller into your PC instead, and the app sends your input to the console.

## Features

- Up to 1080p at 60 FPS, H.265 hardware decoding on the GPU (Direct3D 11)
- Upscaling for big windows and fullscreen on 1440p/4K monitors: NVIDIA RTX Video Super Resolution, with AMD FSR 1 on other GPUs
- Enhance: debanding for compressed gradients, sharper color edges (luma guided chroma upsampling), adjustable sharpening
- Optional 10 bit HDR stream from the PS5, tone mapped for the window and Discord (less banding in skies and dark scenes)
- Game audio through any Windows output device, with volume, mute and buffer settings
- DualSense over USB or Bluetooth: buttons, sticks, triggers, touchpad, gyro
- Rumble, adaptive triggers and lightbar color from the console
- Xbox controllers work too (PS button = Back + Start)
- Setup wizard: finds the console, registers it, checks your controller and audio
- Wakes the console from rest mode, reconnects automatically
- Built in updater: when a new release appears here, the app shows an Update button and installs it for you
- Login data is encrypted with Windows DPAPI. No telemetry. The only internet requests are the PSN sign in during setup and the GitHub update check (can be turned off in Settings).

## Requirements

- Windows 10 or 11, 64 bit
- PS5 (or PS5 Pro) with Remote Play enabled: Settings > System > Remote Play > Enable Remote Play
- PS5 and PC on the same network. Ethernet recommended.
- A PSN account (you sign in once on Sony's page to get your Account ID)
- A controller connected to the PC

## Install

Download the latest release from the [Releases](https://github.com/vLuckyyy/PS5LanCapture/releases) page:

- `PS5-LAN-Capture-Setup-x.y.z.exe` installer, or
- `PS5-LAN-Capture-x.y.z-portable.zip`, unzip and run `PS5LanCapture.exe`

Then follow the setup wizard.

## Sharper picture on big screens

The PS5 sends at most 1080p. When the video on screen is bigger (fullscreen on a 1440p or 4K monitor), the app upscales it. On NVIDIA RTX cards it uses RTX Video Super Resolution: turn on **RTX video enhancement > Super resolution** in NVIDIA Control Panel (Video > Adjust video image settings). Other GPUs use AMD FSR 1. Debanding, color edge reconstruction, sharpening and the HDR stream are in Settings > Video > Enhance.

## Sharing on Discord

1. Join a voice channel and click Share Your Screen.
2. Pick the PS5 LAN Capture window (not the whole screen, so only the game sound is shared).
3. Do not minimize the window. Discord cannot capture minimized windows.

## Building from source

You need Rust, Git, Python 3, Visual Studio 2022 Build Tools (C++ workload) and LLVM (clang-cl).

```powershell
git clone --recurse-submodules https://github.com/vLuckyyy/PS5LanCapture.git
cd PS5LanCapture
powershell -ExecutionPolicy Bypass -File scripts\setup-deps.ps1
cargo build --release
```

`scripts\setup-deps.ps1` downloads vcpkg libraries and FFmpeg into `.tools\`. `scripts\package.ps1` builds the installer and the release files into `dist\` (needs [Inno Setup 6](https://jrsoftware.org/isinfo.php)).

## Releasing a new version

1. Bump `version` in `Cargo.toml`.
2. Run `scripts\package.ps1`.
3. Create a GitHub release with tag `vX.Y.Z` (not a pre-release) and upload everything from `dist\`.

The app checks the latest release on startup and every 6 hours. It needs the `PS5-LAN-Capture-Setup-X.Y.Z.exe` and `SHA256SUMS.txt` assets to update itself; portable copies get a link to the release page.

## Credits

- [chiaki-ng](https://github.com/streetpea/chiaki-ng) for the Remote Play protocol
- [FFmpeg](https://ffmpeg.org) for video decoding
- [AMD FidelityFX Super Resolution 1](https://github.com/GPUOpen-Effects/FidelityFX-FSR) for upscaling
- [egui](https://github.com/emilk/egui), [winit](https://github.com/rust-windowing/winit), [cpal](https://github.com/RustAudio/cpal), [hidapi](https://github.com/ruabmbua/hidapi-rs)

## License

AGPL-3.0, because it includes chiaki-ng. FFmpeg is included as unmodified LGPL DLLs. License texts of all components are in the `licenses` folder of each release.

PlayStation, PS5, PS Remote Play and DualSense are trademarks of Sony Interactive Entertainment. This project is not affiliated with Sony.
