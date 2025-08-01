use super::macros::{check_running, poll_ok};
use crate::{DownloadResult, Event, ProgressEntry, RandReader, RandWriter, Total, WorkerId};
use bytes::Bytes;
use core::{num::NonZeroUsize, sync::atomic::AtomicBool, time::Duration};
use fast_steal::{SplitTask, StealTask, Task, TaskList};
use futures::{TryStreamExt, lock::Mutex};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct DownloadOptions {
    pub download_chunks: Vec<ProgressEntry>,
    pub concurrent: NonZeroUsize,
    pub retry_gap: Duration,
    pub write_queue_cap: usize,
}

pub async fn download_multi<R, W>(
    reader: R,
    mut writer: W,
    options: DownloadOptions,
) -> DownloadResult<R::Error, W::Error>
where
    R: RandReader + 'static,
    W: RandWriter + 'static,
{
    let (tx, event_chain) = kanal::unbounded_async();
    let (tx_write, rx_write) =
        kanal::bounded_async::<(WorkerId, ProgressEntry, Bytes)>(options.write_queue_cap);
    let tx_clone = tx.clone();
    let handle = tokio::spawn(async move {
        while let Ok((id, spin, data)) = rx_write.recv().await {
            poll_ok!(
                {},
                writer.write(spin.clone(), data.clone()).await,
                id @ tx_clone => WriteError,
                options.retry_gap
            );
            tx_clone.send(Event::WriteProgress(id, spin)).await.unwrap();
        }
        poll_ok!(
            {},
            writer.flush().await,
            tx_clone => FlushError,
            options.retry_gap
        );
    });
    let mutex = Arc::new(Mutex::new(()));
    let task_list = Arc::new(TaskList::from(options.download_chunks));
    let tasks = Arc::new(
        Task::from(&*task_list)
            .split_task(options.concurrent.get() as u64)
            .map(Arc::new)
            .collect::<Vec<_>>(),
    );
    let running = Arc::new(AtomicBool::new(true));
    for (id, task) in tasks.iter().enumerate() {
        let task = task.clone();
        let tasks = tasks.clone();
        let task_list = task_list.clone();
        let mutex = mutex.clone();
        let tx = tx.clone();
        let running = running.clone();
        let mut reader = reader.clone();
        let tx_write = tx_write.clone();
        tokio::spawn(async move {
            'steal_task: loop {
                check_running!(id, running, tx);
                let mut start = task.start();
                if start >= task.end() {
                    let guard = mutex.lock().await;
                    if task.steal(&tasks, 16 * 1024) {
                        continue;
                    }
                    drop(guard);
                    tx.send(Event::Finished(id)).await.unwrap();
                    return;
                }
                let download_range = &task_list.get_range(start..task.end());
                for range in download_range {
                    check_running!(id, running, tx);
                    tx.send(Event::Reading(id)).await.unwrap();
                    let mut stream = reader.read(range);
                    let mut downloaded = 0;
                    loop {
                        check_running!(id, running, tx);
                        match stream.try_next().await {
                            Ok(chunk) => match chunk {
                                Some(mut chunk) => {
                                    let len = chunk.len() as u64;
                                    task.fetch_add_start(len);
                                    start += len;
                                    let range_start = range.start + downloaded;
                                    downloaded += len;
                                    let range_end = range.start + downloaded;
                                    let span =
                                        range_start..range_end.min(task_list.get(task.end()));
                                    let len = span.total() as usize;
                                    tx.send(Event::ReadProgress(id, span.clone()))
                                        .await
                                        .unwrap();
                                    tx_write
                                        .send((id, span, chunk.split_to(len)))
                                        .await
                                        .unwrap();
                                    if start >= task.end() {
                                        continue 'steal_task;
                                    }
                                }
                                None => break,
                            },
                            Err(e) => tx.send(Event::ReadError(id, e)).await.unwrap(),
                        }
                    }
                }
            }
        });
    }
    DownloadResult::new(event_chain, handle, running)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        MergeProgress, ProgressEntry,
        core::mock::{MockRandReader, MockRandWriter, build_mock_data},
    };

    #[tokio::test]
    async fn test_concurrent_pulling() {
        let mock_data = build_mock_data(3 * 1024);
        let reader = MockRandReader::new(mock_data.clone());
        let writer = MockRandWriter::new(mock_data.clone());
        #[allow(clippy::single_range_in_vec_init)]
        let download_chunks = vec![0..mock_data.len() as u64];
        let result = download_multi(
            reader,
            writer.clone(),
            DownloadOptions {
                concurrent: NonZeroUsize::new(32).unwrap(),
                retry_gap: Duration::from_secs(1),
                write_queue_cap: 1024,
                download_chunks: download_chunks.clone(),
            },
        )
        .await;

        let mut download_progress: Vec<ProgressEntry> = Vec::new();
        let mut write_progress: Vec<ProgressEntry> = Vec::new();
        while let Ok(e) = result.event_chain.recv().await {
            match e {
                Event::ReadProgress(_, p) => {
                    download_progress.merge_progress(p);
                }
                Event::WriteProgress(_, p) => {
                    write_progress.merge_progress(p);
                }
                _ => {}
            }
        }
        dbg!(&download_progress);
        dbg!(&write_progress);
        assert_eq!(download_progress, download_chunks);
        assert_eq!(write_progress, download_chunks);

        result.join().await.unwrap();
        writer.assert().await;
    }
}
