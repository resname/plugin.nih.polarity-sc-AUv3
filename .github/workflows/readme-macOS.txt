You may need to disable Gatekeeper on macOS to be able to use the plugin:
https://disable-gatekeeper.github.io/

---

To install the VST3 plugin, copy the .vst3 bundle to:
~/Library/Audio/Plug-Ins/VST3

To install the CLAP plugin, copy the .clap bundle to:
~/Library/Audio/Plug-Ins/CLAP

You will need to create these directories yourself if they do not yet exist.

See https://github.com/free-audio/clap#hosts for instructions on how to enable
CLAP support in your DAW.

---

macOS AUv3 (from the AUv3 standalone .dmg — the .appex ships bundled inside
the .app; do not copy it around manually):

Mount the .dmg and drag the app to Applications. Launch it once — after the
first launch the system registers the AUv3 extension, and hosts (Logic Pro,
GarageBand, ...) will find the plugin.

The app is unsigned, so Gatekeeper assesses it on the first launch of every
new build:

- If the app only opens after "System Settings > Privacy & Security > Open
  Anyway", you are done.
- If it refuses even after Open Anyway while the dock icon keeps bouncing,
  macOS has locked the app in a stopped state (an AppleSystemPolicy quirk
  with quarantined ad-hoc apps). Reboot and launch it once right after the
  login — that clears the lock. Removing the quarantine attribute first
  (`xattr -dr com.apple.quarantine "/Applications/Resname Spectral Compressor
  AUv3.app"`) is worth a try, but on current macOS versions the stuck
  launched-in-stopped-state lock can survive that strip, in which case only
  the reboot clears it.
