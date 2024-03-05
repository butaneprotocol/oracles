use std::{collections::HashSet, ops::Sub, sync::Arc};

use rand::Rng;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Mutex};
use tracing::{info, trace, warn};

use crate::networking::{Message, Network};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum RaftMessage {
    Connect {
        node_id: String,
    },
    Disconnect {
        node_id: String,
    },
    RequestVote {
        from: String,
        to: String,
        term: usize,
    },
    RequestVoteResponse {
        from: String,
        to: String,
        term: usize,
        vote: bool,
    },
    Heartbeat {
        leader: String,
        to: String,
        term: usize,
    },
}

#[derive(Eq, PartialEq, Clone)]
pub enum RaftStatus {
    Follower {
        leader: Option<String>,
        voted_for: Option<String>,
    },
    Candidate {
        votes: usize,
    },
    Leader,
}

#[derive(Clone)]
pub struct RaftState {
    pub id: String,
    pub quorum: usize,
    pub peers: HashSet<String>,
    warned_about_quorum: bool,

    pub last_heartbeat: std::time::Instant,
    pub heartbeat_freq: std::time::Duration,
    pub timeout_freq: std::time::Duration,
    pub jitter: std::time::Duration,

    pub status: RaftStatus,
    pub term: usize,
}

#[derive(Clone)]
pub struct Raft {
    pub id: String,

    incoming_messages: Arc<Mutex<mpsc::Receiver<RaftMessage>>>,
    network: Network,
    state: Arc<Mutex<RaftState>>,
}

impl RaftState {
    pub fn new(
        id: &String,
        quorum: usize,
        heartbeat_freq: std::time::Duration,
        timeout_freq: std::time::Duration,
    ) -> Self {
        RaftState {
            id: id.clone(),
            quorum: quorum,
            warned_about_quorum: false,
            peers: HashSet::new(),
            last_heartbeat: std::time::Instant::now(),
            heartbeat_freq,
            timeout_freq,
            jitter: std::time::Duration::from_millis(rand::thread_rng().gen_range(50..100)),
            status: RaftStatus::Follower {
                leader: None,
                voted_for: None,
            },
            term: 0,
        }
    }

    pub fn leader(&self) -> Option<String> {
        match &self.status {
            RaftStatus::Leader => Some(self.id.clone()),
            RaftStatus::Follower {
                leader: Some(leader),
                ..
            } => Some(leader.clone()),
            _ => None,
        }
    }

    pub fn is_leader(&self) -> bool {
        matches!(self.status, RaftStatus::Leader)
    }

    pub fn receive(
        &mut self,
        timestamp: std::time::Instant,
        message: RaftMessage,
    ) -> Vec<(String, RaftMessage)> {
        match &message {
            RaftMessage::Connect { node_id } => {
                info!(me = self.id, "New peer {}", node_id);
                self.peers.insert(node_id.clone());
                if matches!(self.status, RaftStatus::Leader) {
                    // Let them know right away that we're the leader
                    let response = RaftMessage::Heartbeat {
                        leader: self.id.clone(),
                        to: node_id.clone(),
                        term: self.term,
                    };
                    vec![(node_id.clone(), response)]
                } else {
                    vec![]
                }
            }
            RaftMessage::Disconnect { node_id } => {
                info!(me = self.id, "Peer disconnected {}", node_id);
                self.peers.remove(node_id);
                vec![]
            }
            RaftMessage::Heartbeat { leader, term, .. } => {
                let current_leader = self.leader();
                if *term >= self.term {
                    self.term = *term;
                    self.status = RaftStatus::Follower {
                        leader: Some(leader.clone()),
                        voted_for: None,
                    };
                    if Some(leader.clone()) != current_leader {
                        info!(me = self.id, term = term, leader = leader, "New leader");
                    }
                    self.warned_about_quorum = false;
                }
                self.last_heartbeat = timestamp;
                vec![]
            }
            RaftMessage::RequestVote {
                from,
                term: requested_term,
                ..
            } => {
                trace!(
                    me = self.id,
                    term = requested_term,
                    "Vote requested by {}",
                    from
                );
                let peer = from.clone();
                let (vote, reason) = match self.status {
                    RaftStatus::Follower {
                        leader: _,
                        voted_for: Some(_),
                    } => (false, "Already voted"),
                    RaftStatus::Leader
                    | RaftStatus::Follower {
                        leader: _,
                        voted_for: None,
                    } => {
                        if *requested_term > self.term {
                            // Vote for the candidate
                            let current_leader = match &self.status {
                                RaftStatus::Follower {
                                    leader,
                                    voted_for: _,
                                } => leader.clone(),
                                RaftStatus::Leader => Some(self.id.clone()),
                                RaftStatus::Candidate { .. } => None,
                            };
                            self.status = RaftStatus::Follower {
                                leader: current_leader,
                                voted_for: Some(from.clone()),
                            };
                            (true, "newer term with note vote")
                        } else {
                            (false, "old term")
                        }
                    }
                    RaftStatus::Candidate { .. } => {
                        if *requested_term > self.term {
                            self.status = RaftStatus::Follower {
                                leader: None,
                                voted_for: Some(from.clone()),
                            };
                            (true, "newer term with vote")
                        } else {
                            (false, "already voted for self")
                        }
                    }
                };
                trace!(
                    me = self.id,
                    term = requested_term,
                    reason = reason,
                    vote = vote,
                    "Casting vote for {}'s election",
                    peer
                );
                let response = RaftMessage::RequestVoteResponse {
                    from: self.id.clone(),
                    to: peer.clone(),
                    term: *requested_term,
                    vote,
                };
                return vec![(peer, response)];
            }
            RaftMessage::RequestVoteResponse { term, vote, .. } => {
                match self.status {
                    RaftStatus::Candidate { votes: prev_votes } => {
                        if *term != self.term {
                            return vec![];
                        }
                        if !vote {
                            trace!(
                                me = self.id,
                                votes = prev_votes,
                                term = term,
                                my_term = self.term,
                                quorum = self.quorum,
                                "Vote rejected"
                            );
                            return vec![];
                        }

                        let new_votes = prev_votes + 1;
                        trace!(
                            me = self.id,
                            votes = new_votes,
                            quorum = self.quorum,
                            "Vote received"
                        );
                        self.status = RaftStatus::Candidate { votes: new_votes };
                        if new_votes >= self.quorum {
                            info!(
                                me = self.id,
                                term = term,
                                votes = new_votes,
                                quorum = self.quorum,
                                "Election won"
                            );
                            self.status = RaftStatus::Leader;
                            // Immediately send a heartbeat to make leader election stable
                            return self
                                .peers
                                .iter()
                                .map(|peer| {
                                    (
                                        peer.clone(),
                                        RaftMessage::Heartbeat {
                                            leader: self.id.clone(),
                                            to: peer.clone(),
                                            term: *term,
                                        },
                                    )
                                })
                                .collect();
                        } else {
                            vec![]
                        }
                    }
                    _ => {
                        if *term >= self.term {
                            warn!(
                                me = self.id,
                                term = term,
                                "Unexpected message {:?}",
                                message
                            );
                        }
                        vec![]
                    }
                }
            }
        }
    }

    pub fn tick(&mut self, timestamp: std::time::Instant) -> Vec<(String, RaftMessage)> {
        let actual_timeout = self.timeout_freq.sub(self.jitter);
        let is_leader = self.is_leader();
        let elapsed_time = timestamp.duration_since(self.last_heartbeat);
        let heartbeat_timeout = elapsed_time > self.heartbeat_freq;
        let election_timeout = elapsed_time > actual_timeout;
        let can_reach_quorum = (self.peers.len() + 1) >= self.quorum;

        if election_timeout && !can_reach_quorum {
            if !self.warned_about_quorum {
                warn!(
                    me = self.id,
                    term = self.term,
                    nodes = self.peers.len() + 1,
                    quorum = self.quorum,
                    "Timeout (with leader) reached after {:?}, but can't reach quorum",
                    actual_timeout
                );
                self.warned_about_quorum = true;
            }
            vec![]
        } else if election_timeout && can_reach_quorum {
            info!(
                me = self.id,
                term = self.term + 1,
                nodes = self.peers.len() + 1,
                quorum = self.quorum,
                last_heartbeat = format!("{:?}", self.last_heartbeat),
                "Timeout reached after {:?}, starting a new election",
                actual_timeout
            );
            // Update jitter to avoid aggressive election cycles
            self.jitter = std::time::Duration::from_millis((rand::random::<u8>() % 50 + 50) as u64);

            // Start an election
            self.status = RaftStatus::Candidate { votes: 1 };
            self.term += 1;
            self.last_heartbeat = timestamp;

            return self
                .peers
                .iter()
                .map(|peer| {
                    (
                        peer.clone(),
                        RaftMessage::RequestVote {
                            from: self.id.clone(),
                            to: peer.clone(),
                            term: self.term,
                        },
                    )
                })
                .collect();
        } else if is_leader && heartbeat_timeout {
            if self.peers.len() > 0 {
                trace!(me = self.id, "Sending heartbeats as leader");
                // Send heartbeats
                self.last_heartbeat = timestamp;
                self.peers
                    .iter()
                    .map(|peer| {
                        (
                            peer.clone(),
                            RaftMessage::Heartbeat {
                                leader: self.id.clone(),
                                to: peer.clone(),
                                term: self.term,
                            },
                        )
                    })
                    .collect()
            } else {
                vec![]
            }
        } else {
            vec![]
        }
    }
}

impl Raft {
    pub fn new(
        id: &String,
        quorum: usize,
        heartbeat_freq: std::time::Duration,
        timeout_freq: std::time::Duration,

        incoming_messages: mpsc::Receiver<RaftMessage>,
        network: Network,
    ) -> Self {
        info!(
            me = id,
            quorum = quorum,
            heartbeat = format!("{:?}", heartbeat_freq),
            timeout = format!("{:?}", timeout_freq),
            "New raft protocol"
        );
        Raft {
            id: id.clone(),
            incoming_messages: Arc::new(Mutex::new(incoming_messages)),
            network,
            state: Arc::new(Mutex::new(RaftState::new(
                &id,
                quorum,
                heartbeat_freq,
                timeout_freq,
            ))),
        }
    }

    pub async fn handle_messages(&self) {
        let mut messages = self.incoming_messages.lock().await;
        let mut state = self.state.lock().await;
        loop {
            let next_message = messages.try_recv();
            let timestamp = std::time::Instant::now();

            let responses = match next_message {
                Ok(msg) => {
                    trace!(me = self.id, "Received message: {:?}", msg);
                    state.receive(timestamp, msg)
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => state.tick(timestamp),
                Err(err) => {
                    warn!(me = self.id, "Failed to receive message: {}", err);
                    vec![]
                }
            };

            // Send out any responses
            for (peer, response) in responses {
                if let Err(_) = self.network.send(&peer, Message::Raft(response)).await {
                    state.receive(
                        timestamp,
                        RaftMessage::Disconnect {
                            node_id: peer.clone(),
                        },
                    );
                }
            }
            // Yield back to the scheduler, so that other tasks can run
            tokio::task::yield_now().await;
        }
    }
}
