"""An implementation change that preserves the fixture's integer behavior."""
import json
import sys

value = json.load(sys.stdin)
total = 0
for number in value["values"]:
    total += number
print(json.dumps({"total": total}))
