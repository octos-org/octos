#!/usr/bin/env python3
"""Report actual raster dimensions and scale needed for a target export.

Requires Pillow. File dimensions alone cannot establish native generation size.
Pass untouched source files or raw panel crops, not their upscaled derivatives.
"""
import argparse
import json
from pathlib import Path

from PIL import Image


def size_arg(value):
    try:
        width, height = map(int, value.lower().split('x'))
        if width <= 0 or height <= 0:
            raise ValueError
        return width, height
    except ValueError as exc:
        raise argparse.ArgumentTypeError('Expected positive WIDTHxHEIGHT.') from exc


def inspect(paths, target):
    rows = []
    for path in paths:
        with Image.open(path) as im:
            width, height = im.size
            fmt = im.format
        sx, sy = target[0] / width, target[1] / height
        rows.append({
            'file': str(path.resolve()), 'format': fmt,
            'actualPixels': [width, height], 'targetPixels': list(target),
            'scaleX': round(sx, 6), 'scaleY': round(sy, 6),
            'requiresUpsampling': sx > 1 or sy > 1,
            'aspectRatioDifferenceFraction': round(abs(sx / sy - 1), 6),
            'nativeGenerationVerified': False,
        })
    return {
        'scope': 'Stored raster dimensions; generation provenance must be checked separately.',
        'files': rows,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('images', type=Path, nargs='+')
    parser.add_argument('--target', type=size_arg, default=(3840, 2160))
    parser.add_argument('--out', type=Path)
    args = parser.parse_args()
    try:
        result = inspect(args.images, args.target)
    except (OSError, ValueError) as exc:
        parser.error(str(exc))
    report = json.dumps(result, ensure_ascii=False, indent=2)
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(report + '\n')
    print(report)


if __name__ == '__main__':
    main()
