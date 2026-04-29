# Crossfade Math — Explanation & Code

This document explains the mathematics behind the crossfade (dissolve) transition
used between images in this project, and how the formula maps directly to the Rust code.

---

## What is a Crossfade?

A crossfade (also called a **linear dissolve**) blends two images together over a number of frames.
The first image fades out while the second image fades in simultaneously.

Visually:

```
Image A  ████████████░░░░░░░░░░░░░░░░░░░░
Image B  ░░░░░░░░░░░░░░░░████████████████
Output   ████████████▓▓▓▓▓▓▓▓████████████
              hold    ← fade →    hold
```

---

## The Blending Formula

For each pixel at position $(x, y)$, the output pixel at fade frame $f$ is:

$$\text{out}(x,y) = (1 - \alpha) \cdot A(x,y) + \alpha \cdot B(x,y)$$

Where:
- $A(x,y)$ — pixel value from the **current** (outgoing) image
- $B(x,y)$ — pixel value from the **next** (incoming) image
- $\alpha \in (0, 1)$ — the blend weight, increases over the fade

This is called **linear interpolation** (lerp):

$$\text{lerp}(A, B, \alpha) = A + \alpha \cdot (B - A) = (1-\alpha) \cdot A + \alpha \cdot B$$

When $\alpha = 0$, the output is fully $A$.  
When $\alpha = 1$, the output is fully $B$.  
When $\alpha = 0.5$, the output is a 50/50 mix.

---

## How Alpha Steps Through the Fade

Given `fade_frames` total frames in the transition, frame index $f$ goes from $0$ to `fade_frames - 1`.

The alpha formula used in the code is:

$$\alpha_f = \frac{f + 1}{\text{fade\_frames} + 1}$$

### Why `+ 1` in both numerator and denominator?

This ensures alpha **never reaches 0 or 1** within the fade loop — the pure $A$ and pure $B$ frames
are the hold frames on either side. The fade only produces the in-between blend frames.

**Example with `fade_frames = 3`:**

| Frame index $f$ | $\alpha = \frac{f+1}{3+1}$ | Output |
|:---:|:---:|---|
| 0 | $\frac{1}{4} = 0.25$ | 75% A + 25% B |
| 1 | $\frac{2}{4} = 0.50$ | 50% A + 50% B |
| 2 | $\frac{3}{4} = 0.75$ | 25% A + 75% B |

Pure A (α=0) is the last hold frame before the fade.  
Pure B (α=1) is the first hold frame after the fade.

The full sequence for one image transition looks like:

```
... [A][A][A] | [0.25][0.50][0.75] | [B][B][B] ...
     hold       ←  fade frames →      hold
      α=0            α→1               α=1
```

---

## Calculating the Number of Fade Frames

```rust
let fps: f64 = 30.0;
let seconds_fade: f64 = 0.1;

let fade_frames = (fps * seconds_fade).round() as i32;
// = (30.0 * 0.1).round() = 3 frames
```

$$\text{fade\_frames} = \text{round}(\text{fps} \times \text{seconds\_fade})$$

At 30 fps with 0.1s fade: $30 \times 0.1 = 3$ frames.

Similarly for hold frames:

```rust
let seconds_per_image: f64 = 0.9;
let hold_frames = (fps * seconds_per_image).round() as i32;
// = (30.0 * 0.9).round() = 27 frames
```

So each image occupies: $27 \text{ hold} + 3 \text{ fade} = 30$ frames = **exactly 1 second**.

---

## The Code

```rust
// Iterate over each fade frame (f = 0, 1, 2, ..., fade_frames-1)
for f in 0..fade_frames {
    // Alpha increases from just above 0 to just below 1
    let alpha = (f as f64 + 1.0) / (fade_frames as f64 + 1.0);

    let mut blended = Mat::default();

    // OpenCV add_weighted implements: out = src1*alpha1 + src2*alpha2 + gamma
    core::add_weighted(
        &current, 1.0 - alpha,   // A × (1 - α)
        &next,    alpha,          // B × α
        0.0,                      // gamma (brightness offset, 0 = none)
        &mut blended,
        -1,                       // output depth = same as input
    )?;

    ffmpeg_stdin.write_all(blended.data_bytes()?)?;
}
```

### `add_weighted` Signature

OpenCV's `add_weighted` computes:

$$\text{dst}(x,y) = \alpha \cdot \text{src1}(x,y) + \beta \cdot \text{src2}(x,y) + \gamma$$

In our call:
- `src1` = `current` (image A), weight = `1.0 - alpha`
- `src2` = `next` (image B), weight = `alpha`
- `gamma` = `0.0` (no brightness shift)

This is applied **independently to every channel (B, G, R)** of every pixel.

For a single pixel with channels $(B, G, R)$:

$$B_{\text{out}} = (1-\alpha) \cdot B_A + \alpha \cdot B_B$$
$$G_{\text{out}} = (1-\alpha) \cdot G_A + \alpha \cdot G_B$$
$$R_{\text{out}} = (1-\alpha) \cdot R_A + \alpha \cdot R_B$$

---

## Full Timeline for the Whole Video

For $N$ images with `hold_frames` $= H$ and `fade_frames` $= F$:

```
Image 1        Image 2        Image 3
│←── H ──│← F →│←── H ──│← F →│←── H ──│
[1][1]...[1][blend][2][2]...[2][blend][3][3]...[3]
```

Total frames:

$$\text{total\_frames} = N \times H + (N - 1) \times F$$

For 20 images, H=27, F=3:

$$= 20 \times 27 + 19 \times 3 = 540 + 57 = 597 \text{ frames}$$

Total video duration:

$$\frac{597}{30} = 19.9 \text{ seconds}$$

---

## Why Linear Interpolation?

Linear crossfade is the simplest and most common transition. The alternatives are:

| Type | Formula | Feel |
|---|---|---|
| **Linear** (used here) | $\alpha = t$ | Constant rate — simple, clean |
| **Ease-in-out** | $\alpha = 3t^2 - 2t^3$ (smoothstep) | Slow → fast → slow — cinematic |
| **Ease-in** | $\alpha = t^2$ | Slow start, fast end |
| **Ease-out** | $\alpha = 1-(1-t)^2$ | Fast start, slow end |

Where $t = \frac{f+1}{\text{fade\_frames}+1}$ (same as our alpha).

To implement smoothstep instead of linear, replace the alpha line:

```rust
// Linear (current):
let alpha = (f as f64 + 1.0) / (fade_frames as f64 + 1.0);

// Smoothstep (cinematic feel):
let t = (f as f64 + 1.0) / (fade_frames as f64 + 1.0);
let alpha = t * t * (3.0 - 2.0 * t);
```

Smoothstep formula: $\alpha = 3t^2 - 2t^3$

| $t$ | linear $\alpha$ | smoothstep $\alpha$ |
|:---:|:---:|:---:|
| 0.25 | 0.25 | 0.156 |
| 0.50 | 0.50 | 0.500 |
| 0.75 | 0.75 | 0.844 |
