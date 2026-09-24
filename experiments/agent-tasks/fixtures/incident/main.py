import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))

from loader import load_rows


def main():
    load_rows(HERE / "rows.jsonl")


if __name__ == "__main__":
    main()
