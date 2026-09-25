"""One JSON object on stdin, one JSON object on stdout."""
import json
import sys

value = json.load(sys.stdin)
print(json.dumps({"total": sum(value["values"])}))
