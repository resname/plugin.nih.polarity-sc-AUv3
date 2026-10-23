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
