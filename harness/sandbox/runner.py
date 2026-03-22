"""
Harness Python sandbox runner.

Reads a dynamic sentinel from stdin, then reads code until that sentinel
appears again, executes the code in a restricted environment, and
communicates tool calls back to the harness via JSON lines on stdout/stdin.

IPC protocol:
  Python->Rust: {"type":"tool_call","id":"c1","key":"source:tool","params":{}}
  Rust->Python: {"type":"tool_result","id":"c1","result":...}
  Rust->Python: {"type":"tool_error","id":"c1","error":"..."}
  Python->Rust: {"type":"done","output":"...","error":null}
"""

import ast
import builtins
import contextlib
import io
import json
import sys

_ORIGINAL_STDIN = sys.stdin
_ORIGINAL_STDOUT = sys.stdout
_ORIGINAL_STDERR = sys.stderr
_CALL_COUNTER = 0
_MAX_CAPTURE_BYTES = 128 * 1024
_MAX_ERROR_CHARS = 4096


class SandboxViolation(Exception):
    """Raised when user code attempts to access blocked syntax or APIs."""


class _BoundedCapture(io.TextIOBase):
    def __init__(self, max_bytes: int):
        self._max_bytes = max_bytes
        self._bytes_written = 0
        self._parts = []
        self._truncated = False

    def writable(self):
        return True

    def write(self, text):
        if not text:
            return 0
        if self._truncated:
            return len(text)

        encoded = text.encode("utf-8", errors="replace")
        remaining = self._max_bytes - self._bytes_written
        if remaining <= 0:
            self._parts.append("\n... output truncated ...\n")
            self._truncated = True
            return len(text)

        if len(encoded) <= remaining:
            self._parts.append(text)
            self._bytes_written += len(encoded)
            return len(text)

        prefix = encoded[:remaining].decode("utf-8", errors="ignore")
        if prefix:
            self._parts.append(prefix)
            self._bytes_written += len(prefix.encode("utf-8"))
        self._parts.append("\n... output truncated ...\n")
        self._truncated = True
        return len(text)

    def flush(self):
        return None

    def getvalue(self):
        return "".join(self._parts)


class _ToolBridge:
    def __call__(self, key: str, params: dict = None):
        global _CALL_COUNTER
        _CALL_COUNTER += 1
        call_id = f"c{_CALL_COUNTER}"

        request = {
            "type": "tool_call",
            "id": call_id,
            "key": key,
            "params": params or {},
        }

        _ORIGINAL_STDOUT.write(json.dumps(request) + "\n")
        _ORIGINAL_STDOUT.flush()

        response_line = _ORIGINAL_STDIN.readline()
        if not response_line:
            raise RuntimeError("Harness closed connection")

        try:
            response = json.loads(response_line.strip())
        except json.JSONDecodeError as exc:
            raise RuntimeError(f"Malformed JSON response from harness: {exc}")

        if not isinstance(response, dict):
            raise RuntimeError(
                f"Unexpected response type from harness: {type(response).__name__}"
            )

        if response.get("type") == "tool_error":
            raise RuntimeError(
                f"Tool call failed: {response.get('error', 'unknown error')}"
            )

        return response.get("result")


_ALLOWED_MODULES = frozenset(
    [
        "json",
        "math",
        "re",
        "datetime",
        "collections",
        "itertools",
        "functools",
        "string",
        "textwrap",
        "decimal",
        "fractions",
        "statistics",
        "copy",
        "pprint",
        "enum",
        "dataclasses",
        "typing",
        "abc",
        "operator",
        "bisect",
        "heapq",
    ]
)

_BLOCKED_NAMES = frozenset(
    [
        "__builtins__",
        "__import__",
        "open",
        "eval",
        "exec",
        "compile",
        "input",
        "help",
        "license",
        "credits",
        "breakpoint",
        "globals",
        "locals",
        "vars",
        "dir",
        "getattr",
        "setattr",
        "delattr",
        "type",
        "memoryview",
    ]
)

_ORIGINAL_IMPORT = builtins.__import__


def _restricted_import(name, *args, **kwargs):
    top_level = name.split(".")[0]
    if top_level not in _ALLOWED_MODULES:
        raise ImportError(
            f"Import of '{name}' is not allowed in the sandbox. "
            f"Allowed modules: {', '.join(sorted(_ALLOWED_MODULES))}"
        )
    return _ORIGINAL_IMPORT(name, *args, **kwargs)


class _SafetyVisitor(ast.NodeVisitor):
    def visit_Attribute(self, node):
        if node.attr.startswith("_"):
            raise SandboxViolation(
                f"Access to private attribute '{node.attr}' is not allowed"
            )
        self.generic_visit(node)

    def visit_Name(self, node):
        if node.id in _BLOCKED_NAMES:
            raise SandboxViolation(f"Use of '{node.id}' is not allowed")
        self.generic_visit(node)

    def visit_Import(self, node):
        for alias in node.names:
            top_level = alias.name.split(".")[0]
            if top_level not in _ALLOWED_MODULES:
                raise SandboxViolation(
                    f"Import of '{alias.name}' is not allowed in the sandbox"
                )
        self.generic_visit(node)

    def visit_ImportFrom(self, node):
        if node.level != 0:
            raise SandboxViolation("Relative imports are not allowed in the sandbox")
        module = node.module or ""
        top_level = module.split(".")[0]
        if top_level not in _ALLOWED_MODULES:
            raise SandboxViolation(
                f"Import of '{module}' is not allowed in the sandbox"
            )
        for alias in node.names:
            if alias.name == "*":
                raise SandboxViolation("Wildcard imports are not allowed in the sandbox")
            if alias.name.startswith("_"):
                raise SandboxViolation(
                    f"Import of private name '{alias.name}' is not allowed"
                )
        self.generic_visit(node)

    def visit_Call(self, node):
        if isinstance(node.func, ast.Name) and node.func.id in _BLOCKED_NAMES:
            raise SandboxViolation(f"Call to '{node.func.id}' is not allowed")
        self.generic_visit(node)


_SAFE_BUILTINS = {
    "bool": bool,
    "int": int,
    "float": float,
    "str": str,
    "bytes": bytes,
    "bytearray": bytearray,
    "list": list,
    "tuple": tuple,
    "dict": dict,
    "set": set,
    "frozenset": frozenset,
    "complex": complex,
    "object": object,
    "abs": abs,
    "all": all,
    "any": any,
    "bin": bin,
    "chr": chr,
    "divmod": divmod,
    "enumerate": enumerate,
    "filter": filter,
    "format": format,
    "hash": hash,
    "hex": hex,
    "isinstance": isinstance,
    "issubclass": issubclass,
    "iter": iter,
    "len": len,
    "map": map,
    "max": max,
    "min": min,
    "next": next,
    "oct": oct,
    "ord": ord,
    "pow": pow,
    "print": print,
    "range": range,
    "repr": repr,
    "reversed": reversed,
    "round": round,
    "slice": slice,
    "sorted": sorted,
    "sum": sum,
    "zip": zip,
    "__import__": _restricted_import,
    "__build_class__": builtins.__build_class__,
    "Exception": Exception,
    "ValueError": ValueError,
    "TypeError": TypeError,
    "KeyError": KeyError,
    "IndexError": IndexError,
    "AttributeError": AttributeError,
    "RuntimeError": RuntimeError,
    "StopIteration": StopIteration,
    "ImportError": ImportError,
    "NotImplementedError": NotImplementedError,
    "ZeroDivisionError": ZeroDivisionError,
    "True": True,
    "False": False,
    "None": None,
}


def _validate_and_compile(code: str):
    tree = ast.parse(code, "<sandbox>", "exec")
    _SafetyVisitor().visit(tree)
    return compile(tree, "<sandbox>", "exec")


def _build_exec_globals():
    return {
        "__builtins__": _SAFE_BUILTINS.copy(),
        "__name__": "__sandbox__",
        "tool": _ToolBridge(),
        "json": json,
    }


def _truncate_error_message(message: str) -> str:
    if len(message) <= _MAX_ERROR_CHARS:
        return message
    return message[:_MAX_ERROR_CHARS] + "... (truncated)"


def main():
    sentinel = _ORIGINAL_STDIN.readline().strip()
    code_lines = []
    for line in _ORIGINAL_STDIN:
        if line.strip() == sentinel:
            break
        code_lines.append(line)
    code = "".join(code_lines)

    if not code.strip():
        _ORIGINAL_STDOUT.write(
            json.dumps({"type": "done", "output": "", "error": None}) + "\n"
        )
        _ORIGINAL_STDOUT.flush()
        return

    capture_buffer = _BoundedCapture(_MAX_CAPTURE_BYTES)
    exec_globals = _build_exec_globals()

    try:
        compiled = _validate_and_compile(code)
        with contextlib.redirect_stdout(capture_buffer), contextlib.redirect_stderr(
            capture_buffer
        ):
            exec(compiled, exec_globals)
        output = capture_buffer.getvalue()
        _ORIGINAL_STDOUT.write(
            json.dumps({"type": "done", "output": output, "error": None}) + "\n"
        )
    except Exception as exc:
        output = capture_buffer.getvalue()
        _ORIGINAL_STDOUT.write(
            json.dumps(
                {
                    "type": "done",
                    "output": output,
                    "error": _truncate_error_message(f"{type(exc).__name__}: {exc}"),
                }
            )
            + "\n"
        )

    _ORIGINAL_STDOUT.flush()


if __name__ == "__main__":
    main()
