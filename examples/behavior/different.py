"""An intentional change: negative contributions are now silently dropped."""
import json
import sys

value = json.load(sys.stdin)
print(json.dumps({"total": sum(n for n in value["values"] if n >= 0)}))
