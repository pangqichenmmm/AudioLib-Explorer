# Generated MP3 test fixtures

These short sine waves were generated locally with FFmpeg and libmp3lame. They
contain no downloaded recordings or third-party audio. The three files total
6,034 bytes. Tests construct ID3 and malformed-file variants in memory.

The source commands used FFmpeg N-119294-g7684243fbe-20250417. Run these commands
from this directory with a build providing the libmp3lame encoder:

```powershell
ffmpeg -hide_banner -loglevel error -f lavfi -i 'sine=frequency=440:sample_rate=48000:duration=0.125' -ac 2 -c:a libmp3lame -b:a 128k -write_xing 1 -id3v2_version 0 -map_metadata -1 -y cbr-info.mp3
ffmpeg -hide_banner -loglevel error -f lavfi -i 'sine=frequency=997:sample_rate=44100:duration=0.2' -ac 1 -c:a libmp3lame -q:a 3 -write_xing 1 -id3v2_version 0 -map_metadata -1 -y vbr-xing.mp3
ffmpeg -hide_banner -loglevel error -f lavfi -i 'sine=frequency=220:sample_rate=24000:duration=0.1' -ac 1 -c:a libmp3lame -b:a 48k -write_xing 0 -id3v2_version 0 -map_metadata -1 -y cbr-no-xing.mp3
```

`cbr-info.mp3` has 8 MPEG frames including its Info frame, 48 kHz stereo, and
6,000 presentation samples after encoder/decoder delay and padding. `vbr-xing.mp3`
has 10 MPEG frames including Xing, 44.1 kHz mono, and 8,820 presentation samples.
`cbr-no-xing.mp3` has 7 MPEG-2 frames at 24 kHz mono, giving 4,032 coded samples;
without a gapless tag its original encoder delay and padding cannot be inferred.
