use futures::stream::BoxStream;
use notify::{Event, RecursiveMode, Watcher};
use std::path::Path;
use tokio_stream::wrappers::ReceiverStream;

pub fn make_watcher(path: &Path) -> BoxStream<'static, notify::Event> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Event>(1024);

    let path = path.to_owned();
    std::thread::spawn(move || {
        let (notify_tx, notify_rx) = std::sync::mpsc::channel();

        let mut watcher = notify::recommended_watcher(notify_tx).unwrap();

        watcher.watch(&path, RecursiveMode::Recursive).unwrap();
        for res in notify_rx {
            match res {
                Ok(event) => {
                    _ = tx.blocking_send(event);
                }
                Err(err) => {
                    tracing::error!(%err, "notify error");
                }
            }
        }
    });

    Box::pin(ReceiverStream::new(rx))
}
