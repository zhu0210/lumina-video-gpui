# SDR NV12 color spot-check

Lumina carries copy-only color metadata with Linux GStreamer NV12 frames. The
native color module owns the one 8-bit BT.601/BT.709 matrix generator and the
CPU sampler. It supports full range (0..255) and legal range (Y 16..235,
Cb/Cr 16..240), including neutral chroma code 128. Synthetic GPU/CPU checks
must agree with an independent reference within one RGB code value per
channel after rounding.

Manual spot-check:

1. Run the demo against an 8-bit BT.601 limited-range clip and a BT.709
   limited-range clip, then compare saturated red/blue and neutral-gray frames
   with a trusted player.
2. Repeat with full-range clips. Confirm legal-range black/white endpoints and
   chroma-neutral code 128 remain neutral; the expected tolerance is at most
   one RGB code value per channel.
3. Inspect the negotiated metadata once per caps generation. GPU NV12 is used
   only for BT.601/BT.709 matrix, BT.709 primaries, sRGB transfer, full/limited
   range, and centered/JPEG horizontal and vertical siting. MPEG-2/H_COSITED
   requires a -0.5 luma-pixel horizontal offset and therefore downgrades once
   to the worker's bounded CPU RGBA pool. Cosited vertical siting also uses
   that CPU path. Unknown matrix/range and HDR-like metadata publish one typed
   unsupported session error; known SDR metadata without an implemented gamut
   or transfer formula publishes the distinct unsupported-SDR result.

P010, BT.2020, PQ, HLG, HDR metadata, tone mapping, and HDR surfaces are
intentionally excluded. No lossy media fixtures are required; the automated
coverage uses synthetic NV12 byte vectors. The worker allocates exactly two
complete RGBA payloads when a CPU generation is configured; an exhausted pool
drops the incoming frame rather than allocating or blocking. GPUI only uploads
worker-produced RGBA or exact-contract NV12 and performs no CPU color
conversion or scratch allocation.
