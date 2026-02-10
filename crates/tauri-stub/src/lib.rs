#[derive(Clone)]
pub struct AppHandle;

pub mod async_runtime {
    pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Handle::current().block_on(future)
    }
}

