#!/usr/bin/env python3
"""rcargo port shim for `{{FUNCTION}}` (Python 3.8+, standard library only).

In your app (the only edit to the original source; delete the line to undo):

    import sys; sys.path.insert(0, "<path to this shims folder>")
    import rcargo_port

    @rcargo_port.port
    def your_function(a, b): ...

Behaviour of a decorated function (positional arguments only):
  RCARGO_PORT_RECORD=<file>  run the original and append the call to <file> (golden cases)
  RCARGO_PORT_DISABLE=1      always run the original
  otherwise                  ask the Rust binary; fall back to the original when the binary is
                             missing, arguments are not plain JSON data, or the port reports `fatal`.
Results from the port are plain JSON types (tuples come back as lists). When the port reports an
error, the original runs so it raises its own exception (type, message, traceback).

Command line (the original function is imported from a .py file or a module name):
  rcargo_port.py oracle MODULE FUNCTION CANDIDATES.jsonl CASES.jsonl   answer candidate inputs with the original
  rcargo_port.py bench  MODULE FUNCTION CASES.jsonl                    original vs port, end to end
"""
import functools
import importlib
import importlib.util
import json
import os
import subprocess
import sys
import time

BIN = os.environ.get("RCARGO_PORT_BIN") or os.path.normpath(
    os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "release", "{{BIN}}")
)
MAX_CASES = int(os.environ.get("RCARGO_PORT_MAX_CASES", "500"))
_J = {"separators": (",", ":"), "allow_nan": False, "ensure_ascii": False}


_proc = None
_broken = False
_seen = {}


def _ask(request):
    global _proc
    if _proc is None or _proc.poll() is not None:
        _proc = subprocess.Popen(
            [BIN], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, encoding="utf-8", bufsize=1
        )
    _proc.stdin.write(request + "\n")
    _proc.stdin.flush()
    line = _proc.stdout.readline()
    if not line:
        raise OSError("port binary exited")
    return json.loads(line)


def _accelerated(fn, args):
    global _broken
    if _broken or not os.path.exists(BIN):
        return fn(*args)
    try:
        request = '{"args":%s}' % json.dumps(list(args), **_J)
    except (TypeError, ValueError):
        return fn(*args)
    try:
        res = _ask(request)
    except (OSError, ValueError):
        _broken = True
        return fn(*args)
    if "ok" in res:
        return res["ok"]
    # "err": fall through, so the original raises its own exception.
    return fn(*args)


def _record(fn, args, path):
    seen = _seen.get(path)
    if seen is None:
        seen = _seen[path] = set()
        if os.path.exists(path):
            with open(path, encoding="utf-8") as f:
                for line in f:
                    if line.strip():
                        seen.add(json.dumps(json.loads(line)["args"], **_J))
    try:
        key = json.dumps(list(args), **_J)
    except (TypeError, ValueError):
        return fn(*args)
    if len(seen) >= MAX_CASES or key in seen:
        return fn(*args)
    t0 = time.perf_counter_ns()
    err = None
    try:
        result = fn(*args)
    except Exception as e:  # recorded, then re-raised
        err = e
    ns = time.perf_counter_ns() - t0
    try:
        expect = {"err": str(err)} if err is not None else {"ok": result}
        line = '{"args":%s,"expect":%s,"ns":%d}' % (key, json.dumps(expect, **_J), ns)
    except (TypeError, ValueError):
        line = None
    if line is not None:
        os.makedirs(os.path.dirname(os.path.abspath(path)), exist_ok=True)
        with open(path, "a", encoding="utf-8") as f:
            f.write(line + "\n")
        seen.add(key)
    if err is not None:
        raise err
    return result


def port(fn):
    @functools.wraps(fn)
    def wrapper(*args, **kwargs):
        if kwargs or os.environ.get("RCARGO_PORT_DISABLE") == "1":
            return fn(*args, **kwargs)
        record = os.environ.get("RCARGO_PORT_RECORD")
        if record:
            return _record(fn, args, record)
        return _accelerated(fn, args)

    wrapper.original = fn
    return wrapper


def _load(module, function):
    if module.endswith(".py") and os.path.exists(module):
        sys.path.insert(0, os.path.dirname(os.path.abspath(module)))
        spec = importlib.util.spec_from_file_location("rcargo_port_target", module)
        mod = importlib.util.module_from_spec(spec)
        sys.modules["rcargo_port_target"] = mod
        spec.loader.exec_module(mod)
    else:
        sys.path.insert(0, os.getcwd())
        mod = importlib.import_module(module)
    obj = mod
    for part in function.split("."):
        obj = getattr(obj, part)
    return obj


def _cases(path):
    with open(path, encoding="utf-8") as f:
        return [json.loads(line) for line in f if line.strip()]


def _oracle(module, function, candidates, out):
    os.environ["RCARGO_PORT_DISABLE"] = "1"
    os.environ.pop("RCARGO_PORT_RECORD", None)
    fn = _load(module, function)
    have = set()
    if os.path.exists(out):
        have = {json.dumps(c["args"], **_J) for c in _cases(out)}
    added = errors = 0
    os.makedirs(os.path.dirname(os.path.abspath(out)), exist_ok=True)
    with open(out, "a", encoding="utf-8") as f:
        for c in _cases(candidates):
            key = json.dumps(c["args"], **_J)
            if key in have:
                continue
            t0 = time.perf_counter_ns()
            err = None
            try:
                result = fn(*json.loads(key))
            except Exception as e:
                err = e
            ns = time.perf_counter_ns() - t0
            try:
                expect = {"err": str(err)} if err is not None else {"ok": result}
                f.write('{"args":%s,"expect":%s,"ns":%d}\n' % (key, json.dumps(expect, **_J), ns))
            except (TypeError, ValueError):
                continue
            have.add(key)
            added += 1
            errors += err is not None
    print("oracle: %d new cases (%d are errors)" % (added, errors), file=sys.stderr)


def _bench(module, function, cases_path):
    os.environ["RCARGO_PORT_DISABLE"] = "1"
    fn = _load(module, function)
    if not os.path.exists(BIN):
        sys.exit("no port binary at %s: run `rcargo port build` first" % BIN)
    calls = [c["args"] for c in _cases(cases_path)]
    if not calls:
        sys.exit("no cases in %s" % cases_path)
    reqs = ['{"args":%s}' % json.dumps(a, **_J) for a in calls]

    def best(run, rounds=3):
        times = []
        for _ in range(rounds):
            t0 = time.perf_counter()
            run()
            times.append(time.perf_counter() - t0)
        return min(times)

    def run_original():
        for a in calls:
            try:
                fn(*a)
            except Exception:
                pass

    def run_port():
        for r in reqs:
            _ask(r)

    t_orig, t_port = best(run_original), best(run_port)
    print(
        "original %.1fms, port %.1fms: %.1fx per call set (%d calls, best of 3, bridge included)"
        % (t_orig * 1e3, t_port * 1e3, t_orig / t_port if t_port else float("inf"), len(calls))
    )
    if t_port >= t_orig:
        print("the port is not faster end to end: do not enable it (the call crosses the bridge too often, or the work is too small)")


if __name__ == "__main__":
    a = sys.argv[1:]
    if len(a) == 5 and a[0] == "oracle":
        _oracle(*a[1:])
    elif len(a) == 4 and a[0] == "bench":
        _bench(*a[1:])
    else:
        sys.exit(__doc__)
