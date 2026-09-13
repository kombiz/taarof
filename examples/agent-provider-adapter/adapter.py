#!/usr/bin/python3
"""Synthetic, read-only protocol fixture; no accounts, prompts or credentials."""
import json
import sys
request = json.loads(sys.stdin.readline())
assert sys.stdin.read() == ""
operation = request["operation"]
result = {"protocol": 1}
if operation == "metadata":
    result.update(id="synthetic", display_name="Synthetic", capabilities=["new", "resume"])
elif operation == "probe":
    result.update(available=True)
elif operation == "discover":
    result.update(sessions=[{"session_id": "fixture with 'quotes'", "title": "\u001b[31mSynthetic\u001b[0m\u202e", "cwd": "/tmp", "updated_at_unix_ms": 1}])
elif operation in ("plan-new", "plan-resume"):
    result.update(program="/usr/bin/printf", argv=["%s\\n", "Synthetic " + operation], cwd=request["cwd"])
else:
    raise SystemExit(1)
print(json.dumps(result))
