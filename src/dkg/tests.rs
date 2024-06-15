use std::{collections::HashSet, pin::Pin};

use anyhow::Result;
use futures::{Future, FutureExt};
use tokio::select;

use crate::network::{NodeId, OutgoingMessage};

use super::{
    logic::{self, GeneratedKeys},
    KeygenMessage,
};

type TestNetwork = crate::network::TestNetwork<KeygenMessage>;

struct Participant {
    id: NodeId,
    max_signers: u16,
    min_signers: u16,
    prior_session_id: Option<String>,
    task: Pin<Box<dyn Future<Output = Result<GeneratedKeys>>>>,
}

impl Participant {
    pub fn new(network: &mut TestNetwork, max_signers: u16, min_signers: u16) -> Self {
        let (id, channel) = network.connect();
        let task = logic::run(id.clone(), channel, max_signers, min_signers).boxed();
        Self {
            id,
            max_signers,
            min_signers,
            prior_session_id: None,
            task,
        }
    }

    pub async fn run_until_round_1_sent(&mut self, network: &mut TestNetwork) {
        let sent_to = self
            .run_until_message_sent(network, |message| {
                if let KeygenMessage::Part1(_) = message.data {
                    Some(message.to)
                } else {
                    None
                }
            })
            .await;
        assert_eq!(sent_to, None);
    }

    pub async fn run_until_round_2_sent(
        &mut self,
        network: &mut TestNetwork,
        targets: Vec<&NodeId>,
    ) {
        let mut waiting_for: HashSet<NodeId> = targets.into_iter().cloned().collect();
        let mut new_session_id = None;
        while !waiting_for.is_empty() {
            let (to, session_id) = self
                .run_until_message_sent(network, |message| {
                    if let KeygenMessage::Part2(p2message) = message.data {
                        Some((message.to, p2message.session_id))
                    } else {
                        None
                    }
                })
                .await;
            if self.prior_session_id.as_ref() == Some(&session_id) {
                continue;
            }
            new_session_id = Some(session_id);
            waiting_for.remove(&to.unwrap());
        }
        self.prior_session_id = new_session_id;
    }

    pub async fn run_until_done_sent(&mut self, network: &mut TestNetwork) {
        self.run_until_message_sent(network, |message| {
            if let KeygenMessage::Done(_) = message.data {
                Some(())
            } else {
                None
            }
        })
        .await;
    }

    pub async fn run_until_shutdown(self) -> GeneratedKeys {
        self.task.await.unwrap()
    }

    pub async fn reconnect(&mut self, network: &mut TestNetwork) {
        let channel = network.reconnect(&self.id);
        let task = logic::run(self.id.clone(), channel, self.max_signers, self.min_signers).boxed();
        self.task = task;
    }

    async fn run_until_message_sent<T, F: Fn(OutgoingMessage<KeygenMessage>) -> Option<T>>(
        &mut self,
        network: &mut TestNetwork,
        cond: F,
    ) -> T {
        let id = self.id.clone();
        let get_message_task = async move {
            while let Some(message) = network.next_message_from(&id).await {
                if let Some(value) = cond(message) {
                    return value;
                }
            }
            panic!("Network has shut down!");
        };
        select! {
            value = get_message_task => value,
            result = &mut self.task => {
                match result {
                    Ok(_) => panic!("Generated keys too early"),
                    Err(err) => panic!("Error while generating keys: {:#}", err)
                }
            }
        }
    }
}

#[tokio::test]
async fn should_generate_keys() {
    let mut network = TestNetwork::new();
    let max_signers = 2;
    let min_signers = 2;
    let mut p1 = Participant::new(&mut network, max_signers, min_signers);
    let mut p2 = Participant::new(&mut network, max_signers, min_signers);

    p1.run_until_round_1_sent(&mut network).await;
    p2.run_until_round_1_sent(&mut network).await;

    p1.run_until_round_2_sent(&mut network, vec![&p2.id]).await;
    p2.run_until_round_2_sent(&mut network, vec![&p1.id]).await;

    p1.run_until_done_sent(&mut network).await;
    p2.run_until_done_sent(&mut network).await;

    let p1_keys = p1.run_until_shutdown().await;
    let p2_keys = p2.run_until_shutdown().await;
    assert_eq!(p1_keys.1, p2_keys.1);
}

#[tokio::test]
async fn should_generate_keys_after_disconnect() {
    let mut network = TestNetwork::new();
    let max_signers = 2;
    let min_signers = 2;
    let mut p1 = Participant::new(&mut network, max_signers, min_signers);
    let mut p2 = Participant::new(&mut network, max_signers, min_signers);

    p1.run_until_round_1_sent(&mut network).await;
    p2.run_until_round_1_sent(&mut network).await;

    p1.run_until_round_2_sent(&mut network, vec![&p2.id]).await;
    p2.run_until_round_2_sent(&mut network, vec![&p1.id]).await;

    p1.reconnect(&mut network).await;

    p1.run_until_round_1_sent(&mut network).await;
    p2.run_until_round_1_sent(&mut network).await;

    p1.run_until_round_2_sent(&mut network, vec![&p2.id]).await;
    p2.run_until_round_2_sent(&mut network, vec![&p1.id]).await;

    p1.run_until_done_sent(&mut network).await;
    p2.run_until_done_sent(&mut network).await;

    let p1_keys = p1.run_until_shutdown().await;
    let p2_keys = p2.run_until_shutdown().await;
    assert_eq!(p1_keys.1, p2_keys.1);
}

#[tokio::test]
async fn should_generate_keys_if_other_disconnects_after_we_finish() {
    let mut network = TestNetwork::new();
    let max_signers = 2;
    let min_signers = 2;
    let mut p1 = Participant::new(&mut network, max_signers, min_signers);
    let mut p2 = Participant::new(&mut network, max_signers, min_signers);

    p1.run_until_round_1_sent(&mut network).await;
    p2.run_until_round_1_sent(&mut network).await;

    p1.run_until_round_2_sent(&mut network, vec![&p2.id]).await;
    p2.run_until_round_2_sent(&mut network, vec![&p1.id]).await;

    p1.run_until_done_sent(&mut network).await;
    p2.reconnect(&mut network).await;

    p1.run_until_round_1_sent(&mut network).await;
    p2.run_until_round_1_sent(&mut network).await;

    p1.run_until_round_2_sent(&mut network, vec![&p2.id]).await;
    p2.run_until_round_2_sent(&mut network, vec![&p1.id]).await;

    p1.run_until_done_sent(&mut network).await;
    p2.run_until_done_sent(&mut network).await;

    let p1_keys = p1.run_until_shutdown().await;
    let p2_keys = p2.run_until_shutdown().await;
    assert_eq!(p1_keys.1, p2_keys.1);
}
