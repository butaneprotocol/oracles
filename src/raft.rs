use std::{collections::HashSet, ops::Sub};

use rand::Rng;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tracing::{info, trace, warn};

use crate::network::{IncomingMessage, Network, NetworkChannel, NodeId};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum RaftMessage {
    Connect,
    Disconnect,
    RequestVote { term: usize },
    RequestVoteResponse { term: usize, vote: bool },
    Heartbeat { term: usize },
}

#[derive(Eq, PartialEq, Clone)]
pub enum RaftStatus {
    Follower {
        leader: Option<NodeId>,
        voted_for: Option<NodeId>,
    },
    Candidate {
        votes: usize,
    },
    Leader,
}

#[derive(Eq, PartialEq, Clone, Debug)]
pub enum RaftLeader {
    Myself,
    Other(NodeId),
    Unknown,
}

pub struct RaftState {
    pub id: NodeId,
    pub quorum: usize,
    pub peers: HashSet<NodeId>,
    warned_about_quorum: bool,

    pub last_heartbeat: std::time::Instant,
    pub heartbeat_freq: std::time::Duration,
    pub timeout_freq: std::time::Duration,
    pub jitter: std::time::Duration,

    pub status: RaftStatus,
    pub term: usize,

    pub leader_sink: watch::Sender<RaftLeader>,
}

pub struct Raft {
    pub id: NodeId,

    channel: NetworkChannel<RaftMessage>,
    state: RaftState,
}

impl RaftState {
    pub fn new(
        id: NodeId,
        quorum: usize,
        heartbeat_freq: std::time::Duration,
        timeout_freq: std::time::Duration,
        leader_sink: watch::Sender<RaftLeader>,
    ) -> Self {
        RaftState {
            id,
            quorum,
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
            leader_sink,
        }
    }

    pub fn leader(&self) -> Option<NodeId> {
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
        message: IncomingMessage<RaftMessage>,
    ) -> Vec<(NodeId, RaftMessage)> {
        let from = message.from;
        match &message.data {
            RaftMessage::Connect => {
                info!("New peer {}", from);
                self.peers.insert(from.clone());
                if matches!(self.status, RaftStatus::Leader) {
                    // Let them know right away that we're the leader
                    let response = RaftMessage::Heartbeat { term: self.term };
                    vec![(from.clone(), response)]
                } else {
                    vec![]
                }
            }
            RaftMessage::Disconnect => {
                info!("Peer disconnected {}", from);
                self.peers.remove(&from);
                if self.peers.len() < self.quorum - 1 {
                    info!("Too few peers connected, raft status unknown");
                    self.clear_status();
                }
                vec![]
            }
            RaftMessage::Heartbeat { term } => {
                let current_leader = self.leader();
                if *term >= self.term {
                    self.term = *term;
                    self.set_status(RaftStatus::Follower {
                        leader: Some(from.clone()),
                        voted_for: None,
                    });
                    if Some(from.clone()) != current_leader {
                        info!(term = term, leader = %from, "New leader");
                    }
                    self.warned_about_quorum = false;
                }
                self.last_heartbeat = timestamp;
                vec![]
            }
            RaftMessage::RequestVote {
                term: requested_term,
            } => {
                trace!(term = requested_term, "Vote requested by {}", from);
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
                            self.set_status(RaftStatus::Follower {
                                leader: current_leader,
                                voted_for: Some(from.clone()),
                            });
                            (true, "newer term with note vote")
                        } else {
                            (false, "old term")
                        }
                    }
                    RaftStatus::Candidate { .. } => {
                        if *requested_term > self.term {
                            self.set_status(RaftStatus::Follower {
                                leader: None,
                                voted_for: Some(from.clone()),
                            });
                            (true, "newer term with vote")
                        } else {
                            (false, "already voted for self")
                        }
                    }
                };
                trace!(
                    term = requested_term,
                    reason = reason,
                    vote = vote,
                    "Casting vote for {}'s election",
                    peer
                );
                let response = RaftMessage::RequestVoteResponse {
                    term: *requested_term,
                    vote,
                };
                vec![(peer, response)]
            }
            RaftMessage::RequestVoteResponse { term, vote, .. } => {
                match self.status {
                    RaftStatus::Candidate { votes: prev_votes } => {
                        if *term != self.term {
                            return vec![];
                        }
                        if !vote {
                            trace!(
                                votes = prev_votes,
                                term = term,
                                my_term = self.term,
                                quorum = self.quorum,
                                "Vote rejected"
                            );
                            return vec![];
                        }

                        let new_votes = prev_votes + 1;
                        trace!(votes = new_votes, quorum = self.quorum, "Vote received");
                        self.set_status(RaftStatus::Candidate { votes: new_votes });
                        if new_votes >= self.quorum {
                            info!(
                                term = term,
                                votes = new_votes,
                                quorum = self.quorum,
                                "Election won"
                            );
                            self.set_status(RaftStatus::Leader);
                            // Immediately send a heartbeat to make leader election stable
                            return self
                                .peers
                                .iter()
                                .map(|peer| (peer.clone(), RaftMessage::Heartbeat { term: *term }))
                                .collect();
                        } else {
                            vec![]
                        }
                    }
                    _ => {
                        if *term >= self.term {
                            warn!(term = term, from = %from, "Unexpected message {:?}", message.data);
                        }
                        vec![]
                    }
                }
            }
        }
    }

    pub fn tick(&mut self, timestamp: std::time::Instant) -> Vec<(NodeId, RaftMessage)> {
        let actual_timeout = self.timeout_freq.sub(self.jitter);
        let is_leader = self.is_leader();
        let elapsed_time = timestamp.duration_since(self.last_heartbeat);
        let heartbeat_timeout = elapsed_time > self.heartbeat_freq;
        let election_timeout = elapsed_time > actual_timeout;
        let can_reach_quorum = (self.peers.len() + 1) >= self.quorum;

        if election_timeout && !can_reach_quorum {
            if !self.warned_about_quorum {
                warn!(
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
            self.set_status(RaftStatus::Candidate { votes: 1 });
            self.term += 1;
            self.last_heartbeat = timestamp;

            return self
                .peers
                .iter()
                .map(|peer| (peer.clone(), RaftMessage::RequestVote { term: self.term }))
                .collect();
        } else if is_leader && heartbeat_timeout {
            if !self.peers.is_empty() {
                trace!("Sending heartbeats as leader");
                // Send heartbeats
                self.last_heartbeat = timestamp;
                self.peers
                    .iter()
                    .map(|peer| (peer.clone(), RaftMessage::Heartbeat { term: self.term }))
                    .collect()
            } else {
                vec![]
            }
        } else {
            vec![]
        }
    }

    fn clear_status(&mut self) {
        self.set_status(RaftStatus::Follower {
            leader: None,
            voted_for: None,
        });
    }

    fn set_status(&mut self, status: RaftStatus) {
        let leader = match &status {
            RaftStatus::Leader => RaftLeader::Myself,
            RaftStatus::Follower {
                leader: Some(leader),
                ..
            } => RaftLeader::Other(leader.clone()),
            _ => RaftLeader::Unknown,
        };
        self.status = status;
        self.leader_sink.send_if_modified(|old_leader| {
            let changed = *old_leader != leader;
            if changed {
                *old_leader = leader;
            }
            changed
        });
    }
}

impl Raft {
    pub fn new(
        quorum: usize,
        heartbeat_freq: std::time::Duration,
        timeout_freq: std::time::Duration,
        network: &mut Network,
        leader_sink: watch::Sender<RaftLeader>,
    ) -> Self {
        info!(
            quorum = quorum,
            heartbeat = format!("{:?}", heartbeat_freq),
            timeout = format!("{:?}", timeout_freq),
            "New raft protocol"
        );
        let id = network.id.clone();
        let channel = network.raft_channel();
        let state = RaftState::new(
            id.clone(),
            quorum,
            heartbeat_freq,
            timeout_freq,
            leader_sink,
        );
        Raft { id, channel, state }
    }

    pub async fn handle_messages(self) {
        info!("testing");
        let (sender, mut receiver) = self.channel.split();
        let mut state = self.state;
        loop {
            let next_message = receiver.try_recv().await;
            let timestamp = std::time::Instant::now();

            let responses = match next_message {
                Some(msg) => {
                    trace!("Received message: {:?}", msg);
                    state.receive(timestamp, msg)
                }
                None => state.tick(timestamp),
            };

            // Send out any responses
            for (peer, response) in responses {
                sender.send(peer, response).await;
            }
            // Yield back to the scheduler, so that other tasks can run
            tokio::task::yield_now().await;
        }
    }
}
