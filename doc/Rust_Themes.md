# Appearance themes

Choose a theme from **View > Theme** (**视图 > 主题**), the first View command. The change applies immediately
and is saved with the other preferences in `settings.json`. Switching the interface
language updates the displayed theme names; the saved identifier stays the same.
**View > Language**, immediately below Theme, switches between **English** and
**简体中文** without opening Global settings.

| Identifier | English / Chinese | Presentation |
| --- | --- | --- |
| `default` | Default / 默认 | Classic player layout from `e619794`: separate Play and Pause controls, Pitch/Rate fields, and the original Waveform ruler, volume and scroll controls, with refreshed control feedback |
| `studio` | Studio / 工作室 | Redesigned player from PR #182, retaining the subsequent overview keyboard fix: animated transport controls, editable values with feedback, status display and overview navigation |
| `default-v2` | Default v2 / 默认 v2 | Studio navigation, search, library table and inspector, paired with the Classic bottom player and a coordinated charcoal and muted teal palette |

Each theme includes its workbench and player presentations as well as its color
tokens. Default v2 keeps the Default player's separate Play and Pause controls,
Pitch/Rate fields, Waveform ruler, volume and scroll controls while using the
Studio workbench above it. Studio and Default v2 use one library sidebar instead
of a separate icon navigation rail. All sounds, favorites and recent playback
remain in that sidebar; **View > Show library sidebar** reopens it when hidden,
and **Settings > Global settings > General and interface** opens language, scale,
font and cover preferences. Theme selection stays outside that dialog. All three
themes share the same platform menu bar, with the application name in the window
title and search and library navigation in the application toolbar. After opening
a top-level menu, moving the pointer over another menu switches to it. Hovering
alone does not open a closed menu; Escape, a completed command or an outside click
ends that menu session. Existing Default
and Studio player presentations remain available. Frequency colors and non-tonal
peaks are audio data, so themes do not change Spectral Peaks analysis or rendering
colors.

Default and Default v2 share refreshed Classic controls with hover, press and
release feedback, dynamic tooltips and explanations when transport is disabled.
Their separate Play/Pause buttons and Waveform layout remain intact. The transport
indicator follows confirmed engine state: reaching the end displays **Ended**,
while **Paused** means playback was paused. Play after the end restarts from the
beginning; Stop returns the transport to the beginning.

Playback, transforms, volume, zoom, selection, interface scale and font preferences
remain in the shared application state when the presentation changes.
Transient input rejection events are cleared before creating a new presentation.

Settings created before theme selection existed, missing theme values and unknown
identifiers use `default`. Existing `default` selections use the Classic player
layout. Users who prefer the Studio player can select `studio`; no library
migration is required.

## Implementation and extension

The built-in registry in
[`audiolib-core/src/config.rs`](../crates/audiolib-core/src/config.rs) defines stable
identifiers, bilingual names, workbench styles and player styles. It drives settings
validation and the View menu's theme list. The desktop adapter in
[`src/theme.rs`](../app/audiolib-desktop/src/theme.rs) applies the selected palette
and presentation to the Slint
[`Theme` global](../app/audiolib-desktop/ui/theme.slint).

Keep existing identifiers and the Classic layout contracts stable, including
separate Play/Pause controls and the Waveform ruler. Shared control feedback may
evolve without replacing that layout. Add a new registry entry and the corresponding
palette/presenter for a different layout. Workbench and player styles are
independent: a theme can reuse the Studio workbench while retaining the Classic
player. Reuse the application inputs
and callbacks so new presenters retain the same playback and Waveform contracts.
Keep theme-specific animation and visual state inside that presenter, and do not
create a second audio controller.

### Playback repaint cost

The window draws with Slint's software renderer and partial repaint, so a playback
frame costs about the area it repaints and the work behind it. A presenter keeps
the playhead cheap by following these rules:

- Keep the playhead a thin Rectangle. Avoid glows, flashes and per-frame timers.
- Do not bind values that many elements read to the playhead position. Slint marks
  every dependent element changed on each frame, even when its value is the same.
  Assign such values from a `changed` handler only when they really change, as the
  Studio waveform gradient does with its band `span`.
- Do not move or resize a clipping or translucent container on every frame when it
  holds large children: a changed clip repaints all of its children in full. Cut
  the content into small pieces, as the played-region tiles and gradient bands do.
- Give artwork behind the player as bitmaps. The software renderer rasterizes an
  SVG image again on every repaint that touches it; `theme.rs` rasterizes the
  Studio surfaces once and passes them as `Theme.studio-*` images.
- The software renderer ignores `transform-scale` and rotation. Animate width and
  height instead.

`playback_repaint_tests` plays every registered theme, compact and expanded, in
waveform and Spectral Peaks mode, and fails when playback frames keep repainting
more than a small part of the window. A new theme is covered once it is registered.

For a theme change, verify registry/index mapping, settings round trips, bilingual
Theme and Language menu actions, keyboard submenu selection, pointer switching
between open menus, dismissal behavior and representative player states. The
headless `ui_scale_tests` cover these UI behaviors. Setting
`AUDIOLIB_UI_SCALE_SNAPSHOT_DIR` saves synthetic PNGs for visual inspection,
including all registered themes in both languages and a small scaled window. Use
the checks selected by [AGENTS.md](../AGENTS.md#development-checks-by-change-scope), then the
required packaged startup and local installation workflow for application delivery.
