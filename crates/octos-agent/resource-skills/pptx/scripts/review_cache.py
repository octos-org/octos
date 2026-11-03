"""Content-keyed review reuse and in-flight coalescing; no artifact acceptance rules."""
import ast
import hashlib
import inspect
import json
import textwrap
import threading

_guard=threading.Lock()
_locks={}

def content_lock(key):
    with _guard:return _locks.setdefault(key,threading.RLock())

def function_identity(function):
    tree=ast.parse(textwrap.dedent(inspect.getsource(function)))
    return ast.dump(tree,include_attributes=False)

def semantic_review_identity(prompt, functions, settings):
    """Transport, retries and file formatting do not change review semantics."""
    value={'prompt':prompt,'functions':[function_identity(f) for f in functions],'settings':settings}
    return hashlib.sha256(json.dumps(value,sort_keys=True).encode()).hexdigest()
