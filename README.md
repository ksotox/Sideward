# Sideward

Sideload iOS apps with your Apple ID over **USB or Wi-Fi**. It's an open-source alternative to Sideloadly, built on [isideload](https://github.com/nab138/isideload), the library behind [iloader](https://github.com/nab138/iloader).

Sideward ships as a desktop app (`sideward-gui`) and a command-line tool (`sideward`). Both run on Windows, macOS and Linux.

## Why another sideloader?

- **Apple ID login that works in 2026.** In October 2026 Apple started rejecting Android-emulated anisette data, which shows up as `Login failed (-80045/-80046): This action cannot be completed at this time`. Sideward uses the same fix as iloader 2.4: Mac-emulating anisette servers, with the client info fetched from the server. If Apple rejects one server, it switches to the next automatically.
- **Wi-Fi without iTunes.** Plug in once and run `wifi setup`. After that, Sideward finds the device on your network over Bonjour and installs over plain TCP using the saved pairing file. Neither usbmuxd nor iTunes has to see the device. Wi-Fi sync through iTunes/Finder (usbmuxd) works too.
- **AutoRefresh when a device connects.** Free Apple IDs sign apps for 7 days. Sideward remembers what it installed, keeping a copy of the IPA and any tweaks. Whenever a device is plugged in or shows up on Wi-Fi, it re-signs that device's apps before the 7 days run out, and the apps keep their data. You can turn this on or off per app, like Sideloadly's AutoRefresh.
- **Starts with your computer and stays signed in.** Sideward can launch at login and wait in the tray (minimized on Linux), and it keeps you signed in until your Apple ID password changes or Apple asks you to sign in again.
- **Sideloadly's advanced options:** change name, bundle ID, version, or minimum iOS; enable file sharing; remove device restrictions, extensions, or the Watch app; inject tweaks (`.dylib`, `.framework`, `.bundle`, rootful or rootless `.deb`).
- **AltStore-style sources.** Add AltStore/SideStore source URLs, browse their apps, install with one click, and see when an installed app has an update.
- **Device tools built in** (what you'd otherwise use libimobiledevice, ideviceinstaller and ios-deploy for): device info, installed apps and uninstall, a live device log, crash reports, on-device provisioning profiles, copying files in and out of an app, restart/shut down/sleep. All of it works over USB or Wi-Fi.
- **Pseudo-signing like `ldid`.** Ad-hoc sign an IPA without an Apple ID, keeping or replacing its entitlements, for TrollStore or jailbroken devices. `inspect` shows an IPA's signature, entitlements and provisioning profile like `codesign -d`.
- **Safety checks.** It refuses encrypted App Store IPAs up front, and when your account hits its certificate limit it asks which certificate to revoke.

## Install

Download a build from the repository's Actions artifacts, or build it yourself with Rust 1.88 or newer:

```sh
cargo build --release                 # CLI -> target/release/sideward
cargo build --release -p sideward-gui # app -> target/release/sideward-gui
```

Platform notes:

| Platform | USB | Wi-Fi |
|---|---|---|
| Windows | Install **iTunes from apple.com** (not the Microsoft Store version) for Apple Mobile Device Support | Works without iTunes after `wifi setup` |
| macOS | Built in | Built in |
| Linux | Install and start `usbmuxd` | Works without usbmuxd after `wifi setup` |

## Desktop app

Run `sideward-gui`, sign in, pick a device, and drop an `.ipa` onto the window. When a USB device is selected, **Enable Wi-Fi installs** sets it up for wireless installs.

- **My apps** lists each installed app, when it expires, and an **AutoRefresh** switch.
- **Settings → AutoRefresh** sets when apps are re-signed: when fewer than N days are left (default 3), or with 7, every time the device connects (at most every 12 hours per app).
- **Settings → Startup**: **Start Sideward when I sign in to this computer** (Windows: the Run registry key, no admin needed; macOS: a launch agent; Linux: an XDG autostart entry). Started this way it opens straight to the tray.
- On Windows and macOS, closing the window keeps Sideward running in the tray so AutoRefresh keeps working. Right-click the tray icon for **Re-sign apps now** or **Quit Sideward**. You can turn this off in Settings. Linux has no tray: started at login, Sideward opens minimized, and closing the window quits.
- **Stay signed in** is on by default: the password goes to the OS keychain after the first successful sign-in, and Sideward signs in by itself at startup. If Apple rejects the saved password (it was changed) or asks for a 2FA code, the window pops up and asks. **Sign out** forgets the password.
- Only one copy runs at a time; opening Sideward again brings the running window forward.
- **Device** shows the selected device's model, iOS version, battery, storage, and whether Developer Mode is on (sideloaded apps won't open without it). It also has **Installed apps** (with Uninstall), **Provisioning profiles** (with **Remove expired**), **Crash reports** (filter by app, save to Downloads), a **Live log** with a text filter, and Restart / Shut down / Sleep.
- **Sources** adds AltStore/SideStore source URLs, lists their apps with **Install** buttons, and shows **Updates** for apps you installed through Sideward.
- **Pseudo-sign IPA…** on the Install tab writes an ad-hoc signed `.tipa` for TrollStore.

## Command line

```sh
sideward login                          # Apple ID; you stay signed in (OS keychain) until the password changes
sideward devices                        # USB, Wi-Fi (usbmuxd), and Wi-Fi (direct)
sideward install YouTube.ipa            # sign + install to the only/best device
sideward install App.ipa -d "My iPhone" --wifi
sideward install App.ipa --ip 192.168.1.42
```

### Wi-Fi

```sh
sideward wifi setup                     # once, with the device on USB (tap Trust if asked)
sideward devices --wifi                 # finds it on the same network
sideward wifi import pairing.plist      # use a pairing file from SideStore / iloader / Jitterbug
sideward wifi export <UDID> out.plist   # hand the pairing to SideStore, StikDebug, ...
```

Direct Wi-Fi needs the computer and the device on the same network, with the device unlocked at least once since it booted.

### App changes (Sideloadly's "Advanced options")

```sh
sideward install YouTube.ipa \
  --name "YouTube+" --bundle-id com.me.youtube \
  --inject YTLitePlus.deb --inject CydiaSubstrate.framework \
  --file-sharing --remove-extensions
```

| Flag | Effect |
|---|---|
| `--name`, `--bundle-id`, `--app-version`, `--min-ios` | Edit `Info.plist` (extension bundle IDs follow `--bundle-id`) |
| `--file-sharing` | Show the app's Documents in the Files app |
| `--remove-supported-devices` | Drop `UISupportedDevices` |
| `--remove-extensions`, `--remove-watch` | Delete `PlugIns`/`Extensions` or `Watch` |
| `--inject PATH` | Copy into `Frameworks/` and add a load command; `.deb` packages are unpacked and their dylibs, frameworks, and bundles injected; references to `/Library/Frameworks/...` and `/usr/lib/...` are re-pointed at `@rpath` |
| `--register-extensions` | A separate App ID per extension (default: reuse the main one, which saves App IDs) |
| `--increased-memory-limit` | Add the increased-memory-limit entitlement |

Tweaks built against CydiaSubstrate need a substrate framework in the app; inject ElleKit's `CydiaSubstrate.framework` alongside them. Sideward warns you when it's missing.

`sideward export App.ipa -o signed.ipa` signs without installing, and `sideward inspect App.ipa` shows bundle ID, version, extensions, and whether the IPA is encrypted.

### Refreshing apps

```sh
sideward apps list                      # what's installed and when it expires
sideward refresh                        # re-sign apps expiring within 2 days
sideward daemon                         # AutoRefresh: re-sign whenever a device connects
sideward apps auto-refresh YouTube off  # per-app switch
sideward auto-refresh days 3            # re-sign when fewer than 3 days are left (7 = every connect)
```

The daemon watches for devices plugging in over USB (instantly) and saved Wi-Fi devices appearing on the network (every 5 minutes). It also checks devices that stay connected once an hour. It signs in with the saved password, or `SIDEWARD_PASSWORD`. Apps keep their data when refreshed. Pass `--no-save-password` to any command to sign in without saving the password, and run `sideward logout` to forget it.

Apps installed with Sideloadly itself can't be re-signed, because Sideward needs the original IPA. Install them once with Sideward and they're tracked from then on.

### Device tools

Like libimobiledevice, ideviceinstaller and ios-deploy. Each takes `-d <device>`, `--usb`, `--wifi` or `--ip` like `install` does:

```sh
sideward device info                          # model, iOS, battery, storage, Developer Mode
sideward device apps [--system]               # installed apps; marks the sideloaded ones
sideward device uninstall com.example.app
sideward device syslog --filter YouTube       # live log, filtered (Ctrl+C to stop)
sideward device crashes --app YouTube --save ./crashes
sideward device profiles list | remove <uuid> | clean    # clean = remove expired
sideward device files ls <bundle-id> [path]   # an app's container (development-signed) or Documents
sideward device files pull <bundle-id> /Documents --to ./backup
sideward device files push <bundle-id> ./save.dat [--to /Documents]
sideward device restart | shutdown | sleep
```

### Sources (AltStore / SideStore)

```sh
sideward source add https://apps.sidestore.io
sideward source apps [--search delta]
sideward source install SideStore             # name or bundle id; downloads, signs, installs
sideward source updates [--install]           # newer versions of apps you installed
```

### Signing without an Apple ID (ldid / codesign)

```sh
sideward inspect App.ipa                      # signature, team, entitlements, embedded profile
sideward inspect --entitlements App.ipa       # entitlements as XML, like `ldid -e`
sideward export App.ipa -o App.tipa --adhoc   # pseudo-sign (ldid -S), keeping the app's entitlements
sideward export App.ipa -o App.tipa --adhoc --entitlements ents.plist
```

Ad-hoc signed apps install with TrollStore or on jailbroken devices; stock iOS needs an Apple ID signature (`install` / `export` without `--adhoc`). All the app changes (`--inject`, `--name`, ...) work with `--adhoc` too.

### Account housekeeping

```sh
sideward certs list | sideward certs revoke <serial>
sideward appids list | sideward appids delete <id>   # free accounts: 10 App IDs per week
sideward anisette list | sideward anisette set ani3.stikstore.app
```

## Troubleshooting

| Message | Fix |
|---|---|
| `-80045` / `-80046` "This action cannot be completed at this time" | Anisette data rejected. Leave automatic fallback on, or `sideward anisette set` another Mac server |
| `-22406` | Wrong Apple ID or password. Use your real password, not an app-specific one |
| `-36607` | Apple is rate-limiting this login. Wait a few minutes |
| "Could not reach usbmuxd" | Windows: install iTunes from apple.com. Linux: start `usbmuxd`. Wi-Fi direct still works |
| Wi-Fi install fails with "device socket io failed" | The phone dropped the Wi-Fi connection. Sideward keeps it alive with iOS's heartbeat service and retries twice; if it still fails, keep the phone unlocked and on the same network, or use USB |
| Sideloaded app won't open (iOS 16+) | Turn on Settings → Privacy & Security → Developer Mode. `sideward device info` tells you if it's off |
| App crashes on launch | `sideward device crashes --app <name>` or **Device → Crash reports**; a tweak missing CydiaSubstrate is a common cause |
| "This IPA is encrypted" | You have an App Store copy. Use a decrypted IPA |
| Certificate limit | Pick one to revoke when asked, or pass `--auto-revoke` (it prefers Sideward's own old certificate) |

## Where data lives

The platform data directory (`%APPDATA%\sideward`, `~/Library/Application Support/dev.sideward.Sideward`, or `~/.local/share/sideward`), overridable with `SIDEWARD_HOME`. It holds anisette state per server, the signing key and certificate, pairing files, and copies of remembered apps. Passwords are only stored in the OS keychain (Windows Credential Manager, macOS Keychain, or the Secret Service on Linux).

## Layout

```
crates/sideward-core   library: login, devices, Wi-Fi, IPA edits, injection, signing, refresh
apps/sideward-cli      `sideward` command-line tool
apps/sideward-gui      `sideward-gui` desktop app (egui)
```

## Credits

[isideload](https://github.com/nab138/isideload) and [iloader](https://github.com/nab138/iloader) by nab138 (MIT), and [idevice](https://github.com/jkcoxson/idevice) by jkcoxson (MIT). Thanks also to the anisette server operators (jkcoxson, SideStore, StikStore). Sideward isn't affiliated with Apple or Sideloadly.

## License

MIT
