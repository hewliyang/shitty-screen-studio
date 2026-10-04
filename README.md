<p align="center"><img src="assets/icon.png" width="160" alt="💩"></p>

<h1 align="center">Shitty Screen Studio</h1>

<p align="center">A screen recorder for macOS. Like Screen Studio, but shittier.</p>

## Install

Download the `.dmg` from [Releases](https://github.com/hewliyang/shitty-screen-studio/releases) and drag the app to Applications.

The app is not notarized. Clear the quarantine flag before the first launch:

```sh
xattr -cr "/Applications/Shitty Screen Studio.app"
```

Requires macOS 13 or later on Apple silicon.

## Build from source

Requires Rust (stable) and Xcode command line tools.

```sh
scripts/bundle.sh
```

This builds `~/Applications/Shitty Screen Studio.app`. Set `DEST` to put it somewhere else.

macOS gives the Screen Recording permission to the app bundle, so run the bundle, not `cargo run`. To keep the permission across rebuilds, create a code signing certificate named `Shitty Screen Studio Dev` in Keychain Access, or set `SIGN_IDENTITY`.

## Release

Push a `v*` tag. The release workflow builds the DMG and attaches it to a GitHub release.

```sh
git tag v0.1.0 && git push origin v0.1.0
```
