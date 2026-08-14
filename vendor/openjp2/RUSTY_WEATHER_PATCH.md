# Rusty Weather openjp2 patch

This is the published `openjp2` 0.6.1 source, vendored under its BSD-2-Clause
license. Rusty Weather carries one safety correction for JPEG2000 decoding:

- decoding code blocks are allocated through a zero-initializing C-style
  allocator, so they can not contain an uninitialized `std::alloc::Layout`;
- the allocation size is stored instead, and the fixed 16-byte-aligned layout
  is reconstructed only when a non-null decoded buffer is released.

The original implementation could pass a null pointer and an invalid zeroed
layout to `std::alloc::dealloc` while cleaning up valid NOAA GRIB2 payloads.
