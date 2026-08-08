"""A tiny script to prove the real CPython + stdlib runs in a wk container.
Mount this dir and run `python /app/hello.py`."""
import sys
import json
import platform

print(f"CPython {sys.version.split()[0]} on {sys.platform}")
print("stdlib import works:", json.dumps({"answer": 42, "runtime": "wasm"}))
print("byteorder:", sys.byteorder, "| maxsize:", sys.maxsize)
print(f"argv: {sys.argv}")
