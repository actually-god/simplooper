# `vendor/vst2` — provenance and patches

Vendored from **vst-rs 0.4.0** (https://github.com/rustaudio/vst-rs, MIT,
`LICENSE` ships next to this file). The upstream repo is archived; the crate
is still the de-facto VST2 host API for Rust, but three things blocked using
it directly from slooper, so we vendor and patch:

1. **Entry symbol.** Upstream only resolves `VSTPluginMain`. DPF-built
   plugins (and various older ones) export only `main`. Patched to try both
   (`src/host.rs`, `PluginLoader::load`).
2. **libloading 0.8.** Upstream pins 0.7; the VST3 stack uses 0.8. One
   version of `libloading` in the process avoids two `Library` types.
3. **State chunks.** Added `PluginInstance::get_chunk` / `set_chunk`
   (VST2.4 `effGetChunk`/`effSetChunk`, program chunk) so plugin state can
   be persisted in slooper projects.

Renamed the crate to `vst2` to avoid any ambiguity with the unrelated
published `vst` crate. Plugin-authoring machinery (`plugin_main!` etc.) is
unused by slooper but kept as-is to minimize diff; examples and dev-deps
were removed.

To re-sync with upstream, diff against the registry copy of vst 0.4.0 and
re-apply the three patches above (they are all marked `Vendored patch`).
