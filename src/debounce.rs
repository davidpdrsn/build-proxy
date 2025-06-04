use std::{
    pin::Pin,
    task::{self, Poll},
    time::{Duration, Instant},
};

use pin_project_lite::pin_project;
use tracing::info;

#[extend::ext(pub)]
impl<S> S
where
    S: futures::Stream,
{
    fn debounce(self, duration: Duration) -> DebounceStream<S> {
        DebounceStream {
            stream: self,
            duration,
            last_item_yielded_at: None,
        }
    }
}

pin_project! {
    pub struct DebounceStream<S> {
        #[pin]
        stream: S,
        duration: Duration,
        last_item_yielded_at: Option<Instant>,
    }
}

impl<S> futures::Stream for DebounceStream<S>
where
    S: futures::Stream,
{
    type Item = S::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        let item = futures::ready!(this.stream.poll_next(cx));

        if let Some(last_item_yielded_at) = this.last_item_yielded_at {
            if last_item_yielded_at.elapsed() < *this.duration {
                info!("bouncing...");
                return Poll::Pending;
            }
        }

        *this.last_item_yielded_at = Some(Instant::now());
        Poll::Ready(item)
    }
}
