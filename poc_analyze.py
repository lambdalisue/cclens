#!/usr/bin/env python3
"""PoC: Claude Code セッション解析 — スキル別の頻度 / token / 時間 / メインコンテキスト消費量。

区間(span)定義: スキル呼び出しレコード 〜 次の人間ターン(ユーザー発話) まで。
- 出力token: 区間内 assistant の output_tokens 合計
- 経過時間: 区間内の最初〜最後のレコードの timestamp 差
- コンテキスト消費(main): 区間内で prompt_tokens(=input+cache_read+cache_creation) が
  開始時から最大どれだけ増えたか。isSidechain=true は main から除外。
"""
import sys, json, glob, os, re, collections
from datetime import datetime

PROJ = os.path.expanduser("~/.claude/projects")
CMD_RE = re.compile(r"<command-name>/?([^<]+)</command-name>")

def origin_of(path):
    """main セッションか subagent トランスクリプトか。"""
    return "sub" if "/subagents/" in path else "main"

def prompt_tokens(usage):
    return (usage.get("input_tokens", 0)
            + usage.get("cache_read_input_tokens", 0)
            + usage.get("cache_creation_input_tokens", 0))

def parse_ts(s):
    try:
        return datetime.fromisoformat(s.replace("Z", "+00:00"))
    except Exception:
        return None

def is_human_turn(rec):
    """tool_result でなく isMeta でない、実ユーザー発話か。"""
    if rec.get("type") != "user" or rec.get("isMeta"):
        return False
    content = rec.get("message", {}).get("content")
    if isinstance(content, str):
        return True
    if isinstance(content, list):
        for b in content:
            if isinstance(b, dict) and b.get("type") == "tool_result":
                return False
        return True
    return False

# skill -> 集計（main / sub を分離）
def new_bucket():
    return {"count": 0, "out_tokens": 0, "ctx": 0, "secs": 0.0}
agg = collections.defaultdict(lambda: {
    "main": new_bucket(), "sub": new_bucket(),
    "sources": collections.Counter(),
})
files = {"main": 0, "sub": 0}

paths = (glob.glob(os.path.join(PROJ, "*", "*.jsonl"))
         + glob.glob(os.path.join(PROJ, "*", "*", "subagents", "*.jsonl")))
for path in paths:
    origin = origin_of(path)
    try:
        recs = [json.loads(l) for l in open(path) if l.strip()]
    except Exception:
        continue
    files[origin] += 1
    # 線形スキャンでスキルイベントを収集
    events = []  # (idx, name, source)
    for i, r in enumerate(recs):
        if r.get("type") == "user" and not r.get("isMeta"):
            txt = json.dumps(r.get("message", {}).get("content", ""))
            for m in CMD_RE.finditer(txt):
                events.append((i, m.group(1).strip().lstrip("/"), "slash"))
        if r.get("type") == "assistant":
            for b in r.get("message", {}).get("content", []):
                if isinstance(b, dict) and b.get("type") == "tool_use" and b.get("name") == "Skill":
                    events.append((i, b.get("input", {}).get("skill", "?"), "tool"))

    for (idx, name, source) in events:
        # 区間終端 = 次の人間ターン
        end = len(recs)
        for j in range(idx + 1, len(recs)):
            if is_human_turn(recs[j]):
                end = j
                break
        span = recs[idx:end]
        out = 0
        base_ctx = None
        max_ctx = 0
        tss = []
        for r in span:
            ts = parse_ts(r.get("timestamp", ""))
            if ts:
                tss.append(ts)
            if r.get("type") == "assistant":
                u = r.get("message", {}).get("usage", {})
                out += u.get("output_tokens", 0)
                pt = prompt_tokens(u)
                if base_ctx is None:
                    base_ctx = pt
                max_ctx = max(max_ctx, pt)
        a = agg[name]
        b = a[origin]
        b["count"] += 1
        b["out_tokens"] += out
        a["sources"][f"{origin}:{source}"] += 1
        if base_ctx is not None:
            b["ctx"] += max(0, max_ctx - base_ctx)
        if len(tss) >= 2:
            b["secs"] += (max(tss) - min(tss)).total_seconds()

def avg(b, key):
    return b[key] / b["count"] if b["count"] else 0.0

print(f"# main={files['main']}  sub={files['sub']} 本を解析\n")
rows = sorted(agg.items(),
              key=lambda kv: kv[1]["main"]["count"] + kv[1]["sub"]["count"],
              reverse=True)
hdr = (f"{'skill':<24}{'main':>5}{'sub':>5}{'mCtx/回':>9}{'sCtx/回':>9}"
       f"{'m秒/回':>8}  callers")
print(hdr); print("-" * len(hdr))
sub_only = []
for name, a in rows:
    m, s = a["main"], a["sub"]
    callers = ",".join(f"{k}:{v}" for k, v in a["sources"].most_common())
    print(f"{name[:23]:<24}{m['count']:>5}{s['count']:>5}"
          f"{avg(m,'ctx'):>9.0f}{avg(s,'ctx'):>9.0f}{avg(m,'secs'):>8.0f}  {callers}")
    if m["count"] == 0 and s["count"] > 0:
        sub_only.append(name)

print("\n## ⚠ サブエージェント専用（main 呼び出し0 → 削除NG）")
print("  " + (", ".join(sub_only) if sub_only else "なし"))
