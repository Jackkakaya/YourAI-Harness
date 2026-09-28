//! Local patch: deadlines around HTTP response headers and each raw body read.
//! Parsing/heartbeats must never change the meaning of transport progress.
use bytes::Bytes;
use futures::{StreamExt, stream::BoxStream};
use std::{fmt, time::Duration};

#[derive(Debug)]
pub enum StreamTimeout {
	Headers,
	Read,
}
impl fmt::Display for StreamTimeout {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(match self {
			Self::Headers => "model response headers timed out",
			Self::Read => "model response body read timed out",
		})
	}
}
impl std::error::Error for StreamTimeout {}

#[derive(Debug)]
pub(crate) enum TransportError {
	Http(reqwest::Error),
	Timeout(StreamTimeout),
}
impl fmt::Display for TransportError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Http(e) => e.fmt(f),
			Self::Timeout(e) => e.fmt(f),
		}
	}
}
impl std::error::Error for TransportError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		Some(match self {
			Self::Http(e) => e,
			Self::Timeout(e) => e,
		})
	}
}

pub(crate) struct StreamRequest {
	request: reqwest::RequestBuilder,
	header_timeout: Option<Duration>,
	read_timeout: Option<Duration>,
}
impl StreamRequest {
	pub(crate) fn new(
		request: reqwest::RequestBuilder,
		header_timeout: Option<Duration>,
		read_timeout: Option<Duration>,
	) -> Self {
		Self {
			request,
			header_timeout,
			read_timeout,
		}
	}
	pub(crate) async fn send(self) -> Result<StreamResponse, TransportError> {
		let response = match self.header_timeout {
			Some(timeout) => tokio::time::timeout(timeout, self.request.send())
				.await
				.map_err(|_| TransportError::Timeout(StreamTimeout::Headers))?,
			None => self.request.send().await,
		}
		.map_err(TransportError::Http)?;
		Ok(StreamResponse {
			response,
			read_timeout: self.read_timeout,
		})
	}
}

pub(crate) struct StreamResponse {
	response: reqwest::Response,
	read_timeout: Option<Duration>,
}
impl StreamResponse {
	pub(crate) fn headers(&self) -> &reqwest::header::HeaderMap {
		self.response.headers()
	}
	pub(crate) fn status(&self) -> reqwest::StatusCode {
		self.response.status()
	}
	pub(crate) fn bytes_stream(self) -> BoxStream<'static, Result<Bytes, TransportError>> {
		let timeout = self.read_timeout;
		let stream = self.response.bytes_stream().boxed();
		futures::stream::unfold(Some(stream), move |state| async move {
			let mut stream = state?;
			let next = match timeout {
				Some(timeout) => match tokio::time::timeout(timeout, stream.next()).await {
					Ok(next) => next,
					Err(_) => return Some((Err(TransportError::Timeout(StreamTimeout::Read)), None)),
				},
				None => stream.next().await,
			};
			match next {
				Some(Ok(bytes)) => Some((Ok(bytes), Some(stream))),
				Some(Err(e)) => Some((Err(TransportError::Http(e)), None)),
				None => None,
			}
		})
		.boxed()
	}
	pub(crate) async fn text(self) -> Result<String, TransportError> {
		let mut stream = self.bytes_stream();
		let mut bytes = Vec::new();
		while let Some(chunk) = stream.next().await {
			bytes.extend_from_slice(&chunk?);
		}
		Ok(String::from_utf8_lossy(&bytes).into_owned())
	}
}
