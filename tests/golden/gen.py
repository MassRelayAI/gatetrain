"""Generates tests/golden/decisions.jsonl with an implementation independent of the Rust crate
(hashlib + json.dumps), so the Rust canonical bytes and chain are pinned by something else.
Run: python tests/golden/gen.py > tests/golden/decisions.jsonl
"""
import hashlib
import json

GENESIS = "0" * 64
canon = lambda v: json.dumps(v, sort_keys=True, separators=(",", ":"), ensure_ascii=False)  # noqa: E731

entries = [
    dict(at="2026-11-15T10:00:00+07:00", actor="engine", kind="transition", batch="bx", subject="abc123", rule=None,
         policy_hash=None, evidence={"steps": 12, "reds": 0}, outcome="allowed", reason="batch opened", signature=None),
    dict(at="2026-11-15T10:05:00+07:00", actor="agent:sonnet", kind="refusal", batch="bx", subject="lane-a",
         rule="floor.no-force-push", policy_hash="f" * 64, evidence={"z": [1, {"b": 2, "a": 1}], "a": "caf\u00e9 \"q\" \\ \n"},
         outcome="refused", reason="force push to main", signature=None),
    dict(at="2026-11-15T10:10:00+07:00", actor="human:founder", kind="approval", batch=None, subject="deploy:staging",
         rule="6.5", policy_hash="e" * 64, evidence=None, outcome="allowed", reason="ok \u2603 \U0001F680", signature="sshsig:AAAA"),
]
prev = GENESIS
for i, e in enumerate(entries, 1):
    body = dict(seq=i, prev_hash=prev, **e)
    c = canon(body)
    h = hashlib.sha256((prev + "\n" + c).encode("utf-8")).hexdigest()
    print(json.dumps({"body": body, "canonical": c, "hash": h}, ensure_ascii=True))
    prev = h
