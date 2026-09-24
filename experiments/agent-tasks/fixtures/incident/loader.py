import json
from pathlib import Path

from normalize import region_code


def load_rows(path):
    path = Path(path)
    rows = []
    with path.open() as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            rows.append(json.loads(line))
    loaded = []
    for row in rows:
        try:
            loaded.append({"id": row["id"], "code": region_code(row)})
        except Exception as exc:
            raise RuntimeError(f"report failed for {row.get('id')}") from exc
    return loaded
