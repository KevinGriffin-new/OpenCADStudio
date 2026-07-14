#!/usr/bin/env python3
"""Verify the vector content of the round-trip PDF against its source plan-JSON.

Part of the plat2json -> LS_IMPORTPLAN -> EXPORTPDF round-trip check (see
tools/ROUNDTRIP.md). Reads the plan-JSON fixture and the PDF the test exported,
then asserts:

  (a) every expected straight segment (plan `lines` + straight polyline
      segments) is drawn,
  (b) every expected arc (plan `arcs` + polyline bulge segments) and circle is
      drawn (tessellated segments or beziers both accepted),
  (c) ALL geometry is consistent with a single similarity transform of the
      source plan coordinates (fitted from straight segments, residuals
      checked everywhere),
  (d) reports whether text was emitted (PDF text objects and/or vector ink
      near the text anchor) — reported, not failed.

Exit code 0 = all geometry checks pass, 1 = failure.

Usage:
  python tools/check_roundtrip_pdf.py <plan.json> <roundtrip.pdf> [--tol-pt 0.5]
"""

import argparse
import json
import math
import sys

import fitz  # PyMuPDF
import numpy as np


def load_plan(path):
    plan = json.load(open(path))
    straight = []  # (x1, y1, x2, y2)
    for x1, y1, x2, y2, _layer in plan.get("lines", []):
        straight.append((x1, y1, x2, y2))
    arcs = []  # (cx, cy, r, a0_rad, a1_rad) CCW
    for cx, cy, r, a0, a1, _layer in plan.get("arcs", []):
        arcs.append((cx, cy, r, math.radians(a0), math.radians(a1)))
    circles = [(cx, cy, r) for cx, cy, r, _layer in plan.get("circles", [])]
    texts = [(x, y, s) for x, y, s, _style in plan.get("texts", [])]
    for pl in plan.get("polylines", []):
        pts = pl[:-1]
        for i in range(len(pts) - 1):
            x1, y1 = pts[i][0], pts[i][1]
            bulge = pts[i][2] if len(pts[i]) > 2 else 0.0
            x2, y2 = pts[i + 1][0], pts[i + 1][1]
            if bulge == 0.0:
                straight.append((x1, y1, x2, y2))
            else:
                sweep = 4.0 * math.atan(bulge)
                chord = math.hypot(x2 - x1, y2 - y1)
                r = chord / (2.0 * abs(math.sin(sweep / 2.0)))
                mx, my = (x1 + x2) / 2.0, (y1 + y2) / 2.0
                # CCW (bulge > 0): center sits to the LEFT of travel, at the
                # signed apothem r*cos(sweep/2).
                nx, ny = -(y2 - y1) / chord, (x2 - x1) / chord  # left normal
                apo = r * math.cos(sweep / 2.0) * math.copysign(1.0, sweep)
                cx, cy = mx + nx * apo, my + ny * apo
                a0 = math.atan2(y1 - cy, x1 - cx)
                arcs.append((cx, cy, r, a0, a0 + sweep))
    return straight, arcs, circles, texts


def sample_bezier(p0, p1, p2, p3, n=24):
    t = np.linspace(0.0, 1.0, n)[:, None]
    p0, p1, p2, p3 = (np.array([p.x, p.y]) for p in (p0, p1, p2, p3))
    return ((1 - t) ** 3 * p0 + 3 * (1 - t) ** 2 * t * p1
            + 3 * (1 - t) * t**2 * p2 + t**3 * p3)


def pdf_segments(page):
    """All drawn ink on the page as an (N, 4) array of segments (PDF points)."""
    segs = []
    n_lines = n_curves = n_rects = 0
    for d in page.get_drawings():
        for item in d["items"]:
            kind = item[0]
            if kind == "l":
                p1, p2 = item[1], item[2]
                segs.append((p1.x, p1.y, p2.x, p2.y))
                n_lines += 1
            elif kind == "c":
                pts = sample_bezier(item[1], item[2], item[3], item[4])
                for a, b in zip(pts[:-1], pts[1:]):
                    segs.append((a[0], a[1], b[0], b[1]))
                n_curves += 1
            elif kind == "re":
                r = item[1]
                q = [(r.x0, r.y0), (r.x1, r.y0), (r.x1, r.y1), (r.x0, r.y1)]
                for a, b in zip(q, q[1:] + q[:1]):
                    segs.append((a[0], a[1], b[0], b[1]))
                n_rects += 1
            elif kind == "qu":
                q = item[1]
                pts = [q.ul, q.ur, q.lr, q.ll]
                for a, b in zip(pts, pts[1:] + pts[:1]):
                    segs.append((a.x, a.y, b.x, b.y))
    return np.array(segs, float), n_lines, n_curves, n_rects


def min_dist_to_segments(points, segs):
    """Min distance from each point (M,2) to any segment in segs (N,4)."""
    a = segs[:, 0:2][None, :, :]          # (1, N, 2)
    b = segs[:, 2:4][None, :, :]
    p = points[:, None, :]                # (M, 1, 2)
    ab = b - a
    denom = (ab**2).sum(-1)
    denom[denom == 0.0] = 1e-30
    t = ((p - a) * ab).sum(-1) / denom
    t = np.clip(t, 0.0, 1.0)
    proj = a + t[..., None] * ab
    return np.sqrt(((p - proj) ** 2).sum(-1)).min(axis=1)


class Similarity:
    """w = a*z + b over complex points; conj=True uses w = a*conj(z) + b."""

    def __init__(self, a, b, conj):
        self.a, self.b, self.conj = a, b, conj

    def apply(self, pts):
        z = pts[:, 0] + 1j * pts[:, 1]
        if self.conj:
            z = np.conj(z)
        w = self.a * z + self.b
        return np.stack([w.real, w.imag], axis=1)

    @property
    def scale(self):
        return abs(self.a)


def sample_straight(seg, n=8):
    x1, y1, x2, y2 = seg
    t = np.linspace(0.0, 1.0, n)[:, None]
    return (1 - t) * np.array([x1, y1]) + t * np.array([x2, y2])


def fit_transform(straight, segs, tol):
    """RANSAC-lite: hypothesize a similarity from (source seg, drawn seg)
    pairs, score by coverage of all expected straight segments, return the
    best hypothesis refined by least squares."""
    lengths = np.hypot(segs[:, 2] - segs[:, 0], segs[:, 3] - segs[:, 1])
    cand = segs[lengths >= 0.2 * lengths.max()]
    src_samples = [sample_straight(s, 8) for s in straight]

    best, best_score = None, -1.0
    for s in straight:
        zp = complex(s[0], s[1])
        zq = complex(s[2], s[3])
        for t in cand:
            for (u, v) in (((t[0], t[1]), (t[2], t[3])),
                           ((t[2], t[3]), (t[0], t[1]))):
                wu, wv = complex(*u), complex(*v)
                for conj in (False, True):
                    p, q = (np.conj(zp), np.conj(zq)) if conj else (zp, zq)
                    if q == p:
                        continue
                    a = (wv - wu) / (q - p)
                    if abs(a) == 0.0:
                        continue
                    T = Similarity(a, wu - a * p, conj)
                    score = 0.0
                    for pts in src_samples:
                        d = min_dist_to_segments(T.apply(pts), segs)
                        score += (d <= tol).mean()
                    if score > best_score:
                        best_score, best = score, T
    if best is None:
        return None, 0.0

    # Refine: least squares over densely sampled inlier points is overkill —
    # endpoint correspondences of covered source segments are enough, and the
    # hypothesis already comes from exact endpoints. Keep the hypothesis.
    return best, best_score / len(src_samples)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("plan")
    ap.add_argument("pdf")
    ap.add_argument("--tol-pt", type=float, default=0.5,
                    help="tolerance for straight segments, PDF points")
    ap.add_argument("--tol-curve-pt", type=float, default=1.0,
                    help="tolerance for tessellated/bezier curves, PDF points")
    args = ap.parse_args()

    straight, arcs, circles, texts = load_plan(args.plan)
    doc = fitz.open(args.pdf)
    page = doc[0]
    segs, n_lines, n_curves, n_rects = pdf_segments(page)
    print(f"plan: {len(straight)} straight segs, {len(arcs)} arcs, "
          f"{len(circles)} circles, {len(texts)} texts")
    print(f"pdf:  {n_lines} line items, {n_curves} bezier items, "
          f"{n_rects} rects -> {len(segs)} segments total")
    if len(segs) == 0:
        print("FAIL: no vector content in PDF")
        return 1

    T, coverage = fit_transform(straight, segs, args.tol_pt)
    if T is None:
        print("FAIL: could not fit a similarity transform")
        return 1
    print(f"transform: scale={T.scale:.6f} pt/unit, rotation="
          f"{math.degrees(math.atan2(T.a.imag, T.a.real)):.3f} deg, "
          f"reflected={T.conj}, straight-coverage={coverage * 100:.1f}%")

    failures = []

    # (a) + (c) straight segments under the single transform
    worst = 0.0
    for i, s in enumerate(straight):
        pts = T.apply(sample_straight(s, 16))
        d = min_dist_to_segments(pts, segs)
        worst = max(worst, d.max())
        ok = d.max() <= args.tol_pt
        print(f"  line {i}: ({s[0]},{s[1]})->({s[2]},{s[3]}) "
              f"max-resid={d.max():.4f}pt {'OK' if ok else 'FAIL'}")
        if not ok:
            failures.append(f"straight segment {i} not covered (max {d.max():.3f}pt)")

    # (b) arcs (incl. the bulge arc) and circles by dense sampling
    for i, (cx, cy, r, a0, a1) in enumerate(arcs):
        t = np.linspace(a0, a1, 64)
        pts = np.stack([cx + r * np.cos(t), cy + r * np.sin(t)], axis=1)
        d = min_dist_to_segments(T.apply(pts), segs)
        ok = d.max() <= args.tol_curve_pt
        print(f"  arc {i}: c=({cx:.3f},{cy:.3f}) r={r:.3f} "
              f"sweep={math.degrees(a1 - a0):.1f}deg "
              f"max-resid={d.max():.4f}pt {'OK' if ok else 'FAIL'}")
        if not ok:
            failures.append(f"arc {i} not covered (max {d.max():.3f}pt)")

    for i, (cx, cy, r) in enumerate(circles):
        t = np.linspace(0.0, 2.0 * math.pi, 96)
        pts = np.stack([cx + r * np.cos(t), cy + r * np.sin(t)], axis=1)
        d = min_dist_to_segments(T.apply(pts), segs)
        ok = d.max() <= args.tol_curve_pt
        print(f"  circle {i}: c=({cx},{cy}) r={r} "
              f"max-resid={d.max():.4f}pt {'OK' if ok else 'FAIL'}")
        if not ok:
            failures.append(f"circle {i} not covered (max {d.max():.3f}pt)")

    print(f"  worst straight-segment residual: {worst:.4f}pt "
          f"({worst / T.scale:.6f} source units)")

    # (d) text: PDF text objects, else vector ink near the anchor (reported)
    words = page.get_text("words")
    for i, (x, y, s) in enumerate(texts):
        as_text = any(s.lower() in w[4].lower() or w[4].lower() in s.lower()
                      for w in words if w[4].strip())
        anchor = T.apply(np.array([[x, y]], float))[0]
        near = np.hypot(segs[:, 0] - anchor[0], segs[:, 1] - anchor[1])
        # glyphs start at/above the anchor; look within ~ text height * n
        ink = (near <= 12.0 * max(T.scale, 1.0)).any()
        print(f"  text {i} ({s!r}): pdf-text={'yes' if as_text else 'no'}, "
              f"vector-ink-near-anchor={'yes' if ink else 'no'} (reported, not failed)")

    if failures:
        print("FAIL:", "; ".join(failures))
        return 1
    print("PASS: all plan geometry present in the PDF under one similarity transform")
    return 0


if __name__ == "__main__":
    sys.exit(main())
