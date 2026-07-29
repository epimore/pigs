A Rust development scaffold, including encapsulation for logging, error handling, database operations, networking, etc.

## Managed application runtime

`base::utils::rt::GlobalRuntime` owns process signals, runtime classification, managed tasks and ordered graceful shutdown. Long-lived async and blocking work must be registered with a stable task name:

```rust
let network = GlobalRuntime::register_default(RuntimeType::CommonNetwork)?;
let cancel = network.cancel.clone();
network.spawn("service-server", async move {
    cancel.cancelled().await;
})?;

let report = GlobalRuntime::order_shutdown(&[RuntimeType::CommonNetwork]);
if !report.is_graceful() {
    return Err(/* application shutdown error */);
}
```

Runtime types describe ownership and isolation; applications only create the runtimes they need. Shutdown stages run in the declared order, stop accepting new managed tasks, cancel and wait for the current stage, and report task panics, timeouts and remaining task names. A critical server or task failure should call `GlobalRuntime::request_shutdown_with_error()`.

The application shutdown budget is eight seconds. The Unix daemon waits ten seconds after SIGTERM before escalating to SIGKILL. Blocking tasks must stop cooperatively; aborting a blocking task is not a graceful shutdown mechanism.
