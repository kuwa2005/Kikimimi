"""試験用の Claude API の模擬サーバ。KIKIMIMI_ANTHROPIC_BASE=http://127.0.0.1:<port>/v1 で使う。

- GET  /v1/models    : モデル一覧（1 件）
- POST /v1/messages  : 会議の解析なら、書き起こし（<segment>）に議題の言葉が出てきた議題へ回答候補を返す。
                       それ以外（要約など）は短い文を返す。
受け取った依頼は 1 行 1 件の JSON でログに書く（音声を送っていないことの確認用）。
"""

import json
import re
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8799
LOG = sys.argv[2] if len(sys.argv) > 2 else "mock-claude.log"


def analysis(user: str) -> dict:
    segment = re.search(r"<segment>\n(.*?)\n</segment>", user, re.S)
    segment = segment.group(1) if segment else ""
    answers = []
    active = None
    for qid, status, body, current in re.findall(r"- id=(\d+) status=(\S+) body=(.+?) current=(.*)", user):
        # 議題の頭 2 文字（例:「予算」「導入」「決裁」）が発言に出てきたら、その発言を回答候補にする。
        key = body.strip()[:2]
        lines = [line for line in segment.splitlines() if key and key in line]
        if not lines:
            continue
        previous = "" if current.strip() == "（空）" else current.strip()
        candidate = "／".join([p for p in [previous, *lines] if p])
        answers.append({"question_id": int(qid), "candidate": candidate, "status": "draft", "evidence": lines[0][:60]})
        active = int(qid)
    return {
        "active_question_id": active,
        "answers": answers,
        "search_queries": [],
        "next_question": {"text": "", "angle": "具体化"},
        "new_questions": [],
        "skip_questions": [],
        "notes": [],
    }


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def reply(self, payload: dict):
        body = json.dumps(payload, ensure_ascii=False).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path.startswith("/v1/models"):
            self.reply({"data": [{"id": "claude-sonnet-5-5"}]})
        else:
            self.send_error(404)

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length)
        request = json.loads(raw or b"{}")
        user = request.get("messages", [{}])[0].get("content", "")
        if not isinstance(user, str):
            user = json.dumps(user, ensure_ascii=False)
        kind = "analysis" if "<segment>" in user and "<questions>" in user else "other"
        with open(LOG, "a", encoding="utf-8") as f:
            f.write(json.dumps({"kind": kind, "bytes": len(raw), "has_audio": "audio" in raw.decode(errors="ignore")[:200000] and "base64" in raw.decode(errors="ignore")[:200000], "segment": re.findall(r"<segment>\n(.*?)\n</segment>", user, re.S)}, ensure_ascii=False) + "\n")
        if kind == "analysis":
            text = json.dumps(analysis(user), ensure_ascii=False)
        elif "participants" in json.dumps(request.get("output_config", request), ensure_ascii=False):
            text = json.dumps({"participants": []})
        else:
            text = "（模擬の応答）"
        self.reply({"content": [{"type": "text", "text": text}], "stop_reason": "end_turn"})


ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
