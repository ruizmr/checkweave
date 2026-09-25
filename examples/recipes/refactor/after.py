"""Same stdin/stdout contract. Negative contributions are dropped."""
import json
import sys

value = json.load(sys.stdin)
print(json.dumps({"total": sum(item for item in value["values"] if item >= 0)}))
