use futures::Stream;
use pin_project::pin_project;
use std::{io, pin::Pin, sync::Arc, task::Poll};

use async_trait::async_trait;
use futures::{stream::FuturesUnordered, Future};
use tokio::io::AsyncWrite;

use crate::{MultiPartUpload, Result};

type BoxedTryFuture<T> = Pin<Box<dyn Future<Output = Result<T, io::Error>> + Send>>;

// Lifetimes are difficult to manage, so not using AsyncTrait
pub(crate) trait CloudMultiPartUploadImpl {
    /// Upload a single part
    fn upload_part(&self, buf: Vec<u8>, part_idx: usize) -> BoxedTryFuture<(usize, UploadPart)>;

    /// Complete the upload with the provided parts
    fn complete(&self, completed_parts: Vec<Option<UploadPart>>) -> BoxedTryFuture<()>;

    /// Cancel the upload in the cloud service
    fn abort(&self) -> BoxedTryFuture<()>;
}

#[derive(Debug, Clone)]
pub(crate) struct UploadPart {
    pub content_id: String,
}

#[pin_project]
pub(crate) struct CloudMultiPartUpload<T>
where
    T: CloudMultiPartUploadImpl,
{
    inner: Arc<T>,
    /// A list of completed parts, in sequential order.
    completed_parts: Vec<Option<UploadPart>>,
    #[pin]
    /// Part upload tasks currently running
    tasks: FuturesUnordered<BoxedTryFuture<(usize, UploadPart)>>,
    /// Maximum number of upload tasks to run concurrently
    max_concurrency: usize,
    /// Buffer that will be sent in next upload.
    current_buffer: Vec<u8>,
    /// Minimum size of a part in bytes
    min_part_size: usize,
    /// Index of current part
    current_part_idx: usize,
    #[pin]
    /// The completion task
    completion_task: Option<BoxedTryFuture<()>>,
}

impl<T> CloudMultiPartUpload<T>
where
    T: CloudMultiPartUploadImpl,
{
    pub fn new(inner: T, max_concurrency: usize) -> Self {
        Self {
            inner: Arc::new(inner),
            completed_parts: Vec::new(),
            tasks: FuturesUnordered::new(),
            max_concurrency,
            current_buffer: Vec::new(),
            // TODO: Should this vary by provider?
            // TODO: Should we automatically increase then when part index gets large?
            min_part_size: 5_000_000,
            current_part_idx: 0,
            completion_task: None,
        }
    }

    pub fn poll_tasks(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Result<(), io::Error> {
        let mut this = self.project();
        if this.tasks.is_empty() {
            return Ok(());
        }
        while let Poll::Ready(Some(res)) = Pin::new(&mut this.tasks).poll_next(cx) {
            let (part_idx, part) = res?;
            this.completed_parts.resize_with(
                std::cmp::max(part_idx + 1, this.completed_parts.len()),
                || None,
            );
            this.completed_parts[part_idx] = Some(part);
        }
        Ok(())
    }
}

#[async_trait]
impl<T> MultiPartUpload for CloudMultiPartUpload<T>
where
    T: CloudMultiPartUploadImpl + Send + Unpin + Sync,
{
    async fn abort(&mut self) -> Result<()> {
        self.tasks.clear();
        self.abort().await?;
        Ok(())
    }
}

impl<T> AsyncWrite for CloudMultiPartUpload<T>
where
    T: CloudMultiPartUploadImpl + Send + Sync,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, io::Error>> {
        // Poll current tasks
        self.as_mut().poll_tasks(cx)?;

        let this = self.as_mut().project();

        // If adding buf to pending buffer would trigger send, check
        // whether we have capacity for another task.
        let enough_to_send = (buf.len() + this.current_buffer.len()) > *this.min_part_size;
        if enough_to_send && this.tasks.len() < *this.max_concurrency {
            // If we do, copy into the buffer and submit the task, and return ready.
            this.current_buffer.extend_from_slice(buf);

            let mut cleared_buffer: Vec<u8> = Vec::new();
            std::mem::swap(this.current_buffer, &mut cleared_buffer);
            let task = this
                .inner
                .upload_part(cleared_buffer, *this.current_part_idx);
            this.tasks.push(task);
            *this.current_part_idx += 1;

            // We need to poll immediately after adding to setup waker
            self.as_mut().poll_tasks(cx)?;

            Poll::Ready(Ok(buf.len()))
        } else if !enough_to_send {
            this.current_buffer.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        } else {
            Poll::Pending
        }
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        // Poll current tasks
        self.as_mut().poll_tasks(cx)?;

        let this = self.as_mut().project();

        // If current_buffer is not empty, see if it can be submitted
        if !this.current_buffer.is_empty() && this.tasks.len() < *this.max_concurrency {
            let mut out_buffer: Vec<u8> = Vec::new();
            std::mem::swap(this.current_buffer, &mut out_buffer);
            let task = this.inner.upload_part(out_buffer, *this.current_part_idx);
            this.tasks.push(task);
        }

        self.as_mut().poll_tasks(cx)?;

        // If tasks and current_buffer are empty, return Ready
        if self.tasks.is_empty() && self.current_buffer.is_empty() {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        // First, poll flush
        match self.as_mut().poll_flush(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(res) => res?,
        };

        if self.completion_task.is_none() {
            // If shutdown task is not set, set it
            let mut parts: Vec<Option<UploadPart>> = Vec::new();
            std::mem::swap(&mut self.completed_parts, &mut parts);
            self.completion_task = Some(self.inner.complete(parts));
        }

        if let Some(task) = &mut self.completion_task {
            // If shutdown task is set, poll it
            Pin::new(task).poll(cx)
        } else {
            unreachable!("Completion task should always be set above.");
        }
    }
}
