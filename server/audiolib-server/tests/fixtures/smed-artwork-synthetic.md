# Synthetic SMED artwork fixture

These two tiny files were generated entirely for regression tests. They contain
no BOOM image, source audio, real library tags, paths, or user data.

- `smed-artwork-synthetic.png`: an 8 by 4 RGBA gradient generated from pixel
  coordinates. SHA256: `7ef5b7e12004239bd30c5ee654124e594552cc92c1dadaf93df029c43304b844`.
- `smed-artwork-synthetic.wav`: 32 zero PCM samples at 48 kHz, mono, 16 bit; a
  UTF-16LE Soundminer SMED block declares `Synthetic SMED Library`, synthetic
  description/custom text, and the PNG encoded as `Picture type="NSData"` base64.
  It has no ID3 chunk. SHA256:
  `985695f14c4f3b8ceee52fffb55d497c9dce8f6945099a55e8b069ac658dc673`.

The SMED layout uses the same public legacy Blowfish ECB compatibility key and
big-endian unpadded-length header as the audio extractor's existing synthetic
tests. Fixture generation used Python's standard PNG/zlib construction and an
already installed Blowfish implementation; tests consume the saved bytes and
require no Python installation or added Cargo dependency.

The ignored real-tag test accepts only an explicitly supplied private, small,
SMED-only blank-audio WAV through `AUDIOLIB_SMED_SERVER_FIXTURE`. It copies that
input into a temporary test library and checks the original bytes and mtime;
actual vendor images or encrypted tags must never be added to this directory.
