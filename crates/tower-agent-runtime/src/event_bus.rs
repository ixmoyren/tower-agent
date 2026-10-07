//! 事件订阅：逐订阅者 mpsc + 背压（design.md D3 / Q6）。
//!
//! pi 的 `Agent.subscribe(listener)` 会 **await** 每个 listener：listener 处理完当前事件前，
//! agent 不会继续推进，也不会进入 idle。Rust 侧用「每个订阅者一条有界 `mpsc`」表达同一
//! 语义：agent 把事件连同回执一起投递，并 await 回执；订阅者在下一次拉取（或 drop）时
//! 才确认上一条已处理完。`broadcast` 的 `Lagged` 丢弃/报错语义因此被规避。

use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot};

use crate::AgentEvent;

/// 单个订阅者的通道容量。
const SUBSCRIBER_CAPACITY: usize = 16;

/// 投递给单个订阅者的一帧事件：事件本身 + 「已处理完」回执。
struct Delivery {
    event: AgentEvent,
    ack: Option<oneshot::Sender<()>>,
}

/// 多订阅者事件总线。
#[derive(Clone, Default)]
pub struct EventBus {
    inner: Arc<Mutex<BusInner>>,
}

#[derive(Default)]
struct BusInner {
    next_id: u64,
    subscribers: Vec<(u64, mpsc::Sender<Delivery>)>,
}

impl EventBus {
    /// 构造一个没有订阅者的总线。
    pub fn new() -> Self {
        Self::default()
    }

    /// 新增一个订阅者。
    pub fn subscribe(&self) -> Subscription {
        let (tx, rx) = mpsc::channel(SUBSCRIBER_CAPACITY);
        let mut inner = self.inner.lock().expect("event bus mutex poisoned");
        let id = inner.next_id;
        inner.next_id += 1;
        inner.subscribers.push((id, tx));
        drop(inner);
        Subscription {
            id,
            rx,
            outstanding: None,
            bus: self.clone(),
        }
    }

    /// 当前订阅者数量。
    pub fn subscriber_count(&self) -> usize {
        self.inner
            .lock()
            .expect("event bus mutex poisoned")
            .subscribers
            .len()
    }

    /// 向所有订阅者投递事件，并 await 它们处理完。
    ///
    /// 事件是 push 的：没有被 await 的订阅者会通过有界通道形成背压。
    pub async fn emit(&self, event: AgentEvent) {
        let targets: Vec<(u64, mpsc::Sender<Delivery>)> = {
            let inner = self.inner.lock().expect("event bus mutex poisoned");
            inner.subscribers.clone()
        };

        let mut receipts = Vec::with_capacity(targets.len());
        let mut gone = Vec::new();
        for (id, sender) in targets {
            let (ack_tx, ack_rx) = oneshot::channel();
            let delivery = Delivery {
                event: event.clone(),
                ack: Some(ack_tx),
            };
            if sender.send(delivery).await.is_err() {
                gone.push(id);
                continue;
            }
            receipts.push(ack_rx);
        }

        for receipt in receipts {
            let _ = receipt.await;
        }

        if !gone.is_empty() {
            let mut inner = self.inner.lock().expect("event bus mutex poisoned");
            inner.subscribers.retain(|(id, _)| !gone.contains(id));
        }
    }

    fn unsubscribe(&self, id: u64) {
        let mut inner = self.inner.lock().expect("event bus mutex poisoned");
        inner
            .subscribers
            .retain(|(subscriber, _)| *subscriber != id);
    }
}

/// 一个订阅者的接收端。
///
/// 调用 [`Subscription::recv`] 拉取下一个事件；上一次拉取到的事件会在下一次 `recv`
/// 开始时（或在 `Subscription` drop 时）被确认，agent 收到确认后才继续推进。
pub struct Subscription {
    id: u64,
    rx: mpsc::Receiver<Delivery>,
    outstanding: Option<oneshot::Sender<()>>,
    bus: EventBus,
}

impl Subscription {
    /// 拉取下一个事件。
    pub async fn recv(&mut self) -> Option<AgentEvent> {
        self.acknowledge();
        let delivery = self.rx.recv().await?;
        let event = delivery.event;
        self.outstanding = delivery.ack;
        Some(event)
    }

    fn acknowledge(&mut self) {
        if let Some(ack) = self.outstanding.take() {
            let _ = ack.send(());
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.acknowledge();
        self.bus.unsubscribe(self.id);
    }
}

#[cfg(test)]
mod tests {
    use crate::AgentEvent;

    use super::EventBus;

    #[tokio::test]
    async fn emit_waits_for_the_subscriber_to_consume() {
        let bus = EventBus::new();
        let mut subscription = bus.subscribe();

        let emitted = tokio::spawn({
            let bus = bus.clone();
            async move {
                bus.emit(AgentEvent::AgentStart).await;
                bus.emit(AgentEvent::TurnStart).await;
            }
        });

        // 第一条事件会在订阅者下一次 recv 时被确认，第二条才会投递到达。
        assert_eq!(subscription.recv().await, Some(AgentEvent::AgentStart));
        assert_eq!(subscription.recv().await, Some(AgentEvent::TurnStart));

        // 最后一条事件在订阅者 drop 时才被确认，agent 由此感知 listener 已结算。
        drop(subscription);
        emitted.await.expect("emit task joins");
    }

    #[tokio::test]
    async fn dropping_a_subscription_unblocks_the_bus() {
        let bus = EventBus::new();
        let subscription = bus.subscribe();
        drop(subscription);

        bus.emit(AgentEvent::AgentStart).await;
        assert_eq!(bus.subscriber_count(), 0);
    }
}
