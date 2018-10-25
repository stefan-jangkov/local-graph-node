use slog::Logger;
use std::fmt::Debug;
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::prelude::*;
use tokio::timer::DeadlineError;
use tokio_retry::strategy::{jitter, ExponentialBackoff};
use tokio_retry::Error as RetryError;
use tokio_retry::Retry;

/// Generic helper function for retrying async operations with built-in logging.
///
/// To use this helper, do the following:
///
/// 1. Call this function with an operation name (used for logging) and a `Logger`.
/// 2. Chain a call to `.when_err()` or `.when(...)`.
/// 3. Optional: call `.log_after(...)` or `.no_logging()`.
/// 4. Call either `.limit(...)` or `.no_limit()`.
/// 5. Call one of `.timeout_secs(...)`, `.timeout_millis(...)`, `.timeout(...)`, and
///    `.no_timeout()`.
/// 6. Call `.run(...)`.
///
/// All steps are required, except Step 3.
///
/// Example usage:
/// ```
/// # extern crate graph;
/// # use graph::prelude::*;
/// # use graph::tokio::timer::DeadlineError;
/// #
/// # type Memes = (); // the memes are a lie :(
/// #
/// # fn download_the_memes() -> impl Future<Item=(), Error=()> {
/// #     future::ok(())
/// # }
///
/// fn async_function(logger: Logger) -> impl Future<Item=Memes, Error=DeadlineError<()>> {
///     retry("download memes", logger.clone())
///         .when_err() // Retry on all errors
///         .no_limit() // Retry forever
///         .timeout_secs(30) // Retry if an attempt takes > 30 seconds
///         .run(|| {
///             download_the_memes() // Return a Future
///         })
/// }
/// ```
pub fn retry(operation_name: impl ToString, logger: Logger) -> RetryConfig {
    RetryConfig {
        operation_name: operation_name.to_string(),
        logger,
    }
}

pub struct RetryConfig {
    operation_name: String,
    logger: Logger,
}

impl RetryConfig {
    /// Retry any time the future resolves to an error (or on time out).
    ///
    /// See `.when(...)` for fine-grained control over when to retry.
    pub fn when_err<I, E>(self) -> RetryConfigWithPredicate<impl Fn(&Result<I, E>) -> bool, I, E> {
        self.when(|result: &Result<I, E>| result.is_err())
    }

    /// Sets a function used to determine if a retry is needed.
    /// Note: timeouts always trigger a retry.
    pub fn when<P, I, E>(self, predicate: P) -> RetryConfigWithPredicate<P, I, E>
    where
        P: Fn(&Result<I, E>) -> bool,
    {
        RetryConfigWithPredicate {
            inner: self,
            predicate,
            log_after: 1,
            limit: RetryConfigProperty::Unknown,
            phantom_item: PhantomData,
            phantom_error: PhantomData,
        }
    }
}

pub struct RetryConfigWithPredicate<P, I, E>
where
    P: Fn(&Result<I, E>) -> bool,
{
    inner: RetryConfig,
    predicate: P,
    log_after: u64,
    limit: RetryConfigProperty<usize>,
    phantom_item: PhantomData<I>,
    phantom_error: PhantomData<E>,
}

impl<P, I, E> RetryConfigWithPredicate<P, I, E>
where
    P: Fn(&Result<I, E>) -> bool,
    I: Send,
    E: Send,
{
    /// Only log retries after `min_attempts` failed attempts.
    pub fn log_after(mut self, min_attempts: u64) -> Self {
        self.log_after = min_attempts;
        self
    }

    /// Never log failed attempts.
    /// May still log at `trace` logging level.
    pub fn no_logging(mut self) -> Self {
        self.log_after = u64::max_value();
        self
    }

    /// Set a limit on how many retry attempts to make.
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit.set(limit);
        self
    }

    /// Allow unlimited retry attempts.
    pub fn no_limit(mut self) -> Self {
        self.limit.clear();
        self
    }

    /// Set how long (in seconds) to wait for an attempt to complete before giving up on that
    /// attempt.
    pub fn timeout_secs(self, timeout_secs: u64) -> RetryConfigWithTimeout<P, I, E> {
        self.timeout(Duration::from_secs(timeout_secs))
    }

    /// Set how long (in milliseconds) to wait for an attempt to complete before giving up on that
    /// attempt.
    pub fn timeout_millis(self, timeout_ms: u64) -> RetryConfigWithTimeout<P, I, E> {
        self.timeout(Duration::from_millis(timeout_ms))
    }

    /// Set how long to wait for an attempt to complete before giving up on that attempt.
    pub fn timeout(self, timeout: Duration) -> RetryConfigWithTimeout<P, I, E> {
        RetryConfigWithTimeout {
            inner: self,
            timeout,
        }
    }

    /// Allow attempts to take as long as they need (or potentially hang forever).
    pub fn no_timeout(self) -> RetryConfigNoTimeout<P, I, E> {
        RetryConfigNoTimeout { inner: self }
    }
}

pub struct RetryConfigWithTimeout<P, I, E>
where
    P: Fn(&Result<I, E>) -> bool,
{
    inner: RetryConfigWithPredicate<P, I, E>,
    timeout: Duration,
}

impl<P, I, E> RetryConfigWithTimeout<P, I, E>
where
    P: Fn(&Result<I, E>) -> bool + Send + Sync,
    I: Debug + Send,
    E: Debug + Send,
{
    /// Rerun the provided function as many times as needed.
    pub fn run<F, R>(self, try_it: F) -> impl Future<Item = I, Error = DeadlineError<E>>
    where
        F: Fn() -> R + Send,
        R: Future<Item = I, Error = E> + Send,
    {
        let operation_name = self.inner.inner.operation_name;
        let logger = self.inner.inner.logger.clone();
        let predicate = self.inner.predicate;
        let log_after = self.inner.log_after;
        let limit_opt = self.inner.limit.unwrap(&operation_name, "limit");
        let timeout = self.timeout;

        trace!(logger, "Run with retry: {}", operation_name);

        run_retry(
            operation_name,
            logger,
            predicate,
            log_after,
            limit_opt,
            move || try_it().deadline(Instant::now() + timeout),
        )
    }
}

pub struct RetryConfigNoTimeout<P, I, E>
where
    P: Fn(&Result<I, E>) -> bool,
{
    inner: RetryConfigWithPredicate<P, I, E>,
}

impl<P, I, E> RetryConfigNoTimeout<P, I, E>
where
    P: Fn(&Result<I, E>) -> bool + Send + Sync,
{
    /// Rerun the provided function as many times as needed.
    pub fn run<F, R>(self, try_it: F) -> impl Future<Item = I, Error = E>
    where
        I: Debug + Send,
        E: Debug + Send,
        F: Fn() -> R + Send,
        R: Future<Item = I, Error = E> + Send,
    {
        let operation_name = self.inner.inner.operation_name;
        let logger = self.inner.inner.logger.clone();
        let predicate = self.inner.predicate;
        let log_after = self.inner.log_after;
        let limit_opt = self.inner.limit.unwrap(&operation_name, "limit");

        trace!(logger, "Run with retry: {}", operation_name);

        run_retry(
            operation_name,
            logger,
            predicate,
            log_after,
            limit_opt,
            move || {
                try_it().map_err(|e| {
                    // No timeout, so all errors are inner errors
                    DeadlineError::inner(e)
                })
            },
        ).map_err(|e| {
            // No timeout, so all errors are inner errors
            e.into_inner().unwrap()
        })
    }
}

fn run_retry<P, I, E, F, R>(
    operation_name: String,
    logger: Logger,
    predicate: P,
    log_after: u64,
    limit_opt: Option<usize>,
    try_it_with_deadline: F,
) -> impl Future<Item = I, Error = DeadlineError<E>> + Send
where
    I: Debug + Send,
    E: Debug + Send,
    P: Fn(&Result<I, E>) -> bool + Send + Sync,
    F: Fn() -> R + Send,
    R: Future<Item = I, Error = DeadlineError<E>> + Send,
{
    let predicate = Arc::new(predicate);

    let mut attempt_count = 0;
    Retry::spawn(retry_strategy(limit_opt), move || {
        let operation_name = operation_name.clone();
        let logger = logger.clone();
        let predicate = predicate.clone();

        attempt_count += 1;

        try_it_with_deadline().then(move |result_with_deadline| {
            let is_elapsed = result_with_deadline
                .as_ref()
                .err()
                .map(|e| e.is_elapsed())
                .unwrap_or(false);
            let is_timer_err = result_with_deadline
                .as_ref()
                .err()
                .map(|e| e.is_timer())
                .unwrap_or(false);

            if is_elapsed {
                if attempt_count >= log_after {
                    debug!(
                        logger,
                        "Trying again after {} timed out (attempt #{})",
                        &operation_name,
                        attempt_count + 1,
                    );
                }

                // Wrap in Err to force retry
                Err(result_with_deadline)
            } else if is_timer_err {
                // Should never happen
                let timer_error = result_with_deadline.unwrap_err().into_timer().unwrap();
                panic!("tokio timer error: {}", timer_error)
            } else {
                // Any error must now be an inner error.
                // Unwrap the inner error so that the predicate doesn't need to think
                // about DeadlineError.
                let result = result_with_deadline.map_err(|e| e.into_inner().unwrap());

                // If needs retry
                if predicate(&result) {
                    if attempt_count >= log_after {
                        debug!(
                            logger,
                            "Trying again after {} failed (attempt #{})",
                            &operation_name,
                            attempt_count + 1,
                        );
                    }

                    // Wrap in Err to force retry
                    Err(result.map_err(|e| DeadlineError::inner(e)))
                } else {
                    // Wrap in Ok to prevent retry
                    Ok(result.map_err(|e| DeadlineError::inner(e)))
                }
            }
        })
    }).then(|retry_result| {
        // Unwrap the inner result.
        // The outer Ok/Err is only used for retry control flow.
        match retry_result {
            Ok(r) => r,
            Err(RetryError::OperationError(r)) => r,
            Err(RetryError::TimerError(e)) => panic!("tokio timer error: {}", e),
        }
    })
}

fn retry_strategy(limit_opt: Option<usize>) -> Box<Iterator<Item = Duration> + Send> {
    // Exponential backoff, but with a maximum
    let max_delay_ms = 30_000;
    let backoff = ExponentialBackoff::from_millis(2)
        .max_delay(Duration::from_millis(max_delay_ms))
        .map(jitter);

    // Apply limit (maximum retry count)
    match limit_opt {
        Some(limit) => {
            // Items are delays *between* attempts,
            // so subtract 1 from limit.
            Box::new(backoff.take(limit - 1))
        }
        None => Box::new(backoff),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RetryConfigProperty<V> {
    /// Property was explicitly set
    Set(V),

    /// Property was explicitly unset
    Clear,

    /// Property was not explicitly set or unset
    Unknown,
}

impl<V> RetryConfigProperty<V>
where
    V: PartialEq + Eq,
{
    fn set(&mut self, v: V) {
        if *self != RetryConfigProperty::Unknown {
            panic!("Retry config properties must be configured only once");
        }

        *self = RetryConfigProperty::Set(v);
    }

    fn clear(&mut self) {
        if *self != RetryConfigProperty::Unknown {
            panic!("Retry config properties must be configured only once");
        }

        *self = RetryConfigProperty::Clear;
    }

    fn unwrap(self, operation_name: &str, property_name: &str) -> Option<V> {
        match self {
            RetryConfigProperty::Set(v) => Some(v),
            RetryConfigProperty::Clear => None,
            RetryConfigProperty::Unknown => panic!(
                "Retry helper for {} must have {} parameter configured",
                operation_name, property_name
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    #[test]
    fn test() {
        let logger = Logger::root(::slog::Discard, o!());
        let mut runtime = ::tokio::runtime::Runtime::new().unwrap();

        let result = runtime.block_on(future::lazy(|| {
            let c = Mutex::new(0);
            retry("test", logger)
                .when_err()
                .no_logging()
                .no_limit()
                .no_timeout()
                .run(move || {
                    let mut c_guard = c.lock().unwrap();
                    *c_guard += 1;

                    if *c_guard >= 10 {
                        future::ok(*c_guard)
                    } else {
                        future::err(())
                    }
                })
        }));
        assert_eq!(result, Ok(10));
    }

    #[test]
    fn limit_reached() {
        let logger = Logger::root(::slog::Discard, o!());
        let mut runtime = ::tokio::runtime::Runtime::new().unwrap();

        let result = runtime.block_on(future::lazy(|| {
            let c = Mutex::new(0);
            retry("test", logger)
                .when_err()
                .no_logging()
                .limit(5)
                .no_timeout()
                .run(move || {
                    let mut c_guard = c.lock().unwrap();
                    *c_guard += 1;

                    if *c_guard >= 10 {
                        future::ok(*c_guard)
                    } else {
                        future::err(*c_guard)
                    }
                })
        }));
        assert_eq!(result, Err(5));
    }

    #[test]
    fn limit_not_reached() {
        let logger = Logger::root(::slog::Discard, o!());
        let mut runtime = ::tokio::runtime::Runtime::new().unwrap();

        let result = runtime.block_on(future::lazy(|| {
            let c = Mutex::new(0);
            retry("test", logger)
                .when_err()
                .no_logging()
                .limit(20)
                .no_timeout()
                .run(move || {
                    let mut c_guard = c.lock().unwrap();
                    *c_guard += 1;

                    if *c_guard >= 10 {
                        future::ok(*c_guard)
                    } else {
                        future::err(*c_guard)
                    }
                })
        }));
        assert_eq!(result, Ok(10));
    }

    #[test]
    fn custom_when() {
        let logger = Logger::root(::slog::Discard, o!());
        let mut runtime = ::tokio::runtime::Runtime::new().unwrap();

        let result = runtime.block_on(future::lazy(|| {
            let c = Mutex::new(0);

            retry("test", logger)
                .when(|result| result.unwrap() < 10)
                .no_logging()
                .limit(20)
                .no_timeout()
                .run(move || {
                    let mut c_guard = c.lock().unwrap();
                    *c_guard += 1;
                    if *c_guard > 30 {
                        future::err(())
                    } else {
                        future::ok(*c_guard)
                    }
                })
        }));
        assert_eq!(result, Ok(10));
    }
}
