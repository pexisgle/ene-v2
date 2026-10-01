#!/usr/bin/env python3
"""Explicit fixed-input connectivity probe; never reads conversation or saves a key."""
import json
import os
import sys
import time
import urllib.error
import urllib.request


def main():
    if sys.argv[1:] != ["--run"]:
        raise SystemExit("Usage: python3 scripts/check-openrouter.py --run (sends only a fixed Japanese probe)")
    key = os.environ.get("OPENROUTER_API_KEY")
    if not key:
        raise SystemExit("OPENROUTER_API_KEY is not set")
    payload = {
        "model": "openai/gpt-6-luna",
        "provider": {"only": ["openai"], "allow_fallbacks": False},
        "messages": [{"role": "user", "content": "日本語で「接続確認に成功しました」と一文だけ返してください。"}],
        "stream": True,
        "max_tokens": 256,
    }
    request = urllib.request.Request(
        "https://openrouter.ai/api/v1/chat/completions",
        data=json.dumps(payload, ensure_ascii=False).encode(),
        headers={"Authorization": "Bearer " + key, "Content-Type": "application/json"},
    )
    started = time.monotonic()
    first = None
    chunks = 0
    text = ""
    providers = set()
    models = set()
    done = False
    try:
        # Do not forward a credential or probe automatically to a redirect target.
        class NoRedirect(urllib.request.HTTPRedirectHandler):
            def redirect_request(self, req, fp, code, msg, headers, newurl):
                return None
        opener = urllib.request.build_opener(NoRedirect())
        with opener.open(request, timeout=90) as response:
            total = 0
            for line in response:
                total += len(line)
                if total > 256_000 or time.monotonic() - started > 120:
                    raise RuntimeError("probe_limit_exceeded")
                if not line.startswith(b"data:"):
                    continue
                data = line[5:].strip()
                if data == b"[DONE]":
                    done = True
                    break
                event = json.loads(data)
                if "error" in event:
                    raise RuntimeError("upstream_stream_error")
                if event.get("provider"):
                    providers.add(event["provider"])
                if event.get("model"):
                    models.add(event["model"])
                for choice in event.get("choices", []):
                    chunk = choice.get("delta", {}).get("content")
                    if isinstance(chunk, str) and chunk:
                        first = first if first is not None else time.monotonic() - started
                        chunks += 1
                        text += chunk
        if not done or not text.strip():
            raise RuntimeError("incomplete_or_empty_stream")
        if any(provider.lower() != "openai" for provider in providers):
            raise RuntimeError("unexpected_provider")
        print(json.dumps({
            "result": "PASS", "requested_model": payload["model"],
            "reported_models": sorted(models), "reported_providers": sorted(providers),
            "fallbacks": False, "content_chunks": chunks, "unicode_scalars": len(text),
            "first_text_seconds": round(first, 3), "total_seconds": round(time.monotonic()-started, 3),
            "scope": "fixed probe only; no conversation, files, or credentials persisted",
        }, ensure_ascii=False, indent=2))
    except urllib.error.HTTPError as error:
        # External bodies can include echoed headers or secret-bearing diagnostics.
        raise SystemExit(f"FAIL: OpenRouter HTTP {error.code}; response body omitted") from None
    except Exception as error:
        raise SystemExit(f"FAIL: {type(error).__name__}; external details omitted") from None


if __name__ == "__main__":
    main()
