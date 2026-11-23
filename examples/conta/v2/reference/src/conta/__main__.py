import argparse
import sys
from pathlib import Path

from conta.cli import process


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(prog="conta")
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("process")
    p.add_argument("in_dir", type=Path)
    p.add_argument("out_dir", type=Path)
    p.add_argument("--format", choices=["saga", "efactura"], default="saga")
    p.add_argument("--summary", action="store_true")
    p.add_argument("--strict", action="store_true")
    a = ap.parse_args(argv)
    process(a.in_dir, a.out_dir, a.format, summary=a.summary, strict=a.strict)
    return 0


if __name__ == "__main__":
    sys.exit(main())
