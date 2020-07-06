macro_rules! runtime_methods {
    () => {
        use agnostik::join_handle::JoinHandle;
        use std::future::Future;

        pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
            where
                F: Future + Send + 'static,
                F::Output: Send + 'static,
        {
            RUNTIME.spawn(future)
        }

        pub fn spawn_blocking<F, T>(task: F) -> JoinHandle<T>
            where
                F: FnOnce() -> T + Send + 'static,
                T: Send + 'static,
        {
            RUNTIME.spawn_blocking(task)
        }

        pub fn block_on<F>(future: F) -> F::Output
            where
                F: Future + Send + 'static,
                F::Output: Send + 'static,
        {
            RUNTIME.block_on(future)
        }
    };
}

#[cfg(feature = "bastion")]
pub mod runtime {
    use once_cell::sync::Lazy;
    use agnostik::{AgnostikExecutor, Agnostik};
    use agnostik::executors::{BastionExecutor};

    static RUNTIME: Lazy<BastionExecutor> = Lazy::new(|| unsafe {
        std::mem::transmute(Agnostik::bastion())
    });

    runtime_methods!();
}

#[cfg(feature = "tokio")]
pub mod runtime {
    use once_cell::sync::Lazy;
    use agnostik::{Agnostik, LocalAgnostikExecutor};
    use agnostik::executors::TokioExecutor;

    static RUNTIME: Lazy<TokioExecutor> = Lazy::new(|| unsafe {
        std::mem::transmute(Agnostik::tokio())
    });

    runtime_methods!();
}

#[cfg(feature = "asyncstd")]
pub mod runtime {
    use once_cell::sync::Lazy;
    use agnostik::{Agnostik, LocalAgnostikExecutor};
    use agnostik::executors::AsyncStdExecutor;

    static RUNTIME: Lazy<AsyncStdExecutor> = Lazy::new(|| unsafe {
        std::mem::transmute(Agnostik::async_std())
    });

    runtime_methods!();
}

#[cfg(feature = "smol")]
pub mod runtime {
    use once_cell::sync::Lazy;
    use agnostik::{Agnostik, LocalAgnostikExecutor};
    use agnostik::executors::SmolExecutor;

    static RUNTIME: Lazy<SmolExecutor> = Lazy::new(|| unsafe {
        std::mem::transmute(Agnostik::smol())
    });

    runtime_methods!();
}

pub use runtime::*;