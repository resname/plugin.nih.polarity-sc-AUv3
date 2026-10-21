# Logic Pro sidechain gate

Early gate from the AUv3 spec: run this checklist before any polish is invested.
Appex must be installed and `auval` valid first — see
[`auv3/MAC-BRINGUP.md`](MAC-BRINGUP.md).

Record each step's outcome directly below it (text and/or screenshot). A
screenshot in this doc is sufficient evidence.

## Checklist

### 1. Load in Logic Pro

Logic Pro → new empty project. Polarity-SC-Dark appears grouped under
`polarity-music` in the AU list; loads with the Vizia GUI open.

- [ ] Pass
- Evidence:

### 2. Audio sidechain

Create an instrument/Logic track playing audio; set its output to
Polarity-SC-Dark's sidechain (Logic's Sidechain dropdown on the plugin slot).

Expected: compressor reacts (use mode = Sidechain Compression or Sidechain
Matching); `auval` and Logic remain stable for a 5-minute session.

- [ ] Pass
- Evidence:

### 3. Record GUI rendering and sidechain behavior

- [ ] Recorded (screenshot pasted below is sufficient)

### 4. Logic version

Record the Logic Pro version number with the result — AUv3 bus handling
differs across Logic versions.

- Logic Pro version:

## Result

<!-- PASS: proceed (CI polish for user-only release).
     FAIL: stop. Per spec fallback ladder rung 1 = hand-rolled ObjC++ AUv3
     shell — a separate plan; do NOT roll into this one. -->

- [ ] PASS (proceed)
- [ ] FAIL (stop, fallback ladder)
