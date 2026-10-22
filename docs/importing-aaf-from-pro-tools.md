# Importing an AAF export from Pro Tools

SoundCraft imports a public interchange export. It does not open `.ptx` or `.ptf` session files
and does not load AAX plug-ins or their proprietary state.

## Export from Pro Tools

Create a new empty folder for the export. **All files must sit at the same level in that one
folder**: the `.aaf`, the `.mid`, the `.txt` report and every WAV file. Do not leave the WAV files
in a subfolder such as `Converted Audio Files`; if Pro Tools created one, move the WAV files out of
it into the export folder.

1. Export the session as **AAF**, choosing external audio rather than an embedded-media-only
   workflow when that choice is available. Put the converted audio files in the export folder.
2. Export MIDI as a Standard MIDI File (`.mid`).
3. Export a session report as text (`.txt`). This records the sample rate, bit depth, timecode,
   track names, clip names, mute state, and plug-in listing for migration reference.
4. Confirm the export folder contains the `.aaf`, `.mid`, `.txt` and the WAV files, all directly
   in it.

Do not rename or delete individual WAV files after creating the AAF. The AAF refers to its
external media by file name, and SoundCraft looks for each WAV in the AAF's own folder.

## Import in SoundCraft

Use the command-line importer with all files from the same export:

```text
soundcraft-cli import-aaf "Song.aaf" "Song.scraft" ^
  --midi "Song.mid" ^
  --session-report "Song.txt"
```

WAV files are found automatically in the folder that contains the AAF. `--media-dir DIR` is only
an override for a different folder; the supported layout is everything in one folder.

The importer creates a `.soundcraft` session and writes its managed audio into the destination
session's `Audio Files` directory. Keep the original export until you have checked the imported
session.

## Compatibility note

AAF is an interchange standard, but applications do not all write the same optional object-model
extensions. SoundCraft rejects an AAF whose composition or external-media metadata it cannot
interpret; it never creates a partial session while reporting success. If an export is rejected,
keep the AAF, MIDI, WAV files, and session report together and include the exact error when
reporting the compatibility issue.

## What transfers

- AAF timeline tracks, clip placement, clip duration, and references to external WAV media.
- WAV/BWF audio, converted to the SoundCraft session's sample rate when required.
- MIDI notes and tempo information from the separately exported MIDI file.
- Session-report metadata that can be represented by SoundCraft, including the session name,
  sample rate, bit depth, timecode, track names, and clip mute state.

## What needs manual migration

- AAX plug-ins, plug-in presets, and AAX plug-in state.
- Plug-in automation, advanced routing, clip effects, and features without an interchange
  representation.
- Media that was omitted from the AAF export, moved after export, or stored in a format
  SoundCraft cannot decode.

The session report preserves the plug-in list as migration information. Recreate each plug-in with
an available VST3, Audio Unit, or CLAP equivalent, or use rendered stems where preserving the
original AAX processing is necessary.
