# ene-provider Python SDK

Python製の隔離プロバイダーを、Rustホストと同じRemoc RTC契約で実装するネイティブSDKです。
PythonはIPC形式を実装せず、SDK内のRustが接続、要求の相関、ストリーミング、キャンセルを扱います。
PythonからホストのHTTP brokerを非同期で呼び出せます。資格情報と認可はホストに残ります。

```sh
maturin build --manifest-path crates/ene-python/Cargo.toml --features extension-module
pip install target/wheels/ene_provider-*.whl
```

```python
import asyncio
import ene_provider

class Provider:
    async def list_models(self, endpoint, options, broker):
        response = await broker.request({
            "method": "GET", "url": endpoint + "/models",
            "headers": {"accept": "application/json"}, "body": None,
        })
        try:
            if response.status != 200:
                raise RuntimeError("model discovery failed")
            body = b"".join([chunk async for chunk in response])
            # Normalize the upstream response into [{id, name, metadata?}].
            return [{"id": "example", "name": "Example", "metadata": None}]
        finally:
            await response.aclose()

    async def generate(self, request, broker, deltas):
        await deltas.send("Hello")
        return "stop"

async def main():
    await ene_provider.serve(Provider())

asyncio.run(main())
```

`request`はRustの`GenerationRequest`に対応する辞書です。
`broker.request`のbodyはbytesまたはNone、応答はstatus・headersとbytesの非同期イテレーターです。
応答を途中で読むのをやめるときは`aclose()`を呼びます。
生成は`deltas.send(str)`をawaitし、終了理由をstrで返します。例外はホストへ一般化した失敗として返し、詳細はstderrへ出します。
Rustの呼び出しがキャンセルされるとPythonのcoroutineもキャンセルされるため、後処理は`finally`に置きます。

stdoutはRemoc専用です。診断はstderrに出してください。
ホストのsandboxではSDKの拡張モジュールを他の依存とともにpluginディレクトリへ梱包し、
`-I`で実行するentry pointから`/plugin`を明示的にimport pathへ追加します。
PyO3のabi3 wheelを使用しますが、ホストOSと互換性のあるLinux wheelとしてビルドする必要があります。

```sh
cargo build -p ene-python --features extension-module
cargo test -p ene-python --all-features
```

[Remoc RTC](https://docs.rs/remoc/latest/remoc/rtc/index.html)、
[PyO3](https://pyo3.rs/main/)、
[pyo3-async-runtimes](https://docs.rs/pyo3-async-runtimes/latest/pyo3_async_runtimes/)、
[Pythonize](https://docs.rs/pythonize/latest/pythonize/)。
