# Smooth-resize investigation (wgpu + winit + egui, DX12)

Goal: find out why content stretches/jitters while the window is being resized, and whether it is
egui or our swapchain handling.

Reference article (best-practices checklist for DXGI resize flicker):
<https://learn.microsoft.com/en-us/answers/questions/5876868/how-to-fix-size-flickering-on-my-d2d-application>

**Status: not solved.** The stretch is reduced and one real bug was fixed, but a residual jitter when
resizing to the right/down remains. The last round of measurements was unreliable (see
[Measurement caveats](#measurement-caveats)), so treat the numbers below as indicative only.

## What the code does now

`src/main.rs` (egui disabled via `const ENABLE_EGUI: bool = false`, code kept):

- Two test shapes drawn with raw wgpu, no shaders: a 50x50 red square at pixel (400, 200) and a
  blue bar 20% of the width x 50 at (100, 300), both via `copy_texture_to_texture` from a
  pre-filled texture into the swapchain image (surface usage includes `COPY_DST`). Fixed pixel
  offsets from the top-left make any scaling of the presented frame visible.
- egui is laid out for the size of the acquired texture, not `window.inner_size()` (egui-winit
  reads the live window size, which can be newer than the texture).
- On `Resized`: reconfigure the surface, then render and present a frame immediately.
- While resizing, present mode is `AutoNoVsync`; it returns to `AutoVsync` 150 ms after the last
  resize (`end_resize_if_settled`, called from `about_to_wait`).
- `configure_surface` **drops** the held, never-drawn surface texture instead of presenting it.
  Presenting it put a black frame on screen on every resize step. (Dropping relies on the
  pinned wgpu fork rev, `DouglasDwyer/wgpu` `ab4324ac`; not checked against stock wgpu 30.0.1.)
- Normal rendering is unchanged: `about_to_wait` renders then acquires the next texture;
  `RedrawRequested` is the fallback.

## Findings

1. **egui layout mismatch (fixed).** egui-winit takes the screen size from the live
   `window.inner_size()`. `get_current_texture()` blocks on the DX12 frame-latency waitable and the
   window keeps resizing meanwhile, so egui was laid out for a different size than the texture it
   drew into (logged on every frame: e.g. egui 584x461 vs texture 800x600). Pinning
   `raw_input.screen_rect` to the acquired texture removed all 300 logged mismatches.

2. **The squares stretch, so it is not (only) egui.** With squares at fixed offsets they visibly
   stretched with egui disabled. The cause is in the present path: wgpu-hal creates the DX12
   swapchain with `DXGI_SCALING_STRETCH` (`wgpu-hal/src/dx12/mod.rs`, ~line 1543), and DWM rescales
   whatever frame was last presented to the window's *current* size.

3. **Black frame on every resize (fixed).** `configure_surface` presented the acquired but
   undrawn texture. In an instant-resize timeline test the screen showed: stretched old frame at
   t=0, black frame at ~15 ms, correct frame at ~35 ms. Dropping the texture removes the black
   frame.

4. **`DXGI_SCALING_NONE` removes stretch.** Temporarily changing wgpu-hal's scaling to `NONE`
   (reverted afterwards): 0 deviating frames in 2 runs of a continuous drag, vs 37/96, 78/139, 13/73
   with `STRETCH`. Not applied: the request was a fix that works regardless of scaling mode, and
   it needs a change in the wgpu fork.

5. **`desired_maximum_frame_latency` 1 vs 2 makes no difference** (70/140, 41/98, 66/132 deviating
   frames vs 9/187, 51/120, 72/138 baseline).

6. **Timing of a resize step (instrumented, debug build).** `Resized` events arrive ~33 ms apart
   (one per two vsyncs); `RedrawRequested` was not delivered during the drag. Each `Resized`
   handler took ~5.5 ms: ~3.7 ms in `surface.configure` (`ResizeBuffers`) and ~1.8 ms to render and
   present. Disabling the wgpu debug layers cut that to ~4.5 ms but did not change the stale
   rate.

7. **There are two opposite races, and we cannot order them.** Windows calls `WM_SIZE`
   (`Resized`) from inside the `SetWindowPos` that resizes the window, and DWM recomposites using
   the last presented buffer:
   - presenting after `Resized` (original code) -> old buffer stretched into the new rect
     (square at x = 400..404, i.e. content grown);
   - presenting from inside the `Resized` handler (current code) -> in a real drag the new buffer
     is sometimes composited into the *old* rect first (square at x = 397..400, i.e. content
     shrunk). This matches the reported "jitters when resizing right/down".
   In an instant resize triggered from another process (no modal loop), presenting inside the
   handler with `NoVsync` + dropped texture gave 0 stale frames; in a modal drag it did not
   reliably. A vsync-paced present inside the handler was much worse (155/162, 190/191 frames
   off) because it is queued for the next vsync and loses the race.

## Article checklist vs this code

The accepted-answer advice (flip-model swapchain, resize on `WM_SIZE`, release back-buffer
references before `ResizeBuffers`, recreate render targets after, keep a device resource alive,
no window-style changes during resize, redraw immediately after `ResizeBuffers`):

| Practice | Status |
|---|---|
| `ResizeBuffers` on `WM_SIZE`, flip model | done (`surface.configure` on `Resized`; swapchain is `FLIP_DISCARD`) |
| Release back-buffer refs first | done (held texture dropped; per-frame view dropped at end of `render`); wgpu handles the rest |
| Recreate render target after resize | n/a (views are created per frame from the acquired texture) |
| Keep a resource alive across resize | n/a (the wgpu device/queue/textures persist) |
| No window-style changes during resize | done (none) |
| Redraw immediately after `ResizeBuffers` | done (render+present inside `Resized`) |

The article itself only says to *minimize* how long a stretched old frame is visible; it does not
claim to eliminate it. So none of its practices is missing here, and they do not explain the
remaining right/down jitter. The article uses a DirectComposition swapchain
(`CreateSwapChainForDirectComposition`); we use an HWND swapchain.

## Measurement caveats

- Harness: PowerShell + C# script driving a real mouse drag (`SetCursorPos` + `mouse_event`) on a
  window corner/edge, screenshotting the desktop in a loop and measuring the red square. Scripts
  were kept in the session scratchpad and are **not** committed.
- Results were noisy and drifted over time: the same binary gave 0/560 deviating frames in one
  session and ~90/140 later. Treat single numbers as indicative.
- The last variant comparison (`RZ=resized|none|redraw` in an env-var experiment, now removed)
  was invalid: the harness read `GetClientRect`, which can disagree with what DWM has composed, and
  picked up the window's red border as the "square". A rewrite measuring relative to the green
  region on screen was written but never run.

## Ideas not yet tried

- Make DX12 swapchain scaling configurable in the wgpu fork and use `DXGI_SCALING_NONE`
  (verified to remove stretch in the continuous-drag test).
- Present from `Resized` *and* again after the rect commit, e.g. from `about_to_wait`
  (`about_to_wait` does run during the modal drag: ~3 acquires per `Resized`).
- DirectComposition swapchain (as in the article) so the committed visual tree, not the HWND
  rect, decides what DWM shows; no stretch, but newly exposed area is empty until the next frame.
- Re-run the A/B comparison with the corrected harness, on a quiet machine, release build.
