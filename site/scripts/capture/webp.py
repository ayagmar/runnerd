"""Converts capture's PNG screenshots to the WebP files the site imports: webp.py FROM_DIR TO_DIR."""
import sys
from pathlib import Path

from PIL import Image

source, target = Path(sys.argv[1]), Path(sys.argv[2])
for png in sorted(source.glob("*.png")):
    Image.open(png).convert("RGB").save(target / f"{png.stem}.webp", "WEBP", quality=82, method=6)
    print(png.stem)
