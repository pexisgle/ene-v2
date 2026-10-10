//! Python is a thin adapter around the same Remoc provider and broker traits.
use ene_ipc::{
    provider::{
        self, BrokerClient, Capabilities, Codec, GenerationRequest, HttpBroker, HttpResponsePart,
        Model, Provider, ProviderError, ProviderServerShared, Sender,
    },
    remoc::{self, ConnectExt, rch::mpsc, rtc::ServerShared},
};
use pyo3::{
    exceptions::{PyRuntimeError, PyStopAsyncIteration},
    prelude::*,
    types::PyBytes,
};
use pyo3_async_runtimes::{TaskLocals, into_future_with_locals, tokio::future_into_py};
use std::{os::fd::AsFd, sync::Arc};
use tokio::{net::unix::pipe, sync::Mutex};
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};

fn error(error: impl std::fmt::Display) -> PyErr {
    PyRuntimeError::new_err(error.to_string())
}

struct PythonProvider {
    object: Py<PyAny>,
    locals: TaskLocals,
    capabilities: Capabilities,
}

impl Provider for PythonProvider {
    async fn capabilities(&self) -> Result<Capabilities, ProviderError> {
        Ok(self.capabilities.clone())
    }

    async fn list_models(
        &self,
        endpoint: String,
        options: serde_json::Value,
        broker: BrokerClient,
    ) -> Result<Vec<Model>, ProviderError> {
        let future = Python::attach(|py| {
            let options = pythonize::pythonize(py, &options)?;
            let broker = Py::new(py, Broker { client: broker })?;
            let coroutine =
                self.object
                    .call_method1(py, "list_models", (endpoint, options, broker))?;
            python_call(&self.locals, coroutine.into_bound(py))
        })
        .map_err(python_failure)?;
        let result = future.await.map_err(python_failure)?;
        Python::attach(|py| pythonize::depythonize(result.bind(py)))
            .map_err(|_| ProviderError::Failed)
    }

    async fn generate(
        &self,
        request: GenerationRequest,
        broker: BrokerClient,
        deltas: Sender<String>,
    ) -> Result<String, ProviderError> {
        let future = Python::attach(|py| {
            let request = pythonize::pythonize(py, &request)?;
            let broker = Py::new(py, Broker { client: broker })?;
            let deltas = Py::new(py, Deltas { sender: deltas })?;
            let coroutine = self
                .object
                .call_method1(py, "generate", (request, broker, deltas))?;
            python_call(&self.locals, coroutine.into_bound(py))
        })
        .map_err(python_failure)?;
        let result = future.await.map_err(python_failure)?;
        Python::attach(|py| result.extract(py)).map_err(python_failure)
    }
}

// into_future_with_locals does not cancel a Python Task when its Rust future is
// dropped. asyncio's thread-safe Future supplies that cancellation bridge.
fn python_call(
    locals: &TaskLocals,
    coroutine: Bound<'_, PyAny>,
) -> PyResult<impl std::future::Future<Output = PyResult<Py<PyAny>>> + Send + use<>> {
    let py = coroutine.py();
    let locals = locals
        .clone()
        .with_context(locals.context(py).call_method0("copy")?);
    let asyncio = py.import("asyncio")?;
    let task = locals.context(py).call_method1(
        "run",
        (
            asyncio.getattr("run_coroutine_threadsafe")?,
            coroutine,
            locals.event_loop(py),
        ),
    )?;
    let kwargs = pyo3::types::PyDict::new(py);
    kwargs.set_item("loop", locals.event_loop(py))?;
    let awaitable = asyncio.call_method("wrap_future", (&task,), Some(&kwargs))?;
    let future = into_future_with_locals(&locals, awaitable)?;
    let guard = PythonTask(task.unbind());
    Ok(async move {
        let result = future.await;
        drop(guard);
        result
    })
}
struct PythonTask(Py<PyAny>);
impl Drop for PythonTask {
    fn drop(&mut self) {
        Python::try_attach(|py| {
            let _ = self.0.call_method0(py, "cancel");
        });
    }
}

fn python_failure(error: PyErr) -> ProviderError {
    // Diagnostics stay on stderr; traceback text never enters a host response.
    Python::attach(|py| error.print(py));
    ProviderError::Failed
}

#[pyclass(module = "ene_provider")]
struct Deltas {
    sender: Sender<String>,
}

#[pymethods]
impl Deltas {
    fn send<'py>(&self, py: Python<'py>, text: String) -> PyResult<Bound<'py, PyAny>> {
        let sender = self.sender.clone();
        future_into_py(py, async move {
            sender
                .send(text)
                .await
                .map_err(error)?
                .await
                .map_err(error)?;
            Ok(())
        })
    }
}

#[pyclass(module = "ene_provider")]
struct Broker {
    client: BrokerClient,
}

#[pymethods]
impl Broker {
    /// HTTP is performed by the host with the invocation's authorization.
    fn request<'py>(
        &self,
        py: Python<'py>,
        request: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let request = pythonize::depythonize(&request)?;
        let client = self.client.clone();
        future_into_py(py, async move {
            let (sender, mut receiver) = mpsc::channel::<HttpResponsePart, Codec>();
            let call = AbortOnDropHandle::new(tokio::spawn(async move {
                client.request(request, sender).await
            }));
            let head = match receiver.recv().await.map_err(error)? {
                Some(part) => part,
                None => {
                    call.await.map_err(error)?.map_err(error)?;
                    return Err(error("missing HTTP response"));
                }
            };
            let HttpResponsePart::Head { status, headers } = head else {
                return Err(error("missing HTTP headers"));
            };
            Ok(Response {
                stop: CancellationToken::new(),
                status,
                headers,
                body: Arc::new(Mutex::new(Body {
                    receiver,
                    call: Some(call),
                })),
            })
        })
    }
}

struct Body {
    receiver: mpsc::Receiver<HttpResponsePart, Codec>,
    call: Option<AbortOnDropHandle<Result<(), ProviderError>>>,
}

#[pyclass(module = "ene_provider")]
struct Response {
    stop: CancellationToken,
    #[pyo3(get)]
    status: u16,
    #[pyo3(get)]
    headers: std::collections::BTreeMap<String, String>,
    body: Arc<Mutex<Body>>,
}

#[pymethods]
impl Response {
    fn __aiter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }
    fn __anext__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let body = self.body.clone();
        let stop = self.stop.clone();
        future_into_py(py, async move {
            let mut body = tokio::select! { biased; _ = stop.cancelled() => return Err(PyStopAsyncIteration::new_err(())), body = body.lock() => body };
            let next = tokio::select! { biased; _ = stop.cancelled() => return Err(PyStopAsyncIteration::new_err(())), next = body.receiver.recv() => next.map_err(error)? };
            match next {
                Some(HttpResponsePart::Data(bytes)) => {
                    Python::attach(|py| Ok(PyBytes::new(py, &bytes).unbind()))
                }
                Some(HttpResponsePart::Head { .. }) => Err(error("duplicate HTTP headers")),
                None => {
                    if let Some(call) = body.call.take() {
                        tokio::select! { biased; _ = stop.cancelled() => return Err(PyStopAsyncIteration::new_err(())), result = call => result.map_err(error)?.map_err(error)? };
                    }
                    Err(PyStopAsyncIteration::new_err(()))
                }
            }
        })
    }
    fn aclose<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.stop.cancel();
        let body = self.body.clone();
        future_into_py(py, async move {
            let mut body = body.lock().await;
            body.receiver.close();
            body.call.take();
            Ok(())
        })
    }
}

/// Serve one isolated provider process over stdin/stdout. stdout is reserved for Remoc.
#[pyfunction]
#[pyo3(signature = (provider, *, streaming_text=true, model_list=true))]
fn serve<'py>(
    py: Python<'py>,
    provider: Py<PyAny>,
    streaming_text: bool,
    model_list: bool,
) -> PyResult<Bound<'py, PyAny>> {
    let locals = pyo3_async_runtimes::tokio::get_current_locals(py)?;
    future_into_py(py, async move {
        let (server, client) = ProviderServerShared::<_, Codec>::new(Arc::new(PythonProvider {
            object: provider,
            locals,
            capabilities: Capabilities {
                streaming_text,
                model_list,
            },
        }));
        // Cancellable OS pipes let process exit when its last RTC client is dropped.
        let input = pipe::Receiver::from_owned_fd(std::io::stdin().as_fd().try_clone_to_owned()?)
            .map_err(error)?;
        let output = pipe::Sender::from_owned_fd(std::io::stdout().as_fd().try_clone_to_owned()?)
            .map_err(error)?;
        remoc::Connect::io::<_, _, provider::Client, provider::Client, remoc::codec::Default>(
            remoc::Cfg::default(),
            input,
            output,
        )
        .provide(client)
        .await
        .map_err(error)?;
        server.serve().await.map_err(error)
    })
}

#[pymodule]
fn ene_provider(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(serve, module)?)?;
    module.add_class::<Broker>()?;
    module.add_class::<Response>()?;
    module.add_class::<Deltas>()?;
    Ok(())
}
