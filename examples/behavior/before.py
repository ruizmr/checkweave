"""Line-protocol target: one JSON input on stdin, one JSON output on stdout."""
import json
import sys

value = json.load(sys.stdin)
print(json.dumps({"total": sum(value["values"])}))
