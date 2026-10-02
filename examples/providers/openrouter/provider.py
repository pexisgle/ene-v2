#!/usr/bin/env python3
"""Version 1 OpenRouter provider example. All HTTP and authentication belong to the host.

Uses OpenRouter with the OpenAI route and no fallback.
Other compatible services require their own provider implementation.
"""
import base64
import codecs
import json
import sys

LIMIT = 1024 * 1024


def emit(message):
    line = json.dumps(message, ensure_ascii=False, separators=(",", ":"))
    if len(line.encode("utf-8")) + 1 > LIMIT:
        raise ValueError("frame limit")
    print(line, flush=True)


def http(request_id, endpoint, path, body=None):
    message = {"type": "http_request", "request_id": request_id,
               "http_id": "http-1", "method": "GET" if body is None else "POST",
               "url": endpoint.rstrip("/") + path,
               "headers": {"content-type": "application/json", "accept": "application/json"}}
    if body is not None:
        message["body_base64"] = base64.b64encode(json.dumps(body).encode()).decode()
    emit(message)


active = None
for line in sys.stdin.buffer:
    if len(line) > LIMIT or not line.endswith(b"\n"):
        sys.exit(1)
    message = {}
    try:
        message = json.loads(line)
        kind = message["type"]
        if kind == "shutdown":
            break
        if kind == "cancel":
            active = None
        elif kind == "initialize":
            if message["protocol_version"] != 1:
                sys.exit(1)
            emit({"type": "initialized", "request_id": message["request_id"],
                  "protocol_version": 1, "capabilities": {"streaming_text": True, "model_list": True}})
        elif kind in ("generate", "list_models"):
            if active is not None:
                raise ValueError("busy")
            request = message.get("request", message)
            active = {"id": message["request_id"], "kind": kind, "text": "",
                      "decoder": codecs.getincrementaldecoder("utf-8")(), "reason": None}
            if kind == "list_models":
                http(active["id"], request["endpoint"], "/models")
            else:
                options = request["options"]
                if options.get("allow_fallbacks") is not False or options.get("upstream") != "openai":
                    raise ValueError("this sample requires the approved OpenAI route")
                messages = [{"role": m["role"], "content": m["text"]} for m in request["messages"]]
                if request["preamble"]:
                    messages.insert(0, {"role": "system", "content": request["preamble"]})
                http(active["id"], request["endpoint"], "/chat/completions",
                     {"model": request["model"], "messages": messages, "stream": True,
                      "max_tokens": request["max_tokens"],
                      "provider": {"only": [options["upstream"]], "allow_fallbacks": False}})
        elif kind == "http_response" and active is not None:
            if message["request_id"] != active["id"] or message["http_id"] != "http-1":
                raise ValueError("unknown HTTP context")
            if not 200 <= message["status"] < 300:
                raise ValueError("HTTP failure")
            active["text"] += active["decoder"].decode(base64.b64decode(message["data_base64"]), final=message["done"])
            if active["kind"] == "generate":
                while "\n" in active["text"]:
                    event, active["text"] = active["text"].split("\n", 1)
                    if not event.startswith("data:"):
                        continue
                    data = event[5:].strip()
                    if not data or data == "[DONE]":
                        continue
                    value = json.loads(data)
                    if "error" in value:
                        raise ValueError("upstream failure")
                    for choice in value.get("choices", []):
                        delta = choice.get("delta", {}).get("content")
                        if delta:
                            emit({"type": "delta", "request_id": active["id"], "text": delta})
                        if choice.get("finish_reason"):
                            active["reason"] = choice["finish_reason"]
            if message["done"]:
                if active["kind"] == "list_models":
                    data = json.loads(active["text"])["data"]
                    emit({"type": "models", "request_id": active["id"],
                          "models": [{"id": m["id"], "name": m.get("name", m["id"])} for m in data]})
                else:
                    emit({"type": "finished", "request_id": active["id"], "reason": active["reason"] or "incomplete"})
                active = None
        else:
            raise ValueError("unexpected message")
    except (ValueError, KeyError, TypeError, UnicodeError):
        request_id = active["id"] if active is not None else message.get("request_id", "unknown") if isinstance(message, dict) else "unknown"
        emit({"type": "error", "request_id": request_id,
              "code": "provider_failure", "message": "Provider request failed"})
        active = None
