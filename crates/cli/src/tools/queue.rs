//! 同一文件的写操作串行化（对应 pi `coding-agent` 的 `core/tools/file-mutation-queue.ts`）。
//!
//! 针对同一路径的修改串行执行，不同路径仍可并行。key 用真实路径（符号链接解析后），
//! 这样同一文件的两种写法不会绕过串行化。

use std::{
    collections::HashMap,
    future::Future,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use tokio::sync::Mutex as AsyncMutex;

/// 按文件串行化的修改队列。
#[derive(Clone, Default)]
pub struct FileMutationQueue {
    queues: Arc<Mutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>>>,
}

impl std::fmt::Debug for FileMutationQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileMutationQueue").finish_non_exhaustive()
    }
}

impl FileMutationQueue {
    /// 构造一个空队列。
    pub fn new() -> Self {
        Self::default()
    }

    /// 在队列中执行 `f`，保证同一文件上的修改互不并发。
    pub async fn run<F, Fut, T>(&self, path: &Path, f: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T>,
    {
        let key = queue_key(path).await;
        let lock = {
            let mut queues = self.queues.lock().expect("file mutation queue poisoned");
            Arc::clone(
                queues
                    .entry(key.clone())
                    .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
            )
        };

        let guard = lock.lock().await;
        let result = f().await;
        drop(guard);

        // 没有等待者时清掉条目，避免长期运行的进程里不断堆积路径。
        let mut queues = self.queues.lock().expect("file mutation queue poisoned");
        if let Some(existing) = queues.get(&key)
            && Arc::ptr_eq(existing, &lock)
            && Arc::strong_count(existing) <= 2
        {
            queues.remove(&key);
        }
        result
    }
}

async fn queue_key(path: &Path) -> PathBuf {
    match tokio::fs::canonicalize(path).await {
        Ok(canonical) => canonical,
        // 目标还不存在（ENOENT/ENOTDIR）时退回绝对路径。
        Err(_) => match path.canonicalize() {
            Ok(canonical) => canonical,
            Err(_) => {
                if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    std::env::current_dir()
                        .map_or_else(|_| path.to_path_buf(), |cwd| cwd.join(path))
                }
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use super::FileMutationQueue;

    #[tokio::test]
    async fn same_path_is_serialized() {
        let queue = FileMutationQueue::new();
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..4 {
            let queue = queue.clone();
            let active = Arc::clone(&active);
            let max_active = Arc::clone(&max_active);
            handles.push(tokio::spawn(async move {
                queue
                    .run(std::path::Path::new("/tmp/queue-test-file"), || async {
                        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                        max_active.fetch_max(now, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        active.fetch_sub(1, Ordering::SeqCst);
                    })
                    .await;
            }));
        }
        for handle in handles {
            handle.await.expect("task joins");
        }
        assert_eq!(
            max_active.load(Ordering::SeqCst),
            1,
            "no concurrent mutation"
        );
    }

    #[tokio::test]
    async fn different_paths_run_in_parallel() {
        let queue = FileMutationQueue::new();
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let mut handles = Vec::new();
        for name in ["/tmp/queue-a", "/tmp/queue-b"] {
            let queue = queue.clone();
            let barrier = Arc::clone(&barrier);
            handles.push(tokio::spawn(async move {
                queue
                    .run(std::path::Path::new(name), || async {
                        barrier.wait().await;
                    })
                    .await;
            }));
        }
        for handle in handles {
            handle.await.expect("task joins");
        }
    }
}
