# Compact startup windows

Desktop windows start hidden and restore their geometry before becoming visible. No first-start maximize/fullscreen action is introduced.

| Entry | Previous default / minimum | New default / minimum (logical pixels) |
| --- | --- | --- |
| `src-tauri/tauri.conf.json` | 1440 × 920 / 1024 × 720 | 1120 × 720 / 800 × 520 |

First use caps the window to 85% of the available desktop work area after a 24 logical pixel edge margin. If that cap would cut into the configured minimum, controls take priority and the minimum is used up to the available work area. Native title bars and borders are included. Monitor coordinates may be negative; the target monitor scale converts content dimensions to physical pixels.

Normal window geometry is saved atomically in the existing application config directory, per window label. Content dimensions are logical pixels; desktop positions and their original scale are recorded separately. Valid saved geometry, including windows placed against a desktop edge or larger than the new default, is retained. On a removed or smaller monitor it is fitted to the monitor with the largest remaining overlap, then the current/primary monitor. Maximized, fullscreen and minimized windows do not overwrite the saved normal geometry. Explicit immersive modes remain available. Mobile geometry is excluded.

Validation on macOS arm64: all new geometry modules and the explicit Search Workbench surface switch were compiled against the real Tauri 2.12.1 APIs in an isolated harness; seven geometry/state regression cases cover first use, user placement, removed monitors, small work areas, configured minimums and corrupt saved data. `git diff --check` passed. This is API/module validation, not a full application build or native-window visual acceptance.

The frontend production build passed after removing the root fixed desktop minimum. Small work areas retain scroll access. Annotation windows also use 1120 × 720 with an 800 × 520 minimum; the existing background-task utility stays 520 × 680 and is work-area fitted. Twelve native chrome source tests passed.

Limits: full per-application Cargo checks and physical window tests were not completed for this batch. Windows/Linux, multi-monitor hot unplug and physical DPI transitions were not visually tested. The existing Tauri 2.11.5 source cache failed internally (`UnexpectedMenuKind` is referenced but missing from its Error enum); no shared cache or lockfile was patched. The successful harness uses a separate compatible dependency resolution.
